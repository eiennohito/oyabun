//! The one sparkline vocabulary: a value on a [`Scale`] becomes a block-glyph level. Both the
//! header's CPU graph and the table's per-row sparklines draw through [`Level`], so the ladder
//! (how a magnitude maps to a glyph, what blank means) cannot diverge between them; they differ
//! only in the scale: the header's stepped linear scale against its own full height, or the
//! rows' shared logarithmic one.
//!
//! **Why rows use a log scale.** A row's glyph answers "how much CPU, roughly" for processes
//! spanning four orders of magnitude. On a linear one-core scale nearly every row is a flat
//! lowest block and anything above a core is pinned at the top; a log ladder gives a 1% process
//! and a 50% one different shapes, and lets multi-core use keep climbing. The ladder is the
//! same for every row so rows stay comparable.
//!
//! **Limitation: ambiguous-width glyphs.** The eight block glyphs (U+2581..U+2588) are
//! East-Asian-Ambiguous width. Terminals configured to render ambiguous characters wide (some
//! CJK locales) draw each as two columns, which pushes the rest of the line right and
//! misaligns the table. oyabun assumes one column per glyph, like the tree-drawing glyphs, and
//! does not detect or compensate.

use std::hash::{Hash, Hasher};

/// Height of one sparkline cell: blank, or one of eight block glyphs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Level(u8);

/// Glyphs by eighths of the full height; index `level - 1`.
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// How a value maps onto the eight block heights.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scale {
    /// Eighths of a `full`-scale reference; overshoot clamps to the full block.
    Linear { full: u64 },
    /// Basis points of one core on the shared ladder of [`LOG_STEPS_BP`].
    Log,
}

/// Lower bounds (bp of one core) of levels 2..=7 on [`Scale::Log`]; level 1 is any nonzero
/// value below the first bound. A factor of ~3 apart at the bottom (3%, 10%, 25%), doubling
/// from half a core up, with the one-core mark at level 6 and two cores or more at the top.
///
/// The ladder stops at the 7/8 block on purpose: table rows are stacked, and a full block
/// touches the cell above, fusing two rows' sparklines into one shape. Keeping the top eighth
/// empty leaves a gap between every pair of rows, at the cost of one height level.
const LOG_STEPS_BP: [u64; 6] = [300, 1_000, 2_500, 5_000, 10_000, 20_000];

impl Level {
    pub const BLANK: Self = Self(0);
    /// The lowest block: something to show, not much.
    pub const LOWEST: Self = Self(1);

    /// `value` on `scale`. Exactly zero is blank — "did nothing" must read differently from
    /// "did a sliver" — and any nonzero value shows at least the lowest block.
    #[must_use]
    pub fn of(value: u64, scale: Scale) -> Self {
        if value == 0 {
            return Self::BLANK;
        }
        match scale {
            Scale::Linear { full } => {
                let eighths = (u128::from(value) * 8).div_ceil(u128::from(full.max(1)));
                Self(u8::try_from(eighths.clamp(1, 8)).unwrap_or(8))
            }
            Scale::Log => {
                let below = LOG_STEPS_BP.partition_point(|&bound| bound <= value);
                Self(u8::try_from(below + 1).unwrap_or(8))
            }
        }
    }

    /// The glyph; a space for blank.
    #[must_use]
    pub fn glyph(self) -> char {
        match self.0 {
            0 => ' ',
            n => BLOCKS[usize::from(n) - 1],
        }
    }

    #[must_use]
    pub fn is_blank(self) -> bool {
        self.0 == 0
    }
}

impl Hash for Level {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u8(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder() {
        let g = |v| Level::of(v, Scale::Linear { full: 8000 }).glyph();
        assert_eq!(g(0), ' ');
        assert_eq!(g(1), '▁', "any activity is visible");
        assert_eq!(g(1000), '▁');
        assert_eq!(g(1001), '▂');
        assert_eq!(g(4000), '▄');
        assert_eq!(g(8000), '█');
        assert_eq!(g(9000), '█', "overshoot clamps");
        assert!(Level::of(0, Scale::Linear { full: 0 }).is_blank());
        assert_eq!(Level::of(u64::MAX, Scale::Linear { full: 1 }).glyph(), '█');
    }

    #[test]
    fn log_ladder_boundaries() {
        let g = |v| Level::of(v, Scale::Log).glyph();
        assert_eq!(g(0), ' ');
        assert_eq!(g(1), '▁', "any nonzero value shows");
        assert_eq!(g(299), '▁');
        assert_eq!(g(300), '▂');
        assert_eq!(g(999), '▂');
        assert_eq!(g(1000), '▃');
        assert_eq!(g(2499), '▃');
        assert_eq!(g(2500), '▄');
        assert_eq!(g(4999), '▄');
        assert_eq!(g(5000), '▅');
        assert_eq!(g(9999), '▅');
        assert_eq!(g(10_000), '▆', "one full core");
        assert_eq!(g(19_999), '▆');
        assert_eq!(g(20_000), '▇');
        assert_eq!(g(40_000), '▇', "the top eighth stays empty");
        assert_eq!(g(u64::MAX), '▇', "rows never reach the full block");
    }
}
