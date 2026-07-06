//! The gatherer: a single-threaded cycle that fills the live process buffer from an
//! observation **source** (unprivileged `/proc` or privileged BPF), then runs a
//! source-agnostic tail — the per-PID table (CPU% + uid/cmdline), the tree build, and the
//! system-wide stats. Serialized with render by the owner: the borrow checker proves the two
//! never overlap, so there is no publish, no double buffer, and no cross-thread lease.
//!
//! Module map:
//! - [`gatherer`] — the [`Gatherer`] orchestrator + source selection (the cycle).
//! - [`procfs`] — the unprivileged `/proc` source (enumerate + birth probe + stat read).
//! - [`bpf`] — the privileged source (emit-on-change task iterator + fork/free), feature-gated.
//! - [`uring`] / [`syscall`] — the `/proc` source's two I/O backends (persistent-fd pool).
//! - [`parse`] — the `/proc/<pid>/stat` byte-wise parser.
//! - [`table`] — the per-PID [`ProcTable`](table::ProcTable) (CPU history + uid/cmdline + index).
//! - [`cpu`] — the per-process CPU% ring (per-core rate, moving average + peak).
//! - [`ring`] — the fixed-capacity wraparound ring both CPU samplers build on.
//! - [`sysstat`] — the per-cycle system-wide sampler (CPU/mem/load).
//! - [`config`] — env-overridable tuning knobs + the slot-size/ring constants.
//! - [`fxhash`] — the fast PID hasher shared by the backends and the BPF source.

#[cfg(feature = "bpf")]
mod bpf;
mod config;
mod cpu;
mod fxhash;
mod gatherer;
mod parse;
mod procfs;
mod ring;
mod syscall;
mod sysstat;
mod table;
mod uring;

pub use config::REFRESH_INTERVAL;
pub use gatherer::Gatherer;
