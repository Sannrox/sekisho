//! The PostgreSQL backend (#46, #47): every store in one database that
//! replicas on separate hosts share.
//!
//! - **Registry, policy, mitigations.** One JSON document each in
//!   `sekisho_state`. An edit is one transaction that locks the document's
//!   row, applies the same change code as the directory backend, writes it
//!   and inserts its audit records (owner edits, mitigation events), so a
//!   change and its audit commit together or not at all. Reads take no
//!   lock. Policy and mitigations are read on every call, so a change
//!   applies to the next call on every replica; the registry is reloaded by
//!   every replica's [`crate::gateway::Federation`]. The documents do not
//!   exist until `sekisho state import` or `state init` writes all three
//!   with an `initialized` marker in one transaction; until then every read
//!   and edit fails as uninitialized, so calls are refused.
//! - **Rate limits.** One transaction per charge creates missing bucket rows
//!   full and locks every bucket row of the call in one upsert (in key order
//!   so concurrent charges cannot deadlock), runs the same arithmetic as the
//!   directory backend ([`crate::limits::charge`]) and writes the result.
//!   Charges are serialized per bucket, so no update is lost. Buckets that
//!   are full again are deleted, a bounded batch at most once a minute per
//!   replica, since a missing bucket charges like a full one. Each replica
//!   charges at its own wall clock, so clock skew between replicas is the
//!   only error: over any window a bucket admits at most `rate_per_sec ×
//!   skew` more cost than one clock would allow.
//! - **Call audit.** One transaction per event, committed with
//!   `synchronous_commit` on before the call answers. Events of one trace
//!   are numbered under a per-trace advisory lock, so they commit in order
//!   and reads page by that number without skipping.
//! - **Deadlines.** Waits for a connection, statements and lock waits are
//!   bounded by the constants below, and callers stop waiting after
//!   [`super::STORE_DEADLINE`], so a stuck store fails calls closed with a
//!   retry hint instead of holding them.
//! - **Schema.** Created on the first connection of each process by
//!   versioned, additive migrations in one transaction under an advisory
//!   lock, so replicas starting together migrate once. An older release
//!   ignores versions it does not know.
//! - **Connections.** A small pool, connected lazily. A connection that
//!   breaks is dropped and the next use reconnects, so the store recovers
//!   without a restart; while it cannot connect every charge and audit write
//!   fails, and callers fail closed.
//!
//! Every method blocks; async callers run them on the blocking pool.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ::postgres::types::Type;
use ::postgres::{Client, Error};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::{
    Bucket, ImportError, OpenError, Probe, ProbeCache, Snapshot, StateBackend, Store, StoreError,
};
use crate::audit::{CallEvent, CallPage, MAX_PAGE};
use crate::limits::{FileBucket, FileStore};
use crate::mitigations::{Commit, MitigationError, State};
use crate::policy::{PolicyError, PolicyFile};
use crate::registry::{AuditRecord, Registry, RegistryError};

/// Connections per replica.
const CONNECTIONS: usize = 8;
/// How long a caller waits for a free connection before failing closed.
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(1);
/// Used when the URL sets no `connect_timeout`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Per statement on the server (`statement_timeout`), and per lock wait
/// (`lock_timeout`), such as a bucket row another replica holds.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_TIMEOUT: Duration = Duration::from_secs(1);
/// A connection idle this long is checked, within [`VALID_TIMEOUT`],
/// before use, so one the server closed meanwhile is replaced instead of
/// failing a call.
const IDLE_CHECK: Duration = Duration::from_secs(1);
const VALID_TIMEOUT: Duration = Duration::from_secs(1);
/// A connection whose work took longer is not reused.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(4);
/// TCP keepalives, so a read on a dead network path fails after about
/// idle + interval × retries (11 s) instead of hanging; callers stop
/// waiting earlier ([`super::STORE_DEADLINE`]).
const KEEPALIVE_IDLE: Duration = Duration::from_secs(5);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
const KEEPALIVE_RETRIES: u32 = 3;
/// Buckets are swept at most this often per process, and at most
/// [`SWEEP_LIMIT`] per sweep.
const SWEEP_EVERY: Duration = Duration::from_secs(60);
const SWEEP_LIMIT: u64 = 500;
/// Bounds of a schema migration: the wait for another replica's migration
/// (and any other lock), and each statement.
const MIGRATION_LOCK_TIMEOUT: Duration = Duration::from_secs(3);
const MIGRATION_STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Serializes schema migrations across replicas (`SEKISHO` in ASCII).
const MIGRATION_LOCK: i64 = 0x0053_454b_4953_484f;

/// Schema versions, applied in order. Additive only: replicas of an older
/// release keep running against a newer schema during a rolling deploy.
const MIGRATIONS: &[(i32, &str)] = &[
    (
        1,
        "CREATE TABLE sekisho_rate_buckets (
        key text PRIMARY KEY,
        tokens double precision NOT NULL,
        at_ms bigint NOT NULL,
        full_at_ms bigint NOT NULL
    );
    CREATE INDEX sekisho_rate_buckets_full_at ON sekisho_rate_buckets (full_at_ms);
    CREATE TABLE sekisho_call_audit (
        trace_id text NOT NULL,
        seq bigint NOT NULL,
        tenant text,
        event jsonb NOT NULL,
        PRIMARY KEY (trace_id, seq)
    );",
    ),
    (
        2,
        "CREATE TABLE sekisho_state (
        name text PRIMARY KEY,
        body jsonb NOT NULL
    );
    CREATE TABLE sekisho_registry_audit (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        record jsonb NOT NULL
    );
    CREATE TABLE sekisho_mitigation_audit (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        event jsonb NOT NULL
    );",
    ),
];

pub struct Postgres {
    pool: Pool,
    probed: ProbeCache,
    /// Wall time of the last bucket sweep; 0 sweeps on the first charge.
    swept_ms: AtomicU64,
}

impl Postgres {
    /// Read the connection URL from `url_file`. Connects nothing yet.
    pub(crate) fn open(url_file: &Path) -> Result<Self, OpenError> {
        let url = std::fs::read_to_string(url_file).map_err(|source| OpenError::UrlFile {
            path: url_file.into(),
            source,
        })?;
        let mut pg: ::postgres::Config = url.trim().parse().map_err(OpenError::Url)?;
        if pg.get_connect_timeout().is_none() {
            pg.connect_timeout(CONNECT_TIMEOUT);
        }
        pg.keepalives(true)
            .keepalives_idle(KEEPALIVE_IDLE)
            .keepalives_interval(KEEPALIVE_INTERVAL)
            .keepalives_retries(KEEPALIVE_RETRIES);
        // Unacknowledged writes give up too (Linux only).
        #[cfg(target_os = "linux")]
        pg.tcp_user_timeout(KEEPALIVE_IDLE);
        Ok(Self {
            pool: Pool::new(pg, tls()?),
            probed: ProbeCache::default(),
            swept_ms: AtomicU64::new(0),
        })
    }

    fn run<T>(
        &self,
        store: Store,
        work: impl FnOnce(&mut Client) -> Result<T, Error>,
    ) -> Result<T, StoreError> {
        self.pool.with(store, work)
    }

    /// At most once per [`SWEEP_EVERY`] per process, after a charge:
    /// reclaim buckets that are full again ([`sweep_buckets`]), in a
    /// transaction of its own under the usual timeouts. A failure is logged
    /// and never fails the call.
    fn sweep_if_due(&self, now_ms: u64) {
        let last = self.swept_ms.load(Ordering::Relaxed);
        let due = now_ms.saturating_sub(last) >= SWEEP_EVERY.as_millis() as u64;
        if !due
            || self
                .swept_ms
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        if let Err(error) = self.run(Store::Limits, |client| {
            sweep_buckets(client, to_i64(now_ms))
        }) {
            eprintln!("sweeping full rate-limit buckets failed: {error}");
        }
    }

    /// Every store on one pooled connection, each write path in a
    /// transaction that is rolled back: a bucket row is inserted and locked
    /// as a charge would; an audit event is appended to a probe trace of its
    /// own; the registry and mitigation documents are locked, parsed and
    /// rewritten unchanged, with a probe audit record; the policy is read
    /// and validated. Nothing is committed.
    fn probe_now(&self) -> Probe {
        let checked = self.pool.with(Store::Limits, |client| {
            let mut stores = Vec::new();
            // Reported only when known: "uninitialized" means exactly that.
            if let Some(initialized) = probe_initialized(client) {
                stores.push((Store::State, initialized));
            }
            stores.extend([
                (Store::Policy, probe_document(client, Store::Policy)),
                (Store::Registry, probe_document(client, Store::Registry)),
                (
                    Store::Mitigations,
                    probe_document(client, Store::Mitigations),
                ),
                (Store::Audit, probe_audit(client).map_err(|e| detail(&e))),
                (Store::Limits, probe_limits(client).map_err(|e| detail(&e))),
            ]);
            Ok(stores)
        });
        match checked {
            Ok(stores) => stores,
            Err(error) => [
                Store::Policy,
                Store::Registry,
                Store::Mitigations,
                Store::Audit,
                Store::Limits,
            ]
            .into_iter()
            .map(|store| {
                // The connection failed, not one store.
                let reason = match &error {
                    StoreError::Postgres { source, .. } => detail(source),
                    other => other.to_string(),
                };
                (store, Err(reason))
            })
            .collect(),
        }
    }
}

/// The document a store keeps in `sekisho_state`, and its audit table.
fn document(store: Store) -> (&'static str, Option<(&'static str, &'static str)>) {
    match store {
        Store::Registry => ("registry", Some(("sekisho_registry_audit", "record"))),
        Store::Mitigations => ("mitigations", Some(("sekisho_mitigation_audit", "event"))),
        _ => ("policy", None),
    }
}

/// Whether `state import` or `state init` initialized the store; `None`
/// when that cannot be read (the other stores report why).
fn probe_initialized(client: &mut Client) -> Option<Result<(), String>> {
    match read_unlocked(client, "initialized") {
        Ok(Some(_)) => Some(Ok(())),
        Ok(None) => Some(Err(StoreError::Uninitialized {
            store: Store::State,
        }
        .to_string())),
        Err(_) => None,
    }
}

/// A document store's write path, rolled back: see [`Postgres::probe_now`].
fn probe_document(client: &mut Client, store: Store) -> Result<(), String> {
    let (name, audit) = document(store);
    let mut tx = client.transaction().map_err(|e| detail(&e))?;
    let body = read_document(&mut tx, name, audit.is_some()).map_err(|e| detail(&e))?;
    let parsed = match (store, body) {
        (_, None) => Err(StoreError::Uninitialized { store }.to_string()),
        (Store::Policy, Some(body)) => serde_json::from_value::<PolicyFile>(body)
            .map_err(|e| e.to_string())
            .and_then(|policy| policy.validate().map_err(|e| e.to_string()))
            .map(|()| None),
        (Store::Registry, Some(body)) => serde_json::from_value::<Registry>(body)
            .map(|registry| Some(serde_json::json!(registry)))
            .map_err(|e| e.to_string()),
        (_, Some(body)) => serde_json::from_value::<State>(body)
            .map_err(|e| e.to_string())
            .and_then(|state| state.validate().map(|()| Some(serde_json::json!(state)))),
    }?;
    if let (Some(body), Some((table, column))) = (parsed, audit) {
        write_document(&mut tx, name, &body).map_err(|e| detail(&e))?;
        tx.query_typed(
            &format!("INSERT INTO {table} ({column}) VALUES ('{{}}')"),
            &[],
        )
        .map_err(|e| detail(&e))?;
    }
    tx.rollback().map_err(|e| detail(&e))
}

/// The stored document `name`, locked for this transaction when `lock`.
fn read_document(
    tx: &mut ::postgres::Transaction<'_>,
    name: &str,
    lock: bool,
) -> Result<Option<serde_json::Value>, Error> {
    let sql = if lock {
        "SELECT body FROM sekisho_state WHERE name = $1 FOR UPDATE"
    } else {
        "SELECT body FROM sekisho_state WHERE name = $1"
    };
    tx.query_typed_opt(sql, &[(&name, Type::TEXT)])?
        .map(|row| row.try_get(0))
        .transpose()
}

/// Replace the stored document `name`; `false` when there is none, that is
/// the store is not initialized.
fn write_document(
    tx: &mut ::postgres::Transaction<'_>,
    name: &str,
    body: &serde_json::Value,
) -> Result<bool, Error> {
    let rows = tx.query_typed(
        "UPDATE sekisho_state SET body = $2 WHERE name = $1 RETURNING name",
        &[(&name, Type::TEXT), (body, Type::JSONB)],
    )?;
    Ok(!rows.is_empty())
}

/// Insert the document `name`; fails when it exists.
fn insert_document(
    tx: &mut ::postgres::Transaction<'_>,
    name: &str,
    body: &serde_json::Value,
) -> Result<(), Error> {
    tx.query_typed(
        "INSERT INTO sekisho_state (name, body) VALUES ($1, $2)",
        &[(&name, Type::TEXT), (body, Type::JSONB)],
    )?;
    Ok(())
}

/// Insert `rows` into the audit table of a document store.
fn append_audit(
    tx: &mut ::postgres::Transaction<'_>,
    store: Store,
    rows: &[serde_json::Value],
) -> Result<(), Error> {
    if let (_, Some((table, column))) = document(store) {
        tx.query_typed(
            &format!("INSERT INTO {table} ({column}) SELECT unnest($1::jsonb[])"),
            &[(&rows, Type::JSONB_ARRAY)],
        )?;
    }
    Ok(())
}

/// A document read without a lock, in a transaction of its own.
fn read_unlocked(client: &mut Client, name: &str) -> Result<Option<serde_json::Value>, Error> {
    let mut tx = client.transaction()?;
    let body = read_document(&mut tx, name, false)?;
    tx.commit()?;
    Ok(body)
}

fn probe_limits(client: &mut Client) -> Result<(), Error> {
    let mut tx = client.transaction()?;
    let key = format!("probe:{}", uuid::Uuid::new_v4().simple());
    lock_buckets(&mut tx, &[key.as_str()], &[1.0], 0)?;
    tx.rollback()
}

/// Lock the rows of `keys` for this transaction and return them, creating
/// missing ones full (`bursts`) at `now`. One statement in key order: an
/// existing row is locked by a no-op update, so neither a concurrent charge
/// nor a sweep ([`sweep_buckets`]) can change or delete it until commit, and
/// two first charges cannot both start full.
fn lock_buckets(
    tx: &mut ::postgres::Transaction<'_>,
    keys: &[&str],
    bursts: &[f64],
    now: i64,
) -> Result<Vec<::postgres::Row>, Error> {
    tx.query_typed(
        "INSERT INTO sekisho_rate_buckets AS bucket (key, tokens, at_ms, full_at_ms)
         SELECT key, tokens, $3, $3 FROM unnest($1::text[], $2::float8[]) AS fresh(key, tokens)
         ON CONFLICT (key) DO UPDATE SET tokens = bucket.tokens
         RETURNING key, tokens, at_ms, full_at_ms",
        &[
            (&keys, Type::TEXT_ARRAY),
            (&bursts, Type::FLOAT8_ARRAY),
            (&now, Type::INT8),
        ],
    )
}

/// Delete up to [`SWEEP_LIMIT`] buckets that are full again at `now`. A
/// full bucket and a missing one charge the same (a missing one is created
/// full), so this changes no outcome; rows a charge holds are skipped.
fn sweep_buckets(client: &mut Client, now: i64) -> Result<u64, Error> {
    let limit = to_i64(SWEEP_LIMIT);
    let rows = client.query_typed(
        "DELETE FROM sekisho_rate_buckets WHERE key IN (
             SELECT key FROM sekisho_rate_buckets WHERE full_at_ms <= $1
             ORDER BY full_at_ms LIMIT $2 FOR UPDATE SKIP LOCKED
         ) AND full_at_ms <= $1
         RETURNING key",
        &[(&now, Type::INT8), (&limit, Type::INT8)],
    )?;
    Ok(rows.len() as u64)
}

fn probe_audit(client: &mut Client) -> Result<(), Error> {
    let mut tx = client.transaction()?;
    let trace = format!("probe:{}", uuid::Uuid::new_v4().simple());
    append_event(&mut tx, &trace, None, &serde_json::json!({}))?;
    tx.rollback()
}

/// Append `event` to `trace` as its next `seq`, under the trace's advisory
/// lock, which `tx` holds until it ends.
fn append_event(
    tx: &mut ::postgres::Transaction<'_>,
    trace: &str,
    tenant: Option<&str>,
    event: &serde_json::Value,
) -> Result<(), Error> {
    tx.query_typed(
        "SELECT pg_advisory_xact_lock(hashtextextended('sekisho_call_audit:' || $1, 0))",
        &[(&trace, Type::TEXT)],
    )?;
    // A new statement, so its snapshot includes the trace's last commit.
    tx.query_typed(
        "INSERT INTO sekisho_call_audit (trace_id, seq, tenant, event)
         SELECT $1, coalesce(max(seq), 0) + 1, $2, $3 FROM sekisho_call_audit WHERE trace_id = $1",
        &[
            (&trace, Type::TEXT),
            (&tenant, Type::TEXT),
            (event, Type::JSONB),
        ],
    )?;
    Ok(())
}

impl StateBackend for Postgres {
    fn kind(&self) -> &'static str {
        "postgres"
    }

    fn take(
        &self,
        buckets: &[Bucket<'_>],
        cost: f64,
        now_ms: u64,
    ) -> Result<Result<(), Duration>, StoreError> {
        let mut sorted: Vec<&Bucket<'_>> = buckets.iter().collect();
        sorted.sort_by_key(|bucket| bucket.key);
        let keys: Vec<&str> = sorted.iter().map(|bucket| bucket.key).collect();
        let bursts: Vec<f64> = sorted.iter().map(|bucket| bucket.rate.burst).collect();
        let now = to_i64(now_ms);
        let charged = self.run(Store::Limits, |client| {
            let mut tx = client.transaction()?;
            let rows = lock_buckets(&mut tx, &keys, &bursts, now)?;
            let mut store = FileStore::default();
            for row in rows {
                store.buckets.insert(
                    row.try_get(0)?,
                    FileBucket {
                        tokens: row.try_get(1)?,
                        at_ms: to_u64(row.try_get(2)?),
                        full_at_ms: to_u64(row.try_get(3)?),
                    },
                );
            }
            let charged = crate::limits::charge(&mut store, buckets, cost, now_ms);
            // A refusal changes nothing: refill is a function of time.
            if charged.is_ok() {
                let (mut keys, mut tokens, mut at, mut full_at) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for (key, bucket) in &store.buckets {
                    keys.push(key.as_str());
                    tokens.push(bucket.tokens);
                    at.push(to_i64(bucket.at_ms));
                    full_at.push(to_i64(bucket.full_at_ms));
                }
                tx.query_typed(
                    "UPDATE sekisho_rate_buckets AS bucket
                     SET tokens = new.tokens, at_ms = new.at_ms, full_at_ms = new.full_at_ms
                     FROM unnest($1::text[], $2::float8[], $3::int8[], $4::int8[])
                         AS new(key, tokens, at_ms, full_at_ms)
                     WHERE bucket.key = new.key",
                    &[
                        (&keys, Type::TEXT_ARRAY),
                        (&tokens, Type::FLOAT8_ARRAY),
                        (&at, Type::INT8_ARRAY),
                        (&full_at, Type::INT8_ARRAY),
                    ],
                )?;
            }
            tx.commit()?;
            Ok(charged)
        })?;
        self.sweep_if_due(now_ms);
        Ok(charged)
    }

    fn registry(&self) -> Result<Registry, RegistryError> {
        let body = self
            .run(Store::Registry, |client| read_unlocked(client, "registry"))?
            .ok_or(StoreError::Uninitialized {
                store: Store::Registry,
            })?;
        serde_json::from_value(body)
            .map_err(|error| RegistryError::Invalid(format!("stored registry: {error}")))
    }

    /// Locks the registry row; writes it only when `change` changed it.
    fn edit_registry(
        &self,
        change: &mut dyn FnMut(&mut Registry) -> Result<(), RegistryError>,
        audit: Option<&AuditRecord>,
    ) -> Result<Registry, RegistryError> {
        let invalid = |what: &str, error: serde_json::Error| {
            RegistryError::Invalid(format!("{what} registry: {error}"))
        };
        let record = audit
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| invalid("auditing a", error))?;
        self.run(Store::Registry, |client| {
            let mut tx = client.transaction()?;
            let Some(body) = read_document(&mut tx, "registry", true)? else {
                return Ok(Err(StoreError::Uninitialized {
                    store: Store::Registry,
                }
                .into()));
            };
            let mut registry: Registry = match serde_json::from_value(body) {
                Ok(registry) => registry,
                Err(error) => return Ok(Err(invalid("stored", error))),
            };
            let before = registry.clone();
            if let Err(error) = change(&mut registry) {
                return Ok(Err(error));
            }
            if registry != before {
                let body = match serde_json::to_value(&registry) {
                    Ok(body) => body,
                    Err(error) => return Ok(Err(invalid("writing the", error))),
                };
                write_document(&mut tx, "registry", &body)?;
            }
            if let Some(record) = &record {
                append_audit(&mut tx, Store::Registry, std::slice::from_ref(record))?;
            }
            tx.commit()?;
            Ok(Ok(registry))
        })?
    }

    fn policy(&self) -> Result<PolicyFile, PolicyError> {
        let body = self
            .run(Store::Policy, |client| read_unlocked(client, "policy"))?
            .ok_or(StoreError::Uninitialized {
                store: Store::Policy,
            })?;
        let policy: PolicyFile = serde_json::from_value(body)
            .map_err(|error| PolicyError::Invalid(error.to_string()))?;
        policy.validate()?;
        Ok(policy)
    }

    /// Refused until the store is initialized, so a policy alone never
    /// stands in for an import that brings the mitigations too.
    fn set_policy(&self, policy: &PolicyFile) -> Result<(), PolicyError> {
        policy.validate()?;
        let body = serde_json::to_value(policy)
            .map_err(|error| PolicyError::Invalid(error.to_string()))?;
        let written = self.run(Store::Policy, |client| {
            let mut tx = client.transaction()?;
            let written = write_document(&mut tx, "policy", &body)?;
            tx.commit()?;
            Ok(written)
        })?;
        match written {
            true => Ok(()),
            false => Err(StoreError::Uninitialized {
                store: Store::Policy,
            }
            .into()),
        }
    }

    /// One transaction that takes the trace's advisory lock and appends
    /// the event as the trace's next `seq`. The lock is held until commit,
    /// so a trace's events commit in `seq` order and a reader's cursor
    /// never passes an event still to come. The session's
    /// `synchronous_commit` is on (see [`Pool::connect`]), so the event is
    /// durable when this returns.
    fn record_call(&self, event: &CallEvent) -> Result<(), StoreError> {
        let json = serde_json::to_value(event).map_err(|error| StoreError::Io {
            store: Store::Audit,
            source: error.into(),
        })?;
        self.run(Store::Audit, |client| {
            let mut tx = client.transaction()?;
            append_event(&mut tx, &event.trace_id, event.tenant.as_deref(), &json)?;
            tx.commit()
        })
    }

    /// The cursor is the trace's last `seq` read.
    fn read_calls(
        &self,
        tenant: &str,
        trace_id: &str,
        cursor: u64,
        limit: usize,
    ) -> Result<CallPage, StoreError> {
        let limit = limit.clamp(1, MAX_PAGE);
        let rows = self.run(Store::Audit, |client| {
            client.query_typed(
                "SELECT seq, event FROM sekisho_call_audit
                 WHERE trace_id = $1 AND tenant = $2 AND seq > $3 ORDER BY seq LIMIT $4",
                &[
                    (&trace_id, Type::TEXT),
                    (&tenant, Type::TEXT),
                    (&to_i64(cursor), Type::INT8),
                    (&to_i64(limit as u64), Type::INT8),
                ],
            )
        })?;
        let complete = rows.len() < limit;
        let mut next_cursor = cursor;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let read = |source| StoreError::Postgres {
                store: Store::Audit,
                source,
            };
            next_cursor = to_u64(row.try_get(0).map_err(read)?);
            let event: serde_json::Value = row.try_get(1).map_err(read)?;
            // Like the directory log, an event that does not parse is skipped.
            if let Ok(event) = serde_json::from_value(event) {
                events.push(event);
            }
        }
        Ok(CallPage {
            events,
            next_cursor,
            complete,
        })
    }

    fn mitigations(&self) -> Result<State, MitigationError> {
        let body = self
            .run(Store::Mitigations, |client| {
                read_unlocked(client, "mitigations")
            })?
            .ok_or(StoreError::Uninitialized {
                store: Store::Mitigations,
            })?;
        let state: State = serde_json::from_value(body)
            .map_err(|error| MitigationError::Invalid(format!("stored mitigations: {error}")))?;
        state.validate().map_err(MitigationError::Invalid)?;
        Ok(state)
    }

    /// Locks the mitigation row. Changes bump the generation and commit
    /// with their audit events; a refusal commits only its event.
    fn edit_mitigations(
        &self,
        change: &mut dyn FnMut(&mut State) -> Result<Commit, MitigationError>,
    ) -> Result<(), MitigationError> {
        let invalid = |error: serde_json::Error| MitigationError::Invalid(error.to_string());
        self.run(Store::Mitigations, |client| {
            let mut tx = client.transaction()?;
            let Some(body) = read_document(&mut tx, "mitigations", true)? else {
                return Ok(Err(StoreError::Uninitialized {
                    store: Store::Mitigations,
                }
                .into()));
            };
            let mut state: State = match serde_json::from_value(body) {
                Ok(state) => state,
                Err(error) => return Ok(Err(invalid(error))),
            };
            if let Err(reason) = state.validate() {
                return Ok(Err(MitigationError::Invalid(reason)));
            }
            let events = match change(&mut state) {
                Err(error) => return Ok(Err(error)),
                Ok(Commit::Refusal(event)) => vec![*event],
                Ok(Commit::Changes(events)) if events.is_empty() => return Ok(Ok(())),
                Ok(Commit::Changes(events)) => {
                    state.generation += 1;
                    match serde_json::to_value(&state) {
                        // The row is locked above, so it exists.
                        Ok(body) => _ = write_document(&mut tx, "mitigations", &body)?,
                        Err(error) => return Ok(Err(invalid(error))),
                    }
                    events
                }
            };
            let rows: Result<Vec<_>, _> = events.iter().map(serde_json::to_value).collect();
            match rows {
                Ok(rows) => append_audit(&mut tx, Store::Mitigations, &rows)?,
                Err(error) => return Ok(Err(invalid(error))),
            }
            tx.commit()?;
            Ok(Ok(()))
        })?
    }

    /// One transaction writes the policy, registry and mitigation
    /// documents, their audit records and the `initialized` marker, all
    /// inserted: a store initialized before or meanwhile fails the insert,
    /// so the import commits whole, once, or not at all.
    fn import(&self, source: &Snapshot, operator: &str, now_ms: u64) -> Result<(), ImportError> {
        let invalid = |error: serde_json::Error| {
            ImportError::Mitigations(MitigationError::Invalid(error.to_string()))
        };
        let (state, record, events) = super::plan_import(source, operator, now_ms);
        let policy = serde_json::to_value(&source.policy).map_err(invalid)?;
        let registry = serde_json::to_value(&source.registry).map_err(invalid)?;
        let state = serde_json::to_value(&state).map_err(invalid)?;
        let record = serde_json::to_value(&record).map_err(invalid)?;
        let events: Vec<_> = events
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(invalid)?;
        let marker = serde_json::json!({
            "operator": operator,
            "at_unix_ms": now_ms,
            "agents": source.policy.agents.len(),
            "tools": source.registry.tools.len(),
            "mitigations": source.mitigations.mitigations.len(),
        });
        self.run(Store::State, |client| {
            let mut tx = client.transaction()?;
            if read_document(&mut tx, "initialized", false)?.is_some() {
                return Ok(Err(ImportError::AlreadyInitialized));
            }
            insert_document(&mut tx, "policy", &policy)?;
            insert_document(&mut tx, "registry", &registry)?;
            append_audit(&mut tx, Store::Registry, std::slice::from_ref(&record))?;
            insert_document(&mut tx, "mitigations", &state)?;
            append_audit(&mut tx, Store::Mitigations, &events)?;
            insert_document(&mut tx, "initialized", &marker)?;
            tx.commit()?;
            Ok(Ok(()))
        })?
    }

    /// Every store over the pool's own connection path
    /// ([`Self::probe_now`]). Results are reused for [`super::PROBE_TTL`].
    fn probe(&self) -> Probe {
        self.probed.get(|| self.probe_now())
    }
}

/// An error with its cause: alone, a database error displays as `db error`
/// and a connection error as `error connecting to server`.
pub(crate) fn detail(error: &Error) -> String {
    match (error.as_db_error(), std::error::Error::source(error)) {
        (Some(db), _) => format!("{} ({})", db.message(), db.code().code()),
        (None, Some(cause)) => format!("{error}: {cause}"),
        (None, None) => error.to_string(),
    }
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Rustls with the platform's roots (which honor `SSL_CERT_FILE` and
/// `SSL_CERT_DIR`). `sslmode` in the URL decides whether TLS is used; when
/// it is, the server certificate and host name are always verified.
fn tls() -> Result<MakeRustlsConnect, OpenError> {
    let native = rustls_native_certs::load_native_certs();
    for error in &native.errors {
        eprintln!("postgres TLS: loading a system root failed: {error}");
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(native.certs);
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(OpenError::Tls)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    // PostgreSQL 17+ requires it for `sslnegotiation=direct`.
    config.alpn_protocols = vec![b"postgresql".to_vec()];
    Ok(MakeRustlsConnect::new(config))
}

struct Connection {
    client: Client,
    used: Instant,
}

/// At most [`CONNECTIONS`] lazily opened connections. A caller takes an
/// idle one or opens a new one while under the limit, else waits up to
/// [`CHECKOUT_TIMEOUT`] and fails. A connection that broke, or whose work
/// overran [`OPERATION_TIMEOUT`], is dropped instead of returned.
struct Pool {
    config: ::postgres::Config,
    tls: MakeRustlsConnect,
    slots: Mutex<Slots>,
    freed: Condvar,
    /// Set once this process has migrated the schema.
    migrated: AtomicBool,
}

#[derive(Default)]
struct Slots {
    idle: Vec<Connection>,
    /// Connections idle, in use or being opened.
    open: usize,
}

/// A checked-out place in the pool: on drop it returns `connection`, or
/// frees the place when there is none to return.
struct Lease<'a> {
    pool: &'a Pool,
    connection: Option<Connection>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut slots = self.pool.lock();
        match self.connection.take() {
            Some(connection) => slots.idle.push(connection),
            None => slots.open -= 1,
        }
        self.pool.freed.notify_one();
    }
}

impl Pool {
    fn new(config: ::postgres::Config, tls: MakeRustlsConnect) -> Self {
        Self {
            config,
            tls,
            slots: Mutex::default(),
            freed: Condvar::new(),
            migrated: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn with<T>(
        &self,
        store: Store,
        work: impl FnOnce(&mut Client) -> Result<T, Error>,
    ) -> Result<T, StoreError> {
        let failed = |source| StoreError::Postgres { store, source };
        let mut lease = self.checkout().ok_or(StoreError::Timeout { store })?;
        let started = Instant::now();
        let reusable = lease.connection.take().and_then(|mut connection| {
            let alive = !connection.client.is_closed()
                && (connection.used.elapsed() < IDLE_CHECK
                    || connection.client.is_valid(VALID_TIMEOUT).is_ok());
            alive.then_some(connection)
        });
        let mut connection = match reusable {
            Some(connection) => connection,
            None => Connection {
                client: self.connect().map_err(failed)?,
                used: Instant::now(),
            },
        };
        let result = work(&mut connection.client).map_err(failed);
        // A connection that overran may sit on a dead network path; a
        // fresh one replaces it.
        if !connection.client.is_closed() && started.elapsed() < OPERATION_TIMEOUT {
            connection.used = Instant::now();
            lease.connection = Some(connection);
        }
        result
    }

    /// A lease with an idle connection, or an empty one the caller fills.
    /// `None` when every connection stayed busy for [`CHECKOUT_TIMEOUT`].
    fn checkout(&self) -> Option<Lease<'_>> {
        let deadline = Instant::now() + CHECKOUT_TIMEOUT;
        let mut slots = self.lock();
        loop {
            if let Some(connection) = slots.idle.pop() {
                return Some(Lease {
                    pool: self,
                    connection: Some(connection),
                });
            }
            if slots.open < CONNECTIONS {
                slots.open += 1;
                return Some(Lease {
                    pool: self,
                    connection: None,
                });
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            slots = self
                .freed
                .wait_timeout(slots, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// A new session whose commits are durable before they return
    /// (`synchronous_commit` raised to `on` when the server default is
    /// `off`), with the schema migrated if this process has not yet, and
    /// statement and lock waits bounded.
    fn connect(&self) -> Result<Client, Error> {
        let mut client = self.config.connect(self.tls.clone())?;
        client.batch_execute(
            "SELECT set_config('synchronous_commit', 'on', false)
             WHERE current_setting('synchronous_commit') = 'off'",
        )?;
        // With its own, longer bounds: a replica may wait for another's
        // migration.
        if !self.migrated.load(Ordering::Acquire) {
            migrate(&mut client)?;
            self.migrated.store(true, Ordering::Release);
        }
        client.batch_execute(&format!(
            "SET statement_timeout = {}; SET lock_timeout = {}",
            STATEMENT_TIMEOUT.as_millis(),
            LOCK_TIMEOUT.as_millis()
        ))?;
        Ok(client)
    }
}

impl Drop for Pool {
    /// A blocking client cannot close on an async worker, where the last
    /// handle to the backend may drop, so connections close on a thread of
    /// their own.
    fn drop(&mut self) {
        let connections = std::mem::take(
            &mut self
                .slots
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .idle,
        );
        if !connections.is_empty() {
            std::thread::spawn(move || drop(connections));
        }
    }
}

/// Apply every migration newer than the database's version, in one
/// transaction under an advisory lock, so replicas starting at once
/// serialize and each version is applied exactly once. Waits are bounded by
/// [`MIGRATION_LOCK_TIMEOUT`] and [`MIGRATION_STATEMENT_TIMEOUT`].
fn migrate(client: &mut Client) -> Result<(), Error> {
    let mut tx = client.transaction()?;
    // Bounded like every other wait; a migration that cannot finish fails
    // the connection, which is dropped, and the next use tries again.
    tx.batch_execute(&format!(
        "SET LOCAL lock_timeout = {}; SET LOCAL statement_timeout = {}",
        MIGRATION_LOCK_TIMEOUT.as_millis(),
        MIGRATION_STATEMENT_TIMEOUT.as_millis()
    ))?;
    tx.query_typed(
        "SELECT pg_advisory_xact_lock($1)",
        &[(&MIGRATION_LOCK, Type::INT8)],
    )?;
    tx.batch_execute(
        "CREATE TABLE IF NOT EXISTS sekisho_schema_migrations (
            version integer PRIMARY KEY,
            applied_at timestamptz NOT NULL DEFAULT now()
        )",
    )?;
    let current: i32 = tx
        .query_one(
            "SELECT coalesce(max(version), 0) FROM sekisho_schema_migrations",
            &[],
        )?
        .try_get(0)?;
    for (version, sql) in MIGRATIONS.iter().filter(|(version, _)| *version > current) {
        tx.batch_execute(sql)?;
        tx.query_typed(
            "INSERT INTO sekisho_schema_migrations (version) VALUES ($1)",
            &[(version, Type::INT4)],
        )?;
    }
    tx.commit()
}
