#!/usr/bin/env bash
# Kind acceptance for the Tenkai Kubernetes product (deploy/kubernetes).
# Local only: it needs docker, kind v0.24 or later (its default network
# enforces NetworkPolicy), kubectl, gh, and TENKAICTL naming a tenkaictl that
# understands compatibility profile v2.
#
# In a throwaway kind cluster with a local registry and an embedded Tenkai
# store, it checks that:
#   1. a consumer that requires `sekisho.mcp` is refused while no sekisho is installed;
#   2. the previous release's image installs and passes readiness, and then
#      the consumer is admitted and becomes ready by calling it;
#   3. only pods labeled `sekisho/caller: "true"` reach sekisho;
#   4. state written through the CLI survives a pod restart and an upgrade
#      to an image built from this checkout.
#
# Usage: TENKAICTL=/path/to/tenkaictl bash scripts/kind-product-check.sh
#   PREVIOUS_TAG=v0.1.0   release whose image tar is the upgrade start (default: latest release)
#   KEEP=1                keep the cluster, registry and work directory for inspection
set -euo pipefail

ROOT_PATH="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
RUN="sekisho-kind-$$-${RANDOM}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/${RUN}.XXXXXX")"
REGISTRY="${RUN}-registry"
CRANE="gcr.io/go-containerregistry/crane:v0.22.1"
ENV_NAME="local"

say() { printf '\n== %s\n' "$*"; }
fail() { printf 'kind-product-check: %s\n' "$*" >&2; exit 1; }

for tool in docker kind kubectl gh python3; do
  command -v "$tool" >/dev/null || fail "missing ${tool}"
done
[[ -x "${TENKAICTL:-}" ]] || fail "set TENKAICTL to a tenkaictl binary with compatibility profile v2"

cleanup() {
  if [[ "${KEEP:-0}" == 1 ]]; then
    printf 'kept cluster %s, registry %s, work dir %s\n' "$RUN" "$REGISTRY" "$WORK"
    return
  fi
  kind delete cluster --name "$RUN" >/dev/null 2>&1 || true
  docker rm -f "$REGISTRY" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

export TENKAI_DATABASE="${WORK}/tenkai.db"
# The embedded store's development token, as in Tenkai's local dogfood.
export TENKAI_MANAGEMENT_TOKEN="${TENKAI_MANAGEMENT_TOKEN:-tenkai-local-management}"
KUBECONFIG_FILE="${WORK}/kubeconfig"
tenkai() { "$TENKAICTL" "$@"; }
k() { kubectl --kubeconfig "$KUBECONFIG_FILE" -n "$ENV_NAME" "$@"; }

say "registry and cluster"
# The registry is reachable only on the kind network, by container name.
# Images are pushed from a container on that network, not by the docker
# daemon, whose localhost may sit behind a proxy.
docker run -d --name "$REGISTRY" registry:2 >/dev/null
cat >"${WORK}/kind.yaml" <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
  - |-
    [plugins."io.containerd.grpc.v1.cri".registry]
      config_path = "/etc/containerd/certs.d"
EOF
kind create cluster --name "$RUN" --kubeconfig "$KUBECONFIG_FILE" --config "${WORK}/kind.yaml" --wait 120s
docker network connect kind "$REGISTRY"
for node in $(kind get nodes --name "$RUN"); do
  docker exec "$node" mkdir -p "/etc/containerd/certs.d/${REGISTRY}:5000"
  printf 'server = "http://%s:5000"\n' "$REGISTRY" | docker exec -i "$node" cp /dev/stdin "/etc/containerd/certs.d/${REGISTRY}:5000/hosts.toml"
done

# Pushes an image tarball (docker save format) and prints its reference by digest.
push() {
  local tar="$1" name="${REGISTRY}:5000/sekisho:$2" ref
  ref="$(docker run --rm --network kind -v "$(dirname "$tar"):/images:ro" "$CRANE" \
    push --insecure "/images/$(basename "$tar")" "$name")"
  [[ "$ref" =~ ^${REGISTRY}:5000/sekisho@sha256:[0-9a-f]{64}$ ]] || fail "no registry digest for ${name}: ${ref}"
  printf '%s\n' "$ref"
}

say "images"
PREVIOUS_TAG="${PREVIOUS_TAG:-$(gh release view --repo Sannrox/sekisho --json tagName --jq .tagName)}"
gh release download "$PREVIOUS_TAG" --repo Sannrox/sekisho -p "sekisho-${PREVIOUS_TAG#v}-image.tar" -D "$WORK"
docker build -q -f "${ROOT_PATH}/Containerfile" -t "${RUN}-current:local" "$ROOT_PATH" >/dev/null
docker save -o "${WORK}/current-image.tar" "${RUN}-current:local"
docker rmi "${RUN}-current:local" >/dev/null
PREVIOUS_IMAGE="$(push "${WORK}/sekisho-${PREVIOUS_TAG#v}-image.tar" previous)"
CURRENT_IMAGE="$(push "${WORK}/current-image.tar" current)"
# Product versions are local labels; the images are what is being tested.
PREVIOUS_VERSION="0.0.0-kind.1"
CURRENT_VERSION="0.0.0-kind.2"
bash "${ROOT_PATH}/scripts/render-tenkai-product.sh" --image "$PREVIOUS_IMAGE" --version "$PREVIOUS_VERSION" --out "${WORK}/previous"
bash "${ROOT_PATH}/scripts/render-tenkai-product.sh" --image "$CURRENT_IMAGE" --version "$CURRENT_VERSION" --out "${WORK}/current"

say "consumer product"
CONSUMER_PIN="sha256:$(printf 'sekisho-kind-consumer' | shasum -a 256 | cut -d' ' -f1)"
mkdir -p "${WORK}/consumer/manifests"
cat >"${WORK}/consumer/tenkai.toml" <<EOF
[product]
name = "sekisho-consumer"
version = "0.1.0"
description = "Declares a requirement on sekisho.mcp and calls it"
kind = "software"

[deploy]
workdir = "."
executor = "kubernetes-inprocess"
inputs = ["manifests"]

[compatibility]
version = 2
max_evidence_age_ms = 86400000

[compatibility.components.sekisho-consumer.pin]
kind = "digest"
value = "${CONSUMER_PIN}"

[compatibility.components.sekisho-consumer.requires]
"sekisho.mcp" = "1.0.0"
EOF
# Ready only once it reaches sekisho, so the apply proves the policy admits callers.
cat >"${WORK}/consumer/manifests/10-deployment.yaml" <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: sekisho-consumer
spec:
  replicas: 1
  selector:
    matchLabels:
      app: sekisho-consumer
  template:
    metadata:
      labels:
        app: sekisho-consumer
        sekisho/caller: "true"
    spec:
      containers:
        - name: caller
          image: busybox:1.37
          command: ["sleep", "infinity"]
          readinessProbe:
            exec:
              command: ["wget", "-q", "-T", "2", "-O", "/dev/null", "http://sekisho:8090/readyz"]
            periodSeconds: 3
EOF

say "tenkai store"
tenkai init >/dev/null
tenkai env cluster-config set "$ENV_NAME" "$KUBECONFIG_FILE"
kubectl --kubeconfig "$KUBECONFIG_FILE" create namespace "$ENV_NAME"
# The operator-supplied config (deploy/kubernetes/README.md). The JWKS holds
# the RFC 8037 example public key; no tokens are minted here.
mkdir -p "${WORK}/config"
cat >"${WORK}/config/sekisho.toml" <<'EOF'
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
EOF
# An empty policy: no agent may call any tool.
printf 'agents = []\n' >"${WORK}/config/agents.toml"
printf '{"keys":[{"kty":"OKP","crv":"Ed25519","alg":"EdDSA","kid":"kind","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}]}\n' >"${WORK}/config/jwks.json"
k create secret generic sekisho-config --from-file="${WORK}/config/sekisho.toml" --from-file="${WORK}/config/jwks.json" --from-file="${WORK}/config/agents.toml"
for product in previous current consumer; do
  tenkai publish "${WORK}/${product}/tenkai.toml" --allow-unsigned-development
done

# Records owner evidence for one release: its component pin and contracts.
observe() {
  local release="$1" component="$2" pin="$3" requires="$4" provides="$5"
  local digest
  digest="$(tenkai release inspect "$release" | python3 -c 'import json,sys; print(json.load(sys.stdin)["manifest_digest"])')"
  python3 - "$ENV_NAME" "$digest" "$component" "$pin" "$requires" "$provides" >"${WORK}/evidence.json" <<'PY'
import json, sys, time
env, digest, component, pin, requires, provides = sys.argv[1:]
print(json.dumps({
    "version": 1,
    "environment": env,
    "release_digest": digest,
    "observed_at_ms": int(time.time() * 1000),
    "components": {component: {
        "pin": {"kind": "digest", "value": pin},
        "requires": json.loads(requires),
        "provides": json.loads(provides),
    }},
    "contracts": {},
    "capabilities": [],
    "migration": "stable",
}))
PY
  tenkai env compatibility record "$ENV_NAME" "${WORK}/evidence.json"
}
digest_of() { printf '%s' "${1##*@}"; }
observe "sekisho-consumer@0.1.0" sekisho-consumer "$CONSUMER_PIN" '{"sekisho.mcp": "1.0.0"}' '{}'
observe "sekisho@${PREVIOUS_VERSION}" sekisho "$(digest_of "$PREVIOUS_IMAGE")" '{}' '{"sekisho.mcp": "1.0.0"}'
observe "sekisho@${CURRENT_VERSION}" sekisho "$(digest_of "$CURRENT_IMAGE")" '{}' '{"sekisho.mcp": "1.0.0"}'

# Plans the environment and applies the plan; prints the plan output.
plan_apply() {
  local out id
  out="$(tenkai plan --env "$ENV_NAME")"
  printf '%s\n' "$out"
  id="$(printf '%s\n' "$out" | sed -n 's/^plan id: //p' | tail -1)"
  [[ -n "$id" ]] || fail "no plan id in plan output"
  tenkai apply "$id" --allow-unapproved-development --development-reason "kind product check"
}

# An explicit plan is satisfied only by a provider already deployed, so
# sekisho installs first and the consumer is planned after it.
say "1. consumer without sekisho is refused"
if refusal="$(tenkai env compatibility check "$ENV_NAME" "sekisho-consumer@0.1.0" 2>&1)"; then
  fail "the consumer was admitted without a sekisho.mcp provider: ${refusal}"
fi
if ! grep -q 'missing_contract' <<<"$refusal" || ! grep -q 'sekisho.mcp' <<<"$refusal"; then
  fail "refusal does not name the missing sekisho.mcp contract: ${refusal}"
fi
echo "refused as expected: missing_contract sekisho.mcp"

say "2. install ${PREVIOUS_TAG} image, then the consumer"
tenkai promote "sekisho@${PREVIOUS_VERSION}" stable
tenkai env subscribe "$ENV_NAME" sekisho=stable
plan_apply
k wait --for=condition=Ready pod/sekisho-0 --timeout=120s
k get pod sekisho-0 -o jsonpath='{.spec.containers[0].image}{"\n"}' | grep -qxF "$PREVIOUS_IMAGE" || fail "sekisho-0 does not run the previous image"
tenkai env compatibility check "$ENV_NAME" "sekisho-consumer@0.1.0"
tenkai promote "sekisho-consumer@0.1.0" stable
tenkai env subscribe "$ENV_NAME" sekisho-consumer=stable
plan_apply
k rollout status deployment/sekisho-consumer --timeout=120s

say "3. only declared callers reach sekisho"
if k run "not-a-caller-$$" --image=busybox:1.37 --restart=Never --rm -i --quiet --command -- \
  wget -q -T 3 -O /dev/null http://sekisho:8090/readyz 2>/dev/null; then
  fail "a pod without the caller label reached sekisho"
fi
k run "caller-$$" --image=busybox:1.37 --restart=Never --rm -i --quiet --labels='sekisho/caller=true' --command -- \
  wget -q -T 3 -O /dev/null http://sekisho:8090/readyz || fail "a labeled caller could not reach sekisho"
echo "unlabeled pod refused, labeled caller admitted"

sekisho() { k exec sekisho-0 -- /usr/local/bin/sekisho --config /etc/sekisho/sekisho.toml "$@"; }
kept() { sekisho mitigation list | grep -qF "$MITIGATION" || fail "mitigation ${MITIGATION} lost after $1"; echo "mitigation ${MITIGATION} kept after $1"; }

say "4. state survives restart and upgrade"
MITIGATION="$(sekisho mitigation apply --upstream kind-check --decision kind-71 --operator kind-product-check | tail -1)"
[[ -n "$MITIGATION" ]] || fail "mitigation apply printed no id"
k delete pod sekisho-0 --wait=true
k wait --for=condition=Ready pod/sekisho-0 --timeout=120s
kept restart
tenkai promote "sekisho@${CURRENT_VERSION}" stable
plan_apply
k wait --for=condition=Ready pod/sekisho-0 --timeout=120s
k get pod sekisho-0 -o jsonpath='{.spec.containers[0].image}{"\n"}' | grep -qxF "$CURRENT_IMAGE" || fail "sekisho-0 does not run the current image"
kept upgrade
k rollout status deployment/sekisho-consumer --timeout=60s

say "passed"
