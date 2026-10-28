//! Kill-host drill (#48): two `sekisho` processes, each with its own
//! working directory and no shared local file, on one PostgreSQL database,
//! behind a client that retries on the survivor. Under steady load one
//! process is killed with SIGKILL.
//!
//! Asserts: in-flight calls on the killed replica fail with a connection
//! error, never a wrong answer; retries succeed on the survivor; admitted
//! calls stay within the rate-limit bound; every call acknowledged to the
//! client has its `started` and outcome audit events in PostgreSQL; the
//! killed replica rejoins after a restart. Prints the interruption window
//! (kill to first successful retry).
//!
//! Needs the `postgres` feature and `SEKISHO_TEST_POSTGRES_URL`; skipped
//! otherwise:
//!
//! ```sh
//! SEKISHO_TEST_POSTGRES_URL=postgres://user@127.0.0.1:5432/postgres?sslmode=disable \
//!   cargo test --features postgres --test kill_drill -- --nocapture
//! ```
#![cfg(feature = "postgres")]

mod support;

use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderName, HeaderValue};
use rmcp::ServiceError;
use sekisho::audit::Decision;
use sekisho::config::{Config, StateConfig, VerifierConfig};
use support::postgres::TestDb;
use support::stack::{Stack, echo_call, eventually, readyz};

const AGENTS: &str = "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\"]\n";
/// The agent bucket every drill call is charged to.
const RATE_PER_SEC: f64 = 20.0;
const BURST: f64 = 10.0;
const WORKERS: usize = 4;
/// Load before and after the kill.
const BEFORE_KILL: Duration = Duration::from_millis(1_000);
const AFTER_KILL: Duration = Duration::from_millis(2_000);
/// A call is retried on the survivor until this deadline.
const RETRY_DEADLINE: Duration = Duration::from_secs(10);

/// One `sekisho serve` process in a directory of its own.
struct Replica {
    dir: tempfile::TempDir,
    child: Child,
    address: String,
}

impl Replica {
    /// Start `sekisho serve` with a copy of `config` whose every local file
    /// lives in a new directory.
    fn start(config: &Config) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("sekisho-drill-{}-", std::process::id()))
            .tempdir()
            .unwrap();
        let path = own_config(config, dir.path());
        Self::spawn(dir, path)
    }

    fn spawn(dir: tempfile::TempDir, config: PathBuf) -> Self {
        let (child, address) = serve(&config, dir.path());
        Self {
            dir,
            child,
            address,
        }
    }

    /// SIGKILL, as a host loss does.
    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    /// Start again in the same directory with the same config.
    fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        (self.child, self.address) = serve(&self.dir.path().join("sekisho.toml"), self.dir.path());
    }
}

/// `sekisho serve` with `config`, its stderr in `dir`; the process and the
/// address it printed.
fn serve(config: &Path, dir: &Path) -> (Child, String) {
    let log = std::fs::File::create(dir.join("stderr.log")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_sekisho"))
        .arg("--config")
        .arg(config)
        .arg("serve")
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
        .to_string();
    (child, address)
}

impl Drop for Replica {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `config` with every file it names copied into or placed under `dir`,
/// written as `dir/sekisho.toml`.
fn own_config(config: &Config, dir: &Path) -> PathBuf {
    let copy = |from: &Path, name: &str| {
        let to = dir.join(name);
        std::fs::copy(from, &to).unwrap();
        to
    };
    let mut own = config.clone();
    own.listen = "127.0.0.1:0".parse().unwrap();
    if let VerifierConfig::Jwks { jwks_file, .. } = &mut own.verifier {
        *jwks_file = copy(jwks_file, "jwks.json");
    }
    if let Some(exchange) = &mut own.exchange {
        exchange.client_secret_file = copy(&exchange.client_secret_file, "client-secret");
    }
    if let StateConfig::Postgres { url_file } = &mut own.state {
        *url_file = copy(url_file, "postgres-url");
    }
    own.registry.file = dir.join("registry.json");
    own.registry.audit_file = dir.join("registry-audit.jsonl");
    own.policy.file = dir.join("agents.toml");
    own.audit.calls_file = dir.join("calls.jsonl");
    own.mitigations.file = Some(dir.join("mitigations.json"));
    own.mitigations.audit_file = Some(dir.join("mitigations-audit.jsonl"));
    own.cluster.state_dir = None;
    let path = dir.join("sekisho.toml");
    std::fs::write(&path, toml::to_string(&own).unwrap()).unwrap();
    path
}

/// How one call ended for the client.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ending {
    /// Answered with the right result: acknowledged.
    Answered,
    /// Refused by a rate limit.
    Throttled,
}

/// What the load recorded.
#[derive(Default)]
struct Record {
    /// Trace ids of acknowledged calls.
    acknowledged: Vec<String>,
    throttled: usize,
    /// Connection errors from the killed replica.
    connection_errors: usize,
    /// When a call that failed on the killed replica first succeeded on
    /// the survivor.
    first_retry_ok: Option<Instant>,
}

/// One call to `address` under its own trace: `Err` is a connection error
/// (the client never got an answer); any other answer is checked here.
async fn call(stack: &Stack, address: &str, trace: &str, text: &str) -> Result<Ending, String> {
    let traceparent = format!("00-{trace}-00f067aa0ba902b7-01");
    let headers = [(
        HeaderName::from_static("traceparent"),
        HeaderValue::from_str(&traceparent).unwrap(),
    )]
    .into_iter()
    .collect();
    let extra = serde_json::json!({ "act": "agent-7", "tenant_id": "t1" });
    let client = tokio::time::timeout(
        Duration::from_secs(5),
        stack.try_client_on_with(address, extra, headers),
    )
    .await
    .map_err(|_| "connect timed out".to_string())??;
    let answer = client.call_tool(echo_call(text)).await;
    let _ = client.cancel().await;
    match answer {
        Ok(result) => {
            let echoed = result.content[0].as_text().unwrap().text.clone();
            assert_eq!(echoed, format!("echo {text}"), "a wrong answer");
            Ok(Ending::Answered)
        }
        Err(ServiceError::McpError(error)) if error.message.contains("rate limited") => {
            Ok(Ending::Throttled)
        }
        Err(ServiceError::McpError(error)) => panic!("unexpected refusal: {error:?}"),
        Err(error) => Err(error.to_string()),
    }
}

/// Paced calls until `stop`, alternating replicas; a call whose
/// replica gives no answer is retried on the other one.
async fn worker(
    stack: Arc<Stack>,
    addresses: Arc<Mutex<[String; 2]>>,
    worker: usize,
    killed_at: Arc<Mutex<Option<Instant>>>,
    stop: Instant,
    record: Arc<Mutex<Record>>,
) {
    // Steady load at twice the rate limit across all workers, so some
    // calls are throttled without flooding the host.
    let mut pace = tokio::time::interval(Duration::from_secs_f64(
        WORKERS as f64 / (2.0 * RATE_PER_SEC),
    ));
    pace.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n = 0usize;
    while Instant::now() < stop {
        pace.tick().await;
        n += 1;
        let trace = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
        let text = format!("w{worker}-{n}");
        let first = (worker + n) % 2;
        let mut target = first;
        let mut failed = false;
        let deadline = Instant::now() + RETRY_DEADLINE;
        let ending = loop {
            let address = addresses.lock().unwrap()[target].clone();
            // The whole attempt (connect, call, cleanup) ends by the
            // deadline; a replica that accepts but never answers fails the
            // drill instead of hanging it.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let attempt = tokio::time::timeout(remaining, call(&stack, &address, &trace, &text))
                .await
                .unwrap_or_else(|_| panic!("no answer within {RETRY_DEADLINE:?} from {address}"));
            match attempt {
                Ok(ending) => break ending,
                Err(error) => {
                    assert!(Instant::now() < deadline, "no replica answered: {error}");
                    assert!(
                        killed_at.lock().unwrap().is_some(),
                        "a connection error before the kill: {error}"
                    );
                    record.lock().unwrap().connection_errors += 1;
                    failed = true;
                    target = 1 - target;
                }
            }
        };
        let mut record = record.lock().unwrap();
        match ending {
            Ending::Answered => {
                record.acknowledged.push(trace);
                if failed && record.first_retry_ok.is_none() {
                    record.first_retry_ok = Some(Instant::now());
                }
            }
            Ending::Throttled => record.throttled += 1,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn killing_one_of_two_replicas_loses_no_acknowledged_call() {
    let Some(db) = TestDb::create() else {
        return;
    };
    let limits = format!("agent = {{ rate_per_sec = {RATE_PER_SEC}, burst = {BURST} }}\n");
    let stack = Arc::new(
        Stack::start_with(
            Some(AGENTS),
            &format!(
                "\n[state]\nbackend = \"postgres\"\nurl_file = \"{}\"\n[limits]\n{limits}",
                db.url_file.display()
            ),
        )
        .await,
    );
    stack.enable_echo().await;
    let mut a = Replica::start(&stack.config);
    let b = Replica::start(&stack.config);
    for replica in [&a, &b] {
        let address = replica.address.clone();
        assert!(
            eventually(Duration::from_secs(10), async || readyz(&address).await.0
                == 200)
            .await,
            "replica not ready; see {}",
            replica.dir.path().display()
        );
    }
    // Both serve the tool before load starts.
    for replica in [&a, &b] {
        let address = replica.address.clone();
        let warm = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
        assert!(
            eventually(Duration::from_secs(5), async || call(
                &stack, &address, &warm, "warm"
            )
            .await
                == Ok(Ending::Answered))
            .await
        );
    }
    // Admissions are counted where they land: calls that reached the
    // upstream, including the one killed in flight. The bucket holds at
    // most BURST tokens when load starts, whatever the warm-up spent.
    let upstream_before = stack.seen.lock().unwrap().len();
    let load_started = Instant::now();

    let addresses = Arc::new(Mutex::new([a.address.clone(), b.address.clone()]));
    let killed_at = Arc::new(Mutex::new(None));
    let record = Arc::new(Mutex::new(Record::default()));
    let stop = Instant::now() + BEFORE_KILL + AFTER_KILL;
    let workers: Vec<_> = (0..WORKERS)
        .map(|n| {
            tokio::spawn(worker(
                Arc::clone(&stack),
                Arc::clone(&addresses),
                n,
                Arc::clone(&killed_at),
                stop,
                Arc::clone(&record),
            ))
        })
        .collect();

    // A call held in flight on A when it dies.
    let (held_stack, held_address) = (Arc::clone(&stack), a.address.clone());
    let held = tokio::spawn(async move {
        let trace = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
        call(&held_stack, &held_address, &trace, "pause").await
    });
    let paused = stack.paused.clone();
    assert!(
        eventually(Duration::from_secs(5), async || paused
            .load(std::sync::atomic::Ordering::SeqCst))
        .await
    );
    tokio::time::sleep(BEFORE_KILL.saturating_sub(load_started.elapsed())).await;
    *killed_at.lock().unwrap() = Some(Instant::now());
    a.kill();
    let kill = killed_at.lock().unwrap().unwrap();

    let in_flight = held.await.unwrap();
    assert!(
        in_flight.is_err(),
        "the in-flight call failed with a connection error: {in_flight:?}"
    );
    stack.resume.notify_one();
    for worker in workers {
        worker.await.unwrap();
    }
    // Every admission of the load happened between its first request and
    // the last response.
    let elapsed = load_started.elapsed();
    let admitted = stack.seen.lock().unwrap().len() - upstream_before;

    let record = std::mem::take(&mut *record.lock().unwrap());
    let window = record
        .first_retry_ok
        .map(|at| at.saturating_duration_since(kill))
        .expect("a call failed on the killed replica and succeeded on the survivor");
    // One clock (one host), so the limiter's error is zero: at most BURST
    // plus the refill over the load, and one call of slack for rounding.
    // The load asks for twice the rate, so admissions run close to this
    // bound; a limiter that lost its state on failover would admit up to
    // another BURST (10) and fail it.
    let bound = BURST + RATE_PER_SEC * elapsed.as_secs_f64() + 1.0;
    println!(
        "drill: {} acknowledged, {} throttled, {} connection errors over {:.2} s; \
         {admitted} reached the upstream, bound {bound:.1}; interruption window {} ms",
        record.acknowledged.len(),
        record.throttled,
        record.connection_errors,
        elapsed.as_secs_f64(),
        window.as_millis()
    );
    assert!(
        record.connection_errors > 0,
        "load reached the killed replica"
    );
    assert!(record.throttled > 0, "the load exceeded the rate limit");
    assert!(admitted as f64 <= bound, "{admitted} admitted > {bound}");

    // Every acknowledged call has `started` and its outcome under one call
    // id, read from PostgreSQL by a fresh backend.
    let store = sekisho::state::open(&stack.config).unwrap();
    let acknowledged = record.acknowledged.clone();
    let missing = tokio::task::spawn_blocking(move || {
        acknowledged
            .iter()
            .filter(|trace| {
                let events = store.read_calls("t1", trace, 0, 100).unwrap().events;
                !events.iter().any(|outcome| {
                    outcome.decision == Decision::Allowed
                        && events.iter().any(|started| {
                            started.decision == Decision::Started
                                && started.call_id == outcome.call_id
                        })
                })
            })
            .cloned()
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert!(
        missing.is_empty(),
        "acknowledged without audit: {missing:?}"
    );
    println!(
        "drill: audit complete for all {} acknowledged calls",
        record.acknowledged.len()
    );

    // The killed replica rejoins.
    a.restart();
    let address = a.address.clone();
    assert!(
        eventually(Duration::from_secs(10), async || readyz(&address).await.0
            == 200)
        .await,
        "restarted replica not ready"
    );
    let trace = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
    assert!(
        eventually(Duration::from_secs(5), async || call(
            &stack, &address, &trace, "back"
        )
        .await
            == Ok(Ending::Answered))
        .await
    );
    // Replicas and the in-process gateway stop before the database goes.
    drop(a);
    drop(b);
    drop(stack);
    drop(db);
}
