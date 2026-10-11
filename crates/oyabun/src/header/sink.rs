//! One line description, three consumers: measure, key, emit.

use std::fmt::{self, Display, Write as _};
use std::hash::{Hash, Hasher};

use etch::Rgb;

use crate::palette;

/// Receives a line's colored pieces, in order: text and runs of one bar glyph.
pub(super) trait Sink {
    fn put(&mut self, text: impl Display, color: Rgb);

    /// `n` cells of `glyph` (a blank glyph is an uncolored gap).
    fn cells(&mut self, n: usize, glyph: char, color: Rgb);

    /// Called where a slot starts, whether or not it has anything to show. Only the geometry
    /// tests listen.
    fn slot(&mut self, _name: &'static str) {}

    /// A labelled value: `label` then `value`.
    fn pair(&mut self, label: &str, value: impl Display, color: Rgb) {
        self.put(label, palette::LABEL);
        self.put(value, color);
    }
}

/// Display columns of everything put (all text here is ASCII).
#[derive(Default)]
pub(super) struct Width(pub(super) usize);

impl fmt::Write for Width {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

impl Sink for Width {
    fn put(&mut self, text: impl Display, _: Rgb) {
        let _ = write!(self, "{text}");
    }

    fn cells(&mut self, n: usize, _: char, _: Rgb) {
        self.0 += n;
    }
}

/// Columns `text` prints.
pub(super) fn text_width(text: impl Display) -> usize {
    let mut w = Width::default();
    let _ = write!(w, "{text}");
    w.0
}

/// Feeds the pieces (text and color) into a hasher: the repaint key.
pub(super) struct Keyed<'h, H: Hasher>(pub(super) &'h mut H);

impl<H: Hasher> fmt::Write for Keyed<'_, H> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.write(s.as_bytes());
        Ok(())
    }
}

impl<H: Hasher> Sink for Keyed<'_, H> {
    fn put(&mut self, text: impl Display, color: Rgb) {
        color.hash(self.0);
        let _ = write!(self, "{text}");
        self.0.write_u8(0xff);
    }

    fn cells(&mut self, n: usize, glyph: char, color: Rgb) {
        n.hash(self.0);
        glyph.hash(self.0);
        color.hash(self.0);
        self.0.write_u8(0xff);
    }
}

/// A fixed stack buffer that truncates instead of growing — pieces are short.
pub(super) struct Scratch {
    pub(super) buf: [u8; 64],
    pub(super) len: usize,
}

impl fmt::Write for Scratch {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

impl Scratch {
    pub(super) fn new() -> Self {
        Self {
            buf: [0; 64],
            len: 0,
        }
    }

    pub(super) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

/// Emits the pieces onto a line, clipped to `left` columns.
pub(super) struct Emit<'l, 'a> {
    pub(super) line: &'l mut etch::Line<'a>,
    pub(super) left: usize,
}

impl Sink for Emit<'_, '_> {
    fn put(&mut self, text: impl Display, color: Rgb) {
        if self.left == 0 {
            return;
        }
        let mut s = Scratch::new();
        let _ = write!(s, "{text}");
        let s = s.as_str();
        let take = s.len().min(self.left);
        self.line.span(&s[..take], color);
        self.left -= take;
    }

    fn cells(&mut self, n: usize, glyph: char, color: Rgb) {
        let n = n.min(self.left);
        if n == 0 {
            return;
        }
        if glyph == ' ' {
            self.line.gap(n);
        } else {
            self.line.bar(n, glyph, color);
        }
        self.left -= n;
    }
}
