//! Per-call token exchange: engines get a short-lived token scoped to the
//! user, agent, audience and tool, never Sekisho's or the caller's own.

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use support::stack::{ENGINE_AUDIENCE, Stack, echo_call};

const AGENTS: &str = "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\", \"tools.other\"]\nceiling = [\"read\"]\n";

async fn stack() -> Stack {
    let stack = Stack::start(Some(AGENTS)).await;
    stack.enable("tools.echo").await;
    stack.enable("tools.other").await;
    // Let one refresh publish the enables to the data plane.
    tokio::time::sleep(Duration::from_millis(300)).await;
    stack
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_sees_the_user_and_an_actor_chain_naming_the_agent() {
    let stack = stack().await;
    let client = stack
        .client(serde_json::json!({ "act": { "sub": "agent-7" } }))
        .await;
    client.call_tool(echo_call("hi")).await.unwrap();
    let (_, claims) = stack.seen.lock().unwrap()[0].clone();
    assert_eq!(claims["sub"], "user-1");
    assert_eq!(claims["aud"], ENGINE_AUDIENCE);
    assert_eq!(
        claims["scope"], "tool:echo grant:read",
        "ceiling travels as scopes"
    );
    assert_eq!(claims["act"]["sub"], "sekisho");
    assert_eq!(claims["act"]["act"]["sub"], "agent-7");
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_for_one_tool_or_audience_is_refused_elsewhere() {
    let stack = stack().await;
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    client.call_tool(echo_call("hi")).await.unwrap();
    let (captured, _) = stack.seen.lock().unwrap()[0].clone();
    // The echo token does not open `other`.
    assert!(
        stack
            .call_upstream_directly(&captured, "other")
            .await
            .is_err()
    );
    // The caller's own Sekisho token is for another audience.
    let caller = support::token(
        "k1",
        &support::key(1),
        support::claims(serde_json::json!({})),
    );
    assert!(stack.call_upstream_directly(&caller, "echo").await.is_err());
    // Through Sekisho, `other` gets its own token and works.
    client
        .call_tool(CallToolRequestParams::new("tools.other"))
        .await
        .unwrap();
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_exchange_refuses_the_call() {
    let stack = stack().await;
    stack.issuer_down.store(true, Ordering::SeqCst);
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    let error = client.call_tool(echo_call("hi")).await.unwrap_err();
    assert!(
        format!("{error}").contains("token exchange failed"),
        "{error}"
    );
    assert!(
        stack.seen.lock().unwrap().is_empty(),
        "nothing reached the engine"
    );
    let audit = std::fs::read_to_string(&stack.config.policy.audit_file).unwrap();
    assert!(audit.contains("token exchange failed"), "{audit}");
    let _ = client.cancel().await;
}
