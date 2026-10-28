//! One audit event per tool call, linked to engine receipts by trace id.
//!
//! Events go to `[audit] calls_file`, one JSON line each: caller, agent,
//! tool, decision, latency, response size and the W3C trace id. They never
//! carry tokens, arguments or results. The trace id comes from the caller's
//! `traceparent` when it sends a valid one, otherwise it is generated; the
//! upstream call carries it in `traceparent` so the engine's receipt names
//! the same trace.
//!
//! Events are stored by the state backend ([`crate::state`]). Subjects in
//! `[audit] readers` read one trace back (`GET /audit/calls`), only for their
//! own `tenant_id`, oldest first, paged by an opaque cursor.
//!
//! In the directory backend ([`CallLog`]) replicas append to one shared file
//! under an exclusive `flock`, and the cursor is a byte offset into it. Reads
//! take no lock, so they never hold up an append; they stop before a partial
//! last line and scan a bounded number of bytes per page.

use std::io::{BufRead as _, Read as _, Seek as _, Write as _};
use std::path::PathBuf;
use std::sync::Arc;

use crate::limits::FileLock;
use crate::state::{StateBackend, StoreError};

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
    /// Verified `tenant_id` of the caller; reads are scoped to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub agent: String,
    pub tool: String,
    pub decision: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Guardrail summary (direction, action, rule names; never payload, #10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardrail: Option<String>,
    /// Approval gate for a marked tool (#21): `engine` when forwarded for the
    /// engine to park and decide, `required` when refused for lack of a route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    /// Decision reference of the mitigation that refused the call (#42).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitigation: Option<String>,
    pub latency_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_bytes: Option<usize>,
}

/// Most events one read returns.
pub const MAX_PAGE: usize = 500;
/// Most bytes one read scans, matching or not.
pub const MAX_SCAN_BYTES: u64 = 1 << 20;

/// One page of a trace. `next_cursor` is where the scan stopped; `complete`
/// is true once the scan reached the end of the file. A page cut short by
/// `limit` or the scan budget, or a trace still running, continues from
/// `next_cursor`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallPage {
    pub events: Vec<CallEvent>,
    pub next_cursor: u64,
    pub complete: bool,
}

pub struct CallAudit {
    store: Arc<dyn StateBackend>,
    readers: Vec<String>,
}

impl CallAudit {
    pub fn new(store: Arc<dyn StateBackend>) -> Self {
        Self {
            store,
            readers: Vec::new(),
        }
    }

    /// Subjects allowed to read events back (`[audit] readers`).
    pub fn readers(mut self, readers: Vec<String>) -> Self {
        self.readers = readers;
        self
    }

    /// Whether any subject may read; reads are off when none is configured.
    pub fn readable(&self) -> bool {
        !self.readers.is_empty()
    }

    pub fn may_read(&self, subject: &str) -> bool {
        self.readers.iter().any(|reader| reader == subject)
    }

    /// Events of `trace_id` recorded for `tenant` after `cursor`, at most
    /// `limit` (capped at [`MAX_PAGE`]).
    pub fn read(
        &self,
        tenant: &str,
        trace_id: &str,
        cursor: u64,
        limit: usize,
    ) -> Result<CallPage, StoreError> {
        self.store.read_calls(tenant, trace_id, cursor, limit)
    }

    /// Record one event, durable before it returns. A write failure is
    /// reported, not fatal to the call.
    pub fn record(&self, event: &CallEvent) {
        if let Err(error) = self.store.record_call(event) {
            eprintln!("recording call audit event failed: {error}");
        }
    }
}

/// The directory backend's call audit: one JSON line per event in one file.
pub(crate) struct CallLog {
    file: PathBuf,
}

impl CallLog {
    pub(crate) fn new(file: PathBuf) -> Self {
        Self { file }
    }

    pub(crate) fn file(&self) -> &std::path::Path {
        &self.file
    }

    /// Events of `trace_id` recorded for `tenant`, from byte `cursor` on, at
    /// most `limit` (capped at [`MAX_PAGE`]) and at most [`MAX_SCAN_BYTES`]
    /// scanned. A missing file is an empty page. Lines that do not parse (a
    /// cursor inside a line) are skipped.
    pub(crate) fn read(
        &self,
        tenant: &str,
        trace_id: &str,
        cursor: u64,
        limit: usize,
    ) -> std::io::Result<CallPage> {
        let limit = limit.clamp(1, MAX_PAGE);
        let file = match std::fs::File::open(&self.file) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CallPage {
                    events: Vec::new(),
                    next_cursor: cursor,
                    complete: true,
                });
            }
            Err(error) => return Err(error),
        };
        let mut file = file;
        let end = file.metadata()?.len();
        let start = cursor.min(end);
        let mut position = start;
        file.seek(std::io::SeekFrom::Start(position))?;
        let mut reader = std::io::BufReader::new(file);
        let mut events = Vec::new();
        let mut line = Vec::new();
        while events.len() < limit && position - start < MAX_SCAN_BYTES {
            line.clear();
            let budget = MAX_SCAN_BYTES - (position - start);
            let read = (&mut reader).take(budget).read_until(b'\n', &mut line)? as u64;
            if read == 0 {
                break;
            }
            if !line.ends_with(b"\n") {
                // A partial last line: its writer still holds the rest. A line
                // cut by the budget waits for the next page, unless it alone
                // exceeds the budget; then it is skipped so paging progresses
                // (the next page starts inside it, which does not parse).
                if position + read < end && position == start {
                    position += read;
                }
                break;
            }
            position += read;
            if let Ok(event) = serde_json::from_slice::<CallEvent>(&line)
                && event.trace_id == trace_id
                && event.tenant.as_deref() == Some(tenant)
            {
                events.push(event);
            }
        }
        let complete = position >= end;
        Ok(CallPage {
            events,
            next_cursor: position,
            complete,
        })
    }

    /// Append one event under the file lock, so replicas sharing the file do
    /// not interleave lines, and fsync it.
    pub(crate) fn append(&self, event: &CallEvent) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.file)?;
        let mut lock = FileLock::exclusive(file)?;
        // A writer that died mid-line left a fragment; end it so this
        // event starts its own line instead of joining it.
        let len = lock.file.metadata()?.len();
        if len > 0 {
            let mut last = [0_u8];
            lock.file.seek(std::io::SeekFrom::Start(len - 1))?;
            lock.file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                line.insert(0, b'\n');
            }
        }
        lock.file.write_all(&line)?;
        lock.file.sync_all()
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

    fn event(trace: &str, tenant: Option<&str>, tool: &str) -> CallEvent {
        CallEvent {
            at_unix_ms: 1,
            trace_id: trace.into(),
            subject: "user-1".into(),
            tenant: tenant.map(Into::into),
            agent: "agent-7".into(),
            tool: tool.into(),
            decision: Decision::Allowed,
            reason: None,
            guardrail: None,
            approval: None,
            mitigation: None,
            latency_ms: 1,
            response_bytes: None,
        }
    }

    #[test]
    fn reads_page_one_tenants_trace_and_stop_before_a_torn_line() {
        let dir = tempfile::tempdir().unwrap();
        let audit = CallLog::new(dir.path().join("calls.jsonl"));
        let (trace, other) = ("a".repeat(32), "b".repeat(32));
        assert!(audit.read("t1", &trace, 0, 10).unwrap().events.is_empty());
        for tool in ["one", "two", "three"] {
            audit.append(&event(&trace, Some("t1"), tool)).unwrap();
            audit.append(&event(&other, Some("t1"), tool)).unwrap();
            audit.append(&event(&trace, Some("t2"), tool)).unwrap();
        }
        let first = audit.read("t1", &trace, 0, 2).unwrap();
        let tools = |page: &CallPage| {
            page.events
                .iter()
                .map(|e| e.tool.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            (tools(&first), first.complete),
            (vec!["one".into(), "two".into()], false)
        );
        let rest = audit.read("t1", &trace, first.next_cursor, 2).unwrap();
        assert_eq!(
            (tools(&rest), rest.complete),
            (vec!["three".to_string()], true)
        );

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("calls.jsonl"))
            .unwrap();
        file.write_all(b"{\"partial").unwrap();
        let tail = audit.read("t1", &trace, rest.next_cursor, 2).unwrap();
        assert!(tail.events.is_empty());
        assert_eq!(
            tail.next_cursor, rest.next_cursor,
            "a torn line is not consumed"
        );
    }

    #[test]
    fn a_sparse_read_stops_at_the_scan_budget_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        let audit = CallLog::new(path.clone());
        let (trace, other) = ("a".repeat(32), "b".repeat(32));
        let filler = serde_json::to_string(&event(&other, Some("t1"), "noise")).unwrap() + "\n";
        let copies = (MAX_SCAN_BYTES as usize / filler.len()) + 10;
        std::fs::write(&path, filler.repeat(copies)).unwrap();
        audit.append(&event(&trace, Some("t1"), "late")).unwrap();
        let first = audit.read("t1", &trace, 0, 10).unwrap();
        assert!(first.events.is_empty() && !first.complete);
        assert!(first.next_cursor > MAX_SCAN_BYTES - filler.len() as u64);
        assert_eq!(
            first.next_cursor % filler.len() as u64,
            0,
            "stops at a line start"
        );
        let next = audit.read("t1", &trace, first.next_cursor, 10).unwrap();
        assert_eq!(next.events.len(), 1);
        assert!(next.complete);
    }

    #[test]
    fn a_line_longer_than_the_budget_is_skipped_across_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        let audit = CallLog::new(path.clone());
        let trace = "a".repeat(32);
        let huge = "x".repeat(MAX_SCAN_BYTES as usize * 3 / 2) + "\n";
        std::fs::write(&path, huge).unwrap();
        audit.append(&event(&trace, Some("t1"), "after")).unwrap();
        let (mut cursor, mut found) = (0, Vec::new());
        for _ in 0..4 {
            let page = audit.read("t1", &trace, cursor, 10).unwrap();
            found.extend(page.events);
            cursor = page.next_cursor;
            if page.complete {
                break;
            }
        }
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn an_append_after_a_torn_tail_starts_its_own_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        let audit = CallLog::new(path.clone());
        let trace = "a".repeat(32);
        std::fs::write(&path, b"{\"torn").unwrap();
        audit.append(&event(&trace, Some("t1"), "after")).unwrap();
        let page = audit.read("t1", &trace, 0, 10).unwrap();
        assert_eq!(page.events.len(), 1);
        assert!(page.complete);
    }
}
