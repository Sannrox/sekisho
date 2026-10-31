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
curl http://127.0.0.1:8090/healthz    # liveness
curl http://127.0.0.1:8090/readyz     # 503 while a state store cannot do work
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
  approved version. Upstream listings are re-read every
  `refresh_interval_ms` and the stored registry every second, so changes
  apply without a restart. A replica that cannot load the registry for
  5 s serves no tools until it can. Owner edits are appended to the audit
  file.
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
  Audit lines are fsynced before the response; see Call audit for what
  happens when one cannot be written.
- **State backend.** Rate-limit buckets, registry, policy, call audit and
  mitigations go through one state backend. The directory backend, the
  default, is the files described here, in the same formats as earlier
  releases, so mixed versions can share them during a rolling deploy. Its
  replicas need one host or a filesystem whose exclusive locks they all
  see. With a build that has the `postgres` cargo feature
  (`cargo build --release --features postgres`), `[state] backend =
  "postgres"` keeps every store in PostgreSQL, for replicas on separate
  hosts; see PostgreSQL below. `GET /readyz` answers 200 when every store
  can do work and 503 otherwise, with `{"backend", "ready", "stores"}`
  naming each store `ok` or `unavailable`; the reason goes to the log. A
  store is ready when its data parses and its write path works: a scratch
  file is created, synced, renamed and removed in its directory while the
  store's writer lock is held, and its audit log opens with the writer's
  options; an audit log's directory also takes the scratch write, so a full
  disk shows even while the log opens. A probe creates no real file or directory: an absent lock file
  or log is covered by the scratch check where the writer would create it.
  A missing directory counts only where the writer creates it (the rate-limit
  and mitigation state directory); a registry or audit file whose directory
  is missing makes its store unavailable. Results
  are reused for 1 s.
  `/healthz` stays a liveness check.
- **PostgreSQL.** `[state] backend = "postgres"` with `url_file`, a file
  holding the connection URL
  (`postgres://user:password@host:5432/sekisho?sslmode=require`), so the
  password stays out of the config. It holds every store: rate limits,
  call audit, registry, agent policy and mitigations, so `[cluster]
  state_dir`, `[registry] file` and `audit_file`, `[policy] file`,
  `[audit] calls_file` and `[mitigations]` paths are not used. Rate limits
  are always shared with this backend.
  - *Schema.* Sekisho creates its tables (`sekisho_rate_buckets`,
    `sekisho_call_audit`, `sekisho_state`, `sekisho_registry_audit`,
    `sekisho_mitigation_audit`, `sekisho_schema_migrations`) on its first
    connection, in one transaction under an advisory lock, so replicas
    starting together are safe; a replica waits at most 3 s for another's
    migration (30 s per statement), then retries on its next use.
    Migrations are versioned and additive; an older release ignores newer
    versions. The user needs `CREATE` on the
    schema the first time, then read and write on these tables.
  - *Rate limits.* A charge is one transaction that locks the call's
    bucket rows, so replicas never lose an update. Each replica uses its
    own clock: over any window a bucket admits at most `rate_per_sec ×`
    the clock skew between replicas more than one clock would (none with
    synchronized clocks). Buckets that are full again are deleted after a
    charge, at most 500 at a time and once a minute per replica.
  - *Call audit.* Each event is committed before the call answers, with
    `synchronous_commit` raised to `on` when the server default is `off`.
  - *Registry, policy, mitigations.* Each is one JSON document. An edit
    locks it, applies the same rules as the directory backend and commits
    the change with its audit records (owner edits, mitigation events) in
    one transaction, so concurrent edits from any replica serialize and a
    change is never committed unaudited. Policy and mitigations are read
    on every call, so a change applies to the next call on every replica.
    Each replica reloads the registry every second, so an owner edit is
    served everywhere within about 1 s (upstream listings still refresh
    every `refresh_interval_ms`). A replica that has not loaded the
    registry for 5 s serves no tools and `/readyz` reports `registry`
    unavailable, on every backend: a snapshot it cannot confirm might
    still serve a tool an owner disabled.
  - *Initialization.* Registry, policy and mitigations do not exist in
    the database until `sekisho state import --from <config>` (copy from
    another backend) or `sekisho state init --operator <name>` (a new,
    empty deployment) writes all three, their audit records and an
    `initialized` marker in one transaction; either is refused once the
    store is initialized. Until then every call is refused with `retry
    after 1000 ms` and a message naming these commands, and `/readyz`
    answers 503 with `"state": "uninitialized"`.
  - *Policy.* `sekisho policy import agents.toml` validates a policy file
    and replaces the stored policy in one transaction; `sekisho policy
    export` prints it in the same format. It is refused on a store that is
    not initialized, so a policy can never go live without the mitigations
    an import brings. On the directory backend the same commands write and
    read `[policy] file`.
  - *Outages.* While the database cannot be reached, calls fail closed
    with `retry after 1000 ms` and `/readyz` reports every store
    unavailable; connections come back on their own when it returns, with
    no restart. Readiness runs each store's write in a transaction it rolls
    back, on the same connections calls use. Waits are bounded: a free
    connection 1 s, connecting 2 s (unless the URL sets
    `connect_timeout`), a statement 2 s, a lock 1 s (such as a bucket row
    another replica holds), with TCP keepalives on. A call waits on the
    store at most 5 s in all before forwarding (rate limit and `started`
    event; for a call refused before forwarding, its audit event too), and
    at most 5 s more for a forwarded call's outcome event, then fails
    closed; a readiness probe waits at most 5 s, then reports not ready.
  - *Audit reads.* Events of one trace are numbered under a per-trace lock
    and commit in order, so `/audit/calls` paging never skips an event.
  - *TLS.* `sslmode` in the URL (`disable`, `prefer`, `require`) decides
    whether TLS is used; when it is, the server certificate and host name
    are always verified against the system roots (`SSL_CERT_FILE` adds
    one).
  - *Switching.* Moving from the directory backend, or upgrading a
    PostgreSQL deployment of the previous release (which kept registry,
    policy and mitigations in files), is a deliberate switch, not a rolling
    change: old and new replicas consult different authorities. Drain or
    stop the old replicas, run `sekisho --config postgres.toml state import
    --from old.toml --operator <name>`, where `old.toml` selects the
    directory backend over the files the old replicas used (for a previous
    PostgreSQL deployment, its config without the `[state]` section), then
    start the new ones. The import copies the
    agent policy, the registry (owner enablement and approved versions) and
    active mitigations, each audited as an import, in one transaction, so a
    failed import changes nothing and can be retried. Rate-limit buckets
    and audit logs are not copied: buckets start full and old audit events
    stay where they were.
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
- **Mitigations.** An operator can disable a tool (`--tool <upstream>.<tool>`)
  or a whole upstream (`--upstream`) for every caller, one tenant
  (`--tenant`, the `tenant_id` claim) or one agent (`--agent`), or deny an
  outbound destination (`--destination scheme://host[:port]`), with an
  optional `--expires-in-secs`. Each apply and lift names a decision
  reference that a pluggable decision verifier must accept. Without
  `[mitigations.control_plane]` the standalone verifier accepts any
  reference of 1 to 128 `A-Z a-z 0-9 . _ : / # -` from a named operator.
  With it, the gateway looks the Action up on the engine's HTTP/JSON
  projection (`GetActionInstance`; a lift also reads the mitigation object).
  The Action must be decided (`admitted`, granted if it parked), of type
  `security.mitigation.apply` or `security.mitigation.lift`, and name this
  mitigation's target (`gateway_tool_disable` for a tool or upstream,
  `outbound_deny` for a destination). A tenant-scoped mitigation must match
  the Action's namespace. The engine stays the source of truth for grants
  and markings. Unreachable, unknown, parked, denied or mismatched
  references are refused. A rejected decision changes nothing and is
  audited as `refuse`. Mitigations live in `[mitigations] file` and `audit_file`,
  which every replica must share. Unset, both default into `[cluster]
  state_dir` (`mitigations.json`, `mitigations-audit.jsonl`) when a cluster
  is configured, and next to the config file otherwise; an explicit path
  wins and resolves against the config file. Like the rate-limit buckets,
  the store needs a filesystem whose exclusive locks all replicas see;
  changes are serialized under a lock and replace the file atomically, and
  every forwarded call reads it, so a change applies to the next call on
  every replica without a restart. A mitigated call is refused before the
  token exchange and audited as `denied` with the decision in
  `mitigation`. A store that cannot be read or parsed refuses every
  forwarded call. Expiry is checked from the stored time on each call; the
  first call after it removes the mitigation and audits `expire` once.
  Apply, lift, expire and refuse are appended to `[mitigations] audit_file`,
  linked to the decision. A change and its audit events are written to the
  store file together in one atomic replace, then appended to the log; an
  append interrupted by a crash is completed by the next write, without
  duplicates, so the log records only committed changes. Applying the same
  decision to the same target and scope again while it is active, or
  lifting one already gone, changes nothing; a different expiry needs a new
  decision. Destinations
  match the origin a call is forwarded to (the upstream `url`): scheme and
  host lowercased, internationalized names in ASCII form, IP literals
  canonical, a trailing dot dropped, the port explicit (80 or 443 by
  default). Hosts are compared as written, without DNS, so a name and its
  address are different destinations; a deny entry with a path, query or
  user info is rejected. Replicas of earlier releases do not read
  mitigations: upgrade every replica before relying on them.

  ```sh
  sekisho --config sekisho.toml mitigation apply --tool ontology.search --agent assistant-7 \
    --decision INC-42 --operator alice --expires-in-secs 3600   # prints the id
  sekisho --config sekisho.toml mitigation list
  sekisho --config sekisho.toml mitigation lift <id> --decision INC-42-close --operator alice
  ```

  In platform mode `--decision` is the Action instance id (or its
  `operation_id`) of the decided apply or lift.
- **Call audit.** Every tool call appends one JSON line to `[audit]
  calls_file`: subject, agent, tool, decision (`allowed`, `denied`,
  `failed`, `throttled`), call id, reason, guardrail and approval summaries, the
  refusing mitigation's decision, latency,
  response size and trace id. Events never carry
  tokens, arguments or results. A call forwarded upstream also has a
  `started` event with the same call id, written before the upstream
  call: if it cannot be written the call is refused with `retry after
  1000 ms` and nothing runs. If the outcome cannot be written after the
  upstream ran, the call fails without a retry hint, with `call_id` and
  `forwarded: true` in the error data, and no result; check that call
  before retrying, since repeating it may repeat its effect. Releases
  before `started` events skip them on read. The trace id comes from the caller's W3C
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
  audited. The policy is read on every call, so an
  edit applies to the next one. A missing or unreadable policy refuses
  every call. `sekisho policy import <file>` and `export` replace and print
  it on either backend. Engines enforce the user's grants intersected with the ceiling
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
  digest, not by tag. There is no `latest` image tag. Signed `v*` tags
  and GitHub Releases are described in [RELEASING.md](RELEASING.md).

## High availability

With the PostgreSQL backend, replicas are stateless replicas: every store
lives in the database, so any surviving replica takes over a lost one's
traffic with nothing to hand over. Clients retry on another replica. This
is availability of the gateway; restarting or restoring the database is
recovery, not availability.

Stated bounds:

- A call waits at most 5 s on the database before forwarding, and 5 s more
  for a forwarded call's outcome, then fails closed.
- An owner's registry edit reaches every replica within about 1 s; policy
  and mitigation changes apply to the next call. A replica that has not
  loaded the registry for 5 s serves no tools and is not ready.
- Rate limits are serialized in the database; the only error is
  `rate_per_sec ×` the clock skew between replicas.

Drill evidence (`tests/kill_drill.rs`, run with the `postgres` feature and
`SEKISHO_TEST_POSTGRES_URL`): two `sekisho` processes with separate working
directories and no shared local file, on one PostgreSQL, under steady load
at twice the rate limit, behind a client that retries on the survivor; one
process is killed with SIGKILL. Over six local runs:

- Interruption window (kill to first successful retry): 11 to 125 ms.
- Acknowledged-data-loss bound: zero. Every call acknowledged to a client
  had its `started` and outcome audit events in PostgreSQL; the in-flight
  call on the killed process failed with a connection error, not a wrong
  answer.
- Rate limit held across the failover: 68 to 69 calls reached the upstream
  (the killed in-flight one included) against `burst + rate × load time`
  plus one call of slack, 71.3. A limiter that lost its state on failover
  would admit up to another burst (10) and fail the drill.
- The killed process rejoined after a restart (`/readyz` 200).

Two cases are not retried safely, because the upstream may already have
acted:

- a forwarded call whose outcome event cannot be written fails without a
  retry hint and names its call id;
- a connection that drops before the response arrives (a replica killed
  mid-call, as in the drill) gives the client no answer at all.

Retry automatically on another replica only when the response carries a
retry hint, or when the tool is idempotent or deduplicated. Otherwise check
the call's audit events before repeating it. The drill's tool is a
read-only echo, so it cannot show a repeated side effect.

One PostgreSQL instance moves the single point of failure from the gateway
to the database; it does not remove it. Removing it needs a replicated
database with measured failover, which Sekisho neither provides nor has
tested. Remaining single points of failure:

- the PostgreSQL instance (an outage refuses every call until it returns);
- the token issuer that exchanges tokens per call;
- each upstream MCP server;
- the load balancer or client routing that sends traffic to the survivor.

Status: early. See [VISION.md](VISION.md) for the roadmap and GitHub Issues for planned work.

License: Apache-2.0.
