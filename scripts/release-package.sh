#!/usr/bin/env bash
# Build release artifacts into --out without publishing or attesting.
#
# Usage:
#   bash scripts/release-package.sh --out dist [--tag vX.Y.Z] [--git-sha SHA]
#        [--skip-build] [--skip-test] [--skip-github] [--with-images]
set -euo pipefail

ROOT_PATH="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_PATH"

OUT=""
TAG=""
GIT_SHA=""
SKIP_BUILD=0
SKIP_TEST=0
SKIP_GITHUB=0
WITH_IMAGES=0

fail() {
  printf 'release-package: %s\n' "$1" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out) OUT="${2:-}"; shift 2 ;;
    --tag) TAG="${2:-}"; shift 2 ;;
    --git-sha) GIT_SHA="${2:-}"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --skip-test) SKIP_TEST=1; shift ;;
    --skip-github) SKIP_GITHUB=1; shift ;;
    --with-images) WITH_IMAGES=1; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done

[[ -n "$OUT" ]] || fail "usage: $0 --out dist [--tag vX.Y.Z] [--git-sha SHA]"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

crate_version() {
  python3 -c 'import re,sys; from pathlib import Path; text=Path(sys.argv[1]).read_text(); m=re.search(r"(?ms)^\[package\][^\[]*?^version\s*=\s*\"([^\"]+)\"", text); sys.exit("Cargo.toml has no [package] version") if not m else print(m.group(1))' "$ROOT_PATH/Cargo.toml"
}

host_triple() {
  local info
  info="$(rustc -vV)"
  awk -F': ' '/^host:/{print $2; exit}' <<<"$info"
}

VERSION="$(crate_version)"
if [[ -n "$TAG" ]]; then
  [[ "${TAG#v}" == "$VERSION" ]] || fail "tag ${TAG} does not match crate version ${VERSION}"
  python3 "$ROOT_PATH/scripts/changelog-notes.py" "$VERSION" >"${OUT}/RELEASE_NOTES.md"
fi
if [[ -z "$GIT_SHA" ]]; then
  GIT_SHA="$(git rev-parse HEAD)"
fi
[[ "$GIT_SHA" =~ ^[0-9a-f]{40}$ ]] || fail "git sha must be a full lowercase SHA"

if [[ "$SKIP_TEST" -eq 0 ]]; then
  cargo test --locked
fi

TRIPLE="$(host_triple)"
if [[ "$SKIP_BUILD" -eq 0 ]]; then
  cargo build --release --locked --bin sekisho
  src="${ROOT_PATH}/target/release/sekisho"
  [[ -f "$src" ]] || fail "missing binary ${src}"
  dest="${OUT}/sekisho-${VERSION}-${TRIPLE}"
  cp "$src" "$dest"
  if command -v strip >/dev/null 2>&1 && [[ "$TRIPLE" == *-linux-* ]]; then
    strip "$dest" || true
  fi
fi

IMAGE_ARGS=()
if [[ "$WITH_IMAGES" -eq 1 ]]; then
  command -v docker >/dev/null 2>&1 || fail "docker is required for --with-images"
  local_tag="sekisho:v${VERSION}"
  docker build -f Containerfile -t "$local_tag" "$ROOT_PATH"
  if docker images --format '{{.Repository}}:{{.Tag}}' | grep -E '^sekisho:latest$'; then
    fail "must not produce a latest image tag"
  fi
  digest="$(docker image inspect --format '{{.Id}}' "$local_tag")"
  [[ "$digest" == sha256:* ]] || fail "image ${local_tag} has no sha256 id"
  docker save -o "${OUT}/sekisho-${VERSION}-image.tar" "$local_tag"
  IMAGE_ARGS+=(--image "${local_tag}=${digest}")
  printf 'image %s digest %s\n' "$local_tag" "$digest"
fi

python3 "$ROOT_PATH/scripts/release-sbom.py" "${OUT}/sekisho.cdx.json"

python3 -c '
import hashlib, pathlib, sys
out = pathlib.Path(sys.argv[1])
lines = []
for path in sorted(p for p in out.iterdir() if p.is_file() and p.name != "SHA256SUMS"):
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    lines.append(f"{digest}  {path.name}")
(out / "SHA256SUMS").write_text("\n".join(lines) + ("\n" if lines else ""))
' "$OUT"

LABEL_ARGS=()
if [[ -n "$TAG" && "$SKIP_GITHUB" -eq 0 ]] && command -v gh >/dev/null 2>&1; then
  prev="$(git tag --list 'v*' --sort=-version:refname)"
  prev="$(awk -v cur="$TAG" '$0 != cur { print; exit }' <<<"$prev")"
  search=()
  if [[ -n "$prev" ]]; then
    since="$(git log -1 --format=%cs "$prev" 2>/dev/null || true)"
    if [[ -n "$since" ]]; then
      search+=(--search "merged:>=${since}")
    fi
  fi
  while IFS= read -r label; do
    [[ -n "$label" ]] && LABEL_ARGS+=(--label "$label")
  done < <(gh pr list --state merged --limit 100 "${search[@]}" --json labels --jq '.[].labels[].name' 2>/dev/null || true)
fi

python3 "$ROOT_PATH/scripts/release-manifest.py" \
  --component sekisho \
  --version "$VERSION" \
  --git-sha "$GIT_SHA" \
  ${TAG:+--tag "$TAG"} \
  --dist "$OUT" \
  --out "${OUT}/sekisho-release.json" \
  "${IMAGE_ARGS[@]}" \
  "${LABEL_ARGS[@]}"

echo "version=${VERSION}"
echo "triple=${TRIPLE}"
echo "artifacts in ${OUT}:"
cat "${OUT}/SHA256SUMS"
if [[ ${#IMAGE_ARGS[@]} -eq 0 ]]; then
  echo "images: none in this package"
else
  echo "images: ${#IMAGE_ARGS[@]} (no latest tag)"
fi
