# Sekisho vision

Sekisho (関所, checkpoint) is the single governed entry point between AI agents and tools.

## Vision
- **Inbound:** agents reach engine tools (ontology, query, delivery) through one MCP endpoint. Each call carries a short-lived token exchanged for that user, agent, audience and tool.
- **Outbound:** agents reach external MCP servers only through an allowlist, with injected credentials and egress policy.
- **Responsibility split:** engines remain the source of truth for authorization, markings and receipts. Sekisho adds the cross-cutting agent controls: tool registry with owner enablement, per-agent allowlists and ceilings, rate limits, approval gates for destructive tools, redaction and projection, unified audit, kill switch, and progressive discovery.
- Standalone: single binary, own config, pluggable token verifier (JWKS), works without the wider platform.

## Roadmap
1. **v0.1 MVP:** Streamable HTTP MCP server (2026-07-28 stateless with 2025-11-25 fallback), OAuth protected-resource metadata, tool registry (disabled by default), federation of upstream MCP servers, per-agent allowlist, per-call token exchange, rate limits, response caps, audit with trace ids, progressive discovery tools.
2. **v0.2:** outbound egress gateway, approval gate for destructive and open-world tools (#21), response projection and secret redaction.
3. **Later:** tools generated from gRPC/protobuf descriptors, sandboxed stdio servers, HA.
