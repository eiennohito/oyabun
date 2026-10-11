use std::io::Write as _;
use std::time::{Duration, Instant};

use etch::{Display, Rgb};
pub(crate) use oya_stream::Stream;
use oya_stream::{ProcMetadata, RawProc};

use super::super::ui;
use super::source::{CycleResult, Source, SourceCtx};
use super::table::{CmdlineRead, ProcReader};
use crate::app::App;
use crate::fxhash::{FxMap, PidMap};
use crate::gather::Gatherer;
use crate::procs::{ProcessEntry, Procs};
use crate::sys::RawSystemSnapshot;
use crate::view::RowKind;

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
        e.wait_ticks = self.wait;
        e.mem_bytes = self.mem_bytes;
        e.start_time = self.start_time;
        e.is_kthread = self.is_kthread;
        e.non_ascii = self.comm.iter().any(|&b| b >= 0x80);
        e.set_comm(&self.comm);
    }
}

/// Convert stream-format raw fields into the internal snapshot that `SystemSampler` consumes.
fn convert_sys(sys: oya_stream::SystemStats) -> RawSystemSnapshot {
    RawSystemSnapshot::from(sys)
}

pub(crate) struct ReplaySource {
    stream: Stream,
    next: usize,
    base: Instant,
    metadata: PidMap<ProcMetadata>,
    raw_snap: RawSystemSnapshot,
    num_cores: u32,
}

impl ReplaySource {
    pub(crate) fn new(stream: Stream) -> Self {
        let default = oya_stream::default_sys();
        Self {
            stream,
            next: 0,
            base: Instant::now(),
            metadata: PidMap::default(),
            raw_snap: convert_sys(default),
            num_cores: default.num_cores,
        }
    }

    pub(crate) fn raw_snapshot(&self) -> RawSystemSnapshot {
        self.raw_snap
    }

    pub(crate) fn num_cores(&self) -> u32 {
        self.num_cores
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
        self.raw_snap = convert_sys(event.sys);
        self.num_cores = event.sys.num_cores;

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

    #[cfg(test)]
    pub(crate) fn set_header_width(&mut self, width: u16) {
        self.app.set_header_width(width);
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

    /// `(pid, depth)` of every row, in display order.
    #[cfg(test)]
    pub(crate) fn display_depths(&self) -> Vec<(u32, u16)> {
        self.app
            .rows()
            .iter()
            .map(|row| (self.app.row_pid(row), row.depth))
            .collect()
    }

    /// The display row index of the first row shown through `pid`.
    #[cfg(test)]
    pub(crate) fn row_of(&self, pid: u32) -> usize {
        self.app
            .rows()
            .iter()
            .position(|row| self.app.row_pid(row) == pid)
            .unwrap_or_else(|| panic!("pid {pid} not displayed"))
    }

    #[cfg(test)]
    pub(crate) fn totals(&self, pid: u32) -> crate::view::Totals {
        self.app.row_totals(self.row_of(pid))
    }

    #[cfg(test)]
    pub(crate) fn row_series(&self, pid: u32) -> Option<crate::gather::CpuSeries> {
        self.app.row_series(self.row_of(pid))
    }

    #[cfg(test)]
    pub(crate) fn kinds(&self) -> Vec<RowKind> {
        self.app.rows().iter().map(|row| row.kind).collect()
    }

    #[cfg(test)]
    pub(crate) fn collapse_selected(&mut self) {
        self.app.collapse_selected();
    }

    #[cfg(test)]
    pub(crate) fn toggle_group_fold(&mut self) {
        self.app.toggle_group_fold();
    }

    #[cfg(test)]
    pub(crate) fn layout_count(&self) -> u64 {
        self.app.layout_count()
    }

    #[cfg(test)]
    pub(crate) fn row_collapsed(&self, pid: u32) -> bool {
        self.app
            .rows()
            .iter()
            .find(|row| self.app.row_pid(row) == pid)
            .map_or_else(
                || panic!("pid {pid} not displayed"),
                crate::view::ViewRow::is_collapsed,
            )
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

    /// The rightmost block glyph on `row` — the newest cell of a row's sparkline.
    pub(crate) fn newest_spark_cell(&self, row: u16) -> &vt100::Cell {
        (0..self.width)
            .filter_map(|col| self.cell(row, col))
            .rfind(|c| {
                c.contents()
                    .chars()
                    .any(|ch| ('\u{2581}'..='\u{2588}').contains(&ch))
            })
            .unwrap_or_else(|| panic!("no sparkline glyph in row {row}: {:?}", self.row(row)))
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

    const BODY_TOP: u16 = 3;

    fn row_containing(screen: &Rendered, text: &str) -> u16 {
        (0..screen.height)
            .find(|&row| screen.row(row).contains(text))
            .unwrap_or_else(|| panic!("text {text:?} not found in rendered screen"))
    }

    fn stream(input: &str) -> oya_stream::Stream {
        oya_stream::Stream::parse(input).expect("parse replay stream")
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
                RowKind::Group { group_idx } => Some(r.app.app_group(group_idx).label.as_str()),
                RowKind::Process { .. } => None,
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
                .filter(|row| matches!(row.kind, RowKind::Group { .. }))
                .count(),
            5
        );
        for pid in [2278, 2305, 2308] {
            assert!(
                r.display_pids().contains(&pid),
                "pid {pid} must remain native"
            );
        }
        // 1730 belongs to another application but is a native child of a Chrome member: it is
        // hidden inside the Chrome fold (and counted there), not left dangling beside it.
        assert!(!r.display_pids().contains(&1730));
        let collapsed_screen = r.render(100, 24);
        let collapsed_text = collapsed_screen.text();
        assert!(collapsed_text.contains('◇'));
        let chrome_text = collapsed_text
            .lines()
            .find(|line| line.contains("Google Chrome"))
            .unwrap();
        assert!(chrome_text.contains("[+5]"), "{chrome_text}");

        let chrome_row = r
            .app
            .rows()
            .iter()
            .position(|row| matches!(row.kind, RowKind::Group { group_idx } if r.app.app_group(group_idx).label == "Google Chrome"))
            .unwrap();
        r.app.select_row(chrome_row);
        assert!(
            !r.app.kill_selected(),
            "synthetic application rows cannot be signalled"
        );
        r.app.toggle_collapse();
        let chrome_members = [1684, 1690, 1718, 1721, 1730];
        let concrete: Vec<_> = r
            .app
            .rows()
            .iter()
            .filter_map(|row| match row.kind {
                RowKind::Process { proc_idx } => Some(r.app.procs().as_slice()[proc_idx].pid),
                RowKind::Group { .. } => None,
            })
            .collect();
        for pid in chrome_members {
            assert_eq!(
                concrete.iter().filter(|&&shown| shown == pid).count(),
                1,
                "pid {pid}"
            );
        }

        // Expanded: no group row at all — the members are ordinary process rows.
        assert!(
            r.app
                .rows()
                .iter()
                .all(|row| !matches!(row.kind, RowKind::Group { group_idx } if r.app.app_group(group_idx).label == "Google Chrome")),
            "an expanded group has no row of its own"
        );
        assert!(!r.render(100, 24).text().contains("◇ Google Chrome"));
        // The foreign child is back at its native spot, one level under its parent.
        let depths = r.display_depths();
        let depth_of = |pid| depths.iter().find(|&&(p, _)| p == pid).unwrap().1;
        assert_eq!(depth_of(1730), depth_of(1684) + 1);
    }

    #[test]
    fn fold_preference_persists_across_restart_by_identity() {
        use crate::application::FoldPreferences;

        let dir = std::env::temp_dir().join(format!("oya-foldpersist-{}", std::process::id()));
        let path = dir.join("folds");
        let _ = std::fs::remove_dir_all(&dir);

        let zed_row = |r: &Replayer| {
            r.app.rows().iter().position(
                |row| matches!(row.kind, RowKind::Group { group_idx } if r.app.app_group(group_idx).label == "Zed"),
            )
        };

        // Session 1: Zed folds by default; expanding it writes an override keyed by its identity.
        let mut r =
            Replayer::from_stream(stream(include_str!("../../tests/fixtures/app-groups.dsl")));
        r.app.set_folds(FoldPreferences::at(path.clone()));
        r.cycle();
        let row = zed_row(&r).expect("Zed folds by default");
        r.app.select_row(row);
        r.app.toggle_collapse();
        assert!(zed_row(&r).is_none(), "expanded: no group row");

        // Session 2: a fresh run loads the override and Zed starts expanded by identity.
        let mut r2 =
            Replayer::from_stream(stream(include_str!("../../tests/fixtures/app-groups.dsl")));
        r2.app.set_folds(FoldPreferences::at(path.clone()));
        r2.cycle();
        assert!(zed_row(&r2).is_none(), "fold preference persisted");

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
            .position(|row| matches!(row.kind, RowKind::Group { .. }))
            .unwrap();
        r.app.select_row(row);
        r.app.toggle_collapse();
        assert!(!r.kinds().iter().any(|k| matches!(k, RowKind::Group { .. })));

        // 10 exits and 13 (same identity) is born: the expansion follows the identity.
        r.cycle();
        assert!(
            !r.kinds().iter().any(|k| matches!(k, RowKind::Group { .. })),
            "expansion suppression follows the app identity across cycles"
        );
        // Folding again from a member row: the representative was replaced deterministically.
        r.app.select_row(0);
        r.toggle_group_fold();
        let selected = &r.app.rows()[r.app.selected()];
        assert!(matches!(selected.kind, RowKind::Group { .. }));
        assert!(selected.is_collapsed());
        assert_eq!(
            r.app.row_pid(selected),
            11,
            "representative was replaced deterministically after 10 exited"
        );
    }

    #[test]
    fn scrolling_does_not_recompute_every_row() {
        let dsl = format!(
            "{}\ncycle +1s\n",
            include_str!("../../tests/fixtures/app-groups.dsl")
        );
        let mut r = Replayer::from_stream(stream(&dsl));
        r.cycle();
        let rows = r.app.rows().len();
        assert!(rows > 3, "fixture must outscroll a 2-row viewport");
        r.app.prepare_visible_rows(2);
        let full = r.app.view_full_refreshes();
        for row in 0..rows {
            r.app.select_row(row);
            r.app.adjust_scroll(2);
            r.app.prepare_visible_rows(2);
        }
        assert_eq!(
            r.app.view_full_refreshes(),
            full,
            "moving the viewport refreshes only the visible rows"
        );
        // A new cycle is new data: that one is a full pass.
        r.cycle();
        assert!(r.app.view_full_refreshes() > full);
    }

    #[test]
    fn application_memory_sampling_is_visible_and_change_driven() {
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
            .position(|row| matches!(row.kind, RowKind::Group { group_idx } if r.app.app_group(group_idx).label == "Google Chrome"))
            .unwrap();
        r.app.select_row(chrome);
        r.app.adjust_scroll(1);
        r.app.prepare_visible_rows(1);
        let after_first = r.app.memory_read_count();
        assert!(after_first > 0, "a visible folded row samples its members");

        // Same generation, same viewport: the resident-set check reads nothing further.
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

    fn cpu_ramp_stream() -> oya_stream::Stream {
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
    fn replay_cmdline_settles_then_waits_for_refresh_interval() {
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

    /// A cgroup that makes a process a member of the desktop application `foo`.
    const FOO: &str = r#"uid=1000 cgroup="0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo@a.service\n""#;
    fn foo_stream(body: &str) -> oya_stream::Stream {
        stream(&format!("cycle 0\n{body}\ncycle +1s\n"))
    }

    fn group_rows(r: &Replayer) -> usize {
        r.kinds()
            .iter()
            .filter(|k| matches!(k, RowKind::Group { .. }))
            .count()
    }

    #[test]
    fn foreign_child_of_a_member_is_hidden_when_folded_and_nested_when_expanded() {
        let mut r = Replayer::from_stream(foo_stream(&format!(
            "  10 start=10 comm=foo {FOO}\n  11 ppid=10 start=11 comm=foo {FOO}\n  12 ppid=10 start=12 comm=foo {FOO}\n  20 ppid=11 start=20 comm=tool cmd=/bin/tool"
        )));
        r.cycle();
        // Folded: one group row at the root's native spot; the foreign tool is hidden in it.
        assert_eq!(r.display_depths(), vec![(10, 0)]);
        let t = r.totals(10);
        assert_eq!(t.count, 4, "the foreign descendant is counted in the fold");

        // Expanded: members are ordinary rows; the foreign child sits natively under its
        // parent, one level deeper — not at the member's level.
        r.select_pid(10);
        r.toggle_collapse();
        assert_eq!(r.display_depths(), vec![(10, 0), (11, 1), (20, 2), (12, 1)]);
    }

    #[test]
    fn group_with_two_top_members_has_one_row_and_no_row_for_the_other_top() {
        let mut r = Replayer::from_stream(stream(&format!(
            r"
            cycle 0
              1 comm=init
              2 ppid=1 comm=launcher cmd=/bin/launcher
              3 ppid=1 comm=svc cmd=/bin/svc
              10 ppid=2 start=10 comm=foo {FOO} ticks=0 mem=1M
              11 ppid=10 start=11 comm=foo {FOO} ticks=0 mem=1M
              12 ppid=3 start=12 comm=foo {FOO} ticks=0 mem=1M
              13 ppid=12 start=13 comm=foo {FOO} ticks=0 mem=1M

            cycle +1s
              10 ticks=10
              11 ticks=10
              12 ticks=10
              13 ticks=10
            "
        )));
        r.cycle();
        r.cycle();
        // The group row sits at the anchor (the top containing the representative, 10); the other
        // top member's native spot emits nothing.
        assert_eq!(r.display_depths(), vec![(1, 0), (2, 1), (10, 2), (3, 1)]);
        assert!(matches!(r.kinds()[2], RowKind::Group { .. }));
        // Both subtrees are counted once, at the group row.
        let t = r.totals(10);
        assert_eq!(t.count, 4);
        assert_eq!(t.cpu_pct, r.proc(10).cpu_pct * 4);
        assert_eq!(t.mem_bytes, 4 * 1024 * 1024);

        // Unfold, select a process under the non-anchor top, refold: the selection resolves to
        // the group row that now covers it.
        r.select_pid(10);
        r.toggle_collapse();
        r.select_pid(13);
        assert_eq!(r.selected_pid(), Some(13));
        r.toggle_group_fold();
        assert!(matches!(
            r.app.rows()[r.app.selected()].kind,
            RowKind::Group { .. }
        ));
        assert_eq!(r.selected_pid(), Some(10));
    }

    #[test]
    fn nested_group_counts_in_its_collapsed_ancestor_and_partial_groups_keep_their_outside() {
        let mut r = Replayer::from_stream(stream(&format!(
            r"
            cycle 0
              1 comm=init cmd=/sbin/init
              2 ppid=1 comm=shell cmd=/bin/shell
              10 ppid=2 start=10 comm=foo {FOO}
              11 ppid=10 start=11 comm=foo {FOO}
              12 ppid=10 start=12 comm=foo {FOO}
              20 ppid=1 start=20 comm=foo {FOO}

            cycle +1s
            "
        )));
        r.cycle();
        // `foo` has two tops: 10 (under shell 2) and 20. Anchor is the shallowest top (depth 1).
        assert_eq!(group_rows(&r), 1);
        assert_eq!(r.totals(20).count, 4);

        // Collapsing the shell hides top 10 inside it (ancestor wins): the shell counts it, and
        // the group's row is formed from the outside part (20) alone.
        r.select_pid(2);
        r.toggle_collapse();
        assert_eq!(r.display_pids(), vec![1, 2, 20]);
        assert_eq!(r.totals(2).count, 4, "shell + the whole hidden foo subtree");
        assert_eq!(r.totals(20).count, 1, "only the outside part of the group");
        assert_eq!(group_rows(&r), 1);

        // Collapsing init hides everything: no group row at all.
        r.select_pid(1);
        r.toggle_collapse();
        assert_eq!(r.display_pids(), vec![1]);
        assert_eq!(group_rows(&r), 0);
        assert_eq!(r.totals(1).count, 6);
    }

    #[test]
    fn collapsed_process_and_folded_group_agree_on_the_same_set() {
        // Two identical four-process trees: one a plain collapsed process, one a folded group.
        let mut r = Replayer::from_stream(stream(&format!(
            r"
            cycle 0
              100 comm=plain cmd=/bin/plain ticks=0 mem=2M
              101 ppid=100 comm=plain cmd=/bin/plain ticks=0 mem=2M
              102 ppid=100 comm=plain cmd=/bin/plain ticks=0 mem=2M
              103 ppid=101 comm=plain cmd=/bin/plain ticks=0 mem=2M
              200 start=200 comm=foo {FOO} ticks=0 mem=2M
              201 ppid=200 start=201 comm=foo {FOO} ticks=0 mem=2M
              202 ppid=200 start=202 comm=foo {FOO} ticks=0 mem=2M
              203 ppid=201 start=203 comm=foo {FOO} ticks=0 mem=2M

            cycle +1s
              100 ticks=10
              101 ticks=10
              102 ticks=10
              103 ticks=10
              200 ticks=10
              201 ticks=10
              202 ticks=10
              203 ticks=10
            "
        )));
        r.cycle();
        r.cycle();
        r.select_pid(100);
        r.toggle_collapse();
        let (process, group) = (r.totals(100), r.totals(200));
        assert_eq!(process, group, "same hidden set, same numbers");
        assert_eq!(process.count, 4);
        assert_eq!(process.threads, group.threads);
        // The badge counts hidden processes: the process row stays visible itself.
        let text = r.render_text(100, 12);
        let plain = text.lines().find(|l| l.contains("/bin/plain")).unwrap();
        let app = text.lines().find(|l| l.contains("◇")).unwrap();
        assert!(plain.contains("[+3]"), "{plain}");
        assert!(app.contains("[+4]"), "{app}");
    }

    #[test]
    fn g_and_left_on_a_top_member_fold_the_group_but_left_on_a_non_top_collapses_natively() {
        let mut r = Replayer::from_stream(foo_stream(&format!(
            "  10 start=10 comm=foo {FOO}\n  11 ppid=10 start=11 comm=foo {FOO}\n  12 ppid=11 start=12 comm=foo {FOO}\n  13 ppid=11 start=13 comm=foo {FOO}"
        )));
        r.cycle();
        r.select_pid(10);
        r.toggle_collapse();
        assert_eq!(r.display_pids(), vec![10, 11, 12, 13]);

        // Left on a non-top member: native collapse; the group stays expanded.
        r.select_pid(11);
        r.collapse_selected();
        assert_eq!(r.display_pids(), vec![10, 11]);
        assert!(r.row_collapsed(11));
        assert_eq!(group_rows(&r), 0);
        r.toggle_collapse();
        assert_eq!(r.display_pids(), vec![10, 11, 12, 13]);

        // `g` from any member row folds the whole group; selection lands on the group row.
        r.select_pid(12);
        r.toggle_group_fold();
        assert_eq!(r.display_pids(), vec![10]);
        assert!(matches!(
            r.app.rows()[r.app.selected()].kind,
            RowKind::Group { .. }
        ));
        // `g` on the group row expands again, landing on the representative process.
        r.toggle_group_fold();
        assert_eq!(r.display_pids(), vec![10, 11, 12, 13]);
        assert_eq!(r.selected_pid(), Some(10));

        // Left on the top member folds the group too (fold-by-identity).
        r.select_pid(10);
        r.collapse_selected();
        assert_eq!(r.display_pids(), vec![10]);
        assert_eq!(group_rows(&r), 1);
        // Enter on a top member of an expanded group folds it as well.
        r.toggle_collapse();
        assert_eq!(group_rows(&r), 0);
        r.select_pid(10);
        r.toggle_collapse();
        assert_eq!(group_rows(&r), 1);
    }

    #[test]
    fn steady_cycles_do_not_rebuild_rows_but_reparenting_and_toggles_do() {
        let mut r = Replayer::from_stream(stream(
            r"
            cycle 0
              1 uid=0 comm=init cmd=/sbin/init
              2 comm=supervisor cmd=/usr/bin/supervisor
              42 ppid=1 start=4242 comm=worker cmd=/usr/bin/worker

            cycle +1s
              42 ticks=10

            cycle +1s
              42 ppid=2
            ",
        ));
        r.cycle();
        let built = r.layout_count();
        r.cycle();
        assert_eq!(
            r.layout_count(),
            built,
            "a steady cycle reuses the rows (numbers still refresh)"
        );
        assert!(r.proc(42).cpu_pct > 0, "numbers refresh without a rebuild");
        r.cycle();
        assert_eq!(r.layout_count(), built + 1, "reparenting rebuilds the rows");
        assert_eq!(r.display_pids(), vec![1, 2, 42]);

        r.select_pid(2);
        r.toggle_collapse();
        assert_eq!(r.layout_count(), built + 2, "a collapse change rebuilds");
    }

    #[test]
    fn collapsed_row_series_is_the_sum_of_member_series_and_scrolls() {
        let mut r = Replayer::from_stream(stream(
            r"
            cycle 0
              10 comm=parent cmd=/srv/parent ticks=0
              11 ppid=10 comm=a cmd=/srv/a ticks=0
              12 ppid=10 comm=b cmd=/srv/b ticks=0

            cycle +1s
              10 ticks=+10
              11 ticks=+20
              12 ticks=+5

            cycle +1s
              10 ticks=+0
              11 ticks=+0
              12 ticks=+30

            cycle +1s
              12 ticks=+0

            cycle +1s
            ",
        ));
        r.cycle();
        r.cycle();
        r.select_pid(10);
        r.toggle_collapse();
        let sum = |r: &Replayer| {
            let mut out = [crate::gather::SeriesCell::default(); crate::gather::SPARK_CELLS];
            for pid in [10, 11, 12] {
                let series = r.app.cpu_series(pid).expect("tracked");
                for (acc, cell) in out.iter_mut().zip(series.0) {
                    acc.add(cell);
                }
            }
            out
        };
        assert_eq!(r.row_series(10).unwrap().0, sum(&r));
        assert_eq!(
            r.row_series(10).unwrap().rates().last().copied(),
            Some(3_500)
        );

        // Subsequent cycles push the row's own sample and stay equal to the member sum.
        r.cycle();
        assert_eq!(r.row_series(10).unwrap().0, sum(&r));
        r.cycle();
        assert_eq!(r.row_series(10).unwrap().0, sum(&r));
        r.cycle();
        assert_eq!(r.row_series(10).unwrap().0, sum(&r));
        assert_eq!(
            r.row_series(10).unwrap().rates().last().copied(),
            Some(0),
            "quiet cycle scrolls a zero in"
        );

        // A rebuild (expand, collapse again) reconstructs the same series from the members.
        r.toggle_collapse();
        r.toggle_collapse();
        assert_eq!(r.row_series(10).unwrap().0, sum(&r));
    }

    #[test]
    fn a_dying_top_member_does_not_erase_the_groups_sparkline_history() {
        let mut r = Replayer::from_stream(stream(&format!(
            r"
            cycle 0
              1 comm=init
              2 ppid=1 comm=launcher cmd=/bin/launcher
              3 ppid=1 comm=svc cmd=/bin/svc
              10 ppid=2 start=10 comm=foo {FOO} ticks=0
              11 ppid=10 start=11 comm=foo {FOO} ticks=0
              14 ppid=10 start=14 comm=foo {FOO} ticks=0
              12 ppid=3 start=12 comm=foo {FOO} ticks=0
              13 ppid=12 start=13 comm=foo {FOO} ticks=0

            cycle +1s
              10 ticks=+10
              12 ticks=+20

            cycle +1s
              10 ticks=+10
              12 ticks=+20

            cycle +1s
              - 13
              - 12

            cycle +1s
            "
        )));
        for _ in 0..3 {
            r.cycle();
        }
        let before = r.row_series(10).unwrap().rates();
        assert_eq!(before[crate::gather::SPARK_CELLS - 2..], [3_000, 3_000]);
        r.cycle();
        assert_eq!(r.totals(10).count, 3, "top member 12 left the group");
        let after = r.row_series(10).unwrap().rates();
        let n = crate::gather::SPARK_CELLS;
        assert_eq!(
            after[n - 4..],
            [0, 3_000, 3_000, 0],
            "the dead top member's past stays in the row's history"
        );
    }

    #[test]
    fn a_root_that_moves_out_of_the_hidden_set_takes_its_history_along() {
        let mut r = Replayer::from_stream(stream(&format!(
            r"
            cycle 0
              1 comm=init cmd=/sbin/init
              2 ppid=1 comm=shell cmd=/bin/shell ticks=0
              10 ppid=2 start=10 comm=foo {FOO} ticks=0
              11 ppid=10 start=11 comm=foo {FOO} ticks=0
              12 ppid=10 start=12 comm=foo {FOO} ticks=0
              20 ppid=1 start=20 comm=foo {FOO} ticks=0

            cycle +1s
              10 ticks=+10
              20 ticks=+5

            cycle +1s
              10 ticks=+10
              20 ticks=+5
            "
        )));
        for _ in 0..3 {
            r.cycle();
        }
        // The group row (anchored at the shallower top, 20) sums both tops.
        let sum = r.row_series(20).unwrap().rates();
        let n = crate::gather::SPARK_CELLS;
        assert_eq!(sum[n - 2..], [1_500, 1_500]);

        // The shell collapses and takes top 10 into its own hidden set: the group keeps 20 only.
        r.select_pid(2);
        r.toggle_collapse();
        assert_eq!(r.display_pids(), vec![1, 2, 20]);
        let group = r.row_series(20).unwrap();
        assert_eq!(group.rates()[n - 2..], [500, 500]);
        assert_eq!(group, r.app.cpu_series(20).unwrap());
        assert_eq!(r.row_series(2).unwrap().rates()[n - 2..], [1_000, 1_000]);
    }

    #[test]
    fn selection_follows_its_process_into_a_group_that_forms_over_it() {
        let mut r = Replayer::from_stream(stream(&format!(
            r"
            cycle 0
              1 comm=init
              2 ppid=1 comm=launcher cmd=/bin/launcher
              10 ppid=2 start=10 comm=foo {FOO}
              11 ppid=10 start=11 comm=foo {FOO}
              30 ppid=1 comm=other cmd=/bin/other

            cycle +1s
              + 12 ppid=10 start=12 comm=foo {FOO}
            "
        )));
        r.cycle();
        assert_eq!(r.display_pids(), vec![1, 2, 10, 11, 30]);
        r.select_pid(11);
        r.cycle();
        // The three-member group folds over the selected process; the stale index would land on
        // the unrelated row below.
        assert_eq!(r.display_pids(), vec![1, 2, 10, 30]);
        assert!(matches!(
            r.app.rows()[r.app.selected()].kind,
            RowKind::Group { .. }
        ));
    }

    #[test]
    fn kthreadd_stays_expanded_after_the_users_last_choice() {
        let mut r = Replayer::from_stream(stream(
            r"
            cycle 0
              2 uid=0 start=2 kthread=true comm=kthreadd
              3 ppid=2 uid=0 start=3 kthread=true comm=kworker/0:0

            cycle +1s
            ",
        ));
        r.cycle();
        r.select_pid(2);
        for expect_expanded in [true, false, true] {
            r.toggle_collapse();
            assert_eq!(r.display_pids().len() > 1, expect_expanded);
        }
        r.cycle();
        assert_eq!(r.display_pids().len(), 2, "a rebuild keeps the last choice");
    }

    #[test]
    fn memory_falls_back_per_process_and_marks_it() {
        let dir = std::env::temp_dir().join(format!("oya-pss-row-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("11")).unwrap();
        // Process 11 reads as 40 MiB proportional at 100 MiB resident; 12 has no smaps rollup.
        std::fs::write(
            dir.join("11/smaps_rollup"),
            "Rss: 102400 kB\nPss: 40960 kB\nShared_Clean: 102400 kB\nShared_Dirty: 0 kB\n\
             Private_Clean: 0 kB\nPrivate_Dirty: 0 kB\n",
        )
        .unwrap();
        let mut r = Replayer::from_stream(stream(
            r"
            cycle 0
              10 comm=parent cmd=/srv/parent mem=100M
              11 ppid=10 comm=a cmd=/srv/a mem=100M
              12 ppid=10 comm=b cmd=/srv/b mem=100M

            cycle +1s
            ",
        ));
        r.app
            .set_memory(crate::application::MemorySampler::with_root(dir.clone()));
        r.cycle();

        // Before any sampling every row is a resident-set fallback, flagged.
        assert!(r.totals(11).mem_estimated);
        let before = r.render_text(100, 12);
        let row = before.lines().find(|l| l.contains("/srv/a")).unwrap();
        assert!(row.contains("~100.0M"), "{row}");

        assert!(r.app.prepare_visible_rows(10), "sampling changed numbers");
        let after = r.render_text(100, 12);
        let sampled = after.lines().find(|l| l.contains("/srv/a")).unwrap();
        assert!(
            sampled.contains("40.0M") && !sampled.contains('~'),
            "{sampled}"
        );
        let unreadable = after.lines().find(|l| l.contains("/srv/b")).unwrap();
        assert!(unreadable.contains("~100.0M"), "{unreadable}");

        // The same viewport at the same generation samples nothing more.
        assert!(!r.app.prepare_visible_rows(10));

        // A parent folds its children's marker in: one fallback child marks the whole sum.
        r.select_pid(10);
        r.toggle_collapse();
        let t = r.totals(10);
        assert!(t.mem_estimated);
        assert_eq!(t.mem_bytes, (100 + 40 + 100) * 1024 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn chromium_group_stream() -> oya_stream::Stream {
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

    fn cgroup_group_stream() -> oya_stream::Stream {
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
        assert_eq!(r.display_pids(), vec![10, 11, 12, 13, 14, 15]);
        assert!(!r.row_collapsed(10));
    }

    #[test]
    fn replay_user_expansion_suppresses_auto_recollapse_while_group_lives() {
        let mut r = Replayer::from_stream(cgroup_group_stream());
        r.cycle();
        r.select_pid(200);
        r.toggle_collapse();
        assert_eq!(r.display_pids(), vec![200, 201, 202]);

        r.cycle();
        assert_eq!(r.display_pids(), vec![200, 201, 202]);
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
        assert_eq!(r.display_pids(), vec![200, 201, 202]);

        r.cycle();
        assert_eq!(r.display_pids(), Vec::<u32>::new());

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

        assert_eq!(r.display_pids(), vec![200, 201, 202]);
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
        assert_eq!(r.display_pids(), vec![300, 301, 305, 306, 302, 303, 304]);
        assert!(!r.row_collapsed(300));
    }

    #[test]
    fn replay_full_frame_layout_at_80_by_24() {
        // Raw cumulative jiffies: 4 cores at 100 Hz = 400 jiffies/s total.
        // Cycle 0 seeds the ring; cycle 1 computes the delta.
        // Delta: user=80, sys=40, idle=280 out of 400 total → 20% user, 10% sys.
        let stream = stream(
            r"
            cycle 0 stat_idle=10000 mem=8G mem_available=6G mem_free=2G swap_total=4G swap_free=4G runnable=2 psi_cpu=0 psi_memory=0 pswpin=0
              1 uid=0 comm=init cmd=/sbin/init
              42 ppid=1 comm=hot cmd=/usr/bin/hot
              100 comm=batch cmd=/opt/batch

            cycle +1s stat_user=+80 stat_system=+40 stat_idle=+280 mem_available=5G swap_free=3872M psi_cpu=+100000 psi_memory=+0 pswpin=+10
              1 ticks=1
              42 ticks=50 mem=64M
              100 ticks=5 mem=12M
            ",
        );

        let mut r = Replayer::from_stream(stream);
        r.set_header_width(80);
        r.cycle();
        r.cycle();

        let screen = r.render(80, 24);
        // 1.2 busy cores (user 80 + sys 40 of 400 jiffies on 4), 0.4 of them kernel; CPU
        // stalled 100 ms of the 1 s window = 10%.
        let cpu = screen.row(0);
        assert!(cpu.starts_with("CPU 1.2/4  wait  10%  sys 0.4"), "{cpu:?}");
        assert!(
            cpu.contains("▅"),
            "1.2 of a 2-core scale, one interval: {cpu:?}"
        );
        assert!(
            cpu.contains(" /2 1s"),
            "scale label covers the one drawn cell: {cpu:?}"
        );
        let mem = screen.row(1);
        assert!(mem.starts_with("Mem ["), "{mem:?}");
        // used 3G of 8G, free 2G, cache 3G. Slots are fixed-width; at 80 columns the bars get
        // their guaranteed share and the lower-priority slots (swap-in rate, ...) are dropped.
        assert!(
            mem.contains("]   3.0G/8.0G  avail   5.0G  wait   0%  Swap ["),
            "{mem:?}"
        );
        assert!(mem.contains("] 224.0M/4.0G"), "{mem:?}");
        assert!(!mem.contains(" in "), "swap-in slot dropped at 80: {mem:?}");
        for glyph in ['⣿', '⣤'] {
            assert!(mem.contains(glyph), "bar glyph {glyph}: {mem:?}");
        }
        assert!(screen.row(2).contains("PID"));
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
        // The stream never names a wait, so the sparkline has no measurement: its glyph is the
        // neutral tone (not the calm end of the pressure ramp), at the height of 50% of a core.
        let spark = screen.newest_spark_cell(hot_row);
        assert_eq!(spark.contents(), "▅");
        assert_eq!(spark.fgcolor(), vt_color(palette::WAIT_UNKNOWN));
        assert_eq!(
            screen.fg_at_text(hot_row, "R"),
            vt_color(palette::state(b'R'))
        );

        r.move_down();
        assert_eq!(r.selected_pid(), Some(42));
    }

    #[test]
    fn row_sparkline_color_is_the_wait_share_and_height_is_log_cpu() {
        let mut r = Replayer::from_stream(stream(
            r"
            cycle 0
              1 uid=0 comm=init cmd=/sbin/init
              40 ppid=1 comm=calm cmd=/usr/bin/calm ticks=0 wait=0
              41 ppid=1 comm=starved cmd=/usr/bin/starved ticks=0 wait=0
              42 ppid=1 comm=unmeasured cmd=/usr/bin/unmeasured ticks=0
              43 ppid=1 comm=waitonly cmd=/usr/bin/waitonly ticks=0 wait=0

            cycle +1s
              40 ticks=+20 wait=+0
              41 ticks=+20 wait=+20
              42 ticks=+20
              43 wait=+30
            ",
        ));
        r.cycle();
        r.cycle();
        let screen = r.render(100, 16);
        let spark = |name: &str| {
            let cell = screen.newest_spark_cell(row_containing(&screen, name));
            (cell.contents(), cell.fgcolor())
        };
        // 20 ticks over 100 jiffies = 20% of a core: level 3 on the shared ladder.
        assert_eq!(
            spark("/usr/bin/calm"),
            ("▃", vt_color(palette::pressure(0)))
        );
        // Waited as long as it ran: 50% of the time it wanted a CPU, it was queued.
        assert_eq!(
            spark("/usr/bin/starved"),
            ("▃", vt_color(palette::pressure(5000)))
        );
        assert_eq!(
            spark("/usr/bin/unmeasured"),
            ("▃", vt_color(palette::WAIT_UNKNOWN))
        );
        // Never ran, only queued: not idle — the lowest block, fully waiting.
        assert_eq!(
            spark("/usr/bin/waitonly"),
            ("▁", vt_color(palette::pressure(10_000)))
        );
    }

    #[test]
    fn collapsed_row_wait_share_is_summed_wait_over_summed_wait_plus_run() {
        let mut r = Replayer::from_stream(stream(
            r"
            cycle 0
              10 comm=parent cmd=/srv/parent ticks=0 wait=0
              11 ppid=10 comm=a cmd=/srv/a ticks=0 wait=0
              12 ppid=10 comm=b cmd=/srv/b ticks=0 wait=0
              13 ppid=10 comm=c cmd=/srv/c ticks=0

            cycle +1s
              10 ticks=+0
              11 ticks=+90 wait=+10
              12 ticks=+5 wait=+5
              13 ticks=+40
            ",
        ));
        r.cycle();
        r.cycle();
        r.select_pid(10);
        r.toggle_collapse();
        let cell = *r.row_series(10).unwrap().0.last().unwrap();
        // 13 is unmeasured: it adds to the height but to neither side of the share.
        assert_eq!(cell.rate, 13_500);
        assert_eq!(cell.waiting, crate::gather::Waiting { wait: 15, run: 95 });
        assert_eq!(cell.waiting.share_bp(), Some(1363));
        // The rebuilt-from-members series agrees (collapse again → recomputed).
        r.toggle_collapse();
        r.toggle_collapse();
        assert_eq!(*r.row_series(10).unwrap().0.last().unwrap(), cell);
    }

    /// Ad-hoc perf probe over a real capture. Times `App::gather` (the full per-cycle grouping
    /// rebuild) and a cold full render, on real desktop cgroup data.
    /// `OYA_CAPTURE=/path/to.dsl cargo test --release -p oyabun capture_grouping_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "timing; needs OYA_CAPTURE"]
    #[allow(clippy::cast_precision_loss)]
    fn capture_grouping_cost() {
        use std::time::Instant;
        let path = std::env::var("OYA_CAPTURE").expect("set OYA_CAPTURE to a .dsl capture");
        let text = std::fs::read_to_string(&path).expect("read capture");
        let stream = oya_stream::Stream::parse(&text).expect("parse capture");
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
