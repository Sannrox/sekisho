# Sekisho

Sekisho is the governed gateway between AI agents and tools.

- Exposes one MCP endpoint (Streamable HTTP) that federates upstream tool servers.
- Exchanges the caller's token per tool call for a short-lived token scoped to user, agent, audience and tool.
- Includes a tool registry where tools start disabled and the owner enables them.
- Enforces per-agent allowlists, rate limits, approval gates and audit.
- Routes outbound calls to external MCP servers through an allowlist with credential injection.

Engines keep enforcing their own authorization, and Sekisho never holds a privileged downstream credential.

## Run

```sh
cp sekisho.example.toml sekisho.toml   # point jwks_file at your issuer's JWKS
sekisho --config sekisho.toml serve    # prints "listening on <addr>"; MCP at /mcp
curl http://127.0.0.1:8090/healthz
curl http://127.0.0.1:8090/metrics
sekisho --config sekisho.toml diagnostics > bundle.json
```

- **MCP endpoint.** `/mcp` serves MCP over Streamable HTTP: stateless
  2026-07-28 by default, with negotiated fallback to 2025-11-25. Every
  request needs a bearer token. A missing or invalid token gets 401 with
  `WWW-Authenticate: Bearer resource_metadata="…"`, and the OAuth
  protected-resource metadata (RFC 9728) is at
  `/.well-known/oauth-protected-resource/mcp`. `Host` and `Origin` are
  checked against `[mcp]` allowlists, and a body above `max_request_bytes`
  gets 413 before it is parsed. A client that cancels a request or
  disconnects cancels the work (SDK request cancellation).
- **Tool registry.** Sekisho lists each `[[upstreams]]` server's tools and
  records them in the registry file as `<upstream>.<tool>`. A new tool is
  disabled and is neither listed nor callable until an owner enables it. A
  changed description or schema becomes a pending version: agents keep the
  approved one until the owner approves, and `rollback` restores the previous
  approved version. Upstream listings and the registry file are re-read
  every `refresh_interval_ms`, so changes apply without a restart. Owner
  edits are appended to the audit file.
  The optional `token_file` is used only to list tools. Tool calls never
  carry it; per-call token exchange supplies a caller-scoped credential.

  ```sh
  sekisho --config sekisho.toml registry list
  sekisho --config sekisho.toml registry show ontology.search
  sekisho --config sekisho.toml registry enable ontology.search --owner alice
  sekisho --config sekisho.toml registry approve ontology.search --owner alice
  sekisho --config sekisho.toml registry rollback ontology.search --owner alice
  ```
- **Verifier.** `jwks` checks asymmetric JWTs (RS, PS, ES, EdDSA; never
  shared-secret HS) against a local JWKS file: `kid`, signature, issuer,
  audience, expiry and subject. `static` is one development token, compared
  in constant time and refused unless `listen` is a loopback address.
- **Offline.** Sekisho makes no network calls of its own. The JWKS is a file,
  so it starts and verifies tokens without reaching the issuer.
- **Diagnostics.** The bundle lists the version, listen address, verifier
  kind, issuer, audience and key ids. It never includes tokens, key material
  or file contents.
- **Delivery.** `Containerfile` builds the release image. Deploy it by
  digest, not by tag.

Status: early. See [VISION.md](VISION.md) for the roadmap and GitHub Issues for planned work.

License: Apache-2.0.
