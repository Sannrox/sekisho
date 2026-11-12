//! HTTP host: `/healthz` (liveness), `/readyz` (stores can do work, and a
//! configured token issuer or control plane can be reached), `/metrics`,
//! OAuth protected-resource metadata, the MCP endpoint at `/mcp` behind
//! bearer authentication, and `/audit/calls` for configured audit readers.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::McpConfig;
use crate::metrics::Metrics;
use crate::verifier::{CallerToken, Principal, Rejection, Verifier};

pub struct AppState {
    pub verifier: Verifier,
    pub metrics: Arc<Metrics>,
}

impl AppState {
    /// Verify the `Authorization: Bearer` header and count the outcome.
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, Rejection> {
        self.authenticate_token(headers)
            .map(|(principal, _)| principal)
    }

    /// Like [`Self::authenticate`], also returning the presented token.
    pub fn authenticate_token(
        &self,
        headers: &HeaderMap,
    ) -> Result<(Principal, CallerToken), Rejection> {
        let result = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(Rejection::Malformed)
            .and_then(|token| {
                let principal = self.verifier.verify(token)?;
                Ok((principal, CallerToken(token.to_string())))
            });
        self.metrics.record(result.is_ok());
        result
    }
}

/// RFC 9728 protected-resource metadata for `/mcp`.
#[derive(Debug, Clone, Serialize)]
pub struct ProtectedResource {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub bearer_methods_supported: [&'static str; 1],
}

/// Where RFC 9728 puts the metadata for `resource`: the well-known prefix
/// inserted between the origin and the resource path.
fn metadata_url(resource: &str) -> String {
    let after_scheme = resource.find("://").map_or(0, |at| at + 3);
    let path_start = resource[after_scheme..]
        .find('/')
        .map_or(resource.len(), |at| after_scheme + at);
    let (origin, path) = resource.split_at(path_start);
    format!(
        "{origin}/.well-known/oauth-protected-resource{}",
        path.trim_end_matches('/')
    )
}

struct McpAuth {
    state: Arc<AppState>,
    challenge: HeaderValue,
}

pub fn router(
    state: Arc<AppState>,
    mcp: &McpConfig,
    gateway: crate::gateway::Gateway,
    shutdown: CancellationToken,
) -> Router {
    let metadata = ProtectedResource {
        resource: mcp.resource.clone(),
        authorization_servers: mcp.authorization_servers.clone(),
        bearer_methods_supported: ["header"],
    };
    let challenge = HeaderValue::from_str(&format!(
        "Bearer resource_metadata=\"{}\"",
        metadata_url(&mcp.resource)
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("Bearer"));
    let auth = Arc::new(McpAuth {
        state: state.clone(),
        challenge,
    });
    let metadata_route = get(move || {
        let metadata = metadata.clone();
        async move { axum::Json(metadata) }
    });
    let audit_reads = Router::new()
        .route("/audit/calls", get(audit_calls))
        .with_state(Arc::new(AuditReads {
            state: state.clone(),
            audit: gateway.audit(),
        }));
    let readiness = Router::new()
        .route("/readyz", get(readyz))
        .with_state(Readiness {
            store: gateway.state(),
            federation: gateway.federation(),
            exchanger: gateway.exchanger(),
            control_plane: gateway.control_plane_endpoint(),
            deps: Arc::new(Mutex::new(None)),
        });
    let mcp_service = Router::new()
        .nest_service("/mcp", crate::gateway::service(mcp, gateway, shutdown))
        .layer(middleware::from_fn_with_state(auth, require_bearer));
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/metrics", get(metrics))
        .route(
            "/.well-known/oauth-protected-resource",
            metadata_route.clone(),
        )
        .route("/.well-known/oauth-protected-resource/mcp", metadata_route)
        .with_state(state)
        .merge(audit_reads)
        .merge(readiness)
        .merge(mcp_service)
        // Outermost, so preflights (no token) and metadata fetches get it.
        .layer(cors(&mcp.allowed_origins))
}

/// CORS for the configured browser origins only (MCP and its metadata),
/// matched like the Origin check on `/mcp`: scheme, host, and port (scheme default when omitted,
/// `*` for any). No origins, no CORS headers.
fn cors(origins: &[String]) -> tower_http::cors::CorsLayer {
    use axum::http::{HeaderName, Method};
    use tower_http::cors::AllowOrigin;
    let allowed: Vec<String> = origins.to_vec();
    tower_http::cors::CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
            origin
                .to_str()
                .is_ok_and(|origin| allowed.iter().any(|entry| origin_matches(entry, origin)))
        }))
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        // MCP request headers include open-ended `Mcp-Param-*` names, so
        // mirror what the preflight asks for; only allowed origins get here.
        .allow_headers(tower_http::cors::AllowHeaders::mirror_request())
        .expose_headers([
            header::WWW_AUTHENTICATE,
            HeaderName::from_static("mcp-session-id"),
        ])
}

/// `(scheme, host, port)` of an origin; port is `None` for `*`.
fn origin_parts(origin: &str) -> Option<(&str, &str, Option<u16>)> {
    let (scheme, rest) = origin.split_once("://")?;
    let default = match scheme {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    // A bracketed IPv6 host may itself contain colons.
    let host_end = if rest.starts_with('[') {
        rest.find(']')? + 1
    } else {
        rest.find(':').unwrap_or(rest.len())
    };
    let (host, port) = rest.split_at(host_end);
    let port = match port.strip_prefix(':') {
        None if port.is_empty() => Some(default),
        None => return None,
        Some("*") => None,
        Some(port) => Some(port.parse().ok()?),
    };
    Some((scheme, host, port))
}

fn origin_matches(entry: &str, origin: &str) -> bool {
    match (origin_parts(entry), origin_parts(origin)) {
        (Some((scheme, host, port)), Some((origin_scheme, origin_host, Some(origin_port)))) => {
            scheme == origin_scheme
                && host.eq_ignore_ascii_case(origin_host)
                && port.is_none_or(|port| port == origin_port)
        }
        _ => false,
    }
}

/// 401 with the metadata location when the token is missing or invalid, so
/// clients can discover the authorization server. The principal is passed
/// on as a request extension.
async fn require_bearer(
    State(auth): State<Arc<McpAuth>>,
    mut request: Request,
    next: Next,
) -> Response {
    match auth.state.authenticate_token(request.headers()) {
        Ok((principal, token)) => {
            request.extensions_mut().insert(principal);
            request.extensions_mut().insert(token);
            next.run(request).await
        }
        Err(rejection) => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, auth.challenge.clone())],
            format!("{rejection}\n"),
        )
            .into_response(),
    }
}

struct AuditReads {
    state: Arc<AppState>,
    audit: Arc<crate::audit::CallAudit>,
}

struct AuditQuery {
    trace_id: String,
    cursor: u64,
    limit: usize,
}

impl AuditQuery {
    /// `trace_id` is required; `cursor` defaults to 0 and `limit` to 100.
    fn parse(query: Option<&str>) -> Option<Self> {
        let (mut trace_id, mut cursor, mut limit) = (None, 0, 100);
        for (name, value) in url::form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
            match name.as_ref() {
                "trace_id" => trace_id = Some(value.into_owned()),
                "cursor" => cursor = value.parse().ok()?,
                "limit" => limit = value.parse().ok()?,
                _ => {}
            }
        }
        Some(Self {
            trace_id: trace_id?,
            cursor,
            limit,
        })
    }
}

/// `GET /audit/calls?trace_id=&cursor=&limit=`: one trace's call events for
/// the caller's tenant. Off (404) unless `[audit] readers` names subjects; a
/// reader without a `tenant_id` claim is refused. An unknown trace and
/// another tenant's trace read the same: an empty page.
async fn audit_calls(
    State(reads): State<Arc<AuditReads>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    if !reads.audit.readable() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let principal = match reads.state.authenticate(&headers) {
        Ok(principal) => principal,
        Err(rejection) => {
            return (StatusCode::UNAUTHORIZED, format!("{rejection}\n")).into_response();
        }
    };
    let Some(tenant) = principal
        .tenant
        .filter(|_| reads.audit.may_read(&principal.subject))
    else {
        return (StatusCode::FORBIDDEN, "not an audit reader for a tenant\n").into_response();
    };
    let Some(query) = AuditQuery::parse(uri.query()) else {
        return (
            StatusCode::BAD_REQUEST,
            "trace_id is required; cursor and limit are numbers\n",
        )
            .into_response();
    };
    let valid = query.trace_id.len() == 32
        && query
            .trace_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !valid {
        return (
            StatusCode::BAD_REQUEST,
            "trace_id must be 32 lowercase hex\n",
        )
            .into_response();
    }
    let audit = Arc::clone(&reads.audit);
    let page = tokio::task::spawn_blocking(move || {
        audit.read(&tenant, &query.trace_id, query.cursor, query.limit)
    })
    .await;
    match page {
        Ok(Ok(page)) => axum::Json(page).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "audit unavailable\n").into_response(),
    }
}

#[derive(Clone)]
struct Readiness {
    store: Arc<dyn crate::state::StateBackend>,
    federation: Arc<crate::gateway::Federation>,
    exchanger: Arc<crate::exchange::Exchanger>,
    control_plane: Option<String>,
    deps: Arc<Mutex<Option<(Instant, Deps)>>>,
}

#[derive(Clone, Copy)]
struct Deps {
    exchange: Option<&'static str>,
    control_plane: Option<&'static str>,
}

fn probe_status(result: Result<(), String>) -> &'static str {
    if result.is_ok() { "ok" } else { "unavailable" }
}

async fn dependency_status(ready: &Readiness) -> Deps {
    {
        let cached = ready
            .deps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, deps)) = cached.as_ref()
            && at.elapsed() < crate::state::PROBE_TTL
        {
            return *deps;
        }
    }
    let (exchange, control_plane) = tokio::join!(ready.exchanger.probe(), async {
        match &ready.control_plane {
            Some(endpoint) => Some(crate::control_plane::probe_origin(endpoint).await),
            None => None,
        }
    });
    if let Some(Err(reason)) = &exchange {
        eprintln!("readiness: exchange unavailable: {reason}");
    }
    if let Some(Err(reason)) = &control_plane {
        eprintln!("readiness: control plane unavailable: {reason}");
    }
    let deps = Deps {
        exchange: exchange.map(probe_status),
        control_plane: control_plane.map(probe_status),
    };
    *ready
        .deps
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), deps));
    deps
}

/// `GET /readyz`: 200 when every store of the state backend can do work
/// and each configured token issuer or control-plane origin answers HTTP,
/// else 503. The body names the backend and each store as `ok` or
/// `unavailable`; configured `exchange` and `control_plane` use the same
/// words. Reasons go to the log only, since they carry paths. The registry
/// also counts as unavailable while this replica's registry snapshot is
/// stale ([`crate::gateway::REGISTRY_STALE_AFTER`]).
async fn readyz(State(ready): State<Readiness>) -> Response {
    let kind = ready.store.kind();
    let store = Arc::clone(&ready.store);
    // Store deadline and origin timeout run together so the probe stays
    // inside the Kubernetes readiness timeout.
    let mut store_deadline = crate::state::STORE_DEADLINE;
    let store_probe = crate::state::blocking(
        crate::state::Store::Limits,
        &mut store_deadline,
        move || Ok(store.probe()),
    );
    let (probe, deps) = tokio::join!(store_probe, dependency_status(&ready));
    let Ok(probe) = probe else {
        eprintln!("readiness: the probe did not answer within the store deadline");
        return (StatusCode::SERVICE_UNAVAILABLE, "readiness probe failed\n").into_response();
    };
    let mut stores = std::collections::BTreeMap::new();
    for (name, result) in probe {
        if let Err(reason) = &result {
            eprintln!("readiness: {name} store unavailable: {reason}");
        }
        let status = match (&result, name) {
            (Ok(()), _) => "ok",
            (Err(_), crate::state::Store::State) => "uninitialized",
            (Err(_), _) => "unavailable",
        };
        stores.insert(name, status);
    }
    if ready.federation.stale() {
        eprintln!("readiness: the registry snapshot is stale");
        stores.insert(crate::state::Store::Registry, "unavailable");
    }
    let ready = stores.values().all(|state| *state == "ok")
        && deps.exchange.is_none_or(|state| state == "ok")
        && deps.control_plane.is_none_or(|state| state == "ok");
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let mut body = serde_json::json!({ "backend": kind, "ready": ready, "stores": stores });
    if let Some(exchange) = deps.exchange {
        body["exchange"] = serde_json::Value::String(exchange.into());
    }
    if let Some(control_plane) = deps.control_plane {
        body["control_plane"] = serde_json::Value::String(control_plane.into());
    }
    (status, axum::Json(body)).into_response()
}

async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(state.verifier.kind()),
    )
}

#[cfg(test)]
mod tests {
    use super::{metadata_url, origin_matches};

    #[test]
    fn origins_match_on_scheme_host_and_effective_port() {
        assert!(origin_matches(
            "https://app.example.test:443",
            "https://app.example.test"
        ));
        assert!(origin_matches(
            "http://localhost:*",
            "http://localhost:5173"
        ));
        assert!(!origin_matches(
            "https://app.example.test:443",
            "http://app.example.test"
        ));
        assert!(!origin_matches(
            "https://app.example.test:443",
            "https://app.example.test:8443"
        ));
        assert!(!origin_matches(
            "https://app.example.test",
            "https://evil.example.test"
        ));
    }

    #[test]
    fn metadata_url_inserts_the_well_known_prefix_before_the_path() {
        assert_eq!(
            metadata_url("https://gw.example.test/mcp"),
            "https://gw.example.test/.well-known/oauth-protected-resource/mcp"
        );
        assert_eq!(
            metadata_url("https://gw.example.test"),
            "https://gw.example.test/.well-known/oauth-protected-resource"
        );
    }
}
