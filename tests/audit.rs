//! Every tool call yields one audit event whose trace id the engine sees.

mod support;

use std::time::Duration;

use axum::http::{HeaderName, HeaderValue};
use sekisho::audit::{CallEvent, Decision};
use support::stack::{Stack, echo_call};

const AGENTS: &str = "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\"]\n";
const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn events(stack: &Stack) -> Vec<CallEvent> {
    std::fs::read_to_string(&stack.config.audit.calls_file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_and_its_engine_receipt_share_a_trace_id() {
    let stack = Stack::start(Some(AGENTS)).await;
    stack.enable_echo().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let headers = [(
        HeaderName::from_static("traceparent"),
        HeaderValue::from_static(TRACEPARENT),
    )]
    .into_iter()
    .collect();
    let client = stack
        .client_with(serde_json::json!({ "act": "agent-7" }), headers)
        .await;
    client.call_tool(echo_call("hi")).await.unwrap();

    let events = events(&stack);
    let event = events.last().unwrap();
    assert_eq!(event.decision, Decision::Allowed);
    assert_eq!(event.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(
        (event.subject.as_str(), event.agent.as_str()),
        ("user-1", "agent-7")
    );
    assert_eq!(event.tool, "tools.echo");
    assert!(event.response_bytes.is_some_and(|bytes| bytes > 0));
    let engine = stack.traceparents.lock().unwrap()[0].clone().unwrap();
    assert_eq!(sekisho::audit::trace_id(Some(&engine)), event.trace_id);
    assert_ne!(engine, TRACEPARENT, "the upstream call is its own span");
    let raw = std::fs::read_to_string(&stack.config.audit.calls_file).unwrap();
    assert!(!raw.contains("echo hi"), "no payloads in audit: {raw}");
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn denied_calls_are_audited_with_a_generated_trace_id() {
    let stack = Stack::start(Some("")).await;
    stack.enable_echo().await;
    let client = stack.client(serde_json::json!({ "act": "agent-9" })).await;
    assert!(client.call_tool(echo_call("hi")).await.is_err());
    let event = events(&stack).pop().unwrap();
    assert_eq!(event.decision, Decision::Denied);
    assert_eq!(event.agent, "agent-9");
    assert_eq!(event.reason.as_deref(), Some("agent has no policy"));
    assert_eq!(event.trace_id.len(), 32);
    let _ = client.cancel().await;
}

const READERS: &str = "\n[audit]\nreaders = [\"console\"]\n";

fn traced(traceparent: &'static str) -> std::collections::HashMap<HeaderName, HeaderValue> {
    [(
        HeaderName::from_static("traceparent"),
        HeaderValue::from_static(traceparent),
    )]
    .into_iter()
    .collect()
}

/// `GET /audit/calls` on `address` as subject `sub` of tenant `tenant`.
async fn read_trace(
    address: &str,
    sub: &str,
    tenant: Option<&str>,
    trace_id: &str,
) -> (u16, serde_json::Value) {
    let mut extra = serde_json::json!({ "sub": sub });
    if let Some(tenant) = tenant {
        extra["tenant_id"] = tenant.into();
    }
    let bearer = support::token("k1", &support::key(1), support::claims(extra));
    let response = reqwest::Client::new()
        .get(format!("http://{address}/audit/calls?trace_id={trace_id}"))
        .bearer_auth(bearer)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or_default())
}

fn decisions(page: &serde_json::Value) -> Vec<String> {
    page["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            format!(
                "{}:{}",
                event["agent"].as_str().unwrap(),
                event["decision"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reader_reads_its_tenants_trace_in_order_and_no_one_elses() {
    let stack = Stack::start_with(Some(AGENTS), READERS).await;
    stack.enable_echo().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let allowed = stack
        .client_with(
            serde_json::json!({ "act": "agent-7", "tenant_id": "t1" }),
            traced(TRACEPARENT),
        )
        .await;
    allowed.call_tool(echo_call("hi")).await.unwrap();
    let denied = stack
        .client_with(
            serde_json::json!({ "act": "agent-9", "tenant_id": "t1" }),
            traced(TRACEPARENT),
        )
        .await;
    assert!(denied.call_tool(echo_call("hi")).await.is_err());

    let (status, page) = read_trace(&stack.address, "console", Some("t1"), trace).await;
    assert_eq!(status, 200);
    // A forwarded call is `started`, then its outcome; a refusal is one event.
    assert_eq!(
        decisions(&page),
        ["agent-7:started", "agent-7:allowed", "agent-9:denied"]
    );
    assert_eq!(page["complete"], true);
    assert!(!page.to_string().contains("echo hi"), "no payloads: {page}");

    let (status, other) = read_trace(&stack.address, "console", Some("t2"), trace).await;
    assert_eq!((status, decisions(&other).len()), (200, 0));
    let (status, unknown) =
        read_trace(&stack.address, "console", Some("t1"), &"a".repeat(32)).await;
    assert_eq!((status, decisions(&unknown).len()), (200, 0));
    assert_eq!(
        read_trace(&stack.address, "user-1", Some("t1"), trace)
            .await
            .0,
        403
    );
    assert_eq!(
        read_trace(&stack.address, "console", None, trace).await.0,
        403
    );
    let _ = allowed.cancel().await;
    let _ = denied.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_are_off_without_configured_readers() {
    let stack = Stack::start(Some(AGENTS)).await;
    let (status, _) = read_trace(&stack.address, "console", Some("t1"), &"a".repeat(32)).await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn either_replica_reads_events_both_replicas_appended() {
    let stack = Stack::start_with(Some(AGENTS), READERS).await;
    stack.enable_echo().await;
    let (a, b) = (stack.extra_gateway().await, stack.extra_gateway().await);
    let trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    for address in [&a.address, &b.address] {
        let client = stack
            .client_on_with(
                address,
                serde_json::json!({ "act": "agent-7", "tenant_id": "t1" }),
                traced(TRACEPARENT),
            )
            .await;
        client.call_tool(echo_call("hi")).await.unwrap();
        let _ = client.cancel().await;
    }
    for address in [&a.address, &b.address] {
        let (status, page) = read_trace(address, "console", Some("t1"), trace).await;
        // Each call's `started` and `allowed` events.
        assert_eq!((status, decisions(&page).len()), (200, 4));
    }
    a.kill();
    b.kill();
}
