//! Cell/line styling: a 24-bit RGB foreground/background plus bold.
//!
//! [`Rgb`] is the only color type callers see; etch emits nothing but crossterm's
//! `Color::Rgb` escapes — no ANSI-16 named colors, no 256-color fallback — and the
//! conversion is internal to this module's paint path. `None` on `fg`/`bg` means the
//! terminal default.

use crossterm::queue;
use crossterm::style::{
    Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};

/// A 24-bit RGB color. The one color etch renders; magnitude gradients and categorical
/// roles alike resolve to an `Rgb` before reaching the paint path.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    fn to_crossterm(self) -> Color {
        Color::Rgb {
            r: self.0,
            g: self.1,
            b: self.2,
        }
    }
}

/// Foreground/background/bold for a run of text. `None` = terminal default.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
}

impl Style {
    pub const NONE: Style = Style {
        fg: None,
        bg: None,
        bold: false,
    };

    #[must_use]
    pub const fn fg(color: Rgb) -> Style {
        Style {
            fg: Some(color),
            bg: None,
            bold: false,
        }
    }

    #[must_use]
    pub const fn bg(mut self, color: Rgb) -> Style {
        self.bg = Some(color);
        self
    }

    /// Set the foreground from an optional color (`None` = terminal default) — the per-cell
    /// semantic color, which is absent for columns that inherit the row default.
    #[must_use]
    pub const fn with_fg(mut self, fg: Option<Rgb>) -> Style {
        self.fg = fg;
        self
    }

    /// Set the background from an optional color — the selection highlight, applied to
    /// every cell of the selected row and to nothing otherwise.
    #[must_use]
    pub const fn with_bg(mut self, bg: Option<Rgb>) -> Style {
        self.bg = bg;
        self
    }

    #[must_use]
    pub const fn bold(mut self) -> Style {
        self.bold = true;
        self
    }

    pub(crate) fn is_default(self) -> bool {
        self == Style::NONE
    }

    /// Emit the SGR escapes to enter this style. Caller emits [`reset`] afterwards.
    pub(crate) fn enter(self, out: &mut Vec<u8>) {
        if let Some(fg) = self.fg {
            let _ = queue!(out, SetForegroundColor(fg.to_crossterm()));
        }
        if let Some(bg) = self.bg {
            let _ = queue!(out, SetBackgroundColor(bg.to_crossterm()));
        }
        if self.bold {
            let _ = queue!(out, SetAttribute(Attribute::Bold));
        }
    }
}

/// Emit an SGR foreground color, or reset-to-default for `None`. Used by the run
/// painters (lines and the multi-colored fill cell) to switch color mid-line.
pub(crate) fn set_fg(out: &mut Vec<u8>, fg: Option<Rgb>) {
    let color = fg.map_or(Color::Reset, Rgb::to_crossterm);
    let _ = queue!(out, SetForegroundColor(color));
}

/// Full SGR reset (clears fg/bg and attributes). Cheap and unambiguous.
pub(crate) fn reset(out: &mut Vec<u8>) {
    let _ = queue!(out, SetAttribute(Attribute::Reset), ResetColor);
}
