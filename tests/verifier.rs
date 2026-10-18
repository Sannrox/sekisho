mod support;

use sekisho::config::Config;
use sekisho::verifier::{Rejection, Verifier};
use support::*;

fn verifier(fixture: &Fixture) -> Verifier {
    let config = Config::load(&fixture.jwks_config("127.0.0.1:0", &[("k1", &key(1))])).unwrap();
    Verifier::from_config(&config.verifier).unwrap()
}

#[test]
fn accepts_a_signed_token_from_the_local_jwks() {
    let fixture = Fixture::new();
    let verifier = verifier(&fixture);
    let token = token(
        "k1",
        &key(1),
        claims(serde_json::json!({"act": {"sub": "agent-7"}})),
    );
    let principal = verifier.verify(&token).unwrap();
    assert_eq!(principal.subject, "user-1");
    assert_eq!(principal.actor, Some(serde_json::json!({"sub": "agent-7"})));
}

#[test]
fn refuses_wrong_issuer_audience_expiry_key_and_signature() {
    let fixture = Fixture::new();
    let verifier = verifier(&fixture);
    for overrides in [
        serde_json::json!({"iss": "https://other.example.test"}),
        serde_json::json!({"aud": "another-service"}),
        serde_json::json!({"exp": 1}),
        serde_json::json!({"nbf": 4_102_444_800_u64}),
    ] {
        let token = token("k1", &key(1), claims(overrides.clone()));
        assert!(
            matches!(verifier.verify(&token), Err(Rejection::Invalid(_))),
            "{overrides}"
        );
    }
    let unknown = token("k2", &key(1), claims(serde_json::json!({})));
    assert_eq!(verifier.verify(&unknown), Err(Rejection::UnknownKey));
    let forged = token("k1", &key(2), claims(serde_json::json!({})));
    assert!(matches!(
        verifier.verify(&forged),
        Err(Rejection::Invalid(_))
    ));
    assert_eq!(verifier.verify("not-a-jwt"), Err(Rejection::Malformed));
}

#[test]
fn refuses_shared_secret_algorithms() {
    let fixture = Fixture::new();
    let verifier = verifier(&fixture);
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some("k1".into());
    let hs = jsonwebtoken::encode(
        &header,
        &claims(serde_json::json!({})),
        &jsonwebtoken::EncodingKey::from_secret(b"guessable"),
    )
    .unwrap();
    assert_eq!(verifier.verify(&hs), Err(Rejection::Algorithm));
}

#[test]
fn static_token_is_development_only() {
    let fixture = Fixture::new();
    fixture.write("token", "dev-token-123\n");
    let remote = fixture.write(
        "remote.toml",
        &format!("listen = \"0.0.0.0:8080\"\n[verifier]\nkind = \"static\"\ntoken_file = \"token\"\n{MCP_SECTION}"),
    );
    assert!(
        Config::load(&remote)
            .unwrap_err()
            .to_string()
            .contains("loopback")
    );
    let local = fixture.write(
        "local.toml",
        &format!("listen = \"127.0.0.1:0\"\n[verifier]\nkind = \"static\"\ntoken_file = \"token\"\n{MCP_SECTION}"),
    );
    let verifier = Verifier::from_config(&Config::load(&local).unwrap().verifier).unwrap();
    assert!(verifier.verify("dev-token-123").is_ok());
    assert!(verifier.verify("dev-token-124").is_err());
}

#[test]
fn refuses_keys_not_meant_for_signature_verification() {
    let fixture = Fixture::new();
    let mut set: serde_json::Value = serde_json::from_str(&jwks(&[("k1", &key(1))])).unwrap();
    set["keys"][0]["use"] = "enc".into();
    fixture.write("jwks.json", &set.to_string());
    let config = fixture.write(
        "sekisho.toml",
        &format!(
            "listen = \"127.0.0.1:0\"\n[verifier]\nkind = \"jwks\"\nissuer = \"{ISSUER}\"\naudience = \"{AUDIENCE}\"\njwks_file = \"jwks.json\"\n{MCP_SECTION}"
        ),
    );
    let verifier = Verifier::from_config(&Config::load(&config).unwrap().verifier).unwrap();
    let token = token("k1", &key(1), claims(serde_json::json!({})));
    assert_eq!(verifier.verify(&token), Err(Rejection::UnknownKey));
}
