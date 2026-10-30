//! Actions on live processes, gated on process identity so a reused PID is never hit.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::gather::read_stat;
use crate::sys::ProcDir;

/// Nice of a deprioritized thread: the weakest normal priority.
const IDLE_NICE: i32 = 19;
/// `IOPRIO_WHO_PROCESS` — for `ioprio_set`, the target is a single thread id.
const IOPRIO_WHO_PROCESS: libc::c_int = 1;
/// `IOPRIO_PRIO_VALUE(IOPRIO_CLASS_IDLE, 0)`: the class sits above `IOPRIO_CLASS_SHIFT` (13).
const IOPRIO_IDLE: libc::c_int = 3 << 13;
/// Not exported by `libc` for every target; the uapi value.
const SCHED_DEADLINE: libc::c_int = 6;

/// A process pinned by a `pidfd` whose start time matched the one its row recorded: the very
/// incarnation the user selected, not a later process that reused the PID. The pidfd anchors
/// actions to the kernel task (a signal through it to a reaped process fails harmlessly); the
/// start time is what proves the PID still names the selected process at open.
pub struct VerifiedProcess {
    pid: u32,
    pidfd: OwnedFd,
}

impl VerifiedProcess {
    /// Pin `pid` if it is still the incarnation that started at `start_time`; `None` if it is
    /// gone or the PID now names another process.
    pub fn open(pid: u32, start_time: u64) -> Option<Self> {
        let pid_arg = libc::pid_t::try_from(pid).ok()?;
        // SAFETY: pidfd_open with a valid pid and zero flags.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid_arg, 0) };
        let fd = libc::c_int::try_from(raw).ok().filter(|&fd| fd >= 0)?;
        // SAFETY: a fresh fd we exclusively own.
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd) };
        // Checked *after* pinning: a match now means the pidfd holds the selected process.
        (read_stat(pid)?.start_time == start_time).then_some(Self { pid, pidfd })
    }

    /// Send `sig` through the pidfd. Returns whether it was delivered.
    pub fn signal(&self, sig: libc::c_int) -> bool {
        // SAFETY: valid pidfd; null siginfo and zero flags are accepted.
        let r = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                sig,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        r == 0
    }

    /// Not yet reaped — while true, the PID cannot have been reused.
    fn is_unreaped(&self) -> bool {
        self.signal(0)
    }

    /// Push every thread to the background: nice 19, the `SCHED_IDLE` policy, and the idle I/O
    /// class. All three are per-thread on Linux, so each thread is set (threads and children
    /// created later inherit them). Real-time threads are left alone — they were promoted on
    /// purpose (audio, typically), and demoting them causes the very glitches this prevents.
    /// `tids` is scratch. Returns whether any thread's priority was set.
    pub fn deprioritize(&self, tids: &mut Vec<u32>) -> bool {
        let Ok(tasks) = ProcDir::open_tasks(self.pid) else {
            return false;
        };
        let mut dent = [0u8; 4096];
        tasks.read_pids(&mut dent, tids);
        // Opened after the pin; still unreaped after the listing ⇒ the PID was never reused in
        // between, so every listed TID is one of this process's threads.
        if !self.is_unreaped() {
            return false;
        }
        let mut changed = false;
        for &tid in tids.iter() {
            changed |= deprioritize_thread(tid);
        }
        changed
    }
}

/// One thread: see [`VerifiedProcess::deprioritize`]. A TID could in principle be reused between
/// listing and these calls; that window is microseconds wide and these calls can only *lower* a
/// priority, so the worst case is a stray deprioritization — unlike a stray signal.
fn deprioritize_thread(tid: u32) -> bool {
    let Ok(tid_arg) = libc::pid_t::try_from(tid) else {
        return false;
    };
    // SAFETY: plain syscall on a thread id.
    let policy = unsafe { libc::sched_getscheduler(tid_arg) };
    if policy < 0 {
        return false;
    }
    if matches!(
        policy & !libc::SCHED_RESET_ON_FORK,
        libc::SCHED_FIFO | libc::SCHED_RR | SCHED_DEADLINE
    ) {
        return false;
    }
    // Nice first: switching to `SCHED_IDLE` keeps the thread's nice, so a later switch back to
    // the normal policy still lands at 19.
    // SAFETY: plain syscalls on a thread id; `param` outlives the call.
    unsafe {
        let niced = libc::setpriority(libc::PRIO_PROCESS, tid, IDLE_NICE) == 0;
        let param = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(tid_arg, libc::SCHED_IDLE, &raw const param);
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            tid_arg,
            IOPRIO_IDLE,
        );
        niced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_sleep() -> (std::process::Child, u32, u64) {
        let child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let start = read_stat(pid).expect("child stat").start_time;
        (child, pid, start)
    }

    #[test]
    fn open_respects_identity() {
        let (mut child, pid, start) = spawn_sleep();
        // Mismatched start time (as if the PID had been reused) → refuse to pin.
        assert!(
            VerifiedProcess::open(pid, start.wrapping_add(1)).is_none(),
            "must not pin a different incarnation"
        );
        let p = VerifiedProcess::open(pid, start).expect("same incarnation pins");
        assert!(p.signal(libc::SIGTERM));
        let status = child.wait().expect("reap child");
        assert!(
            !status.success(),
            "child should have been terminated by signal"
        );
    }

    #[test]
    fn deprioritize_sets_nice_policy_and_io_class() {
        let (mut child, pid, start) = spawn_sleep();
        let p = VerifiedProcess::open(pid, start).expect("pin child");
        let mut tids = Vec::new();
        assert!(p.deprioritize(&mut tids), "own child is deprioritizable");
        assert_eq!(tids, [pid], "sleep is single-threaded");

        let tid = libc::pid_t::try_from(pid).unwrap();
        assert_eq!(read_stat(pid).expect("child stat").nice, 19);
        // SAFETY: plain syscalls on our child's thread id.
        let (policy, ioprio) = unsafe {
            (
                libc::sched_getscheduler(tid),
                libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, tid),
            )
        };
        assert_eq!(policy & !libc::SCHED_RESET_ON_FORK, libc::SCHED_IDLE);
        assert_eq!(ioprio, libc::c_long::from(IOPRIO_IDLE));

        child.kill().ok();
        child.wait().ok();
    }
}
