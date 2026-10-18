//! MCP client calls to an upstream server: list its tools, call one.
//!
//! Each call opens a short stateless connection. The listing token is only
//! used to list tools. A tool call carries the delegated token exchanged for
//! that caller and tool ([`crate::exchange`]), never Sekisho's credential.

use std::path::Path;

use axum::http::{HeaderName, HeaderValue};

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig, JsonObject};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ServiceExt as _, service::RunningService};

use crate::config::UpstreamConfig;
use crate::registry::ToolVersion;

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("upstream {upstream}: {message}")]
    Call { upstream: String, message: String },
}

fn error(upstream: &UpstreamConfig, message: impl std::fmt::Display) -> UpstreamError {
    UpstreamError::Call {
        upstream: upstream.name.clone(),
        message: message.to_string(),
    }
}

pub fn timed_out(upstream: &UpstreamConfig) -> UpstreamError {
    error(upstream, "tool listing timed out")
}

async fn connect(
    upstream: &UpstreamConfig,
    bearer: Option<String>,
    traceparent: Option<&str>,
) -> Result<RunningService<rmcp::RoleClient, ClientConfig>, UpstreamError> {
    let mut transport = StreamableHttpClientTransportConfig::with_uri(upstream.url.clone());
    if let Some(bearer) = bearer {
        transport = transport.auth_header(bearer);
    }
    if let Some(value) = traceparent.and_then(|value| HeaderValue::from_str(value).ok()) {
        transport = transport.custom_headers(
            [(HeaderName::from_static("traceparent"), value)]
                .into_iter()
                .collect(),
        );
    }
    ClientConfig::default()
        .serve(StreamableHttpClientTransport::from_config(transport))
        .await
        .map_err(|e| error(upstream, e))
}

fn read_token(path: &Path) -> std::io::Result<String> {
    Ok(std::fs::read_to_string(path)?.trim().to_string())
}

/// The upstream's current tools as registry versions.
pub async fn list_tools(
    upstream: &UpstreamConfig,
) -> Result<Vec<(String, ToolVersion)>, UpstreamError> {
    let bearer = upstream
        .token_file
        .as_deref()
        .map(read_token)
        .transpose()
        .map_err(|e| error(upstream, e))?;
    let client = connect(upstream, bearer, None).await?;
    let tools = client
        .list_all_tools()
        .await
        .map_err(|e| error(upstream, e));
    let _ = client.cancel().await;
    Ok(tools?
        .into_iter()
        .map(|tool| {
            (
                tool.name.to_string(),
                ToolVersion {
                    description: tool.description.map(|text| text.to_string()),
                    input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
                },
            )
        })
        .collect())
}

pub async fn call_tool(
    upstream: &UpstreamConfig,
    tool: &str,
    arguments: Option<JsonObject>,
    delegated_token: String,
    traceparent: &str,
) -> Result<CallToolResult, UpstreamError> {
    let client = connect(upstream, Some(delegated_token), Some(traceparent)).await?;
    let mut params = CallToolRequestParams::new(tool.to_string());
    params.arguments = arguments;
    let result = client
        .call_tool(params)
        .await
        .map_err(|e| error(upstream, e));
    let _ = client.cancel().await;
    result
}
