//! CPU history of collapsed rows.
//!
//! A collapsed row's sparkline must equal the sum of the series of everything it hides — one
//! process's history can't stand for several. Keeping that sum costs O(rows) per sample, not
//! O(hidden): each cycle the row's current-interval cell (rate and scheduler wait, already
//! summed by the totals pass) is pushed onto a short ring. Wait and run are kept as separate
//! sums, so the row's wait share is Σwait / Σ(wait + run) over the whole hidden set — never an
//! average of members' shares. When the *set of hidden roots* changes (a new row, or a group
//! gaining/losing a top member) the ring is edited, not rebuilt: roots newly hidden add their
//! series, roots that left but still live subtract theirs, and roots that died keep whatever
//! they contributed — their past is the row's past. A build spawning short-lived compilers
//! must not zero the `make` row's history, nor a dying top member the application's.
//!
//! Rows are keyed by identity (process incarnation or group key), so a ring survives the
//! rebuilds that move rows around, and is dropped when its row disappears. Everything is
//! persistent and looked up by borrowed key, so a rebuild that changes nothing allocates nothing.

use super::row::{RowKind, ViewRow};
use super::totals::Totals;
use crate::application::{AppGroupKey, ApplicationGroups};
use crate::fxhash::FxMap;
use crate::gather::{CpuSeries, Gatherer, SPARK_CELLS, SeriesCell};
use crate::procs::{NONE, ProcessEntry};

/// A process incarnation: a reused PID has a different start time.
type Incarnation = (u32, u64);

fn incarnation(p: &ProcessEntry) -> Incarnation {
    (p.pid, p.start_time)
}

/// The identity of a row that owns a ring, borrowing the group key.
#[derive(Clone, Copy)]
enum RowRef<'a> {
    Process(Incarnation),
    Group(&'a AppGroupKey),
}

impl RowRef<'_> {
    fn of<'a>(
        row: &ViewRow,
        procs: &[ProcessEntry],
        groups: &'a ApplicationGroups,
    ) -> Option<RowRef<'a>> {
        match row.kind {
            RowKind::Process { proc_idx } if row.is_collapsed() => {
                Some(RowRef::Process(incarnation(&procs[proc_idx])))
            }
            RowKind::Group { group_idx } => Some(RowRef::Group(&groups.groups()[group_idx].key)),
            RowKind::Process { .. } => None,
        }
    }
}

/// The recent per-interval cells of a collapsed row, aligned to the sample-cycle clock the
/// per-process series use.
struct Ring {
    cells: CpuSeries,
    /// Sample cycle of the newest cell.
    cycle: u64,
}

impl Ring {
    /// Set the cell of `cycle`: a later cycle scrolls (zero-filling any sample cycles missed
    /// since the last record), the current one overwrites its cell, so refreshing twice within
    /// one sample — or after the roots changed — is harmless.
    fn record(&mut self, cell: SeriesCell, cycle: u64) {
        match cycle.cmp(&self.cycle) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => self.cells.0[SPARK_CELLS - 1] = cell,
            std::cmp::Ordering::Greater => {
                self.advance(cycle);
                self.cells.0[SPARK_CELLS - 1] = cell;
            }
        }
    }

    /// Scroll to a newer `cycle`, leaving zeros for every cycle in between and for the new one.
    fn advance(&mut self, cycle: u64) {
        let steps = usize::try_from(cycle.saturating_sub(self.cycle)).unwrap_or(usize::MAX);
        let steps = steps.min(SPARK_CELLS);
        self.cells.0.rotate_left(steps);
        let from = SPARK_CELLS - steps;
        self.cells.0[from..].fill(SeriesCell::default());
        self.cycle = self.cycle.max(cycle);
    }
}

struct Slot {
    id: OwnedId,
    /// The hidden roots the ring currently accounts for.
    roots: Vec<Incarnation>,
    ring: Ring,
    /// The reconcile pass that last saw the row.
    mark: u64,
}

enum OwnedId {
    Process(Incarnation),
    Group(AppGroupKey),
}

/// One optional ring per display row (`Some` for collapsed rows), aligned to the rows.
#[derive(Default)]
pub(super) struct CollapsedSeries {
    slots: Vec<Slot>,
    by_process: FxMap<Incarnation, usize>,
    by_group: FxMap<AppGroupKey, usize>,
    /// Per display row: index into `slots`, or `NONE`.
    row_slot: Vec<u32>,
    pass: u64,
    walk: Vec<u32>,
    /// Scratch of [`edit`](CollapsedSeries::edit): per process, which side(s) it is under.
    /// All zero between edits, grown to the process count on first need.
    mask: Vec<u8>,
    /// The processes `edit` set in `mask`, so it can zero exactly those afterwards.
    touched: Vec<u32>,
}

impl CollapsedSeries {
    fn find(&self, id: RowRef<'_>) -> Option<usize> {
        match id {
            RowRef::Process(inc) => self.by_process.get(&inc).copied(),
            RowRef::Group(key) => self.by_group.get(key).copied(),
        }
    }

    /// Realign to a rebuilt set of rows: a row with a known identity keeps its ring (edited if
    /// its hidden roots changed), a new one starts as the sum of its members' series, and rings
    /// of vanished rows are dropped. The newest cell is not maintained here — the next
    /// [`push`](Self::push) sets it from the row's totals.
    pub(super) fn reconcile(
        &mut self,
        rows: &[ViewRow],
        roots: &[u32],
        groups: &ApplicationGroups,
        gatherer: &Gatherer,
    ) {
        let procs = gatherer.procs().as_slice();
        let cycle = gatherer.sample_cycle();
        self.pass += 1;
        for row in rows {
            let Some(id) = RowRef::of(row, procs, groups) else {
                continue;
            };
            let range = row.hidden();
            let hidden = &roots[range.start as usize..range.end as usize];
            match self.find(id) {
                Some(ix) => {
                    self.slots[ix].mark = self.pass;
                    self.edit(ix, hidden, procs, gatherer, cycle);
                }
                None => self.insert(id, hidden, procs, gatherer, cycle),
            }
        }
        if self.slots.iter().any(|slot| slot.mark != self.pass) {
            self.sweep();
        }
        let mut row_slot = std::mem::take(&mut self.row_slot);
        row_slot.clear();
        row_slot.extend(rows.iter().map(|row| {
            RowRef::of(row, procs, groups)
                .and_then(|id| self.find(id))
                .map_or(NONE, |ix| u32::try_from(ix).unwrap_or(NONE))
        }));
        self.row_slot = row_slot;
    }

    fn insert(
        &mut self,
        id: RowRef<'_>,
        hidden: &[u32],
        procs: &[ProcessEntry],
        gatherer: &Gatherer,
        cycle: u64,
    ) {
        let mut sum = [SeriesCell::default(); SPARK_CELLS];
        for &root in hidden {
            walk_subtree(&mut self.walk, procs, root as usize, |i| {
                if let Some(series) = gatherer.cpu_series(procs[i].pid) {
                    for (acc, cell) in sum.iter_mut().zip(series.0) {
                        acc.add(cell);
                    }
                }
            });
        }
        let ix = self.slots.len();
        let id = match id {
            RowRef::Process(inc) => {
                self.by_process.insert(inc, ix);
                OwnedId::Process(inc)
            }
            RowRef::Group(key) => {
                self.by_group.insert(key.clone(), ix);
                OwnedId::Group(key.clone())
            }
        };
        self.slots.push(Slot {
            id,
            roots: hidden
                .iter()
                .map(|&r| incarnation(&procs[r as usize]))
                .collect(),
            ring: Ring {
                cells: CpuSeries(sum),
                cycle,
            },
            mark: self.pass,
        });
    }

    /// Bring a kept ring in line with the row's current hidden roots. Does nothing (and
    /// allocates nothing) while they are unchanged.
    fn edit(
        &mut self,
        ix: usize,
        hidden: &[u32],
        procs: &[ProcessEntry],
        gatherer: &Gatherer,
        cycle: u64,
    ) {
        let slot = &mut self.slots[ix];
        let unchanged = slot.roots.len() == hidden.len()
            && slot
                .roots
                .iter()
                .zip(hidden)
                .all(|(inc, &r)| *inc == incarnation(&procs[r as usize]));
        if unchanged {
            return;
        }
        slot.ring.advance(cycle);
        // Compare the processes the ring accounted for (under the old roots that still live;
        // a dead root's contribution is the past and stays) with those under the new roots,
        // by process so a root moving under another root, or a subtree changing hands between
        // roots, nets out. Only the processes under the old and new roots are visited and
        // marked; the mask is zeroed again through the touched list, never swept whole.
        if self.mask.len() < procs.len() {
            self.mask.resize(procs.len(), 0);
        }
        self.touched.clear();
        let (marks, touched) = (&mut self.mask, &mut self.touched);
        let mut mark = |i: usize, side: u8| {
            if marks[i] == 0 {
                touched.push(u32::try_from(i).unwrap_or(NONE));
            }
            marks[i] |= side;
        };
        for inc in &slot.roots {
            if let Ok(idx) = procs.binary_search_by(|p| p.pid.cmp(&inc.0))
                && procs[idx].start_time == inc.1
            {
                walk_subtree(&mut self.walk, procs, idx, |i| mark(i, OLD));
            }
        }
        for &root in hidden {
            walk_subtree(&mut self.walk, procs, root as usize, |i| mark(i, NEW));
        }
        // Only the history is edited; the newest cell is the totals pass's to set.
        let history = &mut slot.ring.cells.0[..SPARK_CELLS - 1];
        for &i in &self.touched {
            let i = i as usize;
            let m = std::mem::take(&mut self.mask[i]);
            if m == OLD | NEW {
                continue;
            }
            let Some(series) = gatherer.cpu_series(procs[i].pid) else {
                continue;
            };
            for (cell, member) in history.iter_mut().zip(series.0) {
                if m == NEW {
                    cell.add(member);
                } else {
                    cell.sub(member);
                }
            }
        }
        slot.roots.clear();
        slot.roots
            .extend(hidden.iter().map(|&r| incarnation(&procs[r as usize])));
    }

    /// Drop slots the last reconcile did not see, keeping the dense vector and both indexes
    /// consistent.
    fn sweep(&mut self) {
        let mut i = 0;
        while i < self.slots.len() {
            if self.slots[i].mark == self.pass {
                i += 1;
                continue;
            }
            let dead = self.slots.swap_remove(i);
            match dead.id {
                OwnedId::Process(inc) => self.by_process.remove(&inc),
                OwnedId::Group(key) => self.by_group.remove(&key),
            };
            // The former last slot now sits at `i`: repoint its index entry.
            if let Some(moved) = self.slots.get(i) {
                match &moved.id {
                    OwnedId::Process(inc) => self.by_process.insert(*inc, i),
                    OwnedId::Group(key) => self.by_group.insert(key.clone(), i),
                };
            }
        }
    }

    /// Record this sample's cell on every collapsed row. `totals` is aligned to the rows.
    pub(super) fn push(&mut self, totals: &[Totals], sample_cycle: u64) {
        for (&ix, total) in self.row_slot.iter().zip(totals) {
            if ix != NONE {
                let cell = SeriesCell {
                    rate: total.cpu_now,
                    waiting: total.wait_now,
                };
                self.slots[ix as usize].ring.record(cell, sample_cycle);
            }
        }
    }

    pub(super) fn get(&self, row: usize) -> Option<CpuSeries> {
        match *self.row_slot.get(row)? {
            NONE => None,
            ix => Some(self.slots[ix as usize].ring.cells),
        }
    }
}

/// Visit every process in the native subtree under `root` (an index into `procs`).
fn walk_subtree(
    walk: &mut Vec<u32>,
    procs: &[ProcessEntry],
    root: usize,
    mut visit: impl FnMut(usize),
) {
    walk.clear();
    walk.push(u32::try_from(root).unwrap_or(NONE));
    while let Some(iu) = walk.pop() {
        visit(iu as usize);
        let mut child = procs[iu as usize].first_child;
        while child != NONE {
            walk.push(child);
            child = procs[child as usize].next_sibling;
        }
    }
}

/// Which side of a hidden-root change a process is under, as mask bits.
const OLD: u8 = 1;
const NEW: u8 = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gather::Waiting;

    fn ring(cycle: u64) -> Ring {
        Ring {
            cells: CpuSeries::default(),
            cycle,
        }
    }

    fn rate(rate: u32) -> SeriesCell {
        SeriesCell {
            rate,
            waiting: Waiting::default(),
        }
    }

    fn rates(r: &Ring) -> [u32; SPARK_CELLS] {
        r.cells.rates()
    }

    #[test]
    fn record_scrolls_and_overwrites_a_repeated_cycle() {
        let mut r = ring(0);
        r.record(rate(5), 1);
        r.record(rate(9), 1);
        assert_eq!(rates(&r)[SPARK_CELLS - 1], 9);
        r.record(rate(7), 2);
        assert_eq!(rates(&r)[SPARK_CELLS - 2..], [9, 7]);
        r.record(rate(1), 1);
        assert_eq!(
            rates(&r)[SPARK_CELLS - 2..],
            [9, 7],
            "an older cycle is ignored"
        );
    }

    #[test]
    fn missed_cycles_are_zero_filled() {
        let mut r = ring(0);
        r.record(rate(5), 1);
        r.record(rate(7), 4);
        assert_eq!(rates(&r)[SPARK_CELLS - 4..], [5, 0, 0, 7]);
    }

    #[test]
    fn row_wait_share_is_summed_wait_over_summed_wait_plus_run() {
        // A busy member with a little wait and an idle-ish one with a lot: the row's share is
        // dominated by the busy member (Σwait / Σ(wait+run)), not the mean of the two shares.
        let mut total = Totals::default();
        total.add(&Totals {
            wait_now: Waiting { wait: 10, run: 90 }, // 10%
            ..Totals::default()
        });
        total.add(&Totals {
            wait_now: Waiting { wait: 5, run: 5 }, // 50%
            ..Totals::default()
        });
        assert_eq!(total.wait_now, Waiting { wait: 15, run: 95 });
        let mut r = ring(0);
        r.record(
            SeriesCell {
                rate: 1,
                waiting: total.wait_now,
            },
            1,
        );
        let share = r.cells.0[SPARK_CELLS - 1].waiting.share_bp();
        assert_eq!(share, Some(15 * 10_000 / 110));
        assert_ne!(share, Some(3000), "not the mean of 10% and 50%");
    }
}
