---
name: prepare-release
description: Prepare a sekisho release by auditing version scope, compatibility, validation, artifacts, and release notes. Use when a maintainer asks for release readiness, a version bump plan, or a draft GitHub Release.
---

# Prepare Release

Assemble decision-ready release evidence. Do not tag, push, publish, or alter
GitHub state unless the maintainer explicitly authorizes that action.

## Procedure

1. Identify the target version, base tag, target commit, and milestone or merged
   PR range. Read `Cargo.toml`, `CHANGELOG.md`, `scripts/changelog-draft.sh`,
   and open release-blocking Issues. Complete when the exact release contents
   are bounded.
2. Classify changes as `Added`, `Changed`, `Fixed`, `Security`, or `Migration`.
   Check SemVer fit; before `1.0`, call out all public breaking changes even when
   they fit a minor bump. Complete when every user-visible merged change is
   represented once.
3. Audit release impact:
   - `Cargo.toml` version and lockfile consistency;
   - MCP, token-exchange, registry and config compatibility;
   - configuration additions, removals, defaults, and `sekisho.example.toml`;
   - security, rollback, and operator actions;
   - Containerfile image packaging (deploy by digest).
   Complete when every applicable item is resolved or a named blocker.
4. Use `verify-change` for the full local gates. Confirm current GitHub CI
   when access is available. Complete when evidence is current for the
   target commit.
5. Draft concise user-facing release notes from the pull request bodies since
   the last tag (`scripts/changelog-draft.sh`). Put upgrade actions before
   internal implementation detail. Do not edit `CHANGELOG.md` in ordinary
   pull requests; it is release-owned.
6. Report go/no-go. A release is `go` only when required checks pass, no known
   blocker remains, and rollback/upgrade implications are explicit.

## Output

Return the target, commit range, readiness checklist, validation results,
compatibility notes, release-note draft, and blockers. Separate verified facts
from recommendations.
