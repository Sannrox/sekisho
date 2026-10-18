mod support;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

use support::*;

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn get(address: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// No network beyond loopback: the JWKS is a local file.
#[test]
fn starts_offline_with_a_local_jwks_and_serves_health_and_metrics() {
    let fixture = Fixture::new();
    let config = fixture.jwks_config("127.0.0.1:0", &[("k1", &key(1))]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_sekisho"))
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _running = Running(child);
    let address = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("unexpected first line {line:?}"))
        .to_string();

    let health = get(&address, "/healthz");
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    let metrics = get(&address, "/metrics");
    assert!(metrics.contains("sekisho_build_info{"), "{metrics}");
    assert!(metrics.contains("verifier=\"jwks\""), "{metrics}");
}

#[test]
fn diagnostics_bundle_contains_no_secrets() {
    let fixture = Fixture::new();
    let secret = "dev-token-do-not-print-4242";
    fixture.write("token", secret);
    let config = fixture.write(
        "sekisho.toml",
        "listen = \"127.0.0.1:0\"\n[verifier]\nkind = \"static\"\ntoken_file = \"token\"\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_sekisho"))
        .arg("diagnostics")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(output.status.success());
    let bundle = String::from_utf8(output.stdout).unwrap();
    assert!(!bundle.contains(secret), "{bundle}");
    let bundle: serde_json::Value = serde_json::from_str(&bundle).unwrap();
    assert_eq!(bundle["verifier"]["kind"], "static");
    assert_eq!(bundle["verifier"]["status"], "loaded");

    let jwks_config = fixture.jwks_config("127.0.0.1:0", &[("k1", &key(1))]);
    let jwks_key = jwks(&[("k1", &key(1))]);
    let x: serde_json::Value = serde_json::from_str(&jwks_key).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sekisho"))
        .args(["diagnostics", "--config"])
        .arg(&jwks_config)
        .output()
        .unwrap();
    let bundle = String::from_utf8(output.stdout).unwrap();
    assert!(
        !bundle.contains(x["keys"][0]["x"].as_str().unwrap()),
        "{bundle}"
    );
    let bundle: serde_json::Value = serde_json::from_str(&bundle).unwrap();
    assert_eq!(bundle["verifier"]["key_ids"], serde_json::json!(["k1"]));
}

#[test]
fn diagnostics_never_echo_a_misconfigured_jwks_file() {
    let fixture = Fixture::new();
    let config = fixture.jwks_config("127.0.0.1:0", &[("k1", &key(1))]);
    fixture.write("jwks.json", "\"credential-in-the-wrong-file-9911\"");
    let output = Command::new(env!("CARGO_BIN_EXE_sekisho"))
        .args(["diagnostics", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    let bundle = String::from_utf8(output.stdout).unwrap();
    assert!(
        !bundle.contains("credential-in-the-wrong-file-9911"),
        "{bundle}"
    );
    assert!(bundle.contains("not a JWKS document"), "{bundle}");
}
