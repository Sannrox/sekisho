//! Progressive discovery: an agent that knows only the meta-tools finds and
//! calls an allowed tool, and never sees disallowed ones.

mod support;

use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use support::stack::Stack;

const AGENTS: &str = "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\"]\n";

fn call(name: &str, arguments: serde_json::Value) -> CallToolRequestParams {
    let mut request = CallToolRequestParams::new(name.to_string());
    request.arguments = arguments.as_object().cloned();
    request
}

#[tokio::test(flavor = "multi_thread")]
async fn meta_tools_find_and_invoke_only_allowed_tools() {
    let stack = Stack::start(Some(AGENTS)).await;
    stack.enable("tools.echo").await;
    stack.enable("tools.other").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;

    let found = client
        .call_tool(call(
            "sekisho.search_tools",
            serde_json::json!({ "query": "tool" }),
        ))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let names: Vec<&str> = found["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"tools.echo"), "{found}");
    assert!(
        !names.contains(&"tools.other"),
        "disallowed tool leaked: {found}"
    );

    let schema = client
        .call_tool(call(
            "sekisho.get_tool_schema",
            serde_json::json!({ "name": "tools.echo" }),
        ))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(schema["input_schema"]["type"], "object");
    assert!(
        client
            .call_tool(call(
                "sekisho.get_tool_schema",
                serde_json::json!({ "name": "tools.other" })
            ))
            .await
            .is_err()
    );

    let result = client
        .call_tool(call(
            "sekisho.invoke_tool",
            serde_json::json!({ "name": "tools.echo", "arguments": { "text": "hi" } }),
        ))
        .await
        .unwrap();
    assert!(format!("{result:?}").contains("echo hi"), "{result:?}");
    assert!(
        client
            .call_tool(call(
                "sekisho.invoke_tool",
                serde_json::json!({ "name": "tools.other" })
            ))
            .await
            .is_err()
    );
    assert!(
        client
            .call_tool(call(
                "sekisho.invoke_tool",
                serde_json::json!({ "name": "sekisho.search_tools" })
            ))
            .await
            .is_err()
    );
    // Invocations are audited under the target tool; refused wrappers too.
    let audit = std::fs::read_to_string(&stack.config.audit.calls_file).unwrap();
    assert!(audit.contains("\"tool\":\"tools.echo\""), "{audit}");
    assert!(audit.contains("cannot call a meta-tool"), "{audit}");
    let _ = client.cancel().await;
}
