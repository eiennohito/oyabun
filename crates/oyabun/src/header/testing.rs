//! Shared test fixtures: text/mark sinks and sample machines.

use std::collections::hash_map::DefaultHasher;
use std::fmt::{Display, Write as _};
use std::hash::{Hash, Hasher};

use etch::Rgb;

use crate::procs::{CpuWait, SystemStats};

use super::cpu_line::cpu_text;
use super::layout::HeaderLayout;
use super::mem_line::mem_text;
use super::sink::{Sink, text_width};

/// Collects the pieces' text.
#[derive(Default)]
pub(super) struct Text(String);

impl Sink for Text {
    fn put(&mut self, text: impl Display, _: Rgb) {
        let _ = write!(self.0, "{text}");
    }

    fn cells(&mut self, n: usize, glyph: char, _: Rgb) {
        self.0.extend(std::iter::repeat_n(glyph, n));
    }
}

/// Records the column each slot starts at.
#[derive(Default)]
pub(super) struct Marks {
    pub(super) at: usize,
    pub(super) marks: Vec<(&'static str, usize)>,
}

impl Sink for Marks {
    fn put(&mut self, text: impl Display, _: Rgb) {
        self.at += text_width(text);
    }

    fn cells(&mut self, n: usize, _: char, _: Rgb) {
        self.at += n;
    }

    fn slot(&mut self, name: &'static str) {
        self.marks.push((name, self.at));
    }
}

pub(super) fn cpu_str(s: &SystemStats, l: &HeaderLayout) -> String {
    let mut t = Text::default();
    cpu_text(s, &l.cpu, &mut t);
    t.0
}

pub(super) fn mem_str(s: &SystemStats, l: &HeaderLayout) -> String {
    let mut t = Text::default();
    mem_text(s, &l.mem, &mut t);
    t.0
}

pub(super) fn cpu_marks(s: &SystemStats, l: &HeaderLayout) -> Marks {
    let mut m = Marks::default();
    cpu_text(s, &l.cpu, &mut m);
    m
}

pub(super) fn mem_marks(s: &SystemStats, l: &HeaderLayout) -> Marks {
    let mut m = Marks::default();
    mem_text(s, &l.mem, &mut m);
    m
}

pub(super) fn key(h: &impl Hash) -> u64 {
    let mut s = DefaultHasher::new();
    h.hash(&mut s);
    s.finish()
}

pub(super) fn sys() -> SystemStats {
    SystemStats {
        num_cores: 256,
        cpu_busy_mc: 37_240,
        cpu_kernel_mc: 6_000,
        cpu_wait: CpuWait::Pressure(300),
        mem_total: 64 << 30,
        mem_available: 12 << 30,
        mem_free: 2 << 30,
        ..SystemStats::default()
    }
}

/// A machine with every structural feature present.
pub(super) fn full() -> SystemStats {
    SystemStats {
        cpu_has_steal: true,
        mem_wait_bp: Some(0),
        io_wait_bp: Some(0),
        swap_total: 8 << 30,
        ..sys()
    }
}
