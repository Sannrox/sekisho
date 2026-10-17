---
name: assess-change-impact
description: Assess a proposed or implemented sekai-chisei change across product, trust, API, persistence, and operations boundaries. Use when scoping an Issue, planning tests, reviewing a diff, or identifying migration, documentation, compatibility, and security obligations.
---

# Assess Change Impact

Build an evidence-backed impact map before implementation or review.

## Procedure

1. Read the linked Issue or request, `VISION.md`, `docs/architecture.md`, and
   the relevant code. For a diff, inspect every changed file and its direct
   callers or implementors. Complete when the claimed outcome and actual change
   surface are both known.
2. Trace applicable boundaries:
   - Sekai durable facts versus Chisei governed decisions;
   - gateway-compatible requests versus native gRPC execution;
   - SQLite versus implemented PostgreSQL interfaces;
   - provider-neutral behavior versus `src/llm/` adapters;
   - namespace authorization, egress, approval, budget, audit, lineage,
     retention, and secrets;
   - public `proto/`, configuration, CLI, receipt, metrics, and operator
     behavior.
   Complete when each applicable boundary has an owner and expected invariant.
3. Identify persistence and compatibility obligations. Include fresh and
   upgraded databases, transactional audit coupling, old clients/configuration,
   error semantics, and rollback or backup impact where relevant. Complete when
   data-loss and partial-failure paths are accounted for.
4. Map evidence to risk: unit tests for pure logic; integration tests for
   public/multi-component behavior; fixtures for provider and stream contracts;
   deterministic gateway smoke for gateway changes; ignored live tests only
   when a real service is essential. Complete when every material risk has a
   proposed check or an explicit residual uncertainty.
5. Determine durable artifacts that must change: docs, `.env.example`, examples,
   protocol notes, an ADR, or a repository Skill. Complete when no artifact is
   proposed merely to record temporary planning.

## Output

Return a compact matrix with columns:

| Surface | Evidence found | Required change/check | Risk if missed |
| --- | --- | --- | --- |

Then list scope boundaries, blocking questions, and the smallest safe PR split.
Do not approve an architecture, perform a full security audit, or claim backend
parity without inspecting the implementations.
