//! Sekisho: the governed entry point between AI agents and tools.
//!
//! This crate is the standalone host: configuration, the bearer-token
//! verifier, the MCP endpoint, health and metrics endpoints, and the
//! diagnostics bundle. It
//! makes no network calls of its own; the JWKS comes from a local file, so it
//! starts and verifies tokens offline.

pub mod audit;
pub mod config;
pub mod diagnostics;
pub mod discovery;
pub mod exchange;
pub mod gateway;
pub mod limits;
pub mod metrics;
pub mod policy;
pub mod registry;
pub mod server;
pub mod upstream;
pub mod verifier;
