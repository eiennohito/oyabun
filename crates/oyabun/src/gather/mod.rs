//! The gatherer: a single-threaded cycle that fills the live process buffer from an
//! observation **source** (unprivileged `/proc` or privileged BPF), then runs a
//! source-agnostic tail — the per-PID table (CPU% + uid/cmdline), the tree build, and the
//! system-wide stats. Serialized with render by the owner: the borrow checker proves the two
//! never overlap, so there is no publish, no double buffer, and no cross-thread lease.
//!
//! Module map:
//! - [`gatherer`] — the [`Gatherer`] orchestrator + source selection (the cycle).
//! - [`source`] — the shared source-output contract each observation source implements.
//! - [`procfs`] — the unprivileged `/proc` source (enumerate + birth probe + stat read).
//! - [`bpf`] — the privileged source (emit-on-change task iterator + fork/free), feature-gated.
//! - [`replay`] — deterministic in-memory streams and render harness for tests.
//! - [`uring`] / [`syscall`] — the `/proc` source's two I/O backends (persistent-fd pool).
//! - [`parse`] — the `/proc/<pid>/stat` byte-wise parser.
//! - [`table`] — the per-PID [`ProcTable`](table::ProcTable) (CPU history + uid/cmdline + index).
//! - [`cpu`] — the per-process CPU% ring (per-core rate, short box average + sparkline series).
//! - [`ring`] — the fixed-capacity wraparound ring both CPU samplers build on.
//! - [`sysstat`] — the per-cycle system-wide sampler (windowed CPU, pressure, memory, swap).
//! - [`cpu_history`] — the header sparkline's per-interval history and its stepped scale.
//! - [`config`] — env-overridable tuning knobs + the slot-size/ring constants.

#[cfg(feature = "bpf")]
mod bpf;
mod config;
mod cpu;
mod cpu_history;
mod gatherer;
mod nvml;
mod parse;
mod procfs;
#[cfg(feature = "record")]
mod record;
#[cfg(test)]
pub(crate) mod replay;
mod ring;
mod schedstat;
mod source;
mod syscall;
mod sysstat;
mod table;
mod uring;

pub use config::REFRESH_INTERVAL;
pub(crate) use config::REFRESH_MS;
pub(crate) use cpu::{CpuSeries, SPARK_CELLS, SeriesCell, Waiting, ns_per_tick};
#[cfg(test)]
pub use cpu_history::CpuCell;
pub use cpu_history::CpuHistory;
pub use gatherer::Gatherer;
pub(crate) use parse::read_stat;
