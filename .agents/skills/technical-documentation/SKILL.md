---
name: technical-documentation
description: Build and review high-quality technical docs as well as agent instruction files in your repository. Uses the shared Sannrox agent-kit installation.
---

# Technical Documentation

Use the canonical [technical-documentation skill](https://github.com/Sannrox/agent-kit/tree/main/skills/technical-documentation).
Read the complete installed `~/.agents/agent-kit/skills/technical-documentation/SKILL.md` and the
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
