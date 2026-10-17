# Sekisho

Sekisho is the governed gateway between AI agents and tools.

- Exposes one MCP endpoint (Streamable HTTP) that federates upstream tool servers.
- Exchanges the caller's token per tool call for a short-lived token scoped to user, agent, audience and tool.
- Includes a tool registry where tools start disabled and the owner enables them.
- Enforces per-agent allowlists, rate limits, approval gates and audit.
- Routes outbound calls to external MCP servers through an allowlist with credential injection.

Engines keep enforcing their own authorization, and Sekisho never holds a privileged downstream credential.

Status: early. See [VISION.md](VISION.md) for the roadmap and GitHub Issues for planned work.

License: Apache-2.0.
