//! One audit event per tool call, linked to engine receipts by trace id.
//!
//! Events go to `[audit] calls_file`, one JSON line each: caller, agent,
//! tool, decision, latency, response size and the W3C trace id. They never
//! carry tokens, arguments or results. The trace id comes from the caller's
//! `traceparent` when it sends a valid one, otherwise it is generated; the
//! upstream call carries it in `traceparent` so the engine's receipt names
//! the same trace.

use std::io::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Forwarded and answered by the upstream.
    Allowed,
    /// Refused by Sekisho before forwarding.
    Denied,
    /// The token exchange or the upstream call failed operationally.
    Failed,
    /// Refused by a rate limit; the caller may retry later.
    Throttled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEvent {
    pub at_unix_ms: u128,
    pub trace_id: String,
    pub subject: String,
    pub agent: String,
    pub tool: String,
    pub decision: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub latency_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_bytes: Option<usize>,
}

pub struct CallAudit {
    file: PathBuf,
}

impl CallAudit {
    pub fn new(file: PathBuf) -> Self {
        Self { file }
    }

    /// Append one event. A write failure is reported, not fatal to the call.
    pub fn record(&self, event: &CallEvent) {
        let append = || -> std::io::Result<()> {
            let mut line = serde_json::to_vec(event)?;
            line.push(b'\n');
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.file)?;
            file.write_all(&line)?;
            file.sync_all()
        };
        if let Err(error) = append() {
            eprintln!("recording call audit event failed: {error}");
        }
    }
}

/// The trace id of a valid W3C `traceparent` (`00-<32 hex>-<16 hex>-<2 hex>`),
/// or a new random one.
pub fn trace_id(traceparent: Option<&str>) -> String {
    traceparent
        .and_then(|value| {
            let mut parts = value.trim().split('-');
            let (version, trace, span, flags) =
                (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            let hex = |s: &str, len| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
            let valid = version == "00"
                && parts.next().is_none()
                && hex(trace, 32)
                && hex(span, 16)
                && hex(flags, 2)
                && trace.bytes().any(|b| b != b'0')
                && span.bytes().any(|b| b != b'0');
            valid.then(|| trace.to_ascii_lowercase())
        })
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string())
}

/// A `traceparent` for the upstream call: same trace, new span, sampled.
pub fn traceparent(trace_id: &str) -> String {
    let span = &uuid::Uuid::new_v4().simple().to_string()[..16];
    format!("00-{trace_id}-{span}-01")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_ids_come_from_valid_traceparents_only() {
        let incoming = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(trace_id(Some(incoming)), "4bf92f3577b34da6a3ce929d0e0e4736");
        for invalid in [
            "garbage",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        ] {
            let generated = trace_id(Some(invalid));
            assert_eq!(generated.len(), 32);
            assert_ne!(generated, "4bf92f3577b34da6a3ce929d0e0e4736");
        }
        let outgoing = traceparent("4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(
            trace_id(Some(&outgoing)),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }
}
