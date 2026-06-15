---
name: precommit
description: >
  Run pre-commit checks: sync docs, format, lint, compile-check.
  Use when the user says "commit", /precommit, or asks to prepare a commit.
argument-hint: "[commit message hint]"
allowed-tools: "Bash Read Edit Write Glob Grep Agent"
---

# Pre-Commit Checklist

The user wants to commit. Run through this checklist before creating the commit.

## 1. Audit user corrections

Scan the session for places the user corrected you — wrong assumptions, wrong design direction, wrong mental model. For each:
- What was the correction? (the reasoning, not just the fact)
- Is it already captured in docs? (grep for key terms)
- If not: find the right home (CLAUDE.md for workflow rules, code comments for implementation constraints) and capture it.

## 2. Sync docs

- Update `docs/` for the areas you changed.
- Update `CLAUDE.md` if a project rule changed.
- Delete completed or abandoned `docs/plans/` files.

## 3. Run `just precommit`

```!
just precommit
```

This formats all code, runs clippy, compile-checks, and runs tests.

## 4. Create the commit

Inspect actual working tree state before committing — the user may have edited files between turns.

If `$ARGUMENTS` contains a hint for the commit message, use it.
