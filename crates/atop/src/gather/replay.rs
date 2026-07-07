use std::collections::HashMap;
use std::time::{Duration, Instant};

use etch::Display;

use super::super::ui;
use super::source::{CycleResult, Source, SourceCtx};
use super::table::{CmdlineRead, ProcReader};
use crate::app::App;
use crate::gather::Gatherer;
use crate::procs::{ProcessEntry, Procs, SystemStats};

#[derive(Clone)]
pub(crate) struct RawProc {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) uid: u32,
    pub(crate) state: u8,
    pub(crate) priority: i8,
    pub(crate) nice: i8,
    pub(crate) num_threads: u32,
    pub(crate) ticks: u64,
    pub(crate) mem_bytes: u64,
    pub(crate) start_time: u64,
    pub(crate) comm: Vec<u8>,
    pub(crate) is_kthread: bool,
}

impl RawProc {
    fn write_into(&self, e: &mut ProcessEntry) {
        *e = ProcessEntry::TOMBSTONE;
        e.pid = self.pid;
        e.ppid = self.ppid;
        e.uid = self.uid;
        e.state = self.state;
        e.priority = self.priority;
        e.nice = self.nice;
        e.num_threads = self.num_threads;
        e.ticks = self.ticks;
        e.mem_bytes = self.mem_bytes;
        e.start_time = self.start_time;
        e.is_kthread = self.is_kthread;
        e.non_ascii = self.comm.iter().any(|&b| b >= 0x80);
        e.set_comm(&self.comm);
    }
}

#[derive(Clone)]
pub(crate) struct CycleEvent {
    pub(crate) wall_ns: u64,
    pub(crate) sys: SystemStats,
    pub(crate) procs: Vec<RawProc>,
    pub(crate) cmdlines: Vec<(u32, Vec<u8>)>,
}

#[derive(Clone)]
pub(crate) struct Stream {
    pub(crate) cycles: Vec<CycleEvent>,
}

impl Stream {
    pub(crate) fn builder() -> StreamBuilder {
        StreamBuilder::new()
    }
}

#[derive(Default)]
pub(crate) struct StreamBuilder {
    cycles: Vec<CycleEvent>,
}

impl StreamBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn cycle(mut self, wall_ns: u64, f: impl FnOnce(&mut CycleBuilder)) -> Self {
        let mut c = CycleBuilder {
            event: CycleEvent {
                wall_ns,
                sys: default_sys(),
                procs: Vec::new(),
                cmdlines: Vec::new(),
            },
        };
        f(&mut c);
        c.event.procs.sort_unstable_by_key(|p| p.pid);
        self.cycles.push(c.event);
        self
    }

    pub(crate) fn build(self) -> Stream {
        Stream {
            cycles: self.cycles,
        }
    }
}

pub(crate) struct CycleBuilder {
    event: CycleEvent,
}

impl CycleBuilder {
    pub(crate) fn proc(&mut self, pid: u32, f: impl FnOnce(&mut ProcBuilder)) -> &mut Self {
        let mut p = ProcBuilder {
            raw: RawProc {
                pid,
                ppid: 0,
                uid: 1000,
                state: b'S',
                priority: 20,
                nice: 0,
                num_threads: 1,
                ticks: 0,
                mem_bytes: 0,
                start_time: u64::from(pid),
                comm: format!("p{pid}").into_bytes(),
                is_kthread: false,
            },
            cmdline: None,
        };
        f(&mut p);
        if let Some(cmdline) = p.cmdline {
            self.event.cmdlines.push((pid, cmdline));
        }
        self.event.procs.push(p.raw);
        self
    }
}

pub(crate) struct ProcBuilder {
    raw: RawProc,
    cmdline: Option<Vec<u8>>,
}

impl ProcBuilder {
    pub(crate) fn ppid(&mut self, ppid: u32) -> &mut Self {
        self.raw.ppid = ppid;
        self
    }

    pub(crate) fn uid(&mut self, uid: u32) -> &mut Self {
        self.raw.uid = uid;
        self
    }

    pub(crate) fn ticks(&mut self, ticks: u64) -> &mut Self {
        self.raw.ticks = ticks;
        self
    }

    pub(crate) fn mem_mb(&mut self, mem_mb: u64) -> &mut Self {
        self.raw.mem_bytes = mem_mb.saturating_mul(1024 * 1024);
        self
    }

    pub(crate) fn comm(&mut self, comm: &[u8]) -> &mut Self {
        self.raw.comm.clear();
        self.raw.comm.extend_from_slice(comm);
        self
    }

    pub(crate) fn cmdline(&mut self, cmdline: &[u8]) -> &mut Self {
        self.cmdline = Some(cmdline.to_vec());
        self
    }
}

pub(crate) struct ReplaySource {
    stream: Stream,
    next: usize,
    base: Instant,
    cmdlines: HashMap<u32, Vec<u8>>,
    sys: SystemStats,
}

impl ReplaySource {
    pub(crate) fn new(stream: Stream) -> Self {
        Self {
            stream,
            next: 0,
            base: Instant::now(),
            cmdlines: HashMap::new(),
            sys: default_sys(),
        }
    }

    pub(crate) fn sys(&self) -> SystemStats {
        self.sys
    }
}

impl Source for ReplaySource {
    fn populate(&mut self, procs: &mut Procs, _ctx: SourceCtx<'_>) -> CycleResult {
        let event = self
            .stream
            .cycles
            .get(self.next)
            .unwrap_or_else(|| panic!("replay exhausted at cycle {}", self.next));
        self.next += 1;

        self.cmdlines.clear();
        for (pid, cmdline) in &event.cmdlines {
            self.cmdlines.insert(*pid, cmdline.clone());
        }
        self.sys = event.sys;

        procs.clear();
        procs.reserve(event.procs.len());
        for raw in &event.procs {
            let mut e = ProcessEntry::TOMBSTONE;
            raw.write_into(&mut e);
            procs.push(e);
        }
        procs.sort_by_pid();

        CycleResult {
            now: self.base + Duration::from_nanos(event.wall_ns),
            pool_overflow: 0,
            short_lived: 0,
        }
    }
}

impl ProcReader for ReplaySource {
    fn cmdline_uid(&mut self, pid: u32) -> CmdlineRead<'_> {
        let Some(bytes) = self.cmdlines.get(&pid) else {
            return CmdlineRead::UNKNOWN;
        };
        CmdlineRead {
            uid: u32::MAX,
            bytes,
            non_ascii: bytes.iter().any(|&b| b >= 0x80),
        }
    }

    fn cap_eff(&mut self, _pid: u32) -> u64 {
        0
    }

    fn exe_deleted(&mut self, _pid: u32) -> bool {
        false
    }

    fn lib_deleted(&mut self, _pid: u32) -> bool {
        false
    }
}

fn default_sys() -> SystemStats {
    SystemStats {
        mem_total: 8 * 1024 * 1024 * 1024,
        num_cores: 4,
        ..SystemStats::default()
    }
}

pub(crate) struct Replayer {
    app: App,
}

impl Replayer {
    pub(crate) fn from_stream(stream: Stream) -> Self {
        let gatherer = Gatherer::replay(stream);
        let mut uid_names = HashMap::new();
        uid_names.insert(0, Box::<str>::from("root"));
        uid_names.insert(1000, Box::<str>::from("user"));
        Self {
            app: App::from_gatherer(gatherer, uid_names),
        }
    }

    pub(crate) fn cycle(&mut self) {
        self.app.gather();
    }

    pub(crate) fn proc(&self, pid: u32) -> &ProcessEntry {
        self.app
            .procs()
            .as_slice()
            .iter()
            .find(|p| p.pid == pid)
            .unwrap_or_else(|| panic!("pid {pid} not found"))
    }

    pub(crate) fn cmdline(&self, pid: u32) -> &[u8] {
        let p = self.proc(pid);
        self.app.cmdline(p)
    }

    pub(crate) fn render_text(&self, width: u16, height: u16) -> String {
        let mut display = Display::new(Vec::<u8>::new());
        let process_gpu = self.app.gpu_process_available();
        let schema = ui::columns(process_gpu);
        {
            let mut frame = display.begin_frame(width, height);
            ui::render(&mut frame, &self.app, &schema);
            frame.commit().expect("render replay frame");
        }
        let mut parser = vt100::Parser::new(height, width, 0);
        parser.process(display.get_ref());
        let screen = parser.screen();
        (0..height)
            .map(|row| {
                (0..width)
                    .map(|col| screen.cell(row, col).map_or("", vt100::Cell::contents))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::NONE;

    fn cpu_ramp_stream() -> Stream {
        Stream::builder()
            .cycle(0, |c| {
                c.proc(1, |p| {
                    p.uid(0).comm(b"init").cmdline(b"/sbin/init").mem_mb(12);
                });
                c.proc(42, |p| {
                    p.ppid(1)
                        .comm(b"bash")
                        .cmdline(b"/bin/bash")
                        .ticks(0)
                        .mem_mb(3);
                });
            })
            .cycle(1_000_000_000, |c| {
                c.proc(1, |p| {
                    p.uid(0)
                        .comm(b"init")
                        .cmdline(b"/sbin/init")
                        .ticks(100)
                        .mem_mb(12);
                });
                c.proc(42, |p| {
                    p.ppid(1)
                        .comm(b"bash")
                        .cmdline(b"/bin/bash")
                        .ticks(50)
                        .mem_mb(3);
                });
            })
            .build()
    }

    #[test]
    fn replay_drives_cpu_tree_and_cmdline_tail() {
        let mut r = Replayer::from_stream(cpu_ramp_stream());
        r.cycle();
        r.cycle();

        let bash = r.proc(42);
        assert!(bash.cpu_pct > 4_000, "bash cpu {} bp", bash.cpu_pct);
        assert_eq!(bash.display_state, b'R');
        assert_eq!(r.cmdline(42), b"/bin/bash");

        let init = r.proc(1);
        assert_eq!(bash.parent_idx, 0);
        assert_eq!(init.first_child, 1);
        assert_eq!(bash.first_child, NONE);
    }

    #[test]
    fn replay_render_produces_terminal_text() {
        let mut r = Replayer::from_stream(cpu_ramp_stream());
        r.cycle();
        r.cycle();

        let screen = r.render_text(80, 16);
        assert!(screen.contains("PID"));
        assert!(screen.contains("/bin/bash"));
        assert!(screen.contains("50.0"));
    }

    #[test]
    fn replay_birth_and_death_are_seen_by_common_tail() {
        let stream = Stream::builder()
            .cycle(0, |c| {
                c.proc(1, |p| {
                    p.uid(0).comm(b"init").cmdline(b"/sbin/init");
                });
            })
            .cycle(1_000_000_000, |c| {
                c.proc(1, |p| {
                    p.uid(0).comm(b"init").cmdline(b"/sbin/init");
                });
                c.proc(7, |p| {
                    p.ppid(1).comm(b"worker").cmdline(b"worker");
                });
            })
            .cycle(2_000_000_000, |c| {
                c.proc(1, |p| {
                    p.uid(0).comm(b"init").cmdline(b"/sbin/init");
                });
            })
            .build();
        let mut r = Replayer::from_stream(stream);
        r.cycle();
        assert_eq!(r.app.procs().as_slice().len(), 1);
        r.cycle();
        assert!(r.app.procs().as_slice().iter().any(|p| p.pid == 7));
        r.cycle();
        assert!(!r.app.procs().as_slice().iter().any(|p| p.pid == 7));
    }
}
