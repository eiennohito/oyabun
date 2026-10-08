//! Application state for the single-threaded loop: it owns the [`Gatherer`] (which produces
//! the live process buffer) plus the view state — collapse set, selection/scroll, and the
//! cached flattened display list. [`App::gather`] runs one gather cycle then rebuilds the
//! display list; render reads everything back through `&self`. The borrow checker proves the
//! gather (`&mut`) and the render (`&`) never overlap, so there is no snapshot exchange.

use crate::application::{
    AppGroup, AppGroupKey, ApplicationGroups, DesktopResolver, FoldPreferences, MemorySampler,
};
use crate::fxhash::{FxMap, PidMap};
use crate::gather::Gatherer;
use crate::procs::{GpuMetrics, NONE, ProcessEntry, Procs, SystemStats};
use crate::sys::{self, ProcDir};

#[derive(Clone, Debug)]
pub enum DisplayRowKind {
    Process { proc_idx: usize },
    Application { group_idx: usize },
}

pub struct DisplayRow {
    pub kind: DisplayRowKind,
    pub depth: u16,
    pub collapsed: bool,
    pub has_children: bool,
    pub has_next: bool,
}

pub struct App {
    /// Produces the live process buffer; owns the THP arena + per-PID stores + I/O backend.
    gatherer: Gatherer,
    /// Flattened display list (tree order, collapsed subtrees skipped). A `Vec`, not an arena
    /// buffer: it is walked **sequentially** in render, so it gains nothing from the huge-page
    /// TLB win — the random walk that does is into `gatherer.procs()`, which *is* arena-backed.
    /// Keeping it on the heap also means a rebuild's `push` can never relocate the arena and
    /// dangle the `&[ProcessEntry]` it reads from.
    rows: Vec<DisplayRow>,
    collapse: PidMap<CollapseState>,
    application_collapse: FxMap<AppGroupKey, AppCollapseState>,
    /// Persisted, per-identity fold overrides — a folded application returns folded next run.
    folds: FoldPreferences,
    applications: ApplicationGroups,
    desktop: DesktopResolver,
    memory: MemorySampler,
    uid_names: FxMap<u32, Box<str>>,
    selected: usize,
    scroll: usize,
    /// Reused DFS stack for flattening the tree into `rows`.
    row_scratch: Vec<u32>,
    /// Reused list of visible collapsed application group indices, refilled each frame for the
    /// memory sampler — avoids a per-frame allocation on the render hot path.
    visible_scratch: Vec<usize>,
}

impl App {
    pub fn new(page_size: u64, proc_dir: ProcDir, uid_names: FxMap<u32, Box<str>>) -> Self {
        Self {
            gatherer: Gatherer::new(page_size, proc_dir),
            rows: Vec::new(),
            collapse: PidMap::default(),
            application_collapse: FxMap::default(),
            folds: FoldPreferences::system(),
            applications: ApplicationGroups::default(),
            desktop: DesktopResolver::system(),
            memory: MemorySampler::system(),
            uid_names,
            selected: 0,
            scroll: 0,
            row_scratch: Vec::new(),
            visible_scratch: Vec::new(),
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
        Self {
            gatherer,
            rows: Vec::new(),
            collapse: PidMap::default(),
            application_collapse: FxMap::default(),
            folds: FoldPreferences::disabled(),
            applications: ApplicationGroups::default(),
            desktop,
            memory: MemorySampler::system(),
            uid_names,
            selected: 0,
            scroll: 0,
            row_scratch: Vec::new(),
            visible_scratch: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_folds(&mut self, folds: FoldPreferences) {
        self.folds = folds;
    }

    /// Run one gather cycle, then rebuild the display list (selection follows its PID).
    pub fn gather(&mut self) {
        let keep = self.selected_key();
        self.gatherer.cycle();
        self.rebuild_application_groups();
        self.rebuild_rows(keep.as_ref());
    }

    // --- views for render ---

    pub fn sys(&self) -> &SystemStats {
        self.gatherer.sys()
    }

    pub fn procs(&self) -> &Procs {
        self.gatherer.procs()
    }

    pub fn rows(&self) -> &[DisplayRow] {
        &self.rows
    }

    pub(crate) fn app_group(&self, group_idx: usize) -> &AppGroup {
        &self.applications.groups()[group_idx]
    }

    #[cfg(test)]
    pub(crate) fn row_pid(&self, row: &DisplayRow) -> u32 {
        match row.kind {
            DisplayRowKind::Process { proc_idx } => self.entry(proc_idx).pid,
            DisplayRowKind::Application { group_idx } => {
                self.entry(self.app_group(group_idx).representative).pid
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn memory_read_count(&self) -> usize {
        self.memory.read_count()
    }

    #[cfg(test)]
    pub(crate) fn meta_epoch(&self) -> u64 {
        self.gatherer.meta_epoch()
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

    pub fn gpu_for_pid(&self, pid: u32) -> Option<GpuMetrics> {
        self.gatherer.gpu_for_pid(pid)
    }

    pub fn subtree_gpu_for_pid(&self, pid: u32) -> Option<GpuMetrics> {
        self.gatherer.subtree_gpu_for_pid(pid)
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
        match &self.rows.get(self.selected)?.kind {
            DisplayRowKind::Process { proc_idx } => {
                Some(SelectionKey::Process(self.entry(*proc_idx).pid))
            }
            DisplayRowKind::Application { group_idx } => Some(SelectionKey::Application(
                self.app_group(*group_idx).key.clone(),
            )),
        }
    }

    fn rebuild_application_groups(&mut self) {
        let App {
            gatherer,
            applications,
            desktop,
            memory,
            ..
        } = self;
        let procs = gatherer.procs().as_slice();
        let identities = (0..procs.len())
            .filter_map(|idx| gatherer.identity(idx).map(|identity| (idx, identity)));
        applications.rebuild(
            procs,
            identities,
            desktop,
            |pid| gatherer.gpu_for_pid(pid),
            gatherer.meta_epoch(),
        );
        // Reset the per-cycle proportional-read budget and evict cached members of dead/reused
        // PIDs. The resident-set check then reads only what actually moved, when a group is
        // visible and collapsed (`prepare_visible_rows`).
        memory.begin_cycle(gatherer.generation(), procs);
    }

    /// Flatten the tree into display order, skipping collapsed subtrees. Restores selection
    /// onto `keep_pid` if present (selection-follow across refreshes), and anchors scroll so the
    /// selected row stays at the same screen line — minimizing visible row movement from
    /// births/deaths outside the viewport. Reads the arena-backed process buffer; `rows` is a
    /// heap `Vec`, so its growth never relocates that buffer.
    fn rebuild_rows(&mut self, keep: Option<&SelectionKey>) {
        let anchor_offset = self.selected.saturating_sub(self.scroll);
        let mut pid_survived = false;
        {
            let App {
                gatherer,
                rows,
                row_scratch,
                collapse,
                application_collapse,
                folds,
                applications,
                selected,
                ..
            } = self;
            let procs = gatherer.procs().as_slice();
            // Native collapse is transient UI state keyed by PID: manual per-process folds plus
            // the one built-in auto-fold of the kernel-thread forest. Semantic grouping is handled
            // separately by application rows (`application_collapse`), not here. If a PID is absent
            // from a gathered frame its UI state is gone; PID reuse within one gather interval may
            // inherit collapse state, which is acceptable for display state and signal-safe.
            collapse.retain(|pid, _| procs.binary_search_by(|p| p.pid.cmp(pid)).is_ok());
            collapse.retain(|_, state| *state != CollapseState::Auto);
            if let Some(kthreadd) = procs
                .iter()
                .find(|proc| proc.comm() == b"kthreadd" && proc.first_child != NONE)
            {
                collapse.entry(kthreadd.pid).or_insert(CollapseState::Auto);
            }

            // Prune collapse state for vanished groups, then seed any new group's default. Both
            // walk the (handful of) live groups directly rather than materializing a key set, and
            // clone a key only on the miss path — so a steady cycle (groups unchanged) allocates
            // nothing here.
            application_collapse
                .retain(|key, _| applications.groups().iter().any(|group| &group.key == key));
            // A group starts folded (the default) unless the user has a persisted expansion
            // override for its stable identity — then it returns expanded next run.
            for group in applications.groups() {
                if !application_collapse.contains_key(&group.key) {
                    let expanded = group
                        .persist_key
                        .as_deref()
                        .is_some_and(|key| folds.is_expanded(key));
                    let state = if expanded {
                        AppCollapseState::Suppressed
                    } else {
                        AppCollapseState::Auto
                    };
                    application_collapse.insert(group.key.clone(), state);
                }
            }

            rows.clear();
            row_scratch.clear();
            push_children(procs, gatherer.first_root(), row_scratch);

            let mut builder = RowBuilder::new(procs, applications, application_collapse, collapse);
            while let Some(iu) = row_scratch.pop() {
                builder.project_native(iu as usize, procs[iu as usize].depth, rows);
            }
            mark_next_siblings(rows);
            if let Some(idx) = rows
                .iter()
                .position(|row| row_matches(row, keep, procs, applications))
            {
                *selected = idx;
                pid_survived = true;
            }
        }
        self.clamp_selection();
        if pid_survived {
            self.scroll = self.selected.saturating_sub(anchor_offset);
        }
    }

    // --- navigation ---

    fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
            self.scroll = 0;
        } else {
            self.selected = self.selected.min(self.rows.len() - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + 1).min(self.rows.len() - 1);
        }
    }

    pub fn page_up(&mut self, page: usize) {
        self.selected = self.selected.saturating_sub(page);
    }

    pub fn page_down(&mut self, page: usize) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + page).min(self.rows.len() - 1);
        }
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.rows.len().saturating_sub(1);
    }

    #[cfg(test)]
    pub(crate) fn select_row(&mut self, row: usize) {
        if !self.rows.is_empty() {
            self.selected = row.min(self.rows.len() - 1);
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
    /// stays pure I/O. Only collapsed application rows in the viewport are candidates; the sampler
    /// estimates each member's proportional set size from a cached split and the free resident
    /// reading, walking the address space only to bootstrap a member or on a staggered backstop
    /// under a per-cycle budget, so this is cheap to call every render frame (a settled desktop
    /// walks nothing).
    pub fn prepare_visible_rows(&mut self, visible_height: usize) {
        let mut visible = std::mem::take(&mut self.visible_scratch);
        visible.clear();
        visible.extend(
            self.rows
                .iter()
                .skip(self.scroll)
                .take(visible_height)
                .filter_map(|row| match row.kind {
                    DisplayRowKind::Application { group_idx } if row.collapsed => Some(group_idx),
                    _ => None,
                }),
        );
        if !visible.is_empty() {
            let App {
                gatherer,
                applications,
                memory,
                ..
            } = self;
            let procs = gatherer.procs().as_slice();
            memory.sample_visible(applications.groups_mut(), &visible, procs);
        }
        self.visible_scratch = visible;
    }

    // --- actions ---

    fn selected_has_children(&self) -> Option<(SelectionKey, bool)> {
        let row = self.rows.get(self.selected)?;
        let selected = match row.kind {
            DisplayRowKind::Process { proc_idx } => SelectionKey::Process(self.entry(proc_idx).pid),
            DisplayRowKind::Application { group_idx } => {
                SelectionKey::Application(self.app_group(group_idx).key.clone())
            }
        };
        Some((selected, row.has_children))
    }

    pub fn toggle_collapse(&mut self) {
        let Some((selected, has_children)) = self.selected_has_children() else {
            return;
        };
        if !has_children {
            return;
        }
        match selected {
            SelectionKey::Application(key) => {
                let expanded = {
                    let state = self
                        .application_collapse
                        .entry(key.clone())
                        .or_insert(AppCollapseState::Auto);
                    *state = match *state {
                        AppCollapseState::Auto => AppCollapseState::Suppressed,
                        AppCollapseState::Suppressed => AppCollapseState::Auto,
                    };
                    *state == AppCollapseState::Suppressed
                };
                self.persist_fold(&key, expanded);
            }
            SelectionKey::Process(pid) => match self.collapse.get(&pid).copied() {
                Some(CollapseState::Auto) => {
                    self.collapse.insert(pid, CollapseState::AutoSuppressed);
                }
                Some(CollapseState::Manual) => {
                    self.collapse.remove(&pid);
                }
                _ => {
                    self.collapse.insert(pid, CollapseState::Manual);
                }
            },
        }
        let keep = self.selected_key();
        self.rebuild_rows(keep.as_ref());
    }

    pub fn collapse_selected(&mut self) {
        let Some((selected, has_children)) = self.selected_has_children() else {
            return;
        };
        if has_children {
            match selected {
                SelectionKey::Process(pid) => {
                    self.collapse.insert(pid, CollapseState::Manual);
                }
                SelectionKey::Application(key) => {
                    self.application_collapse
                        .insert(key.clone(), AppCollapseState::Auto);
                    self.persist_fold(&key, false);
                }
            }
            let keep = self.selected_key();
            self.rebuild_rows(keep.as_ref());
        }
    }

    /// Record the user's fold choice for a group whose identity is stable enough to persist. A
    /// group with no persistable identity (container/pod/structural) is silently session-only.
    fn persist_fold(&mut self, key: &AppGroupKey, expanded: bool) {
        if let Some(persist_key) = self
            .applications
            .groups()
            .iter()
            .find(|group| &group.key == key)
            .and_then(|group| group.persist_key.clone())
        {
            self.folds.set_expanded(&persist_key, expanded);
        }
    }

    /// Signal the selected process, race-safely. Returns whether a signal was sent — the loop
    /// uses that to gather immediately so the change shows without waiting a full interval.
    #[must_use]
    pub fn kill_selected(&self) -> bool {
        let Some(row) = self.rows.get(self.selected) else {
            return false;
        };
        let DisplayRowKind::Process { proc_idx } = row.kind else {
            return false;
        };
        let e = self.entry(proc_idx);
        // Only signals if (pid, start_time) still identify this exact process — a reused PID is
        // never hit.
        sys::kill_verified(e.pid, e.start_time, libc::SIGTERM)
    }
}

/// A row identified independently of its position — a process by PID, or an application group by
/// its key. Used both to follow the selection across a refresh and to name the row an action
/// targets.
#[derive(Clone)]
enum SelectionKey {
    Process(u32),
    Application(AppGroupKey),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppCollapseState {
    Auto,
    Suppressed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CollapseState {
    Manual,
    Auto,
    AutoSuppressed,
}

impl CollapseState {
    fn is_collapsed(self) -> bool {
        matches!(self, Self::Manual | Self::Auto)
    }
}

/// The shared context for projecting the process tree into the flattened display list — the read
/// views (processes, groups, both collapse maps) plus the per-group "already emitted" marks. The
/// mutually recursive projection walks are methods on it so the context is named once, not
/// threaded through every call; the output `rows` is passed per call so its `&mut` never outlives
/// the walk.
struct RowBuilder<'a> {
    procs: &'a [ProcessEntry],
    groups: &'a ApplicationGroups,
    app_collapse: &'a FxMap<AppGroupKey, AppCollapseState>,
    collapse: &'a PidMap<CollapseState>,
    /// One flag per group: an application row is emitted once, at its representative.
    inserted: Vec<bool>,
}

impl<'a> RowBuilder<'a> {
    fn new(
        procs: &'a [ProcessEntry],
        groups: &'a ApplicationGroups,
        app_collapse: &'a FxMap<AppGroupKey, AppCollapseState>,
        collapse: &'a PidMap<CollapseState>,
    ) -> Self {
        Self {
            procs,
            groups,
            app_collapse,
            collapse,
            inserted: vec![false; groups.groups().len()],
        }
    }

    /// Project a native tree node (and its subtree) into `rows`. A member of an application group
    /// emits the group's row once, at its representative, then descends into native children;
    /// everything else emits a process row honouring its manual/auto collapse.
    fn project_native(&mut self, proc_idx: usize, depth: u16, rows: &mut Vec<DisplayRow>) {
        if let Some(group_idx) = self.groups.member_group(proc_idx) {
            let group = &self.groups.groups()[group_idx];
            if proc_idx == group.representative && !self.inserted[group_idx] {
                self.inserted[group_idx] = true;
                let collapsed = self
                    .app_collapse
                    .get(&group.key)
                    .is_none_or(|state| *state == AppCollapseState::Auto);
                rows.push(DisplayRow {
                    kind: DisplayRowKind::Application { group_idx },
                    depth,
                    collapsed,
                    has_children: true,
                    has_next: false,
                });
                if !collapsed {
                    self.append_member_forest(group_idx, depth.saturating_add(1), rows);
                }
            }
            let mut child = self.procs[proc_idx].first_child;
            while child != NONE {
                self.project_native(child as usize, depth, rows);
                child = self.procs[child as usize].next_sibling;
            }
            return;
        }

        let proc = &self.procs[proc_idx];
        let collapsed = self
            .collapse
            .get(&proc.pid)
            .is_some_and(|state| state.is_collapsed());
        rows.push(DisplayRow {
            kind: DisplayRowKind::Process { proc_idx },
            depth,
            collapsed,
            has_children: proc.first_child != NONE,
            has_next: false,
        });
        if collapsed {
            return;
        }
        let mut child = proc.first_child;
        while child != NONE {
            self.project_native(child as usize, depth.saturating_add(1), rows);
            child = self.procs[child as usize].next_sibling;
        }
    }

    /// Emit an expanded group's members, starting each subtree whose parent is outside the group
    /// (a member whose parent is also in the group is reached by recursion instead).
    fn append_member_forest(&self, group_idx: usize, depth: u16, rows: &mut Vec<DisplayRow>) {
        let group = &self.groups.groups()[group_idx];
        for &member in &group.members {
            let parent_in_group = self.procs[member].parent_idx != NONE
                && self
                    .groups
                    .member_group(self.procs[member].parent_idx as usize)
                    == Some(group_idx);
            if !parent_in_group {
                self.append_member(member, group_idx, depth, rows);
            }
        }
    }

    fn append_member(
        &self,
        proc_idx: usize,
        group_idx: usize,
        depth: u16,
        rows: &mut Vec<DisplayRow>,
    ) {
        let proc = &self.procs[proc_idx];
        let mut member_children = Vec::new();
        let mut child = proc.first_child;
        while child != NONE {
            if self.groups.member_group(child as usize) == Some(group_idx) {
                member_children.push(child as usize);
            }
            child = self.procs[child as usize].next_sibling;
        }
        // A virtual application is already the outer automatic boundary. Inside its expanded
        // member forest only an explicit manual collapse is meaningful; nested auto-groups would
        // make expansion reveal another hidden layer.
        let collapsed = self.collapse.get(&proc.pid) == Some(&CollapseState::Manual);
        rows.push(DisplayRow {
            kind: DisplayRowKind::Process { proc_idx },
            depth,
            collapsed,
            has_children: !member_children.is_empty(),
            has_next: false,
        });
        if !collapsed {
            for child in member_children {
                self.append_member(child, group_idx, depth.saturating_add(1), rows);
            }
        }
    }
}

fn mark_next_siblings(rows: &mut [DisplayRow]) {
    let mut open: Vec<Option<usize>> = Vec::new();
    for idx in 0..rows.len() {
        let depth = rows[idx].depth as usize;
        if open.len() <= depth {
            open.resize(depth + 1, None);
        }
        if let Some(previous) = open[depth].replace(idx) {
            rows[previous].has_next = true;
        }
        open.truncate(depth + 1);
    }
}

fn row_matches(
    row: &DisplayRow,
    keep: Option<&SelectionKey>,
    procs: &[ProcessEntry],
    groups: &ApplicationGroups,
) -> bool {
    match (&row.kind, keep) {
        (DisplayRowKind::Process { proc_idx }, Some(SelectionKey::Process(pid))) => {
            procs[*proc_idx].pid == *pid
        }
        (DisplayRowKind::Application { group_idx }, Some(SelectionKey::Application(key))) => {
            groups.groups()[*group_idx].key == *key
        }
        _ => false,
    }
}

/// Push a sibling chain reversed, so a LIFO stack pops it in PID-ascending order.
fn push_children(procs: &[ProcessEntry], head: u32, stack: &mut Vec<u32>) {
    let base = stack.len();
    let mut c = head;
    while c != NONE {
        stack.push(c);
        c = procs[c as usize].next_sibling;
    }
    stack[base..].reverse();
}
