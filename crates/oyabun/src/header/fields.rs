//! Fixed-width value formatters: a value in a slot that never changes width.

use std::fmt::{self, Display, Write as _};

use crate::gather::REFRESH_MS;
use crate::ui::Mem;

use super::sink::text_width;

/// `T` right-aligned in a field of `width` columns (a value wider than the field is printed
/// whole, so callers bound their values). Measures with a counting writer: no allocation.
pub(super) struct Right<T>(pub(super) T, pub(super) usize);

impl<T: Display> Display for Right<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for _ in text_width(&self.0)..self.1 {
            f.write_char(' ')?;
        }
        self.0.fmt(f)
    }
}

/// `n` blank columns: a reserved slot with nothing in it.
pub(super) struct Blank(pub(super) usize);

impl Display for Blank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for _ in 0..self.0 {
            f.write_char(' ')?;
        }
        Ok(())
    }
}

/// Busy cores from millicores, one decimal.
pub(super) struct Cores(u32);

impl Cores {
    /// A cores field: the value capped at the core count (a share of the machine cannot exceed
    /// it) in a slot as wide as the largest such value, so it never changes width.
    pub(super) fn field(mc: u32, num_cores: u32) -> Right<Self> {
        let cap = num_cores.max(1).saturating_mul(1000);
        Right(Self(mc.min(cap)), Self::width(num_cores))
    }

    /// Width of the widest cores value on a machine of `num_cores`: its digits and `.9`.
    pub(super) fn width(num_cores: u32) -> usize {
        digits(u64::from(num_cores.max(1))) + 2
    }
}

impl Display for Cores {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths = (self.0 + 50) / 100;
        write!(f, "{}.{}", tenths / 10, tenths % 10)
    }
}

/// Whole-percent pressure from basis points (at most `100%`).
pub(super) struct Wait(u32);

/// Columns of the widest [`Wait`]: `100%`.
pub(super) const WAIT_W: usize = 4;

impl Wait {
    pub(super) fn field(bp: u32) -> Right<Self> {
        Right(Self(bp), WAIT_W)
    }
}

impl Display for Wait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}%", (self.0.min(10_000) + 50) / 100)
    }
}

/// Columns of a memory size field: the widest [`Mem`] text.
pub(super) const MEM_W: usize = Mem::MAX_WIDTH;

pub(super) fn mem_field(bytes: u64) -> Right<Mem> {
    Right(Mem(bytes), MEM_W)
}

/// A task count field, capped so it never outgrows its slot.
pub(super) const TASKS_W: usize = 4;
pub(super) const TASKS_MAX: u32 = 9999;

pub(super) fn tasks_field(n: u32) -> Right<u32> {
    Right(n.min(TASKS_MAX), TASKS_W)
}

/// Elapsed time the graph spans: `75s`, `2m30s` (seconds up to two minutes, where they still
/// read better).
pub(super) struct Span(u64);

impl Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0;
        if secs < 120 {
            return write!(f, "{secs}s");
        }
        match (secs / 60, secs % 60) {
            (m, 0) => write!(f, "{m}m"),
            (m, s) => write!(f, "{m}m{s:02}s"),
        }
    }
}

impl Span {
    /// The span of `cells` graph cells, one per refresh, rounded up to whole seconds.
    pub(super) fn of_cells(cells: usize) -> Self {
        Self((cells as u64 * REFRESH_MS).div_ceil(1000))
    }

    /// The widest label any span up to `bound_secs` can print. Not monotone in the span
    /// (`119s` is wider than `2m`), so this is a bound, not the width at `bound_secs`.
    pub(super) fn max_width(bound_secs: u64) -> usize {
        if bound_secs < 120 {
            digits(bound_secs) + 1
        } else {
            digits(bound_secs / 60) + "m00s".len()
        }
    }
}

/// ` /<scale> <span>`: the graph's full height in cores and the time it covers.
pub(super) struct Label {
    pub(super) scale: u32,
    pub(super) span: Span,
}

impl Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, " /{} {}", self.scale, self.span)
    }
}

pub(super) fn digits(n: u64) -> usize {
    n.checked_ilog10().map_or(1, |d| d as usize + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cores_and_percent_round() {
        assert_eq!(Cores(37_240).to_string(), "37.2");
        assert_eq!(Cores(949).to_string(), "0.9");
        assert_eq!(Cores(950).to_string(), "1.0");
        assert_eq!(Wait(300).to_string(), "3%");
        assert_eq!(Wait(49).to_string(), "0%");
        assert_eq!(Wait(u32::MAX).to_string(), "100%");
    }

    #[test]
    fn span_labels() {
        assert_eq!(Span(75).to_string(), "75s");
        assert_eq!(Span(150).to_string(), "2m30s");
        assert_eq!(Span(120).to_string(), "2m");
        assert_eq!(Span(59).to_string(), "59s");
        assert_eq!(Span(601).to_string(), "10m01s");
        assert_eq!(Span::of_cells(1).to_string(), "1s", "rounded up, never 0s");
    }

    #[test]
    fn span_max_width_bounds_every_smaller_span() {
        for bound in 0..=7200u64 {
            let widest = (0..=bound.min(7200))
                .rev()
                .take(130)
                .map(|s| Span(s).to_string().len())
                .max()
                .unwrap();
            assert!(Span::max_width(bound) >= widest, "bound {bound}");
        }
        assert_eq!(Span::max_width(601), 6, "10m01s");
    }
}
