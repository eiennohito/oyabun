# General

> **Meta-rule**: Rules include their rationale — rationales are followed more reliably and transfer to novel situations.
- **Do not be a yes-man**: Humans make bad decisions and forget context. Ask, clarify, push back.
- **Be terse**: every output token is 50× the cost of an input token and joins the context permanently. No filler, no preamble, no restating what the user said. Code and data over prose. Tables and bullet points over paragraphs. If a message doesn't add information, don't send it.
- **Premise before conclusion; define terms before using them**: never state a conclusion, recommendation, or question whose terms the reader hasn't been given — a name from your own code, an internal label, an intermediate result. Introduce each referent before you lean on it. A conclusion with the premise omitted, or a question that leaps over an unshared assumption ("I have two dogs — are you a mosquito?"), is noise the reader must reverse-engineer; it costs *more* than the words you saved. This sharpens "be terse" rather than fighting it: cut filler and restated context, never the logical chain or a definition the reader needs.

# Codebase Stage & Work Modes

Codebase is in active evolution. Existing code is provisional — preserve patterns only if they're actually correct.

**Work modes** (user sets or switches mid-session):
- **Evolve** (default): evolve domain model toward correct modeling. Refactors and rewrites welcome.
- **Analyze**: read logs/data, make plans, no code changes. "Triage", "investigate" = Analyze.
- **Meta**: improve interaction workflows.
- **Patch**: minimal diff. Never assume this mode.

**Implementation quality**: Correct on the first pass — upgrading a simplistic version costs ~3.5× more. Spend ~20% of effort cleaning up code around the change. Never propose "simple first, improve later" unless the user asks for patch mode.

**Design rules**:
- Domain objects over god services. Logic that needs one object's data belongs on that object.
- Make invalid states non-representable. Meaningless-without-each-other values are one type.
- Nouns are types. Verbs can be both methods and types. Free functions taking an object → method on that object.
- Don't patch with free functions, helpers, flags, or caches. Awkward behavior = wrong domain model. Redesign the type.

**Problem-solving**: don't patch symptoms — ask "what model change makes this problem structurally impossible?"
Before proposing a fix: what general capability does this case need? Does the system have it? If not, design the mechanism.

# Project Rules

## Environment
- **NEVER commit unless the user literally says "commit."** When committing, run `/precommit`.
- **Commit to `main` directly** — no feature branches. No remote/PR workflow yet, so branches are pure overhead.
- **Use `just`**, not raw commands. Never pipe `just` output through filters.
- **Both feature configs must be green.** `bpf` is default-on; `just precommit` runs `test` (bpf) and `test-nobpf` (`--no-default-features`) so a `cfg`-gated regression in either build is caught.
- **Default-suite tests can't hard-require optional external tools.** A test that needs an absent binary aborts the whole suite; mark it `#[ignore]` (with run instructions) or skip gracefully.

## Workflow

### Session resets
Planning and implementation are **separate sessions**. Plan session → plan doc in `docs/plans/` → `/clear` → impl session reads the plan doc.

### Plans are starting points
A plan is the best understanding at planning time — start with its definition, then improve on it.
The goal is not to deliver 100% of the plan; it's to deliver 120% at better quality.

### Implementation flow
**definition → exploration → discussion → implementation → initial check → precheck → final check → commit**

- **Review Mode**: for non-trivial changes, propose in text, wait for approval.
- **No silent descoping**: never silently drop, deprioritize, or exclude work. Report findings at equal weight — user prioritizes.

## Docs

`docs/` captures **why and invariants, never what the code is**. No identifiers, signatures, constants, env-var names, or code snippets — those live in the code and rot when duplicated. If a doc edit restates code, it's wrong; write the rationale instead. (A fenced block only for an actual illustrative example.) This is *why* conceptual docs survive refactors: the code shape can change without invalidating them.

## Project Structure

Module (not file) overviews, ≤60 chars each; a single-file module gets none.
Per-module detail lives in `docs/ARCHITECTURE.md` — read it before the code.

**`mod.rs` is aggregator-only.** A `mod.rs` is either (a) the module's *only* file and
under ~500 lines, or (b) — once the module has sibling files — nothing but `mod`
declarations, re-exports, and the module-level doc; all logic moves to a peer file (e.g.
`Gatherer` lives in `gather/gatherer.rs`, not `gather/mod.rs`). Rationale: the module's
entry point stays a readable map, and each file tracks one concept instead of accreting into
a grab-bag. A module small enough for one file uses `foo.rs`, not `foo/mod.rs`.

```
crates/
  atop/     process-manager TUI: one thread, serialized gather→render loop
    gather/ source (/proc or BPF) fills rows; CPU%, tree; common tail
    gather/bpf/ privileged source: emit-on-change iter + fork/free (feature "bpf")
  etch/     retained-mode, value-gated terminal renderer
  thoop/    THP storage primitives (MmapRegion, Arena, GenStore, TypedBuf, ThpMap, …)
bpf/        BPF C sources + committed .o (clang/bpftool dev-only; `just bpf`)
docs/       ARCHITECTURE.md (design — read first), GOALS.md, plans/
tools/      caprun.c — setuid cap wrapper (setup: scripts/setup-caps.sh)
scripts/    setup-caps.sh, perf-toggle.sh, run-logged.sh
```

## Project Goals

**atop** — a Rust CLI/TUI process manager. See `docs/GOALS.md` for full goals and
`docs/ARCHITECTURE.md` for the implemented design.
Linux (io_uring + syscall fallback) and macOS (`sysctl`/`libproc`). Windows is not a goal.
Core invariants: near-zero idle CPU, sub-ms input response, sub O(n) on process/core count when possible, safe sudo escalation.
