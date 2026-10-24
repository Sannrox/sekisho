//! Rate limits per agent, user and tenant, and caps on responses and
//! execution time.
//!
//! Each scope has a token bucket (`rate_per_sec` refill, `burst` capacity).
//! A call costs its tool's weight (default 1) and is admitted only if every
//! applicable bucket can pay; otherwise nothing is spent and the caller gets
//! the wait until all of them could. Tenant limits apply when the token
//! carries a `tenant_id`. Defaults are generous so interactive use is not
//! throttled.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    pub rate_per_sec: f64,
    pub burst: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(default = "agent_rate")]
    pub agent: Rate,
    #[serde(default = "user_rate")]
    pub user: Rate,
    #[serde(default = "tenant_rate")]
    pub tenant: Rate,
    /// Cost weight per registry tool key; others cost 1.
    #[serde(default)]
    pub costs: BTreeMap<String, f64>,
    /// Responses larger than this are truncated and marked.
    #[serde(default = "max_response_bytes")]
    pub max_response_bytes: usize,
    /// Upstream calls running longer than this fail.
    #[serde(default = "max_call_ms")]
    pub max_call_ms: u64,
}

fn agent_rate() -> Rate {
    Rate {
        rate_per_sec: 10.0,
        burst: 20.0,
    }
}
fn user_rate() -> Rate {
    Rate {
        rate_per_sec: 20.0,
        burst: 40.0,
    }
}
fn tenant_rate() -> Rate {
    Rate {
        rate_per_sec: 100.0,
        burst: 200.0,
    }
}
fn max_response_bytes() -> usize {
    1024 * 1024
}
fn max_call_ms() -> u64 {
    60_000
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            agent: agent_rate(),
            user: user_rate(),
            tenant: tenant_rate(),
            costs: BTreeMap::new(),
            max_response_bytes: max_response_bytes(),
            max_call_ms: max_call_ms(),
        }
    }
}

impl LimitsConfig {
    pub fn validate(&self) -> Result<(), String> {
        let rates = [self.agent, self.user, self.tenant];
        // NaN fails the comparisons. Infinity must not land in the shared file
        // (JSON encodes it as null and every later admit fails closed).
        let valid = |rate: &Rate| {
            rate.rate_per_sec.is_finite()
                && rate.burst.is_finite()
                && rate.rate_per_sec >= MIN_RATE
                && rate.burst >= 1.0
        };
        if !rates.iter().all(valid) {
            return Err(format!(
                "limits need rate_per_sec >= {MIN_RATE} and burst >= 1"
            ));
        }
        // A cost must fit every bucket, or that tool could never be paid for.
        let smallest_burst = rates
            .iter()
            .map(|rate| rate.burst)
            .fold(f64::INFINITY, f64::min);
        if !self
            .costs
            .values()
            .all(|cost| cost.is_finite() && *cost > 0.0 && *cost <= smallest_burst)
        {
            return Err("tool costs must be positive and at most the smallest burst".into());
        }
        if self.max_response_bytes < 256 || self.max_call_ms == 0 {
            return Err("max_response_bytes must be >= 256 and max_call_ms positive".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Scope {
    Agent,
    User,
    Tenant,
}

struct Bucket {
    tokens: f64,
    at: Instant,
    /// When the bucket is full again; only full buckets may be evicted.
    full_at: Instant,
}

/// Slowest refill accepted, so retry hints stay meaningful.
const MIN_RATE: f64 = 0.001;

/// Bucket keys idle this long are dropped once the table grows large.
const IDLE: Duration = Duration::from_secs(600);
const TABLE_LIMIT: usize = 100_000;

pub struct Limiter {
    config: LimitsConfig,
    buckets: Mutex<Buckets>,
    /// Shared replica file. Process death releases the flock.
    store: Option<PathBuf>,
}

#[derive(Default)]
struct Buckets {
    table: HashMap<(Scope, String), Bucket>,
    /// Last eviction sweep; sweeps run at most every [`SWEEP_EVERY`].
    swept: Option<Instant>,
}

const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Who a call is charged to.
pub struct Caller<'a> {
    pub agent: &'a str,
    pub user: &'a str,
    pub tenant: Option<&'a str>,
}

impl Limiter {
    pub fn new(config: LimitsConfig) -> Self {
        Self {
            config,
            buckets: Mutex::default(),
            store: None,
        }
    }

    /// Rate-limit buckets in `path`, shared by replicas.
    pub fn shared(config: LimitsConfig, path: impl Into<PathBuf>) -> Self {
        Self {
            config,
            buckets: Mutex::default(),
            store: Some(path.into()),
        }
    }

    pub fn from_config(limits: LimitsConfig, state_dir: Option<&Path>) -> Self {
        match state_dir {
            Some(dir) => Self::shared(limits, dir.join("limits.json")),
            None => Self::new(limits),
        }
    }

    pub fn config(&self) -> &LimitsConfig {
        &self.config
    }

    pub(crate) fn is_shared(&self) -> bool {
        self.store.is_some()
    }

    /// Charge `tool`'s cost to every scope, or return how long to wait.
    pub fn admit(&self, caller: &Caller<'_>, tool: &str, now: Instant) -> Result<(), Duration> {
        if let Some(path) = &self.store {
            return self.admit_file(path, caller, tool);
        }
        let cost = self.cost(tool);
        let scopes = self.scopes(caller);
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let due = buckets
            .swept
            .is_none_or(|swept| now.saturating_duration_since(swept) >= SWEEP_EVERY);
        if buckets.table.len() > TABLE_LIMIT && due {
            buckets.swept = Some(now);
            // Dropping a bucket resets it to full, so keep any still refilling.
            buckets.table.retain(|_, bucket| {
                now < bucket.full_at || now.saturating_duration_since(bucket.at) < IDLE
            });
        }
        let mut wait = Duration::ZERO;
        for (scope, key, rate) in &scopes {
            let bucket = buckets
                .table
                .entry((*scope, (*key).to_string()))
                .or_insert(Bucket {
                    tokens: rate.burst,
                    at: now,
                    full_at: now,
                });
            // Time only moves forward per bucket: a call that took the lock
            // late must not rewind it and earn the same refill twice.
            if now > bucket.at {
                let elapsed = (now - bucket.at).as_secs_f64();
                bucket.tokens = (bucket.tokens + elapsed * rate.rate_per_sec).min(rate.burst);
                bucket.at = now;
            }
            let missing = cost - bucket.tokens;
            if missing > 0.0 {
                let seconds = missing / rate.rate_per_sec;
                wait = wait.max(Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX));
            }
        }
        if !wait.is_zero() {
            return Err(wait);
        }
        for (scope, key, rate) in &scopes {
            if let Some(bucket) = buckets.table.get_mut(&(*scope, (*key).to_string())) {
                bucket.tokens -= cost;
                let refill = (rate.burst - bucket.tokens) / rate.rate_per_sec;
                bucket.full_at = Duration::try_from_secs_f64(refill)
                    .ok()
                    .and_then(|refill| now.checked_add(refill))
                    .unwrap_or(now + IDLE * 1_000);
            }
        }
        Ok(())
    }

    fn cost(&self, tool: &str) -> f64 {
        self.config.costs.get(tool).copied().unwrap_or(1.0)
    }

    fn scopes<'a>(&'a self, caller: &'a Caller<'_>) -> Vec<(Scope, &'a str, Rate)> {
        let mut scopes = vec![
            (Scope::Agent, caller.agent, self.config.agent),
            (Scope::User, caller.user, self.config.user),
        ];
        if let Some(tenant) = caller.tenant {
            scopes.push((Scope::Tenant, tenant, self.config.tenant));
        }
        scopes
    }

    /// Shared-file buckets. `Instant` is ignored; wall time is [`SystemTime`].
    /// IO failure fails closed with a 1s retry so a poisoned file cannot
    /// open the limiter.
    fn admit_file(&self, path: &Path, caller: &Caller<'_>, tool: &str) -> Result<(), Duration> {
        self.admit_file_io(path, caller, tool)
            .unwrap_or_else(|error| {
                eprintln!("shared rate-limit store failed: {error}");
                Err(Duration::from_secs(1))
            })
    }

    fn admit_file_io(
        &self,
        path: &Path,
        caller: &Caller<'_>,
        tool: &str,
    ) -> std::io::Result<Result<(), Duration>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Flock a sibling so process death cannot leave a half-written
        // `limits.json`; the data file is replaced by rename.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        let _lock = FileLock::exclusive(lock)?;
        let mut store: FileStore = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileStore::default(),
            Err(error) => return Err(error),
        };
        let result = self.admit_store(&mut store, caller, tool, now_ms());
        let out = serde_json::to_vec(&store)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let tmp = path.with_extension("tmp");
        {
            let mut tmp_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            tmp_file.write_all(&out)?;
            tmp_file.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            let _ = File::open(parent).and_then(|dir| dir.sync_all());
        }
        Ok(result)
    }

    fn admit_store(
        &self,
        store: &mut FileStore,
        caller: &Caller<'_>,
        tool: &str,
        now_ms: u64,
    ) -> Result<(), Duration> {
        let cost = self.cost(tool);
        let scopes = self.scopes(caller);
        if store.buckets.len() > TABLE_LIMIT {
            store.buckets.retain(|_, bucket| {
                now_ms < bucket.full_at_ms
                    || now_ms.saturating_sub(bucket.at_ms) < IDLE.as_millis() as u64
            });
        }
        let mut wait = Duration::ZERO;
        for (scope, key, rate) in &scopes {
            let bucket = store
                .buckets
                .entry(file_key(*scope, key))
                .or_insert(FileBucket {
                    tokens: rate.burst,
                    at_ms: now_ms,
                    full_at_ms: now_ms,
                });
            if now_ms > bucket.at_ms {
                let elapsed = (now_ms - bucket.at_ms) as f64 / 1000.0;
                bucket.tokens = (bucket.tokens + elapsed * rate.rate_per_sec).min(rate.burst);
                bucket.at_ms = now_ms;
            }
            let missing = cost - bucket.tokens;
            if missing > 0.0 {
                let seconds = missing / rate.rate_per_sec;
                wait = wait.max(Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX));
            }
        }
        if !wait.is_zero() {
            return Err(wait);
        }
        for (scope, key, rate) in &scopes {
            if let Some(bucket) = store.buckets.get_mut(&file_key(*scope, key)) {
                bucket.tokens -= cost;
                let refill = (rate.burst - bucket.tokens) / rate.rate_per_sec;
                bucket.full_at_ms = Duration::try_from_secs_f64(refill)
                    .ok()
                    .and_then(|refill| now_ms.checked_add(refill.as_millis() as u64))
                    .unwrap_or(now_ms.saturating_add(IDLE.as_millis() as u64 * 1_000));
            }
        }
        Ok(())
    }
}

#[derive(Default, Serialize, Deserialize)]
struct FileStore {
    #[serde(default)]
    buckets: BTreeMap<String, FileBucket>,
}

#[derive(Serialize, Deserialize)]
struct FileBucket {
    tokens: f64,
    at_ms: u64,
    full_at_ms: u64,
}

fn file_key(scope: Scope, id: &str) -> String {
    match scope {
        Scope::Agent => format!("agent:{id}"),
        Scope::User => format!("user:{id}"),
        Scope::Tenant => format!("tenant:{id}"),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Exclusive `flock` on an open file. Process death releases it.
pub(crate) struct FileLock {
    pub(crate) file: File,
}

impl FileLock {
    pub(crate) fn exclusive(file: File) -> std::io::Result<Self> {
        // SAFETY: `file` is an open fd; LOCK_EX blocks until this process holds it.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: `file` is still the fd we locked.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Truncate `result` to `cap` bytes of text and say so in an extra block.
/// Returns the result and whether it was truncated.
pub(crate) fn cap_response(result: CallToolResult, cap: usize) -> (CallToolResult, bool) {
    let size = serde_json::to_vec(&result).map_or(0, |bytes| bytes.len());
    if size <= cap {
        return (result, false);
    }
    // Text blocks, or the structured content when there are none.
    let mut text: String = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty()
        && let Some(structured) = &result.structured_content
    {
        text = structured.to_string();
    }
    let marker = ContentBlock::text(format!(
        "[sekisho: response truncated from {size} to at most {cap} bytes]"
    ));
    let build = |end: usize| {
        CallToolResult::success(vec![
            ContentBlock::text(text[..end].to_string()),
            marker.clone(),
        ])
    };
    let fits = |end: usize| serde_json::to_vec(&build(end)).is_ok_and(|bytes| bytes.len() <= cap);
    // Longest prefix (on a char boundary) whose encoding fits. JSON escaping
    // makes encoded size grow unevenly with text, so search, don't estimate.
    let (mut low, mut high) = (0, text.len());
    while low < high {
        let mut middle = low + (high - low).div_ceil(2);
        while !text.is_char_boundary(middle) {
            middle -= 1;
        }
        if middle > low && fits(middle) {
            low = middle;
        } else {
            high = if middle > low { middle - 1 } else { low };
        }
    }
    let mut capped = build(low);
    capped.is_error = result.is_error;
    (capped, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(rate: f64, burst: f64) -> Limiter {
        Limiter::new(LimitsConfig {
            agent: Rate {
                rate_per_sec: rate,
                burst,
            },
            ..LimitsConfig::default()
        })
    }

    fn caller() -> Caller<'static> {
        Caller {
            agent: "agent-7",
            user: "user-1",
            tenant: None,
        }
    }

    #[test]
    fn bucket_throttles_at_the_configured_rate_with_a_retry_hint() {
        let limiter = limiter(2.0, 2.0);
        let start = Instant::now();
        assert!(limiter.admit(&caller(), "t", start).is_ok());
        assert!(limiter.admit(&caller(), "t", start).is_ok());
        let wait = limiter.admit(&caller(), "t", start).unwrap_err();
        assert_eq!(wait, Duration::from_millis(500));
        assert!(limiter.admit(&caller(), "t", start + wait).is_ok());
        assert!(limiter.admit(&caller(), "t", start + wait).is_err());
    }

    #[test]
    fn refused_calls_spend_nothing_and_costs_weigh() {
        let config = LimitsConfig {
            agent: Rate {
                rate_per_sec: 1.0,
                burst: 3.0,
            },
            costs: [("heavy".to_string(), 3.0)].into(),
            ..LimitsConfig::default()
        };
        let limiter = Limiter::new(config);
        let now = Instant::now();
        assert!(limiter.admit(&caller(), "light", now).is_ok());
        assert!(limiter.admit(&caller(), "heavy", now).is_err());
        assert!(limiter.admit(&caller(), "light", now).is_ok());
    }

    #[test]
    fn truncation_fits_the_encoded_size_of_escaped_text() {
        let escaped = CallToolResult::success(vec![ContentBlock::text("\n\"".repeat(2_000))]);
        let (capped, truncated) = cap_response(escaped, 1_000);
        assert!(truncated);
        assert!(serde_json::to_vec(&capped).unwrap().len() <= 1_000);
        let kept = capped.content[0].as_text().unwrap().text.len();
        assert!(kept > 300, "a fitting prefix is kept: {kept}");
    }

    #[test]
    fn structured_only_results_keep_a_text_preview() {
        let big = CallToolResult::structured(serde_json::json!({ "rows": "y".repeat(5_000) }));
        let (capped, truncated) = cap_response(big, 1_000);
        assert!(truncated);
        let preview = capped.content[0].as_text().unwrap().text.clone();
        assert!(preview.starts_with("{\"rows\":\"yyy"), "{preview}");
    }

    #[test]
    fn costs_must_fit_every_bucket() {
        let config = LimitsConfig {
            costs: [("huge".to_string(), 100.0)].into(),
            ..LimitsConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn non_finite_rates_are_rejected() {
        let inf = LimitsConfig {
            agent: Rate {
                rate_per_sec: 1.0,
                burst: f64::INFINITY,
            },
            ..LimitsConfig::default()
        };
        assert!(inf.validate().is_err());
    }

    #[test]
    fn oversized_responses_are_truncated_and_marked() {
        let big = CallToolResult::success(vec![ContentBlock::text("x".repeat(5_000))]);
        let (capped, truncated) = cap_response(big, 1_000);
        assert!(truncated);
        assert!(serde_json::to_vec(&capped).unwrap().len() <= 1_000);
        let marker = capped.content[1].as_text().unwrap().text.clone();
        assert!(marker.contains("truncated from"), "{marker}");
        let small = CallToolResult::success(vec![ContentBlock::text("ok")]);
        assert!(!cap_response(small, 1_000).1);
    }

    #[test]
    fn two_limiters_share_file_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limits.json");
        let config = LimitsConfig {
            agent: Rate {
                rate_per_sec: 1.0,
                burst: 2.0,
            },
            ..LimitsConfig::default()
        };
        let a = Limiter::shared(config.clone(), &path);
        let b = Limiter::shared(config, &path);
        let now = Instant::now();
        assert!(a.admit(&caller(), "t", now).is_ok());
        assert!(b.admit(&caller(), "t", now).is_ok());
        assert!(a.admit(&caller(), "t", now).is_err());
    }

    #[test]
    fn shared_file_serializes_concurrent_admits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limits.json");
        let config = LimitsConfig {
            agent: Rate {
                rate_per_sec: 0.001,
                burst: 1.0,
            },
            ..LimitsConfig::default()
        };
        let a = Limiter::shared(config.clone(), &path);
        let b = Limiter::shared(config, &path);
        let ok = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for limiter in [&a, &b] {
                scope.spawn(|| {
                    if limiter.admit(&caller(), "t", Instant::now()).is_ok() {
                        ok.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(ok.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(a.admit(&caller(), "t", Instant::now()).is_err());
    }

    #[test]
    fn poisoned_store_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limits.json");
        std::fs::write(&path, "not-json").unwrap();
        let limiter = Limiter::shared(LimitsConfig::default(), &path);
        let wait = limiter.admit(&caller(), "t", Instant::now()).unwrap_err();
        assert_eq!(wait, Duration::from_secs(1));
    }

    #[test]
    fn empty_existing_store_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limits.json");
        std::fs::write(&path, "").unwrap();
        let limiter = Limiter::shared(LimitsConfig::default(), &path);
        let wait = limiter.admit(&caller(), "t", Instant::now()).unwrap_err();
        assert_eq!(wait, Duration::from_secs(1));
    }

    #[test]
    fn leftover_tmp_does_not_poison_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limits.json");
        std::fs::write(path.with_extension("tmp"), "not-json").unwrap();
        let limiter = Limiter::shared(LimitsConfig::default(), &path);
        assert!(limiter.admit(&caller(), "t", Instant::now()).is_ok());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(&body).is_ok(),
            "{body}"
        );
    }
}
