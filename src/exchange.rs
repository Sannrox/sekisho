//! Per-call token exchange (RFC 8693).
//!
//! For each tool call Sekisho exchanges the caller's token at the issuer's
//! token endpoint for one scoped to the target engine (`audience`) and this
//! tool (`scope = tool:<name>`), plus the agent's grant ceiling as
//! `grant:<g>` scopes so the issuer can intersect the user's grants with it.
//! The token is short-lived and names the agent and gateway in its actor
//! chain. Sekisho authenticates as a client; it never forwards its own
//! credential or the caller's original token. Results are cached per caller
//! token, agent, ceiling, audience and tool until shortly before the
//! issuer-reported expiry; a response without `expires_in` is not cached.
//! When the exchange fails the call is refused; nothing broader is tried.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::Digest as _;

use crate::config::ExchangeConfig;

const TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN: &str = "urn:ietf:params:oauth:token-type:access_token";
/// Re-exchange this long before a cached token expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(10);
/// Cached tokens at most; expired ones are dropped first.
const CACHE_CAPACITY: usize = 10_000;

#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    #[error("token exchange is not configured")]
    NotConfigured,
    /// The issuer, or Sekisho's own checks, refused this exchange.
    #[error("token exchange refused: {0}")]
    Refused(String),
    /// The issuer could not be reached or answered with an error.
    #[error("token exchange failed: {0}")]
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    /// SHA-256 of the caller's token: a different token, even for the same
    /// user and agent, never reuses another's delegated credential.
    caller_token: [u8; 32],
    agent: String,
    scope: String,
    audience: String,
    tool: String,
}

#[derive(Deserialize)]
struct ExchangeResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

pub struct Exchanger {
    config: Option<ExchangeConfig>,
    client: reqwest::Client,
    cache: Mutex<HashMap<CacheKey, (String, Instant)>>,
}

/// What one call needs a token for.
pub struct Request<'a> {
    pub caller_token: &'a str,
    pub agent: &'a str,
    /// The agent's grant ceiling, sent as `grant:<g>` scopes.
    pub ceiling: &'a [String],
    pub audience: &'a str,
    pub tool: &'a str,
}

impl Exchanger {
    pub fn new(config: Option<ExchangeConfig>) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            cache: Mutex::default(),
        }
    }

    pub async fn token(&self, request: Request<'_>) -> Result<String, ExchangeError> {
        let config = self.config.as_ref().ok_or(ExchangeError::NotConfigured)?;
        // Upstream-supplied names must not smuggle extra scope values.
        if !scope_token(request.tool) || !request.ceiling.iter().all(|grant| scope_token(grant)) {
            return Err(ExchangeError::Refused(
                "tool name or ceiling grant is not a plain scope value".into(),
            ));
        }
        let mut scopes = vec![format!("tool:{}", request.tool)];
        scopes.extend(request.ceiling.iter().map(|grant| format!("grant:{grant}")));
        let scope = scopes.join(" ");
        let key = CacheKey {
            caller_token: sha2::Sha256::digest(request.caller_token.as_bytes()).into(),
            agent: request.agent.into(),
            scope: scope.clone(),
            audience: request.audience.into(),
            tool: request.tool.into(),
        };
        let now = Instant::now();
        if let Some((token, until)) = self.cache().get(&key)
            && *until > now
        {
            return Ok(token.clone());
        }
        let secret = read_secret(&config.client_secret_file)?;
        let lifetime = config.lifetime_secs.to_string();
        let response = self
            .client
            .post(&config.token_endpoint)
            // RFC 6749 2.3.1: form-encode each part before Basic.
            .basic_auth(form_encode(&config.client_id), Some(form_encode(&secret)))
            .form(&[
                ("grant_type", TOKEN_EXCHANGE),
                ("subject_token", request.caller_token),
                ("subject_token_type", ACCESS_TOKEN),
                ("requested_token_type", ACCESS_TOKEN),
                ("audience", request.audience),
                ("scope", &scope),
                ("requested_lifetime", &lifetime),
            ])
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|error| ExchangeError::Failed(error.without_url().to_string()))?;
        let status = response.status();
        if status.is_client_error() {
            return Err(ExchangeError::Refused(format!("issuer answered {status}")));
        }
        if !status.is_success() {
            return Err(ExchangeError::Failed(format!("issuer answered {status}")));
        }
        let body: ExchangeResponse = response
            .json()
            .await
            .map_err(|_| ExchangeError::Failed("issuer response is not a token".into()))?;
        // Only the issuer's stated expiry is trusted for caching.
        if let Some(until) = body
            .expires_in
            .and_then(|seconds| (now + Duration::from_secs(seconds)).checked_sub(EXPIRY_MARGIN))
            .filter(|until| *until > now)
        {
            let mut cache = self.cache();
            cache.retain(|_, (_, cached_until)| *cached_until > now);
            if cache.len() < CACHE_CAPACITY {
                cache.insert(key, (body.access_token.clone(), until));
            }
        }
        Ok(body.access_token)
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<CacheKey, (String, Instant)>> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One scope value: non-empty, ASCII letters, digits, `.`, `_` or `-`.
fn scope_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn form_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn read_secret(path: &PathBuf) -> Result<String, ExchangeError> {
    std::fs::read_to_string(path)
        .map(|secret| secret.trim().to_string())
        .map_err(|_| ExchangeError::Failed("client secret is unreadable".into()))
}

#[cfg(test)]
mod tests {
    use super::scope_token;

    #[test]
    fn scope_values_cannot_carry_delimiters() {
        assert!(scope_token("echo"));
        assert!(scope_token("read-only_v1.2"));
        for bad in ["", "echo grant:write", "a\tb", "x\"y", "tool:x"] {
            assert!(!scope_token(bad), "{bad:?}");
        }
    }
}
