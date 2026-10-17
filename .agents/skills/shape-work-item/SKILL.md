---
name: shape-work-item
description: Shape a raw sekisho idea, bug report, refactoring proposal, or research question into a focused GitHub Issue. Use when work needs scope, acceptance evidence, risk routing, or the correct Issue template before implementation.
---

# Shape Work Item

Turn intent into a decision-ready unit of work. Produce a draft unless the user
explicitly authorizes publishing to GitHub.

## Procedure

1. Read `AGENTS.md`, `VISION.md`, `DESIGN.md`, and the matching form under
   `.github/ISSUE_TEMPLATE/`. Inspect affected code or docs when named.
   Complete when the request is framed against actual project boundaries
   (ports + settings; tenkai is delivery only).
2. Search open and closed Issues, Discussions, and PRs when GitHub access is
   available. Record possible duplicates or state that the search was not run.
3. Classify the work:
   - `bug`: reproducible expected-versus-actual behavior;
   - `feature`: a new observable operator or integration outcome;
   - `refactor`: preserved behavior with concrete structural evidence; or
   - `research`: a time-boxed question that ends in a decision.
   Route sensitive/exploitable behavior to `SECURITY.md`. Route cross-boundary,
   public-contract, trust-model, or difficult-to-reverse choices to a Design
   Discussion before implementation.
4. Draft with problem, observable outcome, non-goals, acceptance evidence,
   affected area (run/governance/workspace/settings/embed/ops/security), and
   compatibility/security risks. Omit hostnames, home paths, absolute
   worktree paths, and other private environment inventory from any text
   that will be published to GitHub.
5. Recommend labels (`type:*`, `area:*`, `status:ready` when ready). Do not
   invent priority, assignment, or sprint.

## Output

1. route and rationale;
2. possible duplicates;
3. issue title and body ready for GitHub;
4. recommended labels;
5. unresolved questions blocking `status:ready`.

Do not publish without explicit authorization.
