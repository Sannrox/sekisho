#!/usr/bin/env bash
# Refuse a v* tag that is lightweight, unsigned, signed by an unknown key,
# or whose version does not equal the crate version.
#
# Usage:
#   bash scripts/verify-release-tag.sh verify <tag> [commit-sha]
#   bash scripts/verify-release-tag.sh self-test
set -euo pipefail
trap '' PIPE

SCRIPT_PATH="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/$(basename -- "${BASH_SOURCE[0]}")"
ROOT_PATH="${RELEASE_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}"
FINGERPRINTS_FILE="${RELEASE_TAG_FINGERPRINTS:-$ROOT_PATH/security/release-tag-fingerprints}"
KEY_FILE="${RELEASE_TAG_KEY:-$ROOT_PATH/security/release-tag-key.asc}"
TAG_RE='^v[0-9]+\.[0-9]+\.[0-9]+(-(rc|beta)\.[0-9]+)?$'

fail() {
  printf 'verify-release-tag: %s\n' "$1" >&2
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

expect_failure() {
  local message="$1" output
  shift
  if output="$(bash "$SCRIPT_PATH" verify "$@" 2>&1)"; then
    fail "expected verification failure containing: ${message}"
  fi
  [[ "$output" == *"$message"* ]] || fail "verification failed without expected message: ${message}"
}

self_test() {
  local tmp work gnupg fpr cargo_toml
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/sekisho-release-tag.XXXXXX")"
  work="${tmp}/work"
  gnupg="${tmp}/gpg"
  mkdir -m 700 "$gnupg"
  export GNUPGHOME="$gnupg"
  printf 'pinentry-mode loopback\n' >"${gnupg}/gpg.conf"
  printf 'allow-loopback-pinentry\n' >"${gnupg}/gpg-agent.conf"
  gpg --batch --pinentry-mode loopback --passphrase '' \
    --quick-generate-key "Release self-test <release-self-test@example.invalid>" ed25519 default never >/dev/null 2>&1
  fpr="$(gpg --batch --list-secret-keys --with-colons | awk -F: '/^fpr:/{print $10; exit}')"
  [[ -n "$fpr" ]] || fail "could not generate a self-test signing key"
  gpg --batch --armor --export "$fpr" >"${tmp}/key.asc"
  printf '%s\n' "$fpr" >"${tmp}/fingerprints"
  export RELEASE_TAG_KEY="${tmp}/key.asc"
  export RELEASE_TAG_FINGERPRINTS="${tmp}/fingerprints"

  git init -q "$work"
  git -C "$work" config user.name "Release tag self-test"
  git -C "$work" config user.email "release-self-test@example.invalid"
  git -C "$work" config commit.gpgsign false
  git -C "$work" config tag.gpgSign false
  git -C "$work" config user.signingkey "$fpr"
  git -C "$work" config gpg.program "$(command -v gpg)"
  cargo_toml="${work}/Cargo.toml"
  printf '[package]\nversion = "0.1.0"\n' >"$cargo_toml"
  git -C "$work" add Cargo.toml
  git -C "$work" commit -q -m first
  local first
  first="$(git -C "$work" rev-parse HEAD)"
  export RELEASE_ROOT="$work"

  git -C "$work" tag v0.1.0 "$first"
  (cd "$work" && expect_failure "is lightweight" v0.1.0 "$first")
  git -C "$work" tag -d v0.1.0 >/dev/null

  git -C "$work" tag -a v0.1.0 -m "unsigned" "$first"
  (cd "$work" && expect_failure "unsigned" v0.1.0 "$first")
  git -C "$work" tag -d v0.1.0 >/dev/null

  git -C "$work" tag -s v0.1.0 -m "signed" "$first"
  (cd "$work" && bash "$SCRIPT_PATH" verify v0.1.0 "$first" >/dev/null)

  printf 'AEDA618886BF0331105B9CFCE4DC1F4A579505FB\n' >"${tmp}/fingerprints"
  (cd "$work" && expect_failure "unknown key" v0.1.0 "$first")
  printf '%s\n' "$fpr" >"${tmp}/fingerprints"

  git -C "$work" tag -s v0.2.0 -m "wrong version" "$first"
  (cd "$work" && expect_failure "does not match crate version" v0.2.0 "$first")

  gpgconf --kill gpg-agent >/dev/null 2>&1 || true
  rm -rf "$tmp" || true
  printf 'self-test passed\n'
}

case "${1:-}" in
  verify)
    shift
    [[ $# -ge 1 && $# -le 2 ]] || fail "usage: $0 verify <tag> [commit-sha]"
    verify_release_tag "$1" "${2:-}"
    ;;
  self-test)
    [[ $# -eq 1 ]] || fail "usage: $0 self-test"
    self_test
    exit 0
    ;;
  *)
    fail "usage: $0 {verify <tag> [commit-sha]|self-test}"
    ;;
esac
