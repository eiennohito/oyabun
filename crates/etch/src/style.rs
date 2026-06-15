//! Cell/line styling. Thin layer over crossterm colors + bold.

use crossterm::queue;
use crossterm::style::{
    Attribute, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};

pub use crossterm::style::Color;

/// Foreground/background/bold for a run of text. `Default` = terminal default.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
}

impl Style {
    pub const NONE: Style = Style {
        fg: None,
        bg: None,
        bold: false,
    };

    #[must_use]
    pub const fn fg(color: Color) -> Style {
        Style {
            fg: Some(color),
            bg: None,
            bold: false,
        }
    }

    #[must_use]
    pub const fn bg(mut self, color: Color) -> Style {
        self.bg = Some(color);
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
            let _ = queue!(out, SetForegroundColor(fg));
        }
        if let Some(bg) = self.bg {
            let _ = queue!(out, SetBackgroundColor(bg));
        }
        if self.bold {
            let _ = queue!(out, SetAttribute(Attribute::Bold));
        }
    }
}

/// Full SGR reset (clears fg/bg and attributes). Cheap and unambiguous.
pub(crate) fn reset(out: &mut Vec<u8>) {
    let _ = queue!(out, SetAttribute(Attribute::Reset), ResetColor);
}
