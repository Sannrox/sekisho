//! Sekisho: the governed entry point between AI agents and tools.
//!
//! This crate is the standalone host: configuration, the bearer-token
//! verifier, the MCP endpoint, health and metrics endpoints, and the
//! diagnostics bundle. It
//! makes no network calls of its own; the JWKS comes from a local file, so it
//! starts and verifies tokens offline.

pub mod config;
pub mod diagnostics;
pub mod gateway;
pub mod metrics;
pub mod server;
pub mod verifier;
