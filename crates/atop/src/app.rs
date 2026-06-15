//! UI-thread state: the displayed snapshot, collapse set, selection/scroll, and the
//! cached flattened display list (rebuilt only when the snapshot or collapse state
//! changes — pure scrolling does no work).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use arc_swap::ArcSwap;

use crate::gather::Ctrl;
use crate::snapshot::{NONE, ProcessEntry, Snapshot};
use crate::sys;

pub struct DisplayRow {
    pub proc_idx: usize,
    pub depth: u16,
    pub collapsed: bool,
}

pub struct App {
    cell: Arc<ArcSwap<Snapshot>>,
    snapshot: Arc<Snapshot>,
    rows: Vec<DisplayRow>,
    collapsed: HashSet<u32>,
    uid_names: HashMap<u32, Box<str>>,
    selected: usize,
    scroll: usize,
    last_gen: u64,
    ctrl: Sender<Ctrl>,
    /// Reused DFS stack for flattening the tree into `rows`.
    row_scratch: Vec<u32>,
}

impl App {
    pub fn new(
        cell: Arc<ArcSwap<Snapshot>>,
        ctrl: Sender<Ctrl>,
        uid_names: HashMap<u32, Box<str>>,
    ) -> Self {
        let snapshot = cell.load_full();
        let mut app = Self {
            cell,
            last_gen: snapshot.generation,
            snapshot,
            rows: Vec::new(),
            collapsed: HashSet::new(),
            uid_names,
            selected: 0,
            scroll: 0,
            ctrl,
            row_scratch: Vec::new(),
        };
        app.rebuild_rows(None);
        app
    }

    // --- snapshot view ---

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn rows(&self) -> &[DisplayRow] {
        &self.rows
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

    fn entry(&self, proc_idx: usize) -> &ProcessEntry {
        &self.snapshot.procs[proc_idx]
    }

    fn selected_pid(&self) -> Option<u32> {
        self.rows
            .get(self.selected)
            .map(|r| self.entry(r.proc_idx).pid)
    }

    /// Pick up a newer snapshot if the gatherer published one. Returns whether the
    /// view changed (caller redraws).
    pub fn refresh_view(&mut self) -> bool {
        let snap = self.cell.load_full();
        if snap.generation == self.last_gen {
            return false;
        }
        let keep = self.selected_pid(); // resolve against the *current* snapshot
        self.snapshot = snap;
        self.last_gen = self.snapshot.generation;
        self.rebuild_rows(keep);
        true
    }

    /// Flatten the tree into display order, skipping collapsed subtrees. Restores
    /// selection onto `keep_pid` if present (selection-follow across refreshes).
    fn rebuild_rows(&mut self, keep_pid: Option<u32>) {
        self.rows.clear();
        self.row_scratch.clear();
        push_children(
            &self.snapshot,
            self.snapshot.first_root,
            &mut self.row_scratch,
        );

        // Record the kept PID's new row index during the walk (no second scan).
        let mut found = None;
        while let Some(iu) = self.row_scratch.pop() {
            let p = &self.snapshot.procs[iu as usize];
            let (pid, depth, first_child) = (p.pid, p.depth, p.first_child);
            let collapsed = self.collapsed.contains(&pid);
            if keep_pid == Some(pid) {
                found = Some(self.rows.len());
            }
            self.rows.push(DisplayRow {
                proc_idx: iu as usize,
                depth,
                collapsed,
            });
            if !collapsed {
                push_children(&self.snapshot, first_child, &mut self.row_scratch);
            }
        }

        if let Some(idx) = found {
            self.selected = idx;
        }
        self.clamp_selection();
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

    pub fn kill_selected(&self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let e = self.entry(row.proc_idx);
        // Race-safe: only signals if (pid, start_time) still identify this exact
        // process — a reused PID is never hit.
        if sys::kill_verified(e.pid, e.start_time, libc::SIGTERM) {
            // Nudge the gatherer so the change shows without waiting a full interval.
            let _ = self.ctrl.send(Ctrl::Refresh);
        }
    }
}

/// Push a sibling chain reversed, so a LIFO stack pops it in PID-ascending order.
fn push_children(snap: &Snapshot, head: u32, stack: &mut Vec<u32>) {
    let base = stack.len();
    let mut c = head;
    while c != NONE {
        stack.push(c);
        c = snap.procs[c as usize].next_sibling;
    }
    stack[base..].reverse();
}
