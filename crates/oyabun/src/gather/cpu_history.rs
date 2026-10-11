//! The header sparkline's data: one [`CpuCell`] per refresh interval, plus the vertical scale
//! the graph is drawn against.
//!
//! **Why the scale is part of the history.** The graph is rescaled to the visible peak so a
//! quiet machine's activity is still legible, but a scale that tracks the peak exactly jumps
//! the whole shape whenever the peak sample scrolls out of view. So the scale moves in
//! discrete steps (doubling, then the machine's own core count as the final step — every
//! ratio between steps is ≤ 2, so the peak always lands in the upper half), rises the moment
//! the data needs it, and falls only once everything visible fits under half the scale. That
//! asymmetry is free hysteresis, but it is *state*: it depends on what the scale was a moment
//! ago, so it lives with the data it summarises rather than being recomputed at render.
//!
//! **Why the capacity is grow-only.** One interval per cell means a wider terminal shows more
//! history. The ring is sized to the widest width ever seen, so shrinking the window and
//! growing it back reveals the older cells again instead of an empty graph.

use std::collections::VecDeque;

/// One refresh interval of machine-wide CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuCell {
    /// Busy CPU during the interval, millicores.
    pub busy_mc: u32,
    /// How much runnable work was delayed for CPU during the interval, basis points — the
    /// graph's color. PSI-measured when available, otherwise derived from run-queue overload.
    pub pressure_bp: u32,
}

/// Capacity before any terminal width is known (and a floor after).
const MIN_CAPACITY: usize = 80;

/// A grow-only ring of [`CpuCell`]s with the hysteretic vertical scale of its visible tail.
pub struct CpuHistory {
    cells: VecDeque<CpuCell>,
    capacity: usize,
    /// How many of the newest cells are on screen; the scale fits exactly these.
    visible: usize,
    num_cores: u32,
    /// Current scale in whole cores: a step from [`Self::fit_scale`].
    scale: u32,
}

impl CpuHistory {
    pub fn new(num_cores: u32) -> Self {
        let num_cores = num_cores.max(1);
        Self {
            cells: VecDeque::with_capacity(MIN_CAPACITY),
            capacity: MIN_CAPACITY,
            visible: 0,
            num_cores,
            scale: 1,
        }
    }

    /// Ensure room for `cells` intervals. Never shrinks.
    pub fn reserve(&mut self, cells: usize) {
        if cells > self.capacity {
            self.capacity = cells;
            self.cells.reserve(cells - self.cells.len());
        }
    }

    /// Set how many of the newest cells are on screen (the sparkline's width).
    pub fn set_visible(&mut self, cells: usize) {
        if cells != self.visible {
            self.visible = cells;
            self.retarget();
        }
    }

    /// Replay streams carry their own core count; live reads it once.
    pub fn set_num_cores(&mut self, n: u32) {
        let n = n.max(1);
        if n != self.num_cores {
            self.num_cores = n;
            self.scale = self.scale.min(n);
            self.retarget();
        }
    }

    pub fn push(&mut self, cell: CpuCell) {
        if self.cells.len() == self.capacity {
            self.cells.pop_front();
        }
        self.cells.push_back(cell);
        self.retarget();
    }

    /// The on-screen cells, oldest first. Fewer than [`Self::set_visible`]'s count until the
    /// history has filled.
    pub fn visible_cells(&self) -> impl Iterator<Item = CpuCell> + '_ {
        let skip = self.cells.len().saturating_sub(self.visible);
        self.cells.iter().skip(skip).copied()
    }

    /// The graph's full height, in whole cores.
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// How many cells [`Self::visible_cells`] yields.
    pub fn visible_len(&self) -> usize {
        self.cells.len().min(self.visible)
    }

    /// The smallest step that holds `max_mc`: a power-of-two core count, or the machine's core
    /// count once the next power would exceed it (14 cores: 1, 2, 4, 8, 14).
    fn fit_scale(&self, max_mc: u32) -> u32 {
        let cores_needed = max_mc.div_ceil(1000).max(1);
        let pow2 = cores_needed.next_power_of_two();
        if pow2 >= self.num_cores {
            self.num_cores
        } else {
            pow2
        }
    }

    fn retarget(&mut self) {
        let max_mc = self.visible_cells().map(|c| c.busy_mc).max().unwrap_or(0);
        let fit = self.fit_scale(max_mc);
        let below_half = u64::from(max_mc) * 2 < u64::from(self.scale) * 1000;
        if fit > self.scale || below_half {
            self.scale = fit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(cores_x10: u32) -> CpuCell {
        CpuCell {
            busy_mc: cores_x10 * 100,
            pressure_bp: 0,
        }
    }

    fn history(cores: u32, visible: usize) -> CpuHistory {
        let mut h = CpuHistory::new(cores);
        h.set_visible(visible);
        h
    }

    #[test]
    fn scale_steps_double_then_end_at_core_count() {
        let h = CpuHistory::new(14);
        let steps: Vec<u32> = [0, 1000, 1001, 2000, 2001, 4000, 4001, 8000, 8001, 14_000]
            .into_iter()
            .map(|mc| h.fit_scale(mc))
            .collect();
        assert_eq!(steps, [1, 1, 2, 2, 4, 4, 8, 8, 14, 14]);
        assert_eq!(CpuHistory::new(22).fit_scale(17_000), 22);
        assert_eq!(CpuHistory::new(16).fit_scale(9_000), 16);
        assert_eq!(
            CpuHistory::new(1).fit_scale(5_000),
            1,
            "never above the machine"
        );
    }

    #[test]
    fn scale_rises_immediately() {
        let mut h = history(14, 10);
        h.push(cell(5));
        assert_eq!(h.scale(), 1);
        h.push(cell(90)); // 9 cores
        assert_eq!(
            h.scale(),
            14,
            "jumps straight to the step that holds the peak"
        );
    }

    #[test]
    fn scale_falls_only_when_everything_visible_is_under_half() {
        let mut h = history(14, 4);
        h.push(cell(90));
        assert_eq!(h.scale(), 14);
        // Peak scrolls out: remaining max 7.5 cores is still above half of 14 -> hold.
        h.push(cell(75));
        for _ in 0..3 {
            h.push(cell(75));
        }
        assert_eq!(
            h.scale(),
            14,
            "7.5 cores > 14/2: hysteresis holds the scale"
        );
        // Everything visible below 7 cores -> drops to the step that fits.
        for _ in 0..4 {
            h.push(cell(65));
        }
        assert_eq!(h.scale(), 8);
        // 8 holds while the peak sits in (4, 8].
        for _ in 0..4 {
            h.push(cell(45));
        }
        assert_eq!(h.scale(), 8, "4.5 cores is not below half of 8: hold");
    }

    #[test]
    fn history_older_than_the_window_does_not_pin_the_scale() {
        let mut h = history(8, 3);
        h.push(cell(70));
        for _ in 0..3 {
            h.push(cell(2));
        }
        assert_eq!(h.scale(), 1, "the 7-core burst is off screen");
    }

    #[test]
    fn narrowing_the_view_can_lower_the_scale_and_widening_raises_it() {
        let mut h = history(8, 4);
        h.push(cell(70));
        for _ in 0..3 {
            h.push(cell(2));
        }
        assert_eq!(h.scale(), 8);
        h.set_visible(3);
        assert_eq!(h.scale(), 1);
        h.set_visible(4);
        assert_eq!(h.scale(), 8, "the burst is visible again");
    }

    #[test]
    fn capacity_grows_with_width_and_never_shrinks() {
        let mut h = history(4, 3);
        h.reserve(200);
        for i in 0..250u32 {
            h.push(cell(i % 10));
        }
        assert_eq!(h.cells.len(), 200);
        h.reserve(50);
        assert_eq!(
            h.capacity, 200,
            "a narrower terminal does not shrink the ring"
        );
        assert_eq!(h.visible_cells().count(), 3);
    }
}
