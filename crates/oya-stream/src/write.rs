use std::fmt::Write as _;

use crate::{RawProc, Stream, SystemStats};

pub(crate) fn verbose_stream(stream: &Stream) -> String {
    let mut out = String::new();
    let mut measured: std::collections::HashMap<u32, bool> = std::collections::HashMap::new();
    for (idx, cycle) in stream.cycles.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        cycle.sys.write_header(&mut out, cycle.wall_ns);
        for snapshot in &cycle.procs {
            let proc = &snapshot.raw;
            // The parser carries values across cycles, so a wait that stopped being measured
            // must be written; one that never was needs no mention.
            let was_measured = measured.insert(proc.pid, proc.wait.is_some()) == Some(true);
            proc.write_full(&mut out, was_measured);
            if let Some(cmdline) = &snapshot.metadata.cmdline {
                out.push_str(" cmd=");
                write_value(&mut out, cmdline);
            }
            if let Some(cgroup) = &snapshot.metadata.cgroup {
                out.push_str(" cgroup=");
                write_value(&mut out, cgroup);
            }
            if let Some(flatpak) = &snapshot.metadata.flatpak {
                out.push_str(" flatpak=");
                write_value(&mut out, flatpak);
            }
            out.push('\n');
        }
    }
    out
}

impl SystemStats {
    pub(crate) fn write_header(self, out: &mut String, wall_ns: u64) {
        let _ = write!(
            out,
            "cycle {}ns cores={} \
             stat_user={} stat_nice={} stat_system={} stat_idle={} \
             stat_iowait={} stat_irq={} stat_softirq={} stat_steal={} \
             runnable={} mem={} mem_available={} mem_free={} mem_shmem={} \
             swap_total={} swap_free={}",
            wall_ns,
            self.num_cores,
            self.stat_user,
            self.stat_nice,
            self.stat_system,
            self.stat_idle,
            self.stat_iowait,
            self.stat_irq,
            self.stat_softirq,
            self.stat_steal,
            self.runnable,
            self.mem_total,
            self.mem_available,
            self.mem_free,
            self.mem_shmem,
            self.swap_total,
            self.swap_free,
        );
        // The parser carries values across cycles, so absence is written explicitly: omitting
        // a key would replay as "unchanged".
        for (key, value) in [
            ("psi_cpu", self.psi_cpu),
            ("psi_memory", self.psi_memory),
            ("psi_io", self.psi_io),
            ("pswpin", self.pswpin),
        ] {
            match value {
                Some(v) => {
                    let _ = write!(out, " {key}={v}");
                }
                None => {
                    let _ = write!(out, " {key}=none");
                }
            }
        }
        out.push('\n');
    }
}

impl RawProc {
    pub(crate) fn write_full(&self, out: &mut String, was_wait_measured: bool) {
        let _ = write!(
            out,
            "  {} ppid={} uid={} state={} priority={} nice={} threads={} ticks={} mem={} \
             start={} kthread={}",
            self.pid,
            self.ppid,
            self.uid,
            self.state.as_char(),
            self.priority,
            self.nice,
            self.num_threads,
            self.ticks,
            self.mem_bytes,
            self.start_time,
            self.is_kthread,
        );
        match self.wait {
            Some(w) => {
                let _ = write!(out, " wait={w}");
            }
            None if was_wait_measured => out.push_str(" wait=none"),
            None => {}
        }
        out.push_str(" comm=");
        write_value(out, &self.comm);
    }
}

pub(crate) fn write_value(out: &mut String, bytes: &[u8]) {
    if !bytes.is_empty()
        && bytes
            .iter()
            .all(|&b| matches!(b, b'!'..=b'~') && !matches!(b, b'"' | b'\'' | b'#' | b'\\'))
    {
        out.push_str(&String::from_utf8_lossy(bytes));
        return;
    }
    out.push('"');
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(char::from(b)),
            _ => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out.push('"');
}
