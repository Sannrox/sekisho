#!/usr/bin/env bash
# Build release artifacts into --out without publishing or attesting.
#
# Usage:
#   bash scripts/release-package.sh --out dist [--tag vX.Y.Z] [--git-sha SHA]
#        [--skip-build] [--skip-test] [--skip-github] [--with-images]
#        [--push-image ghcr.io/<owner>/sekisho]
#
# --push-image (needs --with-images and --tag, and a docker login) pushes the
# image as <repo>:<version> and renders the Tenkai Kubernetes product pinned
# to the pushed digest into sekisho-<version>-tenkai-kubernetes.tar.gz.
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
PUSH_IMAGE=""

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
    --push-image) PUSH_IMAGE="${2:-}"; shift 2 ;;
    *) fail "unknown argument: $1" ;;
  esac
done

[[ -n "$OUT" ]] || fail "usage: $0 --out dist [--tag vX.Y.Z] [--git-sha SHA]"
if [[ -n "$PUSH_IMAGE" ]]; then
  [[ "$WITH_IMAGES" -eq 1 && -n "$TAG" ]] || fail "--push-image needs --with-images and --tag"
  [[ "$PUSH_IMAGE" =~ ^[a-z0-9][a-z0-9./_-]*$ ]] || fail "--push-image takes a lowercase repository without a tag, got ${PUSH_IMAGE}"
fi
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
  if [[ -n "$PUSH_IMAGE" ]]; then
    remote="${PUSH_IMAGE}:${VERSION}"
    docker tag "$local_tag" "$remote"
    docker push "$remote"
    pushed="$(docker inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "$remote" | grep -m1 "^${PUSH_IMAGE}@sha256:" || true)"
    [[ -n "$pushed" ]] || fail "no registry digest for ${remote}"
    IMAGE_ARGS+=(--image "${remote}=${pushed##*@}")
    printf 'pushed %s as %s\n' "$remote" "$pushed"
    product="$(mktemp -d "${TMPDIR:-/tmp}/sekisho-product.XXXXXX")"
    bash "$ROOT_PATH/scripts/render-tenkai-product.sh" --image "$pushed" --version "$VERSION" --out "${product}/sekisho"
    tar -czf "${OUT}/sekisho-${VERSION}-tenkai-kubernetes.tar.gz" -C "$product" sekisho
    rm -rf "$product"
  fi
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
  # Gate flags cover exactly the pull requests merged into the release commit
  # since the nearest final tag before it: an rc tag never starts the range,
  # or a final release would drop gates first seen in its release candidates,
  # and a pull request merged after the tagged commit never counts, even when a
  # retry runs days later.
  prev="$(git describe --tags --abbrev=0 --match 'v*' --exclude '*-*' "${GIT_SHA}^" 2>/dev/null || true)"
  range="$GIT_SHA"
  query="merged:<=$(TZ=UTC git log -1 --date=format-local:%Y-%m-%d --format=%cd "$GIT_SHA")"
  if [[ -n "$prev" ]]; then
    range="${prev}..${GIT_SHA}"
    query="merged:>=$(TZ=UTC git log -1 --date=format-local:%Y-%m-%d --format=%cd "$prev") ${query}"
  fi
  while IFS= read -r label; do
    [[ -n "$label" ]] && LABEL_ARGS+=(--label "$label")
  done < <(
    gh pr list --state merged --limit 1000 --search "$query" --json mergeCommit,labels \
      --jq '.[] | select(.mergeCommit != null) | .mergeCommit.oid as $oid | .labels[] | "\($oid)\t\(.name)"' 2>/dev/null \
      | awk -F'\t' 'NR == FNR { in_range[$1] = 1; next } in_range[$1] { print $2 }' <(git rev-list "$range") - \
      | sort -u || true
  )
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
