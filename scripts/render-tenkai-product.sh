#!/usr/bin/env bash
# Render the Tenkai Kubernetes product (deploy/kubernetes) for one image.
# Fills in the product version and pins the image, and the profile's
# component, to the image's registry digest. A tag is refused.
#
# Usage:
#   bash scripts/render-tenkai-product.sh --image <repo>@sha256:<hex> --out DIR
#        [--version X.Y.Z]   # default: the crate version in Cargo.toml
set -euo pipefail

ROOT_PATH="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
  printf 'render-tenkai-product: %s\n' "$1" >&2
  exit 1
}

IMAGE=""
OUT=""
VERSION=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image) IMAGE="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --version) VERSION="${2:-}"; shift 2 ;;
    *) fail "unknown argument: $1" ;;
  esac
done

[[ -n "$IMAGE" && -n "$OUT" ]] || fail "usage: $0 --image <repo>@sha256:<hex> --out DIR [--version X.Y.Z]"
[[ "$IMAGE" =~ ^[a-z0-9][a-z0-9./:_-]*@sha256:[0-9a-f]{64}$ ]] || fail "image must be <repo>@sha256:<64 hex>, got ${IMAGE}"
if [[ -z "$VERSION" ]]; then
  VERSION="$(python3 -c 'import re,sys; t=open(sys.argv[1]).read(); m=re.search(r"(?ms)^\[package\][^\[]*?^version\s*=\s*\"([^\"]+)\"", t); sys.exit("Cargo.toml has no [package] version") if not m else print(m.group(1))' "$ROOT_PATH/Cargo.toml")"
fi
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || fail "version must be X.Y.Z[-pre], got ${VERSION}"
if [[ -e "$OUT" ]] && [[ -n "$(ls -A "$OUT")" ]]; then
  fail "output directory ${OUT} is not empty"
fi

mkdir -p "$OUT"
cp -R "$ROOT_PATH/deploy/kubernetes/tenkai.toml" "$ROOT_PATH/deploy/kubernetes/manifests" "$OUT/"

python3 - "$OUT" "$VERSION" "$IMAGE" <<'PY'
import pathlib, re, sys
out, version, image = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
values = {"@VERSION@": version, "@IMAGE@": image, "@IMAGE_DIGEST@": image.rsplit("@", 1)[1]}
for path in sorted(p for p in out.rglob("*") if p.is_file()):
    text = path.read_text()
    for key, value in values.items():
        text = text.replace(key, value)
    left = re.findall(r"@[A-Z_]+@", text)
    if left:
        sys.exit(f"render-tenkai-product: {path.name} keeps unknown placeholders {sorted(set(left))}")
    path.write_text(text)
PY

printf 'rendered sekisho %s for %s into %s\n' "$VERSION" "$IMAGE" "$OUT"
