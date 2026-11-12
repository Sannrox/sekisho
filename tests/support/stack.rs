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

/// Bearer injected on egress calls in [`Stack::start_egress`].
pub const EGRESS_SECRET: &str = "egress-credential-does-not-leak-9911";

/// Upstream with `echo` and `other` tools. Every call must carry a token the
/// fake issuer minted for this audience and that tool; it records the claims.
/// When `static_bearer` is set, that exact token is accepted instead.
#[derive(Clone)]
struct Upstream {
    description: Arc<Mutex<String>>,
    seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    traceparents: Arc<Mutex<Vec<Option<String>>>>,
    /// Set when a `hold` call has reached the upstream, before it sleeps.
    hold_entered: Arc<std::sync::atomic::AtomicBool>,
    /// A `pause` call sets `paused` and answers once `resume` is notified.
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume: Arc<tokio::sync::Notify>,
    static_bearer: Option<String>,
    list_count: Arc<std::sync::atomic::AtomicUsize>,
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
        self.list_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        let claims = if let Some(expected) = &self.static_bearer {
            (token == *expected)
                .then(|| serde_json::json!({ "injected": true }))
                .ok_or_else(|| ErrorData::invalid_params("token is not for this tool", None))?
        } else {
            delegated_claims(&token)
                .filter(|claims| {
                    let wanted = format!("tool:{}", request.name);
                    claims["scope"]
                        .as_str()
                        .is_some_and(|scope| scope.split(' ').any(|s| s == wanted))
                })
                .ok_or_else(|| ErrorData::invalid_params("token is not for this tool", None))?
        };
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
        if text == "hold" {
            self.hold_entered
                .store(true, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        if text == "pause" {
            self.paused.store(true, std::sync::atomic::Ordering::SeqCst);
            self.resume.notified().await;
        }
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(format!("echo {text}")),
        ])))
    }
}

async fn serve_on(app: axum::Router, stop: CancellationToken, bind: &str) -> String {
    let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { stop.cancelled_owned().await })
            .await;
    });
    address
}

async fn serve_on_loopback(app: axum::Router, stop: CancellationToken) -> String {
    serve_on(app, stop, "127.0.0.1:0").await
}

/// Loopback origin that answers GET / until `stop` is cancelled.
pub async fn serve_origin(stop: CancellationToken) -> String {
    serve_on_loopback(
        axum::Router::new().route("/", axum::routing::get(|| async { "ok\n" })),
        stop,
    )
    .await
}

async fn bind_retry(bind: &str) -> tokio::net::TcpListener {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::net::TcpListener::bind(bind).await {
            Ok(listener) => return listener,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("bind {bind}: {error}"),
        }
    }
}

/// Bind `serve_origin` on `bind` (`host:port`), retrying until the port is free.
pub async fn serve_origin_at(bind: &str, stop: CancellationToken) {
    let listener = bind_retry(bind).await;
    let app = axum::Router::new().route("/", axum::routing::get(|| async { "ok\n" }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { stop.cancelled_owned().await })
            .await;
    });
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

/// `GET /readyz`: status and body.
pub async fn readyz(address: &str) -> (u16, serde_json::Value) {
    let response = reqwest::get(format!("http://{address}/readyz"))
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap())
}

/// Poll `/readyz` past the probe cache until it answers `status`.
pub async fn readiness_settles(address: &str, status: u16) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    let settled = eventually(Duration::from_secs(5), async || {
        let (got, body) = readyz(address).await;
        last = body;
        got == status
    })
    .await;
    assert!(settled, "readyz never answered {status}: {last}");
    last
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
    /// Loopback address of the fake issuer (`host:port`).
    pub issuer_address: String,
    issuer_stop: std::sync::Mutex<CancellationToken>,
    /// An upstream `hold` call has started sleeping.
    pub hold_entered: Arc<std::sync::atomic::AtomicBool>,
    /// An upstream `pause` call is waiting for [`Self::resume`].
    pub paused: Arc<std::sync::atomic::AtomicBool>,
    pub resume: Arc<tokio::sync::Notify>,
    /// Times the fake upstream served `tools/list`.
    pub list_count: Arc<std::sync::atomic::AtomicUsize>,
    stop: CancellationToken,
    _fixture: Fixture,
}

/// A second replica sharing the stack's files. [`Replica::kill`] aborts the
/// listener like process death.
pub struct Replica {
    pub address: String,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}

impl Replica {
    /// Cancel in-flight MCP calls, then drop the listener.
    pub fn kill(self) {
        self.stop.cancel();
        self.server.abort();
    }
}

impl Drop for Replica {
    fn drop(&mut self) {
        self.stop.cancel();
        self.server.abort();
    }
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
        Self::start_inner(agents_toml, extra_toml, None).await
    }

    /// Upstream `tools` with injected `token_file`, no audience. `listed`
    /// puts that origin on `[egress] allow`.
    pub async fn start_egress(agents_toml: Option<&str>, listed: bool) -> Self {
        Self::start_inner(agents_toml, "", Some(listed)).await
    }

    async fn start_inner(
        agents_toml: Option<&str>,
        extra_toml: &str,
        egress: Option<bool>,
    ) -> Self {
        let stop = CancellationToken::new();
        let description = Arc::new(Mutex::new("Echo v1".to_string()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let traceparents = Arc::new(Mutex::new(Vec::new()));
        let hold_entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resume = Arc::new(tokio::sync::Notify::new());
        let list_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let static_bearer = egress.map(|_| EGRESS_SECRET.to_string());
        let upstream = Upstream {
            paused: paused.clone(),
            resume: resume.clone(),
            description: description.clone(),
            seen: seen.clone(),
            traceparents: traceparents.clone(),
            hold_entered: hold_entered.clone(),
            static_bearer: static_bearer.clone(),
            list_count: list_count.clone(),
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
        let issuer_stop = stop.child_token();
        let issuer_address =
            serve_on_loopback(issuer(issuer_down.clone()), issuer_stop.clone()).await;
        let fixture = Fixture::new();
        fixture.write("client-secret", "issuer-secret\n");
        if let Some(agents) = agents_toml {
            fixture.write("agents.toml", agents);
        }
        let path = fixture.jwks_config("127.0.0.1:0", &[("k1", &key(1))]);
        let text = std::fs::read_to_string(&path).unwrap()
            + &format!(
                "\n[registry]\nfile = \"registry.json\"\naudit_file = \"audit.jsonl\"\nrefresh_interval_ms = 100\n\n[exchange]\ntoken_endpoint = \"http://{issuer_address}/token\"\nclient_id = \"sekisho\"\nclient_secret_file = \"client-secret\"\n"
            );
        let upstreams = if egress.is_some() {
            fixture.write("egress-token", EGRESS_SECRET);
            let origin = format!("http://{upstream_address}");
            let allow = match egress {
                Some(true) => format!("[egress]\nallow = [\"{origin}\"]\n"),
                _ => "[egress]\nallow = [\"http://127.0.0.1:1\"]\n".into(),
            };
            format!(
                "[[upstreams]]\nname = \"tools\"\nurl = \"http://{upstream_address}/mcp\"\ntoken_file = \"egress-token\"\n{allow}"
            )
        } else {
            format!(
                "[[upstreams]]\nname = \"tools\"\nurl = \"http://{upstream_address}/mcp\"\naudience = \"{ENGINE_AUDIENCE}\"\n"
            )
        };
        std::fs::write(&path, text + &upstreams + extra_toml).unwrap();
        let config = Config::load(&path).unwrap();
        let store = sekisho::state::open(&config).unwrap();
        // The PostgreSQL backend is initialized and keeps the policy in the
        // database; without a policy file it stays uninitialized.
        if let (Some(_), sekisho::config::StateConfig::Postgres { .. }) =
            (agents_toml, &config.state)
        {
            let (store, file) = (Arc::clone(&store), config.policy.file.clone());
            tokio::task::spawn_blocking(move || {
                sekisho::state::init(store.as_ref(), "test").unwrap();
                store.set_policy(&sekisho::policy::PolicyFile::load(&file).unwrap())
            })
            .await
            .unwrap()
            .unwrap();
        }
        let federation = Arc::new(Federation::new(
            config.upstreams.clone(),
            Arc::clone(&store),
            config.egress.clone(),
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
            Arc::clone(&store),
            sekisho::exchange::Exchanger::new(config.exchange.clone()),
            sekisho::audit::CallAudit::new(Arc::clone(&store))
                .readers(config.audit.readers.clone()),
            Default::default(),
        )
        .guardrails(sekisho::guardrails::Guardrails::new(
            config.guardrails.clone(),
        ))
        .approvals(config.approvals.clone())
        .mitigations(sekisho::mitigations::MitigationStore::new(Arc::clone(
            &store,
        )))
        .control_plane(
            config
                .mitigations
                .control_plane
                .as_ref()
                .map(|plane| plane.endpoint.clone()),
        )
        .egress(config.egress.clone())
        .limits(sekisho::limits::Limiter::from_config(&config, &store));
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
            issuer_address,
            issuer_stop: std::sync::Mutex::new(issuer_stop),
            hold_entered,
            paused,
            resume,
            list_count,
            stop,
            _fixture: fixture,
        }
    }

    /// Stop the fake issuer so its token endpoint cannot be reached.
    pub async fn stop_issuer(&self) {
        self.issuer_stop.lock().unwrap().cancel();
        let address = self.issuer_address.clone();
        let gone = eventually(Duration::from_secs(2), async || {
            tokio::net::TcpStream::connect(&address).await.is_err()
        })
        .await;
        assert!(gone, "issuer still listening on {address}");
    }

    /// Bind the fake issuer again on the address in the loaded config.
    pub async fn resume_issuer(&self) {
        let stop = self.stop.child_token();
        *self.issuer_stop.lock().unwrap() = stop.clone();
        let listener = bind_retry(&self.issuer_address).await;
        let app = issuer(self.issuer_down.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { stop.cancelled_owned().await })
                .await;
        });
    }

    /// Another replica sharing this stack's files.
    pub async fn extra_gateway(&self) -> Replica {
        self.replica(&self.config).await
    }

    /// A replica serving `config`, for example one loaded from another
    /// directory that points at this stack's shared files.
    pub async fn replica(&self, config: &Config) -> Replica {
        let stop = CancellationToken::new();
        let store = sekisho::state::open(config).unwrap();
        let federation = Arc::new(Federation::new(
            config.upstreams.clone(),
            Arc::clone(&store),
            config.egress.clone(),
        ));
        federation.refresh().await.unwrap();
        tokio::spawn(federation.clone().run(
            Duration::from_millis(config.registry.refresh_interval_ms),
            stop.clone(),
        ));
        let state = Arc::new(AppState {
            verifier: Verifier::from_config(&config.verifier).unwrap(),
            metrics: Default::default(),
        });
        let gateway = sekisho::gateway::Gateway::new(
            federation,
            Arc::clone(&store),
            sekisho::exchange::Exchanger::new(config.exchange.clone()),
            sekisho::audit::CallAudit::new(Arc::clone(&store))
                .readers(config.audit.readers.clone()),
            Default::default(),
        )
        .guardrails(sekisho::guardrails::Guardrails::new(
            config.guardrails.clone(),
        ))
        .approvals(config.approvals.clone())
        .mitigations(sekisho::mitigations::MitigationStore::new(Arc::clone(
            &store,
        )))
        .control_plane(
            config
                .mitigations
                .control_plane
                .as_ref()
                .map(|plane| plane.endpoint.clone()),
        )
        .egress(config.egress.clone())
        .limits(sekisho::limits::Limiter::from_config(config, &store));
        let app = router(state, &config.mcp, gateway, stop.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Replica {
            address,
            stop,
            server,
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

    /// Like [`Self::client`], talking to `address` instead of this stack.
    pub async fn client_on(
        &self,
        address: &str,
        extra: serde_json::Value,
    ) -> RunningService<RoleClient, ClientConfig> {
        self.client_on_with(address, extra, std::collections::HashMap::new())
            .await
    }

    /// Like [`Self::client`], sending `headers` on every request.
    pub async fn client_with(
        &self,
        extra: serde_json::Value,
        headers: std::collections::HashMap<axum::http::HeaderName, axum::http::HeaderValue>,
    ) -> RunningService<RoleClient, ClientConfig> {
        self.client_on_with(&self.address, extra, headers).await
    }

    /// Like [`Self::client_on`], sending `headers` on every request.
    pub async fn client_on_with(
        &self,
        address: &str,
        extra: serde_json::Value,
        headers: std::collections::HashMap<axum::http::HeaderName, axum::http::HeaderValue>,
    ) -> RunningService<RoleClient, ClientConfig> {
        self.try_client_on_with(address, extra, headers)
            .await
            .unwrap()
    }

    /// Like [`Self::client_on_with`], returning why a connection failed.
    pub async fn try_client_on_with(
        &self,
        address: &str,
        extra: serde_json::Value,
        headers: std::collections::HashMap<axum::http::HeaderName, axum::http::HeaderValue>,
    ) -> Result<RunningService<RoleClient, ClientConfig>, String> {
        let bearer = token("k1", &key(1), claims(extra));
        ClientConfig::default()
            .serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp"))
                    .auth_header(bearer)
                    .custom_headers(headers),
            ))
            .await
            .map_err(|error| error.to_string())
    }

    /// Wait for the first refresh, then enable `tools.echo` as `owner-1`.
    pub async fn enable_echo(&self) {
        self.enable("tools.echo").await;
    }

    /// Wait until `tool` is in the registry, then enable it as `owner-1`.
    pub async fn enable(&self, tool: &str) {
        self.owner_edit(tool, "enable", true).await;
    }

    /// Wait until `tool` is in the stored registry, then set its enabled
    /// state as `owner-1`.
    pub async fn owner_edit(&self, tool: &str, action: &str, enabled: bool) {
        let store = sekisho::state::open(&self.config).unwrap();
        let (tool, action) = (tool.to_string(), action.to_string());
        let listed = {
            let (store, tool) = (Arc::clone(&store), tool.clone());
            move || store.registry().unwrap().tools.contains_key(&tool)
        };
        assert!(
            eventually(Duration::from_secs(5), async || {
                tokio::task::spawn_blocking(listed.clone()).await.unwrap()
            })
            .await
        );
        tokio::task::spawn_blocking(move || {
            sekisho::registry::owner_edit(store.as_ref(), "owner-1", &action, &tool, |registry| {
                registry.set_enabled(&tool, enabled)
            })
        })
        .await
        .unwrap()
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
