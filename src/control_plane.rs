//! Control-plane [`DecisionVerifier`] (#43).
//!
//! In platform mode, apply and lift look up the named Action on the engine's
//! HTTP/JSON projection (`GetActionInstance`, and `GetObject` for a lift).
//! The engine stays the source of truth: this client checks that the Action
//! is decided, is a mitigating-control type, and names this mitigation's
//! target (and tenant, when scoped). It re-implements no grants or markings.
//! Unreachable, unknown, parked, denied, or mismatched references are
//! refused; messages never echo tokens.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::mitigations::{Change, DecisionCheck, DecisionVerifier, Scope, Target};

/// Apply one catalogue control after the remediation plan is decided.
pub const ACTION_APPLY: &str = "security.mitigation.apply";
/// Lift an applied mitigation after that lift Action is decided.
pub const ACTION_LIFT: &str = "security.mitigation.lift";
pub const CONTROL_GATEWAY_TOOL: &str = "gateway_tool_disable";
pub const CONTROL_OUTBOUND_DENY: &str = "outbound_deny";
pub const KIND_MITIGATION: &str = "security_mitigation";

const GET_ACTION: &str = "/sekai.SekaiService/GetActionInstance";
const GET_OBJECT: &str = "/sekai.SekaiService/GetObject";
const DEFAULT_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    #[error("reading control-plane token {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("control-plane token file is empty")]
    EmptyToken,
    #[error("{0}")]
    Invalid(String),
}

/// How the gateway reaches the engine to verify a mitigation decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlPlaneConfig {
    /// HTTP/JSON projection origin (`http://host:port`, no trailing path).
    pub endpoint: String,
    /// Bearer token used to read Actions; never inlined in the config.
    pub token_file: PathBuf,
    /// Namespace the lookup is authorized for (`x-sekai-namespace`).
    pub namespace: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

impl ControlPlaneConfig {
    pub fn resolve(&mut self, base: &std::path::Path) {
        self.token_file = base.join(&self.token_file);
    }

    pub fn validate(&self) -> Result<(), String> {
        let endpoint = url::Url::parse(self.endpoint.trim())
            .map_err(|error| format!("control-plane endpoint: {error}"))?;
        if endpoint.scheme() != "http" && endpoint.scheme() != "https" {
            return Err("control-plane endpoint must be http or https".into());
        }
        if endpoint.host_str().is_none_or(|host| host.is_empty()) {
            return Err("control-plane endpoint needs a host".into());
        }
        if !endpoint.path().is_empty() && endpoint.path() != "/"
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(
                "control-plane endpoint is an origin (scheme://host[:port]), not an RPC path"
                    .into(),
            );
        }
        if self.namespace.trim().is_empty() {
            return Err("control-plane namespace must not be empty".into());
        }
        if self.timeout_ms == 0 {
            return Err("control-plane timeout_ms must be positive".into());
        }
        Ok(())
    }
}

/// Looks up a decided mitigating-control Action on the engine.
pub struct ControlPlane {
    endpoint: String,
    token: String,
    namespace: String,
    client: reqwest::blocking::Client,
}

impl std::fmt::Debug for ControlPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlane")
            .field("endpoint", &self.endpoint)
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl ControlPlane {
    pub fn from_config(config: &ControlPlaneConfig) -> Result<Self, ControlPlaneError> {
        config.validate().map_err(ControlPlaneError::Invalid)?;
        let token = std::fs::read(&config.token_file)
            .map_err(|source| ControlPlaneError::Read {
                path: config.token_file.display().to_string(),
                source,
            })?
            .trim_ascii()
            .to_vec();
        if token.is_empty() {
            return Err(ControlPlaneError::EmptyToken);
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|error| ControlPlaneError::Invalid(error.to_string()))?;
        Ok(Self {
            endpoint: config.endpoint.trim_end_matches('/').to_string(),
            token: String::from_utf8(token).map_err(|_| {
                ControlPlaneError::Invalid("control-plane token is not UTF-8".into())
            })?,
            namespace: config.namespace.clone(),
            client,
        })
    }

    fn lookup_action(&self, decision: &str) -> Result<ActionView, String> {
        match self.post(GET_ACTION, json!({ "instanceId": decision })) {
            Ok(body) => action_from(body),
            Err(Lookup::NotFound) => {
                match self.post(GET_ACTION, json!({ "operationId": decision })) {
                    Ok(body) => action_from(body),
                    Err(Lookup::NotFound) => Err("action not found".into()),
                    Err(Lookup::Failed(reason)) => Err(reason),
                }
            }
            Err(Lookup::Failed(reason)) => Err(reason),
        }
    }

    fn lookup_object(&self, id: &str) -> Result<ObjectView, String> {
        match self.post(GET_OBJECT, json!({ "id": id })) {
            Ok(body) => object_from(body),
            Err(Lookup::NotFound) => Err("mitigation object not found".into()),
            Err(Lookup::Failed(reason)) => Err(reason),
        }
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, Lookup> {
        let response = self
            .client
            .post(format!("{}{path}", self.endpoint))
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-sekai-namespace", &self.namespace)
            .json(&body)
            .send()
            .map_err(|error| {
                Lookup::Failed(format!(
                    "control plane unreachable: {}",
                    error.without_url()
                ))
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(Lookup::NotFound);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(Lookup::Failed("control plane refused the lookup".into()));
        }
        if !status.is_success() {
            return Err(Lookup::Failed(format!(
                "control plane answered {}",
                status.as_u16()
            )));
        }
        response
            .json()
            .map_err(|_| Lookup::Failed("control plane response is not an action".into()))
    }
}

enum Lookup {
    NotFound,
    Failed(String),
}

#[derive(Debug, Clone)]
struct ActionView {
    type_id: String,
    status: String,
    approval_decision: String,
    namespace: String,
    parameters: Value,
}

#[derive(Debug, Clone)]
struct ObjectView {
    kind: String,
    namespace: String,
    control: String,
    target: String,
}

impl DecisionVerifier for ControlPlane {
    fn verify(&self, check: &DecisionCheck<'_>) -> Result<(), String> {
        if check.operator.trim().is_empty() {
            return Err("an operator must be named".into());
        }
        let action = self.lookup_action(check.decision)?;
        accept_action(check, &action)?;
        if check.change == Change::Lift {
            let object_id = string_param(&action.parameters, "object_id")
                .ok_or_else(|| "lift action does not name a mitigation object".to_string())?;
            accept_object(check, &self.lookup_object(object_id)?)?;
        }
        Ok(())
    }
}

fn accept_action(check: &DecisionCheck<'_>, action: &ActionView) -> Result<(), String> {
    decided(action)?;
    let expected = match check.change {
        Change::Apply => ACTION_APPLY,
        Change::Lift => ACTION_LIFT,
    };
    if action.type_id != expected {
        return Err(format!("action is not a {expected} mitigating control"));
    }
    accept_namespace(check, &action.namespace)?;
    if check.change == Change::Apply {
        let control = string_param(&action.parameters, "control")
            .ok_or_else(|| "action does not name this mitigation's scope".to_string())?;
        let target = string_param(&action.parameters, "target")
            .ok_or_else(|| "action does not name this mitigation's scope".to_string())?;
        accept_scope(check, control, target)?;
    }
    Ok(())
}

fn accept_object(check: &DecisionCheck<'_>, object: &ObjectView) -> Result<(), String> {
    if object.kind != KIND_MITIGATION {
        return Err("object is not a security mitigation".into());
    }
    accept_namespace(check, &object.namespace)?;
    accept_scope(check, &object.control, &object.target)
}

fn accept_namespace(check: &DecisionCheck<'_>, namespace: &str) -> Result<(), String> {
    match &check.mitigation.scope {
        Scope::Tenant { tenant } if tenant != namespace => {
            Err("action does not name this mitigation's scope".into())
        }
        _ => Ok(()),
    }
}

fn accept_scope(check: &DecisionCheck<'_>, control: &str, target: &str) -> Result<(), String> {
    if control == gateway_control(&check.mitigation.target)
        && target == named_target(&check.mitigation.target)
    {
        Ok(())
    } else {
        Err("action does not name this mitigation's scope".into())
    }
}

fn decided(action: &ActionView) -> Result<(), String> {
    match action.status.as_str() {
        "admitted" => {}
        "parked" => return Err("action is not decided".into()),
        "denied" => return Err("action was denied".into()),
        other => return Err(format!("action status {other:?} is not decided")),
    }
    match action.approval_decision.as_str() {
        "" | "grant" => Ok(()),
        other => Err(format!("action decision {other:?} is not a grant")),
    }
}

fn gateway_control(target: &Target) -> &'static str {
    match target {
        Target::Tool { .. } | Target::Upstream { .. } => CONTROL_GATEWAY_TOOL,
        Target::Destination { .. } => CONTROL_OUTBOUND_DENY,
    }
}

fn named_target(target: &Target) -> &str {
    match target {
        Target::Tool { tool } => tool,
        Target::Upstream { upstream } => upstream,
        Target::Destination { destination } => destination,
    }
}

fn action_from(body: Value) -> Result<ActionView, String> {
    let instance = body
        .get("instance")
        .ok_or_else(|| "control plane response is not an action".to_string())?;
    let parameters = match instance.get("parametersJson").and_then(Value::as_str) {
        Some(raw) if !raw.trim().is_empty() => {
            serde_json::from_str(raw).map_err(|_| "action parameters are not JSON".to_string())?
        }
        _ => Value::Null,
    };
    Ok(ActionView {
        type_id: field(instance, "typeId")?,
        status: field(instance, "status")?,
        approval_decision: instance
            .get("approvalDecision")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        namespace: field(instance, "namespace")?,
        parameters,
    })
}

fn object_from(body: Value) -> Result<ObjectView, String> {
    let object = body
        .get("object")
        .ok_or_else(|| "control plane response is not an object".to_string())?;
    let properties = object.get("properties").cloned().unwrap_or(Value::Null);
    Ok(ObjectView {
        kind: field(object, "kind")?,
        namespace: field(object, "namespace")?,
        control: string_param(&properties, "control")
            .ok_or_else(|| "mitigation object has no control".to_string())?
            .to_string(),
        target: string_param(&properties, "target")
            .ok_or_else(|| "mitigation object has no target".to_string())?
            .to_string(),
    })
}

fn field(value: &Value, name: &str) -> Result<String, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "control plane response is not an action".into())
}

fn string_param<'a>(parameters: &'a Value, name: &str) -> Option<&'a str> {
    parameters
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mitigations::{Mitigation, Scope, Target};
    use axum::Json;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Plane {
        token: String,
        namespace: String,
        actions: Arc<Mutex<HashMap<String, Value>>>,
        objects: Arc<Mutex<HashMap<String, Value>>>,
    }

    async fn get_action(
        State(plane): State<Plane>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        if !authorized(&plane, &headers) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"code": "unauthenticated", "message": "missing"})),
            );
        }
        let id = body
            .get("instanceId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .or_else(|| body.get("operationId").and_then(Value::as_str));
        match id.and_then(|id| plane.actions.lock().unwrap().get(id).cloned()) {
            Some(instance) => (StatusCode::OK, Json(json!({ "instance": instance }))),
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({"code": "not_found", "message": "action instance not found"})),
            ),
        }
    }

    async fn get_object(
        State(plane): State<Plane>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        if !authorized(&plane, &headers) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"code": "unauthenticated", "message": "missing"})),
            );
        }
        let id = body.get("id").and_then(Value::as_str);
        match id.and_then(|id| plane.objects.lock().unwrap().get(id).cloned()) {
            Some(object) => (StatusCode::OK, Json(json!({ "object": object }))),
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({"code": "not_found", "message": "not found"})),
            ),
        }
    }

    fn authorized(plane: &Plane, headers: &HeaderMap) -> bool {
        let token = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let namespace = headers
            .get("x-sekai-namespace")
            .and_then(|value| value.to_str().ok());
        token == Some(plane.token.as_str()) && namespace == Some(plane.namespace.as_str())
    }

    struct Live {
        plane: ControlPlane,
        actions: Arc<Mutex<HashMap<String, Value>>>,
        objects: Arc<Mutex<HashMap<String, Value>>>,
        _stop: tokio_util::sync::CancellationToken,
    }

    fn start(token: &str, namespace: &str) -> Live {
        let actions: Arc<Mutex<HashMap<String, Value>>> = Default::default();
        let objects: Arc<Mutex<HashMap<String, Value>>> = Default::default();
        let stop = tokio_util::sync::CancellationToken::new();
        let state = Plane {
            token: token.into(),
            namespace: namespace.into(),
            actions: Arc::clone(&actions),
            objects: Arc::clone(&objects),
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = stop.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let app = Router::new()
                    .route(GET_ACTION, post(get_action))
                    .route(GET_OBJECT, post(get_object))
                    .with_state(state);
                axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown.cancelled_owned())
                    .await
                    .unwrap();
            });
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(20))
                .is_ok()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("token");
        std::fs::write(&token_file, token).unwrap();
        let plane = ControlPlane::from_config(&ControlPlaneConfig {
            endpoint: format!("http://{addr}"),
            token_file,
            namespace: namespace.into(),
            timeout_ms: 2_000,
        })
        .unwrap();
        Live {
            plane,
            actions,
            objects,
            _stop: stop,
        }
    }

    fn disable_echo() -> Mitigation {
        Mitigation {
            decision: "act-apply".into(),
            operator: "ops-1".into(),
            target: Target::Tool {
                tool: "tools.echo".into(),
            },
            scope: Scope::All,
            applied_at_unix_ms: 0,
            expires_at_unix_ms: None,
        }
    }

    fn apply_check<'a>(mitigation: &'a Mitigation, decision: &'a str) -> DecisionCheck<'a> {
        DecisionCheck {
            change: Change::Apply,
            decision,
            operator: &mitigation.operator,
            mitigation,
        }
    }

    fn lift_check<'a>(mitigation: &'a Mitigation, decision: &'a str) -> DecisionCheck<'a> {
        DecisionCheck {
            change: Change::Lift,
            decision,
            operator: &mitigation.operator,
            mitigation,
        }
    }

    fn admitted_apply(id: &str, control: &str, target: &str) -> (String, Value) {
        (
            id.into(),
            json!({
                "instanceId": id,
                "namespace": "acme",
                "typeId": ACTION_APPLY,
                "status": "admitted",
                "approvalDecision": "grant",
                "parametersJson": serde_json::to_string(&json!({
                    "object_id": "m-1",
                    "remediation_id": "r-1",
                    "control": control,
                    "target": target,
                    "state": "applied"
                })).unwrap(),
            }),
        )
    }

    #[test]
    fn a_decided_apply_matching_the_tool_is_accepted() {
        let live = start("plane-token", "acme");
        let (id, instance) = admitted_apply("act-apply", CONTROL_GATEWAY_TOOL, "tools.echo");
        live.actions.lock().unwrap().insert(id.clone(), instance);
        live.plane
            .verify(&apply_check(&disable_echo(), &id))
            .unwrap();
    }

    #[test]
    fn an_operation_id_lookup_is_accepted() {
        let live = start("plane-token", "acme");
        let (_, instance) = admitted_apply("act-apply", CONTROL_GATEWAY_TOOL, "tools.echo");
        live.actions.lock().unwrap().insert("op-9".into(), instance);
        live.plane
            .verify(&apply_check(&disable_echo(), "op-9"))
            .unwrap();
    }

    #[test]
    fn parked_unknown_and_mismatched_actions_are_refused() {
        let live = start("plane-token", "acme");
        let mitigation = disable_echo();
        assert_eq!(
            live.plane
                .verify(&apply_check(&mitigation, "missing"))
                .unwrap_err(),
            "action not found"
        );

        let (id, mut parked) = admitted_apply("act-parked", CONTROL_GATEWAY_TOOL, "tools.echo");
        parked["status"] = "parked".into();
        parked["approvalDecision"] = "".into();
        live.actions.lock().unwrap().insert(id.clone(), parked);
        assert_eq!(
            live.plane
                .verify(&apply_check(&mitigation, &id))
                .unwrap_err(),
            "action is not decided"
        );

        let (id, other) = admitted_apply("act-other", CONTROL_GATEWAY_TOOL, "tools.other");
        live.actions.lock().unwrap().insert(id.clone(), other);
        assert_eq!(
            live.plane
                .verify(&apply_check(&mitigation, &id))
                .unwrap_err(),
            "action does not name this mitigation's scope"
        );

        let (id, freeze) = admitted_apply("act-freeze", "freeze", "records.delete");
        live.actions.lock().unwrap().insert(id.clone(), freeze);
        assert_eq!(
            live.plane
                .verify(&apply_check(&mitigation, &id))
                .unwrap_err(),
            "action does not name this mitigation's scope"
        );
    }

    #[test]
    fn a_tenant_scope_must_match_the_action_namespace() {
        let live = start("plane-token", "acme");
        let (id, instance) = admitted_apply("act-apply", CONTROL_GATEWAY_TOOL, "tools.echo");
        live.actions.lock().unwrap().insert(id.clone(), instance);
        let mut mitigation = disable_echo();
        mitigation.scope = Scope::Tenant {
            tenant: "other".into(),
        };
        assert_eq!(
            live.plane
                .verify(&apply_check(&mitigation, &id))
                .unwrap_err(),
            "action does not name this mitigation's scope"
        );
        mitigation.scope = Scope::Tenant {
            tenant: "acme".into(),
        };
        live.plane.verify(&apply_check(&mitigation, &id)).unwrap();
    }

    #[test]
    fn lifting_needs_a_decided_lift_of_the_matching_object() {
        let live = start("plane-token", "acme");
        let mitigation = disable_echo();
        let (apply_id, apply) = admitted_apply("act-apply", CONTROL_GATEWAY_TOOL, "tools.echo");
        live.actions.lock().unwrap().insert(apply_id.clone(), apply);
        assert_eq!(
            live.plane
                .verify(&lift_check(&mitigation, &apply_id))
                .unwrap_err(),
            "action is not a security.mitigation.lift mitigating control"
        );

        live.actions.lock().unwrap().insert(
            "act-lift".into(),
            json!({
                "instanceId": "act-lift",
                "namespace": "acme",
                "typeId": ACTION_LIFT,
                "status": "admitted",
                "approvalDecision": "grant",
                "parametersJson": "{\"object_id\":\"m-1\",\"state\":\"lifted\"}",
            }),
        );
        live.objects.lock().unwrap().insert(
            "m-1".into(),
            json!({
                "id": "m-1",
                "kind": KIND_MITIGATION,
                "namespace": "acme",
                "properties": {
                    "control": CONTROL_GATEWAY_TOOL,
                    "target": "tools.echo",
                    "state": "applied"
                }
            }),
        );
        live.plane
            .verify(&lift_check(&mitigation, "act-lift"))
            .unwrap();

        live.objects.lock().unwrap().insert(
            "m-1".into(),
            json!({
                "id": "m-1",
                "kind": KIND_MITIGATION,
                "namespace": "acme",
                "properties": {
                    "control": CONTROL_GATEWAY_TOOL,
                    "target": "tools.other",
                    "state": "applied"
                }
            }),
        );
        assert_eq!(
            live.plane
                .verify(&lift_check(&mitigation, "act-lift"))
                .unwrap_err(),
            "action does not name this mitigation's scope"
        );
    }

    #[test]
    fn outbound_deny_matches_a_destination_mitigation() {
        let live = start("plane-token", "acme");
        let mitigation = Mitigation {
            decision: "act-deny".into(),
            operator: "ops-1".into(),
            target: Target::Destination {
                destination: "https://tools.example.test:443".into(),
            },
            scope: Scope::All,
            applied_at_unix_ms: 0,
            expires_at_unix_ms: None,
        };
        let (id, instance) = admitted_apply(
            "act-deny",
            CONTROL_OUTBOUND_DENY,
            "https://tools.example.test:443",
        );
        live.actions.lock().unwrap().insert(id.clone(), instance);
        live.plane.verify(&apply_check(&mitigation, &id)).unwrap();
    }

    #[test]
    fn a_wrong_token_is_refused_without_applying() {
        let live = start("plane-token", "acme");
        let (id, instance) = admitted_apply("act-apply", CONTROL_GATEWAY_TOOL, "tools.echo");
        live.actions.lock().unwrap().insert(id.clone(), instance);
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("token");
        std::fs::write(&token_file, "other-token").unwrap();
        let plane = ControlPlane::from_config(&ControlPlaneConfig {
            endpoint: live.plane.endpoint.clone(),
            token_file,
            namespace: "acme".into(),
            timeout_ms: 2_000,
        })
        .unwrap();
        assert_eq!(
            plane
                .verify(&apply_check(&disable_echo(), &id))
                .unwrap_err(),
            "control plane refused the lookup"
        );
    }

    #[test]
    fn a_decided_action_applies_and_an_unverifiable_one_is_audited() {
        use crate::mitigations::{Action, MitigationError, MitigationStore, Outcome};

        let live = start("plane-token", "acme");
        let (id, instance) = admitted_apply("act-apply", CONTROL_GATEWAY_TOOL, "tools.echo");
        live.actions.lock().unwrap().insert(id.clone(), instance);
        let dir = tempfile::tempdir().unwrap();
        let store = MitigationStore::new(std::sync::Arc::new(crate::state::Directory::in_dir(
            dir.path(),
        )));
        let mut mitigation = disable_echo();
        mitigation.decision = id.clone();
        let (stored_id, outcome) = store.apply(&live.plane, mitigation.clone(), 10).unwrap();
        assert_eq!(outcome, Outcome::Applied);
        assert!(store.load().unwrap().mitigations.contains_key(&stored_id));

        mitigation.decision = "missing".into();
        let error = store.apply(&live.plane, mitigation, 20).unwrap_err();
        assert!(
            matches!(error, MitigationError::Refused(ref reason) if reason == "action not found"),
            "{error}"
        );
        assert_eq!(store.load().unwrap().mitigations.len(), 1);
        let audit: Vec<crate::mitigations::MitigationEvent> =
            std::fs::read_to_string(dir.path().join("mitigations-audit.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(audit[0].action, Action::Apply);
        assert_eq!(audit[1].action, Action::Refuse);
        assert_eq!(audit[1].reason.as_deref(), Some("action not found"));

        live.actions.lock().unwrap().insert(
            "act-lift".into(),
            json!({
                "instanceId": "act-lift",
                "namespace": "acme",
                "typeId": ACTION_LIFT,
                "status": "parked",
                "parametersJson": "{\"object_id\":\"m-1\",\"state\":\"lifted\"}",
            }),
        );
        let error = store
            .lift(&live.plane, &stored_id, "act-lift", "ops-1", 30)
            .unwrap_err();
        assert!(
            matches!(error, MitigationError::Refused(ref reason) if reason == "action is not decided"),
            "{error}"
        );
        assert!(store.load().unwrap().mitigations.contains_key(&stored_id));

        live.actions.lock().unwrap().insert(
            "act-lift".into(),
            json!({
                "instanceId": "act-lift",
                "namespace": "acme",
                "typeId": ACTION_LIFT,
                "status": "admitted",
                "approvalDecision": "grant",
                "parametersJson": "{\"object_id\":\"m-1\",\"state\":\"lifted\"}",
            }),
        );
        live.objects.lock().unwrap().insert(
            "m-1".into(),
            json!({
                "id": "m-1",
                "kind": KIND_MITIGATION,
                "namespace": "acme",
                "properties": {
                    "control": CONTROL_GATEWAY_TOOL,
                    "target": "tools.echo",
                    "state": "applied"
                }
            }),
        );
        assert_eq!(
            store
                .lift(&live.plane, &stored_id, "act-lift", "ops-1", 40)
                .unwrap(),
            Outcome::Lifted
        );
        assert!(store.load().unwrap().mitigations.is_empty());
    }
}
