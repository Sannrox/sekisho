# Repository Guidelines

## Project Structure & Module Organization

`sekisho` is a Rust 2024 crate. Governed tool gateway for AI agents: one MCP entry point with per-agent identity, token exchange, tool registry, rate limits, approvals and audit. Source lives in `src/`. Integration
tests go in `tests/`. Read `VISION.md` (vision and roadmap) and `README.md`
before changing system boundaries.

## Build, Test, and Development Commands

- `cargo fmt` formats Rust code before review.
- `cargo test` runs the test suite.
- `cargo clippy --all-targets --locked -- -D warnings` matches CI.
- `make validate`, `make test` and `make test-integration` are the local gates.
  They match CI (`.github/workflows/ci.yml`): `cargo build --all-targets --locked`,
  `cargo test --locked`, `cargo fmt --check`, and
  `cargo clippy --all-targets --locked -- -D warnings`.

GitHub Issues are the planning source of truth. Project-specific Skills live
under `.agents/skills/`. Never commit hostnames, home paths, secrets or
private environment details.

## Agent delivery closeout (required)

`cargo test` / Clippy / green CI are **not** a substitute for structured code
review. For non-trivial implementation work (any behavior, security, settings,
or public-API change) that will be **committed, pushed, or opened as a PR**:

1. Run `make update`, `make validate`, `make test`, and
   `make test-integration` via the `verify-change` Skill.
2. Run **`autoreview`** before the ship commit (or before push if the commit
   already exists). **Do not vendor** the autoreview skill into this repo;
   resolve the helper from the shared skill install (first match wins):

   ```bash
   # Preferred: shared agent-skills checkout or global skills home
   if [ -x "${AUTOREVIEW:-}" ]; then :; \
   elif [ -x "$HOME/Projects/agent-skills/skills/autoreview/scripts/autoreview" ]; then
     export AUTOREVIEW="$HOME/Projects/agent-skills/skills/autoreview/scripts/autoreview"
   elif [ -x "${AGENTS_HOME:-$HOME/.agents}/skills/autoreview/scripts/autoreview" ]; then
     export AUTOREVIEW="${AGENTS_HOME:-$HOME/.agents}/skills/autoreview/scripts/autoreview"
   else
     echo "autoreview helper not found; install shared agent-skills or set AUTOREVIEW" >&2
     exit 1
   fi
   # Dirty uncommitted work:
   "$AUTOREVIEW" --mode local
   # Topic branch / open PR (preferred after commit):
   "$AUTOREVIEW" --mode branch --base origin/main
   # Already committed on a clean tree:
   "$AUTOREVIEW" --mode commit --commit HEAD
   ```

3. Treat helper output as advisory: verify each accepted finding in the real
   code, fix actionable ones, rerun tests and autoreview until the helper exits
   0 with no accepted/actionable findings (or document a maintainer judgment
   blocker in the PR).
4. Report in the PR or handoff: commands run, tests, autoreview result
   (clean / findings fixed / consciously rejected).

**Do not skip autoreview** because CI is green, the change is “small,” or the
session is optimizing for throughput unless the user explicitly waived it.
Docs-only typo fixes and pure formatting may skip structured review; state the
waiver. The `deliver-ready-issue` Skill requires this same closeout before
publish/land.

Default review engine is Codex (`gpt-6.1-sol` via the helper). Read the shared
`autoreview` skill’s `SKILL.md` next to the resolved helper for engines and
findings policy.

## Parallel delivery lanes

A delivery lane is one Issue, one branch, one isolated checkout, one Pull
Request, and one owner. Several agents may deliver several Issues at the same
time, on one machine or on many, only as separate lanes. One Issue is never
split across lanes, and one lane never carries a second Issue.

### Claims live on GitHub

Machines cannot see each other's worktrees, so a claim is only what GitHub
shows. An Issue is claimed, and therefore active, when any of the following
exists:

1. an open Pull Request, draft or ready, that references the Issue;
2. a branch `<type>/<issue>`, or any branch matching `*/<issue>-*`;
3. the authenticated login assigned to the Issue less than six hours ago.

An assignment older than six hours with neither branch nor Pull Request is an
ownership hint, not a veto: report it and ask the maintainer before claiming.
A claim is stale once its Issue is closed or its Pull Request has merged; its
owner or the maintainer removes it. Any other takeover first preserves the
existing branch, Pull Request, and evidence and requires the maintainer's
confirmation, unless the lead of the same parallel run owns the stalled lane.

A lane claims before it implements, under Publish or Land authority or with an
explicit claim authorization under Implement, using
`bash .agents/skills/deliver-ready-issue/scripts/issue-lane.sh claim <issue>`:

- the branch `<type>/<issue>` is created on GitHub from the default branch by
  an atomic ref creation, so when two machines race exactly one claim
  succeeds and the other sees `claimed`;
- the authenticated login is assigned to the Issue, which shows the claim in
  the Issue list and timestamps it in the Issue timeline.

`<type>` is derived deterministically so that every machine computes the same
branch: the Conventional Commit type in the Issue title, with `bug` mapped to
`fix`; otherwise the type label (`bug` → `fix`, `enhancement` → `feat`,
`documentation` → `docs`); otherwise `chore`. Lane branches carry no slug
because the name is the claim; the Pull Request title carries the description.
Human topic branches may keep `<type>/<issue>-<slug>` and are detected as
claims by the same rule, as are legacy `codex/<issue>-<slug>` branches.

An Implement-only lane cannot claim and is invisible to other machines. Say so
in the report, or ask for claim authority before starting.

### Isolation on each machine

Local isolation protects lanes that share a machine; it is not a claim.

- Each lane checks out its claim branch in `.worktrees/issue-<issue>` inside
  the repository, ignored by Git. A fresh clone dedicated to the lane, as on a
  cloud agent, is equivalent. The base SHA is recorded in the lane report.
- The primary checkout belongs to the maintainer. Agents never switch its
  branch, reset it, stash it, or run long jobs in it while another lane is
  active.
- A worktree serves exactly one Issue. Finished lanes are removed; a worktree is
  never reused for a different Issue or renamed to hide its origin.
- Shared Git state is mutated in short, serialized slots: `git fetch --prune`,
  worktree creation and removal, local branch deletion, and merging belong to
  the lead of a parallel run or happen one lane at a time on a shared machine.
  A lane commits and publishes its own branch from its own worktree; that is
  not a shared mutation. Nobody holds a slot across implementation, test runs,
  or a remote wait.

### Limits, collisions, and landing

The three-lane limit counts claims visible on GitHub per repository: open
implementation Pull Requests plus claim branches without one. Assigned or
planned work with neither is not a running lane.

Parallel lanes must not collide. Collision surfaces in this repository are
config and the MCP endpoint, verifier and token exchange, registry/policy/audit
files, the cluster rate-limit store, and guardrails. Lanes that would both
change one of these surfaces run in sequence, not in parallel.

A lane publishes its first Verified commit to the claim branch as a draft Pull
Request that closes the Issue. Public GitHub lane fields may list only: claim
branch, repo-relative worktree (for example `.worktrees/issue-N` — never an
absolute path), base SHA, and published SHA. Keep absolute paths and
machine/host names in the private agent session only. Never put hostnames,
FQDNs, home directories, LAN or employer network names, or other private
environment inventory on the Pull Request. It marks the Pull Request ready
when verification and review are complete. Immediately before publishing, the
lane fetches and confirms that the default branch is an ancestor of its head;
it refreshes onto `main` only for a conflict, a failing gate, an explicit
request, or a sibling landing on a shared surface, not merely because `main`
advanced.

Landing is sequential. After each merge, fetch, recompute the frontier, and let
the remaining lanes recheck `mergeable` against the new `main`. A failed or
timed-out merge response may still have merged; reconcile the remote state
before retrying.

The lead of a parallel run keeps a ledger per lane: Issue, branch, base SHA,
owner, state, Pull Request, evidence, blockers, and cleanup. Keep hostnames
and absolute checkout paths in the session with the maintainer, never on
GitHub. Report verified outcomes, not launched work. The executable lead
procedure is `.agents/skills/deliver-ready-issue/references/parallel-delivery.md`.

## Coding Style & Naming Conventions

Follow standard Rust formatting with `cargo fmt` and keep modules aligned with
the existing domain boundaries. Use `snake_case` for files, modules, functions,
and variables; use `PascalCase` for types and traits; use
`SCREAMING_SNAKE_CASE` for constants. Prefer explicit fail-closed checks
(missing policy, failed exchange, store errors) over silent degradation. Keep
the CLI thin; put logic in the library so tests and hosts can embed the gateway
without shelling out.

## Testing Guidelines

Add focused tests for changes touching the MCP endpoint, registry, policy,
token exchange, limits, guardrails, or audit. Prefer deterministic tests that
do not require an issuer or upstream. Offline `cargo test` must stay green
without those services.

## Commit & Pull Request Guidelines

Use short imperative subjects, often Conventional Commit style:
`feat(audit): read call events by trace id`, `docs: document token exchange`,
`fix(limits): fail closed when the cluster store cannot lock`. Keep commits
narrow and describe the affected subsystem when useful. Pull requests should
include a concise behavior summary, **tests run**, **autoreview result** (for
non-trivial code), linked issue or context, and any configuration or security
implications. Do not put hostnames, FQDNs, home directories, absolute worktree
paths, LAN or employer network names, or other private environment inventory on
public Issues, Pull Requests, comments, or commit messages. Release notes
belong in the pull request body; [CHANGELOG.md](CHANGELOG.md) is release-owned.

### Verified commits on GitHub

Prefer publishing PR branch tips with signed commits so GitHub shows
**Verified**:

1. Implement and commit locally as usual (`commit.gpgsign` may still apply).
2. Publish the claim branch with `git push -u origin HEAD` (or `git push` if
   upstream is already set) unless the user asked for a different publish path.
   This repository has no `scripts/gh-verified-push.sh`.
3. Never pass `--no-gpg-sign` for local commits; if GPG fails, stop and fix it.

When merging PRs, prefer **squash** (`gh pr merge --squash --delete-branch`) so
the land commit on `main` is also GitHub-signed/Verified and history stays
linear. Use `gh pr merge --merge` only when multi-commit history must be kept
(original SHAs preserved). Avoid GitHub **rebase** merges when Verified history
matters: rebase-merge rewrites commits and drops signatures. Do not rewrite
protected `main` after merging unless the user explicitly approves; if
protection is temporarily relaxed, restore force-push and status-check settings
immediately after the correction.

## Security & Configuration Tips

Never commit secrets, tokens, provider credentials, logs, or local state
(`sekisho.toml` copies, JWKS, token files, registry/audit/cluster files). Never
put hostnames, FQDNs, home directories, absolute worktree paths, LAN or
employer network names, or other private environment inventory on public
Issues, Pull Requests, comments, or commit messages. Report suspected
bypasses privately (GitHub private vulnerability reporting or the maintainer),
never in a public Issue.
