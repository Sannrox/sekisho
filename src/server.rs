//! HTTP host: `/healthz`, `/metrics`, OAuth protected-resource metadata, and
//! the MCP endpoint at `/mcp` behind bearer authentication.

use std::sync::Arc;

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
use crate::verifier::{Principal, Rejection, Verifier};

pub struct AppState {
    pub verifier: Verifier,
    pub metrics: Metrics,
}

impl AppState {
    /// Verify the `Authorization: Bearer` header and count the outcome.
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, Rejection> {
        let result = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(Rejection::Malformed)
            .and_then(|token| self.verifier.verify(token));
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
pub fn metadata_url(resource: &str) -> String {
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
    federation: Arc<crate::gateway::Federation>,
    policy: crate::config::PolicyConfig,
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
    let mcp_service = Router::new()
        .nest_service(
            "/mcp",
            crate::gateway::service(mcp, federation, policy, shutdown),
        )
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
    match auth.state.authenticate(request.headers()) {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
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
