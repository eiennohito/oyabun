//! Every widget's place, decided per terminal width from frozen structure.

use crate::gather::REFRESH_MS;
use crate::procs::SystemStats;
use crate::ui::Mem;

use super::fields::{Cores, MEM_W, Span, TASKS_W, WAIT_W, digits};
use super::sink::text_width;

/// Cells a bar needs besides its fill: `[`, `]` and the space after.
pub(super) const BAR_FRAME: usize = 3;
/// Narrowest bar still worth drawing; below it the bar is dropped, not squeezed.
pub(super) const MIN_BAR: usize = 6;
/// The bars are guaranteed this share (1/N) of the width before any optional memory slot is
/// admitted, so narrow terminals keep a memory bar instead of a row of mostly blank slots.
pub(super) const BAR_SHARE: usize = 4;
/// Graph cells guaranteed (with room for its label) before an optional CPU slot is admitted.
pub(super) const CPU_MIN_ROOM: usize = 20;

/// Widths of the memory line's optional slots (label included).
pub(super) const MEM_WAIT_SLOT: usize = "  wait ".len() + WAIT_W;
pub(super) const IO_SLOT: usize = "  IO wait ".len() + WAIT_W;
pub(super) const RATE_SLOT: usize = "  in ".len() + MEM_W + "/s".len();
pub(super) const SHMEM_SLOT: usize = "  Shmem ".len() + MEM_W;
pub(super) const TASK_SLOT: usize = "  D ".len() + TASKS_W;
pub(super) const SWAP_LABEL: &str = "  Swap ";

/// The memory line's optional slots, in priority order (highest first): a line too narrow for
/// all of them keeps a prefix of this list.
#[derive(Clone, Copy)]
pub(super) enum MemSlot {
    Wait,
    Swap,
    Rate,
    Io,
    Blocked,
    Zombie,
    Shmem,
}

impl MemSlot {
    const PRIORITY: [Self; 7] = [
        Self::Wait,
        Self::Swap,
        Self::Rate,
        Self::Io,
        Self::Blocked,
        Self::Zombie,
        Self::Shmem,
    ];
}

/// Everything the layout is allowed to know about the machine besides the terminal width.
#[derive(Clone, Copy, Default)]
pub(super) struct Structure {
    num_cores: u32,
    has_steal: bool,
    mem_total: u64,
    swap_total: u64,
    mem_psi: bool,
    io_psi: bool,
}

impl Structure {
    pub(super) fn of(sys: &SystemStats) -> Self {
        Self {
            num_cores: sys.num_cores,
            has_steal: sys.cpu_has_steal,
            mem_total: sys.mem_total,
            swap_total: sys.swap_total,
            mem_psi: sys.mem_wait_bp.is_some(),
            io_psi: sys.io_wait_bp.is_some(),
        }
    }

    /// Does the machine have what this slot reports on?
    pub(super) fn offers(&self, slot: MemSlot) -> bool {
        match slot {
            MemSlot::Wait => self.mem_psi,
            MemSlot::Swap | MemSlot::Rate => self.swap_total > 0,
            MemSlot::Io => self.io_psi,
            MemSlot::Blocked | MemSlot::Zombie | MemSlot::Shmem => true,
        }
    }
}

/// The CPU line's slots: which exist, and the graph's cell count.
#[derive(Clone, Copy, Default)]
pub(super) struct CpuSlots {
    pub(super) num_cores: u32,
    pub(super) wait: bool,
    pub(super) kernel: bool,
    pub(super) steal: bool,
    pub(super) cells: usize,
}

impl CpuSlots {
    pub(super) fn new(st: &Structure, width: usize) -> Self {
        let cores = st.num_cores;
        let cw = Cores::width(cores);
        let mut used = "CPU ".len() + cw + 1 + digits(u64::from(cores)) + "  ".len();
        let mut admit = |w: usize| {
            let fits = used + w + CPU_MIN_ROOM <= width;
            if fits {
                used += w;
            }
            fits
        };
        let wait = admit("  wait ".len() + WAIT_W);
        let kernel = wait && admit("  sys ".len() + cw);
        let steal = kernel && st.has_steal && admit("  steal ".len() + cw);
        let room = width.saturating_sub(used);
        let widest_span = Span::max_width((room as u64 * REFRESH_MS).div_ceil(1000));
        // " /" + scale digits (scale <= cores) + " " + span.
        let reserve = 3 + digits(u64::from(cores.max(1))) + widest_span;
        Self {
            num_cores: cores,
            wait,
            kernel,
            steal,
            cells: room.saturating_sub(reserve),
        }
    }
}

/// Widths of the memory and swap bars in cells; zero omits the bar and its frame.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Bars {
    pub(super) mem: usize,
    pub(super) swap: usize,
}

/// The memory line's slots: which optional ones exist, the bars, and the frozen widths of the
/// totals.
#[derive(Clone, Copy, Default)]
pub(super) struct MemSlots {
    pub(super) present: [bool; 7],
    pub(super) bars: Bars,
    pub(super) mem_total_w: usize,
    pub(super) swap_total_w: usize,
}

impl MemSlots {
    pub(super) fn new(st: &Structure, width: usize) -> Self {
        let mut me = Self {
            mem_total_w: text_width(Mem(st.mem_total)),
            swap_total_w: text_width(Mem(st.swap_total)),
            ..Self::default()
        };
        if st.mem_total == 0 {
            return me;
        }
        let mut used = "Mem ".len() + MEM_W + 1 + me.mem_total_w + "  avail ".len() + MEM_W;
        let budget = (width / BAR_SHARE).max(BAR_FRAME + MIN_BAR);
        for slot in MemSlot::PRIORITY {
            if !st.offers(slot) {
                continue;
            }
            let w = me.slot_width(slot);
            if used + w + budget > width {
                break; // lower priorities never leapfrog a refused one
            }
            used += w;
            me.present[slot as usize] = true;
        }
        let room = width.saturating_sub(used);
        // Bars: about 2/3 memory, 1/3 swap; swap's goes first when they cannot both fit.
        if me.has(MemSlot::Swap) {
            let cells = room.saturating_sub(2 * BAR_FRAME);
            let (mem, swap) = (cells * 2 / 3, cells - cells * 2 / 3);
            if mem >= MIN_BAR && swap >= MIN_BAR {
                me.bars = Bars { mem, swap };
                return me;
            }
        }
        let mem = room.saturating_sub(BAR_FRAME);
        if mem >= MIN_BAR {
            me.bars = Bars { mem, swap: 0 };
        }
        me
    }

    pub(super) fn has(&self, slot: MemSlot) -> bool {
        self.present[slot as usize]
    }

    /// Width of a slot without bars.
    fn slot_width(&self, slot: MemSlot) -> usize {
        match slot {
            MemSlot::Wait => MEM_WAIT_SLOT,
            MemSlot::Swap => SWAP_LABEL.len() + MEM_W + 1 + self.swap_total_w,
            MemSlot::Rate => RATE_SLOT,
            MemSlot::Io => IO_SLOT,
            MemSlot::Blocked | MemSlot::Zombie => TASK_SLOT,
            MemSlot::Shmem => SHMEM_SLOT,
        }
    }
}

/// Where everything in the header goes at one terminal width. Built when the width changes (and
/// once the first sample has told what the machine is), then only read; see the module doc.
#[derive(Clone, Copy, Default)]
pub(crate) struct HeaderLayout {
    pub(super) width: usize,
    /// Built from a real sample (the totals are known), so structure is not a guess.
    pub(super) informed: bool,
    pub(super) cpu: CpuSlots,
    pub(super) mem: MemSlots,
}

impl HeaderLayout {
    /// Lay the header out for `width`, freezing `sys`'s structural facts.
    pub(crate) fn new(sys: &SystemStats, width: usize) -> Self {
        let st = Structure::of(sys);
        Self {
            width,
            informed: sys.mem_total > 0,
            cpu: CpuSlots::new(&st, width),
            mem: MemSlots::new(&st, width),
        }
    }

    /// Cells of the CPU graph: what the history sizes its visible window to.
    pub(crate) fn cpu_cells(&self) -> usize {
        self.cpu.cells
    }

    /// Was this layout made from a real sample? A layout made before the first one would freeze
    /// an empty machine, so the owner rebuilds it once.
    pub(crate) fn is_informed(&self) -> bool {
        self.informed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::cpu_line::cpu_text;
    use crate::header::mem_line::mem_text;
    use crate::header::sink::Width;
    use crate::header::testing::*;
    use crate::procs::CpuWait;

    #[test]
    fn width_sink_matches_the_text() {
        let s = full();
        let l = HeaderLayout::new(&s, 200);
        let mut w = Width::default();
        cpu_text(&s, &l.cpu, &mut w);
        assert_eq!(w.0, cpu_str(&s, &l).len());
        let mut w = Width::default();
        mem_text(&s, &l.mem, &mut w);
        assert_eq!(w.0, mem_str(&s, &l).chars().count());
    }

    #[test]
    fn spark_takes_the_remaining_width_and_never_overflows() {
        let s = sys();
        let l = HeaderLayout::new(&s, 120);
        let text = cpu_str(&s, &l).len();
        // 120 wide: room 120 - text cells, less the label " /256 " + span.
        let cells = l.cpu_cells();
        assert!(cells > 0);
        assert_eq!(HeaderLayout::new(&s, 10).cpu_cells(), 0, "too narrow");
        for width in [60, 120, 400, 1000, 5000] {
            let l = HeaderLayout::new(&s, width);
            let cells = l.cpu_cells();
            let label = 3 + 3 + Span::of_cells(cells).to_string().len();
            assert!(text + cells + label <= width, "width {width} overflows");
        }
    }

    #[test]
    fn narrow_cpu_line_drops_low_priority_slots_first() {
        let s = full();
        let slots = |w| {
            let c = HeaderLayout::new(&s, w).cpu;
            (c.wait, c.kernel, c.steal)
        };
        assert_eq!(slots(200), (true, true, true));
        assert_eq!(slots(10), (false, false, false));
        // Monotone: a narrower terminal never has a slot a wider one lacks.
        let mut prev = (false, false, false);
        for w in 0..200 {
            let cur = slots(w);
            assert!(cur.0 >= prev.0 && cur.1 >= prev.1 && cur.2 >= prev.2, "{w}");
            assert!(!cur.1 || cur.0, "sys without wait at {w}");
            assert!(!cur.2 || cur.1, "steal without sys at {w}");
            prev = cur;
        }
    }

    #[test]
    fn bars_degrade_by_width_alone() {
        let s = full();
        let mut saw = (false, false, false);
        for width in 0..400 {
            let l = HeaderLayout::new(&s, width);
            let bars = l.mem.bars;
            saw.0 |= bars == Bars::default();
            saw.1 |= bars.mem > 0 && bars.swap == 0;
            saw.2 |= bars.swap > 0;
            assert!(bars.mem == 0 || bars.mem >= MIN_BAR);
            assert!(bars.swap == 0 || bars.mem > 0, "swap bar outlives mem bar");
            let mut w = Width::default();
            mem_text(&s, &l.mem, &mut w);
            if bars.mem > 0 {
                assert!(w.0 <= width, "width {width}: {}", w.0);
            }
        }
        assert_eq!(saw, (true, true, true), "every degradation step occurs");
        // Swap bar takes about a third of the bar budget.
        let b = HeaderLayout::new(&s, 300).mem.bars;
        assert!(b.mem.abs_diff(2 * b.swap) <= 2, "{b:?}");
        // No swap: no swap bar, memory takes it all.
        let mut t = s;
        t.swap_total = 0;
        let b = HeaderLayout::new(&t, 100).mem.bars;
        assert_eq!(b.swap, 0);
        assert!(b.mem > 0);
    }

    #[test]
    fn memory_bar_survives_at_80_columns() {
        let l = HeaderLayout::new(&full(), 80);
        assert!(l.mem.bars.mem >= MIN_BAR, "{:?}", l.mem.bars);
        let mut t = full();
        t.swap_total = 0;
        t.io_wait_bp = None;
        assert!(HeaderLayout::new(&t, 80).mem.bars.mem >= MIN_BAR);
    }

    #[test]
    fn optional_mem_slots_drop_in_priority_order() {
        let s = full();
        let count = |w| HeaderLayout::new(&s, w).mem.present;
        let mut prev = count(1000);
        assert!(prev.iter().all(|&p| p), "everything fits at 1000");
        for w in (0..1000).rev() {
            let cur = count(w);
            // The kept slots are always a prefix of the priority list.
            let kept = MemSlot::PRIORITY
                .iter()
                .take_while(|&&s| cur[s as usize])
                .count();
            assert!(
                MemSlot::PRIORITY[kept..].iter().all(|&s| !cur[s as usize]),
                "{w}"
            );
            // Narrower never gains a slot.
            assert!(cur.iter().zip(prev).all(|(&c, p)| !c || p), "{w}");
            prev = cur;
        }
    }

    // --- geometry: widgets move only on resize ---

    /// xorshift64*: deterministic, dependency-free randomness.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// Log-uniform: tiny values as likely as huge ones.
        fn mag(&mut self) -> u64 {
            self.next() >> (self.next() % 64)
        }

        fn mag32(&mut self) -> u32 {
            u32::try_from(self.mag()).unwrap_or(u32::MAX)
        }

        fn flip(&mut self) -> bool {
            self.next() & 1 == 1
        }
    }

    /// Same structure as `base`, every value redrawn (tiny, huge, conditionals on and off).
    /// Presence of PSI stays `Some`; values inside are arbitrary.
    fn revalued(base: &SystemStats, r: &mut Rng) -> SystemStats {
        let mut s = *base;
        s.cpu_busy_mc = r.mag32();
        s.cpu_kernel_mc = r.mag32();
        s.cpu_steal_mc = if r.flip() { r.mag32() } else { 0 };
        s.cpu_wait = if r.flip() {
            CpuWait::Pressure(r.mag32())
        } else {
            CpuWait::RunQueue(r.mag32())
        };
        s.mem_available = r.mag();
        s.mem_free = r.mag();
        s.mem_shmem = if r.flip() { r.mag() } else { 0 };
        s.mem_wait_bp = base.mem_wait_bp.map(|_| r.mag32());
        s.io_wait_bp = base.io_wait_bp.map(|_| r.mag32());
        s.swap_used = if r.flip() { r.mag() } else { 0 };
        s.swap_in_bps = if r.flip() { r.mag() } else { 0 };
        s.tasks_blocked = if r.flip() { r.mag32() } else { 0 };
        s.tasks_zombie = if r.flip() { r.mag32() } else { 0 };
        s
    }

    /// Structure changed under a frozen layout: features appearing and vanishing, no resize.
    fn restructured(base: &SystemStats, r: &mut Rng) -> SystemStats {
        let mut s = revalued(base, r);
        s.cpu_has_steal = r.flip();
        s.mem_wait_bp = r.flip().then(|| r.mag32());
        s.io_wait_bp = r.flip().then(|| r.mag32());
        s.swap_total = if r.flip() { base.swap_total } else { 0 };
        s.num_cores = base.num_cores;
        s
    }

    fn structures() -> Vec<SystemStats> {
        let mut v = vec![full(), sys()];
        let mut t = full();
        t.swap_total = 0;
        v.push(t);
        let mut t = full();
        t.mem_wait_bp = None;
        t.io_wait_bp = None;
        t.num_cores = 4;
        v.push(t);
        let mut t = full();
        t.num_cores = 1;
        t.mem_total = 3 << 30;
        t.swap_total = 1 << 30;
        v.push(t);
        v
    }

    #[test]
    fn widgets_stay_put_across_any_value_or_structure_change() {
        let mut r = Rng(0x9E37_79B9_7F4A_7C15);
        for base in structures() {
            for width in 60..=300 {
                let l = HeaderLayout::new(&base, width);
                let (ref_cpu, ref_mem) = (cpu_marks(&base, &l).marks, mem_marks(&base, &l).marks);
                for i in 0..40 {
                    let s = if i % 2 == 0 {
                        revalued(&base, &mut r)
                    } else {
                        restructured(&base, &mut r)
                    };
                    let (cpu, mem) = (cpu_marks(&s, &l), mem_marks(&s, &l));
                    assert_eq!(cpu.marks, ref_cpu, "cpu slots moved at {width}");
                    assert_eq!(mem.marks, ref_mem, "mem slots moved at {width}");
                    assert!(
                        mem.at <= width || l.mem.bars.mem == 0,
                        "mem line {} wide at {width}",
                        mem.at
                    );
                    assert!(
                        cpu.at + l.cpu_cells() <= width || l.cpu_cells() == 0,
                        "cpu line overflows at {width}"
                    );
                }
            }
        }
    }

    #[test]
    fn layout_reads_structure_not_values() {
        let mut r = Rng(42);
        for base in structures() {
            for width in 60..=300 {
                let l = HeaderLayout::new(&base, width);
                for _ in 0..10 {
                    let m = HeaderLayout::new(&revalued(&base, &mut r), width);
                    assert_eq!(m.cpu_cells(), l.cpu_cells(), "graph at {width}");
                    assert_eq!(m.mem.bars, l.mem.bars, "bars at {width}");
                    assert_eq!(m.mem.present, l.mem.present, "slots at {width}");
                }
            }
        }
    }

    #[test]
    fn drawn_lines_never_exceed_the_width() {
        let mut r = Rng(7);
        for base in structures() {
            for width in 60..=300 {
                let l = HeaderLayout::new(&base, width);
                for _ in 0..10 {
                    let s = revalued(&base, &mut r);
                    let mut w = Width::default();
                    mem_text(&s, &l.mem, &mut w);
                    assert!(l.mem.bars.mem == 0 || w.0 <= width, "{width}: {}", w.0);
                    let mut w = Width::default();
                    cpu_text(&s, &l.cpu, &mut w);
                    let label = 3
                        + digits(u64::from(base.num_cores))
                        + Span::of_cells(l.cpu_cells()).to_string().len();
                    assert!(
                        l.cpu_cells() == 0 || w.0 + l.cpu_cells() + label <= width,
                        "{width}"
                    );
                }
            }
        }
    }

    #[test]
    fn structure_takes_effect_only_at_the_next_layout() {
        let mut s = sys();
        let before = HeaderLayout::new(&s, 120);
        s.swap_total = 8 << 30;
        s.swap_used = 1 << 30;
        s.cpu_has_steal = true;
        // Frozen layout: swapon and steal accounting show nothing and move nothing.
        assert_eq!(
            mem_marks(&s, &before).marks,
            mem_marks(&sys(), &before).marks
        );
        assert!(!mem_str(&s, &before).contains("Swap"));
        let after = HeaderLayout::new(&s, 120);
        assert!(mem_str(&s, &after).contains("Swap"));
        assert_ne!(after.mem.bars, before.mem.bars);
    }

    #[test]
    fn layout_before_the_first_sample_is_marked_uninformed() {
        assert!(!HeaderLayout::new(&SystemStats::default(), 80).is_informed());
        assert!(HeaderLayout::new(&sys(), 80).is_informed());
    }
}
