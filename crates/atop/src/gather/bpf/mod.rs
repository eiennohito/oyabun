//! Privileged observation source: the BPF task iterator + fork/free tracepoints.
//!
//! Loaded once at startup ([`BpfSource::probe`]); any failure (missing caps, no BTF, old
//! kernel) returns `None` and the gatherer falls back to the unprivileged `/proc` source —
//! the privileged layer is simply absent, nothing else changes.
//!
//! **Emit-on-change.** The iterator walks every task each cycle (there is no kernel signal for
//! "a sleeping process's rss/utime moved", so polling is unavoidable), but the BPF program
//! writes a row only when the process's observable state *changed* — gated by a per-tgid
//! kernel hash of its **hot** fields (CPU time, run state, resident pages, parent). So the
//! `read()` stream userspace drains is O(changed), not O(all): the kernel walk cost stays, but
//! the `copy_to_user` + parse cost — the measured hot spot — collapses on an idle box. The
//! maintained full set lives in [`BpfSource`]; a cycle applies the delta to it, then
//! materializes it into the row buffer for the source-agnostic tail.
//!
//! Per cycle (`BpfSource::populate`):
//! - **task iterator** — one `read()` of a binary stream of `TaskInfo` structs, one per
//!   *changed* leader, interpreted in place via `zerocopy`; upserted into the maintained set.
//! - **fork/free ringbuf** — `fork` arms short-lived pairing; `free` (the reap, not exit, so a
//!   zombie stays visible until reaped) removes the row. A `free` for a never-walked fork is a
//!   **short-lived** process (born and gone between snapshots), which a snapshot-only tool can
//!   never see.
//! - **resync** — a forced full snapshot (`emit_all`) rebuilds the set from scratch, recovering
//!   any death whose `free` event was dropped on a ringbuf overflow. Armed by an overflow
//!   (the BPF drop counter increased) or a slow periodic backstop — *not* every cycle, because
//!   a full snapshot is exactly the cost emit-on-change exists to avoid.
//!
//! The committed `bpf/atop.bpf.o` is embedded at build time; CO-RE relocations are applied by
//! aya against the running kernel's BTF at load, so the same object works across kernel versions.

mod source;
mod types;

pub use source::BpfSource;
