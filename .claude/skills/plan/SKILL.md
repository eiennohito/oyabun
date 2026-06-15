---
name: plan
description: >
  Enter planning mode. Reads relevant docs, explores code, and produces a plan doc in docs/plans/.
  The plan session ends with a written plan — implementation happens in a separate session
  after /clear. Use when the user says /plan, "let's plan", or starts scoping a feature.
argument-hint: "[topic or problem description]"
allowed-tools: "Bash(git *) Read Glob Grep WebFetch WebSearch"
disable-model-invocation: true
---

# Planning Session

You are entering a planning session. This session produces a **plan document** — not code.
At the end, the user will `/clear` and start a fresh implementation session that reads your plan doc.

## Your job

1. **Understand the problem.** Read relevant docs, code, and git history.
2. **Discuss with the user.** Plans are collaborative. Push back on bad ideas. Ask clarifying questions.
3. **Write the plan doc.** When the plan is solid, write it to `docs/plans/<topic>.md`.
4. **Remind the user to `/clear`** before starting implementation.

## Constraints

- **No code changes.** This is a planning session. Read-only exploration of code is fine and encouraged.
- **No commits.**
- If `$ARGUMENTS` specifies a topic, use it as the starting point. Otherwise ask the user what they want to plan.
