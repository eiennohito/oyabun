//! Table structure: declared once, drives both the header and every body row.
//!
//! A column owns a leading `sep` (separator spaces) plus a `body` width, so the row
//! is gapless — every screen column belongs to exactly one cell, and repainting a
//! single cell can never leave a stale gap. The last column may `fill` the rest of
//! the line. Fixed-column x-offsets are precomputed once; only the fill column's
//! width depends on the live terminal width.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Clone, Copy)]
pub struct ColSpec {
    pub title: &'static str,
    /// Leading separator spaces (absorbed into right-alignment padding).
    pub sep: u16,
    /// Content width in columns. Ignored for the fill column.
    pub body: u16,
    pub align: Align,
    pub fill: bool,
}

impl ColSpec {
    #[must_use]
    pub const fn right(title: &'static str, sep: u16, body: u16) -> Self {
        Self {
            title,
            sep,
            body,
            align: Align::Right,
            fill: false,
        }
    }

    #[must_use]
    pub const fn left(title: &'static str, sep: u16, body: u16) -> Self {
        Self {
            title,
            sep,
            body,
            align: Align::Left,
            fill: false,
        }
    }

    /// The trailing column that consumes the remaining line width.
    #[must_use]
    pub const fn fill(title: &'static str, sep: u16) -> Self {
        Self {
            title,
            sep,
            body: 0,
            align: Align::Left,
            fill: true,
        }
    }
}

/// Resolved per-column geometry handed to the painter.
#[derive(Clone, Copy)]
pub(crate) struct Geom {
    /// Screen column where this column's separator starts.
    pub x: u16,
    pub sep: u16,
    pub body: u16,
    pub align: Align,
}

pub struct Schema {
    specs: Vec<ColSpec>,
    /// `x[i]` = start column of column `i` (cumulative sep+body of preceding cols).
    x: Vec<u16>,
}

impl Schema {
    /// Build a schema, precomputing fixed-column x-offsets.
    ///
    /// # Panics
    /// If more than one column is a fill column, or a fill column isn't last — both
    /// are structural mistakes in the caller's column list.
    #[must_use]
    pub fn new(specs: Vec<ColSpec>) -> Self {
        assert!(
            specs.iter().filter(|c| c.fill).count() <= 1,
            "at most one fill column"
        );
        if let Some(pos) = specs.iter().position(|c| c.fill) {
            assert_eq!(pos, specs.len() - 1, "fill column must be last");
        }
        let mut x = Vec::with_capacity(specs.len());
        let mut cur = 0u16;
        for c in &specs {
            x.push(cur);
            cur = cur.saturating_add(c.sep).saturating_add(c.body);
        }
        Self { specs, x }
    }

    pub(crate) fn len(&self) -> usize {
        self.specs.len()
    }

    /// Geometry of a fixed column.
    pub(crate) fn geom(&self, i: usize) -> Geom {
        let c = &self.specs[i];
        Geom {
            x: self.x[i],
            sep: c.sep,
            body: c.body,
            align: c.align,
        }
    }

    /// `(x, sep, available_body)` of the fill column at the given terminal width.
    pub(crate) fn fill_geom(&self, i: usize, term_width: u16) -> (u16, u16, u16) {
        let c = &self.specs[i];
        let x = self.x[i];
        let avail = term_width.saturating_sub(x).saturating_sub(c.sep);
        (x, c.sep, avail)
    }

    pub(crate) fn iter_titles(&self) -> impl Iterator<Item = (usize, &ColSpec)> {
        self.specs.iter().enumerate()
    }
}
