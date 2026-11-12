//! Shared rate-limit file, durable audit, kill-replica retries, and readiness
//! that follows the state backend, a configured issuer, and a control plane.

mod support;

use std::time::Duration;

use sekisho::audit::{CallEvent, Decision};
use support::stack::{
    Stack, echo_call, eventually, readiness_settles, readyz, serve_origin, serve_origin_at,
};
use tokio_util::sync::CancellationToken;

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

#[tokio::test(flavor = "multi_thread")]
async fn a_down_token_issuer_fails_readiness_until_it_answers() {
    let stack = Stack::start_with(Some(AGENTS), "\n[cluster]\nstate_dir = \"cluster\"\n").await;
    let body = readiness_settles(&stack.address, 200).await;
    assert_eq!(body["exchange"], "ok", "{body}");
    assert!(body.get("control_plane").is_none(), "{body}");

    stack.stop_issuer().await;
    let body = readiness_settles(&stack.address, 503).await;
    assert_eq!(body["exchange"], "unavailable", "{body}");
    assert!(!body.to_string().contains(&stack.issuer_address), "{body}");

    stack.resume_issuer().await;
    let body = readiness_settles(&stack.address, 200).await;
    assert_eq!(body["exchange"], "ok", "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_down_control_plane_fails_readiness_until_it_answers() {
    let plane_stop = CancellationToken::new();
    let plane = serve_origin(plane_stop.clone()).await;
    let extra = format!(
        "\n[cluster]\nstate_dir = \"cluster\"\n[mitigations.control_plane]\n\
         endpoint = \"http://{plane}\"\ntoken_file = \"plane-token\"\nnamespace = \"acme\"\n"
    );
    let stack = Stack::start_with(Some(AGENTS), &extra).await;
    let body = readiness_settles(&stack.address, 200).await;
    assert_eq!(body["control_plane"], "ok", "{body}");
    assert_eq!(body["exchange"], "ok", "{body}");

    plane_stop.cancel();
    let gone = eventually(Duration::from_secs(2), async || {
        tokio::net::TcpStream::connect(&plane).await.is_err()
    })
    .await;
    assert!(gone, "control plane still listening on {plane}");
    let body = readiness_settles(&stack.address, 503).await;
    assert_eq!(body["control_plane"], "unavailable", "{body}");
    assert_eq!(body["exchange"], "ok", "{body}");
    assert!(!body.to_string().contains(&plane), "{body}");

    let resume = CancellationToken::new();
    serve_origin_at(&plane, resume.clone()).await;
    let body = readiness_settles(&stack.address, 200).await;
    assert_eq!(body["control_plane"], "ok", "{body}");
    resume.cancel();
}
