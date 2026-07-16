//! [`Display`]: the retained surface, plus the per-frame [`Frame`]/[`Table`]/[`Row`]
//! builders and the paint routines.
//!
//! Each screen row keeps the state that produced its last output. A frame re-declares
//! structure and binds fresh values; only rows/cells whose bound values changed emit
//! anything. All output is batched into one buffer and flushed once on `commit`.

use std::fmt::Display as FmtDisplay;
use std::fmt::Write as _;
use std::hash::Hash;
use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cell::Cell;
use crate::hash::hash_value;
use crate::line::{Line, Run};
use crate::schema::{Align, Geom, Schema};
use crate::style::{self, Rgb, Style};

/// Addresses one cell within the retained grid for change detection.
#[derive(Clone, Copy)]
struct Slot {
    row: u16,
    idx: usize,
    /// The whole row is forced to repaint (new identity at this line, or a style
    /// change) — bypass per-cell change detection.
    full: bool,
}

/// Resolved fill-column geometry at the current terminal width.
#[derive(Clone, Copy)]
struct FillGeom {
    x: u16,
    sep: u16,
    avail: u16,
}

/// What a screen row currently shows, so the next frame can diff against it.
enum RowSlot {
    Empty,
    /// A free-form line, change-tracked by a single content hash.
    Line {
        hash: u64,
    },
    /// A table row, change-tracked per cell plus a row identity and style.
    Table {
        id: u64,
        style: Style,
        cells: Vec<u64>,
    },
}

pub struct Display<W: Write> {
    out: W,
    width: u16,
    height: u16,
    rows: Vec<RowSlot>,
    batch: Vec<u8>,
    /// Reused formatting scratch for one cell/header.
    scratch: String,
    /// Reused color runs for the multi-colored fill cell (parallels [`line_runs`]).
    fill_runs: Vec<Run>,
    /// Reused buffers for the [`Line`] builder.
    line_text: String,
    line_runs: Vec<Run>,
    /// Force a full repaint on the next frame (first frame, or after a resize).
    must_repaint: bool,
}

impl<W: Write> Display<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            width: 0,
            height: 0,
            rows: Vec::new(),
            batch: Vec::new(),
            scratch: String::new(),
            fill_runs: Vec::new(),
            line_text: String::new(),
            line_runs: Vec::new(),
            must_repaint: true,
        }
    }

    /// Borrow the underlying writer (tests inspect captured bytes through this).
    pub fn get_ref(&self) -> &W {
        &self.out
    }

    /// Force the next frame to repaint everything (e.g. after the terminal was
    /// written to out-of-band).
    pub fn invalidate(&mut self) {
        self.must_repaint = true;
    }

    /// Begin a frame at the given terminal size. A size change (or the first frame)
    /// clears the screen and forces a full repaint.
    pub fn begin_frame(&mut self, width: u16, height: u16) -> Frame<'_, W> {
        let full = self.must_repaint || width != self.width || height != self.height;
        self.batch.clear();
        if full {
            self.width = width;
            self.height = height;
            self.rows.clear();
            self.rows.resize_with(height as usize, || RowSlot::Empty);
            let _ = queue!(self.batch, Clear(ClearType::All));
        }
        self.must_repaint = false;
        Frame { d: self, full }
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.batch.is_empty() {
            self.out.write_all(&self.batch)?;
            self.out.flush()?;
        }
        Ok(())
    }

    // --- row lifecycle ---

    /// Prepare a table row's slot; returns whether the whole row must repaint
    /// (frame-full, new identity at this line, or a style change).
    fn begin_row(
        &mut self,
        row: u16,
        id: u64,
        style: Style,
        ncols: usize,
        frame_full: bool,
    ) -> bool {
        if usize::from(row) >= self.rows.len() {
            return false;
        }
        match &mut self.rows[row as usize] {
            RowSlot::Table {
                id: pid,
                style: st,
                cells,
            } if cells.len() == ncols => {
                if *pid == id {
                    // Same process at this line: per-cell change detection applies.
                    let full = frame_full || *st != style;
                    *st = style;
                    full
                } else {
                    // Different process (scroll / churn): reuse the buffer, reset hashes.
                    *pid = id;
                    *st = style;
                    cells.fill(0);
                    true
                }
            }
            slot => {
                // First use of this line, or a column-count change (resize): allocate.
                *slot = RowSlot::Table {
                    id,
                    style,
                    cells: vec![0; ncols],
                };
                true
            }
        }
    }

    /// Compare a cell's new hash against the stored one and update it. Returns
    /// whether the cell must repaint (changed, or the row is forced).
    fn cell_changed(&mut self, slot: Slot, g: u64) -> bool {
        if usize::from(slot.row) >= self.rows.len() {
            return false;
        }
        if let RowSlot::Table { cells, .. } = &mut self.rows[slot.row as usize] {
            if !slot.full && cells[slot.idx] == g {
                return false;
            }
            cells[slot.idx] = g;
            true
        } else {
            false
        }
    }

    /// Bind a fixed-width field with an explicit per-cell style. The change key
    /// hashes `(value, style)`, so a color-band crossing (same value, new color)
    /// repaints even though the text is unchanged. When unchanged (and no forced
    /// repaint) the value's `Display` impl is never invoked.
    fn paint_field<T: FmtDisplay + Hash>(
        &mut self,
        slot: Slot,
        style: Style,
        geom: Geom,
        value: &T,
    ) {
        if !self.cell_changed(slot, hash_value(&(value, style))) {
            return;
        }
        self.scratch.clear();
        let _ = write!(self.scratch, "{value}");
        paint_fixed(&mut self.batch, slot.row, geom, style, &self.scratch);
    }

    /// Bind the fill column. The render closure runs only when changed / forced
    /// repaint; otherwise no formatting happens. The cell records color runs the fill
    /// painter emits with per-run SGR.
    fn paint_fill<G: Hash>(
        &mut self,
        slot: Slot,
        style: Style,
        geom: FillGeom,
        key: &G,
        render: impl FnOnce(&mut Cell),
    ) {
        if !self.cell_changed(slot, hash_value(key)) {
            return;
        }
        self.scratch.clear();
        self.fill_runs.clear();
        {
            let mut cell = Cell::new(&mut self.scratch, &mut self.fill_runs, geom.avail as usize);
            render(&mut cell);
        }
        paint_fill_emit(
            &mut self.batch,
            slot.row,
            geom.x,
            geom.sep,
            style,
            &self.scratch,
            &self.fill_runs,
        );
    }

    /// Blank a row that is no longer occupied.
    fn clear_row(&mut self, row: u16) {
        if usize::from(row) >= self.rows.len() || matches!(self.rows[row as usize], RowSlot::Empty)
        {
            return;
        }
        let _ = queue!(self.batch, MoveTo(0, row));
        style::reset(&mut self.batch);
        let _ = queue!(self.batch, Clear(ClearType::UntilNewLine));
        self.rows[row as usize] = RowSlot::Empty;
    }

    // --- free-form lines ---

    fn paint_line<G: Hash>(
        &mut self,
        row: u16,
        frame_full: bool,
        key: &G,
        build: impl FnOnce(&mut Line),
    ) {
        if usize::from(row) >= self.rows.len() {
            return;
        }
        let hash = hash_value(key);
        if !frame_full
            && matches!(&self.rows[row as usize], RowSlot::Line { hash: h } if *h == hash)
        {
            return; // unchanged: the build closure never runs — no formatting, no I/O
        }
        {
            let mut line = Line::new(&mut self.line_text, &mut self.line_runs, self.width);
            build(&mut line);
        }
        let _ = queue!(self.batch, MoveTo(0, row));
        for run in &self.line_runs {
            let slice = &self.line_text[run.start..run.start + run.len];
            style::set_fg(&mut self.batch, run.fg);
            let _ = queue!(self.batch, Print(slice));
        }
        style::reset(&mut self.batch);
        let _ = queue!(self.batch, Clear(ClearType::UntilNewLine));
        self.rows[row as usize] = RowSlot::Line { hash };
    }

    fn paint_header(&mut self, row: u16, frame_full: bool, schema: &Schema, style: Style) {
        if usize::from(row) >= self.rows.len() {
            return;
        }
        self.scratch.clear();
        for (i, spec) in schema.iter_titles() {
            if spec.fill {
                for _ in 0..spec.sep {
                    self.scratch.push(' ');
                }
                self.scratch.push_str(spec.title);
            } else {
                let geom = schema.geom(i);
                push_aligned(&mut self.scratch, geom, spec.title);
            }
        }
        let hash = hash_value(&(self.scratch.as_str(), style));
        let changed = !matches!(&self.rows[row as usize], RowSlot::Line { hash: h } if *h == hash);
        if !frame_full && !changed {
            return;
        }
        let _ = queue!(self.batch, MoveTo(0, row));
        if !style.is_default() {
            style.enter(&mut self.batch);
        }
        let _ = queue!(self.batch, Print(&self.scratch));
        style::reset(&mut self.batch);
        let _ = queue!(self.batch, Clear(ClearType::UntilNewLine));
        self.rows[row as usize] = RowSlot::Line { hash };
    }
}

// ---------------------------------------------------------------------------
// Per-frame builders
// ---------------------------------------------------------------------------

pub struct Frame<'a, W: Write> {
    d: &'a mut Display<W>,
    full: bool,
}

impl<W: Write> Frame<'_, W> {
    /// Terminal size `(width, height)` this frame is being drawn at.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        (self.d.width, self.d.height)
    }

    /// A free-form styled line (system stats, footer), change-tracked on the
    /// value(s) that determine its content. When unchanged the `build` closure
    /// never runs — no formatting, no allocation, no I/O.
    pub fn line<G: Hash>(&mut self, row: u16, key: G, build: impl FnOnce(&mut Line)) {
        self.d.paint_line(row, self.full, &key, build);
    }

    /// The table's column-title row, derived from the schema.
    pub fn header(&mut self, row: u16, schema: &Schema, style: Style) {
        self.d.paint_header(row, self.full, schema, style);
    }

    /// The table body: rows `top .. top + height`. Rows the closure doesn't emit are
    /// blanked.
    pub fn table<'s>(
        &mut self,
        schema: &'s Schema,
        top: u16,
        height: u16,
        build: impl FnOnce(&mut Table<'_, 's, W>),
    ) {
        let mut table = Table {
            d: &mut *self.d,
            schema,
            top,
            height,
            full: self.full,
            used: 0,
        };
        build(&mut table);
        let used = table.used;
        for off in used..height {
            self.d.clear_row(top + off);
        }
    }

    /// Flush the frame's queued output in a single write.
    ///
    /// # Errors
    /// Propagates any I/O error from writing to / flushing the underlying writer.
    pub fn commit(self) -> io::Result<()> {
        self.d.flush()
    }
}

pub struct Table<'d, 's, W: Write> {
    d: &'d mut Display<W>,
    schema: &'s Schema,
    top: u16,
    height: u16,
    full: bool,
    used: u16,
}

impl<'s, W: Write> Table<'_, 's, W> {
    /// Bind one row, identified by `id` (the process PID). When `id` differs from the
    /// previous frame's row at this screen line, the whole row repaints.
    pub fn row(&mut self, id: u64, style: Style, build: impl FnOnce(&mut Row<'_, 's, W>)) {
        if self.used >= self.height {
            return;
        }
        let screen_row = self.top + self.used;
        self.used += 1;
        let row_full = self
            .d
            .begin_row(screen_row, id, style, self.schema.len(), self.full);
        let mut row = Row {
            d: &mut *self.d,
            schema: self.schema,
            y: screen_row,
            idx: 0,
            full: row_full,
            style,
        };
        build(&mut row);
        debug_assert_eq!(
            row.idx,
            self.schema.len(),
            "row bound {} fields but schema has {} columns",
            row.idx,
            self.schema.len()
        );
    }
}

pub struct Row<'d, 's, W: Write> {
    d: &'d mut Display<W>,
    schema: &'s Schema,
    /// Screen row (terminal line) this row paints to.
    y: u16,
    idx: usize,
    full: bool,
    style: Style,
}

impl<W: Write> Row<'_, '_, W> {
    /// Bind the next fixed column to a value in the row's style. The value is both
    /// formatted (`Display`) and hashed (`Hash`) — the same value for both, so the
    /// change key can never drift from what's shown.
    pub fn field<T: FmtDisplay + Hash>(&mut self, value: T) {
        self.styled_field(value, self.style);
    }

    /// Like [`field`](Self::field) but overrides the row style for this one cell — the
    /// per-cell coloring path (magnitude gradients, categorical roles). The change key
    /// hashes `(value, style)`, so a same-value color-band crossing still repaints.
    pub fn styled_field<T: FmtDisplay + Hash>(&mut self, value: T, style: Style) {
        let i = self.idx;
        self.idx += 1;
        let geom = self.schema.geom(i);
        let slot = Slot {
            row: self.y,
            idx: i,
            full: self.full,
        };
        self.d.paint_field(slot, style, geom, &value);
    }

    /// Bind the fill column: an explicit change key plus a render closure (run
    /// only when changed) for content too complex for a single `Display`.
    pub fn fill<G: Hash>(&mut self, key: G, render: impl FnOnce(&mut Cell)) {
        let i = self.idx;
        self.idx += 1;
        let (x, sep, avail) = self.schema.fill_geom(i, self.d.width);
        let slot = Slot {
            row: self.y,
            idx: i,
            full: self.full,
        };
        self.d
            .paint_fill(slot, self.style, FillGeom { x, sep, avail }, &key, render);
    }
}

// ---------------------------------------------------------------------------
// Painting (free fns over the batch buffer — testable, no borrow tangles)
// ---------------------------------------------------------------------------

fn write_spaces(out: &mut Vec<u8>, n: usize) {
    out.extend(std::iter::repeat_n(b' ', n));
}

/// Truncate `content` to at most `body` display columns, returning the slice and its
/// column width. ASCII (the overwhelming case — pids, counts, percentages) takes a
/// byte-length fast path; otherwise width is measured with `unicode-width`, so a
/// wide-character value (e.g. a CJK username in the USER column) can't under-measure
/// and overflow its cell.
fn fit(content: &str, body: usize) -> (&str, usize) {
    if content.is_ascii() {
        let len = content.len();
        return if len > body {
            (&content[..body], body)
        } else {
            (content, len)
        };
    }
    let total = UnicodeWidthStr::width(content);
    if total <= body {
        return (content, total);
    }
    // Truncate to `body` columns at a char boundary.
    let mut w = 0;
    let mut end = content.len();
    for (i, ch) in content.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > body {
            end = i;
            break;
        }
        w += cw;
    }
    (&content[..end], w)
}

fn paint_fixed(out: &mut Vec<u8>, row: u16, geom: Geom, style: Style, content: &str) {
    let _ = queue!(out, MoveTo(geom.x, row));
    let styled = !style.is_default();
    if styled {
        style.enter(out);
    }
    let (content, clen) = fit(content, geom.body as usize);
    let pad = geom.body as usize - clen;
    match geom.align {
        Align::Right => {
            write_spaces(out, geom.sep as usize + pad);
            let _ = queue!(out, Print(content));
        }
        Align::Left => {
            write_spaces(out, geom.sep as usize);
            let _ = queue!(out, Print(content));
            write_spaces(out, pad);
        }
    }
    if styled {
        style::reset(out);
    }
}

fn paint_fill_emit(
    out: &mut Vec<u8>,
    row: u16,
    x: u16,
    sep: u16,
    style: Style,
    content: &str,
    runs: &[Run],
) {
    let _ = queue!(out, MoveTo(x, row));
    // The base style (selection bg / bold) applies to the whole cell; each run overrides
    // only the foreground (a `None` run falls back to the style's fg).
    style.enter(out);
    write_spaces(out, sep as usize);
    let mut cur: Option<Option<Rgb>> = None;
    for run in runs {
        let fg = run.fg.or(style.fg);
        if cur != Some(fg) {
            style::set_fg(out, fg);
            cur = Some(fg);
        }
        let _ = queue!(out, Print(&content[run.start..run.start + run.len]));
    }
    // Clear to EOL: erases a shrunken cmdline's tail, and (under a selection bg)
    // extends the highlight to the screen edge.
    let _ = queue!(out, Clear(ClearType::UntilNewLine));
    // Always reset: a colored run leaves foreground state that must not bleed into the
    // next row even when the base style itself was the terminal default.
    style::reset(out);
}

/// Append `content` aligned within a fixed column's geometry to a string (header use).
fn push_aligned(buf: &mut String, geom: Geom, content: &str) {
    let (content, clen) = fit(content, geom.body as usize);
    let pad = geom.body as usize - clen;
    match geom.align {
        Align::Right => {
            for _ in 0..geom.sep as usize + pad {
                buf.push(' ');
            }
            buf.push_str(content);
        }
        Align::Left => {
            for _ in 0..geom.sep as usize {
                buf.push(' ');
            }
            buf.push_str(content);
            for _ in 0..pad {
                buf.push(' ');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn fit_ascii() {
        assert_eq!(fit("ab", 5), ("ab", 2));
        assert_eq!(fit("abcdef", 3), ("abc", 3));
        assert_eq!(fit("abc", 3), ("abc", 3));
    }

    #[test]
    fn fit_wide_chars_measure_by_display_width() {
        // CJK are width-2: "日本" is 4 columns, not 2 chars.
        assert_eq!(fit("日本", 10), ("日本", 4));
        assert_eq!(fit("日本", 4), ("日本", 4));
        // Truncating to 3 columns keeps only the first wide char (2 ≤ 3, next would be 4).
        assert_eq!(fit("日本", 3), ("日", 2));
        // A wide char that doesn't fit at all yields an empty slice.
        assert_eq!(fit("日", 1), ("", 0));
    }
}
