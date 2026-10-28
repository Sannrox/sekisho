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
  The optional `token_file` is used only to list tools; tool calls never
  carry it.
- **Per-call token exchange.** For each tool call Sekisho exchanges the
  caller's token at `[exchange] token_endpoint` (RFC 8693, client
  authenticated) for a short-lived token with the upstream's `audience` and
  `scope = tool:<name>` plus the agent's ceiling as `grant:<g>` scopes,
  which the issuer intersects with the user's grants. The issuer puts the user in `sub` and the gateway
  and agent in `act`. The engine sees only that token. Tokens are cached per
  caller token, agent, ceiling, audience and tool until shortly before the
  issuer's `expires_in`; tokens without it are not cached. If the
  exchange fails, or `[exchange]` is not configured, the call is refused
  and audited; nothing broader is tried.
- **Progressive discovery.** Three meta-tools are always listed:
  `sekisho.search_tools` (every query word in a tool's name or
  description), `sekisho.get_tool_schema`, and `sekisho.invoke_tool`
  (`{name, arguments}`). They only ever show or reach tools the caller may
  use. `invoke_tool` is an ordinary call of its target: same allowlist,
  token exchange and audit. With `[mcp] progressive = true`, `tools/list`
  returns only the meta-tools. Upstream name `sekisho` is reserved.
- **Limits.** Token buckets per agent, user and tenant (from a verified
  `tenant_id` claim), with per-tool cost weights. A call over any bucket is
  refused before the token exchange with `retry_after_ms` and audited as
  `throttled`. Responses above `max_response_bytes` are truncated, with a
  final text block saying so, and upstream calls longer than `max_call_ms`
  fail. Defaults (agent 10/s burst 20, user 20/s burst 40, tenant 100/s
  burst 200, 1 MiB, 60 s) leave interactive use alone. With `[cluster]
  state_dir`, replicas share the buckets in `limits.json` under an exclusive
  flock; a store error fails closed with a 1s retry. MCP sessions are
  already stateless (2026-07-28); registry, policy and audit stay files.
  Audit lines are fsynced before the response.
- **Approval gate.** Tools listed in `[approvals] marked` (destructive or
  open-world, by registry key) are forwarded only along an approval route. A
  marked tool also listed in `engine` goes to an upstream that parks and
  decides the call as a governed Action, so the engine's parking is the
  approval and Sekisho does not prompt again. A marked tool without a route
  is refused before any delegated token is minted, so a missing or mistyped
  route fails closed; an `engine` entry that is not marked fails config
  load. Unmarked tools are unchanged. The audit line records the gate as
  `approval`: `engine` or `required`. A caller-supplied park id is not
  accepted yet: checking one needs a control-plane client.
- **Call audit.** Every tool call appends one JSON line to `[audit]
  calls_file`: subject, agent, tool, decision (`allowed`, `denied`,
  `failed`, `throttled`), reason, guardrail and approval summaries, latency,
  response size and trace id. Events never carry
  tokens, arguments or results. The trace id comes from the caller's W3C
  `traceparent`, or is generated, and the upstream call carries it in a new
  `traceparent` span so engine receipts link back. `/metrics` counts calls
  by decision (`sekisho_tool_calls_total`). Replicas append to the shared
  file under an exclusive flock. Subjects listed in `[audit] readers` read
  one trace back with `GET /audit/calls?trace_id=<32 hex>&cursor=&limit=`
  (oldest first, at most 500 events and 1 MiB scanned per page, without
  blocking appends). They see only events for their own
  `tenant_id` claim, and another tenant's trace reads as an empty page.
  Reads are off (404) while `readers` is empty.

  ```sh
  sekisho --config sekisho.toml registry list
  sekisho --config sekisho.toml registry show ontology.search
  sekisho --config sekisho.toml registry enable ontology.search --owner alice
  sekisho --config sekisho.toml registry approve ontology.search --owner alice
  sekisho --config sekisho.toml registry rollback ontology.search --owner alice
  ```
- **Agent allowlists.** `[policy] file` lists each agent's allowed tools
  and grant ceiling (see `agents.example.toml`). The agent is the token's
  outermost `act` actor, or the subject when there is none. Agents see and
  call only allowed tools; other calls are refused before forwarding and
  audited. The file is read on every call, so an
  edit applies to the next one. A missing or unreadable file refuses every
  call. Engines enforce the user's grants intersected with the ceiling
  through the delegated token from per-call token exchange.
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
