//! The memory line: RAM split bar, swap bar, and the fixed lead slots.

use std::hash::{Hash, Hasher};

use etch::Rgb;

use crate::palette;
use crate::procs::SystemStats;
use crate::ui::Mem;

use super::fields::{Blank, MEM_W, Right, Wait, mem_field, tasks_field};
use super::layout::{
    BAR_FRAME, HeaderLayout, IO_SLOT, MEM_WAIT_SLOT, MemSlot, MemSlots, RATE_SLOT, SHMEM_SLOT,
    SWAP_LABEL, TASK_SLOT,
};
use super::sink::{Emit, Keyed, Sink};

/// Shmem is worth a mention at this fraction of RAM (1/N), or when it exceeds swap use.
pub(super) const SHMEM_RAM_FRACTION: u64 = 16;

/// Is tmpfs/shm memory worth a header slot? Either a meaningful share of RAM, or — on a
/// machine that has swap — more than what has been pushed to it: the "memory nobody owns"
/// case. Without swap there is nothing to compare against, so only the RAM share counts.
pub(super) fn shmem_significant(sys: &SystemStats) -> bool {
    sys.mem_shmem > 0
        && (sys.mem_shmem >= sys.mem_total / SHMEM_RAM_FRACTION
            || (sys.swap_total > 0 && sys.mem_shmem > sys.swap_used))
}

/// How RAM divides, in bytes; the four parts always sum to the total.
///
/// `MemAvailable` already excludes shmem (swap-backed pages are not on the file LRU the kernel
/// counts as reclaimable), so shmem sits inside `total - available`: it is carved out of the
/// "used" part, not added to it. Each part is clamped by what remains, so inconsistent inputs
/// (shmem larger than used, free larger than available) can never push the bar past its end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct MemSplit {
    /// Held by processes and the kernel, not reclaimable.
    used: u64,
    shmem: u64,
    /// Available, but still cached: taking it back costs a refill later.
    cache: u64,
    free: u64,
}

impl MemSplit {
    fn of(sys: &SystemStats) -> Self {
        let available = sys.mem_available.min(sys.mem_total);
        let taken = sys.mem_total - available;
        let shmem = sys.mem_shmem.min(taken);
        let free = sys.mem_free.min(available);
        Self {
            used: taken - shmem,
            shmem,
            cache: available - free,
            free,
        }
    }

    fn total(&self) -> u64 {
        self.used + self.shmem + self.cache + self.free
    }

    /// Cell counts per part for a bar of `n` cells. Boundaries are rounded cumulatively, so
    /// the counts sum to exactly `n` and no part's rounding error leaks into the next.
    fn cells(&self, n: usize) -> [usize; 4] {
        let total = u128::from(self.total());
        if total == 0 {
            return [0, 0, 0, n];
        }
        let edge = |cum: u64| ((u128::from(cum) * n as u128 + total / 2) / total) as usize;
        let a = edge(self.used);
        let b = edge(self.used + self.shmem);
        let c = edge(self.used + self.shmem + self.cache);
        [a, b - a, c - b, n - c]
    }
}

/// Cells of a swap bar of `n` cells that are filled. Any swap in use shows at least one cell:
/// "something is swapped out" is the lead, even when it is a sliver.
pub(super) fn swap_cells(sys: &SystemStats, n: usize) -> usize {
    if sys.swap_total == 0 || sys.swap_used == 0 || n == 0 {
        return 0;
    }
    let total = u128::from(sys.swap_total);
    let used = u128::from(sys.swap_used.min(sys.swap_total));
    (((used * n as u128 + total / 2) / total) as usize).clamp(1, n)
}

/// Bar color for memory that is in use: pressure when the kernel reports it (length = usage,
/// color = waiting, as in the CPU graph), neutral otherwise.
pub(super) fn used_color(sys: &SystemStats) -> Rgb {
    sys.mem_wait_bp.map_or(palette::MEM_USED, palette::pressure)
}

/// Memory/swap bar glyphs, densest first. Braille, not block elements: a full block touches
/// the cell edges and fuses with the CPU graph on the line above into one shape, while braille
/// dots keep a gap on every side, so the bar reads as its own widget. Density steps down with
/// how firmly the memory is held (owned, then shmem, then reclaimable cache), so the segments
/// stay distinguishable without color. Braille is also neutral-width, unlike block elements.
pub(super) const BAR_HELD: char = '⣿';
pub(super) const BAR_SHMEM: char = '⣶';
pub(super) const BAR_CACHE: char = '⣤';

pub(super) fn mem_bar(sys: &SystemStats, n: usize, out: &mut impl Sink) {
    let [used, shmem, cache, free] = MemSplit::of(sys).cells(n);
    out.put("[", palette::FOOTER);
    out.cells(used, BAR_HELD, used_color(sys));
    out.cells(shmem, BAR_SHMEM, palette::MEM_SHMEM);
    out.cells(cache, BAR_CACHE, palette::MEM_CACHE);
    out.cells(free, ' ', palette::FOOTER);
    out.put("] ", palette::FOOTER);
}

pub(super) fn swap_bar(sys: &SystemStats, n: usize, out: &mut impl Sink) {
    let filled = swap_cells(sys, n);
    out.put("[", palette::FOOTER);
    out.cells(filled, BAR_HELD, used_color(sys));
    out.cells(n - filled, ' ', palette::FOOTER);
    out.put("] ", palette::FOOTER);
}

/// The memory line's text, slot by slot in line order. A slot the layout reserved but whose
/// subject is absent or quiet is printed blank at its full width.
pub(super) fn mem_text(sys: &SystemStats, lay: &MemSlots, out: &mut impl Sink) {
    let bars = lay.bars;
    out.slot("mem");
    out.put("Mem ", palette::LABEL);
    if bars.mem > 0 {
        out.slot("mem_bar");
        mem_bar(sys, bars.mem, out);
    }
    out.slot("used");
    out.put(
        mem_field(sys.mem_total.saturating_sub(sys.mem_available)),
        palette::INFO,
    );
    out.put(
        format_args!("/{}", Right(Mem(sys.mem_total), lay.mem_total_w)),
        palette::LABEL,
    );
    out.slot("avail");
    out.pair("  avail ", mem_field(sys.mem_available), palette::INFO);
    if lay.has(MemSlot::Wait) {
        out.slot("wait");
        match sys.mem_wait_bp {
            Some(bp) => out.pair("  wait ", Wait::field(bp), palette::pressure(bp)),
            None => out.put(Blank(MEM_WAIT_SLOT), palette::LABEL),
        }
    }
    if lay.has(MemSlot::Swap) {
        let live = sys.swap_total > 0;
        out.slot("swap");
        if live {
            out.put(SWAP_LABEL, palette::LABEL);
        } else {
            out.put(Blank(SWAP_LABEL.len()), palette::LABEL);
        }
        if bars.swap > 0 {
            out.slot("swap_bar");
            if live {
                swap_bar(sys, bars.swap, out);
            } else {
                out.put(Blank(bars.swap + BAR_FRAME), palette::FOOTER);
            }
        }
        out.slot("swap_used");
        if live {
            out.put(mem_field(sys.swap_used), palette::INFO);
            out.put(
                format_args!("/{}", Right(Mem(sys.swap_total), lay.swap_total_w)),
                palette::LABEL,
            );
        } else {
            out.put(Blank(MEM_W + 1 + lay.swap_total_w), palette::LABEL);
        }
    }
    if lay.has(MemSlot::Io) {
        out.slot("io");
        match sys.io_wait_bp {
            Some(bp) => out.pair("  IO wait ", Wait::field(bp), palette::pressure(bp)),
            None => out.put(Blank(IO_SLOT), palette::LABEL),
        }
    }
    if lay.has(MemSlot::Rate) {
        out.slot("rate");
        if sys.swap_total > 0 && sys.swap_in_bps > 0 {
            out.pair(
                "  in ",
                format_args!("{}/s", mem_field(sys.swap_in_bps)),
                palette::INFO,
            );
        } else {
            out.put(Blank(RATE_SLOT), palette::LABEL);
        }
    }
    let stuck = palette::state(b'D');
    if lay.has(MemSlot::Blocked) {
        out.slot("blocked");
        if sys.tasks_blocked > 0 {
            out.put(
                format_args!("  D {}", tasks_field(sys.tasks_blocked)),
                stuck,
            );
        } else {
            out.put(Blank(TASK_SLOT), palette::LABEL);
        }
    }
    if lay.has(MemSlot::Zombie) {
        out.slot("zombie");
        if sys.tasks_zombie > 0 {
            out.put(format_args!("  Z {}", tasks_field(sys.tasks_zombie)), stuck);
        } else {
            out.put(Blank(TASK_SLOT), palette::LABEL);
        }
    }
    if lay.has(MemSlot::Shmem) {
        out.slot("shmem");
        if shmem_significant(sys) {
            out.pair("  Shmem ", mem_field(sys.mem_shmem), palette::INFO);
        } else {
            out.put(Blank(SHMEM_SLOT), palette::LABEL);
        }
    }
    out.slot("end");
}

/// The memory line. Its repaint key is what is drawn — bar cells as drawn, and the numbers —
/// so a byte-level change that moves no cell and no printed digit repaints nothing.
pub(crate) struct MemLine<'a> {
    sys: &'a SystemStats,
    lay: &'a HeaderLayout,
}

impl<'a> MemLine<'a> {
    pub(crate) fn new(sys: &'a SystemStats, lay: &'a HeaderLayout) -> Self {
        Self { sys, lay }
    }

    pub(crate) fn build(&self, l: &mut etch::Line) {
        mem_text(
            self.sys,
            &self.lay.mem,
            &mut Emit {
                line: l,
                left: self.lay.width,
            },
        );
        l.fill(' ');
    }
}

impl Hash for MemLine<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.lay.width.hash(state);
        mem_text(self.sys, &self.lay.mem, &mut Keyed(state));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::layout::Bars;
    use crate::header::testing::*;
    use crate::procs::CpuWait;

    #[test]
    fn mem_line_key_ignores_cpu_fields() {
        let s = sys();
        let l = HeaderLayout::new(&s, 100);
        let mut t = s;
        t.cpu_busy_mc += 1000;
        t.cpu_wait = CpuWait::Pressure(900);
        assert_eq!(key(&MemLine::new(&s, &l)), key(&MemLine::new(&t, &l)));
        t.mem_available -= 1 << 30;
        assert_ne!(key(&MemLine::new(&s, &l)), key(&MemLine::new(&t, &l)));
    }

    #[test]
    fn mem_text_is_minimal_on_a_healthy_machine() {
        let s = sys();
        let l = HeaderLayout::new(&s, 0);
        assert_eq!(l.mem.bars, Bars::default());
        assert_eq!(mem_str(&s, &l), "Mem  52.0G/64.0G  avail  12.0G");
    }

    #[test]
    fn mem_text_surfaces_every_lead_in_fixed_slots() {
        let mut s = full();
        s.mem_wait_bp = Some(0);
        s.swap_used = 1 << 30;
        s.swap_in_bps = 3 << 20;
        s.mem_shmem = 2 << 30;
        s.io_wait_bp = Some(400);
        s.tasks_blocked = 3;
        s.tasks_zombie = 1;
        let l = HeaderLayout::new(&s, 400);
        // Bars are sized for 400 columns; strip them to compare the text.
        let text = mem_str(&s, &l);
        let tail = text.split_once("] ").unwrap().1;
        assert!(tail.contains("avail  12.0G  wait   0%"), "{text}");
        assert!(
            tail.contains("1.0G/ 8.0G") || tail.contains("1.0G/8.0G"),
            "{text}"
        );
        assert!(
            tail.contains("IO wait   4%  in   3.0M/s  D    3  Z    1  Shmem   2.0G"),
            "{text}"
        );
    }

    #[test]
    fn mem_bars_are_drawn_with_frames() {
        let mut s = full();
        s.mem_wait_bp = None;
        s.io_wait_bp = None;
        s.swap_used = 4 << 30;
        s.mem_shmem = 8 << 30;
        let mut l = HeaderLayout::new(&s, 0);
        l.mem.bars = Bars { mem: 16, swap: 8 };
        l.mem.present[MemSlot::Swap as usize] = true;
        // used 44G, shmem 8G, cache 10G, free 2G of 64G over 16 cells (4G each; free rounds away).
        let text = mem_str(&s, &l);
        assert!(
            text.starts_with(
                "Mem [⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣶⣶⣤⣤⣤]  52.0G/64.0G  avail  12.0G  Swap [⣿⣿⣿⣿    ]   4.0G/8.0G"
            ),
            "{text}"
        );
    }

    #[test]
    fn split_sums_to_total_and_clamps() {
        let mut s = sys();
        s.mem_shmem = 8 << 30;
        let m = MemSplit::of(&s);
        assert_eq!(
            (m.used, m.shmem, m.cache, m.free),
            (44 << 30, 8 << 30, 10 << 30, 2 << 30)
        );
        // Shmem larger than everything not available: it takes all of it, used goes to 0.
        s.mem_shmem = 60 << 30;
        let m = MemSplit::of(&s);
        assert_eq!((m.used, m.shmem), (0, 52 << 30));
        assert_eq!(m.total(), 64 << 30);
        // Free larger than available, available larger than total.
        s.mem_free = 100 << 30;
        s.mem_available = 70 << 30;
        let m = MemSplit::of(&s);
        assert_eq!((m.used, m.shmem, m.cache, m.free), (0, 0, 0, 64 << 30));
        // Zero total: nothing to divide, the bar is blank.
        let m = MemSplit::of(&SystemStats::default());
        assert_eq!(m.cells(10), [0, 0, 0, 10]);
    }

    #[test]
    fn split_cells_sum_to_the_bar_for_any_width() {
        let mut s = sys();
        s.mem_shmem = 3 << 30;
        let m = MemSplit::of(&s);
        for n in 0..200 {
            assert_eq!(m.cells(n).iter().sum::<usize>(), n, "bar of {n}");
        }
    }

    #[test]
    fn swap_sliver_stays_visible() {
        let mut s = sys();
        s.swap_total = 64 << 30;
        s.swap_used = 1 << 20;
        assert_eq!(swap_cells(&s, 10), 1);
        s.swap_used = 0;
        assert_eq!(swap_cells(&s, 10), 0);
        s.swap_used = 200 << 30;
        assert_eq!(swap_cells(&s, 10), 10, "clamped to the bar");
    }

    #[test]
    fn mem_line_key_is_the_drawn_content() {
        let mut machine = sys();
        machine.swap_total = 8 << 30;
        let lay = HeaderLayout::new(&machine, 120);
        let keyed = |stats: &SystemStats| key(&MemLine::new(stats, &lay));
        let base = keyed(&machine);
        // Unrelated: CPU, or a one-byte memory wobble that moves neither a cell nor a digit.
        let mut moved = machine;
        moved.cpu_busy_mc += 500;
        moved.num_cores += 1;
        moved.mem_available -= 1;
        moved.mem_free -= 1;
        assert_eq!(base, keyed(&moved));
        // Related: a segment boundary moves.
        moved.mem_free = 0;
        moved.mem_available -= 8 << 30;
        assert_ne!(base, keyed(&moved));
        // Related: swap fill, pressure.
        let mut other = machine;
        other.swap_used = 6 << 30;
        assert_ne!(base, keyed(&other));
        let mut other = machine;
        other.mem_wait_bp = Some(0);
        assert_ne!(base, keyed(&other), "used-bar color follows pressure");
    }

    #[test]
    fn shmem_needs_to_matter() {
        let mut s = sys(); // 64G RAM: 1/16 = 4G
        s.swap_total = 8 << 30;
        s.swap_used = 4 << 30;
        s.mem_shmem = 1 << 30;
        assert!(!shmem_significant(&s), "small and below swap use");
        s.swap_used = 512 << 20;
        assert!(shmem_significant(&s), "exceeds swap use");
        s.swap_used = 6 << 30;
        s.mem_shmem = 4 << 30;
        assert!(shmem_significant(&s), "a sixteenth of RAM");
        s.mem_shmem = 0;
        s.swap_used = 0;
        assert!(!shmem_significant(&s), "zero is never shown");
    }

    #[test]
    fn shmem_on_a_swapless_machine_needs_a_ram_share() {
        let mut s = sys();
        s.mem_shmem = 1 << 30; // 1/64 of RAM, and swap_used == 0 < shmem
        assert!(
            !shmem_significant(&s),
            "nothing to compare against without swap"
        );
        s.mem_shmem = 4 << 30;
        assert!(shmem_significant(&s));
    }
}
