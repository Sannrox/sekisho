---
name: assess-change-impact
description: Assess a proposed or implemented change across trust, API, dependency, and operations boundaries. Use when scoping an Issue, planning tests, reviewing a diff, or identifying compatibility, documentation, and security obligations. Uses the shared Sannrox agent-kit installation.
---

# Assess Change Impact

Use the canonical [assess-change-impact skill](https://github.com/Sannrox/agent-kit/tree/main/skills/assess-change-impact).
Read the complete installed `~/.agents/agent-kit/skills/assess-change-impact/SKILL.md` and the
references it requires before starting. If installed at another operator-selected location
(`$AGENT_KIT_HOME`), use that installation. This repository entrypoint contains no
workflow implementation.

If the skill is missing, run `bash scripts/validate-workflow-skills.sh` from this repository
(it clones or updates the kit), or clone it directly:
`git clone https://github.com/Sannrox/agent-kit ~/.agents/agent-kit`. Run `git pull` in that
checkout to get the latest version; consumers always use `main`.

Run the shared skill's scripts from the repository being changed, preserving its working
directory. Follow this repository's `AGENTS.md` (including `## Agent skill profile`) in addition to the
canonical skill.

Keep shared fixes and instructions upstream in `Sannrox/agent-kit`. When the stub template
changes upstream, run `bash scripts/validate-workflow-skills.sh --render`. Do not add
repository-local helpers or copies.
