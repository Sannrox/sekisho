//! Support bundle: what is configured, never secret material.

use serde::Serialize;

use crate::config::{Config, VerifierConfig};
use crate::verifier::Verifier;

/// Configuration shape and verifier key ids. Static tokens, JWKS key
/// material, control-plane tokens and file contents are never included.
#[derive(Debug, Serialize)]
pub struct Bundle {
    pub version: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    pub listen: String,
    pub verifier: VerifierSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_plane: Option<ControlPlaneSummary>,
}

#[derive(Debug, Serialize)]
pub struct ControlPlaneSummary {
    pub endpoint: String,
    pub namespace: String,
}

#[derive(Debug, Serialize)]
pub struct VerifierSummary {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Whether the verifier loaded; the error text when it did not.
    pub status: String,
    pub key_ids: Vec<String>,
}

pub fn collect(config: &Config) -> Bundle {
    let (issuer, audience) = match &config.verifier {
        VerifierConfig::Jwks {
            issuer, audience, ..
        } => (Some(issuer.clone()), Some(audience.clone())),
        VerifierConfig::Static { .. } => (None, None),
    };
    let (kind, status, key_ids) = match Verifier::from_config(&config.verifier) {
        Ok(verifier) => (verifier.kind(), "loaded".to_string(), verifier.key_ids()),
        Err(error) => (
            match config.verifier {
                VerifierConfig::Jwks { .. } => "jwks",
                VerifierConfig::Static { .. } => "static",
            },
            error.to_string(),
            Vec::new(),
        ),
    };
    Bundle {
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        listen: config.listen.to_string(),
        verifier: VerifierSummary {
            kind,
            issuer,
            audience,
            status,
            key_ids,
        },
        control_plane: config
            .mitigations
            .control_plane
            .as_ref()
            .map(|plane| ControlPlaneSummary {
                endpoint: plane.endpoint.clone(),
                namespace: plane.namespace.clone(),
            }),
    }
}
