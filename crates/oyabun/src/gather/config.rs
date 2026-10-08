//! Gatherer tuning knobs: slot sizes, intervals, pool sizing, and the env-overridable
//! [`Config`]. Grouped so the knob set is one named thing rather than scattered single-use
//! readers. The slot-size and ring constants are also the contract the I/O backends
//! (`uring`/`syscall`) read, re-exported from the `gather` root.

use std::time::Duration;

use crate::sys::nofile_soft_limit;

/// `/proc/<pid>/stat` read-slot size — a **hard bound** (we only parse through field 21,
/// rss; the kernel may emit more, which we truncate-and-ignore). Derivation of the worst
/// case we must capture: `comm` is `TASK_COMM_LEN`-bounded (16, so ≤ 15 printable chars in
/// parens); fields 0–21 are ~22 small integers, each a `%d`/`%lu` whose widest (vsize,
/// starttime, the fault counters) is ~20 digits → pid + `(comm)` + ~22×~20 ≈ 418 bytes. A
/// single fixed 1 KiB slot covers that with large margin, so there is no two-tier promotion.
/// Used for both the `io_uring` landing-pad slots and the syscall scratch.
pub const STAT_SLOT: usize = 1024;
/// `/proc/<pid>/cmdline` slot. We never display >200 chars; if full detail is needed
/// later, re-read via a non-batched API.
pub const CMD_SLOT: usize = 256;
/// `io_uring` SQ depth. A new PID costs 2 SQEs (open+read), a cached PID 1; the bounded
/// fill/reap loop submits in rounds, so this only bounds in-flight concurrency.
pub(crate) const RING_ENTRIES: u32 = 4096;
/// Default landing-pad read-slot count — the in-flight read bound for the `io_uring` backend.
/// The pad is `READ_SLOTS × STAT_SLOT` bytes of pinned, registered memory (fixed regardless of
/// PID count). Larger ⇒ fewer wait rounds per cycle at high PID counts but more pinned memory;
/// 512 × 1 KiB = 512 KiB is a comfortable, lock-limit-friendly default. Overridable via
/// `OYA_READ_SLOTS` — the pinned-memory ↔ wakeups dial.
pub(crate) const READ_SLOTS: usize = 512;
/// fds reserved outside the persistent pool: the `/proc` dir fd, the ring, stdio, the
/// transient kill pidfd + cmdline/uid reads, and headroom.
const RESERVED_FDS: u64 = 64;
/// Upper bound on the persistent-fd pool — caps held kernel `struct file`s (and the
/// fixed-file table) even when `RLIMIT_NOFILE` is enormous.
pub(crate) const MAX_POOL: u32 = 4096;
/// Initial process-row buffer capacity (rows; grows via the arena). Sized to cover a typical
/// box without a regrow; a busier host grows automatically.
pub(crate) const INITIAL_ROWS: usize = 4096;

/// Display refresh / gather interval — the loop gathers this often.
pub(crate) const REFRESH_MS: u64 = 500;
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(REFRESH_MS);

/// Wall-clock target for a full `/proc` re-enumeration (§3): the periodic full `getdents`
/// scan catches a birth at most this late, so K = `ENUM_WALL_MS / REFRESH_MS` cycles.
const ENUM_WALL_MS: u64 = 1000;
/// Birth-probe window width W: candidate PIDs probed just above the live max per skip cycle.
const PROBE_WIDTH: u32 = 8;

/// Persistent-fd pool capacity: `min(RLIMIT_NOFILE.soft − RESERVED, MAX_POOL)`, or the
/// `OYA_POOL_CAP` override (exercise the overflow path without touching `ulimit`).
pub(crate) fn pool_capacity() -> u32 {
    if let Some(n) = env_u32("OYA_POOL_CAP") {
        return n.max(1);
    }
    let soft = nofile_soft_limit().saturating_sub(RESERVED_FDS);
    u32::try_from(soft).unwrap_or(MAX_POOL).clamp(1, MAX_POOL)
}

/// `OYA_FORCE_SYSCALL` forces the syscall backend even when `io_uring` is available
/// (exercises the fallback / persistent-fd floor on a modern kernel).
pub(crate) fn force_syscall() -> bool {
    std::env::var_os("OYA_FORCE_SYSCALL").is_some()
}

/// Gatherer tuning knobs, read once from the environment. Grouped so the knob set is one
/// named thing rather than scattered single-use readers.
pub(crate) struct Config {
    /// Full-scan interval K — cycles between full `getdents` re-enumerations; skip cycles
    /// reuse the maintained live set + birth probe. Derived from a wall-clock target so
    /// birth latency is `≤ K × interval`. `OYA_ENUM_EVERY` (1 = full scan every cycle).
    pub(crate) enum_every: u64,
    /// Birth-probe window width — candidates probed just above the live max per skip cycle.
    /// `OYA_PROBE_WIDTH`.
    pub(crate) probe_width: u32,
    /// Per-round CQE wait target for the `io_uring` backend (the I/O↔parse overlap dial):
    /// default a full ring ⇒ submit the batch and wait once for all of it (no overlap, since
    /// parse is now cheap). `OYA_URING_BATCH_CAP` lowers it to re-enable overlap.
    pub(crate) batch_cap: usize,
    /// `io_uring` landing-pad slot count — the in-flight read bound and the pinned-memory
    /// ↔ wakeups dial (pad = `read_slots × STAT_SLOT`, fixed). `OYA_READ_SLOTS`.
    pub(crate) read_slots: usize,
}

impl Config {
    pub(crate) fn from_env() -> Self {
        Self {
            enum_every: std::env::var("OYA_ENUM_EVERY")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or((ENUM_WALL_MS / REFRESH_MS).max(1))
                .max(1),
            probe_width: env_u32("OYA_PROBE_WIDTH").unwrap_or(PROBE_WIDTH).max(1),
            batch_cap: env_u32("OYA_URING_BATCH_CAP")
                .map_or(RING_ENTRIES as usize, |n| n.max(1) as usize),
            read_slots: env_u32("OYA_READ_SLOTS").map_or(READ_SLOTS, |n| {
                (n.max(1) as usize).min(RING_ENTRIES as usize)
            }),
        }
    }
}

pub(crate) fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok()?.parse().ok()
}
