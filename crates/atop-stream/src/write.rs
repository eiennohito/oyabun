use std::fmt::Write as _;

use crate::{RawProc, Stream, SystemStats};

pub(crate) fn verbose_stream(stream: &Stream) -> String {
    let mut out = String::new();
    for (idx, cycle) in stream.cycles.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        cycle.sys.write_header(&mut out, cycle.wall_ns);
        for snapshot in &cycle.procs {
            let proc = &snapshot.raw;
            proc.write_full(&mut out);
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
        let _ = writeln!(
            out,
            "cycle {}ns cores={} \
             stat_user={} stat_nice={} stat_system={} stat_idle={} \
             stat_iowait={} stat_irq={} stat_softirq={} stat_steal={} \
             mem={} mem_available={} mem_buffers={} mem_cached={} \
             swap_total={} swap_free={} \
             load={},{},{} uptime={}",
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
            self.mem_total,
            self.mem_available,
            self.mem_buffers,
            self.mem_cached,
            self.swap_total,
            self.swap_free,
            self.load[0],
            self.load[1],
            self.load[2],
            self.uptime_secs,
        );
    }
}

impl RawProc {
    pub(crate) fn write_full(&self, out: &mut String) {
        let _ = write!(
            out,
            "  {} ppid={} uid={} state={} priority={} nice={} threads={} ticks={} mem={} \
             start={} kthread={} comm=",
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
