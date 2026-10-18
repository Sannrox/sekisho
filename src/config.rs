//! `sekisho.toml`: listen address and token verifier.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address to bind. Port 0 picks a free port; `serve` prints the bound one.
    pub listen: SocketAddr,
    pub verifier: VerifierConfig,
    pub mcp: McpConfig,
}

/// The MCP endpoint at `/mcp` and its OAuth protected-resource metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// Canonical URL of this endpoint, as clients reach it (RFC 9728 `resource`).
    pub resource: String,
    /// Authorization servers that issue tokens for this resource.
    pub authorization_servers: Vec<String>,
    /// `Host` values accepted on `/mcp`. Default: loopback names only, which
    /// blocks DNS rebinding against a local server.
    #[serde(default = "loopback_hosts")]
    pub allowed_hosts: Vec<String>,
    /// Browser `Origin` values accepted on `/mcp`, with scheme and port
    /// (`:*` for any port). Requests without `Origin` pass. Empty refuses
    /// every request that carries an `Origin`.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Largest request body. Larger requests get 413 before any parsing.
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
}

fn loopback_hosts() -> Vec<String> {
    ["localhost", "127.0.0.1", "[::1]"]
        .map(String::from)
        .to_vec()
}

fn default_max_request_bytes() -> usize {
    1024 * 1024
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerifierConfig {
    /// Signed JWTs checked against a local JWKS file.
    Jwks {
        issuer: String,
        audience: String,
        jwks_file: PathBuf,
    },
    /// One shared token, for development only. Refused unless `listen` is a
    /// loopback address. The token is read from `token_file`, never the config.
    Static { token_file: PathBuf },
}

impl Config {
    /// Load and validate. Relative paths resolve against the config's directory.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.into(),
            source,
        })?;
        let mut config: Self = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.into(),
            source: Box::new(source),
        })?;
        let base = path.parent().unwrap_or(Path::new("."));
        match &mut config.verifier {
            VerifierConfig::Jwks { jwks_file, .. } => *jwks_file = base.join(&*jwks_file),
            VerifierConfig::Static { token_file } => *token_file = base.join(&*token_file),
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.mcp.resource.trim().is_empty() || self.mcp.authorization_servers.is_empty() {
            return Err(ConfigError::Invalid(
                "mcp needs a resource URL and at least one authorization server".into(),
            ));
        }
        if self.mcp.max_request_bytes == 0 {
            return Err(ConfigError::Invalid(
                "mcp max_request_bytes must be positive".into(),
            ));
        }
        match &self.verifier {
            VerifierConfig::Jwks {
                issuer, audience, ..
            } if issuer.trim().is_empty() || audience.trim().is_empty() => Err(
                ConfigError::Invalid("jwks verifier needs an issuer and an audience".into()),
            ),
            VerifierConfig::Static { .. } if !self.listen.ip().is_loopback() => {
                Err(ConfigError::Invalid(
                    "the static token verifier is for development and needs a loopback listen address"
                        .into(),
                ))
            }
            _ => Ok(()),
        }
    }
}
