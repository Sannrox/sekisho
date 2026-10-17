#!/usr/bin/env bash
# Inspect, claim, or release the delivery lane of one GitHub Issue.
#
# A lane claim lives on GitHub so agents on different machines see it:
#   1. the branch <type>/<issue>, created from the default branch with an
#      atomic ref creation (POST /git/refs fails when the ref exists, so the
#      first claimant wins and every other claimant sees "claimed");
#   2. the authenticated login assigned to the Issue, visible in the Issue
#      list and timestamped by the Issue timeline.
# Open Pull Requests that reference the Issue and any branch matching
# */<issue> or */<issue>-* count as claims too.
#
# Usage:
#   bash issue-lane.sh check <issue> [--json]
#   bash issue-lane.sh claim <issue> [--dry-run]
#   bash issue-lane.sh release <issue> [--yes]
#
# Exit codes: 0 unclaimed or done; 3 claimed by another lane; 1 error.
# Requires an authenticated gh, git, and jq. Run inside the repository.
set -euo pipefail

STALE_HOURS=6

usage() {
  sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-2}"
}

fail() {
  printf 'issue-lane: %s\n' "$1" >&2
  exit 1
}

for tool in gh git jq; do
  command -v "$tool" >/dev/null 2>&1 || fail "missing required command: $tool"
done

command_name="${1:-}"
issue="${2:-}"
case "$command_name" in
  check | claim | release) ;;
  -h | --help | "") usage 0 ;;
  *) usage ;;
esac
printf '%s' "$issue" | grep -Eq '^[0-9]+$' || fail "Issue number required, got '${issue}'"
shift 2

json_output=false
dry_run=false
confirm=false
for arg in "$@"; do
  case "$arg" in
    --json) json_output=true ;;
    --dry-run) dry_run=true ;;
    --yes) confirm=true ;;
    *) usage ;;
  esac
done

repo="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
default_branch="$(gh repo view --json defaultBranchRef --jq .defaultBranchRef.name)"
login="$(gh api user --jq .login)"

issue_json="$(gh issue view "$issue" --json number,title,state,labels,assignees,closedByPullRequestsReferences)"
issue_state="$(printf '%s' "$issue_json" | jq -r .state)"
issue_title="$(printf '%s' "$issue_json" | jq -r .title)"

# Derive the lane type deterministically so every machine computes the same
# branch: Conventional Commit type in the title (bug -> fix), then the type
# label, then chore.
lane_type() {
  local from_title
  from_title="$(printf '%s' "$issue_title" | sed -nE 's/^([a-z]+)(\([^)]*\))?!?:.*/\1/p')"
  case "$from_title" in
    bug) printf 'fix' ; return ;;
    "") ;;
    *) printf '%s' "$from_title" ; return ;;
  esac
  if printf '%s' "$issue_json" | jq -e '.labels[] | select(.name == "bug")' >/dev/null; then
    printf 'fix'
  elif printf '%s' "$issue_json" | jq -e '.labels[] | select(.name == "enhancement")' >/dev/null; then
    printf 'feat'
  elif printf '%s' "$issue_json" | jq -e '.labels[] | select(.name == "documentation")' >/dev/null; then
    printf 'docs'
  else
    printf 'chore'
  fi
}
lane_branch="$(lane_type)/${issue}"

# Every branch named <anything>/<issue> or <anything>/<issue>-<slug>.
claim_branches_json() {
  gh api "repos/${repo}/git/matching-refs/heads/" --paginate \
    | jq -sc --arg issue "$issue" '
        [.[][] | .ref | ltrimstr("refs/heads/")
          | select(test("^[^/]+/" + $issue + "(-[^/]*)?$"))]'
}

# Open Pull Requests that GitHub links to the Issue or whose head is a claim branch.
open_prs_json() {
  local numbers branches
  numbers="$(printf '%s' "$issue_json" | jq -r '.closedByPullRequestsReferences[].number')"
  branches="$(printf '%s' "$1" | jq -r '.[]')"
  {
    for number in $numbers; do
      gh pr view "$number" --json number,state,isDraft,headRefName,updatedAt,url,author \
        | jq -c 'select(.state == "OPEN")'
    done
    for branch in $branches; do
      gh pr list --state open --head "$branch" --json number,state,isDraft,headRefName,updatedAt,url,author \
        | jq -c '.[]'
    done
  } | jq -sc 'unique_by(.number) | sort_by(.number)'
}

# Assignees plus the age of the latest assignment event in hours.
assignment_json() {
  local last_assigned now_epoch then_epoch age_hours
  last_assigned="$(gh api "repos/${repo}/issues/${issue}/timeline" --paginate \
    | jq -rs '[.[] | .[]? // empty] | map(select(.event == "assigned")) | sort_by(.created_at) | last | .created_at // empty')"
  age_hours=null
  if [ -n "$last_assigned" ]; then
    now_epoch="$(date -u +%s)"
    then_epoch="$(date -u -j -f '%Y-%m-%dT%H:%M:%SZ' "$last_assigned" +%s 2>/dev/null \
      || date -u -d "$last_assigned" +%s)"
    age_hours=$(( (now_epoch - then_epoch) / 3600 ))
  fi
  printf '%s' "$issue_json" | jq -c --arg at "${last_assigned:-}" --argjson age "$age_hours" \
    '{assignees: [.assignees[].login], last_assigned_at: (if $at == "" then null else $at end), age_hours: $age}'
}

report_json() {
  local branches prs assignment verdict hint
  branches="$(claim_branches_json)"
  prs="$(open_prs_json "$branches")"
  assignment="$(assignment_json)"
  verdict=unclaimed
  hint=""
  if [ "$issue_state" != "OPEN" ]; then
    verdict=closed
  elif [ "$(printf '%s' "$prs" | jq length)" -gt 0 ] || [ "$(printf '%s' "$branches" | jq length)" -gt 0 ]; then
    verdict=claimed
  elif [ "$(printf '%s' "$assignment" | jq '.assignees | length')" -gt 0 ]; then
    if [ "$(printf '%s' "$assignment" | jq '.age_hours // 0')" -lt "$STALE_HOURS" ]; then
      verdict=claimed
    else
      hint="assignment older than ${STALE_HOURS}h without a branch or Pull Request; ask the maintainer before claiming"
    fi
  fi
  jq -n \
    --arg repo "$repo" --argjson issue "$issue" --arg title "$issue_title" --arg state "$issue_state" \
    --arg lane_branch "$lane_branch" --argjson branches "$branches" --argjson prs "$prs" \
    --argjson assignment "$assignment" --arg verdict "$verdict" --arg hint "$hint" \
    '{repo: $repo, issue: $issue, title: $title, state: $state, lane_branch: $lane_branch,
      claim_branches: $branches, open_prs: $prs, assignment: $assignment,
      verdict: $verdict, hint: (if $hint == "" then null else $hint end)}'
}

print_report() {
  local report="$1"
  if [ "$json_output" = true ]; then
    printf '%s\n' "$report" | jq .
    return
  fi
  printf '%s\n' "$report" | jq -r '
    "Issue #\(.issue) [\(.state)] \(.title)",
    "lane branch: \(.lane_branch)",
    "claim branches: \(if (.claim_branches | length) == 0 then "none" else (.claim_branches | join(", ")) end)",
    "open PRs: \(if (.open_prs | length) == 0 then "none" else ([.open_prs[] | "#\(.number) \(if .isDraft then "draft" else "ready" end) \(.headRefName) by \(.author.login) updated \(.updatedAt)"] | join("; ")) end)",
    "assignees: \(if (.assignment.assignees | length) == 0 then "none" else (.assignment.assignees | join(", ")) + " (assigned \(.assignment.last_assigned_at // "unknown"), \(.assignment.age_hours // "?")h ago)" end)",
    "verdict: \(.verdict)\(if .hint then " — " + .hint else "" end)"'
}

verdict_exit() {
  case "$(printf '%s' "$1" | jq -r .verdict)" in
    unclaimed) return 0 ;;
    claimed) return 3 ;;
    *) return 1 ;;
  esac
}

case "$command_name" in
  check)
    report="$(report_json)"
    print_report "$report"
    verdict_exit "$report"
    ;;

  claim)
    report="$(report_json)"
    if ! verdict_exit "$report"; then
      print_report "$report" >&2
      printf 'issue-lane: refusing to claim #%s\n' "$issue" >&2
      exit 3
    fi
    base_sha="$(gh api "repos/${repo}/git/ref/heads/${default_branch}" --jq .object.sha)"
    printf 'claim plan: branch %s from %s@%s, assign %s to #%s\n' \
      "$lane_branch" "$default_branch" "${base_sha:0:12}" "$login" "$issue" >&2
    if [ "$dry_run" = true ]; then
      printf 'dry run; no GitHub mutation.\n' >&2
      exit 0
    fi
    if ! gh api -X POST "repos/${repo}/git/refs" \
      -f ref="refs/heads/${lane_branch}" -f sha="$base_sha" >/dev/null 2>&1; then
      printf 'issue-lane: could not create %s; another lane probably claimed #%s first:\n' \
        "$lane_branch" "$issue" >&2
      print_report "$(report_json)" >&2
      exit 3
    fi
    gh issue edit "$issue" --add-assignee "@me" >/dev/null
    jq -n --arg repo "$repo" --argjson issue "$issue" --arg branch "$lane_branch" \
      --arg base "$base_sha" --arg assignee "$login" --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      '{repo: $repo, issue: $issue, branch: $branch, base_sha: $base, assignee: $assignee, claimed_at: $at}'
    ;;

  release)
    report="$(report_json)"
    if [ "$(printf '%s' "$report" | jq '.open_prs | length')" -gt 0 ]; then
      print_report "$report" >&2
      fail "an open Pull Request exists; close it first or let it land"
    fi
    if printf '%s' "$report" | jq -e --arg b "$lane_branch" '.claim_branches | index($b)' >/dev/null; then
      ahead="$(gh api "repos/${repo}/compare/${default_branch}...${lane_branch}" --jq .ahead_by)"
      if [ "$ahead" -gt 0 ] && [ "$confirm" != true ]; then
        fail "${lane_branch} has ${ahead} commit(s) beyond ${default_branch}; pass --yes to delete it anyway"
      fi
      gh api -X DELETE "repos/${repo}/git/refs/heads/${lane_branch}" >/dev/null
      printf 'deleted branch %s\n' "$lane_branch" >&2
    fi
    if printf '%s' "$report" | jq -e --arg me "$login" '.assignment.assignees | index($me)' >/dev/null; then
      gh issue edit "$issue" --remove-assignee "@me" >/dev/null
      printf 'removed assignee %s from #%s\n' "$login" "$issue" >&2
    fi
    print_report "$(report_json)"
    ;;
esac
