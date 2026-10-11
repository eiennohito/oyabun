//! The flatten pass and sibling order: native tree + grouping + collapse choices become display
//! rows here, and a future sort plugs in here. Per-cycle refresh sums each row and feeds its series.

use super::collapse::Collapse;
use super::row::{RowKind, RowShape, ViewRow};
use super::series::CollapsedSeries;
use super::totals::{ProcessTotals, Totals};
use crate::application::{ApplicationGroups, MemorySampler};
use crate::gather::{CpuSeries, Gatherer};
use crate::procs::{NONE, ProcessEntry};

/// A native node waiting to be emitted, with the row of the nearest emitted ancestor.
struct Frame {
    proc_idx: u32,
    parent_row: u32,
}

/// A visible top member of a folded group (a member with no member ancestor). Each has a
/// provisional row until `resolve_group_rows` keeps the anchor's and drops the rest. Ordered by
/// group, then row, so a group's tops are one run in display order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TopMember {
    group: usize,
    row: u32,
    proc_idx: usize,
}

/// Buffers reused across rebuilds and refreshes, so a settled cycle allocates nothing.
#[derive(Default)]
struct Scratch {
    stack: Vec<Frame>,
    tops: Vec<TopMember>,
    /// Rows of non-anchor tops, to compact out.
    dropped: Vec<u32>,
    folded: Vec<bool>,
    open: Vec<Option<usize>>,
    walk: Vec<u32>,
}

/// The display tree. Rebuilt only when the native structure, the grouping, or the collapse
/// state moves; per cycle only the totals (and collapsed-row series) are refreshed.
///
/// `rows` is a heap `Vec`, not an arena buffer: it is walked **sequentially** in render, so it
/// gains nothing from the huge-page TLB win — the random walk that does is into the gatherer's
/// arena-backed process buffer. Keeping it on the heap also means a rebuild's `push` can never
/// relocate the arena and dangle the `&[ProcessEntry]` it reads from.
#[derive(Default)]
pub(crate) struct ViewTree {
    rows: Vec<ViewRow>,
    /// Pool of hidden roots; each collapsed row owns a slice of it.
    roots: Vec<u32>,
    /// Per process (aligned to the process buffer): the row that is, or whose hidden set
    /// contains, that process — `NONE` for a process inside a hidden subtree but not a root of
    /// it. Lets [`row_covering`](Self::row_covering) walk ancestors with O(1) lookups.
    cover: Vec<u32>,
    /// Per-row numbers, aligned to `rows`.
    totals: Vec<Totals>,
    own: ProcessTotals,
    series: CollapsedSeries,
    scratch: Scratch,
    /// Full recomputations of every row's numbers, for tests asserting what does not trigger one.
    #[cfg(test)]
    full_refreshes: usize,
}

impl ViewTree {
    /// Current end of the hidden-root pool. Indices fit `u32` by the process-buffer design.
    #[allow(clippy::cast_possible_truncation)]
    fn pool_end(&self) -> u32 {
        self.roots.len() as u32
    }

    pub(crate) fn rows(&self) -> &[ViewRow] {
        &self.rows
    }

    pub(crate) fn totals(&self, row: usize) -> Totals {
        self.totals[row]
    }

    #[cfg(test)]
    pub(crate) fn full_refreshes(&self) -> usize {
        self.full_refreshes
    }

    /// The history series of a collapsed row; `None` for rows with no series of their own.
    pub(crate) fn series(&self, row: usize) -> Option<CpuSeries> {
        self.series.get(row)
    }

    /// The row that shows or contains `proc_idx`: its own row, else the row of its nearest
    /// ancestor that is a row or sits in a collapsed row's hidden set (a folded group counts its
    /// top members' whole subtrees). `None` only if no ancestor is covered.
    pub(crate) fn row_covering(&self, procs: &[ProcessEntry], proc_idx: usize) -> Option<usize> {
        let mut at = u32::try_from(proc_idx).unwrap_or(NONE);
        while at != NONE {
            match self.cover.get(at as usize).copied().unwrap_or(NONE) {
                NONE => at = procs[at as usize].parent_idx,
                row => return Some(row as usize),
            }
        }
        None
    }

    /// Flatten the native tree into display rows.
    ///
    /// Native siblings come out PID-ascending (the order the gatherer's child chains carry);
    /// this is the single place sibling order is decided, so a future sort plugs in here.
    ///
    /// Collapse, uniformly: a collapsed process hides its native subtree; a folded group hides
    /// the whole native subtree of each of its *top* members (members with no member ancestor),
    /// foreign descendants included. **Ancestor wins** falls out of walking top-down and not
    /// descending into hidden subtrees: whatever sits inside an earlier hidden set is counted
    /// there, and a group forms a row only from the top members that are still outside every
    /// hidden set. The row sits at its anchor's native spot (the top member containing the
    /// representative, else the shallowest visible top); every other visible top member emits
    /// nothing at its own spot — its subtree is still counted by the group row. Expanded groups have no row of their own;
    /// members are ordinary process rows where they natively are.
    ///
    /// A row's depth is its emitted parent's plus one, assigned here and nowhere else.
    pub(crate) fn rebuild(
        &mut self,
        gatherer: &Gatherer,
        groups: &ApplicationGroups,
        collapse: &Collapse,
    ) {
        let procs = gatherer.procs().as_slice();
        let first_root = gatherer.first_root();
        self.rows.clear();
        self.roots.clear();
        let scratch = &mut self.scratch;
        scratch.tops.clear();
        scratch.dropped.clear();
        scratch.folded.clear();
        scratch.folded.extend(
            groups
                .groups()
                .iter()
                .map(|g| collapse.group_folded(&g.key)),
        );
        scratch.stack.clear();
        push_chain(procs, first_root, NONE, &mut scratch.stack);

        while let Some(Frame {
            proc_idx,
            parent_row,
        }) = self.scratch.stack.pop()
        {
            let idx = proc_idx as usize;
            let p = &procs[idx];
            let depth = match parent_row {
                NONE => 0,
                r => self.rows[r as usize].depth.saturating_add(1),
            };
            let row = u32::try_from(self.rows.len()).unwrap_or(u32::MAX);

            if let Some(group_idx) = groups.top_group(idx).filter(|&g| self.scratch.folded[g]) {
                // Provisionally a group row with nothing recorded as hidden yet;
                // `resolve_group_rows` keeps one per group and fills in its hidden tops.
                let empty = self.pool_end()..self.pool_end();
                self.rows.push(ViewRow {
                    kind: RowKind::Group { group_idx },
                    depth,
                    shape: RowShape::Collapsed { hidden: empty },
                    has_next: false,
                });
                self.scratch.tops.push(TopMember {
                    group: group_idx,
                    row,
                    proc_idx: idx,
                });
                continue;
            }

            let shape = if p.first_child == NONE {
                RowShape::Leaf
            } else if collapse.proc_collapsed(p.pid) {
                let start = self.pool_end();
                self.roots.push(proc_idx);
                RowShape::Collapsed {
                    hidden: start..start + 1,
                }
            } else {
                RowShape::Expanded
            };
            let expanded = shape == RowShape::Expanded;
            self.rows.push(ViewRow {
                kind: RowKind::Process { proc_idx: idx },
                depth,
                shape,
                has_next: false,
            });
            if expanded {
                push_chain(procs, p.first_child, row, &mut self.scratch.stack);
            }
        }

        self.resolve_group_rows(procs, groups);
        self.mark_next_siblings();
        self.index_cover(procs.len());
        self.series
            .reconcile(&self.rows, &self.roots, groups, gatherer);
    }

    /// Keep one provisional row per group (the anchor's) as the group row, give it every visible
    /// top as a hidden root, and compact the other tops' rows out.
    fn resolve_group_rows(&mut self, procs: &[ProcessEntry], groups: &ApplicationGroups) {
        // Sorted by (group, row): runs are per group, and roots land in display order.
        self.scratch.tops.sort_unstable();
        let mut start = 0;
        while start < self.scratch.tops.len() {
            let group_idx = self.scratch.tops[start].group;
            let end = start
                + self.scratch.tops[start..]
                    .iter()
                    .take_while(|t| t.group == group_idx)
                    .count();
            let run = &self.scratch.tops[start..end];
            let anchor_top = groups.groups()[group_idx].anchor_top;
            let anchor = run
                .iter()
                .find(|t| t.proc_idx == anchor_top)
                .or_else(|| {
                    run.iter()
                        .min_by_key(|t| (self.rows[t.row as usize].depth, procs[t.proc_idx].pid))
                })
                .map(|t| t.row)
                .expect("a run has at least one row");
            let hidden_start = self.pool_end();
            for t in run {
                self.roots.push(u32::try_from(t.proc_idx).unwrap_or(NONE));
                if t.row != anchor {
                    self.scratch.dropped.push(t.row);
                }
            }
            let pool_end = self.pool_end();
            self.rows[anchor as usize].shape = RowShape::Collapsed {
                hidden: hidden_start..pool_end,
            };
            start = end;
        }
        // Provisional rows have no children (their frames push none), so no kept row's parent
        // moves; depth is already stored.
        self.scratch.dropped.sort_unstable();
        let mut dropped = self.scratch.dropped.iter().copied().peekable();
        let mut row = 0u32;
        self.rows.retain(|_| {
            let gone = dropped.next_if_eq(&row).is_some();
            row += 1;
            !gone
        });
    }

    /// A row has a next sibling iff a later row sits at the same depth before the depth drops
    /// below it — one pass with the last row seen per depth.
    fn mark_next_siblings(&mut self) {
        self.scratch.open.clear();
        for idx in 0..self.rows.len() {
            let depth = self.rows[idx].depth as usize;
            if self.scratch.open.len() <= depth {
                self.scratch.open.resize(depth + 1, None);
            }
            if let Some(previous) = self.scratch.open[depth].replace(idx) {
                self.rows[previous].has_next = true;
            }
            self.scratch.open.truncate(depth + 1);
        }
    }

    /// Record, for every process that is a row or a hidden root, the row that shows it (see
    /// `cover`). Runs after row indices are final (the group pass compacts rows).
    fn index_cover(&mut self, process_count: usize) {
        self.cover.clear();
        self.cover.resize(process_count, NONE);
        for (row_idx, row) in self.rows.iter().enumerate() {
            let row_idx = u32::try_from(row_idx).unwrap_or(NONE);
            match row.kind {
                RowKind::Process { proc_idx } => self.cover[proc_idx] = row_idx,
                RowKind::Group { .. } => {
                    let hidden = row.hidden();
                    for &root in &self.roots[hidden.start as usize..hidden.end as usize] {
                        self.cover[root as usize] = row_idx;
                    }
                }
            }
        }
    }

    /// Recompute every row's numbers from the gatherer's latest cycle (and the sampler's
    /// estimates for the `viewport` processes), then record the sample on the collapsed rows'
    /// series. Idempotent within a sample. This is the pass for new data or new rows; when only
    /// memory or the viewport moved, [`refresh_visible`](Self::refresh_visible) is enough.
    pub(crate) fn refresh(
        &mut self,
        gatherer: &Gatherer,
        memory: &MemorySampler,
        viewport: &[usize],
    ) {
        #[cfg(test)]
        {
            self.full_refreshes += 1;
        }
        let procs = gatherer.procs().as_slice();
        // Without GPU process stats the live map is empty: skip the per-process lookups.
        let gpu_live = gatherer.gpu_process_sample_available();
        self.own.rebuild(procs, |pid| {
            gpu_live.then(|| gatherer.gpu_for_pid(pid)).flatten()
        });
        self.own
            .apply_estimates(|f| memory.estimates(procs, viewport, f));
        self.totals.clear();
        for row in &self.rows {
            self.totals.push(
                self.own
                    .of_row(row, &self.roots, procs, &mut self.scratch.walk),
            );
        }
        self.series.push(&self.totals, gatherer.sample_cycle());
    }

    /// Bring the numbers of the rows in `scroll..scroll + height` up to date after the
    /// sampler's estimates or the viewport changed but no new cycle arrived. Only what the
    /// estimates can change is touched: the viewport processes' memory, and the totals of the
    /// visible rows — every process a visible row sums is in `viewport`. CPU and wait come from
    /// the last [`refresh`](Self::refresh) and are unchanged, so the series needs nothing.
    /// Requires a `refresh` since the rows were last rebuilt.
    pub(crate) fn refresh_visible(
        &mut self,
        gatherer: &Gatherer,
        memory: &MemorySampler,
        viewport: &[usize],
        scroll: usize,
        height: usize,
    ) {
        let procs = gatherer.procs().as_slice();
        self.own
            .apply_estimates(|f| memory.estimates(procs, viewport, f));
        let end = scroll.saturating_add(height).min(self.rows.len());
        for idx in scroll.min(end)..end {
            self.totals[idx] =
                self.own
                    .of_row(&self.rows[idx], &self.roots, procs, &mut self.scratch.walk);
        }
    }

    /// Every process the viewport shows a number for: the visible plain rows, and everything
    /// inside the hidden sets of visible collapsed rows. These are what the memory sampler may
    /// spend its walks on.
    pub(crate) fn viewport_procs(
        &mut self,
        procs: &[ProcessEntry],
        scroll: usize,
        height: usize,
        out: &mut Vec<usize>,
    ) {
        out.clear();
        for row in self.rows.iter().skip(scroll).take(height) {
            match row.kind {
                RowKind::Process { proc_idx } if !row.is_collapsed() => out.push(proc_idx),
                RowKind::Process { .. } | RowKind::Group { .. } => {
                    let hidden = row.hidden();
                    let walk = &mut self.scratch.walk;
                    walk.clear();
                    walk.extend_from_slice(&self.roots[hidden.start as usize..hidden.end as usize]);
                    while let Some(iu) = walk.pop() {
                        out.push(iu as usize);
                        push_siblings(procs, procs[iu as usize].first_child, walk);
                    }
                }
            }
        }
    }
}
/// Push a native sibling chain so a LIFO stack pops it PID-ascending.
fn push_chain(procs: &[ProcessEntry], head: u32, parent_row: u32, stack: &mut Vec<Frame>) {
    let base = stack.len();
    let mut c = head;
    while c != NONE {
        stack.push(Frame {
            proc_idx: c,
            parent_row,
        });
        c = procs[c as usize].next_sibling;
    }
    stack[base..].reverse();
}

/// Push a sibling chain in any order (the order is irrelevant to a set walk).
fn push_siblings(procs: &[ProcessEntry], head: u32, stack: &mut Vec<u32>) {
    let mut c = head;
    while c != NONE {
        stack.push(c);
        c = procs[c as usize].next_sibling;
    }
}
