//! Shared rate-limit file, durable audit, and kill-replica retries.

mod support;

use std::time::Duration;

use sekisho::audit::{CallEvent, Decision};
use support::stack::{Stack, echo_call, eventually};

const AGENTS: &str = "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\"]\n";
const CLUSTER: &str = "\n[cluster]\nstate_dir = \"cluster\"\n[limits]\nagent = { rate_per_sec = 0.001, burst = 2.0 }\n";

#[tokio::test(flavor = "multi_thread")]
async fn kill_replica_retries_on_the_survivor_and_keeps_audit() {
    let stack = Stack::start_with(Some(AGENTS), CLUSTER).await;
    stack.enable_echo().await;
    let a = stack.extra_gateway().await;
    let b = stack.extra_gateway().await;

    let client_a = stack
        .client_on(&a.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    client_a.call_tool(echo_call("1")).await.unwrap();
    let _ = client_a.cancel().await;
    a.kill();

    let client_b = stack
        .client_on(&b.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    client_b.call_tool(echo_call("2")).await.unwrap();
    let _ = client_b.cancel().await;

    let events: Vec<CallEvent> = std::fs::read_to_string(&stack.config.audit.calls_file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .filter(|event| event.decision == Decision::Allowed)
            .count()
            >= 2,
        "{events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_burst_holds_across_replicas() {
    let stack = Stack::start_with(Some(AGENTS), CLUSTER).await;
    stack.enable_echo().await;
    let a = stack.extra_gateway().await;
    let b = stack.extra_gateway().await;

    let client_a = stack
        .client_on(&a.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    let client_b = stack
        .client_on(&b.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    client_a.call_tool(echo_call("1")).await.unwrap();
    client_a.call_tool(echo_call("2")).await.unwrap();
    let error = client_b.call_tool(echo_call("3")).await.unwrap_err();
    let text = format!("{error:?}");
    assert!(text.contains("rate limited"), "{text}");
    let _ = client_a.cancel().await;
    let _ = client_b.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn in_flight_on_a_killed_replica_fails_and_retry_succeeds() {
    let stack = Stack::start_with(Some(AGENTS), CLUSTER).await;
    stack.enable_echo().await;
    let a = stack.extra_gateway().await;
    let b = stack.extra_gateway().await;
    let addr_a = a.address.clone();

    let client_a = stack
        .client_on(&addr_a, serde_json::json!({ "act": "agent-7" }))
        .await;
    let inflight = tokio::spawn(async move { client_a.call_tool(echo_call("hold")).await });
    let entered = stack.hold_entered.clone();
    assert!(
        eventually(Duration::from_secs(2), async || {
            entered.load(std::sync::atomic::Ordering::SeqCst)
        })
        .await
    );
    a.kill();
    let failed = inflight.await.expect("join");
    assert!(failed.is_err(), "{failed:?}");
    let client_b = stack
        .client_on(&b.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    client_b.call_tool(echo_call("retry")).await.unwrap();
    let _ = client_b.cancel().await;
}
