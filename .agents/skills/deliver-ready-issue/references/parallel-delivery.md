# Parallel delivery lanes

Use this reference when one lead runs several `deliver-ready-issue` lanes at
once, for example three ready Issues delivered by three agents on one or
several machines. `AGENTS.md` ("Parallel delivery lanes") owns the
policy: lane definition, GitHub claims, the three-lane limit, collision
surfaces, isolation, and serialization. This reference owns the lead procedure.

Every lane still runs `deliver-ready-issue` unchanged: one Issue, one claim
branch `<type>/<issue>`, one worktree or dedicated clone, one Pull Request, one
owner. The lead stays hands-on. It claims the lanes on GitHub, verifies
consequential conclusions, lands lanes one at a time, and may take over a
stalled lane. It is not an orchestration-only role.

## 1. Select lanes

1. Run `advance-issue-frontier` in report-only mode. Candidate lanes are the
   ready, mutually independent Issues it reports, in its presentation order,
   unless the user named the Issues explicitly.
2. Count live lanes on GitHub before adding any. Claims are what GitHub shows,
   not what this machine has checked out:

   ```sh
   gh pr list --state open --json number,headRefName,isDraft,updatedAt
   gh api repos/<owner>/<repo>/git/matching-refs/heads/ --paginate --jq '.[].ref'
   bash .agents/skills/deliver-ready-issue/scripts/issue-lane.sh check <issue>
   ```

   New lanes fit within the remaining capacity of the three-lane limit: open
   implementation Pull Requests plus claim branches without one. Do not count
   assigned or planned work as a running lane, and do not start a lane for an
   Issue whose check says `claimed`.
3. Reject candidate pairs that touch the same collision surface: ports and
   settings, governance adapters, the workspace jail, or the sekai-client proto
   pin. Run such Issues in sequence.
4. Resolve one authority ceiling for the run. Claiming needs Publish or Land,
   or an explicit claim authorization; without it the lanes are invisible to
   other machines and must be reported as such. Land authority still lands one
   lane at a time.

Stop and report when capacity, collisions, or authority are unclear. Selecting
fewer lanes is always acceptable; selecting colliding lanes is not.

## 2. Claim and open each lane

The lead claims every lane on GitHub before any worker starts, so that workers
on other machines and other leads see the lanes immediately:

```sh
bash .agents/skills/deliver-ready-issue/scripts/issue-lane.sh claim <issue>
```

The script prints the branch `<type>/<issue>`, the base SHA, and the assignee.
Record both in the ledger. Exit code 3 means another lane claimed the Issue
between the frontier run and now: drop the candidate, do not rename the branch,
and refill the slot from the frontier order if capacity remains.

Prepare isolation per machine. On a machine with an existing checkout the lead
adds the worktree, serialized with other shared Git mutations:

```sh
git fetch --prune origin
git worktree add --track -b <type>/<issue> .worktrees/issue-<issue> origin/<type>/<issue>
test "$(git -C .worktrees/issue-<issue> rev-parse HEAD)" = "<base>"
```

On a machine without a checkout, such as a cloud agent, the worker clones the
repository and checks out `<type>/<issue>`; that clone is its lane worktree
and must show the same base SHA before work starts.

Give every worker a lane brief with real values. Pasting an Issue number is
context, not the brief.

```text
Lane brief
- repository: <owner/name>
- Issue: <url> — <title>
- authority ceiling: <Implement | Publish | Land>
- claim branch: <type>/<issue> (already exists on GitHub; you are assigned)
- base SHA: <base>
- lane worktree: .worktrees/issue-<issue> (repo-relative; never an absolute
  path or hostname), or a fresh clone of the repository on the claim branch
- reserved by sibling lanes, do not touch: <surfaces or paths, or none>
- procedure: run deliver-ready-issue from "3. Bound the implementation"
  onward inside the worktree; readiness, claim, and isolation are done.
- publish only to the claim branch, with
  scripts/gh-verified-push.sh --branch <type>/<issue> --sync-local; open the
  Pull Request as a draft with "Closes #<issue>" and public lane fields
  (branch, repo-relative worktree, base SHA, published SHA, authority). Do
  not paste this internal brief, hostnames, or absolute paths onto GitHub.
  Mark the PR ready when verified.
- on a shared machine, fetch --prune, worktree add/remove, branch delete, and
  merge are lead-only; commit and publish only your own branch from your own
  worktree. Never enter the primary checkout or another lane's worktree.
- stop and report instead of continuing when: a dependency turns out to be
  open, the Issue needs a split, you need a reserved surface, verification
  needs a service that is unavailable, or the ceiling would be exceeded.
- report: the deliver-ready-issue "Report completion" contract, plus the
  final HEAD SHA and the exact verification commands you ran.
```

## 3. Run and monitor

1. Verify each worker started on the claim branch at the recorded base before
   accepting any later report.
2. Read worker reports as evidence, not as completion. Inspect the diff and the
   verification output of every lane before publishing or landing it.
3. Preserve a stalled lane's branch, Pull Request, and evidence before taking it
   over. Continue on the same claim branch; do not open a second lane for the
   same Issue. Another machine's lane looks the same from GitHub, so a lane the
   lead did not claim is not the lead's to take over without the maintainer.
4. A blocked lane is reported and its claim kept until the maintainer decides.
   Never repurpose its branch or worktree for another Issue. A lane abandoned
   before any commit is released with
   `bash .agents/skills/deliver-ready-issue/scripts/issue-lane.sh release <issue>`.

## 4. Publish and land in sequence

1. Each lane publishes to its claim branch from its own worktree with
   `scripts/gh-verified-push.sh --branch <type>/<issue> --sync-local` under
   Publish authority, after `git fetch --prune origin` and
   `git merge-base --is-ancestor origin/main HEAD` pass. Publishing may happen
   concurrently; landing never.
2. Land one lane at a time under Land authority:
   `gh pr merge <pr> --squash --delete-branch --match-head-commit <verified-sha>`.
   A failed or timed-out merge response may still have merged; reconcile the
   remote state before retrying.
3. After each landing, run `git fetch --prune origin` and recompute the
   frontier. Ask the remaining lanes to check `mergeable` and their tests
   against the new `main`. Refresh a lane onto `main` only for a conflict, a
   failing gate, an explicit request, or a sibling landing on a shared surface.
4. Remove the landed lane's local state, serialized with the other shared
   mutations on that machine:

   ```sh
   git worktree remove .worktrees/issue-<issue>
   git branch -D <type>/<issue>
   git worktree prune
   ```

   Because `main` is squash-merged, `git branch --merged` cannot detect a
   landed lane; confirm with `gh pr view <pr> --json state,mergeCommit` first.
   The merge already deleted the claim branch on GitHub.

## 5. Report

Keep one ledger for the run and return it with the final report:

| Issue | Branch | Base SHA | Owner | State | PR | Evidence | Blockers | Cleanup |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |

States are `claimed`, `implementing`, `verified`, `published`, `landed`,
`blocked`, `released`, and `handed over`. Report verified outcomes with links,
not launched work. Name every lane that is still open, who owns it, and what
unblocks it. Keep hostnames and absolute checkout paths in the session with
the maintainer, never on GitHub.

## Boundaries

- Never split one Issue across lanes or run two lanes for one Issue.
- Never claim with a differently named branch after losing the race, and never
  remove another lane's assignment or branch to make room.
- Never switch, reset, stash, or clean the primary checkout or another lane's
  worktree.
- Never exceed the three-lane limit or the run's authority ceiling.
- Never land two lanes concurrently or land a lane whose sibling changed a
  shared surface without re-verifying it.
- Never delete a worktree or branch you do not own without the maintainer's
  confirmation.
- Never put hostnames, FQDNs, home directories, absolute worktree paths, LAN
  or employer network names, or other private environment inventory on public
  Issues, Pull Requests, comments, or commit messages.
