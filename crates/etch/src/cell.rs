//! [`Cell`]: a width-tracked, multi-color text sink for the fill column.
//!
//! Fixed columns are pure ASCII and padded by the framework, so they don't need
//! this. The fill (Command) column mixes known-width tree glyphs (`●├─│▾`) with a
//! cmdline that is ASCII 99% of the time and occasionally CJK/emoji, and it colors
//! parts of its content independently (dim path prefix, bright basename, muted tree
//! connectors). [`Cell`] tracks display columns explicitly so it can truncate at the
//! terminal edge without grapheme-segmenting the common ASCII path, and records the
//! written text as color **runs** the fill painter emits with per-run SGR.

use std::fmt;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::line::Run;
use crate::style::Rgb;

pub struct Cell<'a> {
    out: &'a mut String,
    runs: &'a mut Vec<Run>,
    used: usize,
    max: usize,
    /// Active foreground for subsequent writes; `None` = the row/base style's fg.
    cur_fg: Option<Rgb>,
}

impl<'a> Cell<'a> {
    pub(crate) fn new(out: &'a mut String, runs: &'a mut Vec<Run>, max: usize) -> Self {
        Self {
            out,
            runs,
            used: 0,
            max,
            cur_fg: None,
        }
    }

    /// Display columns consumed so far.
    #[must_use]
    pub fn width(&self) -> usize {
        self.used
    }

    fn room(&self) -> usize {
        self.max - self.used
    }

    /// Change the active foreground for subsequent writes.
    pub fn set_fg(&mut self, color: Rgb) {
        self.cur_fg = Some(color);
    }

    /// Revert to the row/base style's foreground for subsequent writes.
    pub fn reset_fg(&mut self) {
        self.cur_fg = None;
    }

    /// Fold the bytes appended since `start` into the run list under the active color,
    /// extending the last run when the color is unchanged (adjacent same-color writes
    /// coalesce, so runs alternate colors).
    fn record(&mut self, start: usize) {
        let added = self.out.len() - start;
        if added == 0 {
            return;
        }
        match self.runs.last_mut() {
            Some(r) if r.fg == self.cur_fg => r.len += added,
            _ => self.runs.push(Run {
                start,
                len: added,
                fg: self.cur_fg,
            }),
        }
    }

    /// A run of known display width — tree connectors, bullets, markers. The caller
    /// asserts the width (these are fixed constants), so no measurement happens.
    pub fn glyph(&mut self, s: &str, width: usize) {
        if width <= self.room() {
            let start = self.out.len();
            self.out.push_str(s);
            self.used += width;
            self.record(start);
        }
    }

    /// ASCII bytes, one column each. Non-printable/non-ASCII bytes render as `?`
    /// (defensive — comm is kernel ASCII, cmdline NULs are pre-cleaned to spaces).
    pub fn ascii(&mut self, bytes: &[u8]) {
        let start = self.out.len();
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
        self.record(start);
    }

    /// Possibly-wide bytes: lossy-decode and measure each char with `unicode-width`.
    /// Only taken for the ~1% of processes flagged non-ASCII at parse time. Control
    /// characters are neutralized to `?` — the same defensive policy as [`ascii`](Self::ascii),
    /// and load-bearing here: process comm/cmdline is untrusted (any local user's `argv` /
    /// `prctl(PR_SET_NAME)`), so a raw ESC or other C0/C1 byte reaching this path would be
    /// written straight to the terminal's control channel. `char::is_control` is exactly the
    /// Unicode `Cc` category (C0 `0x00–0x1F`, DEL `0x7F`, C1 `0x80–0x9F`).
    pub fn unicode(&mut self, bytes: &[u8]) {
        let start = self.out.len();
        for ch in String::from_utf8_lossy(bytes).chars() {
            let (ch, w) = if ch.is_control() {
                ('?', 1)
            } else {
                (ch, UnicodeWidthChar::width(ch).unwrap_or(0))
            };
            if self.used + w > self.max {
                break;
            }
            self.out.push(ch);
            self.used += w;
        }
        self.record(start);
    }
}

/// `write!` support for ASCII text (numbers, separators). Width = char count.
impl fmt::Write for Cell<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let start = self.out.len();
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
        self.record(start);
        Ok(())
    }
}
