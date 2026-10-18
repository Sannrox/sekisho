//! Test keys and tokens. Every fixture lives in a unique temp directory.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{Algorithm, EncodingKey, Header};

pub const ISSUER: &str = "https://issuer.example.test";
pub const AUDIENCE: &str = "sekisho";

pub fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

pub fn jwks(keys: &[(&str, &SigningKey)]) -> String {
    let keys: Vec<serde_json::Value> = keys
        .iter()
        .map(|(kid, key)| {
            serde_json::json!({
                "kty": "OKP",
                "crv": "Ed25519",
                "alg": "EdDSA",
                "kid": kid,
                "x": URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes()),
            })
        })
        .collect();
    serde_json::json!({ "keys": keys }).to_string()
}

pub fn token(kid: &str, key: &SigningKey, claims: serde_json::Value) -> String {
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(kid.into());
    // PKCS#8 v1 wrapping of an Ed25519 seed: fixed 16-byte prefix, then the seed.
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(&key.to_bytes());
    jsonwebtoken::encode(&header, &claims, &EncodingKey::from_ed_der(&der)).unwrap()
}

pub fn claims(overrides: serde_json::Value) -> serde_json::Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut claims = serde_json::json!({
        "iss": ISSUER, "aud": AUDIENCE, "sub": "user-1", "exp": now + 300,
    });
    for (name, value) in overrides.as_object().unwrap() {
        claims[name] = value.clone();
    }
    claims
}

pub struct Fixture {
    pub dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            dir: tempfile::Builder::new()
                .prefix(&format!("sekisho-{}-", std::process::id()))
                .tempdir()
                .unwrap(),
        }
    }

    pub fn write(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    /// `sekisho.toml` using a JWKS file with the given keys.
    pub fn jwks_config(&self, listen: &str, keys: &[(&str, &SigningKey)]) -> PathBuf {
        self.write("jwks.json", &jwks(keys));
        self.write(
            "sekisho.toml",
            &format!(
                "listen = \"{listen}\"\n\n[verifier]\nkind = \"jwks\"\nissuer = \"{ISSUER}\"\naudience = \"{AUDIENCE}\"\njwks_file = \"jwks.json\"\n"
            ),
        )
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}
