//! The unified per-PID table maintained across cycles: one PID index coordinating the hot
//! CPU-history store, the cold uid/cmdline metadata store, and the `Cmd` string store.
//!
//! One PID lookup serves every store. The index value ([`PidSlot`]) holds the store handles
//! plus cross-store bookkeeping (incarnation, settling/eviction generations) so the fill — and
//! a future volatility-based update prioritizer — reads per-PID state without chasing into a
//! store. Hot ([`CpuRing`]) and cold ([`PidMeta`]) sit in *separate* huge-page stores so a CPU
//! update never loads metadata cache lines.

use std::time::Instant;

use thoop::{Arena, Gen, GenStore, Ref, StrStore, StringRef, ThpMap};

use super::config::{CMD_SLOT, env_u32};
use super::cpu::{ACTIVE_SAMPLES, CpuRing, MIN_SAMPLE, elapsed_jiffies};
use super::parse;
use crate::identity::ProcMeta;
use crate::procs::{CapLevel, Cmd, ProcessEntry, Procs};
use crate::sys::{self, ProcPath};

/// A freshly-seen PID reads cmdline/uid every cycle for this many cycles (exec/argv
/// still settling — `nginx`/`postgres` rewrite their argv after exec) before dropping
/// to the coarse staggered interval.
const CMDLINE_SETTLE_GENS: u32 = 3;
/// Default coarse cmdline/uid refresh interval: PID `p` refreshes when
/// `(cur_gen + p) % N == 0`, so ~1/N settled PIDs refresh each cycle (no thundering herd).
/// At `REFRESH_MS=500` and `N=16`, worst-case staleness ≈ 8 s. See [`cmdline_refresh_n`].
const CMDLINE_REFRESH_N: u32 = 16;

/// A process must be at least this many cycles old before its first deleted-library maps
/// scan — matches the settling window used elsewhere (~10 s at `REFRESH_MS=500`), so a
/// freshly-exec'd process isn't scanned while its mappings are still churning.
const LIB_SETTLE_GENS: u64 = 20;
/// Minimum cycles between deleted-library re-scans of the same process (~60 s at 500 ms).
/// The maps parse is the heavy check, so it is re-resolved rarely — a `dlclose`/`dlopen`
/// swap is not urgent to surface.
const LIB_RECHECK_GENS: u64 = 120;
/// Cap on deleted-library maps scans started per cycle, bounding the I/O cost regardless of
/// PID count. Due processes are scanned in PID order until the budget runs out; because a
/// scanned process then isn't due again for `LIB_RECHECK_GENS`, the whole population is
/// covered every `live_procs / LIB_CHECKS_PER_CYCLE` cycles without a separate cursor.
const LIB_CHECKS_PER_CYCLE: u32 = 12;

/// The kernel appends this to a `/proc/<pid>/exe` symlink target, or a `/proc/<pid>/maps`
/// mapping path, once the backing on-disk file is unlinked or replaced.
const DELETED_SUFFIX: &[u8] = b" (deleted)";
/// Upper bound on bytes read from a `/proc/<pid>/maps` file — a defensive cap against a
/// pathologically large address space; a truncated tail can only cause a bounded false
/// negative, self-healed on the next re-scan.
const MAPS_MAX: usize = 1 << 20;

/// The generational `Cmd` string store (cmdlines), keyed off the gatherer's `u64`
/// generation. Slots persist across cycles; an unchanged cmdline keeps its slot.
type CmdStore = StrStore<CMD_SLOT, Cmd>;
const CGROUP_SLOT: usize = 1024;
const FLATPAK_SLOT: usize = 1024;
type CgroupStore = StrStore<CGROUP_SLOT, CgroupBytes>;
type FlatpakStore = StrStore<FLATPAK_SLOT, FlatpakBytes>;

struct CgroupBytes;
struct FlatpakBytes;

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

/// Coarse cmdline/uid refresh interval, overridable via `OYA_CMDLINE_REFRESH_N`
/// (1 = refresh every PID every cycle).
pub(crate) fn cmdline_refresh_n() -> u32 {
    env_u32("OYA_CMDLINE_REFRESH_N")
        .unwrap_or(CMDLINE_REFRESH_N)
        .max(1)
}

impl ProcMeta for ProcTable {
    fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
        self.cmdline(e)
    }

    fn cgroup(&self, e: &ProcessEntry) -> &[u8] {
        let Some(slot) = self.index.get(e.pid) else {
            return &[];
        };
        self.cgroup_store.get(self.meta.get(slot.meta).cgroup)
    }

    fn flatpak_info(&self, e: &ProcessEntry) -> &[u8] {
        let Some(slot) = self.index.get(e.pid) else {
            return &[];
        };
        self.flatpak_store.get(self.meta.get(slot.meta).flatpak)
    }
}

/// Slow-changing per-PID metadata (uid + cmdline handle) on huge pages in a
/// `GenStore<PidMeta>`. `Flat` (`Copy`) — no heap. The row carries the resolved `uid` and a
/// cmdline handle, so a dead PID's `PidMeta` slot is freed immediately (no reader leases it).
/// The **cold** counterpart to [`CpuRing`] — a separate store so a CPU sample never loads it.
/// Incarnation/interval bookkeeping lives in [`PidSlot`], not here.
#[derive(Clone, Copy)]
struct PidMeta {
    uid: u32,
    /// Handle to this PID's cmdline in the `Cmd` store (empty for kthreads / no argv). The
    /// slot persists across cycles; replaced (old freed, new alive) only when the cmdline
    /// bytes actually change — no per-cycle re-copy.
    cmd: StringRef<Cmd>,
    /// The cmdline contains a byte ≥ 0x80 (renderer unicode path).
    cmd_non_ascii: bool,
    /// Effective-capability privilege level (from `/proc/<pid>/status`), refreshed on the
    /// coarse interval like uid/cmdline.
    caps: CapLevel,
    /// Raw `/proc/<pid>/cgroup` content, refreshed on the cold metadata interval.
    cgroup: StringRef<CgroupBytes>,
    /// Raw `/proc/<pid>/root/.flatpak-info` content for likely Flatpak/sandbox processes.
    flatpak: StringRef<FlatpakBytes>,
    /// `/proc/<pid>/exe` was marked `" (deleted)"` — the running binary is gone from disk.
    /// Absorbing per incarnation: once observed, latched here for the process's lifetime.
    exe_deleted: bool,
    /// An executable mapping in `/proc/<pid>/maps` points at a deleted file (a replaced .so).
    /// Transient — re-resolved on the coarse [`LIB_RECHECK_GENS`] interval.
    uses_deleted_lib: bool,
}

impl PidMeta {
    const EMPTY: PidMeta = PidMeta {
        uid: u32::MAX,
        cmd: StringRef::EMPTY,
        cmd_non_ascii: false,
        caps: CapLevel::None,
        cgroup: StringRef::EMPTY,
        flatpak: StringRef::EMPTY,
        exe_deleted: false,
        uses_deleted_lib: false,
    };
}

/// The per-PID index value — what one PID lookup yields, coordinating every per-PID store. It
/// holds the store handles plus the cross-store bookkeeping the fill (and a future
/// update-prioritization scan) reads on every PID *without* chasing into a store. This is the
/// intended **extension point**: process-volatility signals for adaptive sampling land here.
/// `Flat` (`Copy`); lives in the THP-resident [`PidIndex`].
#[derive(Clone, Copy)]
pub(crate) struct PidSlot {
    /// Hot CPU-history slot.
    cpu: Ref<CpuRing>,
    /// Cold metadata slot (uid + cmdline handle).
    meta: Ref<PidMeta>,
    /// Incarnation discriminator: `start_time` from stat. A change ⇒ PID reuse.
    start_time: u64,
    /// Generation first seen — drives the age-adaptive cmdline interval (settling window).
    first_seen_gen: u64,
    /// Generation last seen — evicts vanished PIDs.
    seen_gen: u64,
    /// Generation of the last deleted-library maps scan (0 = never). Rate-limits the heavy
    /// scan to once per [`LIB_RECHECK_GENS`]; reset on PID reuse with the rest of the slot.
    lib_checked_gen: u64,
}

/// PID → [`PidSlot`], the per-cycle randomly-probed lookup that coordinates every per-PID
/// store. Resident in the shared arena (a `thoop::ThpMap`) so even this table is on huge pages
/// — the same TLB win as the stores it indexes. The map's fixed `u32` key is the PID; PID 0 is
/// the map's empty sentinel and never a real process (the scheduler is not a `/proc` entry).
pub(crate) type PidIndex = ThpMap<u32, PidSlot>;

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
pub(crate) struct ProcTable {
    /// PID → [`PidSlot`], THP-resident (`thoop::ThpMap`) — on the arena like the stores.
    index: PidIndex,
    /// Hot per-PID CPU history on huge pages. Freed immediately on death.
    cpu: GenStore<CpuRing>,
    /// Cold per-PID metadata on huge pages. Freed immediately on death.
    meta: GenStore<PidMeta>,
    /// Generational cmdline storage, persistent across cycles: an unchanged cmdline keeps its
    /// slot (no per-cycle re-copy); a changed/dead one is freed at once (no reader lease).
    cmd_store: CmdStore,
    cgroup_store: CgroupStore,
    flatpak_store: FlatpakStore,
    /// Last real CPU-sample instant (the window origin); `None` until the first sample.
    last: Option<Instant>,
    clk_tck: u64,
    refresh_n: u32,
    /// Full effective-capability mask for this kernel (`cap_last_cap`), read once at startup —
    /// a masked `CapEff` equal to it means the process holds every capability.
    cap_full_mask: u64,
    /// The active source already fills the row's `uid` (the BPF task iterator carries it), so
    /// the cmdline read here must **not** overwrite it — a failed/permission-denied cmdline
    /// open would otherwise clobber a good uid with `u32::MAX`. The `/proc` path leaves this
    /// `false`: there, uid has no source but this table's cmdline-fd `fstat`.
    source_provides_uid: bool,
    /// Monotonic version of the identity-relevant per-PID inputs (cmdline / cgroup / flatpak
    /// content, plus births / deaths / PID reuse). Advances only when one of those actually
    /// changes — a stale re-read of unchanged bytes does not move it — so a settled cycle leaves
    /// it fixed and the identity resolver can skip recomputation entirely.
    meta_epoch: u64,
}

impl ProcTable {
    pub(crate) fn new(
        arena: &Arena,
        clk_tck: u64,
        refresh_n: u32,
        source_provides_uid: bool,
    ) -> Self {
        Self {
            index: PidIndex::new(arena, PIDINDEX_MIN_CAP),
            cpu: GenStore::new(arena, CPURING_MIN_SLOTS),
            meta: GenStore::new(arena, PIDMETA_MIN_SLOTS),
            cmd_store: CmdStore::new(arena, CMD_STORE_MIN_SLOTS),
            cgroup_store: CgroupStore::new(arena, CMD_STORE_MIN_SLOTS),
            flatpak_store: FlatpakStore::new(arena, CMD_STORE_MIN_SLOTS),
            last: None,
            clk_tck,
            refresh_n,
            cap_full_mask: sys::cap_full_mask(),
            source_provides_uid,
            meta_epoch: 0,
        }
    }

    /// Monotonic version of the identity-relevant per-PID inputs — see [`Self::meta_epoch`]. The
    /// identity resolver rebuilds only when this moves.
    pub(crate) fn meta_epoch(&self) -> u64 {
        self.meta_epoch
    }

    /// Bind the stores to the (now pinned, boxed) arena. Call once after construction, before
    /// any update — each store caches a `*const Arena` and its current base.
    pub(crate) fn wire(&mut self, arena: &Arena) {
        self.index.wire(arena);
        self.cpu.wire(arena);
        self.meta.wire(arena);
        self.cmd_store.wire(arena);
        self.cgroup_store.wire(arena);
        self.flatpak_store.wire(arena);
    }

    /// The shared PID index — read by the `/proc` source's skip-cycle leader check to tell a
    /// known PID from a probe-introduced non-leader thread.
    pub(crate) fn index(&self) -> &PidIndex {
        &self.index
    }

    /// Current high-water slot count of the `Cmd` store (a test/metric hook: confirms unchanged
    /// cmdlines are not re-interned per cycle).
    #[cfg(test)]
    pub(crate) fn cmd_slot_count(&self) -> usize {
        self.cmd_store.slot_count()
    }

    /// Resolve a row's cmdline bytes directly from the `Cmd` store (`&self` read; no lease).
    pub(crate) fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
        self.cmd_store.get(e.cmdline)
    }

    /// One per-PID pass filling CPU% + uid + cmdline, then evicting vanished PIDs. `now` drives
    /// the CPU window; `cur_gen` keys the cmdline interval. CPU% reads fresh every cycle;
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
    #[allow(clippy::too_many_lines)]
    pub(crate) fn update<R: ProcReader>(
        &mut self,
        procs: &mut Procs,
        now: Instant,
        cur_gen: u64,
        reader: &mut R,
    ) {
        // CPU window: real elapsed since the last sample, or `None` if too soon / first call.
        let window = self
            .last
            .map(|t| now.saturating_duration_since(t))
            .filter(|e| *e >= MIN_SAMPLE);
        let first = self.last.is_none();
        let jiff =
            window.map(|e| u32::try_from(elapsed_jiffies(e, self.clk_tck)).unwrap_or(u32::MAX));

        let refresh_n = self.refresh_n;
        let cap_full_mask = self.cap_full_mask;
        // Per-cycle budget for the heavy deleted-library maps scan (see [`LIB_CHECKS_PER_CYCLE`]).
        let mut lib_budget = LIB_CHECKS_PER_CYCLE;
        // Did any identity-relevant input move this cycle (birth / reuse / cmdline / cgroup /
        // flatpak change)? Deaths are folded in by `evict`. Drives the metadata epoch.
        let mut meta_changed = false;

        for i in 0..procs.as_slice().len() {
            let mut e = procs.row(i); // copy out — no live row ref spans a store op below
            let pid = e.pid;
            // One probe: read the prior slot *and* its slot index. The value is copied out; the
            // index is kept (not a pointer) to write back after the per-PID store work, which
            // can relocate the index's chunk but never re-homes the slot (see the method doc).
            let prior_entry: Option<(usize, PidSlot)> = self.index.get_entry(pid);
            let prior: Option<PidSlot> = prior_entry.map(|(_, s)| s);
            let reused = prior.is_some_and(|s| s.start_time != e.start_time);
            // A birth or a PID reuse is a fresh incarnation — its identity is new either way.
            meta_changed |= prior.is_none() || reused;
            let settling = prior.is_some_and(|s| {
                !reused && cur_gen.wrapping_sub(s.first_seen_gen) < u64::from(CMDLINE_SETTLE_GENS)
            });

            // CPU ring (hot): mutated in place; its &mut never spans the cmd/meta ops below.
            let cpu_ref = self.sample_cpu(&mut e, prior, reused, jiff);

            // Metadata (cold): uid + cmdline + cgroup provenance on the interval.
            let stagger =
                refresh_n <= 1 || cur_gen.wrapping_add(u64::from(pid)) % u64::from(refresh_n) == 0;
            let refresh = prior.is_none() || reused || settling || stagger;
            let fresh = if refresh {
                if e.is_kthread {
                    Some(CmdlineRead::KTHREAD) // kthreads: root-owned, no cmdline, no syscall
                } else {
                    Some(reader.cmdline_uid(pid))
                }
            } else {
                None
            };

            let meta_ref = match prior {
                Some(s) => s.meta,
                None => self.meta.insert(Gen::ALIVE, PidMeta::EMPTY),
            };
            if reused {
                let old = *self.meta.get(meta_ref);
                self.cgroup_store.free(old.cgroup);
                self.flatpak_store.free(old.flatpak);
            }
            // Explicit field borrows: `fresh` borrows `self.reader`, refresh_meta touches only
            // `self.meta`/`self.cmd_store` — disjoint, so the cmdline bytes intern straight from
            // the reader's buffer with no intermediate copy.
            let (mut pm, cmd_changed) =
                Self::refresh_meta(&mut self.meta, &mut self.cmd_store, meta_ref, reused, fresh);
            meta_changed |= cmd_changed;
            if refresh && !e.is_kthread {
                let cgroup = reader.cgroup(pid);
                meta_changed |= refresh_bytes(&mut self.cgroup_store, &mut pm.cgroup, cgroup);
                let cmdline = self.cmd_store.get(pm.cmd);
                if likely_flatpak(e.comm(), cmdline, self.cgroup_store.get(pm.cgroup)) {
                    let flatpak = reader.flatpak_info(pid);
                    meta_changed |=
                        refresh_bytes(&mut self.flatpak_store, &mut pm.flatpak, flatpak);
                } else {
                    meta_changed |= !pm.flatpak.is_empty();
                    self.flatpak_store.free(pm.flatpak);
                    pm.flatpak = StringRef::EMPTY;
                }
            }

            // Deleted-binary detection (userspace only — kthreads have neither exe nor maps).
            // A PID-reuse resets the flags with the rest of the incarnation (`PidMeta::EMPTY`
            // from `refresh_meta`), so `pm` already carries the prior incarnation's latched
            // state where applicable.
            let first_seen_gen = match prior {
                Some(s) if !reused => s.first_seen_gen,
                _ => cur_gen,
            };
            let prior_lib_gen = match prior {
                Some(s) if !reused => s.lib_checked_gen,
                _ => 0,
            };
            // Kernel threads run in-kernel with a full capability set, but they are the kernel,
            // not privileged userspace — leave their metadata unremarkable and skip the reads.
            let lib_checked_gen = if e.is_kthread {
                0
            } else {
                Self::refresh_deletions(
                    reader,
                    &mut pm,
                    pid,
                    &mut lib_budget,
                    DelCtx {
                        refresh,
                        // Effective capabilities are fixed at exec and effectively static, so
                        // read them only through the settling window (new / reused / settling),
                        // never on the steady-state stagger — a permanent per-tick status read
                        // for a value that does not change.
                        cap_refresh: prior.is_none() || reused || settling,
                        cap_full_mask,
                        age: cur_gen.wrapping_sub(first_seen_gen),
                        prior_lib_gen,
                        cur_gen,
                    },
                )
            };
            self.meta.assign(meta_ref, pm);

            // uid: the BPF source already set it on the row; the `/proc` source has no other
            // source than this table's read, so take it from the refreshed metadata.
            if !self.source_provides_uid {
                e.uid = pm.uid;
            }
            e.cmdline = pm.cmd;
            if !pm.cmd.is_empty() {
                e.non_ascii |= pm.cmd_non_ascii;
            }
            e.exe_deleted = pm.exe_deleted;
            e.uses_deleted_lib = pm.uses_deleted_lib;
            e.caps = pm.caps;

            let slot = PidSlot {
                cpu: cpu_ref,
                meta: meta_ref,
                start_time: e.start_time,
                first_seen_gen,
                seen_gen: cur_gen,
                lib_checked_gen,
            };
            // Write the slot back: a known PID overwrites at its already-probed slot (no second
            // probe); a birth inserts (may rehash → relocate sibling chunks, all self-healed).
            match prior_entry {
                Some((slot_idx, _)) => self.index.update_at(slot_idx, slot),
                None => self.index.insert(pid, slot),
            }

            procs.set(i, e); // write back
        }

        let evicted = self.evict(cur_gen);
        // Any birth, death, reuse, or content change advances the epoch; a purely steady cycle
        // leaves it fixed so the identity resolver reuses its cache.
        if meta_changed || evicted {
            self.meta_epoch = self.meta_epoch.wrapping_add(1);
        }

        // The window origin advances only on a real sample (or the first call); a too-soon
        // cycle leaves it so elapsed keeps accumulating until it exceeds `MIN_SAMPLE`.
        if first || window.is_some() {
            self.last = Some(now);
        }
    }

    /// Drop PIDs not seen this cycle: free their hot + cold slots and their cmd slot. All
    /// immediate — the dead PID's row was compacted out before this pass, so nothing references
    /// any of its slots. Returns whether any PID was dropped (a death moves the metadata epoch).
    fn evict(&mut self, cur_gen: u64) -> bool {
        let Self {
            index,
            cpu,
            meta,
            cmd_store,
            cgroup_store,
            flatpak_store,
            ..
        } = self;
        let mut dropped = false;
        index.retain(|_, slot| {
            if slot.seen_gen == cur_gen {
                true
            } else {
                let pm = meta.get(slot.meta);
                let cmd = pm.cmd;
                let cgroup = pm.cgroup;
                let flatpak = pm.flatpak;
                cmd_store.free(cmd);
                cgroup_store.free(cgroup);
                flatpak_store.free(flatpak);
                meta.free(slot.meta);
                cpu.free(slot.cpu);
                dropped = true;
                false
            }
        });
        dropped
    }

    /// Fold this cycle's tick observation into the PID's hot CPU ring (creating it for a birth),
    /// writing the derived `cpu_pct`/`cpu_peak` and the display-state override onto the row.
    /// Returns the ring's slot for the caller to store in `PidSlot`. The ring `&mut` never spans
    /// a cmd/meta store op (which could relocate a chunk), so it stays a tight in-place borrow.
    fn sample_cpu(
        &mut self,
        e: &mut ProcessEntry,
        prior: Option<PidSlot>,
        reused: bool,
        jiff: Option<u32>,
    ) -> Ref<CpuRing> {
        let cpu_ref = match prior {
            Some(s) => s.cpu,
            None => self.cpu.insert(Gen::ALIVE, CpuRing::new(e.ticks)),
        };
        let ring = self.cpu.get_mut(cpu_ref);
        ring.sample(e.ticks, jiff, reused, prior.is_some());
        e.cpu_pct = ring.avg();
        e.cpu_peak = ring.peak();
        // Display-time state: a process that ran in the recent window reads `R` stably;
        // otherwise the raw kernel state stands (so D/Z/T/X appear only when the process
        // genuinely isn't executing). Raw `state` is untouched (kill-safety / diagnostics).
        e.display_state = if ring.had_ticks(ACTIVE_SAMPLES) {
            b'R'
        } else {
            e.state
        };
        cpu_ref
    }

    /// Update this PID's cold metadata slot and return the new record (also written back).
    /// Carries the cached record forward, or resets on reuse (freeing the dead incarnation's cmd
    /// slot — immediate, no lease). A `fresh` read replaces the cmd slot **only when the bytes
    /// changed** (free old, intern new); an unchanged or not-refreshed cmdline keeps its slot.
    ///
    /// An associated fn over the two stores it touches (not `&mut self`) so the caller can pass
    /// `fresh` borrowing a *different* field — the [`ProcReader`]'s buffer — without a borrow
    /// conflict; the cmdline bytes intern straight from that buffer.
    ///
    /// Returns the record plus whether the cmdline *content* changed (an input to the metadata
    /// epoch). Reuse is not reported here — the caller already accounts for it.
    fn refresh_meta(
        meta: &mut GenStore<PidMeta>,
        cmd_store: &mut CmdStore,
        meta_ref: Ref<PidMeta>,
        reused: bool,
        fresh: Option<CmdlineRead<'_>>,
    ) -> (PidMeta, bool) {
        let mut pm = if reused {
            cmd_store.free(meta.get(meta_ref).cmd);
            PidMeta::EMPTY
        } else {
            *meta.get(meta_ref)
        };
        let mut cmd_changed = false;
        if let Some(CmdlineRead {
            uid,
            bytes,
            non_ascii,
        }) = fresh
        {
            pm.uid = uid;
            if bytes.is_empty() {
                cmd_changed = !pm.cmd.is_empty();
                cmd_store.free(pm.cmd); // no-op if already empty
                pm.cmd = StringRef::EMPTY;
                pm.cmd_non_ascii = false;
            } else if pm.cmd.is_empty() || cmd_store.get(pm.cmd) != bytes {
                // Changed (or first non-empty argv): free the old slot, intern the new.
                cmd_store.free(pm.cmd);
                pm.cmd = cmd_store.intern(Gen::ALIVE, bytes);
                pm.cmd_non_ascii = non_ascii;
                cmd_changed = true;
            }
            // else: unchanged — keep the existing slot (the whole point of the store).
        }
        meta.assign(meta_ref, pm);
        (pm, cmd_changed)
    }

    /// Resolve a userspace PID's capability level and deleted-binary flags into `pm`, returning
    /// the generation of its last deleted-library scan (stored in `PidSlot`). An associated fn
    /// over the reader + the caller's local `pm` so it composes with the disjoint store borrows
    /// around it, exactly like [`refresh_meta`](Self::refresh_meta).
    ///
    /// Each signal refreshes on the interval its change-rate warrants: caps only through the
    /// settling window (`cap_refresh` — they are fixed at exec); exe-deleted is permanent
    /// (re-probed on the cmdline interval only while unmarked, then latched); deleted-lib is
    /// transient (first scan after settling, then ≤ once per [`LIB_RECHECK_GENS`], globally
    /// ≤ [`LIB_CHECKS_PER_CYCLE`] per cycle). Exe deletion takes visual priority, so when it is
    /// set the (heavier, redundant) maps scan is skipped and any prior lib warning cleared.
    fn refresh_deletions<R: ProcReader>(
        reader: &mut R,
        pm: &mut PidMeta,
        pid: u32,
        lib_budget: &mut u32,
        ctx: DelCtx,
    ) -> u64 {
        if ctx.cap_refresh {
            pm.caps = cap_level(ctx.cap_full_mask, reader.cap_eff(pid));
        }
        if ctx.refresh && !pm.exe_deleted {
            pm.exe_deleted = reader.exe_deleted(pid);
        }
        if pm.exe_deleted {
            pm.uses_deleted_lib = false;
            return ctx.prior_lib_gen;
        }
        let due = ctx.age >= LIB_SETTLE_GENS
            && (ctx.prior_lib_gen == 0
                || ctx.cur_gen.wrapping_sub(ctx.prior_lib_gen) >= LIB_RECHECK_GENS);
        if due && *lib_budget > 0 {
            pm.uses_deleted_lib = reader.lib_deleted(pid);
            *lib_budget -= 1;
            ctx.cur_gen
        } else {
            ctx.prior_lib_gen
        }
    }
}

/// Interval inputs for [`ProcTable::refresh_deletions`], grouped so the call stays a few
/// arguments. All are per-PID values the caller already computed for the metadata refresh.
#[derive(Clone, Copy)]
struct DelCtx {
    /// The cmdline interval fired this cycle — the trigger for the (permanent) exe re-probe.
    refresh: bool,
    /// The PID is new / reused / still settling — the only window in which capabilities (fixed
    /// at exec) are read, so the steady state issues no per-tick status read.
    cap_refresh: bool,
    /// Full capability mask for this kernel (for classifying `CapEff`).
    cap_full_mask: u64,
    /// Cycles since first seen — controls the first deleted-library scan (settling).
    age: u64,
    /// Generation of the prior deleted-library scan (0 = never), for the re-check interval.
    prior_lib_gen: u64,
    /// The generation being built.
    cur_gen: u64,
}

/// What one `/proc/<pid>` identity read yields: owner `uid`, the cleaned cmdline `bytes`
/// (NUL→space, borrowed from the reader's buffer until its next read), and whether any byte is
/// ≥ 0x80. A **whole** value — the bytes travel *with* their metadata, not as a length into a
/// buffer the caller owns separately.
#[derive(Clone, Copy)]
pub(crate) struct CmdlineRead<'a> {
    pub(crate) uid: u32,
    pub(crate) bytes: &'a [u8],
    pub(crate) non_ascii: bool,
}

impl CmdlineRead<'static> {
    /// A kernel thread: root-owned, no cmdline — no syscall needed.
    pub(crate) const KTHREAD: CmdlineRead<'static> = CmdlineRead {
        uid: 0,
        bytes: &[],
        non_ascii: false,
    };
    /// The open failed (gone / permission denied): identity unknown, no cmdline.
    pub(crate) const UNKNOWN: CmdlineRead<'static> = CmdlineRead {
        uid: u32::MAX,
        bytes: &[],
        non_ascii: false,
    };
}

/// Supplies per-PID metadata reads for the table pass. Implementors own any scratch buffers
/// backing returned [`CmdlineRead`] bytes; callers may borrow those bytes only until the next
/// reader call.
pub(crate) trait ProcReader {
    fn cmdline_uid(&mut self, pid: u32) -> CmdlineRead<'_>;
    fn cgroup(&mut self, pid: u32) -> &[u8];
    fn flatpak_info(&mut self, pid: u32) -> &[u8];
    fn cap_eff(&mut self, pid: u32) -> u64;
    fn exe_deleted(&mut self, pid: u32) -> bool;
    fn lib_deleted(&mut self, pid: u32) -> bool;
}

/// Reads `/proc/<pid>` files into buffers it **owns** — the path scratch and the read buffer the
/// result borrows from. Owning both is what lets a read return a whole [`CmdlineRead`] instead
/// of handing the caller back a length into a buffer it had to pass in. One per [`Gatherer`];
/// the gather loop reuses it, so a read allocates nothing.
pub(crate) struct RealProcReader {
    path: ProcPath,
    buf: Vec<u8>,
    /// Reused read buffer for whole multi-line `/proc/<pid>` files (`maps`, `status`) — grown
    /// once to the largest file seen, never per-call allocated.
    read_buf: Vec<u8>,
}

impl RealProcReader {
    pub(crate) fn new() -> Self {
        Self {
            path: ProcPath::new(),
            buf: vec![0u8; CMD_SLOT],
            read_buf: Vec::new(),
        }
    }

    /// Read a whole `/proc/<pid>/<suffix>` into [`read_buf`](Self::read_buf), returning the
    /// slice (capped at [`MAPS_MAX`]). The shared reader for the multi-line files whose field
    /// we scan for rather than parse positionally.
    fn read_all(&mut self, pid: u32, suffix: &[u8]) -> &[u8] {
        let ptr = self.path.write(pid, suffix);
        // SAFETY: valid C path, read-only.
        let fd = unsafe { libc::open(ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            self.read_buf.clear();
            return &self.read_buf;
        }
        self.read_buf.clear();
        let mut chunk = [0u8; 8192];
        loop {
            // SAFETY: chunk is a valid writable buffer; fd is open.
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            let Ok(n) = usize::try_from(n) else { break }; // <0 ⇒ error
            if n == 0 {
                break; // EOF
            }
            self.read_buf
                .extend_from_slice(&chunk[..n.min(chunk.len())]);
            if self.read_buf.len() >= MAPS_MAX {
                break;
            }
        }
        // SAFETY: our fd, closed once.
        unsafe { libc::close(fd) };
        &self.read_buf
    }
}

impl ProcReader for RealProcReader {
    fn cgroup(&mut self, pid: u32) -> &[u8] {
        self.read_all(pid, b"cgroup")
    }

    fn flatpak_info(&mut self, pid: u32) -> &[u8] {
        self.read_all(pid, b"root/.flatpak-info")
    }

    /// The process's effective-capability mask from `/proc/<pid>/status` (`CapEff:` line, hex).
    /// 0 if unreadable. Scanned, not positionally parsed — the line's order in `status` is not
    /// contractual.
    fn cap_eff(&mut self, pid: u32) -> u64 {
        self.read_all(pid, b"status");
        self.read_buf
            .split(|&b| b == b'\n')
            .find_map(|line| line.strip_prefix(b"CapEff:").map(parse_hex))
            .unwrap_or(0)
    }

    /// Whether `/proc/<pid>/exe` resolves to a target the kernel marked `" (deleted)"` — the
    /// running binary was unlinked or replaced on disk. One `readlink`, nothing opened; a pure
    /// suffix check. Uses a local buffer (not `self.buf`), so it never clobbers cmdline bytes a
    /// caller may still hold.
    fn exe_deleted(&mut self, pid: u32) -> bool {
        let ptr = self.path.write(pid, b"exe");
        let mut target = [0u8; 4096];
        // SAFETY: valid C path; readlink writes ≤ len bytes into the local buffer (no NUL).
        let n = unsafe { libc::readlink(ptr, target.as_mut_ptr().cast(), target.len()) };
        let len = usize::try_from(n).unwrap_or(0).min(target.len());
        target[..len].ends_with(DELETED_SUFFIX)
    }

    /// Whether any executable (`x`-perm) mapping in `/proc/<pid>/maps` points at a deleted
    /// file — a replaced/unlinked shared library. Heavier than [`exe_deleted`] (a whole
    /// multi-line file), so the caller rate-limits it.
    fn lib_deleted(&mut self, pid: u32) -> bool {
        // A maps line is `addr perms offset dev inode path`; an executable mapping has `x` as
        // the third perms char, and the path is the line's tail — so an exec line ending in
        // " (deleted)" is a deleted library.
        self.read_all(pid, b"maps")
            .split(|&b| b == b'\n')
            .any(|line| {
                let perms = line
                    .split(|&b| b == b' ')
                    .filter(|f| !f.is_empty())
                    .nth(1)
                    .unwrap_or(&[]);
                perms.get(2) == Some(&b'x') && line.ends_with(DELETED_SUFFIX)
            })
    }

    /// Read `/proc/<pid>/cmdline` (cleaned: NUL→space) plus the owner `uid` from the same fd's
    /// `fstat` — uid rides the cmdline fd for free. The returned bytes borrow `self.buf`, valid
    /// until the next read; `uid` is valid even when the cmdline is empty.
    fn cmdline_uid(&mut self, pid: u32) -> CmdlineRead<'_> {
        let ptr = self.path.write(pid, b"cmdline");
        // SAFETY: valid C path, read-only.
        let fd = unsafe { libc::open(ptr, libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return CmdlineRead::UNKNOWN;
        }
        // SAFETY: buf is a valid writable region; fd is open.
        let (n, uid) = unsafe {
            let n = libc::read(fd, self.buf.as_mut_ptr().cast(), self.buf.len());
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
                bytes: &[],
                non_ascii: false,
            };
        }
        let raw = usize::try_from(n).unwrap_or(0).min(self.buf.len());
        let (clean_len, non_ascii) = parse::clean_cmdline(&mut self.buf[..raw]);
        CmdlineRead {
            uid,
            bytes: &self.buf[..clean_len as usize],
            non_ascii,
        }
    }
}

/// Classify an effective-capability mask against the kernel's full set: nothing, some, or all.
/// Bits above `cap_last_cap` are masked off first (a process may carry higher bits that this
/// kernel doesn't define).
fn cap_level(full_mask: u64, eff: u64) -> CapLevel {
    match eff & full_mask {
        0 => CapLevel::None,
        m if m == full_mask => CapLevel::Full,
        _ => CapLevel::Partial,
    }
}

/// Intern `bytes` into `slot` only when the content actually differs (an unchanged value keeps
/// its slot — no re-copy). Returns whether the stored content changed, which the caller folds
/// into the metadata epoch.
fn refresh_bytes<const N: usize, S>(
    store: &mut StrStore<N, S>,
    slot: &mut StringRef<S>,
    bytes: &[u8],
) -> bool {
    let bytes = &bytes[..bytes.len().min(N)];
    if bytes.is_empty() {
        let had_content = !slot.is_empty();
        store.free(*slot);
        *slot = StringRef::EMPTY;
        had_content
    } else if slot.is_empty() || store.get(*slot) != bytes {
        store.free(*slot);
        *slot = store.intern(Gen::ALIVE, bytes);
        true
    } else {
        false
    }
}

fn likely_flatpak(comm: &[u8], cmdline: &[u8], cgroup: &[u8]) -> bool {
    comm == b"bwrap"
        || contains_ascii(cgroup, b"flatpak")
        || contains_ascii(cmdline, b"flatpak")
        || contains_ascii(cmdline, b"bwrap")
}

fn contains_ascii(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// Parse an ASCII hex integer, skipping leading whitespace and stopping at the first non-hex
/// byte (e.g. the trailing newline). Saturates rather than wraps on overflow (a `CapEff` value
/// never exceeds 64 bits, so this is a defensive guard, not a real case).
fn parse_hex(b: &[u8]) -> u64 {
    let mut v: u64 = 0;
    for &c in b {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            b' ' | b'\t' => continue,
            _ => break,
        };
        v = (v << 4) | u64::from(d);
    }
    v
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{CapLevel, ProcReader, RealProcReader, cap_level, parse_hex};

    #[test]
    fn cap_level_buckets() {
        let full = 0x1ff; // a pretend 9-bit capability set
        assert_eq!(cap_level(full, 0), CapLevel::None);
        assert_eq!(cap_level(full, full), CapLevel::Full);
        assert_eq!(cap_level(full, 0x004), CapLevel::Partial);
        // Bits above the kernel's defined set are masked off before classifying.
        assert_eq!(cap_level(full, u64::MAX), CapLevel::Full);
    }

    #[test]
    fn parse_hex_reads_capeff_field() {
        assert_eq!(parse_hex(b" 0000000000000000\n"), 0);
        assert_eq!(parse_hex(b"\t00000000a80425fb\n"), 0xa804_25fb);
        assert_eq!(parse_hex(b"000001ffffffffff"), 0x1ff_ffff_ffff);
    }

    #[test]
    fn own_cap_eff_is_readable() {
        // Reading our own status must not panic/hang; the value depends on how we were run.
        let mut r = RealProcReader::new();
        let _ = r.cap_eff(std::process::id());
    }

    #[test]
    fn own_exe_and_libs_are_not_deleted() {
        let mut r = RealProcReader::new();
        let me = std::process::id();
        assert!(!r.exe_deleted(me), "our own binary is present on disk");
        assert!(!r.lib_deleted(me), "our own libraries are present on disk");
    }

    /// A binary unlinked while still running: the kernel marks `/proc/<pid>/exe` " (deleted)".
    /// Copies a real binary to a temp path, execs it, waits for the exec to complete, then
    /// unlinks it. Skips (rather than fails) if the environment lacks a binary to copy or the
    /// temp dir is noexec — the default suite must not hard-require external state.
    #[test]
    fn detects_deleted_exe() {
        let Some(src) = ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
        else {
            eprintln!("no sleep binary — skipping");
            return;
        };
        let path = std::env::temp_dir().join(format!("oya_deltest_{}", std::process::id()));
        if std::fs::copy(src, &path).is_err() {
            eprintln!("temp copy failed — skipping");
            return;
        }
        let mut child = match std::process::Command::new(&path).arg("30").spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                eprintln!("spawn from temp failed ({e}) — skipping");
                return;
            }
        };
        // Let the child finish exec'ing before unlinking (unlinking pre-exec would ENOENT the
        // exec); once running, the inode stays open, so the unlink marks exe " (deleted)".
        std::thread::sleep(Duration::from_millis(150));
        let _ = std::fs::remove_file(&path);

        let mut r = RealProcReader::new();
        let deleted = r.exe_deleted(child.id());
        child.kill().ok();
        child.wait().ok();
        assert!(
            deleted,
            "a running binary unlinked from disk must read exe_deleted"
        );
    }
}
