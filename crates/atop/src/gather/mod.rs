//! The gatherer: enumerate `/proc`, read stat files (`io_uring` or syscall) into a fixed
//! landing pad, parse into the live process buffer (`comm` inline), compute CPU%, build the
//! tree. Single-threaded — the owner calls [`Gatherer::cycle`] then renders the same buffer;
//! the borrow checker proves the two never overlap, so there is no publish, no double buffer,
//! and no cross-thread lease.

#[cfg(feature = "bpf")]
mod bpf;
mod parse;
mod syscall;
mod uring;

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::time::{Duration, Instant};

use thoop::{Arena, Gen, GenStore, Ref, StrStore, StringRef, ThpMap};

use crate::procs::{Cmd, NONE, ProcessEntry, Procs, SystemStats};
use crate::sys::{self, ProcDir, ProcPath, RawCpuCounters, clk_tck, nofile_soft_limit};
use crate::tree;
use syscall::SyscallBackend;
use uring::UringBackend;

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
const RING_ENTRIES: u32 = 4096;
/// fds reserved outside the persistent pool: the `/proc` dir fd, the ring, stdio, the
/// transient kill pidfd + cmdline/uid reads, and headroom.
const RESERVED_FDS: u64 = 64;
/// Upper bound on the persistent-fd pool — caps held kernel `struct file`s (and the
/// fixed-file table) even when `RLIMIT_NOFILE` is enormous.
pub(crate) const MAX_POOL: u32 = 4096;
/// Initial process-row buffer capacity (rows; grows via the arena). Sized to cover a typical
/// box without a regrow; a busier host grows automatically.
const INITIAL_ROWS: usize = 4096;

/// Display refresh / gather cadence — the loop gathers this often.
const REFRESH_MS: u64 = 500;
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(REFRESH_MS);

/// CPU averaging / peak window as wall-clock time — the controllable knob (start:
/// 10 s). Longer = a calmer moving average and a longer spike memory.
const CPU_WINDOW_MS: u64 = 10_000;
/// Per-process history depth, derived as `window ÷ refresh` so the window stays
/// ~`CPU_WINDOW_MS` regardless of the refresh cadence.
const CPU_WINDOW: usize = {
    let n = (CPU_WINDOW_MS / REFRESH_MS) as usize;
    if n == 0 { 1 } else { n }
};
/// Below this elapsed interval the rate is too quantized (sub-jiffy) to be
/// meaningful — carry the windowed values forward instead of sampling.
const MIN_SAMPLE: Duration = Duration::from_millis(100);

/// A freshly-seen PID reads cmdline/uid every cycle for this many cycles (exec/argv
/// still settling — `nginx`/`postgres` rewrite their argv after exec) before dropping
/// to the coarse staggered cadence.
const CMDLINE_SETTLE_GENS: u32 = 3;
/// Default coarse cmdline/uid refresh cadence: PID `p` refreshes when
/// `(cur_gen + p) % N == 0`, so ~1/N settled PIDs refresh each cycle (no thundering herd).
/// At `REFRESH_MS=500` and `N=16`, worst-case staleness ≈ 8 s. See [`cmdline_refresh_n`].
const CMDLINE_REFRESH_N: u32 = 16;

/// The generational `Cmd` string store (cmdlines), keyed off the gatherer's `u64`
/// generation. Slots persist across cycles; an unchanged cmdline keeps its slot.
type CmdStore = StrStore<CMD_SLOT, Cmd>;

/// Largish initial slot count for the `Cmd` store, so it rarely grows after warmup
/// (growth relocates the chunk and retires the old region — fine, but a cold path).
const CMD_STORE_MIN_SLOTS: usize = 512;
/// Largish initial slot count for the per-PID metadata store (covers all live PIDs,
/// kthreads included). Grows via the arena if a box runs hotter.
const PIDMETA_MIN_SLOTS: usize = 2048;
/// Initial slot count for the per-PID CPU-history store — same population as metadata (every
/// PID, kthreads included, gets CPU tracked). Grows via the arena.
const CPURING_MIN_SLOTS: usize = 2048;
/// Initial capacity of the PID→`PidSlot` index (rounded to a power of two by the map). Sized
/// to hold a typical box's full PID set (kthreads included) below the grow threshold; a busier
/// host rehashes into a bigger arena chunk automatically.
const PIDINDEX_MIN_CAP: usize = 4096;

/// Persistent-fd pool capacity: `min(RLIMIT_NOFILE.soft − RESERVED, MAX_POOL)`, or the
/// `ATOP_POOL_CAP` override (exercise the overflow path without touching `ulimit`).
fn pool_capacity() -> u32 {
    if let Some(n) = env_u32("ATOP_POOL_CAP") {
        return n.max(1);
    }
    let soft = nofile_soft_limit().saturating_sub(RESERVED_FDS);
    u32::try_from(soft).unwrap_or(MAX_POOL).clamp(1, MAX_POOL)
}

/// `ATOP_FORCE_SYSCALL` forces the syscall backend even when `io_uring` is available
/// (exercises the fallback / persistent-fd floor on a modern kernel).
fn force_syscall() -> bool {
    std::env::var_os("ATOP_FORCE_SYSCALL").is_some()
}

/// Coarse cmdline/uid refresh cadence, overridable via `ATOP_CMDLINE_REFRESH_N`
/// (1 = refresh every PID every cycle).
fn cmdline_refresh_n() -> u32 {
    env_u32("ATOP_CMDLINE_REFRESH_N")
        .unwrap_or(CMDLINE_REFRESH_N)
        .max(1)
}

/// Wall-clock target for a full `/proc` re-enumeration (§3): the periodic full `getdents`
/// scan catches a birth at most this late, so K = `ENUM_WALL_MS / REFRESH_MS` cycles.
const ENUM_WALL_MS: u64 = 1000;
/// Birth-probe window width W: candidate PIDs probed just above the live max per skip cycle.
const PROBE_WIDTH: u32 = 8;

/// Gatherer tuning knobs, read once from the environment. Grouped so the knob set is one
/// named thing rather than scattered single-use readers.
struct Config {
    /// Full-scan cadence K — cycles between full `getdents` re-enumerations; skip cycles
    /// reuse the maintained live set + birth probe. Derived from a wall-clock target so
    /// birth latency is `≤ K × interval`. `ATOP_ENUM_EVERY` (1 = full scan every cycle).
    enum_every: u64,
    /// Birth-probe window width — candidates probed just above the live max per skip cycle.
    /// `ATOP_PROBE_WIDTH`.
    probe_width: u32,
    /// Per-round CQE wait target for the `io_uring` backend (the I/O↔parse overlap dial):
    /// default a full ring ⇒ submit the batch and wait once for all of it (no overlap, since
    /// parse is now cheap). `ATOP_URING_BATCH_CAP` lowers it to re-enable overlap.
    batch_cap: usize,
    /// `io_uring` landing-pad slot count — the in-flight read bound and the pinned-memory
    /// ↔ wakeups dial (pad = `read_slots × STAT_SLOT`, fixed). `ATOP_READ_SLOTS`.
    read_slots: usize,
}

impl Config {
    fn from_env() -> Self {
        Self {
            enum_every: std::env::var("ATOP_ENUM_EVERY")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or((ENUM_WALL_MS / REFRESH_MS).max(1))
                .max(1),
            probe_width: env_u32("ATOP_PROBE_WIDTH").unwrap_or(PROBE_WIDTH).max(1),
            batch_cap: env_u32("ATOP_URING_BATCH_CAP")
                .map_or(RING_ENTRIES as usize, |n| n.max(1) as usize),
            read_slots: env_u32("ATOP_READ_SLOTS").map_or(uring::READ_SLOTS, |n| {
                (n.max(1) as usize).min(RING_ENTRIES as usize)
            }),
        }
    }
}

fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok()?.parse().ok()
}

/// One measured interval: tick delta and the per-core jiffies it spanned.
#[derive(Clone, Copy, Default)]
struct Sample {
    ticks: u32,
    jiff: u32,
}

/// Bounded per-PID CPU history: a ring of the last [`CPU_WINDOW`] intervals plus exact `u64`
/// running sums. The displayed CPU% is the sum-weighted moving **average** (stable);
/// [`peak`](Self::peak) is the max single-interval rate still in the window (captures a spike
/// for up to [`CPU_WINDOW`] intervals after it happens).
///
/// `Flat` (`Copy`, no heap) so it lives on huge pages in a `GenStore<CpuRing>`. It is the
/// **hot** per-PID record — touched on every sample — and is deliberately a *separate* store
/// from the cold [`PidMeta`] so a CPU update loads only this, not metadata cache lines.
/// Liveness/incarnation bookkeeping (`seen_gen`, `start_time`) lives in [`PidSlot`], not here.
#[derive(Clone, Copy)]
struct CpuRing {
    /// `utime + stime` at the last sample.
    prev_ticks: u64,
    samples: [Sample; CPU_WINDOW],
    next: usize,
    len: usize,
    sum_ticks: u64,
    sum_jiff: u64,
    /// Cached peak rate (bp). Recomputed lazily only when the peak sample is evicted.
    peak_bp: u32,
    /// Ring index of the sample that produced `peak_bp` (or `usize::MAX` = dirty).
    peak_at: usize,
}

impl CpuRing {
    fn new(prev_ticks: u64) -> Self {
        Self {
            prev_ticks,
            samples: [Sample::default(); CPU_WINDOW],
            next: 0,
            len: 0,
            sum_ticks: 0,
            sum_jiff: 0,
            peak_bp: 0,
            peak_at: usize::MAX,
        }
    }

    /// Discard history (PID reuse / counter reset), re-baselining at `ticks`.
    fn reset(&mut self, ticks: u64) {
        *self = Self::new(ticks);
    }

    /// Fold one cycle's observation into the ring. `reused` ⇒ a new incarnation took this PID,
    /// so discard the dead one's history. Otherwise, given a real measurement window (`jiff`)
    /// for an already-`known` PID, push the tick delta — or reset if the counter went
    /// backwards (wrap). A brand-new PID just keeps its baseline (first real rate lands next
    /// interval); a too-soon/first cycle (no window) carries the windowed values forward.
    fn sample(&mut self, ticks: u64, jiff: Option<u32>, reused: bool, known: bool) {
        if reused {
            self.reset(ticks);
            return;
        }
        let Some(j) = jiff else { return };
        if !known {
            return;
        }
        if ticks < self.prev_ticks {
            self.reset(ticks);
        } else {
            let delta = u32::try_from(ticks - self.prev_ticks).unwrap_or(u32::MAX);
            self.push(delta, j);
            self.prev_ticks = ticks;
        }
    }

    fn push(&mut self, ticks: u32, jiff: u32) {
        let evicting_peak = self.len == CPU_WINDOW && self.next == self.peak_at;
        if self.len == CPU_WINDOW {
            let evicted = self.samples[self.next];
            self.sum_ticks -= u64::from(evicted.ticks);
            self.sum_jiff -= u64::from(evicted.jiff);
        } else {
            self.len += 1;
        }
        let idx = self.next;
        self.samples[idx] = Sample { ticks, jiff };
        self.sum_ticks += u64::from(ticks);
        self.sum_jiff += u64::from(jiff);
        self.next = (self.next + 1) % CPU_WINDOW;

        let new_rate = rate(u64::from(ticks), u64::from(jiff));
        if new_rate >= self.peak_bp {
            self.peak_bp = new_rate;
            self.peak_at = idx;
        } else if evicting_peak {
            // The old peak was just evicted — rescan to find the new max.
            self.peak_at = usize::MAX;
        }
    }

    /// Sum-weighted moving average over the window (exact integer math).
    fn avg(&self) -> u32 {
        rate(self.sum_ticks, self.sum_jiff)
    }

    /// Max single-interval rate still in the window. O(1) in the common case;
    /// O(`CPU_WINDOW`) only when the previous peak sample is evicted (~1/window).
    fn peak(&mut self) -> u32 {
        if self.peak_at == usize::MAX {
            // Dirty: rescan.
            let (mut best, mut best_at) = (0u32, 0usize);
            for i in 0..self.len {
                let s = &self.samples[i];
                let r = rate(u64::from(s.ticks), u64::from(s.jiff));
                if r >= best {
                    best = r;
                    best_at = i;
                }
            }
            self.peak_bp = best;
            self.peak_at = best_at;
        }
        self.peak_bp
    }
}

/// Fast non-cryptographic hasher (`FxHash`-style) for internal integer keys. The
/// default `RandomState` (`SipHash`) resists hash-flooding but is slow; PIDs here
/// are not attacker-controlled, so we trade that resistance for speed on the
/// per-cycle PID→history lookups.
#[derive(Default)]
struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add(u64::from(b));
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i)); // the hot path: u32 PID keys
    }
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[derive(Default, Clone)]
struct FxBuildHasher;

impl BuildHasher for FxBuildHasher {
    type Hasher = FxHasher;
    fn build_hasher(&self) -> FxHasher {
        FxHasher::default()
    }
}

/// PID-keyed map using the fast hasher above.
type PidMap<V> = HashMap<u32, V, FxBuildHasher>;

/// Per-core elapsed time in jiffies, floored at 1 to keep [`rate`] division safe.
fn elapsed_jiffies(elapsed: Duration, clk_tck: u64) -> u64 {
    (u64::try_from(elapsed.as_micros())
        .unwrap_or(u64::MAX)
        .saturating_mul(clk_tck)
        / 1_000_000)
        .max(1)
}

/// CPU% in basis points: `ticks / jiffies` (10000 = one full core). 0 if no window.
fn rate(ticks: u64, jiff: u64) -> u32 {
    if jiff == 0 {
        return 0;
    }
    u32::try_from(ticks.saturating_mul(10000) / jiff).unwrap_or(u32::MAX)
}

/// Slow-changing per-PID metadata (uid + cmdline handle) on huge pages in a
/// `GenStore<PidMeta>`. `Flat` (`Copy`) — no heap. The row carries the resolved `uid` and a
/// cmdline handle, so a dead PID's `PidMeta` slot is freed immediately (no reader leases it).
/// The **cold** counterpart to [`CpuRing`] — a separate store so a CPU sample never loads it.
/// Incarnation/cadence bookkeeping lives in [`PidSlot`], not here.
#[derive(Clone, Copy)]
struct PidMeta {
    uid: u32,
    /// Handle to this PID's cmdline in the `Cmd` store (empty for kthreads / no argv). The
    /// slot persists across cycles; replaced (old freed, new alive) only when the cmdline
    /// bytes actually change — no per-cycle re-copy.
    cmd: StringRef<Cmd>,
    /// The cmdline contains a byte ≥ 0x80 (renderer unicode path).
    cmd_non_ascii: bool,
}

impl PidMeta {
    const EMPTY: PidMeta = PidMeta {
        uid: u32::MAX,
        cmd: StringRef::EMPTY,
        cmd_non_ascii: false,
    };
}

/// The per-PID index value — what one PID lookup yields, coordinating every per-PID store. It
/// holds the store handles plus the cross-store bookkeeping the fill (and a future
/// update-prioritization scan) reads on every PID *without* chasing into a store. This is the
/// intended **extension point**: process-volatility signals for adaptive sampling land here.
/// `Flat` (`Copy`); lives in the THP-resident [`PidIndex`].
#[derive(Clone, Copy)]
struct PidSlot {
    /// Hot CPU-history slot.
    cpu: Ref<CpuRing>,
    /// Cold metadata slot (uid + cmdline handle).
    meta: Ref<PidMeta>,
    /// Incarnation discriminator: `start_time` from stat. A change ⇒ PID reuse.
    start_time: u64,
    /// Generation first seen — drives the age-adaptive cmdline cadence (settling window).
    first_seen_gen: u64,
    /// Generation last seen — evicts vanished PIDs.
    seen_gen: u64,
}

/// PID → [`PidSlot`], the per-cycle randomly-probed lookup that coordinates every per-PID
/// store. Resident in the shared arena (a `thoop::ThpMap`) so even this table is on huge pages
/// — the same TLB win as the stores it indexes. The map's fixed `u32` key is the PID; PID 0 is
/// the map's empty sentinel and never a real process (the scheduler is not a `/proc` entry).
type PidIndex = ThpMap<u32, PidSlot>;

/// The unified per-PID table: one PID lookup serves every store. Owns the shared [`PidIndex`]
/// plus the per-PID stores — the **hot** [`CpuRing`] (touched every sample), the **cold**
/// [`PidMeta`] (uid/cmdline), and the `Cmd` string store — and the CPU
/// sampling clock. Replacing the former separate `CpuTracker` + `ProcCache`, it collapses the
/// two per-PID hashmap lookups per cycle into one, and gives volatility-based update
/// prioritization a single place to read per-PID state ([`PidSlot`]).
///
/// **CPU%** is a per-core rate over real elapsed time (`Δticks / (Δwall · CLK_TCK)`), so one
/// full core reads 100% and a multithreaded process exceeds it (`top`/`htop` "Irix mode";
/// dividing by the machine-wide `/proc/stat` tick sum undercounts by `1/num_cpus`). The window
/// is a monotonic clock (a jittering interval self-corrects); a refresh closer than
/// [`MIN_SAMPLE`] (e.g. the forced one after a kill) carries the windowed values forward
/// rather than dividing by a near-zero window.
///
/// Single thread means **no lease**: a CPU/meta slot *and* a changed/dead cmdline slot are all
/// freed immediately (render of the prior cycle finished before this gather began, so nothing
/// reads a slot across the boundary). `CpuRing`/`PidMeta` stay *separate* stores so a CPU
/// update loads only the hot ring, never the cold metadata.
struct ProcTable {
    /// PID → [`PidSlot`], THP-resident (`thoop::ThpMap`) — on the arena like the stores.
    index: PidIndex,
    /// Hot per-PID CPU history on huge pages. Freed immediately on death.
    cpu: GenStore<CpuRing>,
    /// Cold per-PID metadata on huge pages. Freed immediately on death.
    meta: GenStore<PidMeta>,
    /// Generational cmdline storage, persistent across cycles: an unchanged cmdline keeps its
    /// slot (no per-cycle re-copy); a changed/dead one is freed at once (no reader lease).
    cmd_store: CmdStore,
    /// Last real CPU-sample instant (the window origin); `None` until the first sample.
    last: Option<Instant>,
    clk_tck: u64,
    refresh_n: u32,
    /// The active source already fills the row's `uid` (the BPF task iterator carries it), so
    /// the cmdline read here must **not** overwrite it — a failed/permission-denied cmdline
    /// open would otherwise clobber a good uid with `u32::MAX`. The `/proc` path leaves this
    /// `false`: there, uid has no source but this table's cmdline-fd `fstat`.
    source_provides_uid: bool,
    cmd_path: ProcPath,
    /// Reused read buffer for `/proc/<pid>/cmdline` (no per-cycle allocation).
    scratch: Vec<u8>,
}

impl ProcTable {
    fn new(arena: &Arena, clk_tck: u64, refresh_n: u32, source_provides_uid: bool) -> Self {
        Self {
            index: PidIndex::new(arena, PIDINDEX_MIN_CAP),
            cpu: GenStore::new(arena, CPURING_MIN_SLOTS),
            meta: GenStore::new(arena, PIDMETA_MIN_SLOTS),
            cmd_store: CmdStore::new(arena, CMD_STORE_MIN_SLOTS),
            last: None,
            clk_tck,
            refresh_n,
            source_provides_uid,
            cmd_path: ProcPath::new(),
            scratch: vec![0u8; CMD_SLOT],
        }
    }

    /// Bind the stores to the (now pinned, boxed) arena. Call once after construction, before
    /// any update — each store caches a `*const Arena` and its current base.
    fn wire(&mut self, arena: &Arena) {
        self.index.wire(arena);
        self.cpu.wire(arena);
        self.meta.wire(arena);
        self.cmd_store.wire(arena);
    }

    /// Resolve a row's cmdline bytes directly from the `Cmd` store (`&self` read; no lease).
    fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
        self.cmd_store.get(e.cmdline)
    }

    /// One per-PID pass filling CPU% + uid + cmdline, then evicting vanished PIDs. `now` drives
    /// the CPU window; `cur_gen` keys the cmdline cadence. CPU% reads fresh every cycle;
    /// uid/cmdline read fresh on first sighting, while settling, or on the staggered coarse
    /// tick, else reuse the cached handle (no I/O, no copy) — an unchanged cmdline keeps its
    /// slot, a changed one frees the old at once.
    ///
    /// The PID's index slot is **read out by value** ([`PidIndex::get_entry`]) at the top and
    /// **written back** at the bottom — a known PID through [`PidIndex::update_at`] using the
    /// slot index from that same probe (no second probe), a birth through [`PidIndex::insert`].
    /// A borrow into the index may not be held across the middle: a store `insert`/`intern`
    /// there can grow and trigger a regime-B repack that relocates the index's own chunk. The
    /// **slot index** survives that — it is a logical position, re-homed only by a map
    /// insert/remove, neither of which runs mid-PID — which is why caching it is sound where
    /// caching a pointer would not be.
    ///
    /// For the same reason each row is **copied out, mutated locally, and written back** —
    /// never held as a `&row` across a store op. The arena-resident stores (and now the index)
    /// self-heal their own bases on the next access; the only rule the caller keeps is not to
    /// span an allocation with a live reference into any arena chunk.
    fn update(&mut self, procs: &mut Procs, now: Instant, cur_gen: u64) {
        // CPU window: real elapsed since the last sample, or `None` if too soon / first call.
        let window = self
            .last
            .map(|t| now.saturating_duration_since(t))
            .filter(|e| *e >= MIN_SAMPLE);
        let first = self.last.is_none();
        let jiff =
            window.map(|e| u32::try_from(elapsed_jiffies(e, self.clk_tck)).unwrap_or(u32::MAX));

        let refresh_n = self.refresh_n;

        for i in 0..procs.as_slice().len() {
            let mut e = procs.row(i); // copy out — no live row ref spans a store op below
            let pid = e.pid;
            // One probe: read the prior slot *and* its slot index. The value is copied out; the
            // index is kept (not a pointer) to write back after the per-PID store work, which
            // can relocate the index's chunk but never re-homes the slot (see the method doc).
            let prior_entry: Option<(usize, PidSlot)> = self.index.get_entry(pid);
            let prior: Option<PidSlot> = prior_entry.map(|(_, s)| s);
            let reused = prior.is_some_and(|s| s.start_time != e.start_time);
            let settling = prior.is_some_and(|s| {
                !reused && cur_gen.wrapping_sub(s.first_seen_gen) < u64::from(CMDLINE_SETTLE_GENS)
            });

            // CPU ring (hot): mutated in place; its &mut never spans the cmd/meta ops below.
            let cpu_ref = match prior {
                Some(s) => s.cpu,
                None => self.cpu.insert(Gen::ALIVE, CpuRing::new(e.ticks)),
            };
            {
                let ring = self.cpu.get_mut(cpu_ref);
                ring.sample(e.ticks, jiff, reused, prior.is_some());
                e.cpu_pct = ring.avg();
                e.cpu_peak = ring.peak();
            }

            // Metadata (cold): uid + cmdline on the cadence.
            let stagger =
                refresh_n <= 1 || cur_gen.wrapping_add(u64::from(pid)) % u64::from(refresh_n) == 0;
            let refresh = prior.is_none() || reused || settling || stagger;
            let fresh = if refresh {
                if e.is_kthread {
                    // kthreads: uid 0, empty cmdline, no syscall.
                    Some(CmdlineRead {
                        uid: 0,
                        len: 0,
                        non_ascii: false,
                    })
                } else {
                    Some(read_cmdline_uid(pid, &mut self.cmd_path, &mut self.scratch))
                }
            } else {
                None
            };

            let meta_ref = match prior {
                Some(s) => s.meta,
                None => self.meta.insert(Gen::ALIVE, PidMeta::EMPTY),
            };
            let pm = self.refresh_meta(meta_ref, reused, fresh);

            // uid: the BPF source already set it on the row; the `/proc` source has no other
            // source than this table's read, so take it from the refreshed metadata.
            if !self.source_provides_uid {
                e.uid = pm.uid;
            }
            e.cmdline = pm.cmd;
            if !pm.cmd.is_empty() {
                e.non_ascii |= pm.cmd_non_ascii;
            }

            let slot = PidSlot {
                cpu: cpu_ref,
                meta: meta_ref,
                start_time: e.start_time,
                first_seen_gen: match prior {
                    Some(s) if !reused => s.first_seen_gen,
                    _ => cur_gen,
                },
                seen_gen: cur_gen,
            };
            // Write the slot back: a known PID overwrites at its already-probed slot (no second
            // probe); a birth inserts (may rehash → relocate sibling chunks, all self-healed).
            match prior_entry {
                Some((slot_idx, _)) => self.index.update_at(slot_idx, slot),
                None => self.index.insert(pid, slot),
            }

            procs.set(i, e); // write back
        }

        self.evict(cur_gen);

        // The window origin advances only on a real sample (or the first call); a too-soon
        // cycle leaves it so elapsed keeps accumulating until it exceeds `MIN_SAMPLE`.
        if first || window.is_some() {
            self.last = Some(now);
        }
    }

    /// Drop PIDs not seen this cycle: free their hot + cold slots and their cmd slot. All
    /// immediate — the dead PID's row was compacted out before this pass, so nothing references
    /// any of its slots.
    fn evict(&mut self, cur_gen: u64) {
        let Self {
            index,
            cpu,
            meta,
            cmd_store,
            ..
        } = self;
        index.retain(|_, slot| {
            if slot.seen_gen == cur_gen {
                true
            } else {
                let cmd = meta.get(slot.meta).cmd;
                cmd_store.free(cmd);
                meta.free(slot.meta);
                cpu.free(slot.cpu);
                false
            }
        });
    }

    /// Update this PID's cold metadata slot and return the new record (also written back).
    /// Carries the cached record forward, or resets on reuse (freeing the dead incarnation's cmd
    /// slot — immediate, no lease). A `fresh` uid/cmdline read replaces the cmd slot **only when
    /// the bytes changed** (free old, intern new); an unchanged or not-refreshed cmdline keeps
    /// its slot. The cleaned cmdline bytes are in `self.scratch[..len]` from a prior
    /// [`read_cmdline_uid`].
    fn refresh_meta(
        &mut self,
        meta_ref: Ref<PidMeta>,
        reused: bool,
        fresh: Option<CmdlineRead>,
    ) -> PidMeta {
        let mut pm = if reused {
            self.cmd_store.free(self.meta.get(meta_ref).cmd);
            PidMeta::EMPTY
        } else {
            *self.meta.get(meta_ref)
        };
        if let Some(CmdlineRead {
            uid,
            len,
            non_ascii,
        }) = fresh
        {
            pm.uid = uid;
            let new_bytes = &self.scratch[..len];
            if new_bytes.is_empty() {
                self.cmd_store.free(pm.cmd); // no-op if already empty
                pm.cmd = StringRef::EMPTY;
                pm.cmd_non_ascii = false;
            } else if pm.cmd.is_empty() || self.cmd_store.get(pm.cmd) != new_bytes {
                // Changed (or first non-empty argv): free the old slot, intern the new.
                self.cmd_store.free(pm.cmd);
                pm.cmd = self.cmd_store.intern(Gen::ALIVE, new_bytes);
                pm.cmd_non_ascii = non_ascii;
            }
            // else: unchanged — keep the existing slot (the whole point of the store).
        }
        self.meta.assign(meta_ref, pm);
        pm
    }
}

/// One `/proc/<pid>/cmdline` read: owner `uid` (from the same fd's `fstat`), the cleaned byte
/// length in the caller's scratch, and whether any byte is ≥ 0x80. A named record so the three
/// are not a bare positional tuple at the call site.
#[derive(Clone, Copy)]
struct CmdlineRead {
    uid: u32,
    len: usize,
    non_ascii: bool,
}

/// Read `/proc/<pid>/cmdline` into `scratch` and the owner `uid` from the same fd's `fstat`
/// ([`CmdlineRead`]); `uid` is still valid when the cmdline is empty. The cleaned bytes live in
/// `scratch[..len]`.
fn read_cmdline_uid(pid: u32, path: &mut ProcPath, scratch: &mut [u8]) -> CmdlineRead {
    let ptr = path.write(pid, b"cmdline");
    // SAFETY: valid C path, read-only.
    let fd = unsafe { libc::open(ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return CmdlineRead {
            uid: u32::MAX,
            len: 0,
            non_ascii: false,
        };
    }
    // SAFETY: scratch is a valid writable region; fd is open.
    let (n, uid) = unsafe {
        let n = libc::read(fd, scratch.as_mut_ptr().cast(), scratch.len());
        let mut st: libc::stat = std::mem::zeroed();
        let uid = if libc::fstat(fd, &raw mut st) == 0 {
            st.st_uid
        } else {
            u32::MAX
        };
        libc::close(fd);
        (n, uid)
    };
    if n <= 0 {
        return CmdlineRead {
            uid,
            len: 0,
            non_ascii: false,
        };
    }
    let raw = usize::try_from(n).unwrap_or(0).min(scratch.len());
    let (clean_len, non_ascii) = parse::clean_cmdline(&mut scratch[..raw]);
    CmdlineRead {
        uid,
        len: clean_len as usize,
        non_ascii,
    }
}

/// Tracks cumulative `/proc/stat` CPU counters to compute deltas.
struct SysCpuAccum {
    prev: Option<RawCpuCounters>,
    num_cores: u32,
}

impl SysCpuAccum {
    fn new() -> Self {
        Self {
            prev: None,
            num_cores: sys::num_cpus(),
        }
    }

    /// Read current counters, compute basis-point rates from the delta, and fill
    /// `sys` with CPU + memory + load stats. The first call baselines only.
    fn update(&mut self, sys: &mut SystemStats) {
        let cur = sys::read_cpu_counters();
        if let Some(prev) = &self.prev {
            let d_user = cur.user.wrapping_sub(prev.user) + cur.nice.wrapping_sub(prev.nice);
            let d_sys = cur.system.wrapping_sub(prev.system)
                + cur.irq.wrapping_sub(prev.irq)
                + cur.softirq.wrapping_sub(prev.softirq);
            let d_iowait = cur.iowait.wrapping_sub(prev.iowait);
            let d_total = cur.total().wrapping_sub(prev.total()).max(1);

            sys.cpu_user_bp = u32::try_from(d_user * 10000 / d_total).unwrap_or(u32::MAX);
            sys.cpu_sys_bp = u32::try_from(d_sys * 10000 / d_total).unwrap_or(u32::MAX);
            sys.cpu_iowait_bp = u32::try_from(d_iowait * 10000 / d_total).unwrap_or(u32::MAX);
        }
        self.prev = Some(cur);
        sys.num_cores = self.num_cores;

        let mem = sys::read_meminfo();
        sys.mem_total = mem.total;
        sys.mem_used = mem.total.saturating_sub(mem.available);
        sys.mem_cached = mem.buffers.saturating_add(mem.cached);
        sys.swap_total = mem.swap_total;
        sys.swap_used = mem.swap_total.saturating_sub(mem.swap_free);

        sys.load = sys::read_loadavg();
        sys.uptime_secs = sys::read_uptime_secs();
    }
}

enum Backend {
    Uring(Box<UringBackend>),
    Syscall(SyscallBackend),
}

impl Backend {
    /// Build the I/O backend on the **calling thread**, which is the single thread that will
    /// also submit on it — the `io_uring` backend's single-issuer flags bind the ring's
    /// submitter task to its creator. Probes `io_uring` (`ATOP_FORCE_SYSCALL` or any probe
    /// failure falls back to the syscall backend). The backend owns its own fixed landing pad,
    /// so probing needs nothing from the process buffer.
    fn from_config(pool_cap: u32, config: &Config) -> Backend {
        if force_syscall() {
            return Backend::Syscall(SyscallBackend::new(pool_cap));
        }
        match UringBackend::probe(pool_cap, RING_ENTRIES, config.batch_cap, config.read_slots) {
            Some(u) => Backend::Uring(Box::new(u)),
            None => Backend::Syscall(SyscallBackend::new(pool_cap)),
        }
    }

    /// Re-read every live PID's stat into `procs`. Returns the overflow count (PIDs that
    /// exceeded the persistent-fd pool and used the transient fallback).
    fn collect(&mut self, pids: &[u32], procs: &mut Procs, page_size: u64) -> std::io::Result<u32> {
        match self {
            Backend::Uring(u) => u.collect(pids, procs, page_size),
            Backend::Syscall(s) => Ok(s.collect(pids, procs, page_size)),
        }
    }
}

/// The unprivileged `/proc` observation source: maintained live set + cadenced `getdents`
/// re-scan + skip-cycle birth probe, with an `io_uring`/syscall [`Backend`] reading each PID's
/// stat. This is everything the gatherer's cycle does *before* the per-PID table / tree build —
/// extracted so a privileged [`Source`] can replace the whole enumerate-and-read step.
struct ProcState {
    /// I/O backend, built once on the owning thread (the sole ring submitter).
    backend: Backend,
    proc_dir: ProcDir,
    pids: Vec<u32>,
    dent_buf: Vec<u8>,
    /// Persistent-fd pool capacity, retained for the mid-run `io_uring`→syscall downgrade.
    pool_cap: u32,
    config: Config,
    /// `pid_max` (the PID-counter wrap point), read once at startup — bounds the probe
    /// window so it never generates an impossible PID.
    pid_max: u32,
}

impl ProcState {
    fn new(proc_dir: ProcDir) -> Self {
        let pool_cap = pool_capacity();
        let config = Config::from_env();
        let backend = Backend::from_config(pool_cap, &config);
        Self {
            backend,
            proc_dir,
            pids: Vec::new(),
            dent_buf: vec![0u8; 64 * 1024],
            pool_cap,
            config,
            pid_max: sys::read_pid_max(),
        }
    }

    /// Fill `procs` with the live set's stat fields, leaving it **compacted and PID-sorted**.
    /// `prev_gen` (the just-finished generation) drives the enumeration cadence and the
    /// skip-cycle leader gate; `index` is the per-PID table's index, read by that gate to tell
    /// a known PID from a probe-introduced non-leader thread. Returns the sample instant and
    /// the pool-overflow count.
    fn populate(
        &mut self,
        procs: &mut Procs,
        index: &PidIndex,
        page_size: u64,
        prev_gen: u64,
    ) -> (Instant, u32) {
        // Enumeration cadence (§3): a full `getdents` re-scan every K cycles is the resync
        // that catches any birth the probe missed (non-sequential, burst > W, post-wrap);
        // skip cycles reuse the maintained live set plus a cheap sequential-birth probe.
        // The set is rebuilt from survivors below, so deaths drop the same cycle (held read →
        // `ESRCH` → re-tombstone → compact) — the full scan is only for births, never pruning.
        if prev_gen.is_multiple_of(self.config.enum_every) {
            self.proc_dir.read_pids(&mut self.dent_buf, &mut self.pids);
            self.pids.sort_unstable();
            self.pids.dedup();
        } else {
            self.probe_births();
        }
        let now = Instant::now();

        Self::prepare(procs, &self.pids);

        let overflow = if let Ok(overflow) = self.backend.collect(&self.pids, procs, page_size) {
            overflow
        } else {
            // io_uring failed mid-cycle: drop to syscall permanently and redo the fill.
            self.backend = Backend::Syscall(SyscallBackend::new(self.pool_cap));
            Self::prepare(procs, &self.pids);
            self.backend
                .collect(&self.pids, procs, page_size)
                .unwrap_or(0)
        };

        // Before compact: rows with real PIDs but unfilled (state='?' = TOMBSTONE default) are
        // rows `fill` never submitted a read for AND never tombstoned.
        #[cfg(debug_assertions)]
        {
            for (i, r) in procs.as_slice().iter().enumerate() {
                if r.pid != 0 && r.state == b'?' {
                    eprintln!(
                        "[UNFILLED] idx={i} pid={} ppid={} state=? mem={} ticks={} start_time={}",
                        r.pid, r.ppid, r.mem_bytes, r.ticks, r.start_time,
                    );
                }
            }
        }

        procs.compact();

        // Thread-leader backstop (syscall backend): the uring backend rejects non-leader
        // threads inline; the syscall backend has no inline filter, so this post-compact pass
        // catches any non-leader thread a probe introduced (uring: a no-op — already gone).
        if !prev_gen.is_multiple_of(self.config.enum_every) {
            let mut re_compact = false;
            for row in procs.as_mut_slice() {
                if index.get(row.pid).is_none() && !sys::is_thread_group_leader(row.pid) {
                    row.pid = 0;
                    re_compact = true;
                }
            }
            if re_compact {
                procs.compact();
            }
        }

        // Rebuild the maintained live set from survivors (PID-sorted, since `compact` preserves
        // order): deaths and probe misses fall out now, confirmed births stay.
        self.pids.clear();
        self.pids.extend(procs.as_slice().iter().map(|p| p.pid));

        (now, overflow)
    }

    /// Skip-cycle birth probe (§3a): append up to `probe_width` candidate PIDs just above
    /// the highest live PID, bounded by the kernel's allocation frontier (`ns_last_pid`).
    /// PIDs are allocated near-monotonically, so a freshly-forked process almost always
    /// takes a number above the current max; `collect` opens these speculatively — survivors
    /// join the maintained set, misses fail their open and are re-tombstoned (§3b, the
    /// hygiene that makes speculative opens safe). In steady state `ns_last_pid` equals our
    /// max, so the window is empty and the probe costs **zero** opens. A burst > W, a
    /// non-sequential birth, or a post-wrap low PID waits for the next full scan — a probe
    /// hit-rate, never a correctness, concern, because the full scan is the backstop.
    ///
    /// **Precondition**: `self.pids` is sorted ascending and holds the current live set.
    fn probe_births(&mut self) {
        debug_assert!(
            self.pids.windows(2).all(|w| w[0] <= w[1]),
            "probe_births requires a sorted live set"
        );
        let w = self.config.probe_width;
        let Some(&anchor) = self.pids.last() else {
            return;
        };
        let frontier = sys::read_ns_last_pid().unwrap_or(anchor.saturating_add(w));
        let hi = anchor.saturating_add(w).min(frontier).min(self.pid_max);
        for cand in anchor.saturating_add(1)..=hi {
            self.pids.push(cand);
        }
        #[cfg(debug_assertions)]
        {
            let n_added = hi.saturating_sub(anchor);
            if n_added > 0 {
                eprintln!("[probe] anchor={anchor} frontier={frontier} hi={hi} added={n_added}");
            }
        }
    }

    /// Reset and pre-size the process buffer, seeding a tombstone for every PID in sorted
    /// order. `reserve` keeps the per-cycle tombstone fill from relocating the buffer chunk
    /// mid-fill — a performance choice, since a grow self-heals harmlessly.
    fn prepare(procs: &mut Procs, pids: &[u32]) {
        procs.clear();
        procs.reserve(pids.len());
        for &pid in pids {
            procs.push_tombstone(pid);
        }
    }
}

/// Where the live process rows come from. Selected once at startup ([`Gatherer::with_source`]):
/// the privileged BPF source if it loads (caps present), else the unprivileged `/proc` source.
/// The rest of a cycle — the per-PID table, tree build, system stats — is source-agnostic.
enum Source {
    Proc(Box<ProcState>),
    #[cfg(feature = "bpf")]
    Bpf(Box<bpf::BpfSource>),
}

/// The single-threaded data producer: fills the live [`Procs`] buffer from a [`Source`], then
/// builds the per-PID table + tree + system stats. Owns the THP arena, the per-PID stores, and
/// the process buffer the renderer reads directly. No `ArcSwap`, no double buffer, no channel —
/// [`cycle`](Self::cycle) runs to completion before the caller renders, so the borrow checker
/// proves gather and render never alias.
pub struct Gatherer {
    /// Shared THP sub-allocator backing the per-PID stores **and** the process buffer.
    /// **Boxed so it is pinned**: every store/buffer caches a `*const Arena` (set at `wire`),
    /// so the gatherer may move freely while the arena's heap address stays fixed.
    arena: Box<Arena>,
    /// The live process rows on a huge page — reset and refilled each cycle, read in place.
    procs: Procs,
    /// The observation source (privileged BPF or unprivileged `/proc`), chosen at startup.
    source: Source,
    /// Unified per-PID table (CPU history + uid/cmdline + the shared PID index).
    table: ProcTable,
    sys_cpu: SysCpuAccum,
    /// System-wide stats for this cycle.
    sys: SystemStats,
    /// Head of the root sibling chain (via `next_sibling`), or [`NONE`].
    first_root: u32,
    /// Live PIDs this cycle that exceeded the pool and used the transient fallback (0 common;
    /// `/proc` source only — the BPF source has no fd pool).
    pool_overflow: u32,
    /// Processes that were born and died between this and the previous cycle, caught only by
    /// the BPF fork/exit events (the snapshot never saw them). 0 in `/proc` mode.
    short_lived: u32,
    tree_stack: Vec<u32>,
    tree_order: Vec<u32>,
    page_size: u64,
    generation: u64,
}

impl Gatherer {
    /// Build the gatherer on the **calling thread**, which must stay the only thread that uses
    /// it: the arena + stores are `!Send`, and the `io_uring` ring binds to its creator. The
    /// arena is boxed (pinned) so the stores/buffer can cache a `*const Arena` during `wire`.
    /// Probes the privileged BPF source first; on success the `/proc` backend is never built.
    /// `proc_dir` is opened by the caller (a fallible op kept out of here).
    #[must_use]
    pub fn new(page_size: u64, proc_dir: ProcDir) -> Self {
        Self::with_source(page_size, proc_dir, true)
    }

    /// Like [`new`](Self::new) but `allow_bpf` gates the privileged probe — `false` forces the
    /// `/proc` source (the proc-path tests assert proc-specific behaviour and must not flip to
    /// BPF when run under `caprun`).
    fn with_source(page_size: u64, proc_dir: ProcDir, allow_bpf: bool) -> Self {
        let clk_tck = clk_tck();
        let arena = Box::new(Arena::new(0));

        // Probe the privileged source first; its presence decides who owns uid. Its landing
        // buffer is allocated in the shared arena (wired below, once the arena is pinned).
        #[cfg(feature = "bpf")]
        let bpf = if allow_bpf {
            bpf::BpfSource::probe(&arena, page_size, clk_tck)
        } else {
            None
        };
        #[cfg(not(feature = "bpf"))]
        let bpf: Option<std::convert::Infallible> = {
            let _ = allow_bpf;
            None
        };

        let source_provides_uid = bpf.is_some();

        let mut table = ProcTable::new(&arena, clk_tck, cmdline_refresh_n(), source_provides_uid);
        table.wire(&arena); // arena is pinned (boxed) → stores may cache its address
        let mut procs = Procs::new(&arena, INITIAL_ROWS);
        procs.wire(&arena);

        let source = match bpf {
            #[cfg(feature = "bpf")]
            Some(mut b) => {
                b.wire(&arena); // bind the landing buffer now that the arena is pinned
                Source::Bpf(Box::new(b))
            }
            None => Source::Proc(Box::new(ProcState::new(proc_dir))),
        };

        Self {
            arena,
            procs,
            source,
            table,
            sys_cpu: SysCpuAccum::new(),
            sys: SystemStats::default(),
            first_root: NONE,
            pool_overflow: 0,
            short_lived: 0,
            tree_stack: Vec::new(),
            tree_order: Vec::new(),
            page_size,
            generation: 0,
        }
    }

    /// The live process rows (read by render).
    #[must_use]
    pub fn procs(&self) -> &Procs {
        &self.procs
    }

    /// System-wide stats for the latest cycle.
    #[must_use]
    pub fn sys(&self) -> &SystemStats {
        &self.sys
    }

    /// Head of the root sibling chain, or [`NONE`].
    #[must_use]
    pub fn first_root(&self) -> u32 {
        self.first_root
    }

    /// Live PIDs that overflowed the persistent-fd pool this cycle (0 in the common case;
    /// always 0 in privileged mode — there is no fd pool).
    #[must_use]
    pub fn pool_overflow(&self) -> u32 {
        self.pool_overflow
    }

    /// Short-lived processes (born+died between cycles) caught by the BPF fork/exit events
    /// this cycle — invisible to a snapshot-only tool. Always 0 in `/proc` mode.
    #[must_use]
    pub fn short_lived(&self) -> u32 {
        self.short_lived
    }

    /// Whether the privileged BPF source is active.
    #[must_use]
    #[cfg_attr(not(feature = "bpf"), allow(clippy::unused_self))] // always false without the feature
    pub fn is_privileged(&self) -> bool {
        #[cfg(feature = "bpf")]
        {
            matches!(self.source, Source::Bpf(_))
        }
        #[cfg(not(feature = "bpf"))]
        {
            false
        }
    }

    /// A row's cmdline bytes, resolved directly from the `Cmd` store (no lease).
    #[must_use]
    pub fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
        self.table.cmdline(e)
    }

    /// Run one gather cycle, filling [`procs`](Self::procs) + [`sys`](Self::sys) in place. The
    /// caller renders the same buffer afterward; the two never overlap, so this needs no
    /// publish, no swap, and no GC lease — slots are freed eagerly and retired arena regions
    /// reclaimed at once (`min_live` = the just-finished generation).
    pub fn cycle(&mut self) {
        // The generation being built. It advances every cycle, in lock-step with the per-PID
        // cadence and the arena's region-retirement tagging.
        let building_gen = self.generation + 1;
        self.arena.set_gen(building_gen);

        // Source-specific: fill `procs` with the live set (compacted, PID-sorted). The `/proc`
        // source enumerates + reads stat; the BPF source reads the task iterator + drains the
        // fork/exit ring. Both leave the same shape for the common tail below.
        let (now, pool_overflow, short_lived) = match &mut self.source {
            Source::Proc(p) => {
                let (now, overflow) = p.populate(
                    &mut self.procs,
                    &self.table.index,
                    self.page_size,
                    self.generation,
                );
                (now, overflow, 0)
            }
            #[cfg(feature = "bpf")]
            Source::Bpf(b) => {
                let (now, short_lived) = b.populate(&mut self.procs);
                (now, 0, short_lived)
            }
        };
        self.pool_overflow = pool_overflow;
        self.short_lived = short_lived;

        // One unified per-PID pass: CPU% + (uid +) cmdline handle on the surviving entries (a
        // single index lookup each).
        self.table.update(&mut self.procs, now, building_gen);
        self.first_root = tree::build(
            self.procs.as_mut_slice(),
            &mut self.tree_stack,
            &mut self.tree_order,
        );
        tree::aggregate(self.procs.as_mut_slice(), &self.tree_order);

        // System-wide stats (tiny reads, ~3 μs total).
        self.sys_cpu.update(&mut self.sys);
        self.sys.set_task_counts(self.procs.count_tasks());

        self.generation = building_gen;

        // Reclaim any arena region a store relocate (regime B) retired this cycle. With no
        // reader leasing the old bytes (render of the prior cycle is finished, this cycle's
        // is not started), `min_live` = the current generation reclaims immediately.
        self.arena.gc(building_gen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a gatherer on the current (test) thread (the single-threaded owner): the arena +
    /// stores are `!Send`, so they are constructed where they are used. Forces the `/proc`
    /// source (`allow_bpf = false`) — these tests assert proc-path behaviour (birth probe,
    /// same-cycle death, getdents flicker) and must not flip to BPF when run under `caprun`.
    fn new_test() -> Gatherer {
        let proc_dir = ProcDir::open().expect("open /proc");
        Gatherer::with_source(crate::sys::page_size(), proc_dir, false)
    }

    /// The `/proc` source's tuning knobs, for tests that drive the enumeration cadence.
    /// Panics if the gatherer is not in `/proc` mode (it always is here — `new_test`).
    fn proc_config(g: &mut Gatherer) -> &mut Config {
        match &mut g.source {
            Source::Proc(p) => &mut p.config,
            #[cfg(feature = "bpf")]
            Source::Bpf(_) => panic!("test gatherer must be /proc mode"),
        }
    }

    #[test]
    fn elapsed_jiffies_is_per_core() {
        // 500 ms at 100 Hz = 50 jiffies on one core, regardless of core count.
        assert_eq!(elapsed_jiffies(Duration::from_millis(500), 100), 50);
        assert_eq!(elapsed_jiffies(Duration::from_secs(1), 100), 100);
        // Floored at 1 so cpu_rate never divides by zero.
        assert_eq!(elapsed_jiffies(Duration::from_micros(1), 100), 1);
    }

    #[test]
    fn fx_hasher_is_deterministic_and_distinct() {
        let bh = FxBuildHasher;
        assert_eq!(bh.hash_one(1234_u32), bh.hash_one(1234_u32));
        assert_ne!(bh.hash_one(1_u32), bh.hash_one(2_u32));
    }

    #[test]
    fn rate_is_per_core_basis_points() {
        assert_eq!(rate(50, 50), 10000); // one full core = 100%
        assert_eq!(rate(200, 50), 40000); // four busy threads = 400%
        assert_eq!(rate(25, 50), 5000); // half a core
        assert_eq!(rate(0, 50), 0); // idle
        assert_eq!(rate(10, 0), 0); // empty window
    }

    #[test]
    fn moving_average_is_exact() {
        let mut h = CpuRing::new(0);
        h.push(25, 50); // 50%
        h.push(50, 50); // 100% → (25+50)/(100) = 75%
        assert_eq!(h.avg(), 7500);
    }

    #[test]
    fn moving_average_evicts_old_samples() {
        let mut h = CpuRing::new(0);
        for _ in 0..CPU_WINDOW {
            h.push(10, 50); // fill window with 20%
        }
        assert_eq!(h.avg(), 2000);
        for _ in 0..CPU_WINDOW {
            h.push(50, 50); // overwrite the whole window with 100%
        }
        assert_eq!(h.avg(), 10000, "old samples must be fully evicted");
    }

    #[test]
    fn peak_captures_spike_the_average_damps() {
        let mut h = CpuRing::new(0);
        h.push(5, 50); // 10%
        h.push(50, 50); // 100% spike
        h.push(5, 50); // 10%
        assert_eq!(h.peak(), 10000, "peak must hold the spike");
        assert!(h.avg() < 5000, "average must stay damped, got {}", h.avg());
    }

    fn enum_pids() -> Vec<u32> {
        let dir = ProcDir::open().unwrap();
        let mut dent = vec![0u8; 64 * 1024];
        let mut pids = Vec::new();
        dir.read_pids(&mut dent, &mut pids);
        pids.sort_unstable();
        pids
    }

    /// A wired, arena-backed `Procs` for direct-backend tests. The returned `Box<Arena>` must
    /// outlive the `Procs` (the buffer caches its pinned address).
    fn test_procs() -> (Box<Arena>, Procs) {
        let arena = Box::new(Arena::new(0));
        let mut procs = Procs::new(&arena, 256);
        procs.wire(&arena);
        (arena, procs)
    }

    fn name_of(p: &ProcessEntry) -> String {
        String::from_utf8_lossy(p.comm()).into_owned()
    }

    /// The `io_uring` backend must agree with the syscall oracle on stable **stat**
    /// fields for processes both scans observed. (uid/cmdline are no longer backend
    /// fields — the `ProcTable` owns them; CPU/ticks/mem can drift between scans; PID reuse
    /// can churn the set — so we anchor on PID 1 and self, and require broad agreement
    /// on the overlap.)
    #[test]
    fn uring_matches_syscall_backend() {
        let page_size = crate::sys::page_size();
        let pids = enum_pids();
        let pool_cap = pool_capacity();

        let (_aa, mut a) = test_procs();
        let (_ab, mut b) = test_procs();

        let Some(mut uring) = UringBackend::probe(
            pool_cap,
            RING_ENTRIES,
            RING_ENTRIES as usize,
            uring::READ_SLOTS,
        ) else {
            eprintln!("io_uring unavailable — skipping oracle comparison");
            return;
        };

        for &pid in &pids {
            a.push_tombstone(pid);
        }
        SyscallBackend::new(pool_cap).collect(&pids, &mut a, page_size);
        a.compact();

        for &pid in &pids {
            b.push_tombstone(pid);
        }
        uring
            .collect(&pids, &mut b, page_size)
            .expect("uring collect");
        b.compact();

        assert!(
            b.as_slice().len() > 10,
            "uring found too few: {}",
            b.as_slice().len()
        );

        let by_pid: HashMap<u32, ProcessEntry> = b.as_slice().iter().map(|p| (p.pid, *p)).collect();

        let mut common = 0;
        let mut name_matches = 0;
        for pa in a.as_slice() {
            if let Some(pb) = by_pid.get(&pa.pid) {
                common += 1;
                if name_of(pa) == name_of(pb) {
                    name_matches += 1;
                }
                if pa.pid == 1 || pa.pid == std::process::id() {
                    assert_eq!(pa.ppid, pb.ppid, "ppid mismatch for pid {}", pa.pid);
                    assert_eq!(
                        pa.is_kthread, pb.is_kthread,
                        "is_kthread mismatch for pid {}",
                        pa.pid
                    );
                    assert_eq!(name_of(pa), name_of(pb), "name mismatch pid {}", pa.pid);
                }
            }
        }
        assert!(common > 20, "too little overlap: {common}");
        // Allow a little churn, but the vast majority must match exactly.
        assert!(
            name_matches * 100 >= common * 95,
            "name agreement too low: {name_matches}/{common}"
        );
    }

    /// A process pegging a core must read as substantial per-core CPU (≫ the old
    /// `1/num_cpus` undercount). Spins a thread in *this* process and checks our own
    /// PID across a few real sample windows.
    #[test]
    fn busy_process_reads_per_core_cpu() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut x = 0u64;
            while !worker_stop.load(Ordering::Relaxed) {
                x = std::hint::black_box(x.wrapping_add(1));
            }
        });

        let mut g = new_test();
        let me = std::process::id();
        let (mut max_cpu, mut max_peak) = (0, 0);
        for _ in 0..4 {
            g.cycle();
            if let Some(p) = g.procs().as_slice().iter().find(|p| p.pid == me) {
                max_cpu = max_cpu.max(p.cpu_pct);
                max_peak = max_peak.max(p.cpu_peak);
            }
            std::thread::sleep(Duration::from_millis(300));
        }

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();

        // One pegged core is ~10000 bp; allow window ramp + scheduling slack.
        assert!(
            max_cpu > 3000,
            "busy process should average >30% per-core, got {max_cpu} bp"
        );
        assert!(
            max_peak >= max_cpu,
            "peak ({max_peak}) must capture at least the average ({max_cpu})"
        );
    }

    /// Full gather pipeline (enumerate → backend → cpu → tree).
    #[test]
    fn gather_fills_valid_buffer() {
        let mut g = new_test();
        g.cycle();
        let procs = g.procs().as_slice();

        assert!(g.generation >= 1);
        assert!(procs.len() > 10, "got {}", procs.len());
        assert!(procs.iter().any(|p| p.pid == 1), "pid 1 missing");
        assert_ne!(g.first_root(), NONE, "no roots");

        // PIDs sorted ascending (tree binary-search precondition).
        assert!(procs.windows(2).all(|w| w[0].pid < w[1].pid));

        // Every non-root parent index is in range and resolves to the right PID.
        for p in procs {
            if p.parent_idx != NONE {
                let parent = &procs[p.parent_idx as usize];
                assert_eq!(parent.pid, p.ppid, "parent_idx points at wrong pid");
            }
        }

        assert_eq!(g.pool_overflow(), 0, "default pool should not overflow");

        // The table filled the slow fields: pid 1 is root-owned; this test process has a
        // non-empty cmdline; kernel threads are flagged and have no cmdline.
        let init = procs.iter().find(|p| p.pid == 1).expect("pid 1");
        assert_eq!(init.uid, 0, "pid 1 is owned by root");

        let me = procs
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("self");
        assert!(!me.is_kthread, "the test process is not a kernel thread");
        assert!(!me.cmdline.is_empty(), "self should have a cmdline");

        for p in procs {
            if p.is_kthread {
                assert!(
                    p.cmdline.is_empty(),
                    "kthread {} must have empty cmdline",
                    p.pid
                );
            }
        }
    }

    /// Steady-state throughput sanity / profiling target. Ignored by default (timing
    /// is machine-dependent); run with:
    /// `cargo test -p atop --release gather_steady_state -- --ignored --nocapture`.
    /// After warmup, cached PIDs cost one stat read each — no opens, no closes.
    #[test]
    #[ignore = "timing-dependent; run manually for profiling"]
    #[allow(clippy::cast_precision_loss)] // a print, not a measurement
    fn gather_steady_state_is_cheap() {
        const ITERS: u32 = 400;
        let mut g = new_test();
        g.cycle();
        g.cycle(); // warm up the persistent-fd pool

        let start = Instant::now();
        for _ in 0..ITERS {
            g.cycle();
        }
        let elapsed = start.elapsed();
        let per_cycle_us = elapsed.as_micros() as f64 / f64::from(ITERS);
        eprintln!(
            "pids={} kthreads={} overflow={} per_cycle={per_cycle_us:.1}µs",
            g.procs().as_slice().len(),
            g.procs().as_slice().iter().filter(|p| p.is_kthread).count(),
            g.pool_overflow(),
        );
        assert_eq!(g.pool_overflow(), 0, "default pool should not overflow");
    }

    /// cmdline survives the coarse cadence: a PID not refreshed this cycle keeps its
    /// persistent `Cmd`-store slot, still resolvable directly from the store.
    #[test]
    fn cmdline_persists_across_coarse_cycles() {
        let mut g = new_test();
        let me = std::process::id();
        g.cycle();
        let cmd1 = {
            let p = *g
                .procs()
                .as_slice()
                .iter()
                .find(|p| p.pid == me)
                .expect("self c1");
            String::from_utf8_lossy(g.cmdline(&p)).into_owned()
        };
        assert!(!cmd1.is_empty(), "self cmdline should be present");

        // A second cycle (no time for argv to change) must still resolve the cmdline from
        // the persistent slot — no re-copy.
        g.cycle();
        let cmd2 = {
            let p = *g
                .procs()
                .as_slice()
                .iter()
                .find(|p| p.pid == me)
                .expect("self c2");
            String::from_utf8_lossy(g.cmdline(&p)).into_owned()
        };
        assert_eq!(cmd1, cmd2, "cmdline must persist across cycles");
    }

    /// The win: an unchanged cmdline is **not** re-interned each cycle, so the `Cmd` store's
    /// high-water slot count stays proportional to live processes, not to cycle count. If
    /// the old per-cycle re-materialization were still happening (or unchanged strings were
    /// re-interned), the store would grow by ~one slot per userspace PID per cycle.
    #[test]
    fn cmd_store_does_not_grow_per_cycle() {
        let mut g = new_test();
        let me = std::process::id();

        // Warm up: settle cmdlines and let births/deaths reach steady state.
        for _ in 0..6 {
            g.cycle();
        }
        let slots_warm = g.table.cmd_store.slot_count();
        let userspace = g
            .procs()
            .as_slice()
            .iter()
            .filter(|p| !p.is_kthread)
            .count();

        for _ in 0..30 {
            g.cycle();
        }
        let slots_after = g.table.cmd_store.slot_count();

        // Slack covers genuine churn (new userspace PIDs over ~18 s); the disaster mode
        // would be `userspace × 30` extra slots. (Freed slots are also reused now, so this is
        // an even tighter bound than under the old demote/gc lease.)
        let growth = slots_after - slots_warm;
        assert!(
            growth <= userspace.max(64),
            "Cmd store grew by {growth} slots over 30 cycles (userspace pids ≈ {userspace}); \
             unchanged cmdlines are being re-interned"
        );

        // And the latest buffer still resolves self correctly.
        let p = *g
            .procs()
            .as_slice()
            .iter()
            .find(|p| p.pid == me)
            .expect("self");
        assert!(!g.cmdline(&p).is_empty(), "self cmdline must resolve");
    }

    /// §3a: a sequentially-allocated new process is caught by the skip-cycle birth probe
    /// within **1** cycle — not only at the next full `getdents` scan. Full scans are
    /// disabled (`enum_every` huge) so the birth can *only* be found by the probe.
    #[test]
    fn birth_probe_catches_sequential_birth_within_one_cycle() {
        let mut g = new_test();
        proc_config(&mut g).enum_every = 1_000_000; // no full re-scan → births only via probe
        proc_config(&mut g).probe_width = 1_000_000; // window spans (max_live, ns_last_pid]
        g.cycle(); // generation 0 → full scan seeds the maintained live set
        assert!(
            g.procs().as_slice().iter().any(|p| p.pid == 1),
            "set seeded"
        );

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();

        g.cycle(); // generation 1 → SKIP cycle: probe must discover the new child
        let caught = g.procs().as_slice().iter().any(|p| p.pid == child_pid);

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            caught,
            "sequential birth (pid {child_pid}) must be caught by the probe within 1 cycle"
        );
    }

    /// Detect the getdents flicker: PIDs that oscillate in/out of the survivor set
    /// across full-scan / skip-cycle alternation. Uses the real gather `cycle()` with
    /// Electron to trigger the kernel `/proc` enumeration gotcha.
    #[test]
    fn probe_flicker_detection() {
        use std::collections::HashSet;
        use std::process::Command;
        use std::thread;

        let app_dir = std::path::PathBuf::from("/tmp/atop-electron-test");
        let _ = std::fs::create_dir_all(&app_dir);
        std::fs::write(
            app_dir.join("index.js"),
            b"const{app}=require('electron');\
              app.on('ready',()=>{setTimeout(()=>process.exit(),60000)});",
        )
        .expect("write electron app");

        let mut g = new_test();
        proc_config(&mut g).enum_every = 2;
        proc_config(&mut g).probe_width = 8;

        // Seed.
        g.cycle();

        // Launch Electron, let it settle.
        let mut electron = Command::new("electron39")
            .args(["--no-sandbox"])
            .arg(&app_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn electron39");
        thread::sleep(Duration::from_secs(2));

        // Track PID sets across cycles to find oscillations.
        let mut prev_pids: HashSet<u32> = HashSet::new();
        let mut added_hist: HashMap<u32, u32> = HashMap::new();
        let mut removed_hist: HashMap<u32, u32> = HashMap::new();

        for _round in 0..60 {
            thread::sleep(Duration::from_millis(500));
            g.cycle();

            let cur_pids: HashSet<u32> = g.procs().as_slice().iter().map(|p| p.pid).collect();

            if !prev_pids.is_empty() {
                for &pid in &cur_pids {
                    if !prev_pids.contains(&pid) {
                        *added_hist.entry(pid).or_default() += 1;
                    }
                }
                for &pid in &prev_pids {
                    if !cur_pids.contains(&pid) {
                        *removed_hist.entry(pid).or_default() += 1;
                    }
                }
            }
            prev_pids = cur_pids;
        }

        let _ = electron.kill();
        let _ = electron.wait();

        // A PID that was both added AND removed multiple times is oscillating.
        let mut oscillators = Vec::new();
        for (&pid, &adds) in &added_hist {
            let removes = removed_hist.get(&pid).copied().unwrap_or(0);
            if adds >= 2 && removes >= 2 {
                oscillators.push((pid, adds, removes));
            }
        }
        oscillators.sort_unstable();

        if !oscillators.is_empty() {
            eprintln!("=== {} oscillating PIDs ===", oscillators.len());
            for &(pid, adds, removes) in &oscillators {
                eprint!("  pid={pid} added={adds}x removed={removes}x");
                // Read /proc/<pid>/stat
                let stat_path = format!("/proc/{pid}/stat");
                if let Ok(stat) = std::fs::read_to_string(&stat_path) {
                    eprint!(" stat={}", stat.trim());
                } else {
                    eprint!(" stat=GONE");
                }
                // Read key fields from /proc/<pid>/status
                let status_path = format!("/proc/{pid}/status");
                if let Ok(status) = std::fs::read_to_string(&status_path) {
                    for line in status.lines() {
                        if line.starts_with("Tgid:")
                            || line.starts_with("Pid:")
                            || line.starts_with("PPid:")
                            || line.starts_with("Name:")
                            || line.starts_with("Threads:")
                        {
                            eprint!(" {}", line.trim());
                        }
                    }
                }
                eprintln!();
            }
        }

        assert!(
            oscillators.is_empty(),
            "{} PIDs oscillate across cycles (getdents flicker):\n  {}",
            oscillators.len(),
            oscillators
                .iter()
                .map(|(pid, a, r)| format!("pid={pid} +{a}x -{r}x"))
                .collect::<Vec<_>>()
                .join("\n  "),
        );
    }

    /// §3b + cadence: a process that dies vanishes the **same** (skip) cycle via the pool
    /// `ESRCH` → re-tombstone → compact → survivor-rebuild path — it never lingers until
    /// the next full scan, and no phantom row is left behind.
    #[test]
    fn death_caught_same_cycle_on_skip() {
        let mut g = new_test();
        proc_config(&mut g).enum_every = 1_000_000; // force skip cycles after the first
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();

        g.cycle(); // generation 0 → full scan sees the (still-live) child
        assert!(
            g.procs().as_slice().iter().any(|p| p.pid == child_pid),
            "child must be observed while alive"
        );

        child.kill().unwrap();
        child.wait().unwrap();

        g.cycle(); // generation 1 → SKIP cycle: held read ESRCH ⇒ death caught now
        let procs = g.procs().as_slice();
        assert!(
            !procs.iter().any(|p| p.pid == child_pid),
            "death must be caught the same skip cycle, not deferred to the next full scan"
        );
        assert!(
            procs.iter().all(|p| p.state != b'?'),
            "no phantom row may survive the death"
        );
    }

    /// Full privileged cycle through the real [`Gatherer`]: source selection picks BPF, and the
    /// source-agnostic tail (CPU%, cmdline via `/proc`, tree build) runs over the iterator's
    /// rows. In particular the BPF-provided uid must survive the cmdline refresh (the
    /// `source_provides_uid` guard) and the cmdline must still resolve. Skips without caps;
    /// run under `tools/caprun cargo test`.
    #[test]
    fn bpf_mode_end_to_end() {
        let proc_dir = ProcDir::open().expect("open /proc");
        let mut g = Gatherer::with_source(crate::sys::page_size(), proc_dir, true);
        if !g.is_privileged() {
            eprintln!("not privileged (no caps?) — skipping bpf end-to-end");
            return;
        }

        g.cycle();
        g.cycle(); // second cycle exercises CPU% windowing + cmdline cadence in bpf mode

        let procs = g.procs().as_slice();
        assert!(procs.iter().any(|p| p.pid == 1), "pid 1 missing");
        assert!(
            procs.windows(2).all(|w| w[0].pid < w[1].pid),
            "rows must be PID-sorted"
        );
        assert_ne!(g.first_root(), NONE, "tree must have a root");
        assert_eq!(g.pool_overflow(), 0, "no fd pool in bpf mode");

        let me = std::process::id();
        let mine = *procs.iter().find(|p| p.pid == me).expect("self");
        assert_eq!(
            mine.uid,
            unsafe { libc::getuid() },
            "uid kept from the iterator"
        );
        assert!(
            !g.cmdline(&mine).is_empty(),
            "cmdline must resolve via /proc in bpf mode"
        );

        // Every non-root parent index resolves to the right PID (tree integrity).
        for p in procs {
            if p.parent_idx != NONE {
                assert_eq!(procs[p.parent_idx as usize].pid, p.ppid);
            }
        }
    }
}
