//! Registry federation end to end: a fake upstream MCP server, Sekisho in
//! front of it, and an official SDK client as the agent.

mod support;

use std::borrow::Cow;
use std::time::Duration;

use sekisho::registry::{self, Registry};
use support::stack::{Stack, echo_call, eventually};

/// The test token's subject, allowed to call `tools.echo`.
const AGENTS: &str = "[[agents]]\nid = \"user-1\"\ntools = [\"tools.echo\"]\n";

#[tokio::test(flavor = "multi_thread")]
async fn tools_start_disabled_then_enable_approve_and_roll_back_without_restart() {
    let stack = Stack::start(Some(AGENTS)).await;
    let store = sekisho::state::open(&stack.config).unwrap();
    let (file, audit) = (
        stack.config.registry.file.clone(),
        stack.config.registry.audit_file.clone(),
    );

    // Discovered, but disabled: not listed and not callable.
    assert!(
        eventually(Duration::from_secs(5), async || Registry::load(&file)
            .unwrap()
            .tools
            .contains_key("tools.echo"))
        .await
    );
    let client = stack.client(serde_json::json!({})).await;
    assert!(support::upstream_tools(client.list_all_tools().await.unwrap()).is_empty());
    assert!(client.call_tool(echo_call("hi")).await.is_err());

    // Enabled by the owner: served within the refresh interval, no restart.
    stack.enable_echo().await;
    let listed = async || support::upstream_tools(client.list_all_tools().await.unwrap());
    assert!(eventually(Duration::from_secs(5), async || !listed().await.is_empty()).await);
    let result = client.call_tool(echo_call("hi")).await.unwrap();
    assert!(format!("{result:?}").contains("echo hi"), "{result:?}");

    // An upstream change waits for approval; rollback restores the old text.
    *stack.description.lock().unwrap() = "Echo v2".into();
    let served = async || -> Option<Cow<'static, str>> {
        listed()
            .await
            .first()
            .and_then(|tool| tool.description.clone())
    };
    assert!(
        eventually(Duration::from_secs(5), async || Registry::load(&file)
            .unwrap()
            .tools["tools.echo"]
            .pending()
            .is_some())
        .await
    );
    assert_eq!(served().await.as_deref(), Some("Echo v1"));
    registry::owner_edit(store.as_ref(), "owner-1", "approve", "tools.echo", |r| {
        r.approve("tools.echo")
    })
    .unwrap();
    assert!(
        eventually(Duration::from_secs(5), async || served().await.as_deref()
            == Some("Echo v2"))
        .await
    );
    registry::owner_edit(store.as_ref(), "owner-1", "rollback", "tools.echo", |r| {
        r.rollback("tools.echo")
    })
    .unwrap();
    assert!(
        eventually(Duration::from_secs(5), async || served().await.as_deref()
            == Some("Echo v1"))
        .await
    );

    let log = std::fs::read_to_string(&audit).unwrap();
    assert_eq!(log.lines().count(), 3, "{log}");
    assert!(log.contains("\"owner\":\"owner-1\""), "{log}");
    let _ = client.cancel().await;
}
