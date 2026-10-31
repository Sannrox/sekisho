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
  `cargo test --locked`, `cargo fmt --check`,
  `cargo clippy --all-targets --locked -- -D warnings`, and
  `bash scripts/validate-release.sh`.
- `scripts/validate-release.sh` checks the tag-triggered release path: signed
  annotated tags, CHANGELOG notes, SBOM, and the family release manifest.
  Pull requests and dry runs must not publish or attest.

GitHub Issues are the planning source of truth. The workflow Skills are the
shared agent-kit Skills (see [Shared agent skills](#shared-agent-skills)). Never commit hostnames, home paths, secrets or
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
`bash "${AGENT_KIT_HOME:-$HOME/.agents/agent-kit}"/skills/deliver-ready-issue/scripts/issue-lane.sh claim <issue>`:

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

A lane publishes its first Verified commit to the claim branch with
`bash "${AGENT_KIT_HOME:-$HOME/.agents/agent-kit}"/skills/deliver-ready-issue/scripts/issue-lane.sh push <issue>`
and opens a draft Pull Request that closes the Issue. Public GitHub lane fields may list only: claim
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

A lane lands with
`bash "${AGENT_KIT_HOME:-$HOME/.agents/agent-kit}"/skills/deliver-ready-issue/scripts/issue-lane.sh land <issue> --pr <pr>`,
which squash-merges only the reviewed head and confirms the merge and the
closed Issue. Landing is sequential. After each merge, fetch, recompute the frontier, and let
the remaining lanes recheck `mergeable` against the new `main`. A failed or
timed-out merge response may still have merged; reconcile the remote state
before retrying.

The lead of a parallel run keeps a ledger per lane: Issue, branch, base SHA,
owner, state, Pull Request, evidence, blockers, and cleanup. Keep hostnames
and absolute checkout paths in the session with the maintainer, never on
GitHub. Report verified outcomes, not launched work. The executable lead
procedure is `references/parallel-delivery.md` in the shared
`deliver-ready-issue` Skill.

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

Lane branch tips are published as GitHub **Verified** commits:

1. Implement and commit locally as usual; never pass `--no-gpg-sign`. If GPG
   fails, stop and fix it.
2. Publish the claim branch with
   `bash "${AGENT_KIT_HOME:-$HOME/.agents/agent-kit}"/skills/deliver-ready-issue/scripts/issue-lane.sh push <issue>`.
   It creates Verified commits on the claim branch and resets the worktree to
   the published commit, so local `HEAD` is the Pull Request head.
3. Land with
   `bash "${AGENT_KIT_HOME:-$HOME/.agents/agent-kit}"/skills/deliver-ready-issue/scripts/issue-lane.sh land <issue> --pr <pr>`.
   It squash-merges the reviewed head, so the land commit on `main` is also
   Verified and history stays linear.

Avoid GitHub **rebase** merges: they rewrite commits and drop signatures. Do
not rewrite protected `main` after merging unless the user explicitly
approves; if protection is temporarily relaxed, restore force-push and
status-check settings immediately after the correction.

## Security & Configuration Tips

Never commit secrets, tokens, provider credentials, logs, or local state
(`sekisho.toml` copies, JWKS, token files, registry/audit/cluster files). Never
put hostnames, FQDNs, home directories, absolute worktree paths, LAN or
employer network names, or other private environment inventory on public
Issues, Pull Requests, comments, or commit messages. Report suspected
bypasses privately (GitHub private vulnerability reporting or the maintainer),
never in a public Issue.

## Shared agent skills

The workflow Skills (`advance-issue-frontier`, `assess-change-impact`,
`capture-project-decision`, `deliver-ready-issue`, `prepare-release`,
`refactor-docs`, `shape-work-item`, `technical-documentation`, `verify-change`)
are canonical in [Sannrox/agent-kit](https://github.com/Sannrox/agent-kit).
There is no pin: this repository always uses the latest `main`.
`.agents/skills/<name>/` holds only a generated stub that points to the shared
skill; read the installed skill under `~/.agents/agent-kit/` (or
`$AGENT_KIT_HOME`). Run `bash scripts/validate-workflow-skills.sh` to clone or
update it.

- Repository-specific facts the shared Skills need come from this file: the
  structure, testing, parallel-lane and security sections, plus
  `## Agent skill profile` below for anything they do not already say. Edit
  this file, not the stubs.
- Never edit the stubs or the vendored `scripts/validate-workflow-skills.sh`
  here; change them upstream in agent-kit, then run
  `bash scripts/validate-workflow-skills.sh --render` here when CI reports
  drift.
- Repository-only Skills live next to the stubs as full folders and stay local.

## Agent skill profile

Facts the shared agent-kit skills read that the sections above do not already
state.

Feature: a new observable operator or integration outcome.

- Read `VISION.md` with `README.md` when scoping or shaping work.
- There is no `docs/decisions/` tree. If a durable boundary, trust model, or
  public contract needs an ADR, propose adding that tree in the same change.
  Durable docs are `README.md`, `VISION.md`, `RELEASING.md`,
  `sekisho.example.toml`, and `agents.example.toml`.
- Affected areas for Issues: mcp, registry, policy, exchange, limits,
  guardrails, audit, cluster.

### Compatibility and failure obligations

Old clients and configuration, error semantics, and fail-closed paths
(missing policy, failed exchange, store errors) on every change. The MCP
Streamable HTTP contract, token exchange, registry, and config formats stay
compatible or the break is called out.

### Additional impact boundaries

- MCP Streamable HTTP contract versus registry, policy, and audit files;
- JWKS-file verifier versus per-call token exchange;
- in-process rate limits versus the cluster file store;
- guardrail layers (default, tenant, agent, tool);
- configuration, CLI, metrics, diagnostics, and the `Containerfile` image.

### Verification matrix

Rows beyond `## Testing Guidelines`:

| Surface | Required evidence |
| --- | --- |
| Rust source | focused tests, then `make update`, `make validate`, `make test`, `make test-integration` |
| MCP / gateway | the affected integration test under `tests/` plus the normal suite |
| configuration | parsing/default tests, `sekisho.example.toml`, README |
| PostgreSQL state backend | `cargo test --locked --features postgres --test state_backends` with `SEKISHO_TEST_POSTGRES_URL` set to a disposable database; skipped otherwise with the reason stated |

### Release audit

- `Cargo.toml` version and `Cargo.lock` consistency;
- MCP, token-exchange, registry, and config compatibility;
- configuration additions, removals, defaults, and `sekisho.example.toml`;
- security, rollback, and operator actions;
- `Containerfile` image packaging (deploy by digest, never a `latest` tag);
- release notes drafted from the pull request bodies since the last tag with
  `scripts/changelog-draft.sh`; `CHANGELOG.md` changes only in the release.
