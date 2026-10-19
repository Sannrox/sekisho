//! Process counters exported at `/metrics` in the Prometheus text format.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Metrics {
    accepted: AtomicU64,
    rejected: AtomicU64,
    /// Tool calls by decision: allowed, denied, failed, throttled.
    calls: [AtomicU64; 4],
    truncated: AtomicU64,
}

impl Metrics {
    pub fn record(&self, accepted: bool) {
        let counter = if accepted {
            &self.accepted
        } else {
            &self.rejected
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_call(&self, decision: crate::audit::Decision) {
        use crate::audit::Decision;
        let index = match decision {
            Decision::Allowed => 0,
            Decision::Denied => 1,
            Decision::Failed => 2,
            Decision::Throttled => 3,
        };
        self.calls[index].fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_truncated(&self) {
        self.truncated.fetch_add(1, Ordering::Relaxed);
    }

    pub fn render(&self, verifier: &str) -> String {
        let calls = |index: usize| self.calls[index].load(Ordering::Relaxed);
        format!(
            "# TYPE sekisho_build_info gauge\n\
             sekisho_build_info{{version=\"{version}\",verifier=\"{verifier}\"}} 1\n\
             # TYPE sekisho_token_verifications_total counter\n\
             sekisho_token_verifications_total{{result=\"accepted\"}} {accepted}\n\
             sekisho_token_verifications_total{{result=\"rejected\"}} {rejected}\n\
             # TYPE sekisho_tool_calls_total counter\n\
             sekisho_tool_calls_total{{decision=\"allowed\"}} {allowed}\n\
             sekisho_tool_calls_total{{decision=\"denied\"}} {denied}\n\
             sekisho_tool_calls_total{{decision=\"failed\"}} {failed}\n\
             sekisho_tool_calls_total{{decision=\"throttled\"}} {throttled}\n\
             # TYPE sekisho_truncated_responses_total counter\n\
             sekisho_truncated_responses_total {truncated}\n",
            version = env!("CARGO_PKG_VERSION"),
            accepted = self.accepted.load(Ordering::Relaxed),
            rejected = self.rejected.load(Ordering::Relaxed),
            allowed = calls(0),
            denied = calls(1),
            failed = calls(2),
            throttled = calls(3),
            truncated = self.truncated.load(Ordering::Relaxed),
        )
    }
}
