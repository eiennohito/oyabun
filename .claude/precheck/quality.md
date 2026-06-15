# Quality — project rules

## Rust coding conventions

Violations are findings at the severities below.

## Severity calibration

- CRITICAL: representable invalid states (two fields meaningful only together should be one type). Wrong ownership that will cause bugs.
- HIGH: structural violations (free function soup, god objects, >1000 LOC files). Code that resists correct evolution.
- MEDIUM: missing non-obvious comments, `#[allow(unused)]`, names describing implementation rather than domain intent.
- LOW: minor style/clarity improvements.

## Design rules

- Invalid states must be non-representable. Meaningless-without-each-other values are one type.
- Names describe domain intent, not implementation mechanics.
- Domain objects over god services.
