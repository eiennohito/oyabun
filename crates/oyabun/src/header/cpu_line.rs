//! The CPU line: slots, then a graph of the history filling the rest.

use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use etch::Rgb;

use crate::gather::CpuHistory;
use crate::palette;
use crate::procs::{CpuWait, SystemStats};
use crate::sparkline::{Level, Scale};

use super::fields::{Blank, Cores, Label, Right, Span, WAIT_W, Wait};
use super::layout::{CpuSlots, HeaderLayout};
use super::sink::{Emit, Keyed, Scratch, Sink};

/// Steal below this (millicores) would print as `0.0`; not worth the space.
pub(super) const STEAL_MIN_MC: u32 = 50;

/// The slots left of the graph: used/total cores, then wait, kernel share, steal. Every number
/// sits in a slot as wide as its largest value; an optional slot that exists is blank, not
/// removed, while it has nothing to say.
pub(super) fn cpu_text(sys: &SystemStats, lay: &CpuSlots, out: &mut impl Sink) {
    let cores = lay.num_cores;
    let cw = Cores::width(cores);
    out.slot("cpu");
    out.pair("CPU ", Cores::field(sys.cpu_busy_mc, cores), palette::INFO);
    out.put(format_args!("/{cores}"), palette::LABEL);
    if lay.wait {
        out.slot("wait");
        match sys.cpu_wait {
            CpuWait::Pressure(bp) => out.pair("  wait ", Wait::field(bp), palette::pressure(bp)),
            CpuWait::RunQueue(n) => out.pair("  runq ", Right(n.min(9999), WAIT_W), palette::INFO),
        }
    }
    if lay.kernel {
        out.slot("sys");
        out.pair(
            "  sys ",
            Cores::field(sys.cpu_kernel_mc, cores),
            palette::INFO,
        );
    }
    if lay.steal {
        out.slot("steal");
        if sys.cpu_steal_mc >= STEAL_MIN_MC {
            out.pair(
                "  steal ",
                Cores::field(sys.cpu_steal_mc, cores),
                palette::WARN,
            );
        } else {
            out.put(Blank("  steal ".len() + cw), palette::LABEL);
        }
    }
    out.put("  ", palette::LABEL);
    out.slot("graph");
}

/// The CPU line: numbers, then a graph of the history filling the remaining width, then the
/// scale and the time the drawn cells cover.
pub(crate) struct CpuLine<'a> {
    sys: &'a SystemStats,
    lay: &'a HeaderLayout,
    history: &'a CpuHistory,
    /// Cells of history actually drawn (`<= lay.cpu.cells`).
    filled: usize,
}

impl<'a> CpuLine<'a> {
    pub(crate) fn new(
        sys: &'a SystemStats,
        lay: &'a HeaderLayout,
        history: &'a CpuHistory,
    ) -> Self {
        Self {
            sys,
            lay,
            history,
            filled: history.visible_len().min(lay.cpu.cells),
        }
    }

    /// The cells actually drawn, oldest first: the newest `filled` of the visible history.
    fn drawn(&self) -> impl Iterator<Item = (Level, Rgb)> + '_ {
        let full = u64::from(self.history.scale().max(1)) * 1000;
        let skip = self.history.visible_len() - self.filled;
        self.history.visible_cells().skip(skip).map(move |c| {
            (
                Level::of(u64::from(c.busy_mc), Scale::Linear { full }),
                palette::pressure(c.pressure_bp),
            )
        })
    }

    /// The scale/span label, from the cells actually drawn: right after start, one cell reads
    /// `1s`, not the span the whole empty graph could hold.
    fn label(&self) -> Label {
        Label {
            scale: self.history.scale(),
            span: Span::of_cells(self.filled),
        }
    }

    pub(crate) fn build(&self, l: &mut etch::Line) {
        cpu_text(
            self.sys,
            &self.lay.cpu,
            &mut Emit {
                line: l,
                left: self.lay.width,
            },
        );
        let cells = self.lay.cpu.cells;
        if cells > 0 {
            // Newest at the right edge; an unfilled history leaves blank space on the left.
            l.gap(cells - self.filled);
            let mut run: Option<(Level, Rgb, usize)> = None;
            for (level, color) in self.drawn() {
                match &mut run {
                    // Blank cells are invisible whatever their color, so they merge freely.
                    Some((rl, rc, n)) if *rl == level && (*rc == color || level.is_blank()) => {
                        *n += 1;
                    }
                    _ => {
                        if let Some((rl, rc, n)) = run.replace((level, color, 1)) {
                            l.bar(n, rl.glyph(), rc);
                        }
                    }
                }
            }
            if let Some((rl, rc, n)) = run {
                l.bar(n, rl.glyph(), rc);
            }
            let mut s = Scratch::new();
            let _ = write!(s, "{}", self.label());
            l.span(s.as_str(), palette::LABEL);
        }
        l.fill(' ');
    }
}

/// The repaint key is what is drawn: the formatted numbers, and the graph as glyph levels and
/// colors with its scale and span — not the history's push counter, which advances every cycle
/// even when the picture does not change.
impl Hash for CpuLine<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.lay.width.hash(state);
        cpu_text(self.sys, &self.lay.cpu, &mut Keyed(state));
        let cells = self.lay.cpu.cells;
        cells.hash(state);
        if cells > 0 {
            self.filled.hash(state);
            self.history.scale().hash(state);
            for (level, color) in self.drawn() {
                level.hash(state);
                if !level.is_blank() {
                    color.hash(state);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gather::CpuCell;
    use crate::header::testing::*;

    #[test]
    fn cpu_text_fills_its_slots() {
        let mut s = sys();
        let l = HeaderLayout::new(&s, 200);
        assert_eq!(cpu_str(&s, &l), "CPU  37.2/256  wait   3%  sys   6.0  ");
        // No steal accounting: no slot at all.
        s.cpu_steal_mc = 400;
        assert!(!cpu_str(&s, &l).contains("steal"));
        // Steal accounting: the slot is there, blank below threshold, filled above.
        s.cpu_has_steal = true;
        let l = HeaderLayout::new(&s, 200);
        assert!(cpu_str(&s, &l).contains("steal   0.4"));
        s.cpu_steal_mc = 49;
        let quiet = cpu_str(&s, &l);
        assert!(!quiet.contains("steal"));
        s.cpu_steal_mc = 400;
        assert_eq!(quiet.len(), cpu_str(&s, &l).len());
        s.cpu_wait = CpuWait::RunQueue(7);
        assert!(cpu_str(&s, &l).contains("runq    7"));
    }

    fn history(cells: &[u32], visible: usize) -> CpuHistory {
        let mut h = CpuHistory::new(8);
        h.set_visible(visible);
        for &mc in cells {
            h.push(CpuCell {
                busy_mc: mc,
                pressure_bp: 0,
            });
        }
        h
    }

    #[test]
    fn label_counts_filled_cells_not_the_graph_width() {
        let s = sys();
        let l = HeaderLayout::new(&s, 200);
        let h = history(&[500], 80);
        let line = CpuLine::new(&s, &l, &h);
        assert_eq!(line.filled, 1);
        assert_eq!(line.label().to_string(), " /1 1s");
    }

    #[test]
    fn key_follows_the_picture_not_the_push_counter() {
        let s = sys();
        let l = HeaderLayout::new(&s, 200);
        let line = |h: &CpuHistory, s: &SystemStats| key(&CpuLine::new(s, &l, h));
        // Idle: once the graph is full, another identical push draws the same picture.
        let idle = history(&[0; 30], 20);
        let idle_later = history(&[0; 31], 20);
        assert_eq!(line(&idle, &s), line(&idle_later, &s));
        let busy = history(&[0, 0, 5000], 20);
        let less_busy = history(&[0, 0, 4000], 20);
        assert_ne!(line(&busy, &s), line(&less_busy, &s));
        let mut changed = s;
        changed.cpu_busy_mc += 100;
        assert_ne!(line(&idle, &s), line(&idle, &changed));
    }
}
