//! Sekisho: the governed entry point between AI agents and tools.
//!
//! This crate is the standalone host: configuration, the bearer-token
//! verifier, the MCP endpoint, health and metrics endpoints, and the
//! diagnostics bundle. The JWKS is a local file, so it starts and verifies
//! tokens without reaching the issuer. Upstream MCP calls and per-call token
//! exchange are separate, and only happen when those are configured.

pub mod approvals;
pub mod audit;
pub mod config;
pub mod control_plane;
pub mod diagnostics;
pub mod discovery;
pub mod exchange;
pub mod gateway;
pub mod guardrails;
pub mod limits;
pub mod metrics;
pub mod mitigations;
pub mod policy;
pub mod registry;
pub mod server;
pub mod state;
pub mod upstream;
pub mod verifier;
