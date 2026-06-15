//! ratatui rendering. Each on-screen row builds a single owned line string (the one
//! allocation a `Paragraph` line requires); numeric columns are written in place
//! with no intermediate allocations, and indentation comes from a static slice.

use std::fmt::Write as _;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::snapshot::NONE;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_header(frame, chunks[0]);
    render_body(frame, chunks[1], app);
    render_footer(frame, chunks[2], app.rows().len());
}

fn render_header(frame: &mut Frame, area: Rect) {
    let header = Line::from(Span::styled(
        format!(
            "  {:>7} {:<8} {} {:>7} {:>7} {:>8} {}",
            "PID", "USER", "S", "CPU%", "PEAK", "MEM", "COMMAND"
        ),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(header), area);
}

fn render_body(frame: &mut Frame, area: Rect, app: &App) {
    let visible = area.height as usize;
    let snap = app.snapshot();
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(visible);

    for (display_idx, row) in app
        .rows()
        .iter()
        .enumerate()
        .skip(app.scroll())
        .take(visible)
    {
        let p = &snap.procs[row.proc_idx];
        let selected = display_idx == app.selected();
        let marker = if p.first_child == NONE {
            " "
        } else if row.collapsed {
            "▸"
        } else {
            "▾"
        };

        let mut text = String::with_capacity(96);
        write_indent(&mut text, row.depth);
        text.push_str(marker);
        if row.collapsed && p.subtree_size > 0 {
            let _ = write!(text, "[+{}]", p.subtree_size);
        }
        let _ = write!(
            text,
            " {:>7} {:<8} {} ",
            p.pid,
            app.uid_name(p.uid),
            p.state as char
        );
        write_pct(&mut text, p.cpu_pct);
        text.push(' ');
        write_pct(&mut text, p.cpu_peak);
        let _ = write!(text, " {:>8} ", format_mem(p.mem_bytes));
        text.push_str(&String::from_utf8_lossy(snap.strings.get(p.name)));

        let style = if selected {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        } else {
            match p.state {
                b'R' => Style::default().fg(Color::Green),
                b'Z' => Style::default().fg(Color::Red),
                b'T' | b't' => Style::default().fg(Color::Yellow),
                _ => Style::default(),
            }
        };

        lines.push(Line::styled(text, style));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_footer(frame: &mut Frame, area: Rect, visible_rows: usize) {
    let footer = Line::from(Span::styled(
        format!(
            " Rows: {visible_rows} | q:quit k:kill ↑↓:scroll Enter/→:expand ←:collapse PgUp/PgDn Home/End"
        ),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(footer), area);
}

/// Write `depth` levels of two-space indent from a static slice (no allocation).
fn write_indent(out: &mut String, depth: u16) {
    const SPACES: &str = "                                                                "; // 64
    let n = (depth as usize * 2).min(SPACES.len());
    out.push_str(&SPACES[..n]);
}

/// Write basis points as `"N.NN%"` right-aligned to width 7, no allocation.
fn write_pct(out: &mut String, bp: u32) {
    let (whole, frac) = (bp / 100, bp % 100);
    let len = decimal_len(whole) + 4; // "<whole>.NN%"
    for _ in len..7 {
        out.push(' ');
    }
    let _ = write!(out, "{whole}.{frac:02}%");
}

fn decimal_len(n: u32) -> usize {
    let mut len = 1;
    let mut v = n;
    while v >= 10 {
        v /= 10;
        len += 1;
    }
    len
}

#[allow(clippy::cast_precision_loss)] // display formatting only
fn format_mem(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;

    if bytes >= GIB {
        format!("{:.1}G", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1}M", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{}K", bytes / KIB)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mem_units() {
        assert_eq!(format_mem(0), "0B");
        assert_eq!(format_mem(512), "512B");
        assert_eq!(format_mem(1024), "1K");
        assert_eq!(format_mem(1024 * 1024), "1.0M");
        assert_eq!(format_mem(1024 * 1024 * 1024), "1.0G");
        assert_eq!(format_mem(1536 * 1024), "1.5M");
    }
}
