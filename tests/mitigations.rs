//! Mitigations (#42): a disabled tool is refused for its scope only, every
//! replica enforces a change on its next call, a denied destination is not
//! forwarded to, expiry is audited once, and an unreadable store refuses.

mod support;

use std::time::Duration;

use rmcp::RoleClient;
use rmcp::model::ClientConfig;
use rmcp::service::RunningService;
use sekisho::mitigations::{
    Action, Mitigation, MitigationEvent, MitigationStore, Outcome, Scope, Standalone, Target,
    now_ms,
};
use support::stack::{Stack, echo_call, eventually};

const AGENTS: &str = "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\", \"tools.other\"]\n\n[[agents]]\nid = \"agent-8\"\ntools = [\"tools.echo\"]\n";

type Client = RunningService<RoleClient, ClientConfig>;

async fn started() -> (Stack, MitigationStore) {
    let stack = Stack::start(Some(AGENTS)).await;
    stack.enable_echo().await;
    stack.enable("tools.other").await;
    let store = MitigationStore::new(sekisho::state::open(&stack.config).unwrap());
    (stack, store)
}

/// Wait until `client` can call `tools.echo`, so later refusals are the
/// mitigation's and not a registry refresh still pending.
async fn ready(client: &Client) {
    assert!(
        eventually(Duration::from_secs(5), async || client
            .call_tool(echo_call("warm"))
            .await
            .is_ok())
        .await
    );
}

fn mitigation(target: Target, scope: Scope) -> Mitigation {
    Mitigation {
        decision: "DEC-1".into(),
        operator: "ops-1".into(),
        target,
        scope,
        applied_at_unix_ms: 0,
        expires_at_unix_ms: None,
    }
}

fn echo_disabled(scope: Scope) -> Mitigation {
    mitigation(
        Target::Tool {
            tool: "tools.echo".into(),
        },
        scope,
    )
}

fn call_audit(stack: &Stack) -> Vec<serde_json::Value> {
    std::fs::read_to_string(&stack.config.audit.calls_file)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn mitigation_audit(stack: &Stack) -> Vec<MitigationEvent> {
    std::fs::read_to_string(stack.config.mitigations.audit_file.as_ref().unwrap())
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn other_call() -> rmcp::model::CallToolRequestParams {
    rmcp::model::CallToolRequestParams::new("tools.other")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_tool_is_refused_and_audited_for_its_agent_only() {
    let (stack, store) = started().await;
    let agent_7 = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    let agent_8 = stack.client(serde_json::json!({ "act": "agent-8" })).await;
    ready(&agent_7).await;
    ready(&agent_8).await;
    let forwarded = stack.seen.lock().unwrap().len();

    let scope = Scope::Agent {
        agent: "agent-7".into(),
    };
    let (_, outcome) = store
        .apply(&Standalone, echo_disabled(scope), now_ms())
        .unwrap();
    assert_eq!(outcome, Outcome::Applied);

    let error = agent_7.call_tool(echo_call("x")).await.unwrap_err();
    assert!(format!("{error}").contains("tool is disabled"), "{error}");
    assert_eq!(stack.seen.lock().unwrap().len(), forwarded, "not forwarded");
    let refused = call_audit(&stack)
        .into_iter()
        .rfind(|record| record["decision"] == "denied")
        .expect("audited refusal");
    assert_eq!(refused["agent"], "agent-7");
    assert_eq!(refused["mitigation"], "DEC-1");
    assert_eq!(refused["trace_id"].as_str().unwrap().len(), 32);

    agent_8.call_tool(echo_call("x")).await.unwrap();
    agent_7.call_tool(other_call()).await.unwrap();
    let _ = agent_7.cancel().await;
    let _ = agent_8.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn both_replicas_enforce_apply_and_lift_on_the_next_call() {
    let (stack, store) = started().await;
    let a = stack.extra_gateway().await;
    let b = stack.extra_gateway().await;
    let on_a = stack
        .client_on(&a.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    let on_b = stack
        .client_on(&b.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    ready(&on_a).await;
    ready(&on_b).await;

    let upstream = mitigation(
        Target::Upstream {
            upstream: "tools".into(),
        },
        Scope::All,
    );
    let (id, _) = store.apply(&Standalone, upstream, now_ms()).unwrap();
    for client in [&on_a, &on_b] {
        let error = client.call_tool(other_call()).await.unwrap_err();
        assert!(
            format!("{error}").contains("upstream is disabled"),
            "{error}"
        );
    }

    let lifted = store
        .lift(&Standalone, &id, "DEC-2", "ops-1", now_ms())
        .unwrap();
    assert_eq!(lifted, Outcome::Lifted);
    on_a.call_tool(echo_call("back")).await.unwrap();
    on_b.call_tool(echo_call("back")).await.unwrap();
    let actions: Vec<Action> = mitigation_audit(&stack).iter().map(|e| e.action).collect();
    assert_eq!(actions, [Action::Apply, Action::Lift]);
    let _ = on_a.cancel().await;
    let _ = on_b.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_destination_is_not_forwarded_to() {
    let (stack, store) = started().await;
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    ready(&client).await;
    let forwarded = stack.seen.lock().unwrap().len();

    // Written differently from the configured upstream URL on purpose.
    let deny = mitigation(
        Target::Destination {
            destination: format!("HTTP://{}/", stack.upstream_address),
        },
        Scope::All,
    );
    store.apply(&Standalone, deny, now_ms()).unwrap();
    let error = client.call_tool(echo_call("x")).await.unwrap_err();
    assert!(
        format!("{error}").contains("destination is denied"),
        "{error}"
    );
    assert_eq!(stack.seen.lock().unwrap().len(), forwarded);
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_mitigation_stops_on_the_next_call_and_is_audited_once() {
    let (stack, store) = started().await;
    let a = stack.extra_gateway().await;
    let on_a = stack
        .client_on(&a.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    let on_stack = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    ready(&on_a).await;
    ready(&on_stack).await;

    // Applied ten seconds ago with a one-second expiry: expired now.
    let applied = now_ms() - 10_000;
    let mut expiring = echo_disabled(Scope::All);
    expiring.expires_at_unix_ms = Some(applied + 1_000);
    store.apply(&Standalone, expiring, applied).unwrap();

    on_a.call_tool(echo_call("x")).await.unwrap();
    on_stack.call_tool(echo_call("x")).await.unwrap();
    let expired = mitigation_audit(&stack)
        .into_iter()
        .filter(|event| event.action == Action::Expire)
        .collect::<Vec<_>>();
    assert_eq!(expired.len(), 1, "{expired:?}");
    assert_eq!(expired[0].decision, "DEC-1");
    let _ = on_a.cancel().await;
    let _ = on_stack.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_store_refuses_forwarded_calls() {
    let (stack, _) = started().await;
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    ready(&client).await;
    let forwarded = stack.seen.lock().unwrap().len();

    std::fs::write(stack.config.mitigations.file.as_ref().unwrap(), "not-json").unwrap();
    let error = client.call_tool(echo_call("x")).await.unwrap_err();
    assert!(
        format!("{error}").contains("mitigation state is unavailable"),
        "{error}"
    );
    assert_eq!(stack.seen.lock().unwrap().len(), forwarded);
    let _ = client.cancel().await;
}

/// Replicas whose configs sit in different directories and name only the
/// shared `[cluster] state_dir` find the same mitigation store there.
#[tokio::test(flavor = "multi_thread")]
async fn replicas_configured_apart_share_the_store_in_state_dir() {
    let stack = Stack::start_with(Some(AGENTS), "\n[cluster]\nstate_dir = \"cluster\"\n").await;
    stack.enable_echo().await;
    // The same settings, without `[mitigations]`, loaded from another directory.
    let mut elsewhere = stack.config.clone();
    elsewhere.mitigations = Default::default();
    let other_dir = tempfile::Builder::new()
        .prefix(&format!("sekisho-{}-", std::process::id()))
        .tempdir()
        .unwrap();
    let path = other_dir.path().join("sekisho.toml");
    std::fs::write(&path, toml::to_string(&elsewhere).unwrap()).unwrap();
    let loaded = sekisho::config::Config::load(&path).unwrap();
    let state_dir = stack.config.cluster.state_dir.clone().unwrap();
    assert_eq!(
        loaded.mitigations.file,
        Some(state_dir.join("mitigations.json"))
    );
    assert_eq!(loaded.mitigations, stack.config.mitigations);

    let b = stack.replica(&loaded).await;
    let on_b = stack
        .client_on(&b.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    ready(&on_b).await;
    MitigationStore::new(sekisho::state::open(&stack.config).unwrap())
        .apply(&Standalone, echo_disabled(Scope::All), now_ms())
        .unwrap();
    let error = on_b.call_tool(echo_call("x")).await.unwrap_err();
    assert!(format!("{error}").contains("tool is disabled"), "{error}");
    let _ = on_b.cancel().await;
}
