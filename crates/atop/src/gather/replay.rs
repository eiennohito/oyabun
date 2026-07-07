use std::collections::HashMap;
use std::io::Write as _;
use std::time::{Duration, Instant};

use atop_stream::RawProc;
pub(crate) use atop_stream::Stream;
use etch::{Display, Rgb};

use super::super::ui;
use super::source::{CycleResult, Source, SourceCtx};
use super::table::{CmdlineRead, ProcReader};
use crate::app::App;
use crate::gather::Gatherer;
use crate::procs::{ProcessEntry, Procs, SystemStats};

trait RawProcExt {
    fn write_into(&self, e: &mut ProcessEntry);
}

impl RawProcExt for RawProc {
    fn write_into(&self, e: &mut ProcessEntry) {
        *e = ProcessEntry::TOMBSTONE;
        e.pid = self.pid;
        e.ppid = self.ppid;
        e.uid = self.uid;
        e.state = self.state.as_u8();
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

fn convert_sys(sys: atop_stream::SystemStats) -> SystemStats {
    SystemStats {
        cpu_user_bp: sys.cpu_user_bp,
        cpu_sys_bp: sys.cpu_sys_bp,
        cpu_iowait_bp: sys.cpu_iowait_bp,
        mem_total: sys.mem_total,
        mem_used: sys.mem_used,
        mem_cached: sys.mem_cached,
        swap_total: sys.swap_total,
        swap_used: sys.swap_used,
        load: sys.load,
        uptime_secs: sys.uptime_secs,
        num_cores: sys.num_cores,
        ..SystemStats::default()
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
        self.sys = convert_sys(event.sys);

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

    // The replay DSL does not carry capability/deleted-file inputs yet, so these remain
    // unremarkable until the stream format grows caps/exe_del/lib_del fields.
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
        Self::from_gatherer(gatherer)
    }

    #[cfg(test)]
    pub(crate) fn from_stream_with_refresh_n(stream: Stream, refresh_n: u32) -> Self {
        let gatherer = Gatherer::replay_with_refresh_n(stream, refresh_n);
        Self::from_gatherer(gatherer)
    }

    fn from_gatherer(gatherer: Gatherer) -> Self {
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

    #[cfg(test)]
    pub(crate) fn proc_idx(&self, pid: u32) -> u32 {
        self.app
            .procs()
            .as_slice()
            .iter()
            .position(|p| p.pid == pid)
            .map_or_else(
                || panic!("pid {pid} not found"),
                |idx| u32::try_from(idx).expect("process index fits u32"),
            )
    }

    pub(crate) fn cmdline(&self, pid: u32) -> &[u8] {
        let p = self.proc(pid);
        self.app.cmdline(p)
    }

    #[cfg(test)]
    pub(crate) fn select_pid(&mut self, pid: u32) {
        let row = self
            .app
            .rows()
            .iter()
            .position(|row| self.app.procs().as_slice()[row.proc_idx].pid == pid)
            .unwrap_or_else(|| panic!("pid {pid} not displayed"));
        self.app.select_row(row);
    }

    #[cfg(test)]
    pub(crate) fn toggle_collapse(&mut self) {
        self.app.toggle_collapse();
    }

    #[cfg(test)]
    pub(crate) fn move_down(&mut self) {
        self.app.move_down();
    }

    #[cfg(test)]
    pub(crate) fn selected_pid(&self) -> Option<u32> {
        self.app
            .rows()
            .get(self.app.selected())
            .map(|row| self.app.procs().as_slice()[row.proc_idx].pid)
    }

    #[cfg(test)]
    pub(crate) fn display_pids(&self) -> Vec<u32> {
        self.app
            .rows()
            .iter()
            .map(|row| self.app.procs().as_slice()[row.proc_idx].pid)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn render(&self, width: u16, height: u16) -> Rendered {
        let bytes = self.render_bytes(width, height);
        let mut parser = vt100::Parser::new(height.saturating_add(1), width, 0);
        parser.process(&vt100_cursor_rows(&bytes));
        Rendered {
            width,
            height,
            parser,
        }
    }

    pub(crate) fn render_text(&self, width: u16, height: u16) -> String {
        self.render(width, height).text()
    }

    fn render_bytes(&self, width: u16, height: u16) -> Vec<u8> {
        let mut display = Display::new(Vec::<u8>::new());
        let process_gpu = self.app.gpu_process_available();
        let schema = ui::columns(process_gpu);
        {
            let mut frame = display.begin_frame(width, height);
            ui::render(&mut frame, &self.app, &schema);
            frame.commit().expect("render replay frame");
        }
        display.get_ref().clone()
    }
}

#[cfg(test)]
pub(crate) struct Rendered {
    width: u16,
    height: u16,
    parser: vt100::Parser,
}

#[cfg(test)]
impl Rendered {
    pub(crate) fn text(&self) -> String {
        (0..self.height)
            .map(|row| self.row(row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn row(&self, row: u16) -> String {
        (0..self.width)
            .map(|col| self.cell(row, col).map_or("", vt100::Cell::contents))
            .collect()
    }

    pub(crate) fn cell(&self, row: u16, col: u16) -> Option<&vt100::Cell> {
        self.parser.screen().cell(row, col)
    }

    pub(crate) fn fg_at_text(&self, row: u16, text: &str) -> vt100::Color {
        self.cell_at_text(row, text).fgcolor()
    }

    pub(crate) fn bg_at_text(&self, row: u16, text: &str) -> vt100::Color {
        self.cell_at_text(row, text).bgcolor()
    }

    fn cell_at_text(&self, row: u16, text: &str) -> &vt100::Cell {
        let row_text = self.row(row);
        let start = row_text
            .find(text)
            .unwrap_or_else(|| panic!("text {text:?} not found in row {row}: {row_text:?}"));
        let col = u16::try_from(start).expect("column fits u16");
        self.cell(row, col)
            .unwrap_or_else(|| panic!("cell at row {row}, col {col} missing"))
    }
}

#[cfg(test)]
fn vt_color(rgb: Rgb) -> vt100::Color {
    vt100::Color::Rgb(rgb.0, rgb.1, rgb.2)
}

#[cfg(test)]
fn vt100_cursor_rows(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes.get(i..i + 2) == Some(b"\x1b[") {
            let row_start = i + 2;
            let mut semi = row_start;
            while semi < bytes.len() && bytes[semi].is_ascii_digit() {
                semi += 1;
            }
            if semi > row_start && bytes.get(semi) == Some(&b';') {
                let col_start = semi + 1;
                let mut end = col_start;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end > col_start && bytes.get(end) == Some(&b'H') {
                    let row = std::str::from_utf8(&bytes[row_start..semi])
                        .expect("cursor row is ascii")
                        .parse::<u16>()
                        .expect("cursor row is numeric");
                    out.extend_from_slice(b"\x1b[");
                    let _ = write!(out, "{}", row.saturating_add(1));
                    out.push(b';');
                    out.extend_from_slice(&bytes[col_start..=end]);
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette;
    use crate::procs::NONE;

    const BODY_TOP: u16 = 4;

    fn row_containing(screen: &Rendered, text: &str) -> u16 {
        (0..screen.height)
            .find(|&row| screen.row(row).contains(text))
            .unwrap_or_else(|| panic!("text {text:?} not found in rendered screen"))
    }

    fn stream(input: &str) -> atop_stream::Stream {
        atop_stream::Stream::parse(input).expect("parse replay stream")
    }

    fn cpu_ramp_stream() -> atop_stream::Stream {
        stream(
            r"
            cycle 0
              1 uid=0 comm=init cmd=/sbin/init mem=12M
              42 ppid=1 comm=bash cmd=/bin/bash ticks=0 mem=3M

            cycle +1s
              1 ticks=100
              42 ticks=+50
            ",
        )
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
        let stream = stream(
            r"
            cycle 0
              1 uid=0 comm=init cmd=/sbin/init

            cycle +1s
              + 7 ppid=1 comm=worker cmd=worker

            cycle +1s
              - 7
            ",
        );
        let mut r = Replayer::from_stream(stream);
        r.cycle();
        assert_eq!(r.app.procs().as_slice().len(), 1);
        r.cycle();
        assert!(r.app.procs().as_slice().iter().any(|p| p.pid == 7));
        r.cycle();
        assert!(!r.app.procs().as_slice().iter().any(|p| p.pid == 7));
    }

    #[test]
    fn replay_tree_reshuffles_on_reparenting() {
        let stream = stream(
            r"
            cycle 0
              1 uid=0 comm=init cmd=/sbin/init
              2 comm=supervisor cmd=/usr/bin/supervisor
              42 ppid=1 start=4242 comm=worker cmd=/usr/bin/worker

            cycle +1s
              42 ppid=2
            ",
        );

        let mut r = Replayer::from_stream(stream);
        r.cycle();
        assert_eq!(r.proc(42).parent_idx, r.proc_idx(1));
        assert_eq!(r.proc(1).first_child, r.proc_idx(42));
        assert_eq!(r.proc(42).depth, 1);
        assert_eq!(r.display_pids(), vec![1, 42, 2]);

        r.cycle();
        assert_eq!(r.proc(42).parent_idx, r.proc_idx(2));
        assert_eq!(r.proc(1).first_child, NONE);
        assert_eq!(r.proc(2).first_child, r.proc_idx(42));
        assert_eq!(r.proc(42).depth, 1);
        assert_eq!(r.display_pids(), vec![1, 2, 42]);
    }

    #[test]
    fn replay_cmdline_settles_then_waits_for_refresh_cadence() {
        let stream = stream(
            r"
            cycle 0
              42 comm=cmd cmd=alpha

            cycle +1s
              42 cmd=beta

            cycle +1s
              42 cmd=gamma

            cycle +1s
              42 cmd=delta-ignored

            cycle +1s

            cycle +1s
              42 cmd=epsilon
            ",
        );

        let mut r = Replayer::from_stream_with_refresh_n(stream, 16);
        r.cycle();
        assert_eq!(r.cmdline(42), b"alpha");
        r.cycle();
        assert_eq!(r.cmdline(42), b"beta");
        r.cycle();
        assert_eq!(r.cmdline(42), b"gamma");
        r.cycle();
        assert_eq!(r.cmdline(42), b"gamma");
        r.cycle();
        assert_eq!(r.cmdline(42), b"gamma");
        r.cycle();
        assert_eq!(r.cmdline(42), b"epsilon");
    }

    #[test]
    fn replay_collapsed_subtree_uses_aggregate_cpu_and_memory() {
        let stream = stream(
            r"
            cycle 0
              10 comm=parent cmd=/srv/parent ticks=0 mem=1M
              11 ppid=10 comm=child-a cmd=/srv/child-a ticks=0 mem=2M
              12 ppid=10 comm=child-b cmd=/srv/child-b ticks=0 mem=3M

            cycle +1s
              10 ticks=10
              11 ticks=20
              12 ticks=30
            ",
        );

        let mut r = Replayer::from_stream(stream);
        r.cycle();
        r.cycle();

        assert_eq!(r.proc(10).subtree_cpu, 6_000);
        assert_eq!(r.proc(10).subtree_mem, 6 * 1024 * 1024);

        r.select_pid(10);
        r.toggle_collapse();
        assert_eq!(r.display_pids(), vec![10]);

        let screen = r.render(80, 16);
        let parent_row = row_containing(&screen, "/srv/parent");
        let text = screen.row(parent_row);
        assert!(text.contains("[+2]"), "{text}");
        assert!(text.contains("60.00%"), "{text}");
        assert!(text.contains("6.0M"), "{text}");
    }

    #[test]
    fn replay_full_frame_layout_at_80_by_24() {
        let stream = stream(
            r"
            cycle 0 cpu_user=1000 cpu_sys=500 mem_used=2G mem_cached=512M swap_total=4G load=125,75,50 uptime=3600
              1 uid=0 comm=init cmd=/sbin/init
              42 ppid=1 comm=hot cmd=/usr/bin/hot
              100 comm=batch cmd=/opt/batch

            cycle +1s cpu_user=2000 cpu_sys=1000 mem_used=3G swap_used=128M load=150,100,80 uptime=3660
              1 ticks=1
              42 ticks=50 mem=64M
              100 ticks=5 mem=12M
            ",
        );

        let mut r = Replayer::from_stream(stream);
        r.cycle();
        r.cycle();

        let screen = r.render(80, 24);
        assert!(screen.row(0).contains("CPU["));
        assert!(screen.row(1).contains("Mem["));
        assert!(screen.row(2).contains("Load:"));
        assert!(screen.row(3).contains("PID"));
        assert!(screen.row(BODY_TOP).contains("/sbin/init"));
        assert!(screen.text().contains("/usr/bin/hot"));
        assert!(screen.text().contains("/opt/batch"));
        assert!(screen.row(23).contains("Rows: 3"));
        for row in 0..24 {
            assert!(
                screen.row(row).chars().count() <= 80,
                "row {row} exceeded 80 columns: {:?}",
                screen.row(row)
            );
        }
    }

    #[test]
    fn replay_render_colors_match_palette_rules() {
        let stream = stream(
            r"
            cycle 0
              1 uid=0 comm=init cmd=/sbin/init
              42 ppid=1 comm=hot cmd=/usr/bin/hot ticks=0

            cycle +1s
              42 S ticks=50
            ",
        );

        let mut r = Replayer::from_stream(stream);
        r.cycle();
        r.cycle();
        r.select_pid(42);
        assert_eq!(r.selected_pid(), Some(42));
        let expected_cpu = r.proc(42).cpu_pct;

        let screen = r.render(80, 16);
        let hot_row = row_containing(&screen, "/usr/bin/hot");
        assert_eq!(
            screen.fg_at_text(hot_row, "50.00%"),
            vt_color(palette::cpu(expected_cpu))
        );
        assert_eq!(
            screen.bg_at_text(hot_row, "/usr/bin/hot"),
            vt_color(palette::SELECTION_BG)
        );
        assert_eq!(
            screen.fg_at_text(hot_row, "R"),
            vt_color(palette::state(b'R'))
        );

        r.move_down();
        assert_eq!(r.selected_pid(), Some(42));
    }
}
