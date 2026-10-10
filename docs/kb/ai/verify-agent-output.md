---
description: Let coding agents implement while humans own requirements and the ship decision, and accept agent work only on evidence from a running system, not on reading the diff.
tags: [ai, coding-agents, verification, review]
status: adopted
reviewed: 2026-10-10
---

# Verify agent output by running it

## Rule

Coding agents write the implementation. Humans define the requirements, architecture, and user experience, decide what ships, and accept work on evidence.

- Every agent task ends with verification against a running system: build, lint, tests, and an end-to-end check with a real client, not unit tests alone
- The agent reports the commands it ran and their output. A claim of done without output is not done
- Treat the first version as a prototype and improve it through use, deployment, and feedback rather than line-by-line review
- Use sub-agents and automated review for breadth, and keep the final accept or reject decision human
- Record recurring conventions in versioned context files, see [agent-context-files.md](agent-context-files.md)

## Why

Agents produce working code faster than people can read it. When review is the gate, it either becomes the bottleneck or turns into skimming, and neither catches behavior bugs. A running system does.

- **Evidence scales**: test output, rendered manifests, and a successful request against a deployed service are checked in seconds, whatever the size of the diff
- **Failure modes differ**: agent code tends to compile and read plausibly while missing an edge case or a contract, which only execution exposes
- **Ownership stays clear**: deciding what to build and what to support remains a human call, because that is where the cost of being wrong sits

## Exceptions

- Design debates stay between people. Talking a problem through with colleagues is where shared judgment forms, and delegating it to an agent loses that
- Security-sensitive changes, such as IAM policies, network exposure, and secret handling, get human review of the diff in addition to execution evidence

## References

- [The end of programming](https://pauldix.com/the-end-of-programming)
- [Why Is Everyone In Tech So Sad?](https://www.noemamag.com/why-is-everyone-in-tech-so-sad/)
