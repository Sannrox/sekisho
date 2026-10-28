//! The same rate-limit and call-audit contract on every state backend, and
//! a PostgreSQL store outage.
//!
//! PostgreSQL cases build with the `postgres` feature and run when
//! `SEKISHO_TEST_POSTGRES_URL` names a server (plain `postgres://` URL of a
//! user that may create databases); otherwise they are skipped. Each case
//! creates its own database and drops it afterwards, so cases and agents
//! can share one server.

mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::{HeaderName, HeaderValue};
use sekisho::audit::Decision;
use sekisho::limits::{Caller, Limiter};
use support::stack::{Stack, echo_call, eventually};

const AGENTS: &str = "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\"]\n";
const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

enum Backend {
    Directory,
    #[cfg(feature = "postgres")]
    Postgres(Box<pg::TestDb>),
}

impl Backend {
    /// Config selecting this backend, with `limits` as the `[limits]` body.
    fn toml(&self, limits: &str) -> String {
        let backend = match self {
            Self::Directory => "[cluster]\nstate_dir = \"cluster\"\n".to_string(),
            #[cfg(feature = "postgres")]
            Self::Postgres(db) => format!(
                "[state]\nbackend = \"postgres\"\nurl_file = \"{}\"\n",
                db.url_file.display()
            ),
        };
        format!("\n{backend}[limits]\n{limits}")
    }

    /// Make every call-audit write fail, or (`broken = false`) work again.
    fn break_audit(&self, stack: &Stack, broken: bool) {
        match self {
            Self::Directory => {
                // The writer cannot append to a directory.
                let log = &stack.config.audit.calls_file;
                let saved = log.with_extension("saved");
                if broken {
                    std::fs::rename(log, &saved).unwrap();
                    std::fs::create_dir(log).unwrap();
                } else {
                    std::fs::remove_dir(log).unwrap();
                    std::fs::rename(&saved, log).unwrap();
                }
            }
            #[cfg(feature = "postgres")]
            Self::Postgres(db) => db.execute(if broken {
                "ALTER TABLE sekisho_call_audit ADD CONSTRAINT sekisho_test_refuse CHECK (false) NOT VALID"
            } else {
                "ALTER TABLE sekisho_call_audit DROP CONSTRAINT sekisho_test_refuse"
            }),
        }
    }
}

fn traced() -> HashMap<HeaderName, HeaderValue> {
    [(
        HeaderName::from_static("traceparent"),
        HeaderValue::from_static(TRACEPARENT),
    )]
    .into_iter()
    .collect()
}

/// The first call, retried until the registry refresh serves the tool.
async fn first_call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>,
) {
    assert!(
        eventually(Duration::from_secs(5), async || client
            .call_tool(echo_call("warm"))
            .await
            .is_ok())
        .await
    );
}

/// A fresh replica's view of the trace's audit events for tenant `t1`.
async fn recorded(stack: &Stack) -> Vec<sekisho::audit::CallEvent> {
    let store = sekisho::state::open(&stack.config).unwrap();
    tokio::task::spawn_blocking(move || store.read_calls("t1", TRACE, 0, 100))
        .await
        .unwrap()
        .unwrap()
        .events
}

/// Two limiters with their own backend instances on one store, charged
/// concurrently, admit exactly the burst.
async fn limiters_on_one_store_admit_exactly_the_burst(backend: Backend) {
    let stack = Stack::start_with(
        Some(AGENTS),
        &backend.toml("agent = { rate_per_sec = 0.001, burst = 5.0 }\n"),
    )
    .await;
    let config = stack.config.clone();
    let results = tokio::task::spawn_blocking(move || {
        let limiters: Vec<Arc<Limiter>> = (0..2)
            .map(|_| {
                let store = sekisho::state::open(&config).unwrap();
                Arc::new(Limiter::from_config(&config, &store))
            })
            .collect();
        let admit = |limiter: &Limiter| {
            let caller = Caller {
                agent: "agent-7",
                user: "user-1",
                tenant: Some("t1"),
            };
            limiter.admit(&caller, "tools.echo", Instant::now())
        };
        std::thread::scope(|scope| {
            let calls: Vec<_> = (0..16)
                .map(|n| {
                    let limiter = Arc::clone(&limiters[n % 2]);
                    scope.spawn(move || admit(&limiter))
                })
                .collect();
            calls
                .into_iter()
                .map(|call| call.join().unwrap())
                .collect::<Vec<_>>()
        })
    })
    .await
    .unwrap();
    let admitted = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(admitted, 5, "{results:?}");
    // Refusals are real throttling (a wait near 1000 s), not store errors.
    assert!(
        results
            .iter()
            .filter_map(|result| result.err())
            .all(|wait| wait > Duration::from_secs(100)),
        "{results:?}"
    );
}

/// When a call answers, its audit event is already readable by any replica.
async fn a_call_is_audited_before_it_answers(backend: Backend) {
    let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
    stack.enable_echo().await;
    let client = stack
        .client_with(
            serde_json::json!({ "act": "agent-7", "tenant_id": "t1" }),
            traced(),
        )
        .await;
    first_call(&client).await;
    let events = recorded(&stack).await;
    assert_eq!(
        events.last().map(|event| event.decision),
        Some(Decision::Allowed),
        "{events:?}"
    );
    assert!(events.iter().all(|event| event.tool == "tools.echo"));
    let _ = client.cancel().await;
}

/// A client for agent-7 of tenant `t1` in the test trace.
async fn traced_client(
    stack: &Stack,
) -> rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig> {
    stack
        .client_with(
            serde_json::json!({ "act": "agent-7", "tenant_id": "t1" }),
            traced(),
        )
        .await
}

/// A call whose `started` event cannot be written is refused with a retry
/// hint before it reaches the upstream, and calls answer again once the
/// audit store works.
async fn a_started_write_failure_refuses_before_the_upstream(backend: Backend) {
    let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
    stack.enable_echo().await;
    let client = traced_client(&stack).await;
    first_call(&client).await;
    let upstream_calls = stack.seen.lock().unwrap().len();
    backend.break_audit(&stack, true);
    let error = client.call_tool(echo_call("lost")).await.unwrap_err();
    let text = format!("{error:?}");
    assert!(
        text.contains("call audit is unavailable; retry after 1000 ms"),
        "{text}"
    );
    assert_eq!(
        stack.seen.lock().unwrap().len(),
        upstream_calls,
        "not forwarded"
    );
    backend.break_audit(&stack, false);
    client.call_tool(echo_call("again")).await.unwrap();
    let _ = client.cancel().await;
}

/// A call whose outcome cannot be written after the upstream ran fails
/// without a retry hint and names its call id, whose `started` event is on
/// record; the upstream ran once.
async fn an_outcome_write_failure_fails_without_a_retry_hint(backend: Backend) {
    let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
    stack.enable_echo().await;
    let client = traced_client(&stack).await;
    first_call(&client).await;
    let upstream_calls = stack.seen.lock().unwrap().len();
    let paused = tokio::spawn(async move {
        let result = client.call_tool(echo_call("pause")).await;
        let _ = client.cancel().await;
        result
    });
    let entered = stack.paused.clone();
    assert!(
        eventually(Duration::from_secs(5), async || entered
            .load(std::sync::atomic::Ordering::SeqCst))
        .await
    );
    // The `started` event is durable before the upstream runs.
    let started = recorded(&stack).await;
    let started = started
        .iter()
        .rfind(|event| event.decision == Decision::Started)
        .and_then(|event| event.call_id.clone())
        .expect("a started event");
    backend.break_audit(&stack, true);
    stack.resume.notify_one();
    let error = paused.await.unwrap().unwrap_err();
    let text = format!("{error:?}");
    backend.break_audit(&stack, false);
    assert!(text.contains("outcome could not be audited"), "{text}");
    assert!(text.contains(&started), "names the call id: {text}");
    assert!(!text.contains("retry_after_ms"), "{text}");
    assert_eq!(stack.seen.lock().unwrap().len(), upstream_calls + 1);
}

/// A stored policy is replaced whole, and an invalid one is refused and
/// changes nothing.
async fn a_policy_import_replaces_the_policy_or_nothing(backend: Backend) {
    let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
    let store = sekisho::state::open(&stack.config).unwrap();
    let agents = |ids: &[&str]| sekisho::policy::PolicyFile {
        agents: ids
            .iter()
            .map(|id| sekisho::policy::AgentPolicy {
                id: (*id).into(),
                tools: vec!["tools.echo".into()],
                ceiling: Vec::new(),
                guardrails: None,
            })
            .collect(),
    };
    let (two, duplicate) = (agents(&["agent-7", "agent-8"]), agents(&["a", "a"]));
    let stored = tokio::task::spawn_blocking(move || {
        store.set_policy(&two).unwrap();
        let refused = store.set_policy(&duplicate);
        (refused, store.policy().unwrap())
    })
    .await
    .unwrap();
    assert!(stored.0.is_err(), "duplicate agents are refused");
    let ids: Vec<_> = stored
        .1
        .agents
        .iter()
        .map(|agent| agent.id.clone())
        .collect();
    assert_eq!(ids, ["agent-7", "agent-8"]);
}

/// Owner edits of different tools through two replicas at once both land.
async fn concurrent_registry_edits_from_two_replicas_both_land(backend: Backend) {
    let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
    stack.enable("tools.other").await;
    stack.owner_edit("tools.other", "disable", false).await;
    let config = stack.config.clone();
    let registry = tokio::task::spawn_blocking(move || {
        std::thread::scope(|scope| {
            for tool in ["tools.echo", "tools.other"] {
                let config = &config;
                scope.spawn(move || {
                    let store = sekisho::state::open(config).unwrap();
                    sekisho::registry::owner_edit(store.as_ref(), "owner-1", "enable", tool, |r| {
                        r.set_enabled(tool, true)
                    })
                    .unwrap();
                });
            }
        });
        sekisho::state::open(&config).unwrap().registry().unwrap()
    })
    .await
    .unwrap();
    assert!(registry.tools["tools.echo"].enabled && registry.tools["tools.other"].enabled);
}

/// A mitigation applied through one replica refuses the next call on
/// another, and lifting it lets the next call through.
async fn a_mitigation_through_one_replica_applies_on_another(backend: Backend) {
    use sekisho::mitigations::{Mitigation, MitigationStore, Scope, Standalone, Target, now_ms};
    let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
    stack.enable_echo().await;
    let other = stack.extra_gateway().await;
    let client = stack
        .client_on(&other.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    first_call(&client).await;
    let store = MitigationStore::new(sekisho::state::open(&stack.config).unwrap());
    let applied = store.clone();
    let id = tokio::task::spawn_blocking(move || {
        let mitigation = Mitigation {
            decision: "INC-1".into(),
            operator: "ops-1".into(),
            target: Target::Tool {
                tool: "tools.echo".into(),
            },
            scope: Scope::All,
            applied_at_unix_ms: 0,
            expires_at_unix_ms: None,
        };
        applied.apply(&Standalone, mitigation, now_ms()).unwrap().0
    })
    .await
    .unwrap();
    let error = client.call_tool(echo_call("refused")).await.unwrap_err();
    assert!(format!("{error:?}").contains("disabled"), "{error:?}");
    tokio::task::spawn_blocking(move || {
        store
            .lift(&Standalone, &id, "INC-1-close", "ops-1", now_ms())
            .unwrap()
    })
    .await
    .unwrap();
    client.call_tool(echo_call("again")).await.unwrap();
    let _ = client.cancel().await;
}

macro_rules! conformance {
    ($($case:ident),* $(,)?) => {
        mod directory {
            $(
                #[tokio::test(flavor = "multi_thread")]
                async fn $case() {
                    super::$case(super::Backend::Directory).await;
                }
            )*
        }

        #[cfg(feature = "postgres")]
        mod postgres {
            $(
                #[tokio::test(flavor = "multi_thread")]
                async fn $case() {
                    let Some(db) = super::pg::TestDb::create() else {
                        return;
                    };
                    super::$case(super::Backend::Postgres(Box::new(db))).await;
                }
            )*
        }
    };
}

conformance!(
    limiters_on_one_store_admit_exactly_the_burst,
    a_call_is_audited_before_it_answers,
    a_started_write_failure_refuses_before_the_upstream,
    an_outcome_write_failure_fails_without_a_retry_hint,
    a_policy_import_replaces_the_policy_or_nothing,
    concurrent_registry_edits_from_two_replicas_both_land,
    a_mitigation_through_one_replica_applies_on_another,
);

#[cfg(feature = "postgres")]
mod pg {
    use std::path::PathBuf;

    use super::*;
    use support::stack::{readiness_settles, readyz};

    /// A database of its own on the test server, dropped with this value.
    pub struct TestDb {
        admin: ::postgres::Config,
        pub name: String,
        pub url_file: PathBuf,
        _dir: tempfile::TempDir,
    }

    /// Run `work` on its own thread: the blocking client must not run on an
    /// async worker.
    fn blocking<T: Send>(work: impl FnOnce() -> T + Send) -> T {
        std::thread::scope(|scope| scope.spawn(work).join().unwrap())
    }

    fn run(config: &::postgres::Config, sql: &str) {
        blocking(|| {
            let mut client = config.connect(::postgres::NoTls).unwrap();
            client.batch_execute(sql).unwrap();
        });
    }

    impl TestDb {
        /// `None`, after saying so, when no test server is configured.
        pub fn create() -> Option<Self> {
            let Ok(url) = std::env::var("SEKISHO_TEST_POSTGRES_URL") else {
                eprintln!("skipped: SEKISHO_TEST_POSTGRES_URL is not set");
                return None;
            };
            let admin: ::postgres::Config = url.parse().unwrap();
            let name = format!(
                "sekisho_test_{}_{}",
                std::process::id(),
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            );
            run(&admin, &format!("CREATE DATABASE {name}"));
            let mut database = url::Url::parse(&url).unwrap();
            database.set_path(&format!("/{name}"));
            let dir = tempfile::tempdir().unwrap();
            let url_file = dir.path().join("postgres-url");
            std::fs::write(&url_file, database.as_str()).unwrap();
            Some(Self {
                admin,
                name,
                url_file,
                _dir: dir,
            })
        }

        /// Run `sql` in this database.
        pub fn execute(&self, sql: &str) {
            let mut config = self.admin.clone();
            config.dbname(&self.name);
            run(&config, sql);
        }

        /// Refuse new connections to this database and end the open ones,
        /// as a store outage does; `false` lets connections in again.
        pub fn outage(&self, down: bool) {
            let name = &self.name;
            run(
                &self.admin,
                &if down {
                    format!(
                        "ALTER DATABASE {name} ALLOW_CONNECTIONS false;
                         SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{name}';"
                    )
                } else {
                    format!("ALTER DATABASE {name} ALLOW_CONNECTIONS true")
                },
            );
        }
    }

    /// A transaction another session keeps open until [`Held::release`].
    pub struct Held {
        release: std::sync::mpsc::Sender<()>,
        session: std::thread::JoinHandle<()>,
    }

    impl Held {
        /// Commit the transaction.
        pub fn release(self) {
            self.release.send(()).unwrap();
            self.session.join().unwrap();
        }
    }

    impl TestDb {
        /// Run `sql` in a transaction of this database and keep it open,
        /// with its locks, until released.
        pub fn hold(&self, sql: &str) -> Held {
            let mut config = self.admin.clone();
            config.dbname(&self.name);
            let sql = format!("BEGIN; {sql}");
            let (ready, started) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel::<()>();
            let session = std::thread::spawn(move || {
                let mut client = config.connect(::postgres::NoTls).unwrap();
                client.batch_execute(&sql).unwrap();
                ready.send(()).unwrap();
                let _ = released.recv();
                client.batch_execute("COMMIT").unwrap();
            });
            started.recv().unwrap();
            Held { release, session }
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            run(
                &self.admin,
                &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name),
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_outage_fails_calls_closed_and_recovers_without_a_restart() {
        let Some(db) = TestDb::create() else {
            return;
        };
        let backend = Backend::Postgres(Box::new(db));
        let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
        let Backend::Postgres(db) = &backend else {
            unreachable!()
        };
        stack.enable_echo().await;
        let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
        first_call(&client).await;
        let body = readiness_settles(&stack.address, 200).await;
        assert_eq!(body["backend"], "postgres");
        assert_eq!(body["stores"]["limits"], "ok", "{body}");
        assert_eq!(body["stores"]["audit"], "ok", "{body}");

        db.outage(true);
        let error = client.call_tool(echo_call("down")).await.unwrap_err();
        assert!(format!("{error:?}").contains("retry after"), "{error:?}");
        let body = readiness_settles(&stack.address, 503).await;
        for store in ["limits", "audit", "policy", "registry", "mitigations"] {
            assert_eq!(body["stores"][store], "unavailable", "{body}");
        }
        assert!(!body.to_string().contains(&db.name), "no details: {body}");

        db.outage(false);
        readiness_settles(&stack.address, 200).await;
        client.call_tool(echo_call("back")).await.unwrap();
        let _ = client.cancel().await;
        assert_eq!(readyz(&stack.address).await.0, 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replicas_starting_together_migrate_the_schema_once() {
        let Some(db) = TestDb::create() else {
            return;
        };
        let backend = Backend::Postgres(Box::new(db));
        let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
        let config = stack.config.clone();
        let probes = tokio::task::spawn_blocking(move || {
            std::thread::scope(|scope| {
                let replicas: Vec<_> = (0..4)
                    .map(|_| {
                        let config = config.clone();
                        scope.spawn(move || sekisho::state::open(&config).unwrap().probe())
                    })
                    .collect();
                replicas
                    .into_iter()
                    .map(|replica| replica.join().unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .await
        .unwrap();
        for probe in probes {
            for (store, result) in probe {
                assert!(result.is_ok(), "{store}: {result:?}");
            }
        }
        let Backend::Postgres(db) = &backend else {
            unreachable!()
        };
        // A duplicate version would violate the primary key; check the set.
        db.execute(
            "DO $$ BEGIN
               IF (SELECT array_agg(version ORDER BY version) FROM sekisho_schema_migrations) <> ARRAY[1, 2]
               THEN RAISE EXCEPTION 'unexpected schema versions'; END IF;
             END $$",
        );
    }

    /// Lock waits are bounded: a call whose bucket row another session
    /// holds fails with the retry hint within the bound, readiness goes
    /// false while the audit table is locked, and the pool serves again
    /// once the locks are gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_held_lock_fails_calls_within_the_bound_and_the_pool_recovers() {
        let Some(db) = TestDb::create() else {
            return;
        };
        let backend = Backend::Postgres(Box::new(db));
        let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
        let Backend::Postgres(db) = &backend else {
            unreachable!()
        };
        stack.enable_echo().await;
        let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
        first_call(&client).await;
        readiness_settles(&stack.address, 200).await;

        let held = db.hold(
            "SELECT 1 FROM sekisho_rate_buckets WHERE key = 'agent:agent-7' FOR UPDATE;
             LOCK TABLE sekisho_call_audit IN ACCESS EXCLUSIVE MODE;",
        );
        let started = Instant::now();
        let error = client.call_tool(echo_call("blocked")).await.unwrap_err();
        let waited = started.elapsed();
        assert!(format!("{error:?}").contains("retry after"), "{error:?}");
        assert!(waited < sekisho::state::STORE_DEADLINE, "{waited:?}");
        let body = readiness_settles(&stack.address, 503).await;
        assert_eq!(body["stores"]["audit"], "unavailable", "{body}");

        held.release();
        readiness_settles(&stack.address, 200).await;
        for n in 0..10 {
            client.call_tool(echo_call(&n.to_string())).await.unwrap();
        }
        let _ = client.cancel().await;
    }

    /// An event that commits after a reader's page is still read from the
    /// page's cursor: the trace's next event waits for the one in flight.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_audit_reader_does_not_skip_an_event_committed_late() {
        let Some(db) = TestDb::create() else {
            return;
        };
        let backend = Backend::Postgres(Box::new(db));
        let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
        let Backend::Postgres(db) = &backend else {
            unreachable!()
        };
        let event = |tool: &str| sekisho::audit::CallEvent {
            at_unix_ms: 1,
            trace_id: TRACE.into(),
            call_id: None,
            subject: "user-1".into(),
            tenant: Some("t1".into()),
            agent: "agent-7".into(),
            tool: tool.into(),
            decision: Decision::Allowed,
            reason: None,
            guardrail: None,
            approval: None,
            mitigation: None,
            latency_ms: 1,
            response_bytes: None,
        };
        let store = sekisho::state::open(&stack.config).unwrap();
        // Connect and migrate before another session holds the trace.
        let _ = recorded(&stack).await;

        // A writer in flight: holds the trace and its first event, uncommitted.
        let first = serde_json::to_string(&event("first")).unwrap();
        let held = db.hold(&format!(
            "SELECT pg_advisory_xact_lock(hashtextextended('sekisho_call_audit:{TRACE}', 0));
             INSERT INTO sekisho_call_audit (trace_id, seq, tenant, event)
             VALUES ('{TRACE}', 1, 't1', '{first}'::jsonb);"
        ));
        let writer = Arc::clone(&store);
        let second = std::thread::spawn(move || writer.record_call(&event("second")));
        std::thread::sleep(Duration::from_millis(200));
        let reader = Arc::clone(&store);
        let page = tokio::task::spawn_blocking(move || reader.read_calls("t1", TRACE, 0, 10))
            .await
            .unwrap()
            .unwrap();
        assert!(page.events.is_empty(), "{:?}", page.events);

        held.release();
        second.join().unwrap().unwrap();
        let cursor = page.next_cursor;
        let page = tokio::task::spawn_blocking(move || store.read_calls("t1", TRACE, cursor, 10))
            .await
            .unwrap()
            .unwrap();
        let tools: Vec<&str> = page.events.iter().map(|e| e.tool.as_str()).collect();
        assert_eq!(tools, ["first", "second"]);
    }

    /// A replica whose migration waits on another's lock gives up within
    /// the bound, reports its stores unavailable, and migrates once the
    /// lock is gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_held_migration_lock_fails_migration_within_the_bound() {
        let Some(db) = TestDb::create() else {
            return;
        };
        let backend = Backend::Postgres(Box::new(db));
        let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
        let Backend::Postgres(db) = &backend else {
            unreachable!()
        };
        // The migration lock key, `SEKISHO` in ASCII.
        let held = db.hold("SELECT pg_advisory_xact_lock(x'0053454b4953484f'::bigint);");
        let store = sekisho::state::open(&stack.config).unwrap();
        let probing = Arc::clone(&store);
        let started = Instant::now();
        let probe = tokio::task::spawn_blocking(move || probing.probe())
            .await
            .unwrap();
        let waited = started.elapsed();
        assert!(waited < sekisho::state::STORE_DEADLINE, "{waited:?}");
        let unavailable: Vec<_> = probe
            .iter()
            .filter_map(|(store, result)| {
                result.as_ref().err().map(|reason| (*store, reason.clone()))
            })
            .collect();
        assert_eq!(unavailable.len(), 5, "every store: {unavailable:?}");
        assert!(
            unavailable
                .iter()
                .all(|(_, reason)| reason.contains("lock timeout")),
            "{unavailable:?}"
        );

        held.release();
        assert!(
            eventually(Duration::from_secs(5), async || {
                let store = Arc::clone(&store);
                tokio::task::spawn_blocking(move || store.probe())
                    .await
                    .unwrap()
                    .iter()
                    .all(|(_, result)| result.is_ok())
            })
            .await
        );
    }

    /// A charge sweeps buckets that are full again and keeps one still
    /// refilling.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_charge_sweeps_full_buckets_and_keeps_refilling_ones() {
        let Some(db) = TestDb::create() else {
            return;
        };
        let backend = Backend::Postgres(Box::new(db));
        let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
        let Backend::Postgres(db) = &backend else {
            unreachable!()
        };
        let config = stack.config.clone();
        let charge = move || {
            let config = config.clone();
            std::thread::spawn(move || {
                let store = sekisho::state::open(&config).unwrap();
                let caller = Caller {
                    agent: "agent-7",
                    user: "user-1",
                    tenant: None,
                };
                Limiter::from_config(&config, &store).admit(&caller, "tools.echo", Instant::now())
            })
            .join()
            .unwrap()
        };
        // Creates the schema; a new backend sweeps on its first charge.
        charge().unwrap();
        db.execute(
            "INSERT INTO sekisho_rate_buckets (key, tokens, at_ms, full_at_ms) VALUES
                 ('user:expired', 0, 1, 2),
                 ('user:refilling', 0.5, 1, 9223372036854775807)",
        );
        charge().unwrap();
        db.execute(
            "DO $$ BEGIN
               IF EXISTS (SELECT 1 FROM sekisho_rate_buckets WHERE key = 'user:expired')
                  OR NOT EXISTS (SELECT 1 FROM sekisho_rate_buckets WHERE key = 'user:refilling')
               THEN RAISE EXCEPTION 'unexpected sweep'; END IF;
             END $$",
        );
    }

    /// Acceptance for #47: an owner edit made through replica A is served by
    /// replica B, which re-lists upstreams only once a minute, within the
    /// registry reload bound.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_registry_edit_through_one_replica_reaches_another_within_the_bound() {
        let Some(db) = TestDb::create() else {
            return;
        };
        let backend = Backend::Postgres(Box::new(db));
        let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
        stack.enable_echo().await;
        let mut slow = stack.config.clone();
        slow.registry.refresh_interval_ms = 60_000;
        let b = stack.replica(&slow).await;
        let client = stack
            .client_on(&b.address, serde_json::json!({ "act": "agent-7" }))
            .await;
        first_call(&client).await;
        let bound = sekisho::gateway::REGISTRY_RELOAD + Duration::from_millis(1_500);

        stack.owner_edit("tools.echo", "disable", false).await;
        let started = Instant::now();
        assert!(
            eventually(bound, async || client
                .call_tool(echo_call("off"))
                .await
                .is_err())
            .await,
            "disabled on B within {bound:?}"
        );
        stack.owner_edit("tools.echo", "enable", true).await;
        assert!(
            eventually(bound, async || client
                .call_tool(echo_call("on"))
                .await
                .is_ok())
            .await
        );
        assert!(started.elapsed() < bound * 2);
        let _ = client.cancel().await;
    }

    /// A mitigation change and its audit event commit together: when the
    /// event cannot be written the change is not made.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_mitigation_change_commits_with_its_audit_event_or_not_at_all() {
        use sekisho::mitigations::{
            Mitigation, MitigationStore, Scope, Standalone, Target, now_ms,
        };
        let Some(db) = TestDb::create() else {
            return;
        };
        let backend = Backend::Postgres(Box::new(db));
        let stack = Stack::start_with(Some(AGENTS), &backend.toml("")).await;
        let Backend::Postgres(db) = &backend else {
            unreachable!()
        };
        let store = MitigationStore::new(sekisho::state::open(&stack.config).unwrap());
        let mitigation = Mitigation {
            decision: "INC-2".into(),
            operator: "ops-1".into(),
            target: Target::Upstream {
                upstream: "tools".into(),
            },
            scope: Scope::All,
            applied_at_unix_ms: 0,
            expires_at_unix_ms: None,
        };
        let apply = |store: MitigationStore, mitigation: Mitigation| {
            std::thread::spawn(move || store.apply(&Standalone, mitigation, now_ms()))
                .join()
                .unwrap()
        };
        let load = |store: MitigationStore| {
            std::thread::spawn(move || store.load().unwrap())
                .join()
                .unwrap()
        };
        let initial = load(store.clone());
        assert!(initial.mitigations.is_empty());
        db.execute(
            "ALTER TABLE sekisho_mitigation_audit ADD CONSTRAINT refuse CHECK (false) NOT VALID",
        );
        assert!(apply(store.clone(), mitigation.clone()).is_err());
        let state = load(store.clone());
        assert!(
            state.mitigations.is_empty() && state.generation == initial.generation,
            "{state:?}"
        );

        db.execute("ALTER TABLE sekisho_mitigation_audit DROP CONSTRAINT refuse");
        apply(store.clone(), mitigation).unwrap();
        assert_eq!(load(store).generation, initial.generation + 1);
        db.execute(
            "DO $$ BEGIN
               IF (SELECT count(*) FROM sekisho_mitigation_audit) <> 1
               THEN RAISE EXCEPTION 'expected one audit event'; END IF;
             END $$",
        );
    }

    /// `state import` copies a directory backend's policy, registry and
    /// mitigations in one transaction: a failure part way leaves the target
    /// untouched and the import can be retried; once done, it is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_directory_backend_imports_into_postgres_whole_or_not_at_all() {
        use sekisho::mitigations::{
            Mitigation, MitigationStore, Scope, Standalone, Target, now_ms,
        };
        let Some(db) = TestDb::create() else {
            return;
        };
        let source = Stack::start_with(Some(AGENTS), &Backend::Directory.toml("")).await;
        source.enable_echo().await;
        let mut target = source.config.clone();
        target.state = sekisho::config::StateConfig::Postgres {
            url_file: db.url_file.clone(),
        };
        let migrate = target.clone();
        let (from, to) = (source.config.clone(), target);
        let import = move || {
            let source = sekisho::state::open(&from).unwrap();
            let target = sekisho::state::open(&to).unwrap();
            let imported = sekisho::state::import(source.as_ref(), target.as_ref(), "ops-1");
            let policy = target.policy();
            (imported, target.registry(), policy)
        };
        let applied = MitigationStore::new(sekisho::state::open(&source.config).unwrap());
        tokio::task::spawn_blocking(move || {
            let mitigation = Mitigation {
                decision: "INC-3".into(),
                operator: "ops-1".into(),
                target: Target::Tool {
                    tool: "tools.other".into(),
                },
                scope: Scope::All,
                applied_at_unix_ms: 0,
                expires_at_unix_ms: None,
            };
            applied.apply(&Standalone, mitigation, now_ms()).unwrap();
        })
        .await
        .unwrap();
        // Migrate the target, then fail its last write: the mitigation audit.
        tokio::task::spawn_blocking(move || sekisho::state::open(&migrate).unwrap().probe())
            .await
            .unwrap();
        db.execute(
            "ALTER TABLE sekisho_mitigation_audit ADD CONSTRAINT refuse CHECK (false) NOT VALID",
        );
        let (failed, registry, policy) = tokio::task::spawn_blocking(import.clone()).await.unwrap();
        assert!(failed.is_err());
        assert!(registry.is_err(), "still uninitialized");
        assert!(policy.is_err(), "no policy stored");

        db.execute("ALTER TABLE sekisho_mitigation_audit DROP CONSTRAINT refuse");
        let (first, registry, policy) = tokio::task::spawn_blocking(import.clone()).await.unwrap();
        let first = first.unwrap();
        assert_eq!((first.agents, first.mitigations), (1, 1));
        assert!(registry.unwrap().tools["tools.echo"].enabled);
        assert_eq!(policy.unwrap().agents[0].id, "agent-7");
        let (second, _, _) = tokio::task::spawn_blocking(import).await.unwrap();
        assert!(second.is_err(), "a second import is refused");
    }

    /// An uninitialized store (a fresh database, or one a #46-era
    /// deployment used for limits and audit only) refuses every call with
    /// a retry hint naming the command, is not ready, and refuses a policy
    /// import; `state init` and a policy import make it serve.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_uninitialized_store_refuses_calls_until_initialized() {
        let Some(db) = TestDb::create() else {
            return;
        };
        let backend = Backend::Postgres(Box::new(db));
        // No policy file: the stack leaves the store uninitialized.
        let stack = Stack::start_with(None, &backend.toml("")).await;
        let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
        let error = client.call_tool(echo_call("early")).await.unwrap_err();
        let text = format!("{error:?}");
        assert!(
            text.contains("sekisho state init") && text.contains("retry after"),
            "{text}"
        );
        let body = readiness_settles(&stack.address, 503).await;
        assert_eq!(body["stores"]["state"], "uninitialized", "{body}");

        let store = sekisho::state::open(&stack.config).unwrap();
        let policy: sekisho::policy::PolicyFile = toml::from_str(AGENTS).unwrap();
        let refused = {
            let (store, policy) = (Arc::clone(&store), policy.clone());
            tokio::task::spawn_blocking(move || store.set_policy(&policy))
                .await
                .unwrap()
        };
        assert!(refused.is_err(), "a policy alone does not initialize");

        tokio::task::spawn_blocking(move || {
            sekisho::state::init(store.as_ref(), "ops-1").unwrap();
            store.set_policy(&policy).unwrap();
        })
        .await
        .unwrap();
        stack.enable_echo().await;
        let body = readiness_settles(&stack.address, 200).await;
        assert_eq!(body["stores"]["state"], "ok", "{body}");
        first_call(&client).await;
        let _ = client.cancel().await;
    }
}
