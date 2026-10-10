---
description: Give coding agents conventions through versioned context files, AGENTS.md for the repo, skills for procedures, DESIGN.md for UI, and review third-party skills like any dependency.
tags: [ai, coding-agents, agents-md, skills, supply-chain]
status: adopted
reviewed: 2026-10-10
---

# Keep agent context in versioned files

## Rule

Conventions that agents must follow live in files committed next to the code, not in chat history.

- [AGENTS.md](https://agents.md/) at the repository root records only what the code cannot tell: conventions, policies, and traps. No directory trees or feature summaries an agent can read itself
- [Agent Skills](https://agentskills.io/) hold repeatable procedures. Write them declaratively, as constraints the output must satisfy and how to validate it, and keep them short. Omit general knowledge the model already has
- A DESIGN.md holds the visual system for UI work: colors, type, spacing, components, and the reasoning behind them, so agent-built pages stay consistent
- Review a third-party skill before installing it from a directory such as [skills.sh](https://skills.sh/): read every file, pin it to a commit, and prefer copying it into the repo over live installs

## Why

- **Repeatability**: a convention stated once in a file applies to every session and every agent, while a correction in chat is forgotten next time
- **Review**: context files go through the same review and history as code, so a bad instruction is visible and revertable
- **Token budget**: context is loaded into every session, so restating what the code already says costs tokens and buries the rules that matter
- **Supply chain**: a skill is instructions and often scripts that the agent executes with your permissions. Install counts on a directory measure popularity, not safety

## Exceptions

- One-off task instructions belong in the prompt, not in a context file

## References

- [AGENTS.md](https://agents.md/)
- [Agent Skills](https://agentskills.io/)
- [skills.sh](https://skills.sh/)
- [getdesign.md](https://getdesign.md/)
