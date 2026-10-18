//! Registry federation end to end: a fake upstream MCP server, Sekisho in
//! front of it, and an official SDK client as the agent.

mod support;

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientConfig, ContentBlock,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt as _};
use sekisho::config::Config;
use sekisho::gateway::Federation;
use sekisho::registry::{self, Registry};
use sekisho::server::{AppState, router};
use sekisho::verifier::Verifier;
use support::*;
use tokio_util::sync::CancellationToken;

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
        let schema = serde_json::json!({"type": "object"});
        let serde_json::Value::Object(schema) = schema else {
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

async fn agent(address: &str) -> rmcp::service::RunningService<rmcp::RoleClient, ClientConfig> {
    let bearer = token("k1", &key(1), claims(serde_json::json!({})));
    ClientConfig::default()
        .serve(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp"))
                .auth_header(bearer),
        ))
        .await
        .unwrap()
}

/// Poll `condition` until it holds or `limit` passes.
async fn eventually(limit: Duration, mut condition: impl AsyncFnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        if condition().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_start_disabled_then_enable_approve_and_roll_back_without_restart() {
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
    let address = serve_on_loopback(
        router(state, &config.mcp, federation, stop.clone()),
        stop.clone(),
    )
    .await;
    let (file, audit) = (
        config.registry.file.clone(),
        config.registry.audit_file.clone(),
    );

    // Discovered, but disabled: not listed and not callable.
    assert!(
        eventually(Duration::from_secs(5), async || Registry::load(&file)
            .unwrap()
            .tools
            .contains_key("tools.echo"))
        .await
    );
    let client = agent(&address).await;
    assert!(client.list_all_tools().await.unwrap().is_empty());
    let mut call = CallToolRequestParams::new("tools.echo");
    call.arguments = serde_json::json!({"text": "hi"}).as_object().cloned();
    assert!(client.call_tool(call.clone()).await.is_err());

    // Enabled by the owner: served within the refresh interval, no restart.
    registry::owner_edit(&file, &audit, "owner-1", "enable", "tools.echo", |r| {
        r.set_enabled("tools.echo", true)
    })
    .unwrap();
    let listed = async || client.list_all_tools().await.unwrap();
    assert!(eventually(Duration::from_secs(5), async || !listed().await.is_empty()).await);
    let result = client.call_tool(call).await.unwrap();
    assert!(format!("{result:?}").contains("echo hi"), "{result:?}");

    // An upstream change waits for approval; rollback restores the old text.
    *description.lock().unwrap() = "Echo v2".into();
    let served = async || -> Option<Cow<'static, str>> {
        listed()
            .await
            .first()
            .and_then(|tool| tool.description.clone())
    };
    assert!(
        eventually(Duration::from_secs(5), async || Registry::load(&file)
            .unwrap()
            .tools["tools.echo"]
            .pending()
            .is_some())
        .await
    );
    assert_eq!(served().await.as_deref(), Some("Echo v1"));
    registry::owner_edit(&file, &audit, "owner-1", "approve", "tools.echo", |r| {
        r.approve("tools.echo")
    })
    .unwrap();
    assert!(
        eventually(Duration::from_secs(5), async || served().await.as_deref()
            == Some("Echo v2"))
        .await
    );
    registry::owner_edit(&file, &audit, "owner-1", "rollback", "tools.echo", |r| {
        r.rollback("tools.echo")
    })
    .unwrap();
    assert!(
        eventually(Duration::from_secs(5), async || served().await.as_deref()
            == Some("Echo v1"))
        .await
    );

    let log = std::fs::read_to_string(&audit).unwrap();
    assert_eq!(log.lines().count(), 3, "{log}");
    assert!(log.contains("\"owner\":\"owner-1\""), "{log}");
    let _ = client.cancel().await;
    stop.cancel();
}
