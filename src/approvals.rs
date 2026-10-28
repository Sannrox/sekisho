//! Approval gate for destructive and open-world tools (#21).
//!
//! An operator marks the tools that can destroy or reach beyond the governed
//! data. A marked tool is forwarded only along an approval route; one without
//! a route is refused before any delegated token is minted, so a missing or
//! mistyped route fails closed. Unmarked tools keep the allowlist and
//! exchange path unchanged.
//!
//! The one route is `engine`: the tool's upstream is a governing engine that
//! parks the call as an Action and decides it, so forwarding the call is the
//! approval request and Sekisho does not prompt a second time. Engines stay
//! the source of truth for authorization; Sekisho keeps no approval state.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalsConfig {
    /// Destructive or open-world tools, by registry key (`upstream.tool`).
    #[serde(default)]
    pub marked: BTreeSet<String>,
    /// Marked tools whose upstream parks and decides the call as a governed
    /// Action, so the engine's parking is their approval.
    #[serde(default)]
    pub engine: BTreeSet<String>,
}

/// What the gate does with one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Not marked: the call takes the usual path.
    Unmarked,
    /// Marked and routed to an engine that parks and decides it.
    Engine,
    /// Marked without an approval route: refused before forwarding.
    Refused,
}

impl Gate {
    /// The audit label for a marked call (`approval` on the call event).
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Unmarked => None,
            Self::Engine => Some("engine"),
            Self::Refused => Some("required"),
        }
    }
}

impl ApprovalsConfig {
    pub fn gate(&self, tool: &str) -> Gate {
        if !self.marked.contains(tool) {
            Gate::Unmarked
        } else if self.engine.contains(tool) {
            Gate::Engine
        } else {
            Gate::Refused
        }
    }

    /// An engine route for an unmarked tool is a mistake, most likely a
    /// mistyped key that would leave the real tool unmarked.
    pub fn validate(&self) -> Result<(), String> {
        match self.engine.difference(&self.marked).next() {
            Some(tool) => Err(format!(
                "approvals.engine lists {tool:?}, which approvals.marked does not mark"
            )),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(marked: &[&str], engine: &[&str]) -> ApprovalsConfig {
        ApprovalsConfig {
            marked: marked.iter().map(|tool| tool.to_string()).collect(),
            engine: engine.iter().map(|tool| tool.to_string()).collect(),
        }
    }

    #[test]
    fn marked_tools_need_a_route_and_unmarked_ones_pass() {
        let approvals = config(&["records.delete", "web.fetch"], &["records.delete"]);
        assert_eq!(approvals.gate("records.search"), Gate::Unmarked);
        assert_eq!(approvals.gate("records.delete"), Gate::Engine);
        assert_eq!(approvals.gate("web.fetch"), Gate::Refused);
        assert_eq!(Gate::Refused.label(), Some("required"));
        assert_eq!(Gate::Unmarked.label(), None);
    }

    #[test]
    fn an_engine_route_must_name_a_marked_tool() {
        assert!(
            config(&["records.delete"], &["records.delete"])
                .validate()
                .is_ok()
        );
        let error = config(&["records.delete"], &["records.delet"])
            .validate()
            .unwrap_err();
        assert!(error.contains("records.delet"), "{error}");
    }
}
