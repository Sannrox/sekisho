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

/// Audience the fake issuer binds exchanged tokens to for upstream `tools`.
pub const ENGINE_AUDIENCE: &str = "tools-engine";
const ISSUER_KID: &str = "issuer-2";

/// Upstream with `echo` and `other` tools. Every call must carry a token the
/// fake issuer minted for this audience and that tool; it records the claims.
#[derive(Clone)]
struct Upstream {
    description: Arc<Mutex<String>>,
    seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    traceparents: Arc<Mutex<Vec<Option<String>>>>,
}

/// Claims of a delegated token, if signed by the issuer for this audience.
fn delegated_claims(token: &str) -> Option<serde_json::Value> {
    use base64::Engine as _;
    let x =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key(2).verifying_key().to_bytes());
    let decoding = jsonwebtoken::DecodingKey::from_ed_components(&x).ok()?;
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
    validation.set_audience(&[ENGINE_AUDIENCE]);
    validation.set_issuer(&[ISSUER]);
    jsonwebtoken::decode::<serde_json::Value>(token, &decoding, &validation)
        .ok()
        .map(|data| data.claims)
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
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new("echo", description, Arc::new(schema.clone())),
            Tool::new("other", "Another tool", Arc::new(schema)),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let token = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.headers.get("authorization"))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::to_string)
            .unwrap_or_default();
        let claims = delegated_claims(&token)
            .filter(|claims| {
                let wanted = format!("tool:{}", request.name);
                claims["scope"]
                    .as_str()
                    .is_some_and(|scope| scope.split(' ').any(|s| s == wanted))
            })
            .ok_or_else(|| ErrorData::invalid_params("token is not for this tool", None))?;
        let traceparent = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.headers.get("traceparent"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        self.traceparents.lock().unwrap().push(traceparent);
        self.seen.lock().unwrap().push((token, claims));
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
    pub upstream_address: String,
    pub config: Config,
    pub description: Arc<Mutex<String>>,
    /// (delegated token, claims) of every call the upstream accepted.
    pub seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    /// `traceparent` header of every call the upstream accepted.
    pub traceparents: Arc<Mutex<Vec<Option<String>>>>,
    /// Makes the fake issuer refuse every exchange.
    pub issuer_down: Arc<std::sync::atomic::AtomicBool>,
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
        Self::start_with(agents_toml, "").await
    }

    /// Like [`Self::start`], appending `extra_toml` to the config.
    pub async fn start_with(agents_toml: Option<&str>, extra_toml: &str) -> Self {
        let stop = CancellationToken::new();
        let description = Arc::new(Mutex::new("Echo v1".to_string()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let traceparents = Arc::new(Mutex::new(Vec::new()));
        let upstream = Upstream {
            description: description.clone(),
            seen: seen.clone(),
            traceparents: traceparents.clone(),
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

        let issuer_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let issuer_address = serve_on_loopback(issuer(issuer_down.clone()), stop.clone()).await;
        let fixture = Fixture::new();
        fixture.write("client-secret", "issuer-secret\n");
        if let Some(agents) = agents_toml {
            fixture.write("agents.toml", agents);
        }
        let path = fixture.jwks_config("127.0.0.1:0", &[("k1", &key(1))]);
        let text = std::fs::read_to_string(&path).unwrap()
            + &format!(
                "\n[registry]\nfile = \"registry.json\"\naudit_file = \"audit.jsonl\"\nrefresh_interval_ms = 100\n\n[exchange]\ntoken_endpoint = \"http://{issuer_address}/token\"\nclient_id = \"sekisho\"\nclient_secret_file = \"client-secret\"\n\n[[upstreams]]\nname = \"tools\"\nurl = \"http://{upstream_address}/mcp\"\naudience = \"{ENGINE_AUDIENCE}\"\n"
            );
        std::fs::write(&path, text + extra_toml).unwrap();
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
        let gateway = sekisho::gateway::Gateway::new(
            federation,
            config.policy.clone(),
            sekisho::exchange::Exchanger::new(config.exchange.clone()),
            sekisho::audit::CallAudit::new(config.audit.calls_file.clone()),
            Default::default(),
        )
        .limits(config.limits.clone());
        let app = router(state, &config.mcp, gateway, stop.clone());
        let address = serve_on_loopback(app, stop.clone()).await;
        Self {
            address,
            upstream_address,
            config,
            description,
            seen,
            traceparents,
            issuer_down,
            stop,
            _fixture: fixture,
        }
    }

    /// An SDK client for a token with these extra claims (for example `act`).
    pub async fn client(
        &self,
        extra: serde_json::Value,
    ) -> RunningService<RoleClient, ClientConfig> {
        self.client_with(extra, std::collections::HashMap::new())
            .await
    }

    /// Like [`Self::client`], sending `headers` on every request.
    pub async fn client_with(
        &self,
        extra: serde_json::Value,
        headers: std::collections::HashMap<axum::http::HeaderName, axum::http::HeaderValue>,
    ) -> RunningService<RoleClient, ClientConfig> {
        let bearer = token("k1", &key(1), claims(extra));
        ClientConfig::default()
            .serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(format!(
                    "http://{}/mcp",
                    self.address
                ))
                .auth_header(bearer)
                .custom_headers(headers),
            ))
            .await
            .unwrap()
    }

    /// Wait for the first refresh, then enable `tools.echo` as `owner-1`.
    pub async fn enable_echo(&self) {
        self.enable("tools.echo").await;
    }

    /// Wait until `tool` is in the registry, then enable it as `owner-1`.
    pub async fn enable(&self, tool: &str) {
        let file = self.config.registry.file.clone();
        assert!(
            eventually(Duration::from_secs(5), async || {
                sekisho::registry::Registry::load(&file)
                    .unwrap()
                    .tools
                    .contains_key(tool)
            })
            .await
        );
        sekisho::registry::owner_edit(
            &file,
            &self.config.registry.audit_file,
            "owner-1",
            "enable",
            tool,
            |registry| registry.set_enabled(tool, true),
        )
        .unwrap();
    }

    /// Call the upstream directly, bypassing Sekisho, with `bearer`.
    pub async fn call_upstream_directly(
        &self,
        bearer: &str,
        tool: &str,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ServiceError> {
        let client = ClientConfig::default()
            .serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(format!(
                    "http://{}/mcp",
                    self.upstream_address
                ))
                .auth_header(bearer.to_string()),
            ))
            .await
            .unwrap();
        let result = client
            .call_tool(CallToolRequestParams::new(tool.to_string()))
            .await;
        let _ = client.cancel().await;
        result
    }
}

/// Fake RFC 8693 issuer. Checks the client, verifies the subject token with
/// the caller key, and mints a 60 s token for the requested audience and
/// scope whose actor chain is the gateway, then the original actor.
fn issuer(down: Arc<std::sync::atomic::AtomicBool>) -> axum::Router {
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse as _;
    axum::Router::new().route(
        "/token",
        axum::routing::post(
            move |headers: HeaderMap, axum::Form(form): axum::Form<std::collections::HashMap<String, String>>| {
                let down = down.clone();
                async move {
                    use base64::Engine as _;
                    let expected = format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD.encode("sekisho:issuer-secret")
                    );
                    let client_ok = headers
                        .get("authorization")
                        .is_some_and(|value| value.as_bytes() == expected.as_bytes());
                    let grant_ok = form.get("grant_type").map(String::as_str)
                        == Some("urn:ietf:params:oauth:grant-type:token-exchange");
                    if down.load(std::sync::atomic::Ordering::SeqCst) {
                        return (StatusCode::SERVICE_UNAVAILABLE, "down").into_response();
                    }
                    if !client_ok || !grant_ok {
                        return (StatusCode::BAD_REQUEST, "invalid_grant").into_response();
                    }
                    let x = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(key(1).verifying_key().to_bytes());
                    let caller_key = jsonwebtoken::DecodingKey::from_ed_components(&x).unwrap();
                    let mut validation =
                        jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
                    validation.set_audience(&[AUDIENCE]);
                    let Ok(subject) = jsonwebtoken::decode::<serde_json::Value>(
                        &form["subject_token"],
                        &caller_key,
                        &validation,
                    ) else {
                        return (StatusCode::BAD_REQUEST, "invalid_subject_token").into_response();
                    };
                    let mut act = serde_json::json!({ "sub": "sekisho" });
                    if let Some(original) = subject.claims.get("act") {
                        act["act"] = original.clone();
                    }
                    let minted = token(
                        ISSUER_KID,
                        &key(2),
                        claims(serde_json::json!({
                            "sub": subject.claims["sub"],
                            "aud": form["audience"],
                            "scope": form["scope"],
                            "act": act,
                        })),
                    );
                    axum::Json(serde_json::json!({
                        "access_token": minted,
                        "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
                        "token_type": "Bearer",
                        "expires_in": 60,
                    }))
                    .into_response()
                }
            },
        ),
    )
}
