//! Per-agent tool allowlists and grant ceilings.
//!
//! The policy file is read on every call, so an edit applies to the next
//! call, and a file that cannot be read or parsed refuses every call (fail
//! closed). An agent is the first actor in the token's `act` chain, or the
//! subject when the token names no actor. An agent with no entry may call
//! nothing.
//!
//! The ceiling names the grants an agent may ever use. Engines enforce the
//! user's grants intersected with it through the delegated token that
//! per-call token exchange requests; Sekisho records it here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::verifier::Principal;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPolicy {
    pub id: String,
    /// Registry tool keys (`<upstream>.<tool>`) this agent may call.
    pub tools: Vec<String>,
    /// Grants the agent may use, at most.
    #[serde(default)]
    pub ceiling: Vec<String>,
    /// Guardrail layer for this agent, over the tenant and default (#10).
    #[serde(default)]
    pub guardrails: Option<crate::guardrails::GuardrailPolicy>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    #[serde(default)]
    pub agents: Vec<AgentPolicy>,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("agent policy {path} cannot be loaded: {reason}")]
    Unavailable { path: PathBuf, reason: String },
}

impl PolicyFile {
    /// Load and check for duplicate agents. A missing file is an error: an
    /// absent policy must not mean "no restrictions".
    pub fn load(path: &Path) -> Result<Self, PolicyError> {
        let unavailable = |reason: String| PolicyError::Unavailable {
            path: path.into(),
            reason,
        };
        let raw = std::fs::read_to_string(path).map_err(|error| unavailable(error.to_string()))?;
        let file: Self =
            toml::from_str(&raw).map_err(|error| unavailable(error.message().into()))?;
        let mut ids = std::collections::BTreeSet::new();
        if let Some(duplicate) = file.agents.iter().find(|agent| !ids.insert(&agent.id)) {
            return Err(unavailable(format!("agent {} appears twice", duplicate.id)));
        }
        Ok(file)
    }

    pub fn agent(&self, id: &str) -> Option<&AgentPolicy> {
        self.agents.iter().find(|agent| agent.id == id)
    }
}

/// The agent a principal acts as: the outermost `act` actor, else the
/// subject. `None` when `act` is present but names no actor, so a malformed
/// claim never inherits the subject's policy.
pub fn agent_id(principal: &Principal) -> Option<String> {
    let actor = match &principal.actor {
        None => return Some(principal.subject.clone()),
        Some(serde_json::Value::String(id)) => Some(id.as_str()),
        Some(serde_json::Value::Object(fields)) => {
            fields.get("sub").and_then(serde_json::Value::as_str)
        }
        Some(_) => None,
    };
    actor.filter(|id| !id.trim().is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(actor: Option<serde_json::Value>) -> Principal {
        Principal {
            subject: "user-1".into(),
            actor,
            tenant: None,
        }
    }

    #[test]
    fn agent_is_the_outermost_actor_or_the_subject() {
        assert_eq!(agent_id(&principal(None)).as_deref(), Some("user-1"));
        assert_eq!(
            agent_id(&principal(Some("agent-7".into()))).as_deref(),
            Some("agent-7")
        );
        let nested = serde_json::json!({"sub": "agent-7", "act": {"sub": "gateway"}});
        assert_eq!(
            agent_id(&principal(Some(nested))).as_deref(),
            Some("agent-7")
        );
        for malformed in [
            serde_json::json!({}),
            serde_json::json!({"sub": 1}),
            "".into(),
            3.into(),
        ] {
            assert_eq!(agent_id(&principal(Some(malformed))), None);
        }
    }
}
