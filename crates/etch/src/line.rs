//! [`Line`]: a free-form styled line (system-stat header, footer).
//!
//! A line is gated on the *values* that feed it (passed to [`Frame::line`](crate::Frame::line)),
//! exactly like a table cell — so on an unchanged frame the build closure never runs
//! and nothing is formatted. The builder itself just records colored runs into reused
//! buffers; the painter emits them only when the gate misses.

use crate::style::Color;

pub(crate) struct Run {
    pub start: usize,
    pub len: usize,
    pub color: Color,
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

    fn push(&mut self, s: &str, color: Color) {
        let start = self.text.len();
        self.text.push_str(s);
        self.runs.push(Run {
            start,
            len: s.len(),
            color,
        });
        // Display width of these lines is ASCII / known-width bar glyphs = char count.
        self.used = self
            .used
            .saturating_add(u16::try_from(s.chars().count()).unwrap_or(u16::MAX));
    }

    /// A styled text span.
    pub fn span(&mut self, s: &str, color: Color) {
        self.push(s, color);
    }

    /// A bar segment: `n` copies of `ch` in `color`.
    pub fn bar(&mut self, n: usize, ch: char, color: Color) {
        if n == 0 {
            return;
        }
        let start = self.text.len();
        for _ in 0..n {
            self.text.push(ch);
        }
        let len = self.text.len() - start;
        self.runs.push(Run { start, len, color });
        self.used = self
            .used
            .saturating_add(u16::try_from(n).unwrap_or(u16::MAX));
    }

    /// Pad the remaining width with `ch` in the default color.
    pub fn fill(&mut self, ch: char) {
        let n = self.width.saturating_sub(self.used) as usize;
        if n > 0 {
            self.bar(n, ch, Color::Reset);
        }
    }
}
