#!/usr/bin/env bash
# Validate this repository's agent-kit stubs against the latest Sannrox/agent-kit.
#
# Canonical source: Sannrox/agent-kit scripts/validate-workflow-skills.sh.
# Consuming repositories vendor this file byte-for-byte.
#
# Usage: bash scripts/validate-workflow-skills.sh [--render]
#
# Clones Sannrox/agent-kit into ${AGENT_KIT_HOME:-$HOME/.agents/agent-kit} when
# missing, otherwise fast-forwards it to the latest main (set AGENT_KIT_DIR to
# use a checkout as-is), then runs the kit's scripts/check-consumer.sh.
# --render rewrites stubs and vendored scripts from the kit.
set -euo pipefail

ROOT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

if [ -n "${AGENT_KIT_DIR:-}" ]; then
  kit="$AGENT_KIT_DIR"
else
  kit="${AGENT_KIT_HOME:-$HOME/.agents/agent-kit}"
  if [ -d "${kit}/.git" ]; then
    git -C "$kit" pull --quiet --ff-only origin main
  else
    mkdir -p "$(dirname "$kit")"
    git clone --quiet https://github.com/Sannrox/agent-kit "$kit"
  fi
fi
[ -f "${kit}/scripts/check-consumer.sh" ] || { echo "workflow skill check failed: ${kit} is not an agent-kit checkout" >&2; exit 1; }

bash "${kit}/scripts/check-consumer.sh" "$kit" "$ROOT_PATH" "$@"
printf 'workflow skill check passed\n'
