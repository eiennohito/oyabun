use std::io::Write as _;
use std::time::{Duration, Instant};

pub(crate) use atop_stream::Stream;
use atop_stream::{ProcMetadata, RawProc};
use etch::{Display, Rgb};

use super::super::ui;
use super::source::{CycleResult, Source, SourceCtx};
use super::table::{CmdlineRead, ProcReader};
use crate::app::App;
use crate::fxhash::{FxMap, PidMap};
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
    metadata: PidMap<ProcMetadata>,
    sys: SystemStats,
}

impl ReplaySource {
    pub(crate) fn new(stream: Stream) -> Self {
        Self {
            stream,
            next: 0,
            base: Instant::now(),
            metadata: PidMap::default(),
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

        self.metadata.clear();
        for snapshot in &event.procs {
            self.metadata
                .insert(snapshot.raw.pid, snapshot.metadata.clone());
        }
        self.sys = convert_sys(event.sys);

        procs.clear();
        procs.reserve(event.procs.len());
        for snapshot in &event.procs {
            let mut e = ProcessEntry::TOMBSTONE;
            snapshot.raw.write_into(&mut e);
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
        let Some(bytes) = self
            .metadata
            .get(&pid)
            .and_then(|metadata| metadata.cmdline.as_deref())
        else {
            return CmdlineRead::UNKNOWN;
        };
        CmdlineRead {
            uid: u32::MAX,
            bytes,
            non_ascii: bytes.iter().any(|&b| b >= 0x80),
        }
    }

    fn cgroup(&mut self, pid: u32) -> &[u8] {
        self.metadata
            .get(&pid)
            .and_then(|metadata| metadata.cgroup.as_deref())
            .unwrap_or(&[])
    }

    fn flatpak_info(&mut self, pid: u32) -> &[u8] {
        self.metadata
            .get(&pid)
            .and_then(|metadata| metadata.flatpak.as_deref())
            .unwrap_or(&[])
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
        let mut uid_names = FxMap::default();
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
            .position(|row| self.app.row_pid(row) == pid)
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
            .map(|row| self.app.row_pid(row))
    }

    #[cfg(test)]
    pub(crate) fn display_pids(&self) -> Vec<u32> {
        self.app
            .rows()
            .iter()
            .map(|row| self.app.row_pid(row))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn row_collapsed(&self, pid: u32) -> bool {
        self.app
            .rows()
            .iter()
            .find(|row| self.app.row_pid(row) == pid)
            .map_or_else(|| panic!("pid {pid} not displayed"), |row| row.collapsed)
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
    use crate::app::DisplayRowKind;
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

    #[test]
    fn capture_fixture_projects_familiar_application_groups() {
        let mut r =
            Replayer::from_stream(stream(include_str!("../../tests/fixtures/app-groups.dsl")));
        r.cycle();

        let mut labels: Vec<_> = r
            .app
            .rows()
            .iter()
            .filter_map(|row| match row.kind {
                DisplayRowKind::Application { group_idx } => {
                    Some(r.app.app_group(group_idx).label.as_str())
                }
                DisplayRowKind::Process { .. } => None,
            })
            .collect();
        labels.sort_unstable();
        assert_eq!(
            labels,
            [
                "Google Chrome",
                "Slack",
                "Steam",
                "Visual Studio Code",
                "Zed"
            ]
        );
        assert_eq!(
            r.app
                .rows()
                .iter()
                .filter(|row| matches!(row.kind, DisplayRowKind::Application { .. }))
                .count(),
            5
        );
        for pid in [2278, 2305, 2308, 1730] {
            assert!(
                r.display_pids().contains(&pid),
                "pid {pid} must remain native"
            );
        }
        let collapsed_screen = r.render(100, 24);
        let collapsed_text = collapsed_screen.text();
        assert!(collapsed_text.contains('◇'));
        let chrome_text = collapsed_text
            .lines()
            .find(|line| line.contains("Google Chrome"))
            .unwrap();
        assert!(chrome_text.contains("[+4]"), "{chrome_text}");

        let chrome_row = r
            .app
            .rows()
            .iter()
            .position(|row| matches!(row.kind, DisplayRowKind::Application { group_idx } if r.app.app_group(group_idx).label == "Google Chrome"))
            .unwrap();
        r.app.select_row(chrome_row);
        assert!(
            !r.app.kill_selected(),
            "synthetic application rows cannot be signalled"
        );
        r.app.toggle_collapse();
        let chrome_members = [1684, 1690, 1718, 1721];
        let concrete: Vec<_> = r
            .app
            .rows()
            .iter()
            .filter_map(|row| match row.kind {
                DisplayRowKind::Process { proc_idx } => {
                    Some(r.app.procs().as_slice()[proc_idx].pid)
                }
                DisplayRowKind::Application { .. } => None,
            })
            .collect();
        for pid in chrome_members {
            assert_eq!(
                concrete.iter().filter(|&&shown| shown == pid).count(),
                1,
                "pid {pid}"
            );
        }

        let screen = r.render(100, 24);
        assert!(screen.text().contains('◆'));
        assert!(screen.text().contains("Google Chrome"));
    }

    #[test]
    fn fold_preference_persists_across_restart_by_identity() {
        use crate::application::FoldPreferences;

        let dir = std::env::temp_dir().join(format!("atop-foldpersist-{}", std::process::id()));
        let path = dir.join("folds");
        let _ = std::fs::remove_dir_all(&dir);

        let zed_row = |r: &Replayer| {
            r.app
                .rows()
                .iter()
                .position(|row| matches!(row.kind, DisplayRowKind::Application { group_idx } if r.app.app_group(group_idx).label == "Zed"))
                .expect("Zed application row")
        };

        // Session 1: Zed folds by default; expanding it writes an override keyed by its identity.
        let mut r =
            Replayer::from_stream(stream(include_str!("../../tests/fixtures/app-groups.dsl")));
        r.app.set_folds(FoldPreferences::at(path.clone()));
        r.cycle();
        assert!(r.app.rows()[zed_row(&r)].collapsed, "Zed folds by default");
        let row = zed_row(&r);
        r.app.select_row(row);
        r.app.toggle_collapse();
        assert!(!r.app.rows()[zed_row(&r)].collapsed);

        // Session 2: a fresh run loads the override and Zed starts expanded by identity.
        let mut r2 =
            Replayer::from_stream(stream(include_str!("../../tests/fixtures/app-groups.dsl")));
        r2.app.set_folds(FoldPreferences::at(path.clone()));
        r2.cycle();
        assert!(
            !r2.app.rows()[zed_row(&r2)].collapsed,
            "fold preference persisted"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn steady_cycle_does_not_move_the_metadata_epoch() {
        // Two identical cycles — the second carries the first's processes forward with no births,
        // deaths, or content changes — so the metadata epoch must hold steady, letting the
        // identity resolver and application grouping both skip recomputation.
        let stream = stream(
            r#"
            cycle 0
              10 uid=1000 start=10 comm=foo cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo@a.service\n"
              11 uid=1000 start=11 comm=foo cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo@a.service\n"
              12 uid=1000 start=12 comm=foo cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo@a.service\n"

            cycle +1s
            "#,
        );
        let mut r = Replayer::from_stream(stream);
        r.cycle();
        let epoch = r.app.meta_epoch();
        r.cycle();
        assert_eq!(
            r.app.meta_epoch(),
            epoch,
            "a steady cycle must not move the epoch"
        );
    }

    #[test]
    fn application_representative_and_expansion_follow_identity() {
        // Members split across `app-foo@a` and `app-foo@b` share the app id `foo`, so they are one
        // group whose row survives the specific representative process exiting.
        let stream = stream(
            r#"
            cycle 0
              10 uid=1000 start=10 comm=foo cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo@a.service\n"
              11 uid=1000 start=11 comm=foo cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo@a.service\n"
              12 uid=1000 start=12 comm=foo cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo@a.service\n"

            cycle +1s
              - 10
              + 13 uid=1000 start=13 comm=foo cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo@b.service\n"
            "#,
        );
        let mut r = Replayer::from_stream(stream);
        r.cycle();
        let row = r
            .app
            .rows()
            .iter()
            .position(|row| matches!(row.kind, DisplayRowKind::Application { .. }))
            .unwrap();
        r.app.select_row(row);
        r.app.toggle_collapse();

        r.cycle();
        let selected = &r.app.rows()[r.app.selected()];
        assert!(matches!(selected.kind, DisplayRowKind::Application { .. }));
        assert!(
            !selected.collapsed,
            "expansion suppression follows the app identity across cycles"
        );
        assert_eq!(
            r.app.row_pid(selected),
            11,
            "representative was replaced deterministically after 10 exited"
        );
    }

    #[test]
    fn application_memory_sampling_is_visible_and_gated() {
        let mut r =
            Replayer::from_stream(stream(include_str!("../../tests/fixtures/app-groups.dsl")));
        r.cycle();
        // No proportional reads until a folded application row is actually in the viewport.
        assert_eq!(r.app.memory_read_count(), 0);

        // Chrome is the folded group carrying a resident member in the fixture, so it is the one
        // that yields a proportional read once visible.
        let chrome = r
            .app
            .rows()
            .iter()
            .position(|row| matches!(row.kind, DisplayRowKind::Application { group_idx } if r.app.app_group(group_idx).label == "Google Chrome"))
            .unwrap();
        r.app.select_row(chrome);
        r.app.adjust_scroll(1);
        r.app.prepare_visible_rows(1);
        let after_first = r.app.memory_read_count();
        assert!(after_first > 0, "a visible folded row samples its members");

        // Same generation, same viewport: the resident-set gate reads nothing further.
        r.app.prepare_visible_rows(1);
        assert_eq!(r.app.memory_read_count(), after_first);
    }

    #[test]
    fn kthreadd_is_collapsed_by_default_and_can_be_expanded() {
        let mut r = Replayer::from_stream(stream(
            r"
            cycle 0
              2 uid=0 start=2 kthread=true comm=kthreadd
              3 ppid=2 uid=0 start=3 kthread=true comm=kworker/0:0
              4 ppid=2 uid=0 start=4 kthread=true comm=ksoftirqd/0
            ",
        ));
        r.cycle();
        assert_eq!(r.display_pids(), vec![2]);
        assert!(r.row_collapsed(2));

        r.select_pid(2);
        r.toggle_collapse();
        assert_eq!(r.display_pids(), vec![2, 3, 4]);
        assert!(!r.row_collapsed(2));
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
              - 10
              - 11
              - 12
              - 13
              - 14
              - 15

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

    fn chromium_group_stream() -> atop_stream::Stream {
        stream(
            r#"
            cycle 0
              10 start=100 comm=chrome cmd=/opt/app/chrome
              11 ppid=10 start=101 comm=chrome cmd=/opt/app/chrome
              12 ppid=10 start=102 comm=chrome cmd="/opt/app/chrome --type=renderer"
              13 ppid=10 start=103 comm=chrome cmd="/opt/app/chrome --type=gpu-process"
              14 ppid=10 start=104 comm=chrome cmd="/opt/app/chrome --type=utility"
              15 ppid=10 start=105 comm=chrome cmd="/opt/app/chrome --type=zygote"

            cycle +1s
            "#,
        )
    }

    fn cgroup_group_stream() -> atop_stream::Stream {
        stream(
            r#"
            cycle 0
              200 start=200 comm=daemon cmd=/usr/bin/daemon cgroup="0::/system.slice/example.service\n"
              201 ppid=200 start=201 comm=worker cmd=/usr/bin/worker cgroup="0::/system.slice/example.service\n"
              202 ppid=200 start=202 comm=worker cmd=/usr/bin/worker cgroup="0::/system.slice/example.service\n"

            cycle +1s
            "#,
        )
    }

    #[test]
    fn replay_structural_chromium_group_auto_folds_with_label() {
        // A Chromium/Electron process fan has no cgroup boundary — it is a structural,
        // session-only group. On a personal workstation it still folds by default, into one row
        // labelled by the root command, with the helper subtree hidden.
        let mut r = Replayer::from_stream(chromium_group_stream());
        r.cycle();
        assert_eq!(r.display_pids(), vec![10]);
        assert!(r.row_collapsed(10));
        let screen = r.render(120, 16);
        assert!(screen.text().contains("/opt/app/chrome"));
        assert!(
            !screen.text().contains("--type=renderer"),
            "members hidden while folded"
        );
    }

    #[test]
    fn replay_structural_chromium_group_expands_to_member_forest() {
        let mut r = Replayer::from_stream(chromium_group_stream());
        r.cycle();
        r.select_pid(10);
        r.toggle_collapse();
        // The group row plus its reconstructed member forest (root then helpers).
        assert_eq!(r.display_pids(), vec![10, 10, 11, 12, 13, 14, 15]);
        assert!(!r.row_collapsed(10));
    }

    #[test]
    fn replay_user_expansion_suppresses_auto_recollapse_while_group_lives() {
        let mut r = Replayer::from_stream(cgroup_group_stream());
        r.cycle();
        r.select_pid(200);
        r.toggle_collapse();
        assert_eq!(r.display_pids(), vec![200, 200, 201, 202]);

        r.cycle();
        assert_eq!(r.display_pids(), vec![200, 200, 201, 202]);
        assert!(
            !r.row_collapsed(200),
            "expansion follows the identity across cycles"
        );
    }

    #[test]
    fn replay_ephemeral_group_expansion_clears_on_disappearance() {
        // A container id is ephemeral, so its fold is session-only: an expansion is transient
        // state, cleared when the group disappears (unlike a persistable desktop/systemd identity).
        let stream = stream(
            r#"
            cycle 0
              200 start=100 comm=daemon cgroup="0::/system.slice/docker-abcdef1234567890.scope\n"
              201 ppid=200 start=101 comm=worker cgroup="0::/system.slice/docker-abcdef1234567890.scope\n"
              202 ppid=200 start=102 comm=worker cgroup="0::/system.slice/docker-abcdef1234567890.scope\n"

            cycle +1s
              - 200
              - 201
              - 202

            cycle +1s
              + 200 start=200 comm=daemon cgroup="0::/system.slice/docker-abcdef1234567890.scope\n"
              + 201 ppid=200 start=201 comm=worker cgroup="0::/system.slice/docker-abcdef1234567890.scope\n"
              + 202 ppid=200 start=202 comm=worker cgroup="0::/system.slice/docker-abcdef1234567890.scope\n"
            "#,
        );
        let mut r = Replayer::from_stream(stream);
        r.cycle();
        r.select_pid(200);
        r.toggle_collapse();
        assert_eq!(r.display_pids(), vec![200, 200, 201, 202]);

        r.cycle();
        assert!(r.display_pids().is_empty());

        r.cycle();
        assert_eq!(r.display_pids(), vec![200]);
        assert!(
            r.row_collapsed(200),
            "transient state cleared when the group disappeared"
        );
    }

    #[test]
    fn replay_collapsed_cgroup_group_label_renders() {
        let mut r = Replayer::from_stream(cgroup_group_stream());
        r.cycle();

        assert_eq!(r.display_pids(), vec![200]);
        assert!(r.row_collapsed(200));
        let screen = r.render(80, 16);
        assert!(screen.text().contains("example.service"));
        assert!(!screen.text().contains("/usr/bin/worker"));
    }

    #[test]
    fn replay_expanded_cgroup_group_reconstructs_members() {
        let mut r = Replayer::from_stream(cgroup_group_stream());
        r.cycle();
        r.select_pid(200);
        r.toggle_collapse();

        assert_eq!(r.display_pids(), vec![200, 200, 201, 202]);
        assert!(!r.row_collapsed(200));
        let screen = r.render(80, 16);
        assert!(screen.text().contains("/usr/bin/daemon"));
        assert!(screen.text().contains("/usr/bin/worker"));
    }

    #[test]
    fn replay_trusted_descendant_collapses_under_untrusted_parent() {
        let stream = stream(
            r#"
            cycle 0
              300 start=300 comm=chrome cmd=/opt/chrome cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-chrome-300.scope\n"
              301 ppid=300 start=301 comm=chrome cmd="/opt/chrome --type=zygote" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-chrome@abc.service\n"
              302 ppid=300 start=302 comm=chrome cmd="/opt/chrome --type=renderer" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-chrome@abc.service\n"
              303 ppid=300 start=303 comm=chrome cmd="/opt/chrome --type=gpu-process" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-chrome@abc.service\n"
              304 ppid=300 start=304 comm=chrome cmd="/opt/chrome --type=utility" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-chrome@abc.service\n"
              305 ppid=301 start=305 comm=chrome cmd="/opt/chrome --type=renderer" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-chrome@abc.service\n"
              306 ppid=301 start=306 comm=chrome cmd="/opt/chrome --type=renderer" cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-chrome@abc.service\n"

            cycle +1s
            "#,
        );
        let mut r = Replayer::from_stream(stream);
        r.cycle();
        assert_eq!(r.display_pids(), vec![300]);
        assert!(r.row_collapsed(300));

        r.select_pid(300);
        r.toggle_collapse();
        assert_eq!(
            r.display_pids(),
            vec![300, 300, 301, 305, 306, 302, 303, 304]
        );
        assert!(!r.row_collapsed(300));
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

    /// Ad-hoc perf probe over a real capture. Times `App::gather` (the full per-cycle grouping
    /// rebuild) and a cold full render, on real desktop cgroup data.
    /// `ATOP_CAPTURE=/path/to.dsl cargo test --release -p atop capture_grouping_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "timing; needs ATOP_CAPTURE"]
    #[allow(clippy::cast_precision_loss)]
    fn capture_grouping_cost() {
        use std::time::Instant;
        let path = std::env::var("ATOP_CAPTURE").expect("set ATOP_CAPTURE to a .dsl capture");
        let text = std::fs::read_to_string(&path).expect("read capture");
        let stream = atop_stream::Stream::parse(&text).expect("parse capture");
        let ncycles = stream.cycles.len();
        let nprocs = stream.cycles.first().map_or(0, |c| c.procs.len());
        let mut r = Replayer::from_stream(stream);

        let warm = 5.min(ncycles.saturating_sub(1));
        let (mut gather_us, mut render_us) = (Vec::new(), Vec::new());
        for i in 0..ncycles {
            let t = Instant::now();
            r.cycle();
            let g = t.elapsed().as_secs_f64() * 1e6;
            let t = Instant::now();
            let _ = r.render_text(200, 60);
            let rd = t.elapsed().as_secs_f64() * 1e6;
            if i >= warm {
                gather_us.push(g);
                render_us.push(rd);
            }
        }
        let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        let mx = |v: &[f64]| v.iter().copied().fold(0.0_f64, f64::max);
        eprintln!(
            "cycles={ncycles} procs={nprocs} | App::gather avg={:.0}us max={:.0}us | cold_render(200x60) avg={:.0}us | gather duty@500ms={:.2}% core",
            avg(&gather_us),
            mx(&gather_us),
            avg(&render_us),
            avg(&gather_us) / 1e6 / 0.5 * 100.0
        );
    }
}
