#!/usr/bin/env bash
# List the changes merged to main since the last release tag, newest first,
# as a starting point for the release owner's CHANGELOG.md section. Pull
# requests do not edit CHANGELOG.md; their bodies carry the release notes.
set -euo pipefail

ROOT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "${ROOT_PATH}"

git fetch --quiet --tags origin main
since="${1:-$(git describe --tags --abbrev=0 origin/main 2>/dev/null || git rev-list --max-parents=0 origin/main)}"
echo "## Unreleased (since ${since})"
echo
git log --first-parent --format='- %s' "${since}..origin/main"
