//! MCP endpoint conformance against the official Rust SDK client.

mod support;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::Arc;

use rmcp::model::{ClientConfig, ProtocolVersion};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt as _, ServiceExt as _};
use sekisho::config::Config;
use sekisho::server::{AppState, router};
use sekisho::verifier::Verifier;
use support::*;
use tokio_util::sync::CancellationToken;

struct Running {
    address: String,
    shutdown: CancellationToken,
    _fixture: Fixture,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn start() -> Running {
    let fixture = Fixture::new();
    let config = Config::load(&fixture.jwks_config("127.0.0.1:0", &[("k1", &key(1))])).unwrap();
    let state = Arc::new(AppState {
        verifier: Verifier::from_config(&config.verifier).unwrap(),
        metrics: Default::default(),
    });
    let shutdown = CancellationToken::new();
    let app = router(state, &config.mcp, shutdown.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let stop = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { stop.cancelled_owned().await })
            .await
            .unwrap();
    });
    Running {
        address,
        shutdown,
        _fixture: fixture,
    }
}

fn transport(address: &str) -> StreamableHttpClientTransport<reqwest::Client> {
    let bearer = token("k1", &key(1), claims(serde_json::json!({})));
    StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp"))
            .auth_header(bearer),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_client_speaks_2026_07_28_statelessly() {
    let running = start().await;
    let client = ClientConfig::default()
        .serve_with_lifecycle(
            transport(&running.address),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
    let info = client.peer_info().unwrap();
    assert_eq!(info.protocol_version, ProtocolVersion::V_2026_07_28);
    assert!(info.capabilities.tools.is_some());
    assert!(client.list_all_tools().await.unwrap().is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_client_falls_back_to_2025_11_25() {
    let running = start().await;
    let client = ClientConfig::default()
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
        .serve(transport(&running.address))
        .await
        .unwrap();
    let info = client.peer_info().unwrap();
    assert_eq!(info.protocol_version, ProtocolVersion::V_2025_11_25);
    assert_eq!(
        info.server_info.as_ref().map(|server| server.name.as_str()),
        Some("sekisho")
    );
    assert!(client.list_all_tools().await.unwrap().is_empty());
    client.cancel().await.unwrap();
}

/// Raw HTTP/1.1 request; returns the full response text. A write error is
/// ignored: the server may answer (413) and close before the body is sent.
fn http(address: &str, request: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    let _ = stream.write_all(request.as_bytes());
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

fn post_mcp(address: &str, extra_headers: &str, body: &str) -> String {
    http(
        address,
        &format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

const PING: &str = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

#[tokio::test(flavor = "multi_thread")]
async fn missing_or_invalid_tokens_get_401_with_discovery_metadata() {
    let running = start().await;
    let address = running.address.clone();
    let (missing, invalid, metadata) = tokio::task::spawn_blocking(move || {
        (
            post_mcp(&address, "", PING),
            post_mcp(&address, "Authorization: Bearer not-a-jwt\r\n", PING),
            http(
                &address,
                "GET /.well-known/oauth-protected-resource/mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            ),
        )
    })
    .await
    .unwrap();
    let challenge = "www-authenticate: Bearer resource_metadata=\"http://127.0.0.1/.well-known/oauth-protected-resource/mcp\"";
    for response in [&missing, &invalid] {
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        assert!(
            response.to_lowercase().contains(&challenge.to_lowercase()),
            "{response}"
        );
    }
    assert!(metadata.starts_with("HTTP/1.1 200"), "{metadata}");
    let body: serde_json::Value =
        serde_json::from_str(metadata.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["resource"], "http://127.0.0.1/mcp");
    assert_eq!(
        body["authorization_servers"],
        serde_json::json!(["https://issuer.example.test"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_and_cross_origin_requests_are_refused() {
    let running = start().await;
    let address = running.address.clone();
    let bearer = token("k1", &key(1), claims(serde_json::json!({})));
    let (oversized, cross_origin) = tokio::task::spawn_blocking(move || {
        let auth = format!("Authorization: Bearer {bearer}\r\n");
        let huge = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{"pad":"{}"}}}}"#,
            "x".repeat(2 * 1024 * 1024)
        );
        (
            post_mcp(&address, &auth, &huge),
            post_mcp(
                &address,
                &format!("{auth}Origin: https://attacker.example.test\r\n"),
                PING,
            ),
        )
    })
    .await
    .unwrap();
    assert!(
        oversized.starts_with("HTTP/1.1 413"),
        "{}",
        &oversized[..200.min(oversized.len())]
    );
    assert!(cross_origin.starts_with("HTTP/1.1 403"), "{cross_origin}");
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_browser_origins_pass_cors_preflight() {
    let fixture = Fixture::new();
    let path = fixture.jwks_config("127.0.0.1:0", &[("k1", &key(1))]);
    let text = std::fs::read_to_string(&path).unwrap()
        + "allowed_origins = [\"https://console.example.test:443\"]\n";
    std::fs::write(&path, text).unwrap();
    let config = Config::load(&path).unwrap();
    let state = Arc::new(AppState {
        verifier: Verifier::from_config(&config.verifier).unwrap(),
        metrics: Default::default(),
    });
    let shutdown = CancellationToken::new();
    let app = router(state, &config.mcp, shutdown.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let stop = shutdown.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { stop.cancelled_owned().await })
            .await;
    });
    let preflight = |origin: &'static str| {
        let address = address.clone();
        move || {
            http(
                &address,
                &format!(
                    "OPTIONS /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: {origin}\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: authorization, content-type, mcp-method, mcp-protocol-version\r\nConnection: close\r\n\r\n"
                ),
            )
        }
    };
    let allowed = tokio::task::spawn_blocking(preflight("https://console.example.test"))
        .await
        .unwrap()
        .to_lowercase();
    assert!(allowed.starts_with("http/1.1 200"), "{allowed}");
    assert!(
        allowed.contains("access-control-allow-origin: https://console.example.test"),
        "{allowed}"
    );
    assert!(allowed.contains("mcp-method"), "{allowed}");
    let metadata_address = address.clone();
    let metadata = tokio::task::spawn_blocking(move || {
        http(
            &metadata_address,
            "GET /.well-known/oauth-protected-resource/mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://console.example.test\r\nConnection: close\r\n\r\n",
        )
    })
    .await
    .unwrap()
    .to_lowercase();
    assert!(
        metadata.contains("access-control-allow-origin: https://console.example.test"),
        "{metadata}"
    );
    let other = tokio::task::spawn_blocking(preflight("https://attacker.example.test"))
        .await
        .unwrap()
        .to_lowercase();
    assert!(!other.contains("access-control-allow-origin"), "{other}");
    shutdown.cancel();
}
