# Tenkai product on Kubernetes

This directory is the template for sekisho's Tenkai product. Each release
renders it with `scripts/render-tenkai-product.sh` and attaches the result
to the GitHub Release as `sekisho-<version>-tenkai-kubernetes.tar.gz`. Install
the rendered product. The template itself has placeholders and is not
installable.

## What it installs

- **StatefulSet `sekisho`**, one replica, image pinned by digest. Readiness is
  `/readyz`, which is ready only while every state store can do work, and
  while a configured token issuer or control plane can be reached.
  Liveness is `/healthz`.
- **Volume `data`** at `/var/lib/sekisho` (1 Gi, `ReadWriteOnce`). The claim
  outlives the pod, restarts, upgrades and an uninstall.
- **Service `sekisho`** on port 8090.
- **NetworkPolicy `sekisho`**: only pods in the same namespace labeled
  `sekisho/caller: "true"` reach port 8090. Add that label to each caller.

The product declares `executor = "kubernetes-inprocess"`, so Tenkai applies
`manifests/` with server-side apply into the environment's namespace.

## Contract

The compatibility profile (version 2) provides `sekisho.mcp` at `1.0.0`: the
`/mcp` endpoint, its OAuth protected-resource metadata, and the
`sekisho.*` discovery tools. A dependent declares, for example:

```toml
[compatibility.components.platform.requires]
"sekisho.mcp" = { minimum = "1.0.0", optional = true }
```

The component pin is the image digest, so owner evidence for a release
names that digest. [RELEASING.md](../../RELEASING.md#tenkai-product) says
when the contract version moves.

## Configuration

The product ships no configuration. Before the first apply, create Secret
`sekisho-config` in the environment namespace. It is mounted read-only at
`/etc/sekisho`, so `sekisho.toml` must be one of its keys, and so must every
file the config names relative to it (JWKS, agent policy, exchange client
secret, upstream tokens). `/readyz` stays unready until the policy loads; an
empty policy is `agents = []`. Change the policy by updating the Secret. Put
every state file on the volume with absolute paths:

```toml
listen = "0.0.0.0:8090"

[verifier]
kind = "jwks"
issuer = "https://issuer.example.test"
audience = "sekisho"
jwks_file = "jwks.json"

[mcp]
resource = "http://sekisho:8090/mcp"
authorization_servers = ["https://issuer.example.test"]
allowed_hosts = ["sekisho", "sekisho:8090"]

[registry]
file = "/var/lib/sekisho/registry.json"
audit_file = "/var/lib/sekisho/registry-audit.jsonl"
refresh_interval_ms = 30000

[policy]
file = "agents.toml"

[audit]
calls_file = "/var/lib/sekisho/calls.jsonl"

[mitigations]
file = "/var/lib/sekisho/mitigations.json"
audit_file = "/var/lib/sekisho/mitigations-audit.jsonl"
```

```sh
kubectl -n <environment> create secret generic sekisho-config \
  --from-file=sekisho.toml --from-file=jwks.json --from-file=agents.toml
```

Run owner commands in the pod, for example
`kubectl -n <environment> exec sekisho-0 -- /usr/local/bin/sekisho --config /etc/sekisho/sekisho.toml mitigation list`.

## Availability

One replica on the directory state backend. A lost pod is restarted on its
volume, and calls fail until `/readyz` is ready again. The release image is
built without the `postgres` feature, so this product does not run the
multi-replica layout from [High availability](../../README.md#high-availability).

## Acceptance check

`scripts/kind-product-check.sh` runs the product in a throwaway kind cluster
with a local registry and an embedded Tenkai store. It needs docker, kind
v0.24 or later (its default network enforces NetworkPolicy), kubectl, gh and
`TENKAICTL` pointing at a tenkaictl that understands profile v2. It checks that a consumer requiring `sekisho.mcp` is refused with no
provider and admitted once sekisho is installed, that only labeled callers
get through, and that state survives a pod restart and an upgrade from the
latest release's image to one built from the checkout.
