//! Where replica state lives (#26): rate-limit buckets, the tool registry,
//! agent policy, the call audit and mitigations.
//!
//! [`StateBackend`] is the seam. [`Directory`] is the default backend: the
//! files every earlier release used, in the same formats, so replicas of both
//! versions can share them during a rolling deploy. Its consistency rests on
//! exclusive file locks, so replicas must share one host or a filesystem
//! whose locks every replica sees.
//!
//! With the `postgres` feature, `[state] backend = "postgres"` keeps rate
//! limits and the call audit in PostgreSQL (`state::postgres`), so replicas on
//! separate hosts share them. Registry, policy and mitigations stay on the
//! directory backend until #47.
//!
//! Every method blocks on storage; async callers run them on the blocking
//! pool. A store error is an error, never an empty answer, so callers fail
//! closed.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audit::{CallEvent, CallLog, CallPage};
use crate::config::{Config, StateConfig};
use crate::limits::Rate;
use crate::mitigations::{Commit, MitigationError, MitigationFiles, State};
use crate::policy::{PolicyError, PolicyFile};
use crate::registry::{AuditRecord, Registry, RegistryError};

/// One part of the replica state, as errors and readiness name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Store {
    Limits,
    Registry,
    Policy,
    Audit,
    Mitigations,
}

impl std::fmt::Display for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limits => "rate-limit",
            Self::Registry => "registry",
            Self::Policy => "policy",
            Self::Audit => "audit",
            Self::Mitigations => "mitigation",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{store} store: {source}")]
    Io {
        store: Store,
        #[source]
        source: std::io::Error,
    },
    #[error("{store} store is not configured")]
    NotConfigured { store: Store },
    #[error("{store} store did not answer in time")]
    Timeout { store: Store },
    #[cfg(feature = "postgres")]
    #[error("{store} store: {}", self::postgres::detail(source))]
    Postgres {
        store: Store,
        #[source]
        source: ::postgres::Error,
    },
}

/// How long a caller waits before retrying a call a store failure refused.
pub const STORE_RETRY: Duration = Duration::from_secs(1);

/// The longest a call waits on the state backend before forwarding (rate
/// limit, `started` audit event, and the outcome of a call refused before
/// forwarding), and again for the outcome of a forwarded call; the longest
/// a readiness probe waits. Past it the call fails closed with
/// [`STORE_RETRY`] and readiness is false, whatever the backend is stuck
/// on; the blocking work may still finish later.
pub const STORE_DEADLINE: Duration = Duration::from_secs(5);

/// Run blocking store work off the async workers, waiting at most the
/// remaining `budget` and taking the time spent from it, so several steps
/// of one call share one budget. A panic or an exhausted budget is a
/// [`StoreError::Timeout`] of `store`.
pub async fn blocking<T: Send + 'static>(
    store: Store,
    budget: &mut Duration,
    work: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, StoreError> {
    let started = Instant::now();
    let result = tokio::time::timeout(*budget, tokio::task::spawn_blocking(work)).await;
    *budget = budget.saturating_sub(started.elapsed());
    match result {
        Ok(Ok(result)) => result,
        Ok(Err(_)) | Err(_) => Err(StoreError::Timeout { store }),
    }
}

/// Why [`open`] could not build the configured backend.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("reading the postgres URL file {path}: {source}")]
    UrlFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[cfg(feature = "postgres")]
    #[error("postgres URL: {0}")]
    Url(#[source] ::postgres::Error),
    #[cfg(feature = "postgres")]
    #[error("postgres TLS: {0}")]
    Tls(#[source] rustls::Error),
    #[cfg(not(feature = "postgres"))]
    #[error("[state] backend = \"postgres\" needs a build with the postgres feature")]
    NotBuilt,
}

/// One rate-limit bucket a call is charged to: its key and refill rate.
pub struct Bucket<'a> {
    pub key: &'a str,
    pub rate: Rate,
}

/// Whether a store's writer creates the parent directory of its file. A
/// probe may only stand in for a missing directory when the writer would
/// create it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parent {
    /// The writer calls `create_dir_all`: the nearest existing ancestor
    /// must be writable.
    Created,
    /// The writer opens the file in place: its directory must exist and be
    /// writable.
    MustExist,
}

/// Probes never create real files: a file a writer would create, such as
/// an audit log whose directory entry the writer syncs on creation, must
/// still be absent for that writer. `path` opens with `options` (the
/// writer's flags without `create`) when it exists; when it does not, the
/// scratch check in its directory proves the writer could create it.
fn probe_existing(
    path: &Path,
    options: &std::fs::OpenOptions,
    parent: Parent,
) -> std::io::Result<Option<File>> {
    match options.open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            probe_writable(path, parent).map(|()| None)
        }
        Err(error) => Err(error),
    }
}

/// Whether an append log opens as its writer opens it: for appending, and
/// also for reading when `read` (writers that inspect the tail), and its
/// directory takes a scratch write, so a full filesystem shows even while
/// the log opens. The log itself is neither written nor created.
pub(crate) fn probe_append(path: &Path, read: bool, parent: Parent) -> std::io::Result<()> {
    let existing = probe_existing(
        path,
        std::fs::OpenOptions::new().read(read).append(true),
        parent,
    )?;
    // An absent log already had its directory checked.
    match existing {
        Some(_) => probe_writable(path, parent),
        None => Ok(()),
    }
}

/// Test hook: directories whose scratch writes fail as if the disk is full.
#[cfg(test)]
pub(crate) static FULL_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The writer's exclusive lock on `lock_path`, opened for writing (and
/// reading when `read`) and held until the result drops. An absent lock
/// file is not created; nothing else can hold it then.
pub(crate) fn probe_lock(
    lock_path: &Path,
    read: bool,
    parent: Parent,
) -> std::io::Result<Option<File>> {
    let lock = probe_existing(
        lock_path,
        std::fs::OpenOptions::new().read(read).write(true),
        parent,
    )?;
    if let Some(file) = &lock {
        file.lock()?;
    }
    Ok(lock)
}

/// Readiness of each store: `Err` carries the reason, for the log only.
pub type Probe = Vec<(Store, Result<(), String>)>;

/// Shared replica state. Implementations keep every write durable before
/// they return, and serialize read-modify-writes across replicas.
pub trait StateBackend: Send + Sync {
    /// Short name for health output (`directory` or `postgres`).
    fn kind(&self) -> &'static str;

    /// Charge `cost` to every bucket at wall time `now_ms`, or nothing and
    /// return the wait until all of them could pay.
    fn take(
        &self,
        buckets: &[Bucket<'_>],
        cost: f64,
        now_ms: u64,
    ) -> Result<Result<(), Duration>, StoreError>;

    fn registry(&self) -> Result<Registry, RegistryError>;

    /// Change the registry under the backend's exclusive lock and return
    /// the result. With `audit`, the record is durable before the change is
    /// committed, so a change that cannot be audited is never committed.
    fn edit_registry(
        &self,
        change: &mut dyn FnMut(&mut Registry) -> Result<(), RegistryError>,
        audit: Option<&AuditRecord>,
    ) -> Result<Registry, RegistryError>;

    /// The agent policy, read fresh. Missing or unreadable is an error.
    fn policy(&self) -> Result<PolicyFile, PolicyError>;

    /// Append one call event, durable before it returns.
    fn record_call(&self, event: &CallEvent) -> Result<(), StoreError>;

    /// Events of `trace_id` for `tenant` after the opaque `cursor`.
    fn read_calls(
        &self,
        tenant: &str,
        trace_id: &str,
        cursor: u64,
        limit: usize,
    ) -> Result<CallPage, StoreError>;

    fn mitigations(&self) -> Result<State, MitigationError>;

    /// Change mitigations under the backend's exclusive lock. A
    /// [`Commit::Changes`] commits the state and its audit events together;
    /// a [`Commit::Refusal`] audits the refusal and leaves the state as it was.
    fn edit_mitigations(
        &self,
        change: &mut dyn FnMut(&mut State) -> Result<Commit, MitigationError>,
    ) -> Result<(), MitigationError>;

    /// Whether each store can do work now.
    fn probe(&self) -> Probe;
}

/// The backend `config` selects. Opening connects nothing; the PostgreSQL
/// backend connects, and migrates its schema, on first use.
pub fn open(config: &Config) -> Result<Arc<dyn StateBackend>, OpenError> {
    match &config.state {
        StateConfig::Directory => Ok(Arc::new(Directory::new(config))),
        #[cfg(feature = "postgres")]
        StateConfig::Postgres { url_file } => {
            Ok(Arc::new(self::postgres::Postgres::open(config, url_file)?))
        }
        #[cfg(not(feature = "postgres"))]
        StateConfig::Postgres { .. } => Err(OpenError::NotBuilt),
    }
}

#[cfg(feature = "postgres")]
pub mod postgres;

/// The last probe, reused for [`PROBE_TTL`].
#[derive(Default)]
pub(crate) struct ProbeCache(Mutex<Option<(Instant, Probe)>>);

impl ProbeCache {
    /// The cached probe while fresh, else `probe` run and cached.
    pub(crate) fn get(&self, probe: impl FnOnce() -> Probe) -> Probe {
        let mut probed = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, probe)) = probed.as_ref()
            && at.elapsed() < PROBE_TTL
        {
            return probe.clone();
        }
        let probe = probe();
        *probed = Some((Instant::now(), probe.clone()));
        probe
    }
}

/// Files on a local or shared filesystem; see the module docs.
pub struct Directory {
    /// `limits.json` in `[cluster] state_dir`; unset keeps limits in-process.
    pub(crate) limits_file: Option<PathBuf>,
    pub(crate) registry_file: PathBuf,
    pub(crate) registry_audit_file: PathBuf,
    pub(crate) policy_file: PathBuf,
    pub(crate) calls: CallLog,
    pub(crate) mitigations: MitigationFiles,
    probed: ProbeCache,
}

/// How long a probe result is reused, so frequent `/readyz` polls do not
/// each write scratch files or open transactions. A store that breaks shows
/// as unavailable within this long.
pub const PROBE_TTL: Duration = Duration::from_secs(1);

/// Whether the write path next to `file` works: in its directory (or,
/// with [`Parent::Created`], the nearest existing ancestor), create a
/// uniquely named scratch file (pid and random suffix), write and fsync a
/// byte, rename it, and remove it. `file` itself is never touched and no
/// directory is created; a missing directory the writer does not create
/// fails.
pub(crate) fn probe_writable(file: &Path, parent: Parent) -> std::io::Result<()> {
    let mut dir = file;
    loop {
        dir = match dir.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        if parent == Parent::MustExist || dir.exists() || dir == Path::new(".") {
            break;
        }
    }
    #[cfg(test)]
    if FULL_DIRS.lock().unwrap().iter().any(|full| full == dir) {
        return Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
    }
    let stem = format!(
        ".sekisho-probe-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    let (first, second) = (dir.join(format!("{stem}.a")), dir.join(format!("{stem}.b")));
    let attempt = || -> std::io::Result<()> {
        let mut scratch = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&first)?;
        scratch.write_all(b"0")?;
        scratch.sync_all()?;
        std::fs::rename(&first, &second)?;
        std::fs::remove_file(&second)
    };
    let result = attempt();
    if result.is_err() {
        let _ = std::fs::remove_file(&first);
        let _ = std::fs::remove_file(&second);
    }
    result
}

impl Directory {
    pub fn new(config: &Config) -> Self {
        Self {
            limits_file: config
                .cluster
                .state_dir
                .as_ref()
                .map(|dir| dir.join("limits.json")),
            registry_file: config.registry.file.clone(),
            registry_audit_file: config.registry.audit_file.clone(),
            policy_file: config.policy.file.clone(),
            calls: CallLog::new(config.audit.calls_file.clone()),
            mitigations: MitigationFiles::new(&config.mitigations),
            probed: ProbeCache::default(),
        }
    }

    /// Every file under `dir` with its default name; for unit tests.
    #[cfg(test)]
    pub(crate) fn in_dir(dir: &std::path::Path) -> Self {
        Self {
            limits_file: Some(dir.join("limits.json")),
            registry_file: dir.join("registry.json"),
            registry_audit_file: dir.join("registry-audit.jsonl"),
            policy_file: dir.join("agents.toml"),
            calls: CallLog::new(dir.join("calls.jsonl")),
            mitigations: MitigationFiles::new(&crate::mitigations::MitigationsConfig {
                file: Some(dir.join("mitigations.json")),
                audit_file: Some(dir.join("mitigations-audit.jsonl")),
            }),
            probed: ProbeCache::default(),
        }
    }
}

impl StateBackend for Directory {
    fn kind(&self) -> &'static str {
        "directory"
    }

    fn take(
        &self,
        buckets: &[Bucket<'_>],
        cost: f64,
        now_ms: u64,
    ) -> Result<Result<(), Duration>, StoreError> {
        let path = self.limits_file.as_ref().ok_or(StoreError::NotConfigured {
            store: Store::Limits,
        })?;
        crate::limits::take_file(path, buckets, cost, now_ms).map_err(|source| StoreError::Io {
            store: Store::Limits,
            source,
        })
    }

    fn registry(&self) -> Result<Registry, RegistryError> {
        Registry::load(&self.registry_file)
    }

    fn edit_registry(
        &self,
        change: &mut dyn FnMut(&mut Registry) -> Result<(), RegistryError>,
        audit: Option<&AuditRecord>,
    ) -> Result<Registry, RegistryError> {
        crate::registry::edit(&self.registry_file, |registry| {
            change(registry)?;
            match audit {
                Some(record) => crate::registry::append_record(&self.registry_audit_file, record),
                None => Ok(()),
            }
        })
        .map(|((), registry)| registry)
    }

    fn policy(&self) -> Result<PolicyFile, PolicyError> {
        PolicyFile::load(&self.policy_file)
    }

    fn record_call(&self, event: &CallEvent) -> Result<(), StoreError> {
        self.calls.append(event).map_err(|source| StoreError::Io {
            store: Store::Audit,
            source,
        })
    }

    fn read_calls(
        &self,
        tenant: &str,
        trace_id: &str,
        cursor: u64,
        limit: usize,
    ) -> Result<CallPage, StoreError> {
        self.calls
            .read(tenant, trace_id, cursor, limit)
            .map_err(|source| StoreError::Io {
                store: Store::Audit,
                source,
            })
    }

    fn mitigations(&self) -> Result<State, MitigationError> {
        self.mitigations.load()
    }

    fn edit_mitigations(
        &self,
        change: &mut dyn FnMut(&mut State) -> Result<Commit, MitigationError>,
    ) -> Result<(), MitigationError> {
        self.mitigations.edit(change)
    }

    /// Reads every store a call needs and exercises each write path: the
    /// writer's lock, a scratch file next to its data ([`probe_writable`]),
    /// and its audit log opened with the writer's options. Results are
    /// reused for [`PROBE_TTL`].
    fn probe(&self) -> Probe {
        self.probed.get(|| self.probe_now())
    }
}

impl Directory {
    fn probe_now(&self) -> Probe {
        let reason = |error: &dyn std::fmt::Display| error.to_string();
        let mut stores = self.probe_files();
        stores.push((
            Store::Audit,
            crate::state::probe_append(self.calls.file(), true, Parent::MustExist)
                .map_err(|e| reason(&e)),
        ));
        if let Some(path) = &self.limits_file {
            stores.push((
                Store::Limits,
                crate::limits::probe_file(path).map_err(|e| reason(&e)),
            ));
        }
        stores
    }

    /// Policy, registry and mitigations: the stores every backend keeps in
    /// files so far.
    pub(crate) fn probe_files(&self) -> Probe {
        let reason = |error: &dyn std::fmt::Display| error.to_string();
        vec![
            (
                Store::Policy,
                self.policy().map(drop).map_err(|e| reason(&e)),
            ),
            (
                Store::Registry,
                crate::registry::probe(&self.registry_file, &self.registry_audit_file)
                    .map_err(|e| reason(&e)),
            ),
            (
                Store::Mitigations,
                self.mitigations.probe().map_err(|e| reason(&e)),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_names_each_store_that_cannot_do_work() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Directory::in_dir(dir.path());
        let failing = |backend: &Directory| -> Vec<Store> {
            backend
                .probe_now()
                .into_iter()
                .filter(|(_, result)| result.is_err())
                .map(|(store, _)| store)
                .collect()
        };
        // No policy file: every call would be refused.
        assert_eq!(failing(&backend), [Store::Policy]);
        std::fs::write(dir.path().join("agents.toml"), "").unwrap();
        assert!(failing(&backend).is_empty());
        std::fs::write(dir.path().join("limits.json"), "not-json").unwrap();
        std::fs::write(dir.path().join("mitigations.json"), "not-json").unwrap();
        assert_eq!(failing(&backend), [Store::Mitigations, Store::Limits]);
    }

    #[test]
    fn a_probe_creates_nothing_and_the_first_write_still_syncs_its_log() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agents.toml"), "").unwrap();
        let backend = Directory {
            policy_file: dir.path().join("agents.toml"),
            registry_file: dir.path().join("registry.json"),
            registry_audit_file: dir.path().join("registry-audit.jsonl"),
            calls: CallLog::new(dir.path().join("calls.jsonl")),
            ..Directory::in_dir(&dir.path().join("state"))
        };
        assert!(backend.probe_now().iter().all(|(_, result)| result.is_ok()));
        let names = || -> Vec<_> {
            std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect()
        };
        assert_eq!(names(), ["agents.toml"], "the probe created nothing");

        // The first refusal creates the audit log and syncs its directory.
        let synced = Arc::clone(&backend.mitigations.synced_dirs);
        let audit_file = backend.mitigations.audit_file.clone();
        std::fs::create_dir(dir.path().join("state")).unwrap();
        let store = crate::mitigations::MitigationStore::new(Arc::new(backend));
        let refused = store.apply(
            &crate::mitigations::Standalone,
            crate::mitigations::Mitigation {
                decision: "not a reference!".into(),
                operator: "ops-1".into(),
                target: crate::mitigations::Target::Tool {
                    tool: "tools.a".into(),
                },
                scope: crate::mitigations::Scope::All,
                applied_at_unix_ms: 0,
                expires_at_unix_ms: None,
            },
            10,
        );
        assert!(refused.is_err());
        assert!(audit_file.exists());
        assert!(synced.lock().unwrap().contains(&dir.path().join("state")));
    }

    #[test]
    fn a_missing_directory_is_ready_only_where_its_writer_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agents.toml"), "").unwrap();
        let unavailable = |backend: &Directory| -> Vec<Store> {
            backend
                .probe_now()
                .into_iter()
                .filter(|(_, result)| result.is_err())
                .map(|(store, _)| store)
                .collect()
        };
        // Limits and mitigations create their state directory.
        let state = Directory {
            policy_file: dir.path().join("agents.toml"),
            registry_file: dir.path().join("registry.json"),
            registry_audit_file: dir.path().join("registry-audit.jsonl"),
            calls: CallLog::new(dir.path().join("calls.jsonl")),
            ..Directory::in_dir(&dir.path().join("missing"))
        };
        assert!(unavailable(&state).is_empty());
        // The registry and the call audit do not create theirs.
        let missing = dir.path().join("missing");
        let logs = Directory {
            policy_file: dir.path().join("agents.toml"),
            registry_file: missing.join("registry.json"),
            calls: CallLog::new(missing.join("calls.jsonl")),
            ..Directory::in_dir(dir.path())
        };
        assert_eq!(unavailable(&logs), [Store::Registry, Store::Audit]);
        assert!(!missing.exists(), "the probe created nothing");
    }

    #[test]
    fn a_full_audit_directory_fails_its_store_even_when_the_log_opens() {
        let root = tempfile::tempdir().unwrap();
        let logs = root.path().join("logs");
        std::fs::create_dir(&logs).unwrap();
        std::fs::write(root.path().join("agents.toml"), "").unwrap();
        let backend = Directory {
            policy_file: root.path().join("agents.toml"),
            registry_audit_file: logs.join("registry-audit.jsonl"),
            calls: CallLog::new(logs.join("calls.jsonl")),
            mitigations: crate::mitigations::MitigationFiles::new(
                &crate::mitigations::MitigationsConfig {
                    file: Some(root.path().join("mitigations.json")),
                    audit_file: Some(logs.join("mitigations-audit.jsonl")),
                },
            ),
            ..Directory::in_dir(root.path())
        };
        for log in [
            "registry-audit.jsonl",
            "calls.jsonl",
            "mitigations-audit.jsonl",
        ] {
            std::fs::write(logs.join(log), "{}\n").unwrap();
        }
        let unavailable = || -> Vec<Store> {
            backend
                .probe_now()
                .into_iter()
                .filter(|(_, result)| result.is_err())
                .map(|(store, _)| store)
                .collect()
        };
        assert!(unavailable().is_empty());
        FULL_DIRS.lock().unwrap().push(logs.clone());
        let full = unavailable();
        FULL_DIRS.lock().unwrap().retain(|dir| *dir != logs);
        assert_eq!(full, [Store::Registry, Store::Mitigations, Store::Audit]);
        assert!(unavailable().is_empty());
        for log in [
            "registry-audit.jsonl",
            "calls.jsonl",
            "mitigations-audit.jsonl",
        ] {
            assert_eq!(std::fs::read(logs.join(log)).unwrap(), b"{}\n");
        }
    }

    #[tokio::test]
    async fn steps_of_one_call_share_one_store_budget() {
        let stalled = || -> Result<(), StoreError> {
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        };
        let mut budget = Duration::from_millis(200);
        let started = Instant::now();
        let first = blocking(Store::Limits, &mut budget, stalled).await;
        let second = blocking(Store::Audit, &mut budget, stalled).await;
        assert!(matches!(first, Err(StoreError::Timeout { .. })));
        assert!(matches!(second, Err(StoreError::Timeout { .. })));
        assert!(budget.is_zero());
        assert!(started.elapsed() < Duration::from_millis(400), "one budget");
    }

    #[test]
    fn a_read_only_directory_fails_the_write_probe_and_leaves_no_scratch() {
        // Root ignores directory permissions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("limits.json");
        probe_writable(&file, Parent::MustExist).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(dir.path(), mode(0o555)).unwrap();
        let result = probe_writable(&file, Parent::MustExist);
        std::fs::set_permissions(dir.path(), mode(0o755)).unwrap();
        assert!(result.is_err());
    }
}
