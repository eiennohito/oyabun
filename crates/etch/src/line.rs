//! [`Line`]: a free-form styled line (system-stat header, footer).
//!
//! A line is change-tracked on the *values* that feed it (passed to
//! [`Frame::line`](crate::Frame::line)), exactly like a table cell — so on an unchanged
//! frame the build closure never runs and nothing is formatted. The builder itself just
//! records colored runs into reused buffers; the painter emits them only when a change
//! is detected.

use crate::style::Rgb;

/// A contiguous run of `text` in one foreground color. `None` = terminal default
/// (the padding/fill produced by [`Line::fill`]). Byte offsets into the line's text.
pub(crate) struct Run {
    pub start: usize,
    pub len: usize,
    pub fg: Option<Rgb>,
}

pub struct Line<'a> {
    text: &'a mut String,
    runs: &'a mut Vec<Run>,
    width: u16,
    used: u16,
}

impl<'a> Line<'a> {
    pub(crate) fn new(text: &'a mut String, runs: &'a mut Vec<Run>, width: u16) -> Self {
        text.clear();
        runs.clear();
        Self {
            text,
            runs,
            width,
            used: 0,
        }
    }

    fn push(&mut self, s: &str, fg: Option<Rgb>) {
        let start = self.text.len();
        self.text.push_str(s);
        self.runs.push(Run {
            start,
            len: s.len(),
            fg,
        });
        // Display width of these lines is ASCII / known-width bar glyphs = char count.
        self.used = self
            .used
            .saturating_add(u16::try_from(s.chars().count()).unwrap_or(u16::MAX));
    }

    fn bar_run(&mut self, n: usize, ch: char, fg: Option<Rgb>) {
        if n == 0 {
            return;
        }
        let start = self.text.len();
        for _ in 0..n {
            self.text.push(ch);
        }
        let len = self.text.len() - start;
        self.runs.push(Run { start, len, fg });
        self.used = self
            .used
            .saturating_add(u16::try_from(n).unwrap_or(u16::MAX));
    }

    /// A styled text span.
    pub fn span(&mut self, s: &str, color: Rgb) {
        self.push(s, Some(color));
    }

    /// A bar segment: `n` copies of `ch` in `color`.
    pub fn bar(&mut self, n: usize, ch: char, color: Rgb) {
        self.bar_run(n, ch, Some(color));
    }

    /// `n` spaces in the terminal default color — an uncolored spacer (e.g. the empty
    /// tail of a usage bar, before a trailing label).
    pub fn gap(&mut self, n: usize) {
        self.bar_run(n, ' ', None);
    }

    /// Pad the remaining width with `ch` in the terminal default color.
    pub fn fill(&mut self, ch: char) {
        let n = self.width.saturating_sub(self.used) as usize;
        self.bar_run(n, ch, None);
    }
}
