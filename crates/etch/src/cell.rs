//! [`Cell`]: a width-tracked text sink for the fill column.
//!
//! Fixed columns are pure ASCII and padded by the framework, so they don't need
//! this. The fill (Command) column mixes known-width tree glyphs (`●├─│▾`) with a
//! cmdline that is ASCII 99% of the time and occasionally CJK/emoji. [`Cell`] tracks
//! display columns explicitly so it can truncate at the terminal edge without
//! grapheme-segmenting the common ASCII path.

use std::fmt;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub struct Cell<'a> {
    out: &'a mut String,
    used: usize,
    max: usize,
}

impl<'a> Cell<'a> {
    pub(crate) fn new(out: &'a mut String, max: usize) -> Self {
        Self { out, used: 0, max }
    }

    /// Display columns consumed so far.
    #[must_use]
    pub fn width(&self) -> usize {
        self.used
    }

    fn room(&self) -> usize {
        self.max - self.used
    }

    /// A run of known display width — tree connectors, bullets, markers. The caller
    /// asserts the width (these are fixed constants), so no measurement happens.
    pub fn glyph(&mut self, s: &str, width: usize) {
        if width <= self.room() {
            self.out.push_str(s);
            self.used += width;
        }
    }

    /// ASCII bytes, one column each. Non-printable/non-ASCII bytes render as `?`
    /// (defensive — comm is kernel ASCII, cmdline NULs are pre-cleaned to spaces).
    pub fn ascii(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.used >= self.max {
                break;
            }
            self.out.push(if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '?'
            });
            self.used += 1;
        }
    }

    /// Possibly-wide bytes: lossy-decode and measure each char with `unicode-width`.
    /// Only taken for the ~1% of processes flagged non-ASCII at parse time.
    pub fn unicode(&mut self, bytes: &[u8]) {
        for ch in String::from_utf8_lossy(bytes).chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if self.used + w > self.max {
                break;
            }
            self.out.push(ch);
            self.used += w;
        }
    }
}

/// `write!` support for ASCII text (numbers, separators). Width = char count.
impl fmt::Write for Cell<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let w = UnicodeWidthStr::width(s);
        if w <= self.room() {
            self.out.push_str(s);
            self.used += w;
        } else {
            for ch in s.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if self.used + cw > self.max {
                    break;
                }
                self.out.push(ch);
                self.used += cw;
            }
        }
        Ok(())
    }
}
