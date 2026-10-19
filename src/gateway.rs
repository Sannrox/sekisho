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
use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use tokio_util::sync::CancellationToken;

use crate::audit::{CallAudit, CallEvent, Decision, traceparent};
use crate::config::{McpConfig, PolicyConfig, UpstreamConfig};
use crate::discovery;
use crate::exchange::Exchanger;
use crate::limits::{Caller, Limiter, LimitsConfig, cap_response};
use crate::metrics::Metrics;
use crate::policy::{AgentPolicy, PolicyFile, agent_id};
use crate::registry::{self, Registry};
use crate::verifier::{CallerToken, Principal};

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
    exchanger: Arc<Exchanger>,
    audit: Arc<CallAudit>,
    metrics: Arc<Metrics>,
    progressive: bool,
    limiter: Arc<Limiter>,
}

/// Why a call did not produce an upstream answer.
struct Refused {
    decision: Decision,
    reason: String,
    error: McpError,
}

impl Refused {
    fn denied(tool: &str, reason: &str) -> Self {
        Self {
            decision: Decision::Denied,
            reason: reason.into(),
            error: McpError::invalid_params(format!("tool {tool} is not allowed: {reason}"), None),
        }
    }

    fn failed(reason: &str, error: McpError) -> Self {
        Self {
            decision: Decision::Failed,
            reason: reason.into(),
            error,
        }
    }
}

impl Gateway {
    pub fn new(
        federation: Arc<Federation>,
        policy: PolicyConfig,
        exchanger: Exchanger,
        audit: CallAudit,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            federation,
            policy: Arc::new(policy),
            exchanger: Arc::new(exchanger),
            audit: Arc::new(audit),
            metrics,
            progressive: false,
            limiter: Arc::new(Limiter::new(LimitsConfig::default())),
        }
    }

    /// Rate limits and response and time caps.
    pub fn limits(mut self, limiter: Limiter) -> Self {
        self.limiter = Arc::new(limiter);
        self
    }

    /// List only the discovery meta-tools.
    pub fn progressive(mut self, progressive: bool) -> Self {
        self.progressive = progressive;
        self
    }

    /// Enabled registry tools on this agent's allowlist, with the approved
    /// version agents see.
    fn visible(&self, agent: Option<&AgentPolicy>) -> Vec<(String, crate::registry::ToolVersion)> {
        let Some(agent) = agent else {
            return Vec::new();
        };
        self.federation
            .snapshot()
            .tools
            .into_iter()
            .filter(|(key, entry)| entry.served() && agent.tools.contains(key))
            .map(|(key, entry)| (key, entry.approved_version().clone()))
            .collect()
    }

    /// The caller's agent policy, loaded fresh. Fails closed when the policy
    /// file cannot be loaded; a malformed actor matches no agent.
    fn agent_policy(&self, principal: &Principal) -> Result<Option<AgentPolicy>, McpError> {
        let file = PolicyFile::load(&self.policy.file).map_err(|error| {
            eprintln!("{error}");
            McpError::internal_error("agent policy is unavailable", None)
        })?;
        Ok(agent_id(principal).and_then(|id| file.agent(&id).cloned()))
    }

    /// Policy, registry, exchange and upstream call for one tool call.
    /// Returns the response and its encoded size.
    async fn forward(
        &self,
        request: CallToolRequestParams,
        context: &RequestContext<RoleServer>,
        principal: Option<&Principal>,
        trace_id: &str,
        invalid_invoke: Option<&str>,
    ) -> Result<(CallToolResponse, usize), Refused> {
        let tool = request.name.to_string();
        let principal = principal.ok_or_else(|| Refused::denied(&tool, "no verified principal"))?;
        let agent = self
            .agent_policy(principal)
            .map_err(|error| Refused {
                decision: Decision::Denied,
                reason: "agent policy is unavailable".into(),
                error,
            })?
            .ok_or_else(|| Refused::denied(&tool, "agent has no policy"))?;
        // Every call is charged, including meta-tools and calls the
        // allowlist will refuse, so floods are throttled either way.
        let caller = Caller {
            agent: &agent.id,
            user: &principal.subject,
            tenant: principal.tenant.as_deref(),
        };
        let admitted = if self.limiter.is_shared() {
            let limiter = Arc::clone(&self.limiter);
            let agent_id = agent.id.clone();
            let user = principal.subject.clone();
            let tenant = principal.tenant.clone();
            let tool = tool.clone();
            tokio::task::spawn_blocking(move || {
                limiter.admit(
                    &Caller {
                        agent: &agent_id,
                        user: &user,
                        tenant: tenant.as_deref(),
                    },
                    &tool,
                    Instant::now(),
                )
            })
            .await
            .unwrap_or_else(|_| Err(Duration::from_secs(1)))
        } else {
            self.limiter.admit(&caller, &tool, Instant::now())
        };
        if let Err(wait) = admitted {
            // Round up so waiting exactly this long is enough.
            let retry_after_ms = wait.as_micros().div_ceil(1_000).max(1);
            return Err(Refused {
                decision: Decision::Throttled,
                reason: format!("rate limited; retry after {retry_after_ms} ms"),
                error: McpError::invalid_request(
                    format!("rate limited; retry after {retry_after_ms} ms"),
                    Some(serde_json::json!({ "retry_after_ms": retry_after_ms })),
                ),
            });
        }
        if let Some(reason) = invalid_invoke {
            return Err(Refused::denied(&tool, reason));
        }
        if tool == discovery::SEARCH || tool == discovery::SCHEMA {
            let visible = self.visible(Some(&agent));
            let field = |name| {
                request
                    .arguments
                    .as_ref()
                    .and_then(|arguments| arguments.get(name))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            };
            let answer = if tool == discovery::SEARCH {
                let query =
                    field("query").ok_or_else(|| Refused::denied(&tool, "query is required"))?;
                discovery::search(&visible, &query)
            } else {
                let name =
                    field("name").ok_or_else(|| Refused::denied(&tool, "name is required"))?;
                discovery::schema(&visible, &name)
                    .ok_or_else(|| Refused::denied(&name, "not in the agent allowlist"))?
            };
            let (result, truncated) = cap_response(
                CallToolResult::structured(answer),
                self.limiter.config().max_response_bytes,
            );
            if truncated {
                self.metrics.record_truncated();
            }
            let size = serde_json::to_vec(&result).map_or(0, |bytes| bytes.len());
            return Ok((CallToolResponse::Complete(result), size));
        }
        if !agent.tools.contains(&tool) {
            return Err(Refused::denied(&tool, "not in the agent allowlist"));
        }
        let entry = self
            .federation
            .snapshot()
            .tools
            .get(&tool)
            .filter(|entry| entry.served())
            .cloned()
            .ok_or_else(|| Refused::denied(&tool, "tool is not enabled"))?;
        let upstream = self
            .federation
            .upstreams
            .iter()
            .find(|upstream| upstream.name == entry.upstream)
            .ok_or_else(|| Refused::denied(&tool, "tool upstream is not configured"))?;
        let caller_token = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<CallerToken>())
            .cloned()
            .ok_or_else(|| Refused::denied(&tool, "request has no caller token"))?;
        let delegated = self
            .exchanger
            .token(crate::exchange::Request {
                caller_token: &caller_token.0,
                agent: &agent.id,
                ceiling: &agent.ceiling,
                audience: &upstream.audience,
                tool: &entry.name,
            })
            .await
            .map_err(|error| {
                eprintln!("{error}");
                match error {
                    crate::exchange::ExchangeError::Failed(_) => Refused::failed(
                        "token exchange failed",
                        McpError::internal_error("token exchange failed", None),
                    ),
                    _ => Refused::denied(&tool, "token exchange refused"),
                }
            })?;
        let traceparent = traceparent(trace_id);
        let call = crate::upstream::call_tool(
            upstream,
            &entry.name,
            request.arguments,
            delegated,
            &traceparent,
        );
        // A cancelled or disconnected caller drops the upstream call, which
        // closes its connection.
        let limit = Duration::from_millis(self.limiter.config().max_call_ms);
        let call = async move {
            tokio::time::timeout(limit, call)
                .await
                .unwrap_or_else(|_| Err(crate::upstream::timed_out_call(upstream)))
        };
        let result = tokio::select! {
            result = call => result.map_err(|error| {
                Refused::failed("upstream call failed", McpError::internal_error(error.to_string(), None))
            })?,
            () = context.ct.cancelled() => {
                return Err(Refused::failed("request cancelled", McpError::internal_error("request cancelled", None)));
            }
        };
        let (result, truncated) = cap_response(result, self.limiter.config().max_response_bytes);
        if truncated {
            self.metrics.record_truncated();
        }
        let size = serde_json::to_vec(&result).map_or(0, |bytes| bytes.len());
        Ok((CallToolResponse::Complete(result), size))
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
        let principal = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Principal>())
            .ok_or_else(|| McpError::internal_error("request has no verified principal", None))?;
        let agent = self.agent_policy(principal)?;
        let mut tools = discovery::meta_tools();
        if !self.progressive {
            tools.extend(
                self.visible(agent.as_ref())
                    .into_iter()
                    .map(|(key, version)| {
                        let schema = match version.input_schema {
                            serde_json::Value::Object(schema) => schema,
                            _ => Default::default(),
                        };
                        Tool::new_with_raw(
                            key,
                            version.description.map(Cow::Owned),
                            Arc::new(schema),
                        )
                    }),
            );
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let started = Instant::now();
        // `invoke_tool` is a call of its target: same checks, same audit.
        // A malformed one is refused and audited under the wrapper's name.
        let mut invalid_invoke = None;
        if request.name == discovery::INVOKE {
            match discovery::invoke_target(request.arguments.as_ref()) {
                None => invalid_invoke = Some("invoke_tool needs a name and object arguments"),
                Some((name, _)) if name.starts_with("sekisho.") => {
                    invalid_invoke = Some("invoke_tool cannot call a meta-tool");
                }
                Some((name, arguments)) => {
                    request.name = name.into();
                    request.arguments = arguments;
                }
            }
        }
        let parts = context.extensions.get::<axum::http::request::Parts>();
        let trace_id = crate::audit::trace_id(
            parts
                .and_then(|parts| parts.headers.get("traceparent"))
                .and_then(|value| value.to_str().ok()),
        );
        let principal = parts
            .and_then(|parts| parts.extensions.get::<Principal>())
            .cloned();
        let tool = request.name.to_string();
        let outcome = self
            .forward(
                request,
                &context,
                principal.as_ref(),
                &trace_id,
                invalid_invoke,
            )
            .await;
        let (decision, reason, response_bytes) = match &outcome {
            Ok((_, size)) => (Decision::Allowed, None, Some(*size)),
            Err(refused) => (refused.decision, Some(refused.reason.clone()), None),
        };
        self.metrics.record_call(decision);
        let audit = Arc::clone(&self.audit);
        let event = CallEvent {
            at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |at| at.as_millis()),
            trace_id,
            subject: principal
                .as_ref()
                .map_or_else(String::new, |principal| principal.subject.clone()),
            agent: principal
                .as_ref()
                .and_then(agent_id)
                .unwrap_or_else(|| "<unknown>".into()),
            tool,
            decision,
            reason,
            latency_ms: started.elapsed().as_millis(),
            response_bytes,
        };
        // Durable before the response; fsync stays off the worker.
        let _ = tokio::task::spawn_blocking(move || audit.record(&event)).await;
        outcome
            .map(|(response, _)| response)
            .map_err(|refused| refused.error)
    }
}

/// Stateless Streamable HTTP service. `shutdown` ends in-flight streams.
pub fn service(
    config: &McpConfig,
    gateway: Gateway,
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
        move || Ok(gateway.clone()),
        Arc::new(NeverSessionManager::default()),
        http,
    )
}
