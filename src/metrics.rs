//! Process counters exported at `/metrics` in the Prometheus text format.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Metrics {
    accepted: AtomicU64,
    rejected: AtomicU64,
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

    pub fn render(&self, verifier: &str) -> String {
        format!(
            "# TYPE sekisho_build_info gauge\n\
             sekisho_build_info{{version=\"{version}\",verifier=\"{verifier}\"}} 1\n\
             # TYPE sekisho_token_verifications_total counter\n\
             sekisho_token_verifications_total{{result=\"accepted\"}} {accepted}\n\
             sekisho_token_verifications_total{{result=\"rejected\"}} {rejected}\n",
            version = env!("CARGO_PKG_VERSION"),
            accepted = self.accepted.load(Ordering::Relaxed),
            rejected = self.rejected.load(Ordering::Relaxed),
        )
    }
}
