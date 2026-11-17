#!/usr/bin/env bash
# The only script that changes a GitHub Release. Every command first refuses a
# published release, so "Re-run failed jobs" can resume a draft but never
# rewrite what was published. A tag with a "-" (vX.Y.Z-rc.N) is a pre-release
# and is never marked latest. Needs GH_TOKEN and GH_REPO.
#
# Usage:
#   bash scripts/release/github-release.sh guard <tag>
#       Exit 0 when the release is a draft or does not exist yet.
#   bash scripts/release/github-release.sh draft <tag> <notes> <asset>...
#       Create the draft, or resume it: drop the earlier run's assets, upload
#       these, and reset title, notes, and pre-release state.
#   bash scripts/release/github-release.sh publish <tag>
#       Publish the draft.
set -euo pipefail

fail() {
  printf 'github-release: %s\n' "$1" >&2
  exit 1
}

# Prints "draft" or "missing"; fails on a published release or a lookup error.
# A token that cannot see drafts reports a draft as not found.
release_state() {
  local tag="$1" state
  if state="$(gh release view "$tag" --json isDraft --jq .isDraft 2>&1)"; then
    [[ "$state" == true ]] || fail "release ${tag} is already published; cut a new tag instead of re-running"
    echo draft
  elif [[ "$state" == *"release not found"* ]]; then
    echo missing
  else
    fail "cannot read release ${tag}: ${state}"
  fi
}

# Sets KIND to the pre-release flags for a tag.
release_kind() {
  [[ "$1" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-(rc|beta)\.[0-9]+)?$ ]] || fail "invalid release tag: $1"
  KIND=()
  if [[ "$1" == *-* ]]; then
    KIND=(--prerelease --latest=false)
  fi
}

cmd="${1:-}"
[[ $# -ge 1 ]] && shift
case "$cmd" in
  guard)
    [[ $# -eq 1 ]] || fail "usage: $0 guard <tag>"
    release_state "$1" >/dev/null
    ;;
  draft)
    [[ $# -ge 3 ]] || fail "usage: $0 draft <tag> <notes> <asset>..."
    tag="$1" notes="$2"
    shift 2
    release_kind "$tag"
    state="$(release_state "$tag")"
    if [[ "$state" == draft ]]; then
      # Every run rebuilds every asset; drop the earlier run's so a renamed or
      # removed asset is not published.
      names="$(gh release view "$tag" --json assets --jq '.assets[].name')"
      while IFS= read -r name; do
        [[ -n "$name" ]] && gh release delete-asset "$tag" "$name" --yes
      done <<<"$names"
      gh release upload "$tag" "$@" --clobber
      gh release edit "$tag" --title "$tag" --notes-file "$notes" --draft ${KIND[@]+"${KIND[@]}"}
    else
      gh release create "$tag" "$@" --draft --verify-tag ${KIND[@]+"${KIND[@]}"} \
        --title "$tag" --notes-file "$notes"
    fi
    ;;
  publish)
    [[ $# -eq 1 ]] || fail "usage: $0 publish <tag>"
    tag="$1"
    release_kind "$tag"
    state="$(release_state "$tag")"
    [[ "$state" == draft ]] || fail "release ${tag} has no draft to publish"
    gh release edit "$tag" --draft=false ${KIND[@]+"${KIND[@]}"}
    echo "Published GitHub Release ${tag} from CHANGELOG"
    echo "Image tags: version only, never latest"
    ;;
  *)
    fail "usage: $0 {guard|draft|publish} <tag> ..."
    ;;
esac
