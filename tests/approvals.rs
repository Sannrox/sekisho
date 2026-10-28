//! Approval gate for destructive and open-world tools (#21): a marked tool
//! without a route is refused and audited, one routed to a governing engine
//! is forwarded once, and unmarked tools are unchanged.

mod support;

use std::time::Duration;

use support::stack::{Stack, echo_call, eventually};

const AGENTS: &str =
    "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\"]\nceiling = [\"read\"]\n";

async fn started(
    approvals: &str,
) -> (
    Stack,
    rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
) {
    let stack = Stack::start_with(Some(AGENTS), approvals).await;
    stack.enable_echo().await;
    let client = stack
        .client(serde_json::json!({ "act": { "sub": "agent-7" } }))
        .await;
    (stack, client)
}

fn audit_records(stack: &Stack) -> Vec<serde_json::Value> {
    std::fs::read_to_string(&stack.config.audit.calls_file)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_marked_tool_without_a_route_is_refused_audited_and_not_forwarded() {
    let (stack, client) = started("\n[approvals]\nmarked = [\"tools.echo\"]\n").await;
    // Once the registry serves the tool, the gate is what refuses it.
    assert!(
        eventually(Duration::from_secs(5), async || client
            .call_tool(echo_call("drop everything"))
            .await
            .is_err_and(
                |error| format!("{error}").contains("approval required")
            ))
        .await
    );
    assert!(stack.seen.lock().unwrap().is_empty());
    let refused = audit_records(&stack)
        .into_iter()
        .rfind(|record| record["approval"] == "required")
        .expect("audited refusal");
    assert_eq!(refused["decision"], "denied");
    assert_eq!(refused["tool"], "tools.echo");
    assert!(!refused["trace_id"].as_str().unwrap().is_empty());
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_marked_tool_routed_to_its_engine_is_forwarded_once() {
    let (stack, client) =
        started("\n[approvals]\nmarked = [\"tools.echo\"]\nengine = [\"tools.echo\"]\n").await;
    assert!(
        eventually(Duration::from_secs(5), async || client
            .call_tool(echo_call("park me"))
            .await
            .is_ok())
        .await
    );
    assert_eq!(stack.seen.lock().unwrap().len(), 1);
    let forwarded = audit_records(&stack)
        .into_iter()
        .rfind(|record| record["decision"] == "allowed")
        .expect("audited forward");
    assert_eq!(forwarded["approval"], "engine");
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unmarked_tools_keep_the_allowlist_and_exchange_path() {
    let (stack, client) = started("\n[approvals]\nmarked = [\"tools.other\"]\n").await;
    assert!(
        eventually(Duration::from_secs(5), async || client
            .call_tool(echo_call("hello"))
            .await
            .is_ok())
        .await
    );
    let forwarded = audit_records(&stack)
        .into_iter()
        .rfind(|record| record["decision"] == "allowed")
        .expect("audited forward");
    assert!(forwarded.get("approval").is_none(), "{forwarded}");
    let _ = client.cancel().await;
}

/// A routed call that fails after the gate keeps its approval label: the
/// audit line still says the engine route was taken (#21).
#[tokio::test(flavor = "multi_thread")]
async fn a_routed_call_that_fails_after_the_gate_keeps_its_label() {
    let (stack, client) =
        started("\n[approvals]\nmarked = [\"tools.echo\"]\nengine = [\"tools.echo\"]\n").await;
    stack
        .issuer_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        eventually(Duration::from_secs(5), async || {
            client.call_tool(echo_call("park me")).await.is_err()
                && audit_records(&stack)
                    .iter()
                    .any(|record| record["decision"] == "failed")
        })
        .await
    );
    let failed = audit_records(&stack)
        .into_iter()
        .rfind(|record| record["decision"] == "failed")
        .expect("audited failure");
    assert_eq!(failed["approval"], "engine");
    let _ = client.cancel().await;
}
