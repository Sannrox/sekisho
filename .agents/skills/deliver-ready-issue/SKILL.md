---
name: deliver-ready-issue
description: Deliver a dependency-ready sekisho GitHub Issue through a bounded implementation workflow in an isolated worktree lane. Use when asked to implement, publish, or land a specific ready Issue, to take the next explicitly approved frontier item through verification and review, or to run several ready Issues as parallel lanes.
---

# Deliver Ready Issue

Take one approved Issue from readiness check to the highest delivery stage the
user authorized. Keep the Issue as planning truth and the Pull Request as
implementation truth. A pasted Issue reference is context, not authority to
publish or to widen the task.

One run of this Skill is one delivery lane: one Issue, one claim branch
`<type>/<issue>` on GitHub, one worktree, one Pull Request, one owner. Claims
live on GitHub because lanes may run on different machines; worktrees only
isolate lanes that share a machine. To deliver several ready Issues at once,
the lead follows [references/parallel-delivery.md](references/parallel-delivery.md)
and runs this Skill once per lane. [scripts/issue-lane.sh](scripts/issue-lane.sh)
inspects, claims, and releases a lane deterministically.

## Establish the authority ceiling

Infer the ceiling from the user's explicit request. When it is unclear, choose
the lower ceiling and state what remains:

- **Implement**: change the local working tree and verify it.
- **Publish**: implement, commit, push, and open a ready Pull Request.
- **Land**: publish, resolve review and CI, then merge and clean up.

Permission for a higher stage includes its preceding stages. It never includes
unrelated issue creation, prioritization, assignment, release publication,
force-pushing protected branches, or weakening repository protections.

## Deliver the Issue

### 1. Prove readiness

1. Resolve the exact repository and Issue. If the user asks for the "next"
   Issue, require an explicit selection or a recommendation produced by
   `advance-issue-frontier` before starting.
2. Read the repository instructions, the Issue, linked decisions, and the live
   Pull Request and Issue state.
3. Read `## GitHub dependencies` literally. Treat an open predecessor or an
   unresolved non-Issue dependency as blocking.
4. Check the claim state on GitHub, the only state other machines share:

   ```sh
   bash .agents/skills/deliver-ready-issue/scripts/issue-lane.sh check <issue>
   ```

   It reports open Pull Requests referencing the Issue, claim branches
   (`<type>/<issue>`, `*/<issue>-*`), assignees with the assignment age, and
   a verdict; exit code 3 is `claimed`. A claim means another lane owns the
   Issue: continue only when the user directs the takeover, and preserve the
   existing branch, Pull Request, and evidence first. An assignment older than
   six hours without branch or Pull Request is reported as a hint; ask the
   maintainer before claiming. Then inspect this machine with
   `git worktree list --porcelain` and `git branch --list '*/<issue>*'`; a
   local lane for the Issue is somebody's isolation, not yours.
5. Confirm that the Issue is open, unblocked, focused enough for one Pull
   Request, and has testable acceptance criteria.

Stop without creating a branch when readiness, ownership, or dependencies are
ambiguous. Report the smallest action that would unblock delivery.

### 2. Isolate the work

1. Inspect `git status -sb` and `git worktree list`. Preserve every unrelated
   change, branch, worktree, and running process. Never switch, reset, stash,
   or clean the primary checkout or another lane's worktree.
2. Claim the Issue on GitHub before implementing, under Publish or Land
   authority or an explicit claim authorization:

   ```sh
   bash .agents/skills/deliver-ready-issue/scripts/issue-lane.sh claim <issue>
   ```

   The script derives `<type>` from the Issue, creates `<type>/<issue>` on
   GitHub from the default branch with an atomic ref creation, assigns you, and
   prints the branch and base SHA. Exit code 3 means another machine claimed
   first: stop and report; do not create a differently named branch. Under an
   Implement-only ceiling, do not claim; state in the report that the Issue
   stays unclaimed and invisible to other machines, or ask for claim
   authority first.
3. Check the claim branch out in the lane worktree and confirm the base SHA:

   ```sh
   git fetch --prune origin
   git worktree add --track -b <type>/<issue> .worktrees/issue-<issue> origin/<type>/<issue>
   git -C .worktrees/issue-<issue> rev-parse HEAD
   ```

   On a machine without an existing checkout, a fresh clone on the claim branch
   is the lane worktree. Do all further work inside it. If the worktree already
   exists, another lane on this machine owns it; return to step 1.4.
4. When other lanes are active on the machine, serialize shared Git mutations:
   `fetch --prune`, worktree creation or removal, local branch deletion, and
   merges happen one at a time and never while a sibling lane runs them.
   Committing and publishing your own branch from your own worktree is not a
   shared mutation.

Never discard, overwrite, stash, or commit unrelated work merely to obtain a
clean tree, and never reuse a worktree for a different Issue. Beyond the
branch and the assignment the script creates, do not comment on or otherwise
mark the Issue unless documented maintainer policy requires it.

### 3. Bound the implementation

1. Run `assess-change-impact` against the Issue and affected paths.
2. Translate the Issue acceptance criteria into code, test, documentation,
   migration, configuration, and security obligations.
3. Implement one coherent outcome. Avoid opportunistic cleanup.
4. If the Issue cannot produce one reviewable Pull Request, stop and recommend
   a split. Do not create follow-up Issues without authorization.

### 4. Verify and review

1. Add focused deterministic tests while implementing.
2. Run `verify-change` and retain exact commands, results, skips, and remaining
   uncertainty. At minimum for ship: `cargo fmt --check`, focused/`cargo test`,
   and `cargo clippy --all-targets -- -D warnings` (offline; no plane required).
3. Run **`autoreview` before committing** (or before push if already committed).
   Do **not** vendor the skill into this repo. Resolve the shared helper (env
   `AUTOREVIEW`, else agent-skills / `AGENTS_HOME` / sekai-chisei path—see
   `AGENTS.md` “Agent delivery closeout”). Do not skip because CI is green:

   ```bash
   # After commit on a topic branch (preferred):
   "$AUTOREVIEW" --mode branch --base origin/main
   # Uncommitted work only:
   "$AUTOREVIEW" --mode local
   ```

   Read the shared autoreview `SKILL.md` next to the helper for engines,
   findings policy, and when to stop. Fix actionable findings; rerun tests and
   autoreview until the helper exits 0 with no accepted/actionable findings, or
   document a maintainer-judgment blocker in the PR body.
4. Inspect the final diff for scope, generated artifacts, secrets, and
   accidental runtime state.
5. In the PR description (or implement-only handoff), report verify-change
   evidence **and** the autoreview command + clean/fixed/rejected outcome.

### 5. Publish when authorized

1. Stage only the intended paths and create a narrow imperative commit.
   Never use `--no-gpg-sign`. If signing fails, stop and fix GPG.
   Immediately before publishing, run `git fetch --prune origin` and
   `git merge-base --is-ancestor origin/main HEAD`. If the check fails, refresh
   the branch onto `main` and rerun the affected checks. Otherwise refresh only
   for a failing gate, an explicit request, or a sibling lane that landed on a
   shared surface (ports and settings, governance adapters, the workspace jail,
   the sekai-client proto pin, or the kekkai pin). Do not rebase merely because `main` advanced.
2. Publish to the claim branch with **GitHub-verified** commits via
   `scripts/gh-verified-push.sh` (GraphQL `createCommitOnBranch`), not a plain
   `git push`, unless the user explicitly asks for git-protocol push:
   - Claim branch (already exists on GitHub):
     `scripts/gh-verified-push.sh --branch <type>/<issue> --sync-local`
   - A branch that does not exist yet (no claim was possible):
     `scripts/gh-verified-push.sh --create-branch-from origin/main --branch <type>/<issue> --sync-local`
   - Confirm the script reports `verification.verified=true` and that the
     hosted tree matches local `HEAD`.
3. Open the Pull Request as a draft with the first published commit, then mark
   it ready once verification and review are complete and `mergeable` is no
   longer `UNKNOWN` (`gh pr ready <pr>`). The Pull Request:
   - closes the Issue with a visible `Closes #<issue>` line;
   - may list public lane fields only: claim branch, repo-relative worktree
     (for example `.worktrees/issue-N` — never an absolute path), base SHA,
     published SHA, authority ceiling. Never hostname, FQDN, home path,
     absolute worktree path, LAN or employer network name, or other
     environment inventory;
   - summarizes behavior rather than file operations;
   - lists verification evidence, autoreview closeout, and any skipped checks;
   - calls out configuration, compatibility, migration, and security impact;
   - includes an agent transcript when the project workflow requires it,
     with local paths already omitted.
4. Return the Pull Request URL. Do not publish when the ceiling is Implement.

### 6. Land when authorized

1. Wait for required CI and review. Resolve actionable feedback in the same
   branch and rerun affected checks. Re-publish review fixes with
   `scripts/gh-verified-push.sh` so the PR tip stays Verified.
2. Recheck that dependencies and repository protections still permit landing.
   Land one lane at a time; wait for a sibling lane's merge to finish and
   recheck `mergeable` before starting yours.
3. Prefer squash merge for Verified linear history on `main`:
   `gh pr merge <pr> --auto --squash --delete-branch --match-head-commit <sha>`
   (see the landing rules below).
   Use `--match-head-commit` with the published tip when available. Do not use
   GitHub rebase-merge when Verified commits matter. A failed or timed-out
   merge response may still have merged; reconcile the remote Pull Request
   state and `main` ancestry before retrying.
4. Confirm the Issue closed and the default branch contains the merge. The
   merge deleted the claim branch and the closed Issue ends the claim; the
   assignment stays as history. Then remove the local lane, serialized with
   other shared Git mutations:

   ```sh
   git worktree remove .worktrees/issue-<issue>
   git branch -D <type>/<issue>
   git worktree prune
   ```

   Squash merges are invisible to `git branch --merged`; confirm the landing
   with `gh pr view <pr> --json state,mergeCommit` before deleting. Leave the
   primary checkout untouched unless the user asks to synchronize it. A lane
   that is abandoned before publishing releases its claim with
   `bash .agents/skills/deliver-ready-issue/scripts/issue-lane.sh release <issue>`,
   which refuses to delete a branch with
   commits or an open Pull Request unless told to.
5. Invoke `advance-issue-frontier` in report-only mode unless the user also
   authorized frontier status updates.

Landing rules (pinned auto-merge, no reruns for green):

- Land with GitHub auto-merge pinned to the reviewed head:
  `gh pr merge <pr> --auto --squash --delete-branch --match-head-commit <sha>`.
  GitHub merges once the required checks pass. A later push needs a new
  request. Check the head, checks and mergeability every few minutes until
  the merge is confirmed; a watcher timing out ends observation, not the task.
- Never rerun CI to chase green. Classify each failed check against the
  newest `main` run of the same job: fix a failure the change caused; a
  failure that also fails on `main` is inherited, and a red `main` is an
  emergency fixed in its own change first. Name inherited failures in the
  Pull Request.
- A failure that does not reproduce and is unrelated to the change is a flaky
  test: open an Issue with the failing output before any rerun, and fix the
  flake rather than rerunning it again.
- Refresh onto `main` only for a conflict, a failing gate, or an explicit
  request, never just because `main` moved.

Never bypass checks, relax protection, or merge beyond the authority ceiling.

## Report completion

Return to the maintainer in the session (not on GitHub):

- Issue and authority ceiling;
- claim state (claimed, unclaimed under Implement, or taken over), local
  worktree path if useful privately, branch, base SHA, final commit, and Pull
  Request when created;
- implemented outcome;
- verification evidence (commands + results) and autoreview closeout;
- merge state and newly available follow-up work when applicable;
- blockers, skipped checks, and remaining uncertainty.

## Boundaries

- Deliver only the selected Issue; do not choose project priority.
- Do not implement blocked work or infer that silence grants ownership.
- Work only inside your lane worktree. Never switch, reset, stash, or clean the
  primary checkout or another lane's worktree, and never delete a worktree or
  branch you do not own.
- Never work around a lost claim race with a differently named branch, a
  forced ref update, or by removing another lane's assignment.
- Keep secrets, credentials, logs, databases, and runtime state out of Git.
- Never put hostnames, FQDNs, home directories, absolute worktree paths, LAN
  or employer network names, or other private environment inventory on public
  Issues, Pull Requests, comments, or commit messages.
- Do not substitute a successful build for Issue acceptance evidence.
- Do not close an Issue manually when the implementation has not landed.
