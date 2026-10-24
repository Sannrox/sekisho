---
name: capture-project-decision
description: Capture an accepted sekisho design or project outcome in the smallest durable artifact. Use after a Discussion, Issue, or PR resolves a choice that may require maintained documentation, an example, or a reusable Skill.
---

# Capture Project Decision

Promote the durable result, not the conversation that produced it.

## Procedure

1. Read the complete source Discussion, Issue, PR, and relevant current docs or
   code. Confirm the choice is accepted rather than merely proposed. Complete
   when the decision, owner, alternatives, and supporting evidence are named.
2. Select exactly the artifacts justified by future use:
   - documentation: repeated operator, integrator, or contributor knowledge
     (`README.md`, `VISION.md`, example config);
   - example: a supported path is best taught and tested as executable code;
   - Skill: a repeated project-specific AI procedure or high-risk checklist;
   - none: the outcome is local to the closed work item.
   Complete when every artifact has a future audience and owner.
3. Preserve one source of truth. Link to code rather than duplicating
   exhaustive details. Complete when the same rule is not maintained in
   multiple prose locations.
4. This repository has no `docs/decisions/` tree. If a durable boundary, trust
   model, or public contract needs an ADR, propose adding that tree in the
   same change rather than copying a missing template.
5. Check links, examples, Skill metadata, and affected indexes. Complete when a
   future contributor can discover the result without the original prompt.

## Output

State the selected artifact(s), why each is durable, files changed or proposed,
source links, owner, and validation. If no promotion is warranted, say so and
leave the knowledge in the closed Issue/PR.

Do not turn unresolved debate into policy, copy investigation logs into docs,
or create an ADR for routine implementation detail.
