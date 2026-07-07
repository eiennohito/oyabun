# Event-stream testing

Status: **partially implemented**

## Port status

Implemented:

- `atop-stream` crate: text DSL parser (stateful inheritance/deltas/birth/death), verbose
  writer, `ProcState` enum, `Stream::parse`/`Stream::load`/`Stream::to_verbose_dsl`
- `Recorder` (`gather/record.rs`): `ATOP_RECORD=path` live capture, feature-gated (`record`)
- Source-output replay through the common tail (`ReplaySource`, `Replayer`)
- All inline tests ported from `StreamBuilder` to `Stream::parse` DSL strings
- Synthetic CPU/display-state, birth/death, and terminal text smoke tests
- Tree reshuffling on reparenting
- Cmdline settling window and staggered refresh cadence
- Collapsed subtree CPU/memory aggregate rendering
- Full-frame 80×24 layout assertions
- `vt100::Cell`-based foreground/background color assertions for CPU, selection, and state cells

### Structural seams — done

The two structural changes to non-test code that the plan called for are complete:

1. **Source trait** (`gather/source.rs`): `trait Source { fn populate(...) }` with the
   source-output contract. Three implementations exist: `ProcSource` (unprivileged `/proc`),
   `BpfSource` (privileged, feature-gated), `ReplaySource` (`#[cfg(test)]`). The `Gatherer`
   uses an `ObservationSource` enum dispatching to all three (kept as an enum, not a trait
   object — the hot path monomorphizes).

2. **ProcReader trait** (`gather/table.rs`): `trait ProcReader` abstracts the per-PID metadata
   reads (`cmdline_uid`, `cap_eff`, `exe_deleted`, `lib_deleted`). `RealProcReader` does the
   `/proc` reads; `ReplaySource` implements it with in-memory lookup tables. `ProcTable::update`
   is generic over the reader.

### DSL crate (`atop-stream`) — done

Separate workspace crate with the text DSL parser and writer. Types: `RawProc`, `ProcState`
(enum with `TryFrom<u8>`), `SystemStats`, `CycleEvent`, `Stream`. The parser resolves
stateful inheritance, relative deltas (`ticks=+50`), relative cycle times (`+1s`), explicit
birth/death (`+ pid` / `- pid`), quoted values with escapes, and `#` comments. The verbose
writer emits full-state-per-cycle DSL (the recording format). Round-trip tested.

### Recording (`gather/record.rs`) — done

`Recorder` activated by `ATOP_RECORD=path`, feature-gated behind `record`. Converts live
`ProcessEntry` + `SystemStats` → `atop_stream` types → verbose DSL text → file append.
Wired into the gatherer cycle after the tree build and system stats.

### System stats — done, simplified

`CycleEvent` carries derived `SystemStats` directly (the plan's "simpler and sufficient"
option), not raw `/proc/stat` counters. The `Gatherer` detects replay mode and uses the
stream's stats instead of reading the host.

### Replayer — done

Wraps a replay-configured `Gatherer` + `App`. Exposes `proc(pid)`, `proc_idx(pid)`,
`cmdline(pid)`, `render(w, h)`, `select_pid`, `toggle_collapse`, `display_pids`, and
`Rendered` with `text()`, `row()`, `cell()`, `fg_at_text()`, `bg_at_text()`.

### Test coverage — done

All nine originally planned test scenarios are covered:

- CPU% ramp-up over N cycles
- Display-state stabilization (S→R when ticks accumulate)
- Birth/death across cycles
- Tree reshuffling on reparenting
- Cmdline settling window + staggered refresh cadence
- Full-frame layout at 80×24
- Collapsed subtree with aggregate CPU/mem
- CPU color gradient band crossings
- Selection highlight background

Future work remains:

- Python library for DSL read/write/subset
- Compact (delta) writer mode for minimized fixtures
- Replay of deleted-binary/capability/library signals
- GPU telemetry replay

## Problem

Tests today are either unit-level (CPU ring arithmetic, tree build, parse) or
live-system integration (spawn a real gatherer against `/proc`). There is no way to:

- Replay a deterministic sequence of process observations through the full pipeline
- Assert rendered terminal output for a known scenario
- Capture a live session for regression use

The gap grows as features land — every new column, color rule, or display-state
derivation needs a controlled multi-cycle scenario, and constructing one against a live
system is fragile (PIDs move, timing varies).

LLMs writing tests tend to underestimate scenario complexity — synthetic 3-process streams
test one axis at a time but miss bugs that emerge from overlapping transitions across
hundreds of processes. Captures from real systems are the ground truth the LLM can't
fabricate; the LLM's job is to subset them, not to imagine scenarios.

## Core idea

The observation source — the thing that fills the process buffer each cycle — **is** the
event emitter. Both existing sources (proc and BPF) already converge to the same shape:
a `Procs` buffer of `ProcessEntry` rows with identity + volatile stat fields populated,
CPU%/tree/cmdline-handle not yet derived. That convergence point is the event type.

A source that can emit events can also record them. A source that can replay recorded
events is a test source. So the source trait itself carries the seam — recording and
replay are just two more source implementations, not external hooks.

### What a cycle event contains

Everything the source writes, nothing the common tail derives:

| field | type | note |
|---|---|---|
| wall_ns | u64 | monotonic nanoseconds (synthetic or captured) |
| sys | SystemStats | derived system stats (not raw counters — see design constraints) |
| procs | Vec\<RawProc\> | one per live process this cycle |
| cmdlines | Vec\<(u32, Vec\<u8\>)\> | pid → resolved cmdline bytes |

`RawProc` — the source-output subset of `ProcessEntry`:
```
pid, ppid, uid, state, priority, nice, num_threads, ticks, mem_bytes,
start_time, comm, is_kthread
```

Missing: `cpu_pct`, `cpu_peak`, `display_state`, tree links, subtree aggregates,
`cmdline` handle, `exe_deleted`, `uses_deleted_lib`, `caps` — all derived by the common
tail. (The deleted-binary/capability signals come from `/proc` reads inside `ProcTable`,
not the source; the stream carries the inputs to those decisions, not their outputs.)

### What replaying a stream tests

The full common tail: CPU% computation (ring, moving average, peak), display-state
derivation (S→R stabilization), the cmdline cadence and settling window, tree build,
subtree aggregation, system-stats windowing — and then the renderer on top of it. One
stream, one replay, both the state-computation and UI pipelines exercised.

## Stream DSL

A text format that serves as **both** the inline test fixture format and the capture file
format. One representation everywhere — no separate binary codec.

### Design goals

- **Readable**: one line per process event, PID-leading, `key=value` fields
- **Compact**: field inheritance (absent process = unchanged from prior cycle), relative
  values for counters (`ticks=+50`), sensible defaults for births
- **Tool-friendly**: line-oriented, so `grep`/`awk`/Python `for line in f` work
- **Diffable**: text, so `git diff` shows exactly what changed in a fixture

### Format

```
cycle 0 cores=4 mem=8G
  1 ppid=0 uid=0 S comm=init cmd=/sbin/init ticks=100 mem=12M
  42 ppid=1 S comm=bash cmd=/bin/bash mem=3M

cycle +1s
  42 ticks=+50
  + 43 ppid=1 R comm=cc1

cycle +1s
  42 ticks=+50
  43 ticks=+500 mem=128M
  - 7

cycle +1s
```

#### Cycle lines

- `cycle <time>` starts a cycle; time is absolute (`0`, `5s`) or relative (`+1s`, `+500ms`)
- System stats follow as `key=value` on the same line, inherited when omitted
- An empty cycle (no process lines) means nothing changed — advances the wall clock only
- Blank lines and `#` comments are ignored

#### Process events

Every state transition is explicit:

- **First cycle**: all processes listed with their initial fields (implicit births)
- **`+ <pid> [fields...]`**: birth — omitted fields get defaults (uid=1000, state=S,
  nice=0, threads=1, ticks=0, mem=0)
- **`- <pid>`**: death
- **`<pid> [fields...]`**: update — only changed fields, rest inherited from prior cycle
- **Absent process**: unchanged, carried forward silently

#### Field values

- **Absolute** (replace): `mem=128M`, `state=R`, `ppid=2`, `uid=0`, `nice=-5`,
  `comm=worker`, `cmd=/usr/bin/worker`, `threads=4`
- **Relative** (delta): `ticks=+50` — add to prior value; natural for monotonic counters
- State is a bare character (`S`, `R`, `D`, `Z`, `T`)
- Time units: `ns`, `us`, `ms`, `s`
- Memory units: bare bytes, `K`, `M`, `G`

The format is **stateful** — cycle N's full state depends on cycles 0..N-1. This is fine:
replay is always sequential, and LLM subsetting reads the whole file.

### Two modes

**Raw captures** (`ATOP_RECORD` output): full state every cycle, no deltas, no inheritance.
Every process lists all fields every time. Verbose but self-contained per cycle — easier to
grep, easier for the LLM to read a single cycle in isolation when exploring.

**Minimized fixtures** (checked in): deltas, inheritance, only changed fields. Compact,
readable, diffable. The interesting transitions are visible because the noise is gone.

Same parser handles both — inheritance/deltas are optional, not required. A full-state line
is just an update that happens to specify every field. The recording emitter always writes
full state (simple, no bookkeeping). Minimization converts to the compact form.

### Four uses

1. **Inline test fixtures**: `Stream::parse(r"...")` in Rust tests — replaces `StreamBuilder`
2. **Captured files**: `Stream::load("fixtures/regression-123.atop")` — same parser
3. **LLM-authored subsets**: Python library reads a capture, ad-hoc script subsets it
4. **Human review**: readable in any editor, diffable in git

`StreamBuilder` was removed — `Stream::parse` / `Stream::load` are the only construction
paths.

## Python library

A small library (`tools/atopstream.py` or `python/atopstream/`) for reading, writing, and
subsetting DSL streams. This is the **durable** piece of the subsetting workflow — the
scripts that use it are throwaway, but the library itself is maintained.

Scope:

- **Parse** a DSL file into a resolved list of cycles (each cycle: wall time, system stats,
  dict of pid → full field set with inheritance/deltas applied)
- **Write** cycles back out in both modes (verbose full-state, compact delta)
- **Convenience filters**: `stream.keep_pids({1, 42, 43})`,
  `stream.window(start_cycle, end_cycle)`, `stream.ancestors(pid)` (transitive parent
  closure for tree validity)

Not a maintained minimizer API with a stable interface — just enough that a one-off
subsetting script is 5 lines instead of 50. Different bugs need different cropping logic;
the library provides the primitives, the script composes them.

## Data flow

```
┌─────────────┐    ┌─────────────┐    ┌───────────────┐
│ ProcSource  │    │ BpfSource   │    │ ReplaySource  │
│ (live /proc)│    │ (live BPF)  │    │ (DSL text)    │
└──────┬──────┘    └──────┬──────┘    └───────┬───────┘
       │                  │                   │
       │  ┌───────────────┘                   │
       │  │  (optionally wrapped in           │
       │  │   RecordingSource for capture)     │
       ▼  ▼                                   ▼
   ┌──────────────────────────────────────────────┐
   │  Procs buffer (source-output shape)          │
   ├──────────────────────────────────────────────┤
   │  Common tail:                                │
   │    ProcTable::update (CPU%, cmdline, uid,    │
   │      exe-deleted, caps, display-state)       │
   │    tree::build + tree::aggregate             │
   │    SystemSampler::update                     │
   ├──────────────────────────────────────────────┤
   │  App (display list, selection, scroll)       │
   ├──────────────────────────────────────────────┤
   │  ui::render → terminal output                │
   └──────────────────────────────────────────────┘
```

## Live capture

### Recorder — done

`Recorder` (`gather/record.rs`) emits verbose (full-state) DSL text after each gather
cycle, appending to a file. Activated by `ATOP_RECORD=path`. Feature-gated (`record`) so
the production binary pays zero cost. Not a source wrapper — it reads from the populated
`Procs` buffer and `ProcTable` cmdline store after the common tail completes, converting
live types to `atop_stream` types for serialization.

### Capture → fixture workflow

1. Run atop with `ATOP_RECORD=path` — captures the full session as verbose DSL text
2. Reproduce the scenario (CPU spike, process death, tree reshuffle, etc.)
3. LLM reads the capture, identifies the interesting region
4. LLM writes an ad-hoc Python script using the `atopstream` library to subset the capture
   (time window, PID filter, ancestor closure) and emit a compact-mode fixture
5. Fixture checked in under `tests/fixtures/`

The subsetting scripts are throwaway — different bugs need different cropping logic. The
Python library provides the parsing/writing/filtering primitives; the script composes them.

## Replay ProcReader gaps

The current `ReplaySource`'s `ProcReader` implementation stubs out deletion and
capability signals:

- `cap_eff` → always 0 (no capabilities)
- `exe_deleted` → always false
- `lib_deleted` → always false

These are adequate for the current test suite (which tests CPU, tree, cmdline, and
rendering), but mean the replay harness cannot yet test:

- `CapLevel::Partial`/`Full` classification and USER column coloring
- Deleted-binary alarms (exe tint, lib tint in the Command cell)

Extending the DSL with optional per-process fields (`caps=`, `exe_del`, `lib_del`) and
wiring the replay reader to return them is straightforward.

## GPU telemetry replay

The `NvmlSampler` produces per-device and per-process GPU metrics outside the `Source`
path (it runs in the gatherer's cycle after the tree build). Replaying GPU telemetry would
need either:

- A parallel GPU event in `CycleEvent` with per-PID utilization/memory
- Or a `GpuSource` trait analogous to `ProcReader`

Not yet designed. Low priority — GPU columns are sparse and the sampler itself has limited
coverage (NVML availability).

## Implementation sequence (remaining)

The DSL crate, recording, replay harness, and all inline tests are done. What remains:

1. **Python library** (`tools/atopstream.py`) — parse, write, filter DSL streams. Tested
   against the Rust parser for round-trip fidelity.

2. **Compact writer** — delta/inheritance mode for `Stream::to_compact_dsl()`, producing
   the minimized fixture format. The verbose writer exists; the compact writer is the
   counterpart for checked-in fixtures.

3. **ProcReader enrichment** — add cap/deletion fields to the DSL and the replay reader,
   then test the corresponding UI signals.

## Design constraints

- **No DSL crate in the hot path.** `Recorder` is feature-gated (`record`); `ReplaySource`
  is `#[cfg(test)]`. The production binary with neither pays zero cost — `atop-stream` is a
  dev-dependency only.
- **`ReplaySource` uses a real arena.** The CPU ring/cmdline store live on huge pages via
  the arena. Replay constructs a real `Arena` + `ProcTable` — the `RawProc` → row
  conversion is the same writes the real source does, so the common tail runs unchanged.
- **Source trait does not leak into the hot path's monomorphization.** Kept as an enum
  (`ObservationSource`) with `Replay` as a `#[cfg(test)]` variant — zero dispatch cost in
  release.
- **System-stats replay**: `CycleEvent` carries derived `SystemStats` directly and the
  sampler is bypassed. Simpler and sufficient — system stats are not the interesting test
  target.

## What this does NOT test

- The `/proc` enumeration and parse pipeline (already live-tested + unit-tested).
- The io_uring / syscall backend mechanics (the existing backend oracle test).
- Terminal I/O (crossterm raw mode, resize) — inherently interactive.
- BPF object load and CO-RE relocation — requires real caps + kernel BTF.
