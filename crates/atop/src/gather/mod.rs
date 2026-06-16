//! The gatherer thread: enumerate `/proc`, read stat files (`io_uring` or syscall)
//! into a fixed landing pad, parse into the back snapshot (copying `comm` into its
//! string arena), compute CPU%, build the tree, and publish via `ArcSwap` using a
//! two-buffer recycling protocol.

mod parse;
mod syscall;
mod uring;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use thoop::{Arena, ByteResolver, Gen, GenStore, Ref, StrStore, StringRef};

use crate::snapshot::{Cmd, ProcessEntry, Snapshot, SystemStats};
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
/// Typical `comm` size for the arena reserve hint (`TASK_COMM_LEN` = 16, name ≤ 15 chars).
/// Unlike [`STAT_SLOT`] this is **not** a hard bound: the actual copy uses `comm.len()` and
/// the arena grows freely — see [`ARENA_BYTES_PER_PID`].
const COMM_SLOT: usize = 16;
/// Comm-arena bytes reserved per PID. Only `comm` lives in the per-cycle arena now —
/// cmdline moved to the generational `Cmd` store (no per-cycle re-copy). A **reserve sizing
/// hint only**; the arena grows freely if exceeded.
const ARENA_BYTES_PER_PID: usize = COMM_SLOT;
/// `io_uring` SQ depth. A new PID costs 2 SQEs (open+read), a cached PID 1; the bounded
/// fill/reap loop submits in rounds, so this only bounds in-flight concurrency.
const RING_ENTRIES: u32 = 4096;
/// fds reserved outside the persistent pool: the `/proc` dir fd, the ring, stdio, the
/// transient kill pidfd + cmdline/uid reads, and headroom.
const RESERVED_FDS: u64 = 64;
/// Upper bound on the persistent-fd pool — caps held kernel `struct file`s (and the
/// fixed-file table) even when `RLIMIT_NOFILE` is enormous.
pub(crate) const MAX_POOL: u32 = 4096;
/// Initial arena size per snapshot buffer (grows by doubling if exceeded).
/// 2 MiB ≈ 2700 PIDs at default slot sizes; grows automatically.
const INIT_BUF: usize = 2 * 1024 * 1024;

/// Display refresh / gather cadence — the gatherer produces a snapshot this often.
const REFRESH_MS: u64 = 500;
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(REFRESH_MS);

/// Control messages from the UI thread to the gatherer.
pub enum Ctrl {
    /// Gather immediately (e.g. right after a kill, so the change shows fast).
    Refresh,
    /// Stop the gatherer loop and exit the thread.
    Quit,
}

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
/// (growth relocates + retires the old chunk via the lease — fine, but a cold path).
const CMD_STORE_MIN_SLOTS: usize = 512;
/// Largish initial slot count for the per-PID metadata store (covers all live PIDs,
/// kthreads included). Grows via the arena if a box runs hotter.
const PIDMETA_MIN_SLOTS: usize = 2048;

/// How many generations a demoted `Cmd` slot is held before GC may reclaim it. The live
/// window is two snapshots (double buffer), so a lag of 2 is the minimum safe value; the
/// `u8` generation tag has ~127 generations of headroom, so this is purely conservative
/// margin (reclaim one generation later than strictly required, never sooner). See the GC
/// derivation in [`Gatherer::gather`].
const GC_LAG: u64 = 2;

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

/// Bounded per-PID history: a ring of the last [`CPU_WINDOW`] intervals plus exact
/// `u64` running sums. The displayed CPU% is the sum-weighted moving **average**
/// (stable); [`peak`](Self::peak) is the max single-interval rate still in the
/// window (captures a spike for up to [`CPU_WINDOW`] intervals after it happens).
struct CpuHistory {
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
    /// Tracker generation when last seen, for evicting vanished PIDs.
    seen_gen: u32,
}

impl CpuHistory {
    fn new(prev_ticks: u64, seen_gen: u32) -> Self {
        Self {
            prev_ticks,
            samples: [Sample::default(); CPU_WINDOW],
            next: 0,
            len: 0,
            sum_ticks: 0,
            sum_jiff: 0,
            peak_bp: 0,
            peak_at: usize::MAX,
            seen_gen,
        }
    }

    /// Discard history (PID reuse / counter reset), re-baselining at `ticks`.
    fn reset(&mut self, ticks: u64) {
        *self = Self::new(ticks, self.seen_gen);
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

/// Derives per-process CPU% as a **per-core rate over real elapsed time** —
/// `Δticks / (Δwall · CLK_TCK)`, so one fully-used core reads 100% and a
/// multi-threaded process can exceed 100% (matching `top`/`htop` "Irix mode").
/// Dividing instead by the `/proc/stat` machine-wide tick sum (an earlier approach)
/// yielded `1/num_cpus` of the truth — a 32× undercount on a 32-core box.
///
/// Each PID keeps a [`CpuHistory`]: the moving average is the stable reading, the
/// peak captures spikes. The window is a monotonic clock (a jittering gather
/// interval self-corrects), and refreshes closer than [`MIN_SAMPLE`] (e.g. the
/// forced refresh after a kill) carry forward rather than dividing by a near-zero
/// window. PIDs absent from a cycle are evicted via the generation tag.
struct CpuTracker {
    hist: PidMap<CpuHistory>,
    last: Option<Instant>,
    clk_tck: u64,
    gen_counter: u32,
}

impl CpuTracker {
    fn new(clk_tck: u64) -> Self {
        Self {
            hist: PidMap::default(),
            last: None,
            clk_tck,
            gen_counter: 0,
        }
    }

    fn update(&mut self, procs: &mut [ProcessEntry], now: Instant) {
        let window = self
            .last
            .map(|t| now.saturating_duration_since(t))
            .filter(|e| *e >= MIN_SAMPLE);

        let Some(elapsed) = window else {
            // First sample, or a refresh too soon to measure: carry the windowed
            // values forward; seed baselines only on the very first call.
            let first = self.last.is_none();
            if first {
                self.gen_counter = self.gen_counter.wrapping_add(1);
                self.last = Some(now);
            }
            for p in procs.iter_mut() {
                match self.hist.get_mut(&p.pid) {
                    Some(h) => {
                        p.cpu_pct = h.avg();
                        p.cpu_peak = h.peak();
                    }
                    None if first => {
                        self.hist
                            .insert(p.pid, CpuHistory::new(p.ticks, self.gen_counter));
                    }
                    None => {}
                }
            }
            return;
        };

        let jiff = u32::try_from(elapsed_jiffies(elapsed, self.clk_tck)).unwrap_or(u32::MAX);
        self.gen_counter = self.gen_counter.wrapping_add(1);
        let cur_gen = self.gen_counter;
        for p in procs.iter_mut() {
            let h = match self.hist.entry(p.pid) {
                Entry::Occupied(e) => {
                    let h = e.into_mut();
                    if p.ticks < h.prev_ticks {
                        h.reset(p.ticks); // PID reuse / counter wrap
                    } else {
                        let delta = u32::try_from(p.ticks - h.prev_ticks).unwrap_or(u32::MAX);
                        h.push(delta, jiff);
                        h.prev_ticks = p.ticks;
                    }
                    h
                }
                // New PID: baseline now; first real rate lands next interval.
                Entry::Vacant(e) => e.insert(CpuHistory::new(p.ticks, cur_gen)),
            };
            h.seen_gen = cur_gen;
            p.cpu_pct = h.avg();
            p.cpu_peak = h.peak();
        }
        self.hist.retain(|_, h| h.seen_gen == cur_gen); // drop vanished PIDs
        self.last = Some(now);
    }
}

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

/// Slow-changing per-PID metadata, stored on huge pages in a [`GenStore`] keyed by
/// [`PidIndex`]. `Flat` (`Copy`) — no heap fields — so it lives in a THP arena chunk. It is
/// **gatherer-internal**: no published snapshot references a `PidMeta` slot (the snapshot
/// carries the resolved `uid`/`cmdline` handle), so slots are freed immediately on death
/// (no lease). Only the cmdline *string* it points at is leased.
#[derive(Clone, Copy)]
struct PidMeta {
    /// Start time of the incarnation this metadata belongs to (PID-reuse discriminator).
    start_time: u64,
    uid: u32,
    /// Handle to this PID's cmdline in the `Cmd` store (empty for kthreads / no argv). The
    /// slot persists across cycles; replaced (old demoted, new alive) only when the cmdline
    /// bytes actually change — no per-cycle re-copy.
    cmd: StringRef<Cmd>,
    /// The cmdline contains a byte ≥ 0x80 (renderer unicode path).
    cmd_non_ascii: bool,
    /// Generation when first seen — drives the age-adaptive cmdline cadence.
    first_seen_gen: u64,
    /// Generation when last seen, for evicting vanished PIDs.
    seen_gen: u64,
}

impl PidMeta {
    fn new(start_time: u64, cur_gen: u64) -> Self {
        Self {
            start_time,
            uid: u32::MAX,
            cmd: StringRef::EMPTY,
            cmd_non_ascii: false,
            first_seen_gen: cur_gen,
            seen_gen: cur_gen,
        }
    }
}

/// PID → its [`PidMeta`] slot. A plain `FxHashMap` for now; phase 6 replaces it with a
/// THP-resident open-addressing `ThpMap` so even this lookup table is on huge pages.
type PidIndex = PidMap<Ref<PidMeta>>;

/// Owns the slow per-PID metadata (`uid`, `cmdline`) backend-agnostically. comm rides
/// inside stat (re-read every cycle, free); this layer covers what does not. Each cycle
/// it materializes the cmdline — fresh-read or cached — into the *current* arena so the
/// `StringRef` stays valid across the double-buffer reset.
struct ProcCache {
    /// PID → its [`PidMeta`] slot (gatherer-internal; phase 6 → THP `ThpMap`).
    pid_index: PidIndex,
    /// Per-PID metadata on huge pages (uid, cmdline handle, cadence bookkeeping). Freed
    /// immediately on death — no snapshot leases it.
    meta: GenStore<PidMeta>,
    /// Generational cmdline storage, shared across snapshots (persistent). A `StringRef<Cmd>`
    /// in a published snapshot stays valid until GC, which lags by [`GC_LAG`] generations.
    cmd_store: CmdStore,
    refresh_n: u32,
    cmd_path: ProcPath,
    /// Reused read buffer for `/proc/<pid>/cmdline` (no per-cycle allocation).
    scratch: Vec<u8>,
}

impl ProcCache {
    fn new(arena: &mut Arena, refresh_n: u32) -> Self {
        Self {
            pid_index: PidIndex::default(),
            meta: GenStore::new(arena, PIDMETA_MIN_SLOTS),
            cmd_store: CmdStore::new(arena, CMD_STORE_MIN_SLOTS),
            refresh_n,
            cmd_path: ProcPath::new(),
            scratch: vec![0u8; CMD_SLOT],
        }
    }

    /// Read-only `Cmd`-store view to publish in the snapshot for UI-side resolution.
    fn resolver(&self, arena: &Arena) -> ByteResolver<Cmd> {
        self.cmd_store.resolver(arena)
    }

    /// Reclaim cmdline slots whose generation the live snapshot window has passed.
    /// Cross-thread soundness: a published snapshot only references `ALIVE` or
    /// recently-demoted slots; `min_live` (= current gen − [`GC_LAG`]) never reaches those,
    /// so the UI thread's `resolve` reads of leased slots never race a reclaim. Demotion
    /// flips only a slot's 1-byte tag (a distinct memory location from its data bytes), and
    /// new interns target free/new slots no live snapshot references. (Arena region
    /// retirement from a store relocate is reclaimed on the same lease — see
    /// [`Gatherer::gather`].)
    fn gc(&mut self, arena: &Arena, min_live: u64) {
        self.cmd_store.gc(arena, min_live);
    }

    /// Fill `uid` + `cmdline` on every live entry (stat fields are already set). Reads
    /// fresh on first sighting, while settling, or on the staggered coarse tick; otherwise
    /// reuses the cached handle (no I/O, no copy). A fresh read replaces the `Cmd` slot
    /// **only when the bytes changed** — an unchanged cmdline keeps its slot, so the
    /// per-cycle re-materialization of every PID's cmdline is gone. `gen` is the generation
    /// of the snapshot being built; demotions/allocations key the generational lease to it.
    #[allow(clippy::cast_possible_truncation)] // cmdline len bounded by CMD_SLOT
    fn update(&mut self, arena: &mut Arena, procs: &mut [ProcessEntry], cur_gen: u64) {
        let Self {
            pid_index,
            meta,
            cmd_store,
            refresh_n,
            cmd_path,
            scratch,
        } = self;
        let refresh_n = *refresh_n;

        for e in procs.iter_mut() {
            let pid = e.pid;
            // Copy the prior record out (`PidMeta` is `Copy`) so no `meta`/`arena` borrow is
            // held across the `cmd_store` ops below, which also need the arena.
            let prior = pid_index
                .get(&pid)
                .map(|&mref| (mref, *meta.get(arena, mref)));
            let (present, reused, settling) = match prior {
                Some((_, pm)) => {
                    let reused = pm.start_time != e.start_time;
                    let settling = !reused
                        && cur_gen.wrapping_sub(pm.first_seen_gen) < u64::from(CMDLINE_SETTLE_GENS);
                    (true, reused, settling)
                }
                None => (false, false, true), // new → treat as settling
            };
            let stagger =
                refresh_n <= 1 || cur_gen.wrapping_add(u64::from(pid)) % u64::from(refresh_n) == 0;
            let refresh = !present || reused || settling || stagger;

            // Read fresh into scratch (kthreads cost zero syscalls: uid 0, cmdline empty).
            let fresh = if refresh {
                if e.is_kthread {
                    Some((0u32, 0usize, false))
                } else {
                    Some(read_cmdline_uid(pid, cmd_path, scratch))
                }
            } else {
                None
            };

            // Build the record locally. On reuse, demote the prior incarnation's cmd slot and
            // re-baseline; on a kept PID, carry its record forward.
            let mut pm = match prior {
                Some((_, pm)) if !reused => pm,
                _ => PidMeta::new(e.start_time, cur_gen),
            };
            if reused && let Some((_, old)) = prior {
                cmd_store.demote(arena, old.cmd, cur_gen);
            }
            pm.seen_gen = cur_gen;

            if let Some((uid, len, non_ascii)) = fresh {
                pm.uid = uid;
                let new_bytes = &scratch[..len];
                if new_bytes.is_empty() {
                    cmd_store.demote(arena, pm.cmd, cur_gen); // no-op if already empty
                    pm.cmd = StringRef::EMPTY;
                    pm.cmd_non_ascii = false;
                } else if pm.cmd.is_empty() || cmd_store.get(arena, pm.cmd) != new_bytes {
                    // Changed (or first non-empty argv): new slot alive, old slot demoted.
                    cmd_store.demote(arena, pm.cmd, cur_gen);
                    pm.cmd = cmd_store.intern(arena, Gen::ALIVE, new_bytes);
                    pm.cmd_non_ascii = non_ascii;
                }
                // else: unchanged — keep the existing slot (the whole point of the store).
            }

            // Write back: overwrite the existing slot, or allocate one for a new PID + index.
            if let Some((mref, _)) = prior {
                meta.assign(arena, mref, pm);
            } else {
                let mref = meta.insert(arena, Gen::ALIVE, pm);
                pid_index.insert(pid, mref);
            }

            e.uid = pm.uid;
            e.cmdline = pm.cmd;
            if !pm.cmd.is_empty() {
                e.non_ascii |= pm.cmd_non_ascii;
            }
        }

        // Evict vanished PIDs: demote their cmd slot (leased) + free their PidMeta slot
        // (gatherer-internal, immediate).
        pid_index.retain(|_, &mut mref| {
            let pm = *meta.get(arena, mref);
            if pm.seen_gen == cur_gen {
                true
            } else {
                cmd_store.demote(arena, pm.cmd, cur_gen);
                meta.free(arena, mref);
                false
            }
        });
    }
}

/// Read `/proc/<pid>/cmdline` into `scratch` and the owner `uid` from the same fd's
/// `fstat`. Returns `(uid, cleaned_len, non_ascii)`; `uid` is still valid when the
/// cmdline is empty. The cleaned bytes live in `scratch[..len]`.
fn read_cmdline_uid(pid: u32, path: &mut ProcPath, scratch: &mut [u8]) -> (u32, usize, bool) {
    let ptr = path.write(pid, b"cmdline");
    // SAFETY: valid C path, read-only.
    let fd = unsafe { libc::open(ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return (u32::MAX, 0, false);
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
        return (uid, 0, false);
    }
    let raw = usize::try_from(n).unwrap_or(0).min(scratch.len());
    let (clean_len, non_ascii) = parse::clean_cmdline(&mut scratch[..raw]);
    (uid, clean_len as usize, non_ascii)
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
    /// Re-read every live PID's stat into `snap`. Returns the overflow count (PIDs that
    /// exceeded the persistent-fd pool and used the transient fallback).
    fn collect(
        &mut self,
        pids: &[u32],
        snap: &mut Snapshot,
        page_size: u64,
    ) -> std::io::Result<u32> {
        match self {
            Backend::Uring(u) => u.collect(pids, snap, page_size),
            Backend::Syscall(s) => Ok(s.collect(pids, snap, page_size)),
        }
    }
}

pub struct Gatherer {
    arc_swap: Arc<ArcSwap<Snapshot>>,
    recycled: Option<Arc<Snapshot>>,
    proc_dir: ProcDir,
    pids: Vec<u32>,
    dent_buf: Vec<u8>,
    cpu: CpuTracker,
    cache: ProcCache,
    sys_cpu: SysCpuAccum,
    tree_stack: Vec<u32>,
    tree_order: Vec<u32>,
    /// Shared THP sub-allocator backing the gatherer's generational stores (currently the
    /// `Cmd` store; phases 3–6 add per-PID/CPU/index stores). One per gatherer; growth is
    /// lease-deferred so cross-thread readers stay valid.
    arena: Arena,
    /// Persistent-fd pool capacity, retained for the mid-run `io_uring`→syscall downgrade.
    pool_cap: u32,
    config: Config,
    /// `pid_max` (the PID-counter wrap point), read once at startup — bounds the probe
    /// window so it never generates an impossible PID.
    pid_max: u32,
    page_size: u64,
    generation: u64,
}

impl Gatherer {
    /// Build the gatherer and the shared snapshot cell. Returns the `ArcSwap` for the UI to
    /// load from. The I/O backend is **not** built here — [`run`](Self::run) builds it on the
    /// gatherer thread, so the `io_uring` ring is owned by its sole submitter. Runs on the
    /// main thread; does no `/proc` I/O.
    pub fn new(page_size: u64) -> std::io::Result<(Self, Arc<ArcSwap<Snapshot>>)> {
        let front = Arc::new(Snapshot::new(INIT_BUF));
        let back = Arc::new(Snapshot::new(INIT_BUF));

        let pool_cap = pool_capacity();
        let arc_swap = Arc::new(ArcSwap::from(front));
        let mut arena = Arena::new(0);
        let cache = ProcCache::new(&mut arena, cmdline_refresh_n());
        let gatherer = Self {
            arc_swap: arc_swap.clone(),
            recycled: Some(back),
            proc_dir: ProcDir::open()?,
            pids: Vec::new(),
            dent_buf: vec![0u8; 64 * 1024],
            cpu: CpuTracker::new(clk_tck()),
            cache,
            sys_cpu: SysCpuAccum::new(),
            tree_stack: Vec::new(),
            tree_order: Vec::new(),
            arena,
            pool_cap,
            config: Config::from_env(),
            pid_max: sys::read_pid_max(),
            page_size,
            generation: 0,
        };
        Ok((gatherer, arc_swap))
    }

    /// Gatherer thread entry point and **sole ring submitter**. Builds the I/O backend on
    /// this thread (binding the ring's single-issuer submitter task here), produces the
    /// priming snapshot, signals `ready`, then sleeps on the control channel (zero idle CPU),
    /// gathering on timeout or `Refresh` and exiting on `Quit`/hangup. The backend lives in
    /// this frame and is passed to each `gather` — so it is non-optional and never escapes
    /// the submitter thread. `ready` lets the main thread rendezvous on the first snapshot
    /// (or observe this thread's early exit, when the sender drops).
    pub fn run(mut self, ctrl: &Receiver<Ctrl>, interval: Duration, ready: &Sender<()>) {
        let mut backend = self.build_backend();
        self.gather(&mut backend); // prime
        let _ = ready.send(()); // first snapshot published — release the main thread
        // Exits on Quit or a hung-up channel; gathers on Refresh or interval timeout.
        while let Ok(Ctrl::Refresh) | Err(RecvTimeoutError::Timeout) = ctrl.recv_timeout(interval) {
            self.gather(&mut backend);
        }
    }

    /// Build the I/O backend on the **calling thread** — which must be the gatherer thread,
    /// because the `io_uring` backend's single-issuer flags bind the ring's submitter task to
    /// its creator. Probes `io_uring` (`ATOP_FORCE_SYSCALL` or any probe failure falls back to
    /// the syscall backend).
    fn build_backend(&self) -> Backend {
        if force_syscall() {
            return Backend::Syscall(SyscallBackend::new(self.pool_cap));
        }
        // The backend owns its own fixed landing pad; it no longer registers the
        // snapshot arenas, so probing needs nothing from the snapshots.
        match UringBackend::probe(
            self.pool_cap,
            RING_ENTRIES,
            self.config.batch_cap,
            self.config.read_slots,
        ) {
            Some(u) => Backend::Uring(Box::new(u)),
            None => Backend::Syscall(SyscallBackend::new(self.pool_cap)),
        }
    }

    /// Skip-cycle birth probe (§3a): append up to `probe_width` candidate PIDs just above
    /// the highest live PID, bounded by the kernel's allocation frontier (`ns_last_pid`).
    /// PIDs are allocated near-monotonically, so a freshly-forked process almost always
    /// takes a number above the current max; `collect` opens these speculatively — survivors
    /// join the maintained set, misses fail their open and are re-tombstoned (§3b, the
    /// hygiene that makes speculative opens safe). In steady state `ns_last_pid` equals our
    /// max, so the window is empty and the probe costs **zero** opens. A burst > W, a
    /// non-sequential birth, or a post-wrap low PID (the live max cannot follow the counter
    /// below itself) waits for the next full scan — a probe hit-rate, never a correctness,
    /// concern, because the full scan is the backstop.
    ///
    /// **Precondition**: `self.pids` is sorted ascending and holds the current live set —
    /// the probe reads the max (`last`) as its anchor and appends strictly-larger candidates
    /// to stay sorted. The survivor rebuild after `compact` upholds this. `ns_last_pid` is
    /// read from the gatherer's own PID namespace (the only one whose numbering it tracks).
    fn probe_births(&mut self) {
        debug_assert!(
            self.pids.windows(2).all(|w| w[0] <= w[1]),
            "probe_births requires a sorted live set"
        );
        let w = self.config.probe_width;
        let Some(&anchor) = self.pids.last() else {
            return; // no live set yet — the next full scan seeds it
        };
        let frontier = sys::read_ns_last_pid().unwrap_or(anchor.saturating_add(w));
        let hi = anchor.saturating_add(w).min(frontier).min(self.pid_max);
        for cand in anchor.saturating_add(1)..=hi {
            self.pids.push(cand); // > current max ⇒ appends keep `pids` sorted
        }
    }

    fn gather(&mut self, backend: &mut Backend) {
        // Enumeration cadence (§3): a full `getdents` re-scan every K cycles is the resync
        // that catches any birth the probe missed (non-sequential, burst > W, post-wrap);
        // skip cycles reuse the maintained live set plus a cheap sequential-birth probe.
        // `self.pids` is rebuilt from survivors after `compact` (below), so deaths drop the
        // same cycle (held read → `ESRCH` → re-tombstone → compact) — the full scan is only
        // for births, never for pruning.
        if self.generation.is_multiple_of(self.config.enum_every) {
            self.proc_dir.read_pids(&mut self.dent_buf, &mut self.pids);
        } else {
            self.probe_births();
        }
        self.pids.sort_unstable();
        self.pids.dedup();
        let now = Instant::now();

        // Reclaim the recycled buffer; skip this cycle if the UI still holds it
        // (vanishingly rare — see take_back).
        let Some(mut arc) = self.take_back() else {
            return;
        };
        let snap = Arc::get_mut(&mut arc).expect("recycled buffer is unique");

        // Arena holds only the copied-out comm + re-materialized cmdline per PID now
        // (raw stat lands in the backend's fixed read pad). A reserve hint — the arena
        // grows freely if exceeded, since it is no longer an io_uring target.
        let needed = self.pids.len() * ARENA_BYTES_PER_PID;
        Self::prepare(snap, &self.pids, needed);

        let overflow = if let Ok(overflow) = backend.collect(&self.pids, snap, self.page_size) {
            overflow
        } else {
            // io_uring failed mid-cycle: drop to syscall permanently and redo.
            *backend = Backend::Syscall(SyscallBackend::new(self.pool_cap));
            Self::prepare(snap, &self.pids, needed);
            backend
                .collect(&self.pids, snap, self.page_size)
                .unwrap_or(0)
        };
        snap.pool_overflow = overflow;

        snap.compact();

        // Rebuild the maintained live set from survivors (PID-sorted, since `compact`
        // preserves order): deaths and probe misses fall out now, confirmed births stay.
        // The next skip cycle reuses this set; the next full scan replaces it wholesale.
        self.pids.clear();
        self.pids.extend(snap.procs.iter().map(|p| p.pid));

        // The generation of the snapshot we are building. `self.generation` only advances
        // on a successful publish, so this is one past the last published generation; a
        // skipped cycle (take_back failed above) never reaches here, keeping the generation
        // clock in lock-step with published snapshots — the GC lease math depends on it.
        let building_gen = self.generation + 1;
        self.arena.set_gen(building_gen);

        // Fill uid + cmdline handle (slow fields) on the surviving entries, then publish
        // the Cmd-store view for UI resolution, then CPU%.
        self.cache
            .update(&mut self.arena, &mut snap.procs, building_gen);
        snap.cmd = self.cache.resolver(&self.arena);
        self.cpu.update(&mut snap.procs, now);
        snap.first_root = tree::build(&mut snap.procs, &mut self.tree_stack, &mut self.tree_order);
        tree::aggregate(&mut snap.procs, &self.tree_order);

        // System-wide stats (tiny reads, ~3 μs total).
        self.sys_cpu.update(&mut snap.sys);
        snap.count_tasks();

        self.generation = building_gen;
        snap.generation = building_gen;

        let prev = self.arc_swap.swap(arc);
        self.recycled = Some(prev);

        // Reclaim cmdline slots no live snapshot can reference. At end of generation N the
        // live snapshots are N (just published) and possibly N−1 (UI not yet advanced);
        // anything demoted at gen ≤ N−1 is referenced only by snapshots ≤ N−2, all dead
        // (building N required the UI to have dropped N−2). `GC_LAG` (2) reclaims ≤ N−2 —
        // one generation more conservative than strictly required. The arena GC frees any
        // region a store relocate retired, on the same lease.
        let min_live = self.generation.saturating_sub(GC_LAG);
        self.cache.gc(&self.arena, min_live);
        self.arena.gc(min_live);
    }

    /// Reset and pre-size the snapshot, seeding tombstones for every PID in sorted
    /// order. The arena is no longer an `io_uring` target, so `reserve` is a pure
    /// performance hint (avoid mid-cycle re-mmap+copy) — a grow no longer races
    /// in-flight reads or staleness any registered buffer.
    fn prepare(snap: &mut Snapshot, pids: &[u32], needed: usize) {
        snap.reset();
        for &pid in pids {
            snap.push_tombstone(pid);
        }
        snap.strings.reserve(needed);
    }

    /// Reclaim the back buffer with unique access. Spins briefly if the UI still
    /// references it; gives up (caller skips the cycle) rather than ever allocating
    /// a third, unregistered buffer.
    fn take_back(&mut self) -> Option<Arc<Snapshot>> {
        let mut arc = self.recycled.take()?;
        for _ in 0..200 {
            if Arc::get_mut(&mut arc).is_some() {
                return Some(arc);
            }
            std::thread::sleep(Duration::from_micros(50));
        }
        self.recycled = Some(arc);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut h = CpuHistory::new(0, 1);
        h.push(25, 50); // 50%
        h.push(50, 50); // 100% → (25+50)/(100) = 75%
        assert_eq!(h.avg(), 7500);
    }

    #[test]
    fn moving_average_evicts_old_samples() {
        let mut h = CpuHistory::new(0, 1);
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
        let mut h = CpuHistory::new(0, 1);
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

    fn name_of(snap: &Snapshot, p: &ProcessEntry) -> String {
        String::from_utf8_lossy(snap.strings.get(p.name)).into_owned()
    }

    /// The `io_uring` backend must agree with the syscall oracle on stable **stat**
    /// fields for processes both scans observed. (uid/cmdline are no longer backend
    /// fields — `ProcCache` owns them; CPU/ticks/mem can drift between scans; PID reuse
    /// can churn the set — so we anchor on PID 1 and self, and require broad agreement
    /// on the overlap.)
    #[test]
    fn uring_matches_syscall_backend() {
        let page_size = crate::sys::page_size();
        let pids = enum_pids();
        let pool_cap = pool_capacity();

        let mut a = Snapshot::new(INIT_BUF);
        let mut b = Snapshot::new(INIT_BUF);

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

        assert!(b.procs.len() > 10, "uring found too few: {}", b.procs.len());

        let by_pid: HashMap<u32, &ProcessEntry> = b.procs.iter().map(|p| (p.pid, p)).collect();

        let mut common = 0;
        let mut name_matches = 0;
        for pa in &a.procs {
            if let Some(pb) = by_pid.get(&pa.pid) {
                common += 1;
                if name_of(&a, pa) == name_of(&b, pb) {
                    name_matches += 1;
                }
                if pa.pid == 1 || pa.pid == std::process::id() {
                    assert_eq!(pa.ppid, pb.ppid, "ppid mismatch for pid {}", pa.pid);
                    assert_eq!(
                        pa.is_kthread, pb.is_kthread,
                        "is_kthread mismatch for pid {}",
                        pa.pid
                    );
                    assert_eq!(
                        name_of(&a, pa),
                        name_of(&b, pb),
                        "name mismatch pid {}",
                        pa.pid
                    );
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
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut x = 0u64;
            while !worker_stop.load(Ordering::Relaxed) {
                x = std::hint::black_box(x.wrapping_add(1));
            }
        });

        let (mut g, cell) = Gatherer::new(crate::sys::page_size()).expect("gatherer");
        let mut backend = g.build_backend();
        let me = std::process::id();
        let (mut max_cpu, mut max_peak) = (0, 0);
        for _ in 0..4 {
            g.gather(&mut backend);
            let snap = cell.load_full();
            if let Some(p) = snap.procs.iter().find(|p| p.pid == me) {
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

    /// Full gatherer pipeline (enumerate → backend → cpu → tree → publish).
    #[test]
    fn gatherer_publishes_valid_snapshot() {
        let (mut g, cell) = Gatherer::new(crate::sys::page_size()).expect("gatherer");
        let mut backend = g.build_backend();
        g.gather(&mut backend);
        let snap = cell.load_full();

        assert!(snap.generation >= 1);
        assert!(snap.procs.len() > 10, "got {}", snap.procs.len());
        assert!(snap.procs.iter().any(|p| p.pid == 1), "pid 1 missing");
        assert_ne!(snap.first_root, crate::snapshot::NONE, "no roots");

        // PIDs sorted ascending (tree binary-search precondition).
        assert!(snap.procs.windows(2).all(|w| w[0].pid < w[1].pid));

        // Every non-root parent index is in range and resolves to the right PID.
        for p in &snap.procs {
            if p.parent_idx != crate::snapshot::NONE {
                let parent = &snap.procs[p.parent_idx as usize];
                assert_eq!(parent.pid, p.ppid, "parent_idx points at wrong pid");
            }
        }

        assert_eq!(snap.pool_overflow, 0, "default pool should not overflow");

        // ProcCache filled the slow fields: pid 1 is root-owned; this test process has
        // a non-empty cmdline; kernel threads are flagged and have no cmdline.
        let init = snap.procs.iter().find(|p| p.pid == 1).expect("pid 1");
        assert_eq!(init.uid, 0, "pid 1 is owned by root");

        let me = snap
            .procs
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("self");
        assert!(!me.is_kthread, "the test process is not a kernel thread");
        assert!(!me.cmdline.is_empty(), "self should have a cmdline");

        for p in &snap.procs {
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
        let (mut g, cell) = Gatherer::new(crate::sys::page_size()).expect("gatherer");
        let mut backend = g.build_backend();
        g.gather(&mut backend);
        g.gather(&mut backend); // warm up the persistent-fd pool

        let start = Instant::now();
        for _ in 0..ITERS {
            g.gather(&mut backend);
        }
        let elapsed = start.elapsed();
        let snap = cell.load_full();
        let per_cycle_us = elapsed.as_micros() as f64 / f64::from(ITERS);
        eprintln!(
            "pids={} kthreads={} overflow={} per_cycle={per_cycle_us:.1}µs",
            snap.procs.len(),
            snap.procs.iter().filter(|p| p.is_kthread).count(),
            snap.pool_overflow,
        );
        assert_eq!(snap.pool_overflow, 0, "default pool should not overflow");
    }

    /// cmdline survives the coarse cadence: a PID not refreshed this cycle keeps its
    /// persistent `Cmd`-store slot, still resolvable through the new snapshot's view.
    #[test]
    fn cmdline_persists_across_coarse_cycles() {
        let (mut g, cell) = Gatherer::new(crate::sys::page_size()).expect("gatherer");
        let mut backend = g.build_backend();
        let me = std::process::id();
        g.gather(&mut backend);
        let snap1 = cell.load_full();
        let cmd1 = {
            let p = snap1.procs.iter().find(|p| p.pid == me).expect("self c1");
            String::from_utf8_lossy(snap1.cmdline(p)).into_owned()
        };
        assert!(!cmd1.is_empty(), "self cmdline should be present");

        // A second cycle (no time for argv to change) must still resolve the cmdline from
        // the persistent slot — no re-copy, but the new snapshot's resolver sees it.
        g.gather(&mut backend);
        let snap2 = cell.load_full();
        let cmd2 = {
            let p = snap2.procs.iter().find(|p| p.pid == me).expect("self c2");
            String::from_utf8_lossy(snap2.cmdline(p)).into_owned()
        };
        assert_eq!(cmd1, cmd2, "cmdline must persist across cycles");
    }

    /// Generational lease across the double buffer: a snapshot held by the "UI" must keep
    /// resolving a PID's cmdline even after that PID dies and a later gather evicts it —
    /// eviction *demotes* the slot, and GC (lagging by [`GC_LAG`]) must not reclaim it while
    /// the older snapshot still references it. This is the safety property the shared,
    /// persistent `Cmd` store rests on (the per-snapshot arena reset could never violate it,
    /// but a shared store can if the lease is wrong).
    #[test]
    fn held_snapshot_resolves_dead_pid_cmdline() {
        let (mut g, cell) = Gatherer::new(crate::sys::page_size()).expect("gatherer");
        let mut backend = g.build_backend();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let cpid = child.id();

        g.gather(&mut backend); // gen 1: child alive, cmdline read fresh
        let held = cell.load_full(); // UI leases gen 1 (and its Cmd-store slots)
        let cmd_before = {
            let p = held
                .procs
                .iter()
                .find(|p| p.pid == cpid)
                .expect("child in gen 1");
            String::from_utf8_lossy(held.cmdline(p)).into_owned()
        };
        assert!(
            cmd_before.contains("sleep"),
            "child cmdline was {cmd_before:?}"
        );

        child.kill().unwrap();
        child.wait().unwrap();

        // gen 2: child gone → evicted from the cache, its cmd slot demoted at gen 2. GC runs
        // at min_live = gen2 − GC_LAG, which cannot reach gen 2 → the slot is kept.
        g.gather(&mut backend);

        let p = held
            .procs
            .iter()
            .find(|p| p.pid == cpid)
            .expect("child still present in the held gen-1 snapshot");
        assert_eq!(
            String::from_utf8_lossy(held.cmdline(p)),
            cmd_before,
            "held snapshot must keep resolving the dead PID's cmdline (generational lease)"
        );
    }

    /// The win: an unchanged cmdline is **not** re-interned each cycle, so the `Cmd` store's
    /// high-water slot count stays proportional to live processes, not to cycle count. If
    /// the old per-cycle re-materialization were still happening (or unchanged strings were
    /// re-interned), the store would grow by ~one slot per userspace PID per cycle.
    #[test]
    fn cmd_store_does_not_grow_per_cycle() {
        let (mut g, cell) = Gatherer::new(crate::sys::page_size()).expect("gatherer");
        let mut backend = g.build_backend();
        let me = std::process::id();

        // Warm up: settle cmdlines and let births/deaths reach steady state. Drop each
        // snapshot so the gatherer recycles buffers freely (no lease stall).
        for _ in 0..6 {
            g.gather(&mut backend);
            drop(cell.load_full());
        }
        let slots_warm = g.cache.cmd_store.slot_count();
        let userspace = {
            let s = cell.load_full();
            s.procs.iter().filter(|p| !p.is_kthread).count()
        };

        for _ in 0..30 {
            g.gather(&mut backend);
            drop(cell.load_full());
        }
        let slots_after = g.cache.cmd_store.slot_count();

        // Slack covers genuine churn (new userspace PIDs over ~18 s); the disaster mode
        // would be `userspace × 30` extra slots.
        let growth = slots_after - slots_warm;
        assert!(
            growth <= userspace.max(64),
            "Cmd store grew by {growth} slots over 30 cycles (userspace pids ≈ {userspace}); \
             unchanged cmdlines are being re-interned"
        );

        // And the latest snapshot still resolves self correctly.
        let s = cell.load_full();
        let p = s.procs.iter().find(|p| p.pid == me).expect("self");
        assert!(!s.cmdline(p).is_empty(), "self cmdline must resolve");
    }

    /// §3a: a sequentially-allocated new process is caught by the skip-cycle birth probe
    /// within **1** cycle — not only at the next full `getdents` scan. Full scans are
    /// disabled (`enum_every` huge) so the birth can *only* be found by the probe.
    #[test]
    fn birth_probe_catches_sequential_birth_within_one_cycle() {
        let (mut g, cell) = Gatherer::new(crate::sys::page_size()).expect("gatherer");
        g.config.enum_every = 1_000_000; // effectively no full re-scan → births only via probe
        g.config.probe_width = 1_000_000; // window spans (max_live, ns_last_pid] regardless of W
        let mut backend = g.build_backend();
        g.gather(&mut backend); // generation 0 → full scan seeds the maintained live set
        assert!(cell.load().procs.iter().any(|p| p.pid == 1), "set seeded");

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();

        g.gather(&mut backend); // generation 1 → SKIP cycle: probe must discover the new child
        let caught = cell.load().procs.iter().any(|p| p.pid == child_pid);

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            caught,
            "sequential birth (pid {child_pid}) must be caught by the probe within 1 cycle"
        );
    }

    /// §3b + cadence: a process that dies vanishes the **same** (skip) cycle via the pool
    /// `ESRCH` → re-tombstone → compact → survivor-rebuild path — it never lingers until
    /// the next full scan, and no phantom row is left behind.
    #[test]
    fn death_caught_same_cycle_on_skip() {
        let (mut g, cell) = Gatherer::new(crate::sys::page_size()).expect("gatherer");
        g.config.enum_every = 1_000_000; // force skip cycles after the first
        let mut backend = g.build_backend();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();

        g.gather(&mut backend); // generation 0 → full scan sees the (still-live) child
        assert!(
            cell.load().procs.iter().any(|p| p.pid == child_pid),
            "child must be observed while alive"
        );

        child.kill().unwrap();
        child.wait().unwrap();

        g.gather(&mut backend); // generation 1 → SKIP cycle: held read ESRCH ⇒ death caught now
        let snap = cell.load();
        assert!(
            !snap.procs.iter().any(|p| p.pid == child_pid),
            "death must be caught the same skip cycle, not deferred to the next full scan"
        );
        assert!(
            snap.procs.iter().all(|p| p.state != b'?'),
            "no phantom row may survive the death"
        );
    }
}
