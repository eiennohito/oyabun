//! [`BpfSource`]: the loaded BPF programs + maps and the per-cycle emit-on-change logic.
//! The design rationale (emit-on-change, fork/free pairing, resync) lives in the module
//! overview in `mod.rs`.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::time::Instant;

use aya::maps::{Array, MapData, RingBuf};
use aya::programs::{BtfTracePoint, Iter};
use aya::{Btf, Ebpf};
use thoop::{Arena, TypedBuf};
use zerocopy::FromBytes;

use super::super::source::{CycleResult, Source, SourceCtx};
use super::types::{EVENT_FORK, EVENT_FREE, ProcEvent, TaskInfo};
use crate::fxhash::{FxBuildHasher, FxMap};
use crate::procs::{ProcessEntry, Procs};

/// The committed BPF object — sources in `bpf/`, rebuilt via `just bpf`. Embedded so a normal
/// `cargo build` needs no clang / bpftool (the artifact is the dependency, like a generated
/// protobuf file).
static OBJECT: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../bpf/atop.bpf.o"));

/// Headroom grown into the read buffer per `read()` round of the task-iterator stream.
const READ_CHUNK: usize = 64 * 1024;
/// Initial landing-buffer capacity in `u64` words (≈ 64 KiB ≈ 700 process rows) — sized so a
/// typical box never grows it; a busier host grows via the arena.
const INIT_WORDS: usize = 8 * 1024;

/// Cap on outstanding un-reconciled fork events, a defensive bound against a leak (every real
/// fork is reconciled within ~1 cycle by either a snapshot sighting or a reap). Far above any
/// real fork backlog between two cycles.
const PENDING_CAP: usize = 1 << 16;

/// Periodic full-snapshot backstop, in cycles: even with no detected overflow, force a resync
/// this often so any unanticipated desync self-heals. Coarse — a resync re-emits everyone, the
/// cost emit-on-change avoids — so this is a safety net, not the primary death path (which is
/// the per-cycle `free` events). At `REFRESH_MS=500`, 64 cycles ≈ 32 s.
const RESYNC_CYCLES: u64 = 64;

/// `ctrl` array indices, mirroring `ATOP_CTRL_*` in `bpf/atop.bpf.c`.
const CTRL_EMIT_ALL: u32 = 0;
const CTRL_DROPS: u32 = 1;

/// PID set with the gatherer's fast integer hasher (PIDs are not attacker-controlled, so the
/// default `SipHash` would only cost cycles).
type PidSet = HashSet<u32, FxBuildHasher>;

/// The maintained full live set: PID → its last-known source fields. Heap-resident with the
/// fast PID hasher, like [`PidSet`] — the same arena-residency follow-up the doc notes for the
/// `/proc` backend's fd pools applies here. Rebuilt order-free each cycle from the delta.
type PidTaskMap = FxMap<u32, TaskInfo>;

/// The loaded BPF programs + maps and the per-cycle scratch. Holds `ebpf` alive for the process
/// lifetime so the fork/exit links stay attached.
pub struct BpfSource {
    /// Owns the loaded programs (task iterator + the two attached tracepoints). Kept alive so
    /// the tracepoint links are not dropped (which would detach them).
    ebpf: Ebpf,
    /// Birth/reap ring, taken out of `ebpf` so it can be drained with `&mut`.
    events: RingBuf<MapData>,
    /// Control/stats array (`ctrl` map): index [`CTRL_EMIT_ALL`] forces a full snapshot
    /// (userspace writes), [`CTRL_DROPS`] counts dropped event records (BPF increments).
    ctrl: Array<MapData, u32>,
    /// THP-resident, 8-byte-aligned landing buffer for the iterator stream — cast in place to
    /// `&[TaskInfo]`. A `TypedBuf<u64>` on the shared arena (not a heap `Vec`) so the whole
    /// gather working set lives in the arena; grows to the high-water task count, never shrinks.
    /// Its element length is unused — the read tracks bytes directly over the raw capacity.
    words: TypedBuf<u64>,
    /// The maintained full live set. Each cycle applies the iterator's delta (and the reap
    /// events) here, then materializes it into the row buffer. A resync rebuilds it from a
    /// forced full snapshot.
    set: PidTaskMap,
    /// Forked PIDs not yet reconciled against a snapshot — used to classify a fork+free pair
    /// that never appeared in any snapshot as short-lived.
    pending: PidSet,
    /// Cycle counter, driving the periodic resync backstop and the first-cycle full sync.
    cycle: u64,
    /// Last-seen event-drop counter; an increase means the ringbuf overflowed → arm a resync.
    prev_drops: u32,
    /// Set when a resync read produced nothing (the iterator attach/read failed): the set was
    /// *not* cleared, and the next cycle re-arms the resync so the full snapshot is retried —
    /// without this, a delta after a failed resync would only refill the few changed PIDs.
    force_resync: bool,
    /// Rows the iterator emitted this cycle — the change count. ≈ the full set on a resync,
    /// a small fraction on a steady-state delta. The measure of the emit-on-change win.
    #[cfg_attr(not(test), allow(dead_code))] // read only by tests today; a future churn metric
    last_emitted: usize,
    page_size: u64,
    /// Precomputed ns→clock-tick divisor (see [`types::ns_per_tick`]).
    ns_per_tick: u64,
}

impl Source for BpfSource {
    fn populate(&mut self, procs: &mut Procs, _ctx: SourceCtx<'_>) -> CycleResult {
        let (now, short_lived) = self.scan(procs);
        CycleResult {
            now,
            pool_overflow: 0,
            short_lived,
        }
    }
}

impl BpfSource {
    /// Try to load the privileged layer. `None` (with a one-line note under `ATOP_FORCE_BPF`
    /// or debug builds) means fall back to `/proc`. `ATOP_NO_BPF` skips the attempt entirely.
    /// The landing buffer is allocated in `arena` (the gatherer's shared THP arena); the result
    /// must be [`wire`](Self::wire)d once the arena is pinned, before any [`scan`](Self::scan).
    pub fn probe(arena: &Arena, page_size: u64, clk_tck: u64) -> Option<Self> {
        if std::env::var_os("ATOP_NO_BPF").is_some() {
            return None;
        }
        match Self::load(arena, page_size, clk_tck) {
            Ok(src) => Some(src),
            Err(e) => {
                if std::env::var_os("ATOP_FORCE_BPF").is_some() || cfg!(debug_assertions) {
                    eprintln!("atop: privileged BPF mode unavailable, using /proc: {e}");
                }
                None
            }
        }
    }

    fn load(
        arena: &Arena,
        page_size: u64,
        clk_tck: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let btf = Btf::from_sys_fs()?;
        // `Ebpf::load` applies CO-RE relocations against the kernel BTF at load time.
        let mut ebpf = Ebpf::load(OBJECT)?;

        // Task iterator: load (resolving the `bpf_iter_task` attach point); attach is per-read.
        let iter: &mut Iter = program(&mut ebpf, "atop_task_iter")?.try_into()?;
        iter.load("task", &btf)?;

        // Fork/free tracepoints: load + attach once; the links live in `ebpf`. `free` is the
        // reap (release_task) — a zombie stays a live /proc entry until then.
        let fork: &mut BtfTracePoint = program(&mut ebpf, "atop_sched_fork")?.try_into()?;
        fork.load("sched_process_fork", &btf)?;
        fork.attach()?;
        let free: &mut BtfTracePoint = program(&mut ebpf, "atop_sched_free")?.try_into()?;
        free.load("sched_process_free", &btf)?;
        free.attach()?;

        let events = RingBuf::try_from(ebpf.take_map("events").ok_or("events map missing")?)?;
        let ctrl = Array::try_from(ebpf.take_map("ctrl").ok_or("ctrl map missing")?)?;

        // Allocate the landing buffer only now that the load has succeeded, so a fallback path
        // never leaves an arena chunk behind.
        Ok(Self {
            ebpf,
            events,
            ctrl,
            words: TypedBuf::new(arena, INIT_WORDS),
            set: PidTaskMap::default(),
            pending: PidSet::default(),
            cycle: 0,
            prev_drops: 0,
            force_resync: false,
            last_emitted: 0,
            page_size,
            ns_per_tick: super::types::ns_per_tick(clk_tck),
        })
    }

    /// Bind the landing buffer to the (now pinned) arena. Call once after construction, before
    /// any [`scan`](Self::scan) — like the other arena-resident buffers.
    pub fn wire(&mut self, arena: &Arena) {
        self.words.wire(arena);
    }

    /// Apply this cycle's delta and fill `procs` with the full live set, leaving it
    /// **PID-sorted** (the tree build's precondition). Returns the sample instant and the count
    /// of short-lived processes (fork+free between snapshots) drained this cycle.
    ///
    /// The iterator emits only *changed* leaders; the maintained [`set`](Self::set) carries the
    /// rest forward. A **resync** (first cycle, a detected ringbuf overflow, or the periodic
    /// backstop) forces a full snapshot and rebuilds the set from it, so a death whose `free`
    /// event was dropped cannot linger.
    pub fn scan(&mut self, procs: &mut Procs) -> (Instant, u32) {
        self.cycle += 1;

        // Decide resync, then tell the BPF program (it reads `emit_all` during the walk). An
        // overflow is detected by the drop counter moving since last cycle.
        let drops = self.ctrl.get(&CTRL_DROPS, 0).unwrap_or(0);
        let resync = self.cycle == 1
            || self.cycle.is_multiple_of(RESYNC_CYCLES)
            || drops != self.prev_drops
            || self.force_resync;
        self.prev_drops = drops;
        let _ = self.ctrl.set(CTRL_EMIT_ALL, u32::from(resync), 0);

        // Drain reaps *before* the walk, never after: a `free` event in the ring means the PID
        // is already off the kernel task list, so this cycle's walk physically cannot re-emit it
        // — `set.remove` here is final, not something the delta below could undo. (A reap whose
        // RCU-deferred event lands after the walk simply waits one more cycle; that is the only
        // staleness, bounded by a grace period.)
        let short_lived = self.drain_events();

        // Sample instant just before the bulk read — the snapshot's "as of" time, the basis for
        // the CPU% window exactly as the /proc path's pre-collect instant is.
        let now = Instant::now();
        let bytes = self.read_snapshot();

        let size = size_of::<TaskInfo>();
        let usable = bytes - (bytes % size);
        self.last_emitted = usable / size;

        // A resync stream is the complete live set, so rebuild from scratch (this is what evicts
        // a PID whose `free` was dropped). A normal stream is a delta — upsert onto the set.
        // Guard the clear on a non-empty read: a resync that read nothing means the attach/read
        // failed (a real resync always emits ≥1 leader), so keep the prior set rather than
        // blanking, and re-arm the resync next cycle (a delta alone would not refill it).
        self.force_resync = resync && usable == 0;
        if resync && usable > 0 {
            self.set.clear();
        }
        if usable > 0 {
            // The buffer is a `TypedBuf<u64>` (base 8-aligned ≥ `TaskInfo`'s alignment); `usable`
            // bytes were just written by `read`. `ref_from_bytes` validates alignment + length.
            let raw = &self.words.byte_capacity_mut()[..usable];
            let infos = <[TaskInfo]>::ref_from_bytes(raw).expect("8-aligned, length a multiple");
            for ti in infos {
                if ti.pid == 0 {
                    continue; // never a real process (the scheduler is not a /proc entry)
                }
                self.set.insert(ti.pid, *ti);
                // Seen in a snapshot ⇒ confirmed alive, so a later `free` is a real death, not a
                // short-lived born-and-gone. (A fork still absent stays pending for next cycle.)
                self.pending.remove(&ti.pid);
            }
        }
        if self.pending.len() > PENDING_CAP {
            self.pending.clear();
        }

        // Materialize the full set into the row buffer, PID-sorted (the tree precondition). The
        // set is unordered, so sort after; the tail then runs identically to the /proc path.
        procs.clear();
        procs.reserve(self.set.len());
        for ti in self.set.values() {
            let mut e = ProcessEntry::TOMBSTONE;
            ti.write_into(&mut e, self.page_size, self.ns_per_tick);
            procs.push(e);
        }
        procs.sort_by_pid();

        (now, short_lived)
    }

    /// Drain the fork/free ring, maintaining `set` + `pending` and counting short-lived
    /// processes. `fork` arms pairing; `free` (the reap) removes the row and, if its fork was
    /// never reconciled by a snapshot, counts it short-lived (forked and gone between cycles).
    fn drain_events(&mut self) -> u32 {
        let mut short_lived = 0u32;
        while let Some(item) = self.events.next() {
            let Ok((ev, _)) = ProcEvent::ref_from_prefix(&item) else {
                continue;
            };
            match ev.event {
                EVENT_FORK => {
                    self.pending.insert(ev.pid);
                }
                EVENT_FREE => {
                    self.set.remove(&ev.pid); // row gone (no-op if it was never walked)
                    if self.pending.remove(&ev.pid) {
                        short_lived += 1; // never seen in a snapshot — born and died between cycles
                    }
                }
                _ => {}
            }
        }
        short_lived
    }

    /// Read the whole task-iterator stream into `self.words`, returning the byte length. A fresh
    /// iterator instance is created per cycle (attach → create iter fd → read → drop); the few
    /// extra syscalls are negligible at the gather cadence. Returns 0 on any failure (the next
    /// cycle retries — there is no mid-run fallback to `/proc`).
    fn read_snapshot(&mut self) -> usize {
        let mut file = match self.open_iter() {
            Ok(f) => f,
            Err(e) => {
                if cfg!(debug_assertions) {
                    eprintln!("atop: task-iter attach failed: {e}");
                }
                return 0;
            }
        };
        read_stream(&mut file, &mut self.words)
    }

    /// Create one task-iterator instance and return its readable file.
    fn open_iter(&mut self) -> Result<File, Box<dyn std::error::Error>> {
        let iter: &mut Iter = program(&mut self.ebpf, "atop_task_iter")?.try_into()?;
        let link_id = iter.attach()?;
        let link = iter.take_link(link_id)?;
        Ok(link.into_file()?)
    }

    /// Rows the iterator emitted in the last [`scan`](Self::scan) — the change count.
    #[cfg(test)]
    fn emitted(&self) -> usize {
        self.last_emitted
    }
}

/// Fetch a program by name with a typed error (the `?`-friendly form of `program_mut`).
fn program<'a>(
    ebpf: &'a mut Ebpf,
    name: &'static str,
) -> Result<&'a mut aya::programs::Program, Box<dyn std::error::Error>> {
    ebpf.program_mut(name)
        .ok_or_else(|| format!("program {name} missing").into())
}

/// Read `file` to EOF into the arena landing buffer (8-aligned for the `TaskInfo` cast),
/// returning the byte length. Records may split across `read()` chunks; concatenating into one
/// buffer reassembles them, so the total is an exact multiple of `size_of::<TaskInfo>()`.
fn read_stream(file: &mut File, words: &mut TypedBuf<u64>) -> usize {
    let mut len = 0usize;
    loop {
        // Ensure ≥ READ_CHUNK spare bytes beyond `len` (grows the arena chunk if short).
        words.reserve((len + READ_CHUNK).div_ceil(8));
        let buf = words.byte_capacity_mut();
        match file.read(&mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys;
    use thoop::Arena;

    /// `/proc/self/stat` fields after the comm's closing ')': field 22 (starttime, ticks) and
    /// the comm, for cross-checking the BPF snapshot against the kernel's own text path.
    fn proc_self_stat(pid: u32) -> (u64, Vec<u8>) {
        let s = std::fs::read(format!("/proc/{pid}/stat")).expect("read stat");
        let open = s.iter().position(|&b| b == b'(').unwrap();
        let close = s.iter().rposition(|&b| b == b')').unwrap();
        let comm = s[open + 1..close].to_vec();
        let rest = &s[close + 2..];
        let field = rest.split(|&b| b == b' ').nth(19).unwrap();
        let start = std::str::from_utf8(field).unwrap().trim().parse().unwrap();
        (start, comm)
    }

    /// The BPF task-iterator snapshot must agree with `/proc` on the stable identity fields —
    /// most importantly `start_time` (the kill-verification discriminator) must match field 22
    /// exactly, or a privileged-mode kill would refuse to fire. Skips if BPF can't load (no
    /// caps): run under `tools/caprun cargo test`.
    #[test]
    fn bpf_snapshot_matches_proc() {
        let arena = Box::new(Arena::new(0));
        let Some(mut src) = BpfSource::probe(&arena, sys::page_size(), sys::clk_tck()) else {
            eprintln!("BPF unavailable (no caps?) — skipping");
            return;
        };
        let mut procs = Procs::new(&arena, 256);
        src.wire(&arena);
        procs.wire(&arena);

        // Two cycles: the second exercises the per-cycle re-attach + ring drain path.
        src.scan(&mut procs);
        src.scan(&mut procs);

        let rows = procs.as_slice();
        assert!(rows.len() > 10, "too few processes: {}", rows.len());
        assert!(
            rows.windows(2).all(|w| w[0].pid < w[1].pid),
            "snapshot must be PID-sorted (tree precondition)"
        );

        let init = rows.iter().find(|p| p.pid == 1).expect("pid 1 present");
        assert_eq!(init.uid, 0, "pid 1 is root-owned");

        let me = std::process::id();
        let mine = rows.iter().find(|p| p.pid == me).expect("self present");
        assert!(!mine.is_kthread, "the test process is not a kernel thread");
        assert_eq!(mine.uid, sys_uid(), "uid must match getuid()");

        let (proc_start, proc_comm) = proc_self_stat(me);
        assert_eq!(
            mine.start_time, proc_start,
            "start_time must equal /proc field 22 exactly (kill identity)"
        );
        assert_eq!(mine.comm(), proc_comm.as_slice(), "comm must match /proc");
    }

    fn sys_uid() -> u32 {
        // SAFETY: getuid is always safe and never fails.
        unsafe { libc::getuid() }
    }

    /// The fork/free tracepoints fire and reach the ringbuf. A burst of processes born and
    /// reaped entirely *between* snapshots is invisible to the iterator, yet the fork+free
    /// event pair must classify each as short-lived. Skips without caps.
    ///
    /// `sched_process_free` is the **reap**, and the kernel defers it through an RCU callback
    /// (`delayed_put_task_struct`), so the events trickle in over a grace period rather than by
    /// the time `status()` returns. The live tool drains every ~500 ms cycle, far longer than a
    /// grace period; here we poll a few short cycles and accumulate, which is what exercises the
    /// async-delivery path the tool relies on.
    #[test]
    fn bpf_detects_short_lived() {
        let arena = Box::new(Arena::new(0));
        let Some(mut src) = BpfSource::probe(&arena, sys::page_size(), sys::clk_tck()) else {
            eprintln!("BPF unavailable (no caps?) — skipping");
            return;
        };
        let mut procs = Procs::new(&arena, 256);
        src.wire(&arena);
        procs.wire(&arena);

        src.scan(&mut procs); // baseline: drain any pre-existing events

        // Each `status()` spawns then reaps — born and dead before the call returns, so all live
        // and die strictly between this and the following snapshots.
        for _ in 0..50 {
            let _ = std::process::Command::new("true").status();
        }

        // Free events arrive asynchronously (RCU-deferred), so poll up to ~1 s, accumulating.
        let mut short_lived = 0u32;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            short_lived += src.scan(&mut procs).1;
            if short_lived > 0 {
                break;
            }
        }
        assert!(
            short_lived > 0,
            "fork/free tracepoints must catch born-and-died processes; got {short_lived}"
        );
    }

    /// The core emit-on-change win: a resync emits the whole live set, but the very next
    /// (steady-state) cycle emits only the handful of processes that changed — far fewer — while
    /// the materialized row buffer still carries the *full* set forward (unchanged processes are
    /// not lost, they ride the maintained set). Skips without caps.
    #[test]
    fn bpf_emit_on_change_is_a_delta() {
        let arena = Box::new(Arena::new(0));
        let Some(mut src) = BpfSource::probe(&arena, sys::page_size(), sys::clk_tck()) else {
            eprintln!("BPF unavailable (no caps?) — skipping");
            return;
        };
        let mut procs = Procs::new(&arena, 256);
        src.wire(&arena);
        procs.wire(&arena);

        // Cycle 1 is a forced resync: it emits every live leader.
        src.scan(&mut procs);
        let full = src.emitted();
        let total = procs.as_slice().len();
        assert!(full > 20, "resync must emit the whole set; got {full}");
        assert_eq!(full, total, "resync emits exactly the materialized set");

        // Cycle 2 is a steady-state delta: only changed processes re-emit, but the row buffer
        // still holds the full set (unchanged ones carried forward by the maintained set).
        src.scan(&mut procs);
        let changed = src.emitted();
        assert!(
            changed < full,
            "a delta cycle must emit fewer rows than a full snapshot; delta={changed} full={full}"
        );
        assert!(
            procs.as_slice().len() >= full - changed,
            "the full set must survive a delta cycle (unchanged rows kept)"
        );
        assert!(
            procs.as_slice().iter().any(|p| p.pid == 1),
            "pid 1 (idle, not re-emitted) must persist across the delta"
        );
    }

    /// `num_threads` comes from `signal->nr_threads` via CO-RE — and the kernel BTF has more
    /// than one `nr_threads` symbol, so confirm CO-RE resolved `signal_struct`'s (not garbage):
    /// a `sleep` child is single-threaded, so its count must be exactly 1.
    #[test]
    fn bpf_num_threads_is_signal_nr_threads() {
        let arena = Box::new(Arena::new(0));
        let Some(mut src) = BpfSource::probe(&arena, sys::page_size(), sys::clk_tck()) else {
            eprintln!("BPF unavailable (no caps?) — skipping");
            return;
        };
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();

        let mut procs = Procs::new(&arena, 256);
        src.wire(&arena);
        procs.wire(&arena);
        src.scan(&mut procs);

        let found = procs
            .as_slice()
            .iter()
            .find(|p| p.pid == child_pid)
            .copied();
        child.kill().unwrap();
        child.wait().unwrap();

        let s = found.expect("sleep child in snapshot");
        assert_eq!(
            s.num_threads, 1,
            "single-threaded sleep must report 1 thread"
        );
    }

    /// kthreads must come through flagged with empty cmdline semantics and uid 0.
    #[test]
    fn bpf_flags_kernel_threads() {
        let arena = Box::new(Arena::new(0));
        let Some(mut src) = BpfSource::probe(&arena, sys::page_size(), sys::clk_tck()) else {
            eprintln!("BPF unavailable (no caps?) — skipping");
            return;
        };
        let mut procs = Procs::new(&arena, 256);
        src.wire(&arena);
        procs.wire(&arena);
        src.scan(&mut procs);

        // pid 2 is kthreadd on Linux — always a kernel thread, root-owned.
        let kthreadd = procs.as_slice().iter().find(|p| p.pid == 2);
        if let Some(k) = kthreadd {
            assert!(
                k.is_kthread,
                "pid 2 (kthreadd) must be flagged kernel thread"
            );
            assert_eq!(k.uid, 0);
        }
    }
}
