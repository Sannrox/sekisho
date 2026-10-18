//! The MCP server behind `/mcp`.
//!
//! Streamable HTTP, protocol negotiation, Host and Origin validation, and
//! the request-size cap come from the official Rust SDK. 2026-07-28 is
//! served statelessly; 2025-11-25 clients negotiate down to it. Tools come
//! from the [`Federation`]: only owner-enabled registry tools are listed or
//! callable, with their approved description.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use tokio_util::sync::CancellationToken;

use crate::config::{McpConfig, PolicyConfig, UpstreamConfig};
use crate::policy::{AgentPolicy, PolicyFile, Refusal, agent_id, audit_refusal};
use crate::registry::{self, Registry};
use crate::verifier::Principal;

/// Longest wait for one upstream's tool listing during a refresh.
const LISTING_TIMEOUT: Duration = Duration::from_secs(10);

/// Protocol versions Sekisho speaks, newest first.
pub const PROTOCOL_VERSIONS: &[ProtocolVersion] =
    &[ProtocolVersion::V_2026_07_28, ProtocolVersion::V_2025_11_25];

/// Upstreams and the registry snapshot the data plane serves from.
pub struct Federation {
    pub upstreams: Vec<UpstreamConfig>,
    pub registry_file: PathBuf,
    registry: RwLock<Registry>,
}

impl Federation {
    pub fn new(upstreams: Vec<UpstreamConfig>, registry_file: PathBuf) -> Self {
        Self {
            upstreams,
            registry_file,
            registry: RwLock::default(),
        }
    }

    pub fn snapshot(&self) -> Registry {
        self.registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Re-list every upstream, merge the listings into the registry file,
    /// and serve the result. An unreachable upstream lists nothing, so its
    /// tools stop being served until it answers again.
    pub async fn refresh(&self) -> Result<(), registry::RegistryError> {
        // Concurrent and bounded, so one stalled upstream cannot hold back
        // the others or owner edits.
        let mut listings = tokio::task::JoinSet::new();
        for upstream in self.upstreams.clone() {
            listings.spawn(async move {
                let listed =
                    tokio::time::timeout(LISTING_TIMEOUT, crate::upstream::list_tools(&upstream))
                        .await
                        .unwrap_or_else(|_| Err(crate::upstream::timed_out(&upstream)))
                        .unwrap_or_else(|error| {
                            eprintln!("{error}");
                            Vec::new()
                        });
                (upstream.name, listed)
            });
        }
        let listings = listings.join_all().await;
        let configured: Vec<&str> = self.upstreams.iter().map(|u| u.name.as_str()).collect();
        let ((), registry) = registry::edit(&self.registry_file, |registry| {
            registry.retain_upstreams(&configured);
            for (upstream, listed) in &listings {
                registry.observe(upstream, listed);
            }
            Ok(())
        })?;
        *self
            .registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = registry;
        Ok(())
    }

    /// Refresh every `interval` until `shutdown`.
    pub async fn run(self: Arc<Self>, interval: Duration, shutdown: CancellationToken) {
        loop {
            if let Err(error) = self.refresh().await {
                eprintln!("registry refresh failed: {error}");
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(interval) => {}
            }
        }
    }
}

#[derive(Clone)]
pub struct Gateway {
    federation: Arc<Federation>,
    policy: Arc<PolicyConfig>,
}

impl Gateway {
    /// The caller's agent and its policy, loaded fresh. Fails closed when
    /// the principal is missing or the policy file cannot be loaded.
    fn caller(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<(Principal, Option<AgentPolicy>), McpError> {
        let principal = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Principal>())
            .cloned()
            .ok_or_else(|| McpError::internal_error("request has no verified principal", None))?;
        let file = PolicyFile::load(&self.policy.file).map_err(|error| {
            eprintln!("{error}");
            McpError::internal_error("agent policy is unavailable", None)
        })?;
        // A malformed actor matches no agent, so it is refused below.
        let agent = agent_id(&principal).and_then(|id| file.agent(&id).cloned());
        Ok((principal, agent))
    }

    fn refuse(&self, principal: &Principal, tool: &str, reason: &str) -> McpError {
        let refusal = Refusal {
            at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |at| at.as_millis()),
            subject: principal.subject.clone(),
            agent: agent_id(principal).unwrap_or_else(|| "<malformed act>".into()),
            tool: tool.into(),
            reason: reason.into(),
        };
        if let Err(error) = audit_refusal(&self.policy.audit_file, &refusal) {
            eprintln!("recording refusal failed: {error}");
        }
        McpError::invalid_params(format!("tool {tool} is not allowed: {reason}"), None)
    }
}

impl ServerHandler for Gateway {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("sekisho", env!("CARGO_PKG_VERSION")))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(PROTOCOL_VERSIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let (_, agent) = self.caller(&context)?;
        let allowed = |key: &str| {
            agent
                .as_ref()
                .is_some_and(|agent| agent.tools.iter().any(|tool| tool == key))
        };
        let registry = self.federation.snapshot();
        let tools = registry
            .tools
            .iter()
            .filter(|(key, entry)| entry.served() && allowed(key))
            .map(|(key, entry)| {
                let version = entry.approved_version();
                let schema = match &version.input_schema {
                    serde_json::Value::Object(schema) => schema.clone(),
                    _ => Default::default(),
                };
                Tool::new_with_raw(
                    key.clone(),
                    version.description.clone().map(Cow::Owned),
                    Arc::new(schema),
                )
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let (principal, agent) = match self.caller(&context) {
            Ok(caller) => caller,
            Err(error) => {
                // Still audit, under whatever principal the request carries.
                if let Some(principal) = context
                    .extensions
                    .get::<axum::http::request::Parts>()
                    .and_then(|parts| parts.extensions.get::<Principal>())
                {
                    self.refuse(principal, &request.name, "agent policy is unavailable");
                }
                return Err(error);
            }
        };
        let Some(agent) = agent else {
            return Err(self.refuse(&principal, &request.name, "agent has no policy"));
        };
        if !agent.tools.iter().any(|tool| *tool == request.name) {
            return Err(self.refuse(&principal, &request.name, "not in the agent allowlist"));
        }
        let entry = self
            .federation
            .snapshot()
            .tools
            .get(request.name.as_ref())
            .filter(|entry| entry.served())
            .cloned()
            .ok_or_else(|| {
                McpError::invalid_params(format!("tool {} is not enabled", request.name), None)
            })?;
        let upstream = self
            .federation
            .upstreams
            .iter()
            .find(|upstream| upstream.name == entry.upstream)
            .ok_or_else(|| McpError::internal_error("tool upstream is not configured", None))?;
        // A cancelled or disconnected caller drops the upstream call, which
        // closes its connection.
        tokio::select! {
            result = crate::upstream::call_tool(upstream, &entry.name, request.arguments) => result
                .map(CallToolResponse::Complete)
                .map_err(|error| McpError::internal_error(error.to_string(), None)),
            () = context.ct.cancelled() => Err(McpError::internal_error("request cancelled", None)),
        }
    }
}

/// Stateless Streamable HTTP service. `shutdown` ends in-flight streams.
pub fn service(
    config: &McpConfig,
    federation: Arc<Federation>,
    policy: PolicyConfig,
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
    let policy = Arc::new(policy);
    StreamableHttpService::new(
        move || {
            Ok(Gateway {
                federation: federation.clone(),
                policy: policy.clone(),
            })
        },
        Arc::new(NeverSessionManager::default()),
        http,
    )
}
