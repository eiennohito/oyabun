use std::time::Instant;

use super::table::PidIndex;
use crate::procs::Procs;

/// Per-cycle side-band returned by an observation source after it materializes the live process
/// rows. `now` is the sample instant that anchors the per-process CPU window; the counters are
/// source-specific diagnostics surfaced in the footer.
#[derive(Clone, Copy)]
pub(crate) struct CycleResult {
    pub(crate) now: Instant,
    pub(crate) pool_overflow: u32,
    pub(crate) short_lived: u32,
}

/// Immutable context a source may need while filling a cycle. The PID index is read-only and
/// belongs to the table tail; `/proc` uses it only to distinguish known leaders from speculative
/// birth-probe threads.
#[derive(Clone, Copy)]
pub(crate) struct SourceCtx<'a> {
    pub(crate) index: &'a PidIndex,
    pub(crate) page_size: u64,
    pub(crate) prev_gen: u64,
}

/// A producer for the source-output shape of the process buffer.
///
/// Implementations must leave `procs` compacted, free of tombstones, and sorted by ascending PID
/// before returning. They populate only raw observation fields (`pid`, identity, stat counters,
/// memory, state, comm, kthread flag); the common table/tree/system tail derives CPU%, display
/// state, cmdline handles, metadata alarms, and subtree aggregates afterward.
pub(crate) trait Source {
    fn populate(&mut self, procs: &mut Procs, ctx: SourceCtx<'_>) -> CycleResult;
}
