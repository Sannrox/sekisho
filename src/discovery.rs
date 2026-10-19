//! Progressive discovery: meta-tools to search tools, read one schema, and
//! invoke a tool by name, so an agent need not load every definition.
//!
//! Results only include tools the caller may use: enabled in the registry
//! and on its agent allowlist. `invoke_tool` is unwrapped into an ordinary
//! call of the named tool, with the same policy, exchange and audit.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool};

use crate::registry::ToolVersion;

pub const SEARCH: &str = "sekisho.search_tools";
pub const SCHEMA: &str = "sekisho.get_tool_schema";
pub const INVOKE: &str = "sekisho.invoke_tool";
/// Most search results returned.
const SEARCH_LIMIT: usize = 20;

fn object(value: serde_json::Value) -> Arc<JsonObject> {
    match value {
        serde_json::Value::Object(object) => Arc::new(object),
        _ => Arc::default(),
    }
}

/// The three meta-tools, always listed.
pub fn meta_tools() -> Vec<Tool> {
    vec![
        Tool::new(
            SEARCH,
            "Find tools you may call. Every word of `query` must appear in a tool's name or description.",
            object(serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
            })),
        ),
        Tool::new(
            SCHEMA,
            "Get one tool's description and input schema by name.",
            object(serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"],
            })),
        ),
        Tool::new(
            INVOKE,
            "Call a tool by name with its arguments.",
            object(serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "arguments": { "type": "object" },
                },
                "required": ["name"],
            })),
        ),
    ]
}

/// Visible tools matching every query word, case-insensitively.
pub fn search(visible: &[(String, ToolVersion)], query: &str) -> serde_json::Value {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let matches: Vec<serde_json::Value> = visible
        .iter()
        .filter(|(name, version)| {
            let haystack = format!(
                "{} {}",
                name.to_lowercase(),
                version.description.as_deref().unwrap_or("").to_lowercase()
            );
            words.iter().all(|word| haystack.contains(word.as_str()))
        })
        .take(SEARCH_LIMIT)
        .map(|(name, version)| {
            serde_json::json!({ "name": name, "description": version.description })
        })
        .collect();
    serde_json::json!({ "tools": matches })
}

/// One visible tool's description and schema, or `None` when not visible.
pub fn schema(visible: &[(String, ToolVersion)], name: &str) -> Option<serde_json::Value> {
    visible
        .iter()
        .find(|(key, _)| key == name)
        .map(|(key, version)| {
            serde_json::json!({
                "name": key,
                "description": version.description,
                "input_schema": version.input_schema,
            })
        })
}

/// The target of an `invoke_tool` call: `(name, arguments)`.
pub fn invoke_target(arguments: Option<&JsonObject>) -> Option<(String, Option<JsonObject>)> {
    let arguments = arguments?;
    let name = arguments.get("name")?.as_str()?.to_string();
    let inner = match arguments.get("arguments") {
        None => None,
        Some(serde_json::Value::Object(inner)) => Some(inner.clone()),
        Some(_) => return None,
    };
    Some((name, inner))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible() -> Vec<(String, ToolVersion)> {
        let tool = |name: &str, description: &str| {
            (
                name.to_string(),
                ToolVersion {
                    description: Some(description.into()),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            )
        };
        vec![
            tool("ontology.search", "Search ontology objects by text"),
            tool("delivery.rollout", "Start a rollout"),
        ]
    }

    #[test]
    fn search_matches_every_word_in_name_or_description() {
        let found = search(&visible(), "OBJECTS search");
        assert_eq!(found["tools"][0]["name"], "ontology.search");
        assert_eq!(found["tools"].as_array().unwrap().len(), 1);
        assert!(search(&visible(), "nothing").to_string().contains("[]"));
        assert!(schema(&visible(), "delivery.rollout").is_some());
        assert!(schema(&visible(), "hidden.tool").is_none());
    }
}
