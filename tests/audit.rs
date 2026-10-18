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
