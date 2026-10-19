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
    #[serde(default)]
    pub registry: RegistryConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub audit: AuditConfig,
    /// Per-call token exchange. Without it, tool calls are refused.
    #[serde(default)]
    pub exchange: Option<ExchangeConfig>,
    /// Upstream MCP servers whose tools the registry federates.
    #[serde(default)]
    pub upstreams: Vec<UpstreamConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConfig {
    pub file: PathBuf,
    /// Owner edits, one JSON line each.
    pub audit_file: PathBuf,
    /// How often upstream listings and the registry file are re-read.
    pub refresh_interval_ms: u64,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            file: "registry.json".into(),
            audit_file: "registry-audit.jsonl".into(),
            refresh_interval_ms: 30_000,
        }
    }
}

/// RFC 8693 token exchange at the issuer ([`crate::exchange`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeConfig {
    pub token_endpoint: String,
    /// Sekisho's client id at the issuer.
    pub client_id: String,
    /// File holding the client secret; never inlined in the config.
    pub client_secret_file: PathBuf,
    /// Requested lifetime of exchanged tokens.
    #[serde(default = "default_lifetime_secs")]
    pub lifetime_secs: u64,
}

fn default_lifetime_secs() -> u64 {
    60
}

/// Per-agent allowlists ([`crate::policy`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    /// Read on every call; unreadable means every call is refused.
    pub file: PathBuf,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            file: "agents.toml".into(),
        }
    }
}

/// Tool-call audit ([`crate::audit`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    /// One JSON line per tool call, allowed or not.
    pub calls_file: PathBuf,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            calls_file: "calls.jsonl".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Prefix of this upstream's tools (`<name>.<tool>`): ASCII letters,
    /// digits, `-` and `_`.
    pub name: String,
    /// The upstream's Streamable HTTP MCP endpoint.
    pub url: String,
    /// Optional bearer token file used to list the upstream's tools.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// Audience of exchanged tokens for this upstream's tools.
    pub audience: String,
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
    /// List only the discovery meta-tools; agents find the rest by search.
    #[serde(default)]
    pub progressive: bool,
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
        config.registry.file = base.join(&config.registry.file);
        config.registry.audit_file = base.join(&config.registry.audit_file);
        config.policy.file = base.join(&config.policy.file);
        config.audit.calls_file = base.join(&config.audit.calls_file);
        if let Some(exchange) = &mut config.exchange {
            exchange.client_secret_file = base.join(&exchange.client_secret_file);
        }
        for upstream in &mut config.upstreams {
            if let Some(token_file) = &mut upstream.token_file {
                *token_file = base.join(&*token_file);
            }
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
        if self.registry.refresh_interval_ms == 0 {
            return Err(ConfigError::Invalid(
                "registry refresh_interval_ms must be positive".into(),
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        for upstream in &self.upstreams {
            let valid = !upstream.name.is_empty()
                && upstream
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            // `sekisho.*` names belong to the discovery meta-tools.
            if !valid || upstream.name == "sekisho" || !names.insert(&upstream.name) {
                return Err(ConfigError::Invalid(format!(
                    "upstream name {:?} must be unique and use letters, digits, - or _",
                    upstream.name
                )));
            }
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
