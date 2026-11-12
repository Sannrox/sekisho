//! Outbound egress (#76): unlisted origins are refused and audited, the
//! injected credential never reaches the agent or the audit log, and a
//! destination deny drops an in-flight dial.

mod support;

use std::time::Duration;

use sekisho::audit::{CallEvent, Decision};
use sekisho::mitigations::{Mitigation, MitigationStore, Scope, Standalone, Target, now_ms};
use support::stack::{EGRESS_SECRET, Stack, echo_call, eventually};

const AGENTS: &str = "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\"]\n";

fn events(stack: &Stack) -> Vec<CallEvent> {
    std::fs::read_to_string(&stack.config.audit.calls_file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn leak(haystack: &str) -> bool {
    haystack.contains(EGRESS_SECRET)
}

async fn wait_past_enable(
    client: &rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
) {
    assert!(
        eventually(Duration::from_secs(5), async || {
            match client.call_tool(echo_call("warm")).await {
                Ok(_) => true,
                Err(error) => !format!("{error}").contains("not enabled"),
            }
        })
        .await
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unlisted_destination_is_refused_and_audited() {
    let dark = Stack::start_egress(Some(AGENTS), false).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        dark.list_count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "unlisted origins must not be dialed to list tools"
    );

    let stack = Stack::start_egress(Some(AGENTS), true).await;
    stack.enable_echo().await;
    let listed = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    wait_past_enable(&listed).await;
    assert!(stack.list_count.load(std::sync::atomic::Ordering::SeqCst) > 0);

    let mut config = stack.config.clone();
    config.egress.allow = vec!["http://127.0.0.1:1".into()];
    let replica = stack.replica(&config).await;
    let client = stack
        .client_on(&replica.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    wait_past_enable(&client).await;
    let error = client.call_tool(echo_call("hi")).await.unwrap_err();
    assert!(
        format!("{error}").contains("destination is not allowed"),
        "{error}"
    );
    let event = events(&stack).pop().unwrap();
    assert_eq!(event.decision, Decision::Denied);
    assert_eq!(event.reason.as_deref(), Some("destination is not allowed"));
    listed
        .call_tool(echo_call("still"))
        .await
        .expect("a denied replica must not mark the upstream unavailable");
    let raw = std::fs::read_to_string(&stack.config.audit.calls_file).unwrap();
    assert!(!leak(&raw), "{raw}");
    let _ = client.cancel().await;
    let _ = listed.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listed_destination_injects_the_credential_without_leaking_it() {
    let stack = Stack::start_egress(Some(AGENTS), true).await;
    stack.enable_echo().await;
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    wait_past_enable(&client).await;
    let result = client.call_tool(echo_call("hi")).await.unwrap();
    let body = format!("{result:?}");
    assert!(!leak(&body), "{body}");
    let (token, claims) = stack.seen.lock().unwrap()[0].clone();
    assert_eq!(token, EGRESS_SECRET);
    assert_eq!(claims["injected"], true);
    let raw = std::fs::read_to_string(&stack.config.audit.calls_file).unwrap();
    assert!(!leak(&raw), "{raw}");
    let event = events(&stack).pop().unwrap();
    assert_eq!(event.decision, Decision::Allowed);
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_exchange_ignores_the_egress_allowlist() {
    let stack = Stack::start_with(
        Some(AGENTS),
        "\n[egress]\nallow = [\"http://127.0.0.1:1\"]\n",
    )
    .await;
    stack.enable_echo().await;
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    wait_past_enable(&client).await;
    client.call_tool(echo_call("hi")).await.unwrap();
    assert!(!stack.seen.lock().unwrap().is_empty());
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_destination_deny_drops_an_in_flight_egress_call() {
    let stack = Stack::start_egress(Some(AGENTS), true).await;
    stack.enable_echo().await;
    let store = MitigationStore::new(sekisho::state::open(&stack.config).unwrap());
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    assert!(
        eventually(Duration::from_secs(5), async || client
            .call_tool(echo_call("warm"))
            .await
            .is_ok())
        .await
    );

    let inflight = tokio::spawn({
        let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
        async move { client.call_tool(echo_call("hold")).await }
    });
    let entered = stack.hold_entered.clone();
    assert!(
        eventually(Duration::from_secs(2), async || {
            entered.load(std::sync::atomic::Ordering::SeqCst)
        })
        .await
    );
    let deny = Mitigation {
        decision: "DEC-egress".into(),
        operator: "ops-1".into(),
        target: Target::Destination {
            destination: format!("HTTP://{}/", stack.upstream_address),
        },
        scope: Scope::All,
        applied_at_unix_ms: 0,
        expires_at_unix_ms: None,
    };
    store.apply(&Standalone, deny, now_ms()).unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(2), inflight)
        .await
        .expect("in-flight call should not wait out the hold")
        .expect("join");
    let error = failed.unwrap_err();
    assert!(
        format!("{error}").contains("destination is denied"),
        "{error}"
    );
    let raw = std::fs::read_to_string(&stack.config.audit.calls_file).unwrap();
    assert!(!leak(&raw), "{raw}");
    let _ = client.cancel().await;
}
