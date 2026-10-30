//! Application state for the single-threaded loop: it owns the [`Gatherer`] (which produces
//! the live process buffer) plus the view state — collapse choices, selection/scroll, and the
//! [`ViewTree`] of display rows. [`App::gather`] runs one gather cycle, rebuilds the tree only
//! if the structure, grouping, or collapse choices moved, and refreshes the per-row numbers;
//! render reads everything back through `&self`. The borrow checker proves the gather (`&mut`)
//! and the render (`&`) never overlap, so there is no snapshot exchange.

use crate::application::{
    AppGroup, AppGroupKey, ApplicationGroups, DesktopResolver, FoldPreferences, MemorySampler,
};
use crate::control::VerifiedProcess;
use crate::fxhash::FxMap;
use crate::gather::{CpuHistory, CpuSeries, Gatherer};
use crate::header;
use crate::procs::{ProcessEntry, Procs, SystemStats};
use crate::sys::ProcDir;
use crate::view::{Collapse, RowKind, Totals, ViewRow, ViewTree};

/// What the viewport shows: the candidate set of processes the view sums numbers for is a
/// function of these alone.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ViewportKey {
    layout: u64,
    scroll: usize,
    height: usize,
}

/// What the memory sampler's last pass looked at. The candidate set (what the viewport shows or
/// hides) is a function of these alone, so an unchanged key means nothing new to sample and the
/// per-frame call is free.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SampleKey {
    generation: u64,
    layout: u64,
    scroll: usize,
    height: usize,
}

pub struct App {
    /// Produces the live process buffer; owns the THP arena + per-PID stores + I/O backend.
    gatherer: Gatherer,
    /// The display rows and their per-cycle numbers.
    view: ViewTree,
    /// What the view was last built from: the structure/grouping epoch and the collapse
    /// version. Equal ⇒ the rows are still right and only the numbers need refreshing.
    built: Option<(u64, u64)>,
    /// Bumped on every rebuild of the rows; part of what invalidates a sampling pass.
    layout: u64,
    sampled: Option<SampleKey>,
    /// Visible body height as of the last [`App::prepare_visible_rows`]; 0 before the first.
    viewport_height: usize,
    collapse: Collapse,
    /// Persisted, per-identity fold overrides — a folded application returns folded next run.
    folds: FoldPreferences,
    applications: ApplicationGroups,
    desktop: DesktopResolver,
    memory: MemorySampler,
    uid_names: FxMap<u32, Box<str>>,
    selected: usize,
    scroll: usize,
    /// The processes the viewport shows numbers for (visible rows and the hidden sets of visible
    /// collapsed rows), and the viewport it was computed for. Kept so the sampler and the totals
    /// pass share one candidate list, refilled only when the viewport or the rows move.
    viewport_scratch: Vec<usize>,
    viewport_for: Option<ViewportKey>,
    /// Terminal width the header was last laid out for; sizes the CPU graph's history window.
    header_width: u16,
    /// Where every header widget goes at `header_width`: rebuilt only on a resize (or once the
    /// first sample reveals the machine), so widgets never move with the data.
    header_layout: header::HeaderLayout,
}

impl App {
    pub fn new(page_size: u64, proc_dir: ProcDir, uid_names: FxMap<u32, Box<str>>) -> Self {
        Self::assemble(
            Gatherer::new(page_size, proc_dir),
            uid_names,
            FoldPreferences::system(),
            DesktopResolver::system(),
        )
    }

    fn assemble(
        gatherer: Gatherer,
        uid_names: FxMap<u32, Box<str>>,
        folds: FoldPreferences,
        desktop: DesktopResolver,
    ) -> Self {
        Self {
            gatherer,
            view: ViewTree::default(),
            built: None,
            layout: 0,
            sampled: None,
            viewport_height: 0,
            collapse: Collapse::default(),
            folds,
            applications: ApplicationGroups::default(),
            desktop,
            memory: MemorySampler::system(),
            uid_names,
            selected: 0,
            scroll: 0,
            viewport_scratch: Vec::new(),
            viewport_for: None,
            header_width: 0,
            header_layout: header::HeaderLayout::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_gatherer(gatherer: Gatherer, uid_names: FxMap<u32, Box<str>>) -> Self {
        Self::from_gatherer_with_desktop(
            gatherer,
            uid_names,
            DesktopResolver::with_roots(
                vec![std::path::PathBuf::from(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/desktops"
                ))],
                FxMap::default(),
            ),
        )
    }

    #[cfg(test)]
    pub(crate) fn from_gatherer_with_desktop(
        gatherer: Gatherer,
        uid_names: FxMap<u32, Box<str>>,
        desktop: DesktopResolver,
    ) -> Self {
        Self::assemble(gatherer, uid_names, FoldPreferences::disabled(), desktop)
    }

    #[cfg(test)]
    pub(crate) fn set_folds(&mut self, folds: FoldPreferences) {
        self.folds = folds;
    }

    /// Sample proportional memory from a fake `/proc` instead of the real one.
    #[cfg(test)]
    pub(crate) fn set_memory(&mut self, memory: MemorySampler) {
        self.memory = memory;
    }

    /// Run one gather cycle, then bring the view up to date (selection follows its identity).
    pub fn gather(&mut self) {
        let keep = self.selected_key();
        self.gatherer.set_wait_interest(&self.viewport_scratch);
        self.gatherer.cycle();
        if !self.header_layout.is_informed() {
            self.relayout_header();
        }
        // The one gate: grouping and rows are a function of the structure epoch and the collapse
        // choices, so neither is rebuilt while both are what the view was built from.
        let built = (self.gatherer.meta_epoch(), self.collapse.version());
        let regroup = self.built.is_none_or(|(epoch, _)| epoch != built.0);
        if regroup {
            self.regroup_applications();
        }
        self.begin_memory_cycle();
        if regroup || self.built != Some(built) {
            self.relayout(keep.as_ref());
        } else {
            self.refresh_view();
        }
        self.sampled = None;
    }

    /// Tell the app the terminal width. The header is laid out for it here and nowhere else
    /// (besides the first sample): a resize is the only thing that moves a widget. The CPU graph
    /// takes one cell per refresh, so history depth follows the layout: the ring grows to the
    /// widest width seen (never shrinks) and the graph's scale tracks exactly what is visible.
    pub fn set_header_width(&mut self, width: u16) {
        // Called every main-loop iteration.
        if width == self.header_width {
            return;
        }
        self.header_width = width;
        self.relayout_header();
    }

    fn relayout_header(&mut self) {
        let width = usize::from(self.header_width);
        self.header_layout = header::HeaderLayout::new(self.gatherer.sys(), width);
        self.gatherer
            .fit_cpu_history(width, self.header_layout.cpu_cells());
    }

    pub fn header_layout(&self) -> &header::HeaderLayout {
        &self.header_layout
    }

    // --- views for render ---

    pub fn sys(&self) -> &SystemStats {
        self.gatherer.sys()
    }

    pub fn cpu_history(&self) -> &CpuHistory {
        self.gatherer.cpu_history()
    }

    pub fn procs(&self) -> &Procs {
        self.gatherer.procs()
    }

    pub fn rows(&self) -> &[ViewRow] {
        self.view.rows()
    }

    /// The numbers a row shows: its own for a plain process, the sum over its hidden set for a
    /// collapsed row.
    pub(crate) fn row_totals(&self, row: usize) -> Totals {
        self.view.totals(row)
    }

    /// The history series a row's sparkline draws, if it has one.
    pub(crate) fn row_series(&self, row: usize) -> Option<CpuSeries> {
        match self.view.rows().get(row)?.kind {
            RowKind::Process { proc_idx } if !self.view.rows()[row].is_collapsed() => {
                self.gatherer.cpu_series(self.entry(proc_idx).pid)
            }
            _ => self.view.series(row),
        }
    }

    pub(crate) fn app_group(&self, group_idx: usize) -> &AppGroup {
        &self.applications.groups()[group_idx]
    }

    /// The process a row is shown through: the process itself or a group's representative.
    /// Test convenience.
    #[cfg(test)]
    pub(crate) fn row_pid(&self, row: &ViewRow) -> u32 {
        match row.kind {
            RowKind::Process { proc_idx } => self.entry(proc_idx).pid,
            RowKind::Group { group_idx } => {
                self.entry(self.app_group(group_idx).representative).pid
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn view_full_refreshes(&self) -> usize {
        self.view.full_refreshes()
    }

    #[cfg(test)]
    pub(crate) fn memory_read_count(&self) -> usize {
        self.memory.read_count()
    }

    #[cfg(test)]
    pub(crate) fn meta_epoch(&self) -> u64 {
        self.gatherer.meta_epoch()
    }

    /// How many times the display rows have been rebuilt.
    #[cfg(test)]
    pub(crate) fn layout_count(&self) -> u64 {
        self.layout
    }

    /// Live PIDs this cycle that overflowed the persistent-fd pool (0 in the common case).
    /// Non-zero ⇒ `RLIMIT_NOFILE` is the binding constraint — surfaced so it is never silent.
    pub fn pool_overflow(&self) -> u32 {
        self.gatherer.pool_overflow()
    }

    /// Whether the privileged BPF observation source is active.
    pub fn is_privileged(&self) -> bool {
        self.gatherer.is_privileged()
    }

    /// Short-lived processes (born+died between cycles) caught by BPF fork/exit events this
    /// cycle — invisible to a snapshot-only tool. Always 0 in `/proc` mode.
    pub fn short_lived(&self) -> u32 {
        self.gatherer.short_lived()
    }

    pub fn gpu_available(&self) -> bool {
        self.gatherer.gpu_available()
    }

    pub fn gpu_process_available(&self) -> bool {
        self.gatherer.gpu_process_available()
    }

    pub fn gpu_process_sample_available(&self) -> bool {
        self.gatherer.gpu_process_sample_available()
    }

    #[cfg(test)]
    pub(crate) fn cpu_series(&self, pid: u32) -> Option<CpuSeries> {
        self.gatherer.cpu_series(pid)
    }

    pub fn empty_gpus(&self) -> Option<&[u32]> {
        self.gatherer.empty_gpus()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn uid_name(&self, uid: u32) -> &str {
        self.uid_names.get(&uid).map_or("?", |s| s)
    }

    /// A row's cmdline bytes, resolved directly from the gatherer's `Cmd` store.
    pub fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
        self.gatherer.cmdline(e)
    }

    fn entry(&self, proc_idx: usize) -> &ProcessEntry {
        &self.gatherer.procs().as_slice()[proc_idx]
    }

    fn selected_key(&self) -> Option<SelectionKey> {
        let row = self.view.rows().get(self.selected)?;
        Some(match row.kind {
            RowKind::Process { proc_idx } => SelectionKey::Process(self.entry(proc_idx).pid),
            RowKind::Group { group_idx } => {
                let group = self.app_group(group_idx);
                SelectionKey::Group(group.key.clone(), self.entry(group.representative).pid)
            }
        })
    }

    fn regroup_applications(&mut self) {
        let App {
            gatherer,
            applications,
            desktop,
            ..
        } = self;
        let procs = gatherer.procs().as_slice();
        let identities = (0..procs.len())
            .filter_map(|idx| gatherer.identity(idx).map(|identity| (idx, identity)));
        applications.rebuild(procs, identities, desktop);
    }

    /// Reset the per-cycle proportional-read budget and evict cached entries of dead/reused
    /// PIDs. The sampler then reads only what the viewport shows (`prepare_visible_rows`).
    fn begin_memory_cycle(&mut self) {
        let procs = self.gatherer.procs().as_slice();
        self.memory.begin_cycle(self.gatherer.generation(), procs);
    }

    /// Rebuild the display rows from the native tree, grouping, and collapse state, restore the
    /// selection onto `keep` (selection-follow across rebuilds), and anchor scroll so the
    /// selected row stays at the same screen line — minimizing visible row movement from
    /// births/deaths outside the viewport. If `keep` no longer has a row of its own (its process
    /// was folded into a group, or hidden under a collapsed ancestor), the selection moves to
    /// the row that now shows or contains that process rather than staying on a stale index.
    fn relayout(&mut self, keep: Option<&SelectionKey>) {
        let anchor_offset = self.selected.saturating_sub(self.scroll);
        let mut survived = false;
        {
            let App {
                gatherer,
                view,
                collapse,
                folds,
                applications,
                selected,
                ..
            } = self;
            let procs = gatherer.procs().as_slice();
            collapse.sync(procs, applications, folds);
            view.rebuild(gatherer, applications, collapse);
            let found = view
                .rows()
                .iter()
                .position(|row| row_matches(row, keep, procs, applications))
                .or_else(|| {
                    let pid = keep?.pid();
                    let idx = procs.binary_search_by(|p| p.pid.cmp(&pid)).ok()?;
                    view.row_covering(procs, idx)
                });
            if let Some(idx) = found {
                *selected = idx;
                survived = true;
            }
        }
        self.built = Some((self.gatherer.meta_epoch(), self.collapse.version()));
        self.layout += 1;
        self.clamp_selection();
        if survived {
            self.scroll = self.selected.saturating_sub(anchor_offset);
        }
        // After scroll settles: the viewport (hence the estimates the totals use) depends on it.
        self.refresh_view();
    }

    fn viewport_key(&self) -> ViewportKey {
        ViewportKey {
            layout: self.layout,
            scroll: self.scroll,
            height: self.viewport_height,
        }
    }

    /// Refill the candidate list if the viewport or the rows moved since it was computed.
    /// Returns whether it was refilled.
    fn ensure_viewport(&mut self) -> bool {
        let key = self.viewport_key();
        if self.viewport_for == Some(key) {
            return false;
        }
        let App {
            gatherer,
            view,
            viewport_scratch,
            ..
        } = self;
        let procs = gatherer.procs().as_slice();
        view.viewport_procs(procs, key.scroll, key.height, viewport_scratch);
        self.viewport_for = Some(key);
        true
    }

    fn refresh_view(&mut self) {
        self.ensure_viewport();
        let App {
            gatherer,
            view,
            memory,
            viewport_scratch,
            ..
        } = self;
        view.refresh(gatherer, memory, viewport_scratch);
    }

    // --- navigation ---

    fn clamp_selection(&mut self) {
        if self.view.rows().is_empty() {
            self.selected = 0;
            self.scroll = 0;
        } else {
            self.selected = self.selected.min(self.view.rows().len() - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.view.rows().is_empty() {
            self.selected = (self.selected + 1).min(self.view.rows().len() - 1);
        }
    }

    pub fn page_up(&mut self, page: usize) {
        self.selected = self.selected.saturating_sub(page);
    }

    pub fn page_down(&mut self, page: usize) {
        if !self.view.rows().is_empty() {
            self.selected = (self.selected + page).min(self.view.rows().len() - 1);
        }
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.view.rows().len().saturating_sub(1);
    }

    #[cfg(test)]
    pub(crate) fn select_row(&mut self, row: usize) {
        if !self.view.rows().is_empty() {
            self.selected = row.min(self.view.rows().len() - 1);
        }
    }

    /// Keep the selected row within the viewport. Returns whether scroll moved.
    pub fn adjust_scroll(&mut self, visible_height: usize) -> bool {
        if visible_height == 0 {
            return false;
        }
        let before = self.scroll;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible_height {
            self.scroll = self.selected - visible_height + 1;
        }
        self.scroll != before
    }

    /// Perform bounded proportional-memory sampling after the viewport is known, so rendering
    /// stays pure I/O. The candidates are the processes the viewport shows a number for: plain
    /// rows and everything hidden inside visible collapsed rows. The sampler estimates each
    /// one's proportional set size from a cached split and the free resident reading, walking
    /// the address space only to bootstrap a process or on a staggered backstop under a
    /// per-cycle budget. The whole pass is skipped unless the generation, layout, scroll, or
    /// viewport height moved, so calling it every loop iteration costs nothing on a settled
    /// screen. Returns whether the numbers may have changed (new readings, or a viewport showing
    /// different estimates): the frame must repaint.
    pub fn prepare_visible_rows(&mut self, visible_height: usize) -> bool {
        let key = SampleKey {
            generation: self.gatherer.generation(),
            layout: self.layout,
            scroll: self.scroll,
            height: visible_height,
        };
        if self.sampled == Some(key) {
            return false;
        }
        self.sampled = Some(key);
        self.viewport_height = visible_height;
        // A moved viewport can show already-cached estimates, so it refreshes the numbers even
        // when nothing is read.
        let moved = self.ensure_viewport();
        let read = {
            let App {
                gatherer,
                memory,
                viewport_scratch,
                ..
            } = self;
            memory.sample(viewport_scratch, gatherer.procs().as_slice())
        };
        if read || moved {
            // Only estimates or the viewport changed (the gather's full pass already ran for
            // this cycle's data): recompute just the visible rows, not every process.
            let App {
                gatherer,
                view,
                memory,
                viewport_scratch,
                scroll,
                ..
            } = self;
            view.refresh_visible(gatherer, memory, viewport_scratch, *scroll, visible_height);
        }
        read || moved
    }

    // --- actions ---

    fn selected_row(&self) -> Option<&ViewRow> {
        self.view.rows().get(self.selected)
    }

    /// Fold or expand a group by identity, persisting the choice where the identity is stable.
    /// Selection stays on the group row when folding, and lands on the representative when
    /// expanding (the group row no longer exists).
    fn set_group_folded(&mut self, group_idx: usize, folded: bool) {
        let group = self.app_group(group_idx);
        let key = group.key.clone();
        let persist_key = group.persist_key.clone();
        let representative_pid = self.entry(group.representative).pid;
        self.collapse.set_group_folded(&key, folded);
        // A group with no persistable identity (container/pod/structural) is session-only.
        if let Some(persist_key) = persist_key {
            self.folds.set_expanded(&persist_key, !folded);
        }
        let keep = if folded {
            SelectionKey::Group(key, representative_pid)
        } else {
            SelectionKey::Process(representative_pid)
        };
        self.relayout(Some(&keep));
    }

    fn group_is_folded(&self, group_idx: usize) -> bool {
        self.collapse.group_folded(&self.app_group(group_idx).key)
    }

    /// The group the selected row belongs to: its own group row, or a member
    /// process row of an expanded group.
    fn selected_group(&self) -> Option<usize> {
        match self.selected_row()?.kind {
            RowKind::Group { group_idx } => Some(group_idx),
            RowKind::Process { proc_idx } => self.applications.member_group(proc_idx),
        }
    }

    /// The group the selected process row is a *top* member of, if any: collapsing the top of an
    /// application's process tree means collapsing the application.
    fn selected_top_group(&self) -> Option<usize> {
        match self.selected_row()?.kind {
            RowKind::Process { proc_idx } => self.applications.top_group(proc_idx),
            RowKind::Group { .. } => None,
        }
    }

    /// Enter / →: expand-or-collapse the selected row. A group row toggles its fold; a top
    /// member of an expanded group folds the group; any other process toggles its native collapse.
    pub fn toggle_collapse(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let (kind, has_children) = (row.kind, row.has_children());
        match kind {
            RowKind::Group { group_idx } => {
                let folded = self.group_is_folded(group_idx);
                self.set_group_folded(group_idx, !folded);
            }
            RowKind::Process { proc_idx } => {
                let pid = self.entry(proc_idx).pid;
                if self.collapse.proc_collapsed(pid) {
                    self.toggle_native(pid);
                } else if let Some(group_idx) = self.selected_top_group() {
                    self.set_group_folded(group_idx, true);
                } else if has_children {
                    self.toggle_native(pid);
                }
            }
        }
    }

    /// ←: collapse the selected row. A top member of an expanded group folds the whole group;
    /// any other process with children collapses natively.
    pub fn collapse_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let (kind, has_children) = (row.kind, row.has_children());
        match kind {
            RowKind::Group { group_idx } => self.set_group_folded(group_idx, true),
            RowKind::Process { proc_idx } => {
                if let Some(group_idx) = self.selected_top_group() {
                    self.set_group_folded(group_idx, true);
                } else if has_children {
                    let pid = self.entry(proc_idx).pid;
                    self.collapse.set_proc_collapsed(pid, true);
                    let keep = self.selected_key();
                    self.relayout(keep.as_ref());
                }
            }
        }
    }

    /// `g`: toggle the fold of the selected row's group, from the group row or any
    /// member process row of an expanded group.
    pub fn toggle_group_fold(&mut self) {
        if let Some(group_idx) = self.selected_group() {
            let folded = self.group_is_folded(group_idx);
            self.set_group_folded(group_idx, !folded);
        }
    }

    fn toggle_native(&mut self, pid: u32) {
        self.collapse.toggle_proc(pid);
        let keep = self.selected_key();
        self.relayout(keep.as_ref());
    }

    /// Signal the selected process, race-safely. Returns whether a signal was sent — the loop
    /// uses that to gather immediately so the change shows without waiting a full interval.
    #[must_use]
    pub fn kill_selected(&self) -> bool {
        let Some(row) = self.selected_row() else {
            return false;
        };
        let RowKind::Process { proc_idx } = row.kind else {
            return false;
        };
        let e = self.entry(proc_idx);
        VerifiedProcess::open(e.pid, e.start_time).is_some_and(|p| p.signal(libc::SIGTERM))
    }

    /// `N`: push the selected row's whole tree to the background (see
    /// [`VerifiedProcess::deprioritize`]). Kernel threads and oyabun itself are skipped: the
    /// first are the kernel's to schedule, the second would make the UI lag under the very load
    /// being tamed. Returns whether anything changed — the loop then gathers immediately.
    #[must_use]
    pub fn deprioritize_selected(&mut self) -> bool {
        if self.view.rows().get(self.selected).is_none() {
            return false;
        }
        let me = std::process::id();
        let mut targets = Vec::new();
        let procs = self.gatherer.procs().as_slice();
        self.view.tree_procs(self.selected, procs, |iu| {
            let e = &procs[iu as usize];
            if !e.is_kthread && e.pid != me {
                targets.push((e.pid, e.start_time));
            }
        });
        let mut tids = Vec::new();
        let mut changed = Vec::new();
        for (pid, start_time) in targets {
            if VerifiedProcess::open(pid, start_time).is_some_and(|p| p.deprioritize(&mut tids)) {
                changed.push(pid);
            }
        }
        self.gatherer.reprioritized(&changed);
        !changed.is_empty()
    }
}

/// A row identified independently of its position — a process by PID, a group by its key.
/// Used both to follow the selection across a rebuild and to name the row an action targets.
#[derive(Clone)]
enum SelectionKey {
    Process(u32),
    /// A group by key, plus its representative: where the selection goes if the group dissolves.
    Group(AppGroupKey, u32),
}

impl SelectionKey {
    /// The process this row is shown through, for finding the row that now covers it.
    fn pid(&self) -> u32 {
        match *self {
            Self::Process(pid) | Self::Group(_, pid) => pid,
        }
    }
}

fn row_matches(
    row: &ViewRow,
    keep: Option<&SelectionKey>,
    procs: &[ProcessEntry],
    groups: &ApplicationGroups,
) -> bool {
    match (row.kind, keep) {
        (RowKind::Process { proc_idx }, Some(SelectionKey::Process(pid))) => {
            procs[proc_idx].pid == *pid
        }
        (RowKind::Group { group_idx }, Some(SelectionKey::Group(key, _))) => {
            groups.groups()[group_idx].key == *key
        }
        _ => false,
    }
}
