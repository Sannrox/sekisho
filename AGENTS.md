# Repository Guidelines

## Project Structure & Module Organization

`sekisho` is a Rust 2024 crate. Governed tool gateway for AI agents: one MCP entry point with per-agent identity, token exchange, tool registry, rate limits, approvals and audit. Source lives in `src/`. Integration
tests go in `tests/`. Read `VISION.md` (vision and roadmap) and accepted ADRs
under `docs/decisions/` before changing system boundaries.

## Build, Test, and Development Commands

- `cargo fmt` formats Rust code before review.
- `cargo test` runs the test suite.
- `cargo clippy --all-targets --locked -- -D warnings` matches CI.
- `make validate`, `make test` and `make test-integration` are the local gates.

GitHub Issues are the planning source of truth. Project-specific Skills live
under `.agents/skills/`. Never commit hostnames, home paths, secrets or
private environment details.

## Agent delivery closeout (required)

`cargo test` / Clippy / green CI are **not** a substitute for structured code
review. For non-trivial implementation work (any behavior, security, settings,
or public-API change) that will be **committed, pushed, or opened as a PR**:

1. Run `make update`, `make validate`, `make test`, and
   `make test-integration` via the `verify-change` Skill. Run `make test-e2e`
   when the host proof is affected.
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
   elif [ -x "$HOME/Projects/sekai-chisei/.agents/skills/autoreview/scripts/autoreview" ]; then
     export AUTOREVIEW="$HOME/Projects/sekai-chisei/.agents/skills/autoreview/scripts/autoreview"
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

Default review engine is Codex (`gpt-5.5` via the helper). Read the shared
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
ports and settings, governance adapters, the workspace jail, the
sekai-client proto pin, and the kekkai pin. Lanes that would both change one of these surfaces run
in sequence, not in parallel.

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

## Ontology Policy

For work involving portable ontology definitions, classes, relations,
provenance, validation, import, export, or structural queries, always use the
project-local `sekai-ontology` Skill in `.agents/skills/sekai-ontology/`.

Select the ontology database explicitly with `--db <path>` or `SEKAI_DB`, then
run `sekai --db <path> --json validate` before relying on its contents. Treat
successful ontology output as structured repository evidence, preserve its
provenance in answers, and state when validation fails or the requested fact is
absent rather than inferring it. Do not use a harness state directory or a
control-plane database as a portable ontology database.

## Coding Style & Naming Conventions

Follow standard Rust formatting with `cargo fmt` and keep modules aligned with
the existing domain boundaries. Use `snake_case` for files, modules, functions,
and variables; use `PascalCase` for types and traits; use
`SCREAMING_SNAKE_CASE` for constants. Keep governance, workspace, model, and
event behavior behind port traits and in-tree adapters. Prefer explicit
fail-closed governance when required over silent degradation. Individual units
of work are **runs**, not “a sekisho.” Keep the CLI thin; put logic in the
library so hosts can embed `Harness` without shelling out.

## Testing Guidelines

Add focused tests for changes touching the turn loop, tools, workspace
materialization, settings resolution, or governance adapters. Prefer
deterministic tests that do not require external services. Mark plane- or
provider-dependent tests ignored, following `tests/plane_live.rs`, and document
required local services in the test or related docs. Offline `cargo test` must
stay green without sekai-chisei.

## Commit & Pull Request Guidelines

Use short imperative subjects, often Conventional Commit style:
`feat(run): jail bash to workspace`, `docs: document settings resolution`,
`fix(governance): fail closed without plane endpoint`. Keep commits narrow and
describe the affected subsystem when useful. Pull requests should include a
concise behavior summary, **tests run**, **autoreview result** (for non-trivial
code), linked issue or context, and any configuration or security implications.
Do not put hostnames, FQDNs, home directories, absolute worktree paths, LAN
or employer network names, or other private environment inventory on public
Issues, Pull Requests, comments, or commit messages. Update
[CHANGELOG.md](CHANGELOG.md) for user-visible changes.

### Verified commits on GitHub

Prefer publishing PR branch tips with GitHub-signed commits so GitHub shows
**Verified**:

1. Implement and commit locally as usual (`commit.gpgsign` may still apply).
2. Publish the branch tip with `scripts/gh-verified-push.sh` instead of a plain
   `git push` when you want the hosted commit Verified (OpenClaw-style GraphQL
   `createCommitOnBranch`). That path creates one server-side commit with the
   local `HEAD` tree; committer is typically **GitHub**.
3. New branch:
   `scripts/gh-verified-push.sh --create-branch-from origin/main --branch <topic> --sync-local`
4. Existing PR branch:
   `scripts/gh-verified-push.sh --branch <topic> --sync-local`
   (uses the current remote tip as `expectedHeadOid`).
5. Never pass `--no-gpg-sign` for local commits; if GPG fails, stop and fix it.
6. After publish, confirm `verification.verified=true` (the script prints this).

When merging PRs, prefer **squash** (`gh pr merge --squash --delete-branch`) so
the land commit on `main` is also GitHub-signed/Verified and history stays
linear. Use `gh pr merge --merge` only when multi-commit history must be kept
(original SHAs preserved). Avoid GitHub **rebase** merges when Verified history
matters: rebase-merge rewrites commits and drops signatures. Do not rewrite
protected `main` after merging unless the user explicitly approves; if
protection is temporarily relaxed, restore force-push and status-check settings
immediately after the correction.

## Security & Configuration Tips

Never commit secrets, tokens, provider credentials, logs, or local harness
state (`.sekisho-state/`). Do not treat delivery systems as runtime control
dependencies. When profile `governed` or `governance.fail_closed` is set, missing or
unhealthy governance must fail doctor and run. Never put hostnames, FQDNs,
home directories, absolute worktree paths, LAN or employer network names, or
other private environment inventory on public Issues, Pull Requests, comments,
or commit messages. Report vulnerabilities through [SECURITY.md](SECURITY.md).
Follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) in all project spaces.
