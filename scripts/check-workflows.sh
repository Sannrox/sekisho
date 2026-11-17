#!/usr/bin/env bash
# Lint .github/workflows with a pinned workflow linter (actionlint) and a
# pinned workflow security auditor (zizmor). CI and local runs use the same
# binaries: each is downloaded once per version, checked against the SHA-256
# below, and cached under ${XDG_CACHE_HOME:-~/.cache}/check-workflows.
#
# Usage: bash scripts/check-workflows.sh
set -euo pipefail

ACTIONLINT_VERSION=1.7.12
ZIZMOR_VERSION=1.32.0

ROOT_PATH="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/check-workflows"

fail() {
  printf 'check-workflows: %s\n' "$1" >&2
  exit 1
}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    actionlint_asset=linux_amd64
    actionlint_sha=8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8
    zizmor_asset=x86_64-unknown-linux-gnu
    zizmor_sha=bb110cc765eca9cd7efb2e284a956be5974c60027b0970f16397d86208a9957b ;;
  Linux-aarch64 | Linux-arm64)
    actionlint_asset=linux_arm64
    actionlint_sha=325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6
    zizmor_asset=aarch64-unknown-linux-gnu
    zizmor_sha=cb152b207f815116d7fa1c1ee4614f61578803351395052908095e79adfd9fd0 ;;
  Darwin-arm64)
    actionlint_asset=darwin_arm64
    actionlint_sha=aba9ced2dee8d27fecca3dc7feb1a7f9a52caefa1eb46f3271ea66b6e0e6953f
    zizmor_asset=aarch64-apple-darwin
    zizmor_sha=6efed18c4539a663fd3c851a2be9533fd8456c4e646d9416206da059154a0d3c ;;
  Darwin-x86_64)
    actionlint_asset=darwin_amd64
    actionlint_sha=5b44c3bc2255115c9b69e30efc0fecdf498fdb63c5d58e17084fd5f16324c644
    zizmor_asset=x86_64-apple-darwin
    zizmor_sha=6983143c719ada6845a69cc4dd2bacc215f971662fecd8bbd2678388e5ce5969 ;;
  *) fail "no pinned linters for $(uname -s)-$(uname -m)" ;;
esac

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# fetch <name> <version> <url> <sha256>: print the path of the verified binary.
# Downloads into a private temp dir and renames into place, so concurrent runs
# never see a partial or unverified binary.
fetch() {
  local name="$1" version="$2" url="$3" want="$4" dir="${CACHE}/${1}-${2}" tmp got
  if [[ ! -x "${dir}/${name}" ]]; then
    mkdir -p "$CACHE"
    tmp="$(mktemp -d "${CACHE}/.${name}.XXXXXX")"
    curl -fsSL --retry 3 -o "${tmp}/archive.tar.gz" "$url" || { rm -rf "$tmp"; fail "cannot download ${url}"; }
    got="$(sha256 "${tmp}/archive.tar.gz")"
    [[ "$got" == "$want" ]] || { rm -rf "$tmp"; fail "${name} ${version} checksum ${got} does not match the pin ${want}"; }
    tar -xzf "${tmp}/archive.tar.gz" -C "$tmp" "$name" || { rm -rf "$tmp"; fail "cannot unpack ${name}"; }
    mkdir -p "${tmp}/out" && mv "${tmp}/${name}" "${tmp}/out/${name}"
    mv "${tmp}/out" "$dir" 2>/dev/null || [[ -x "${dir}/${name}" ]] || { rm -rf "$tmp"; fail "cannot install ${name} into ${dir}"; }
    rm -rf "$tmp"
  fi
  printf '%s\n' "${dir}/${name}"
}

actionlint="$(fetch actionlint "$ACTIONLINT_VERSION" \
  "https://github.com/rhysd/actionlint/releases/download/v${ACTIONLINT_VERSION}/actionlint_${ACTIONLINT_VERSION}_${actionlint_asset}.tar.gz" \
  "$actionlint_sha")"
zizmor="$(fetch zizmor "$ZIZMOR_VERSION" \
  "https://github.com/zizmorcore/zizmor/releases/download/v${ZIZMOR_VERSION}/zizmor-${zizmor_asset}.tar.gz" \
  "$zizmor_sha")"

cd "$ROOT_PATH"
"$actionlint"
# Offline: the same findings locally and in CI, without a token.
"$zizmor" --offline --no-progress .github/workflows
