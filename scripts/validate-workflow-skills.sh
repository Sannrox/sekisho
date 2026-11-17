#!/usr/bin/env bash
# Validate this repository's agent-kit stubs against the latest Sannrox/agent-kit.
#
# Canonical source: Sannrox/agent-kit scripts/validate-workflow-skills.sh.
# Consuming repositories vendor this file byte-for-byte.
#
# Usage: bash scripts/validate-workflow-skills.sh [--render]
#
# Clones Sannrox/agent-kit into ${AGENT_KIT_HOME:-$HOME/.agents/agent-kit} when
# missing, otherwise fetches the latest main: a checkout on main fast-forwards,
# and one on any other branch (or detached) stays untouched while the check
# runs against a snapshot of the fetched main. Set AGENT_KIT_DIR to use a
# checkout as-is. Then runs the kit's scripts/check-consumer.sh.
# --render rewrites stubs and vendored scripts from the kit.
set -euo pipefail

ROOT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

if [ -n "${AGENT_KIT_DIR:-}" ]; then
  kit="$AGENT_KIT_DIR"
else
  kit="${AGENT_KIT_HOME:-$HOME/.agents/agent-kit}"
  if [ -d "${kit}/.git" ]; then
    git -C "$kit" fetch --quiet origin main
    if [ "$(git -C "$kit" symbolic-ref --quiet --short HEAD || true)" = main ]; then
      git -C "$kit" merge --quiet --ff-only FETCH_HEAD
    else
      snapshot="$(mktemp -d "${TMPDIR:-/tmp}/agent-kit-main.$$.XXXXXX")"
      trap 'rm -rf "$snapshot"' EXIT
      git -C "$kit" archive FETCH_HEAD | tar -x -C "$snapshot"
      kit="$snapshot"
    fi
  else
    mkdir -p "$(dirname "$kit")"
    git clone --quiet https://github.com/Sannrox/agent-kit "$kit"
  fi
fi
[ -f "${kit}/scripts/check-consumer.sh" ] || { echo "workflow skill check failed: ${kit} is not an agent-kit checkout" >&2; exit 1; }

bash "${kit}/scripts/check-consumer.sh" "$kit" "$ROOT_PATH" "$@"
printf 'workflow skill check passed\n'
