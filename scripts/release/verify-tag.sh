#!/usr/bin/env bash
# Refuse a v* tag that is lightweight, unsigned, signed by an unknown key,
# or whose version does not equal the crate version.
#
# Usage:
#   bash scripts/release/verify-tag.sh verify <tag> [commit-sha]
set -euo pipefail
trap '' PIPE

ROOT_PATH="${RELEASE_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)}"
FINGERPRINTS_FILE="${RELEASE_TAG_FINGERPRINTS:-$ROOT_PATH/security/release-tag-fingerprints}"
KEY_FILE="${RELEASE_TAG_KEY:-$ROOT_PATH/security/release-tag-key.asc}"
TAG_RE='^v[0-9]+\.[0-9]+\.[0-9]+(-(rc|beta)\.[0-9]+)?$'

fail() {
  printf 'verify-tag: %s\n' "$1" >&2
  exit 1
}

normalize_fpr() {
  printf '%s' "$1" | tr '[:lower:]' '[:upper:]' | tr -d '[:space:]'
}

load_allowed_fingerprints() {
  local line
  ALLOWED_FPRS=()
  [[ -f "$FINGERPRINTS_FILE" ]] || fail "missing fingerprints file: ${FINGERPRINTS_FILE}"
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ -z "${line// }" ]] && continue
    ALLOWED_FPRS+=("$(normalize_fpr "$line")")
  done <"$FINGERPRINTS_FILE"
  [[ ${#ALLOWED_FPRS[@]} -gt 0 ]] || fail "no fingerprints in ${FINGERPRINTS_FILE}"
}

fingerprint_allowed() {
  local got="$1" allowed
  for allowed in "${ALLOWED_FPRS[@]}"; do
    [[ "$got" == "$allowed" ]] && return 0
  done
  return 1
}

crate_version() {
  python3 -c 'import re,sys; from pathlib import Path; text=Path(sys.argv[1]).read_text(); m=re.search(r"(?ms)^\[package\][^\[]*?^version\s*=\s*\"([^\"]+)\"", text); sys.exit("Cargo.toml has no [package] version") if not m else print(m.group(1))' "$ROOT_PATH/Cargo.toml"
}

import_allowed_key() {
  [[ -f "$KEY_FILE" ]] || fail "missing release-tag public key: ${KEY_FILE}"
  gpg --batch --import "$KEY_FILE" >/dev/null 2>&1 || fail "could not import ${KEY_FILE}"
}

verify_release_tag() {
  local tag="$1" expected_commit="${2:-}" objtype peeled raw status_fpr primary_fpr
  [[ "$tag" =~ $TAG_RE ]] || fail "invalid release tag: ${tag}"
  load_allowed_fingerprints
  import_allowed_key

  objtype="$(git cat-file -t "refs/tags/${tag}" 2>/dev/null)" || fail "tag ${tag} was not found"
  [[ "$objtype" == "tag" ]] || fail "tag ${tag} is lightweight; release tags must be annotated"

  if [[ -n "$expected_commit" ]]; then
    [[ "$expected_commit" =~ ^[0-9a-f]{40}$ ]] || fail "expected commit must be a full lowercase SHA"
    peeled="$(git rev-parse "${tag}^{commit}")"
    [[ "$peeled" == "$expected_commit" ]] || fail "tag ${tag} resolves to ${peeled}, expected ${expected_commit}"
  fi

  raw="$(git verify-tag --raw "$tag" 2>&1)" || true
  [[ "$raw" == *VALIDSIG* ]] || fail "tag ${tag} is unsigned or the signature is not valid"
  status_fpr="$(awk '/^\[GNUPG:\] VALIDSIG / { print $3; exit }' <<<"$raw")"
  primary_fpr="$(awk '/^\[GNUPG:\] VALIDSIG / { print $12; exit }' <<<"$raw")"
  status_fpr="$(normalize_fpr "$status_fpr")"
  primary_fpr="$(normalize_fpr "$primary_fpr")"
  fingerprint_allowed "$status_fpr" || fingerprint_allowed "$primary_fpr" || fail "tag ${tag} is signed by an unknown key"

  local version="${tag#v}" expected
  expected="$(crate_version)"
  [[ "$version" == "$expected" ]] || fail "tag ${tag} does not match crate version ${expected}"
  printf 'Release tag %s is annotated, signed, and matches crate version %s\n' "$tag" "$expected"
}

case "${1:-}" in
  verify)
    shift
    [[ $# -ge 1 && $# -le 2 ]] || fail "usage: $0 verify <tag> [commit-sha]"
    verify_release_tag "$1" "${2:-}"
    ;;
  *)
    fail "usage: $0 verify <tag> [commit-sha]"
    ;;
esac
