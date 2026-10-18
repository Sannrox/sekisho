//! A fake upstream MCP server and Sekisho in front of it, on loopback.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientConfig, ContentBlock,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt as _};
use sekisho::config::Config;
use sekisho::gateway::Federation;
use sekisho::server::{AppState, router};
use sekisho::verifier::Verifier;
use tokio_util::sync::CancellationToken;

use super::*;

/// Upstream with one `echo` tool whose description the test can change.
#[derive(Clone)]
struct Upstream {
    description: Arc<Mutex<String>>,
}

impl ServerHandler for Upstream {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let description = self.description.lock().unwrap().clone();
        let serde_json::Value::Object(schema) = serde_json::json!({"type": "object"}) else {
            unreachable!()
        };
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "echo",
            description,
            Arc::new(schema),
        )]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let text = request
            .arguments
            .and_then(|arguments| arguments.get("text")?.as_str().map(str::to_string))
            .unwrap_or_default();
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(format!("echo {text}")),
        ])))
    }
}

async fn serve_on_loopback(app: axum::Router, stop: CancellationToken) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { stop.cancelled_owned().await })
            .await;
    });
    address
}

/// Poll `condition` until it holds or `limit` passes.
pub async fn eventually(limit: Duration, mut condition: impl AsyncFnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        if condition().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

pub fn echo_call(text: &str) -> CallToolRequestParams {
    let mut call = CallToolRequestParams::new("tools.echo");
    call.arguments = serde_json::json!({ "text": text }).as_object().cloned();
    call
}

pub struct Stack {
    pub address: String,
    pub config: Config,
    pub description: Arc<Mutex<String>>,
    stop: CancellationToken,
    _fixture: Fixture,
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Stack {
    /// Upstream `tools` serving `echo`, Sekisho refreshing every 100 ms, and
    /// `agents_toml` as the agent policy file (`None`: no policy file).
    pub async fn start(agents_toml: Option<&str>) -> Self {
        let stop = CancellationToken::new();
        let description = Arc::new(Mutex::new("Echo v1".to_string()));
        let upstream = Upstream {
            description: description.clone(),
        };
        let upstream_service = StreamableHttpService::new(
            move || Ok(upstream.clone()),
            Arc::new(NeverSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_sse_keep_alive(None),
        );
        let upstream_address = serve_on_loopback(
            axum::Router::new().nest_service("/mcp", upstream_service),
            stop.clone(),
        )
        .await;

        let fixture = Fixture::new();
        if let Some(agents) = agents_toml {
            fixture.write("agents.toml", agents);
        }
        let path = fixture.jwks_config("127.0.0.1:0", &[("k1", &key(1))]);
        let text = std::fs::read_to_string(&path).unwrap()
            + &format!(
                "\n[registry]\nfile = \"registry.json\"\naudit_file = \"audit.jsonl\"\nrefresh_interval_ms = 100\n\n[[upstreams]]\nname = \"tools\"\nurl = \"http://{upstream_address}/mcp\"\n"
            );
        std::fs::write(&path, text).unwrap();
        let config = Config::load(&path).unwrap();
        let federation = Arc::new(Federation::new(
            config.upstreams.clone(),
            config.registry.file.clone(),
        ));
        tokio::spawn(
            federation
                .clone()
                .run(Duration::from_millis(100), stop.clone()),
        );
        let state = Arc::new(AppState {
            verifier: Verifier::from_config(&config.verifier).unwrap(),
            metrics: Default::default(),
        });
        let app = router(
            state,
            &config.mcp,
            federation,
            config.policy.clone(),
            stop.clone(),
        );
        let address = serve_on_loopback(app, stop.clone()).await;
        Self {
            address,
            config,
            description,
            stop,
            _fixture: fixture,
        }
    }

    /// An SDK client for a token with these extra claims (for example `act`).
    pub async fn client(
        &self,
        extra: serde_json::Value,
    ) -> RunningService<RoleClient, ClientConfig> {
        let bearer = token("k1", &key(1), claims(extra));
        ClientConfig::default()
            .serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(format!(
                    "http://{}/mcp",
                    self.address
                ))
                .auth_header(bearer),
            ))
            .await
            .unwrap()
    }

    /// Wait for the first refresh, then enable `tools.echo` as `owner-1`.
    pub async fn enable_echo(&self) {
        let file = self.config.registry.file.clone();
        assert!(
            eventually(Duration::from_secs(5), async || {
                sekisho::registry::Registry::load(&file)
                    .unwrap()
                    .tools
                    .contains_key("tools.echo")
            })
            .await
        );
        sekisho::registry::owner_edit(
            &file,
            &self.config.registry.audit_file,
            "owner-1",
            "enable",
            "tools.echo",
            |registry| registry.set_enabled("tools.echo", true),
        )
        .unwrap();
    }
}
