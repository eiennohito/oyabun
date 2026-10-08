//! Zero-copy Rust mirrors of `bpf/oya_types.h` — the BPF↔Rust layout contract.
//!
//! The BPF programs write these C structs; Rust reads the bytes back as `&[TaskInfo]` /
//! `ProcEvent` via a pointer cast (`zerocopy`), never a field-by-field parse. The layouts
//! must match the C header exactly; `size_of` is asserted at compile time on both sides so a
//! drift is a build error. Same machine ⇒ same endianness ⇒ no byte-swapping.
//!
//! Raw kernel values cross the boundary; conversion to oyabun's display units (clock ticks,
//! bytes, the state char) happens here — one place, testable, identical to the `/proc` path.

use zerocopy::{FromBytes, Immutable, KnownLayout};

use crate::procs::ProcessEntry;

/// `TASK_COMM_LEN`.
const COMM_LEN: usize = 16;
/// `PF_KTHREAD` in `task->flags`.
const PF_KTHREAD: u32 = 0x0020_0000;

/// fork/free discriminators in [`ProcEvent::event`], mirroring `OYA_EVENT_*`. `FREE` is the
/// reap (`release_task`): a zombie stays a live `/proc` entry the walk keeps showing until then.
pub const EVENT_FORK: u8 = 0;
pub const EVENT_FREE: u8 = 1;

/// One process row from the `iter/task` program (a thread-group leader). Mirrors
/// `struct oya_task_info`.
#[derive(FromBytes, Immutable, KnownLayout, Clone, Copy)]
#[repr(C)]
pub struct TaskInfo {
    pub utime_ns: u64,
    pub stime_ns: u64,
    pub start_boottime_ns: u64,
    pub rss_pages: i64,
    pub pid: u32,
    pub tgid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub flags: u32,
    pub state: u32,
    pub exit_state: u32,
    pub nr_threads: u32,
    pub prio: i16,
    pub static_prio: i16,
    pub comm: [u8; COMM_LEN],
    pub _pad: [u8; 4],
}

const _: () = assert!(
    size_of::<TaskInfo>() == 88,
    "TaskInfo must match oya_task_info"
);
const _: () = assert!(align_of::<TaskInfo>() == 8);

/// A birth/death record from the ringbuf. Mirrors `struct oya_proc_event`.
#[derive(FromBytes, Immutable, KnownLayout, Clone, Copy)]
#[repr(C)]
pub struct ProcEvent {
    pub pid: u32,
    pub event: u8,
    pub _pad: [u8; 3],
}

const _: () = assert!(
    size_of::<ProcEvent>() == 8,
    "ProcEvent must match oya_proc_event"
);

/// Nanoseconds per clock tick — the divisor turning kernel ns into `/proc`-style jiffies.
/// Computed **once** from `CLK_TCK` (constant for the process) and stored by the source, so the
/// per-row conversion is a single division, not this plus the `1e9/CLK_TCK` division each time.
/// `CLK_TCK` is ~always 100 (⇒ 10 ms), where the division is exact, matching the kernel's
/// `nsec_to_clock_t`. A non-dividing `CLK_TCK` rounds slightly; immaterial to CPU% (already
/// ±1-jiffy quantized) and to start-time identity (a mismatch only makes `kill` refuse — safe).
///
/// If profiling ever shows the per-row `ns / ns_per_tick` division is hot (unlikely: ~2 per
/// process per cycle at a ~2 Hz interval), the divisor is a single value — swap in a precomputed
/// libdivide-style magic multiply+shift here without touching call sites.
pub fn ns_per_tick(clk_tck: u64) -> u64 {
    (1_000_000_000 / clk_tck.max(1)).max(1)
}

impl TaskInfo {
    /// Thread-group `utime + stime` in clock ticks (the CPU% delta input, same unit as the
    /// stat parser). The BPF program walks the thread list to match `/proc/<pid>/stat`.
    fn ticks(&self, ns_per_tick: u64) -> u64 {
        self.utime_ns.saturating_add(self.stime_ns) / ns_per_tick
    }

    /// Start time in clock ticks since boot — `/proc` stat field 22, the PID-reuse /
    /// kill-verification discriminator. Must equal what `sys::kill_verified` reads.
    fn start_time_ticks(&self, ns_per_tick: u64) -> u64 {
        self.start_boottime_ns / ns_per_tick
    }

    fn is_kthread(&self) -> bool {
        self.flags & PF_KTHREAD != 0
    }

    /// `priority` column = `task->prio − MAX_RT_PRIO` (100). Normal tasks read `20 + nice`.
    fn priority(&self) -> i8 {
        i8::try_from(i32::from(self.prio) - 100).unwrap_or(i8::MIN)
    }

    /// `nice` = `task->static_prio − DEFAULT_PRIO` (120), range −20…19.
    fn nice(&self) -> i8 {
        i8::try_from(i32::from(self.static_prio) - 120).unwrap_or(0)
    }

    /// `comm` trimmed at the NUL terminator (kernel writes a C string into 16 bytes).
    fn comm_bytes(&self) -> &[u8] {
        let end = self.comm.iter().position(|&b| b == 0).unwrap_or(COMM_LEN);
        &self.comm[..end]
    }

    /// Fill a process row's identity + volatile stat fields **and uid** (the BPF source owns
    /// uid, unlike the `/proc` path where `ProcTable` reads it). `cpu_pct`/`cpu_peak`, the
    /// cmdline handle, and tree links are set by later stages, exactly as for the stat path.
    /// `ns_per_tick` is the source's precomputed ns→tick divisor (see [`ns_per_tick`]).
    pub fn write_into(&self, e: &mut ProcessEntry, page_size: u64, ns_per_tick: u64) {
        e.pid = self.pid;
        e.ppid = self.ppid;
        e.uid = self.uid;
        e.state = state_char(self.state, self.exit_state);
        e.priority = self.priority();
        e.nice = self.nice();
        e.num_threads = self.nr_threads;
        e.ticks = self.ticks(ns_per_tick);
        e.start_time = self.start_time_ticks(ns_per_tick);
        let pages = u64::try_from(self.rss_pages).unwrap_or(0); // transient negatives → 0
        e.mem_bytes = pages.saturating_mul(page_size);
        e.is_kthread = self.is_kthread();
        let comm = self.comm_bytes();
        e.non_ascii = comm.iter().any(|&b| b >= 0x80);
        e.set_comm(comm);
    }
}

/// Map raw `task->__state` + `task->exit_state` to the single-char code `/proc` reports.
/// A faithful port of the kernel's `task_state_index` (fls over the reportable bits, with the
/// `IDLE` / `RTLOCK_WAIT` special cases). Cosmetic only — never affects identity or kill — so an
/// exotic unported combo degrading to `?` is harmless.
fn state_char(state: u32, exit_state: u32) -> u8 {
    const TASK_REPORT: u32 = 0x7f; // R|S|D|T|t|EXIT_DEAD|EXIT_ZOMBIE|P (RUNNING=0)
    const TASK_IDLE: u32 = 0x402; // TASK_UNINTERRUPTIBLE | TASK_NOLOAD
    const TASK_RTLOCK_WAIT: u32 = 0x100;
    const ARR: &[u8; 9] = b"RSDTtXZPI";

    let mut s = (state | exit_state) & TASK_REPORT;
    if state == TASK_IDLE {
        s = 0x80; // TASK_REPORT_IDLE → index 8
    } else if state == TASK_RTLOCK_WAIT {
        s = 0x02; // reported as uninterruptible sleep
    }
    let idx = if s == 0 {
        0
    } else {
        s.ilog2() as usize + 1 // fls(s)
    };
    ARR.get(idx).copied().unwrap_or(b'?')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_chars_match_proc() {
        assert_eq!(state_char(0x0000, 0), b'R'); // TASK_RUNNING
        assert_eq!(state_char(0x0001, 0), b'S'); // INTERRUPTIBLE
        assert_eq!(state_char(0x0002, 0), b'D'); // UNINTERRUPTIBLE
        assert_eq!(state_char(0x0004, 0), b'T'); // STOPPED
        assert_eq!(state_char(0x0008, 0), b't'); // TRACED
        assert_eq!(state_char(0x0000, 0x0010), b'X'); // EXIT_DEAD
        assert_eq!(state_char(0x0000, 0x0020), b'Z'); // EXIT_ZOMBIE
        assert_eq!(state_char(0x0040, 0), b'P'); // PARKED
        assert_eq!(state_char(0x0402, 0), b'I'); // TASK_IDLE
        assert_eq!(state_char(0x0100, 0), b'D'); // RTLOCK_WAIT → D
    }

    #[test]
    fn ns_to_ticks_is_exact_at_100hz() {
        let ti = TaskInfo {
            utime_ns: 70_000_000, // 7 ticks at 100 Hz
            stime_ns: 80_000_000, // 8 ticks
            start_boottime_ns: 1_200_106_100_000_000,
            rss_pages: 0,
            pid: 1,
            tgid: 1,
            ppid: 0,
            uid: 0,
            flags: 0,
            state: 0,
            exit_state: 0,
            nr_threads: 1,
            prio: 120,
            static_prio: 120,
            comm: [0; COMM_LEN],
            _pad: [0; 4],
        };
        let npt = ns_per_tick(100); // 10_000_000
        assert_eq!(ti.ticks(npt), 15);
        assert_eq!(ti.start_time_ticks(npt), 120_010_610);
        assert_eq!(ti.priority(), 20);
        assert_eq!(ti.nice(), 0);
    }

    #[test]
    fn comm_trims_at_nul() {
        let mut comm = [0u8; COMM_LEN];
        comm[..4].copy_from_slice(b"bash");
        let ti = TaskInfo {
            comm,
            ..zerocopy::FromZeros::new_zeroed()
        };
        assert_eq!(ti.comm_bytes(), b"bash");
    }
}
