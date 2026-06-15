---
name: impl-session
description: >
  Implementation session protocol. Invoke at the start of a session that
  implements a plan from docs/plans/.
argument-hint: "[plan file path]"
disable-model-invocation: true
---

# Implementation Session

The user is starting an implementation session. Read the plan file (argument or ask), then follow this protocol.

## 1. Read the plan

Read the plan doc from `docs/plans/`. Build a mental model of what it's trying to achieve — the goal, not the task list.

## 2. Explore before executing

Read the code the plan touches. Check assumptions the plan makes:
- Do the APIs/types it references actually exist and work as described?
- Does the domain model match what the plan assumes?

Flag anything that doesn't hold. Don't start implementing on a false premise.

## 3. The plan is a starting point, not a spec

Improve on the plan when you find better approaches. The goal is to deliver 120%.
Deviate when evidence says to. Flag deviations to the user with the reasoning.

## 4. Verify as you go

After each major piece:
- Does it actually work? (build, test)
- Does the user's manual testing find issues? Act on them immediately.

## 5. Clean up the plan when done

- Delete the plan file (the work is in the code now).
- Port domain knowledge to docs.
- Update `CLAUDE.md` for the areas you changed.
