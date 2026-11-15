---
name: ci-audit
description: Audit a repository's CI from its run history and gate every new or changed CI job. Use when CI is slow, when the check list on a Pull Request keeps growing, when asked which jobs earn a place on Pull Requests, or when adding or changing a job or trigger in `.github/workflows/`. Measure first, judge a job by what it alone catches, move before deleting. Uses the shared Sannrox agent-kit installation.
---

# CI Audit

Use the canonical [ci-audit skill](https://github.com/Sannrox/agent-kit/tree/main/skills/ci-audit).
Read the complete installed `~/.agents/agent-kit/skills/ci-audit/SKILL.md` and the
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
