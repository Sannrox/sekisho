//! Bearer-token verification.
//!
//! [`Verifier::Jwks`] is the default: an asymmetric JWT whose `kid` names a
//! key in a local JWKS file, with issuer, audience and expiry checked.
//! [`Verifier::Static`] compares one development token in constant time.

use std::path::Path;

use jsonwebtoken::jwk::{JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use subtle::ConstantTimeEq as _;

use crate::config::VerifierConfig;

/// Asymmetric algorithms only; shared-secret JWTs are refused.
const ALLOWED: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

#[derive(Debug, thiserror::Error)]
pub enum VerifierError {
    #[error("reading {label} {path}: {source}")]
    Read {
        label: &'static str,
        path: String,
        source: std::io::Error,
    },
    #[error("invalid JWKS: {0}")]
    Jwks(String),
    #[error("static token file is empty")]
    EmptyToken,
}

/// Why a presented token was refused. Messages never echo the token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    #[error("token is malformed")]
    Malformed,
    #[error("token key is not in the configured JWKS")]
    UnknownKey,
    #[error("token algorithm is not allowed")]
    Algorithm,
    #[error("token is invalid: {0}")]
    Invalid(String),
}

/// Who a verified token speaks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub subject: String,
    /// The `act` (actor chain) claim as sent, for later authorization layers.
    pub actor: Option<serde_json::Value>,
}

pub enum Verifier {
    Jwks {
        issuer: String,
        audience: String,
        keys: JwkSet,
    },
    Static {
        token: Vec<u8>,
    },
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    act: Option<serde_json::Value>,
}

impl Verifier {
    /// Build from config, reading the JWKS or token file once.
    pub fn from_config(config: &VerifierConfig) -> Result<Self, VerifierError> {
        match config {
            VerifierConfig::Jwks {
                issuer,
                audience,
                jwks_file,
            } => {
                let raw = read("JWKS", jwks_file)?;
                // Position only: parser messages can echo file contents.
                let keys: JwkSet = serde_json::from_slice(&raw).map_err(|error| {
                    VerifierError::Jwks(format!(
                        "not a JWKS document (line {}, column {})",
                        error.line(),
                        error.column()
                    ))
                })?;
                if keys.keys.is_empty() || keys.keys.iter().any(|key| key.common.key_id.is_none()) {
                    return Err(VerifierError::Jwks(
                        "it needs at least one key, and every key needs a kid".into(),
                    ));
                }
                Ok(Self::Jwks {
                    issuer: issuer.clone(),
                    audience: audience.clone(),
                    keys,
                })
            }
            VerifierConfig::Static { token_file } => {
                let token = read("static token", token_file)?.trim_ascii().to_vec();
                if token.is_empty() {
                    return Err(VerifierError::EmptyToken);
                }
                Ok(Self::Static { token })
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Jwks { .. } => "jwks",
            Self::Static { .. } => "static",
        }
    }

    /// Key ids in the JWKS; empty for the static verifier.
    pub fn key_ids(&self) -> Vec<String> {
        match self {
            Self::Jwks { keys, .. } => keys
                .keys
                .iter()
                .filter_map(|key| key.common.key_id.clone())
                .collect(),
            Self::Static { .. } => Vec::new(),
        }
    }

    pub fn verify(&self, bearer: &str) -> Result<Principal, Rejection> {
        match self {
            Self::Jwks {
                issuer,
                audience,
                keys,
            } => {
                let header =
                    jsonwebtoken::decode_header(bearer).map_err(|_| Rejection::Malformed)?;
                if !ALLOWED.contains(&header.alg) {
                    return Err(Rejection::Algorithm);
                }
                let jwk = header
                    .kid
                    .as_deref()
                    .and_then(|kid| keys.find(kid))
                    .ok_or(Rejection::UnknownKey)?;
                if jwk
                    .common
                    .key_algorithm
                    .is_some_and(|declared| declared != KeyAlgorithm::from(header.alg))
                {
                    return Err(Rejection::Algorithm);
                }
                // A key restricted to anything but signature verification is not usable.
                let use_ok = matches!(
                    jwk.common.public_key_use,
                    None | Some(PublicKeyUse::Signature)
                );
                let ops_ok = jwk
                    .common
                    .key_operations
                    .as_ref()
                    .is_none_or(|ops| ops.contains(&KeyOperations::Verify));
                if !use_ok || !ops_ok {
                    return Err(Rejection::UnknownKey);
                }
                let key = DecodingKey::from_jwk(jwk).map_err(|_| Rejection::UnknownKey)?;
                let mut validation = Validation::new(header.alg);
                validation.set_issuer(&[issuer]);
                validation.set_audience(&[audience]);
                validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
                validation.validate_nbf = true;
                let claims = jsonwebtoken::decode::<Claims>(bearer, &key, &validation)
                    .map_err(|error| Rejection::Invalid(error.to_string()))?
                    .claims;
                if claims.sub.trim().is_empty() {
                    return Err(Rejection::Invalid("empty subject".into()));
                }
                Ok(Principal {
                    subject: claims.sub,
                    actor: claims.act,
                })
            }
            Self::Static { token } => {
                if constant_time_eq(token, bearer.as_bytes()) {
                    Ok(Principal {
                        subject: "development".into(),
                        actor: None,
                    })
                } else {
                    Err(Rejection::Invalid("static token does not match".into()))
                }
            }
        }
    }
}

/// Equal-length inputs compare in constant time. Unequal lengths fail
/// without comparing contents, so timing reveals at most the length.
pub fn constant_time_eq(expected: &[u8], presented: &[u8]) -> bool {
    expected.len() == presented.len() && bool::from(expected.ct_eq(presented))
}

fn read(label: &'static str, path: &Path) -> Result<Vec<u8>, VerifierError> {
    std::fs::read(path).map_err(|source| VerifierError::Read {
        label,
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_compare_is_exact() {
        assert!(constant_time_eq(b"s3cret-token", b"s3cret-token"));
        assert!(!constant_time_eq(b"s3cret-token", b"s3cret-tokeX"));
        assert!(!constant_time_eq(b"s3cret-token", b"s3cret"));
        assert!(!constant_time_eq(b"s3cret-token", b""));
    }
}
