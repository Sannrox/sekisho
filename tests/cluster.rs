//! Shared rate-limit file, durable audit, kill-replica retries, and readiness
//! that follows the state backend.

mod support;

use std::time::Duration;

use sekisho::audit::{CallEvent, Decision};
use support::stack::{Stack, echo_call, eventually};

const AGENTS: &str = "[[agents]]\nid = \"agent-7\"\ntools = [\"tools.echo\"]\n";
const CLUSTER: &str = "\n[cluster]\nstate_dir = \"cluster\"\n[limits]\nagent = { rate_per_sec = 0.001, burst = 2.0 }\n";

#[tokio::test(flavor = "multi_thread")]
async fn kill_replica_retries_on_the_survivor_and_keeps_audit() {
    let stack = Stack::start_with(Some(AGENTS), CLUSTER).await;
    stack.enable_echo().await;
    let a = stack.extra_gateway().await;
    let b = stack.extra_gateway().await;

    let client_a = stack
        .client_on(&a.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    client_a.call_tool(echo_call("1")).await.unwrap();
    let _ = client_a.cancel().await;
    a.kill();

    let client_b = stack
        .client_on(&b.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    client_b.call_tool(echo_call("2")).await.unwrap();
    let _ = client_b.cancel().await;

    let events: Vec<CallEvent> = std::fs::read_to_string(&stack.config.audit.calls_file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .filter(|event| event.decision == Decision::Allowed)
            .count()
            >= 2,
        "{events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_burst_holds_across_replicas() {
    let stack = Stack::start_with(Some(AGENTS), CLUSTER).await;
    stack.enable_echo().await;
    let a = stack.extra_gateway().await;
    let b = stack.extra_gateway().await;

    let client_a = stack
        .client_on(&a.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    let client_b = stack
        .client_on(&b.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    client_a.call_tool(echo_call("1")).await.unwrap();
    client_a.call_tool(echo_call("2")).await.unwrap();
    let error = client_b.call_tool(echo_call("3")).await.unwrap_err();
    let text = format!("{error:?}");
    assert!(text.contains("rate limited"), "{text}");
    let _ = client_a.cancel().await;
    let _ = client_b.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn in_flight_on_a_killed_replica_fails_and_retry_succeeds() {
    let stack = Stack::start_with(Some(AGENTS), CLUSTER).await;
    stack.enable_echo().await;
    let a = stack.extra_gateway().await;
    let b = stack.extra_gateway().await;
    let addr_a = a.address.clone();

    let client_a = stack
        .client_on(&addr_a, serde_json::json!({ "act": "agent-7" }))
        .await;
    let inflight = tokio::spawn(async move { client_a.call_tool(echo_call("hold")).await });
    let entered = stack.hold_entered.clone();
    assert!(
        eventually(Duration::from_secs(2), async || {
            entered.load(std::sync::atomic::Ordering::SeqCst)
        })
        .await
    );
    a.kill();
    let failed = inflight.await.expect("join");
    assert!(failed.is_err(), "{failed:?}");
    let client_b = stack
        .client_on(&b.address, serde_json::json!({ "act": "agent-7" }))
        .await;
    client_b.call_tool(echo_call("retry")).await.unwrap();
    let _ = client_b.cancel().await;
}

async fn readyz(address: &str) -> (u16, serde_json::Value) {
    let response = reqwest::get(format!("http://{address}/readyz"))
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_store_fails_readiness_and_calls_closed_until_it_recovers() {
    // Default limits, so warm-up calls cannot exhaust a bucket.
    let stack = Stack::start_with(Some(AGENTS), "\n[cluster]\nstate_dir = \"cluster\"\n").await;
    stack.enable_echo().await;
    let client = stack.client(serde_json::json!({ "act": "agent-7" })).await;
    assert!(
        eventually(Duration::from_secs(5), async || client
            .call_tool(echo_call("warm"))
            .await
            .is_ok())
        .await
    );
    let (status, body) = readyz(&stack.address).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["backend"], "directory");
    assert_eq!(body["stores"]["limits"], "ok");

    let limits = stack
        .config
        .cluster
        .state_dir
        .as_ref()
        .unwrap()
        .join("limits.json");
    let saved = std::fs::read(&limits).unwrap();
    std::fs::write(&limits, "not-json").unwrap();
    assert_eq!(
        readiness_settles(&stack.address, 503).await["stores"]["limits"],
        "unavailable"
    );
    let error = client.call_tool(echo_call("1")).await.unwrap_err();
    assert!(format!("{error:?}").contains("retry after"), "{error:?}");

    std::fs::write(&limits, saved).unwrap();
    readiness_settles(&stack.address, 200).await;
    client.call_tool(echo_call("2")).await.unwrap();
    let _ = client.cancel().await;
}

/// Poll `/readyz` past the probe cache until it answers `status`.
async fn readiness_settles(address: &str, status: u16) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    let settled = eventually(Duration::from_secs(5), async || {
        let (got, body) = readyz(address).await;
        last = body;
        got == status
    })
    .await;
    assert!(settled, "readyz never answered {status}: {last}");
    last
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_state_dir_fails_readiness_while_files_stay_readable() {
    // Root ignores directory permissions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let stack = Stack::start_with(Some(AGENTS), "\n[cluster]\nstate_dir = \"cluster\"\n").await;
    stack.enable_echo().await;
    let state_dir = stack.config.cluster.state_dir.clone().unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    readiness_settles(&stack.address, 200).await;

    use std::os::unix::fs::PermissionsExt as _;
    let mode = |m| std::fs::Permissions::from_mode(m);
    std::fs::set_permissions(&state_dir, mode(0o555)).unwrap();
    let body = readiness_settles(&stack.address, 503).await;
    std::fs::set_permissions(&state_dir, mode(0o755)).unwrap();
    assert_eq!(body["stores"]["limits"], "unavailable", "{body}");
    assert_eq!(body["stores"]["mitigations"], "unavailable", "{body}");
    assert_eq!(body["stores"]["policy"], "ok", "{body}");
    assert!(!body.to_string().contains("cluster"), "no paths: {body}");
    readiness_settles(&stack.address, 200).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unusable_lock_or_audit_log_fails_readiness_for_its_store() {
    // Root ignores file permissions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let stack = Stack::start_with(Some(AGENTS), "\n[cluster]\nstate_dir = \"cluster\"\n").await;
    stack.enable_echo().await;
    readiness_settles(&stack.address, 200).await;
    use std::os::unix::fs::PermissionsExt as _;
    let mitigation_lock = stack
        .config
        .mitigations
        .file
        .as_ref()
        .unwrap()
        .with_extension("lock");
    std::fs::create_dir_all(mitigation_lock.parent().unwrap()).unwrap();
    std::fs::write(&mitigation_lock, "").unwrap();
    let cases = [
        // Written but unreadable: the writer reads the tail before appending.
        (stack.config.audit.calls_file.clone(), 0o200, "audit"),
        (
            stack.config.registry.file.with_extension("lock"),
            0o444,
            "registry",
        ),
        (mitigation_lock, 0o444, "mitigations"),
    ];
    for (path, mode, store) in cases {
        if !path.exists() {
            std::fs::write(&path, "").unwrap();
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let body = readiness_settles(&stack.address, 503).await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(body["stores"][store], "unavailable", "{body}");
        readiness_settles(&stack.address, 200).await;
    }
}
