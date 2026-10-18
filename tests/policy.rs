//! Per-agent allowlists: refusal before forwarding, audit, live edits, and
//! failing closed.

mod support;

use std::time::Duration;

use support::stack::{Stack, echo_call, eventually};

const AGENT: &str = "agent-7";

#[tokio::test(flavor = "multi_thread")]
async fn agent_outside_its_allowlist_is_refused_and_audited_until_the_list_changes() {
    let stack = Stack::start(Some(
        "[[agents]]\nid = \"agent-7\"\ntools = []\nceiling = [\"read\"]\n",
    ))
    .await;
    stack.enable_echo().await;
    let client = stack
        .client(serde_json::json!({ "act": { "sub": AGENT } }))
        .await;
    // The registry serves the tool, but this agent may not use it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(client.list_all_tools().await.unwrap().is_empty());
    let error = client.call_tool(echo_call("hi")).await.unwrap_err();
    assert!(
        format!("{error}").contains("not in the agent allowlist"),
        "{error}"
    );

    let audit = std::fs::read_to_string(&stack.config.audit.calls_file).unwrap();
    let record: serde_json::Value = serde_json::from_str(audit.lines().next().unwrap()).unwrap();
    assert_eq!(record["agent"], AGENT);
    assert_eq!(record["subject"], "user-1");
    assert_eq!(record["tool"], "tools.echo");

    // The next call sees the edited allowlist; no restart.
    std::fs::write(
        &stack.config.policy.file,
        "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\"]\n",
    )
    .unwrap();
    assert!(
        eventually(Duration::from_secs(5), async || client
            .call_tool(echo_call("hi"))
            .await
            .is_ok())
        .await
    );
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_or_broken_policy_refuses_every_call() {
    let stack = Stack::start(None).await;
    stack.enable_echo().await;
    let client = stack.client(serde_json::json!({ "act": AGENT })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(client.call_tool(echo_call("hi")).await.is_err());
    assert!(client.list_all_tools().await.is_err());
    std::fs::write(&stack.config.policy.file, "not = [toml").unwrap();
    assert!(client.call_tool(echo_call("hi")).await.is_err());
    let audit = std::fs::read_to_string(&stack.config.audit.calls_file).unwrap();
    assert!(audit.contains("agent policy is unavailable"), "{audit}");
    let _ = client.cancel().await;
}
