//! The MCP server behind `/mcp`.
//!
//! Streamable HTTP, protocol negotiation, Host and Origin validation, and
//! the request-size cap come from the official Rust SDK. 2026-07-28 is
//! served statelessly; 2025-11-25 clients negotiate down to it. Tools arrive
//! with the registry; until then the tool list is empty.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::ServerHandler;
use rmcp::model::{Implementation, InitializeResult, ProtocolVersion, ServerCapabilities};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::config::McpConfig;

/// Protocol versions Sekisho speaks, newest first.
pub const PROTOCOL_VERSIONS: &[ProtocolVersion] =
    &[ProtocolVersion::V_2026_07_28, ProtocolVersion::V_2025_11_25];

#[derive(Clone, Default)]
pub struct Gateway;

impl ServerHandler for Gateway {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("sekisho", env!("CARGO_PKG_VERSION")))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(PROTOCOL_VERSIONS)
    }
}

/// Stateless Streamable HTTP service. `shutdown` ends in-flight streams.
pub fn service(
    config: &McpConfig,
    shutdown: CancellationToken,
) -> StreamableHttpService<Gateway, NeverSessionManager> {
    let http = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_sse_keep_alive(None)
        .with_allowed_hosts(config.allowed_hosts.clone())
        .with_allowed_origins(config.allowed_origins.clone())
        .enforce_origin_validation()
        .with_max_request_body_bytes(config.max_request_bytes)
        .with_cancellation_token(shutdown);
    StreamableHttpService::new(
        || Ok(Gateway),
        Arc::new(NeverSessionManager::default()),
        http,
    )
}
