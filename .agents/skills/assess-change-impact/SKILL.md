---
name: assess-change-impact
description: Assess a proposed or implemented sekisho change across MCP, identity, registry, limits, and operations boundaries. Use when scoping an Issue, planning tests, reviewing a diff, or identifying documentation, compatibility, and security obligations.
---

# Assess Change Impact

Build an evidence-backed impact map before implementation or review.

## Procedure

1. Read the linked Issue or request, `VISION.md`, `README.md`, and the
   relevant code. For a diff, inspect every changed file and its direct
   callers or implementors. Complete when the claimed outcome and actual change
   surface are both known.
2. Trace applicable boundaries:
   - MCP Streamable HTTP contract versus registry, policy and audit files;
   - JWKS-file verifier versus per-call token exchange;
   - in-process rate limits versus the cluster file store;
   - guardrail layers (default, tenant, agent, tool);
   - configuration, CLI, metrics, diagnostics, and the Containerfile image.
   Complete when each applicable boundary has an owner and expected invariant.
3. Identify compatibility obligations. Include old clients and configuration,
   error semantics, and fail-closed paths (missing policy, failed exchange,
   store errors). Complete when partial-failure paths are accounted for.
4. Map evidence to risk: unit tests for pure logic; integration tests under
   `tests/` for MCP and multi-component behavior; focused tests for config
   parsing. Complete when every material risk has a proposed check or an
   explicit residual uncertainty.
5. Determine durable artifacts that must change: README, example config, an
   Issue comment, or a repository Skill. Complete when no artifact is
   proposed merely to record temporary planning.

## Output

Return a compact matrix with columns:

| Surface | Evidence found | Required change/check | Risk if missed |
| --- | --- | --- | --- |

Then list scope boundaries, blocking questions, and the smallest safe PR split.
Do not approve an architecture, perform a full security audit, or claim
behavior without inspecting the implementations.
