---
name: verify-change
description: Verify a sekisho Rust, protocol, documentation, configuration, or workflow change with proportionate deterministic checks. Use after implementation, before review, or when a contributor needs an exact evidence report without overstating unrun tests.
---

# Verify Change

Run the narrowest useful checks first, then expand according to change risk.

## Procedure

1. Inspect `git status`, the diff, and the stated outcome. Preserve unrelated
   worktree changes. Use `assess-change-impact` when risk is unclear. Complete
   when every changed path is classified.
2. Run focused tests for the affected module or integration first. Add checks
   based on the surface:

   | Surface | Required evidence |
   | --- | --- |
   | Rust source | focused tests plus the repository verifier’s source gate |
   | public or multi-component behavior | affected integration test plus normal suite |
   | gateway | deterministic `scripts/chisei_gateway_smoke.sh` |
   | protocol | generated build, service/client/example coverage, compatibility review |
   | persistence | fresh and upgrade behavior; SQLite/PostgreSQL alignment where implemented |
   | configuration | parsing/default tests, `.env.example`, configuration docs |
   | docs/templates/Skills | syntax, links or commands where practical; Skill validator for Skills |

3. Before ship-level handoff, use the repository Make targets for the normal
   gates unless the user explicitly requested a narrower check:

   ```bash
   make update
   make validate
   make test
   make test-integration
   ```

   Run `make test-e2e` for changes affecting the host proof or public embedding
   path. The Makefile is the source of truth for the command; do not ask an
   agent to reproduce it from prose.

   Run `cargo build --locked` when packaging, feature selection, or binaries
   changed independently of tests. Complete when every applicable local gate
   has a result.
4. Keep service-dependent tests ignored unless prerequisites and credentials
   are intentionally available. Never print secrets or persist live provider
   payloads. Complete when skipped checks name both the reason and residual
   risk.
5. Review failures against the changed scope. Report pre-existing failures with
   evidence; do not relabel a failure as pre-existing without comparison.

## Output

Report:

- commands run and pass/fail result;
- focused behavior covered;
- checks skipped with reasons;
- failures and whether they block the stated outcome; and
- remaining uncertainty.

Never use “all tests pass” unless all stated tests actually ran and passed.

## Relationship to autoreview

`verify-change` is **deterministic** local evidence only. It does **not** replace
structured second-model review. For non-trivial code that will be committed or
opened as a PR, run the **shared** `autoreview` helper after verify-change and
before ship (see `AGENTS.md` “Agent delivery closeout” and
`deliver-ready-issue`). Do not vendor that skill into this repository.
