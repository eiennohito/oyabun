# Project context for precheck reviewers

## Stack & architecture

Rust CLI/TUI process manager. Linux-only, reads from `/proc` filesystem.
Single binary, no network, no database. Terminal UI via ratatui/crossterm.

## Priorities

Near-zero CPU at idle. Correct process tree modeling. Safe privilege escalation.
Scale to 256+ cores and thousands of processes without degradation.

## Conventions

- Domain objects over god services. Make invalid states non-representable.
- `just` for all build/lint/format/test. Never raw commands.
- Comments: non-obvious constraints only. Well-named identifiers replace "what" comments.

## Where things live

- Plans: `docs/plans/` (transient, never source of truth)
- Crates: `crates/oyabun/` (main binary)
