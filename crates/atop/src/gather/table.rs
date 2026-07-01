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
use super::cpu::{CpuRing, MIN_SAMPLE, elapsed_jiffies};
use super::parse;
use crate::procs::{Cmd, ProcessEntry, Procs};
use crate::sys::ProcPath;

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

/// Coarse cmdline/uid refresh cadence, overridable via `ATOP_CMDLINE_REFRESH_N`
/// (1 = refresh every PID every cycle).
pub(crate) fn cmdline_refresh_n() -> u32 {
    env_u32("ATOP_CMDLINE_REFRESH_N")
        .unwrap_or(CMDLINE_REFRESH_N)
        .max(1)
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
pub(crate) struct PidSlot {
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
    /// Last real CPU-sample instant (the window origin); `None` until the first sample.
    last: Option<Instant>,
    clk_tck: u64,
    refresh_n: u32,
    /// The active source already fills the row's `uid` (the BPF task iterator carries it), so
    /// the cmdline read here must **not** overwrite it — a failed/permission-denied cmdline
    /// open would otherwise clobber a good uid with `u32::MAX`. The `/proc` path leaves this
    /// `false`: there, uid has no source but this table's cmdline-fd `fstat`.
    source_provides_uid: bool,
    /// Reads `/proc/<pid>/cmdline` + uid into buffers it owns (no per-cycle allocation).
    reader: ProcReader,
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
            last: None,
            clk_tck,
            refresh_n,
            source_provides_uid,
            reader: ProcReader::new(),
        }
    }

    /// Bind the stores to the (now pinned, boxed) arena. Call once after construction, before
    /// any update — each store caches a `*const Arena` and its current base.
    pub(crate) fn wire(&mut self, arena: &Arena) {
        self.index.wire(arena);
        self.cpu.wire(arena);
        self.meta.wire(arena);
        self.cmd_store.wire(arena);
    }

    /// The shared PID index — read by the `/proc` source's skip-cycle leader gate to tell a
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
    pub(crate) fn update(&mut self, procs: &mut Procs, now: Instant, cur_gen: u64) {
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
                    Some(CmdlineRead::KTHREAD) // kthreads: root-owned, no cmdline, no syscall
                } else {
                    Some(self.reader.cmdline_uid(pid))
                }
            } else {
                None
            };

            let meta_ref = match prior {
                Some(s) => s.meta,
                None => self.meta.insert(Gen::ALIVE, PidMeta::EMPTY),
            };
            // Explicit field borrows: `fresh` borrows `self.reader`, refresh_meta touches only
            // `self.meta`/`self.cmd_store` — disjoint, so the cmdline bytes intern straight from
            // the reader's buffer with no intermediate copy.
            let pm =
                Self::refresh_meta(&mut self.meta, &mut self.cmd_store, meta_ref, reused, fresh);

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
    /// slot — immediate, no lease). A `fresh` read replaces the cmd slot **only when the bytes
    /// changed** (free old, intern new); an unchanged or not-refreshed cmdline keeps its slot.
    ///
    /// An associated fn over the two stores it touches (not `&mut self`) so the caller can pass
    /// `fresh` borrowing a *different* field — the [`ProcReader`]'s buffer — without a borrow
    /// conflict; the cmdline bytes intern straight from that buffer.
    fn refresh_meta(
        meta: &mut GenStore<PidMeta>,
        cmd_store: &mut CmdStore,
        meta_ref: Ref<PidMeta>,
        reused: bool,
        fresh: Option<CmdlineRead<'_>>,
    ) -> PidMeta {
        let mut pm = if reused {
            cmd_store.free(meta.get(meta_ref).cmd);
            PidMeta::EMPTY
        } else {
            *meta.get(meta_ref)
        };
        if let Some(CmdlineRead {
            uid,
            bytes,
            non_ascii,
        }) = fresh
        {
            pm.uid = uid;
            if bytes.is_empty() {
                cmd_store.free(pm.cmd); // no-op if already empty
                pm.cmd = StringRef::EMPTY;
                pm.cmd_non_ascii = false;
            } else if pm.cmd.is_empty() || cmd_store.get(pm.cmd) != bytes {
                // Changed (or first non-empty argv): free the old slot, intern the new.
                cmd_store.free(pm.cmd);
                pm.cmd = cmd_store.intern(Gen::ALIVE, bytes);
                pm.cmd_non_ascii = non_ascii;
            }
            // else: unchanged — keep the existing slot (the whole point of the store).
        }
        meta.assign(meta_ref, pm);
        pm
    }
}

/// What one `/proc/<pid>` identity read yields: owner `uid`, the cleaned cmdline `bytes`
/// (NUL→space, borrowed from the reader's buffer until its next read), and whether any byte is
/// ≥ 0x80. A **whole** value — the bytes travel *with* their metadata, not as a length into a
/// buffer the caller owns separately.
#[derive(Clone, Copy)]
struct CmdlineRead<'a> {
    uid: u32,
    bytes: &'a [u8],
    non_ascii: bool,
}

impl CmdlineRead<'static> {
    /// A kernel thread: root-owned, no cmdline — no syscall needed.
    const KTHREAD: CmdlineRead<'static> = CmdlineRead {
        uid: 0,
        bytes: &[],
        non_ascii: false,
    };
    /// The open failed (gone / permission denied): identity unknown, no cmdline.
    const UNKNOWN: CmdlineRead<'static> = CmdlineRead {
        uid: u32::MAX,
        bytes: &[],
        non_ascii: false,
    };
}

/// Reads `/proc/<pid>` files into buffers it **owns** — the path scratch and the read buffer the
/// result borrows from. Owning both is what lets a read return a whole [`CmdlineRead`] instead
/// of handing the caller back a length into a buffer it had to pass in. One per [`ProcTable`];
/// the gather loop reuses it, so a read allocates nothing.
struct ProcReader {
    path: ProcPath,
    buf: Vec<u8>,
}

impl ProcReader {
    fn new() -> Self {
        Self {
            path: ProcPath::new(),
            buf: vec![0u8; CMD_SLOT],
        }
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
