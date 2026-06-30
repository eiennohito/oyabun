//! Application state for the single-threaded loop: it owns the [`Gatherer`] (which produces
//! the live process buffer) plus the view state — collapse set, selection/scroll, and the
//! cached flattened display list. [`App::gather`] runs one gather cycle then rebuilds the
//! display list; render reads everything back through `&self`. The borrow checker proves the
//! gather (`&mut`) and the render (`&`) never overlap, so there is no snapshot exchange.

use std::collections::{HashMap, HashSet};

use crate::gather::Gatherer;
use crate::procs::{NONE, ProcessEntry, Procs, SystemStats};
use crate::sys::{self, ProcDir};

pub struct DisplayRow {
    pub proc_idx: usize,
    pub depth: u16,
    pub collapsed: bool,
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
    collapsed: HashSet<u32>,
    uid_names: HashMap<u32, Box<str>>,
    selected: usize,
    scroll: usize,
    /// Reused DFS stack for flattening the tree into `rows`.
    row_scratch: Vec<u32>,
}

impl App {
    pub fn new(page_size: u64, proc_dir: ProcDir, uid_names: HashMap<u32, Box<str>>) -> Self {
        Self {
            gatherer: Gatherer::new(page_size, proc_dir),
            rows: Vec::new(),
            collapsed: HashSet::new(),
            uid_names,
            selected: 0,
            scroll: 0,
            row_scratch: Vec::new(),
        }
    }

    /// Run one gather cycle, then rebuild the display list (selection follows its PID).
    pub fn gather(&mut self) {
        let keep = self.selected_pid();
        self.gatherer.cycle();
        self.rebuild_rows(keep);
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

    fn selected_pid(&self) -> Option<u32> {
        self.rows
            .get(self.selected)
            .map(|r| self.entry(r.proc_idx).pid)
    }

    /// Flatten the tree into display order, skipping collapsed subtrees. Restores selection
    /// onto `keep_pid` if present (selection-follow across refreshes), and anchors scroll so the
    /// selected row stays at the same screen line — minimizing visible row movement from
    /// births/deaths outside the viewport. Reads the arena-backed process buffer; `rows` is a
    /// heap `Vec`, so its growth never relocates that buffer.
    fn rebuild_rows(&mut self, keep_pid: Option<u32>) {
        let anchor_offset = self.selected.saturating_sub(self.scroll);
        let mut pid_survived = false;
        {
            let App {
                gatherer,
                rows,
                row_scratch,
                collapsed,
                selected,
                ..
            } = self;
            let procs = gatherer.procs().as_slice();
            rows.clear();
            row_scratch.clear();
            push_children(procs, gatherer.first_root(), row_scratch);

            // Record the kept PID's new row index during the walk (no second scan).
            let mut found = None;
            while let Some(iu) = row_scratch.pop() {
                let p = &procs[iu as usize];
                let (pid, depth, first_child) = (p.pid, p.depth, p.first_child);
                let is_collapsed = collapsed.contains(&pid);
                if keep_pid == Some(pid) {
                    found = Some(rows.len());
                }
                rows.push(DisplayRow {
                    proc_idx: iu as usize,
                    depth,
                    collapsed: is_collapsed,
                });
                if !is_collapsed {
                    push_children(procs, first_child, row_scratch);
                }
            }

            if let Some(idx) = found {
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

    // --- actions ---

    fn selected_has_children(&self) -> Option<(u32, bool)> {
        let row = self.rows.get(self.selected)?;
        let p = self.entry(row.proc_idx);
        Some((p.pid, p.first_child != NONE))
    }

    pub fn toggle_collapse(&mut self) {
        let Some((pid, has_children)) = self.selected_has_children() else {
            return;
        };
        if !has_children {
            return;
        }
        if !self.collapsed.remove(&pid) {
            self.collapsed.insert(pid);
        }
        let keep = self.selected_pid();
        self.rebuild_rows(keep);
    }

    pub fn collapse_selected(&mut self) {
        let Some((pid, has_children)) = self.selected_has_children() else {
            return;
        };
        if has_children && self.collapsed.insert(pid) {
            let keep = self.selected_pid();
            self.rebuild_rows(keep);
        }
    }

    /// Signal the selected process, race-safely. Returns whether a signal was sent — the loop
    /// uses that to gather immediately so the change shows without waiting a full interval.
    #[must_use]
    pub fn kill_selected(&self) -> bool {
        let Some(row) = self.rows.get(self.selected) else {
            return false;
        };
        let e = self.entry(row.proc_idx);
        // Only signals if (pid, start_time) still identify this exact process — a reused PID is
        // never hit.
        sys::kill_verified(e.pid, e.start_time, libc::SIGTERM)
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
