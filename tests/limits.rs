//! Rate limits with retry hints, audited throttling, and response caps.

mod support;

use std::time::Duration;

use sekisho::audit::{CallEvent, Decision};
use support::stack::{Stack, echo_call};

const AGENTS: &str = "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\"]\n";
const LIMITS: &str =
    "\n[limits]\nagent = { rate_per_sec = 2.0, burst = 2.0 }\nmax_response_bytes = 1000\n";

#[tokio::test(flavor = "multi_thread")]
async fn agent_is_throttled_at_its_rate_and_may_retry_after_the_hint() {
    let stack = Stack::start_with(Some(AGENTS), LIMITS).await;
    stack.enable_echo().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    client.call_tool(echo_call("1")).await.unwrap();
    client.call_tool(echo_call("2")).await.unwrap();
    let error = client.call_tool(echo_call("3")).await.unwrap_err();
    let text = format!("{error:?}");
    assert!(text.contains("rate limited"), "{text}");
    let retry_ms = text
        .split("retry_after_ms")
        .nth(1)
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())
        })
        .and_then(|digits| digits.parse::<u64>().ok())
        .expect("retry_after_ms hint");
    assert!((1..=500).contains(&retry_ms), "{retry_ms}");
    tokio::time::sleep(Duration::from_millis(retry_ms + 20)).await;
    client.call_tool(echo_call("4")).await.unwrap();

    let events: Vec<CallEvent> = std::fs::read_to_string(&stack.config.audit.calls_file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.decision == Decision::Throttled)
            .count(),
        1
    );
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_response_is_truncated_and_marked() {
    let stack = Stack::start_with(Some(AGENTS), LIMITS).await;
    stack.enable_echo().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    let result = client
        .call_tool(echo_call(&"x".repeat(5_000)))
        .await
        .unwrap();
    let marker = result
        .content
        .last()
        .and_then(|block| block.as_text())
        .unwrap();
    assert!(
        marker.text.contains("response truncated from"),
        "{}",
        marker.text
    );
    assert!(serde_json::to_vec(&result).unwrap().len() <= 1_100);
    let _ = client.cancel().await;
}
