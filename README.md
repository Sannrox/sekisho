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
sekisho serve --config sekisho.toml    # prints "listening on <addr>"
curl http://127.0.0.1:8090/healthz
curl http://127.0.0.1:8090/metrics
sekisho diagnostics --config sekisho.toml > bundle.json
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
