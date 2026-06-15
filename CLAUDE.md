# General

> **Meta-rule**: Rules include their rationale — rationales are followed more reliably and transfer to novel situations.
- **Do not be a yes-man**: Humans make bad decisions and forget context. Ask, clarify, push back.
- **Be terse**: every output token is 50× the cost of an input token and joins the context permanently. No filler, no preamble, no restating what the user said. Code and data over prose. Tables and bullet points over paragraphs. If a message doesn't add information, don't send it.

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

## Project Structure

```
crates/       Rust workspace crates
  atop/src/
    main.rs     terminal setup, thread spawn, UI event loop (drives etch::Display)
    app.rs      UI-thread state: selection, collapse, cached display rows
    ui.rs       rendering via etch: column Schema + per-frame value binding; sys-stat
                header, htop-style tree, Pct/Mem value-formatters (Display+Hash gates)
    snapshot.rs Snapshot / ProcessEntry / SystemStats (index-based, POD)
    arena.rs    HugePageBuf + StringRef (mmap/THP arena)
    tree.rs     index-based intrusive tree build + subtree aggregation
    sys.rs      sysconf, getdents64 enumeration, uid map, /proc/stat|meminfo|loadavg readers
    gather/     gatherer thread
      mod.rs    Gatherer, CpuTracker, Backend dispatch, double-buffer recycling
      parse.rs  zero-copy /proc/<pid>/stat parse + cmdline cleanup (+ non_ascii flag)
      syscall.rs  open/read/fstat fallback backend (+ test oracle)
      uring.rs    io_uring backend (two linked chains per PID: stat+cmdline, + statx)
  etch/src/     retained-mode, value-gated terminal renderer (no atop types)
    lib.rs      public API: Display/Frame/Table/Row, Schema/ColSpec, Cell, Line, Style/Color
    display.rs  Display + Frame/Table/Row builders + paint routines (the gate lives here)
    schema.rs   ColSpec/Schema/Align — column geometry, precomputed x-offsets
    cell.rs     Cell — width-tracked fill-column writer (ascii/glyph/unicode)
    line.rs     Line — free-form styled spans, content-hash gated
    hash.rs     GateHasher (FxHash) + gate()
    style.rs    Style/Color (crossterm wrapper)
    tests/integration.rs  vt100 render + gate tests (identical frame ⇒ 0 bytes)
docs/
  ARCHITECTURE.md  how it's built — read before touching code
  GOALS.md         goals/constraints
  plans/           active work-in-progress plans
```

## Project Goals

**atop** — a Rust CLI/TUI process manager. See `docs/GOALS.md` for full goals and
`docs/ARCHITECTURE.md` for the implemented design.
Linux (io_uring + syscall fallback) and macOS (`sysctl`/`libproc`). Windows is not a goal.
Core invariants: near-zero idle CPU, sub-ms input response, O(n) on process/core count, safe sudo escalation.
