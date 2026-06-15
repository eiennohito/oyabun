//! ratatui rendering.
//!
//! Layout (top → bottom):
//! - System stats header (3 lines): CPU bar, Mem/Swap bars, Load/Tasks/Uptime.
//! - Column header (1 line).
//! - Process list (fills remaining space): htop-style tree in the Command column.
//! - Footer (1 line): keybinds.
//!
//! Collapsed rows show subtree-aggregate CPU%/MEM and a `▸ [+N]` marker.
//! Tree connectors (`├─`/`└─`/`│`) are drawn only in the Command column.

use std::fmt::Write as _;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::snapshot::{NONE, SystemStats};

/// Number of terminal rows consumed by the system stats header.
pub const HEADER_LINES: u16 = 3;
/// Total non-process rows: header + column header + footer.
pub const CHROME_LINES: u16 = HEADER_LINES + 2;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(HEADER_LINES),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_sys_stats(frame, chunks[0], &app.snapshot().sys);
    render_column_header(frame, chunks[1]);
    render_body(frame, chunks[2], app);
    render_footer(frame, chunks[3], app.rows().len());
}

// ---------------------------------------------------------------------------
// System stats header
// ---------------------------------------------------------------------------

fn render_sys_stats(frame: &mut Frame, area: Rect, sys: &SystemStats) {
    let w = area.width as usize;
    let lines = vec![
        build_cpu_line(sys, w),
        build_mem_line(sys, w),
        build_info_line(sys),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

/// Build a colored bar segment: `n` copies of `ch` pushed into `buf`.
fn bar_segment(spans: &mut Vec<Span<'static>>, buf: &mut String, n: usize, ch: char, color: Color) {
    if n == 0 {
        return;
    }
    buf.clear();
    for _ in 0..n {
        buf.push(ch);
    }
    spans.push(Span::styled(buf.clone(), Style::default().fg(color)));
}

fn build_cpu_line(sys: &SystemStats, width: usize) -> Line<'static> {
    let label = "CPU[".to_string();
    let suffix = format!(
        " {:.1}%]  ({} cores)",
        f64::from(sys.cpu_user_bp + sys.cpu_sys_bp + sys.cpu_iowait_bp) / 100.0,
        sys.num_cores
    );
    let bar_width = width.saturating_sub(label.len() + suffix.len()).max(4);

    let user_cells = (sys.cpu_user_bp as usize * bar_width / 10000).min(bar_width);
    let sys_cells = (sys.cpu_sys_bp as usize * bar_width / 10000).min(bar_width - user_cells);
    let io_cells =
        (sys.cpu_iowait_bp as usize * bar_width / 10000).min(bar_width - user_cells - sys_cells);
    let empty_cells = bar_width - user_cells - sys_cells - io_cells;

    let mut spans = Vec::with_capacity(6);
    let mut buf = String::with_capacity(bar_width);
    spans.push(Span::styled(label, Style::default().fg(Color::Cyan)));
    bar_segment(&mut spans, &mut buf, user_cells, '|', Color::Green);
    bar_segment(&mut spans, &mut buf, sys_cells, '|', Color::Red);
    bar_segment(&mut spans, &mut buf, io_cells, '|', Color::Blue);
    bar_segment(&mut spans, &mut buf, empty_cells, ' ', Color::Reset);
    spans.push(Span::styled(suffix, Style::default().fg(Color::Cyan)));
    Line::from(spans)
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn build_mem_line(sys: &SystemStats, width: usize) -> Line<'static> {
    let used_label = format_mem(sys.mem_used);
    let total_label = format_mem(sys.mem_total);
    let mem_prefix = "Mem[".to_string();
    let mem_suffix = format!(" {used_label}/{total_label}]");

    let swap_used_label = format_mem(sys.swap_used);
    let swap_total_label = format_mem(sys.swap_total);
    let swap_prefix = "  Swp[".to_string();
    let swap_suffix = format!(" {swap_used_label}/{swap_total_label}]");

    let chrome = mem_prefix.len() + mem_suffix.len() + swap_prefix.len() + swap_suffix.len();
    let total_bar = width.saturating_sub(chrome).max(8);
    let mem_bar = total_bar * 2 / 3;
    let swap_bar = total_bar - mem_bar;

    let mut spans = Vec::with_capacity(10);
    let mut buf = String::with_capacity(total_bar);

    // Mem bar
    spans.push(Span::styled(mem_prefix, Style::default().fg(Color::Cyan)));
    let used_frac = if sys.mem_total > 0 {
        (sys.mem_used as f64 / sys.mem_total as f64).min(1.0)
    } else {
        0.0
    };
    let cached_frac = if sys.mem_total > 0 {
        (sys.mem_cached as f64 / sys.mem_total as f64).min(1.0 - used_frac)
    } else {
        0.0
    };
    let used_cells = (used_frac * mem_bar as f64) as usize;
    let cached_cells = (cached_frac * mem_bar as f64) as usize;
    let empty = mem_bar.saturating_sub(used_cells + cached_cells);
    bar_segment(&mut spans, &mut buf, used_cells, '|', Color::Green);
    bar_segment(&mut spans, &mut buf, cached_cells, '|', Color::Yellow);
    bar_segment(&mut spans, &mut buf, empty, ' ', Color::Reset);
    spans.push(Span::styled(mem_suffix, Style::default().fg(Color::Cyan)));

    // Swap bar
    spans.push(Span::styled(swap_prefix, Style::default().fg(Color::Cyan)));
    let swap_frac = if sys.swap_total > 0 {
        (sys.swap_used as f64 / sys.swap_total as f64).min(1.0)
    } else {
        0.0
    };
    let swap_cells = (swap_frac * swap_bar as f64) as usize;
    let swap_empty = swap_bar.saturating_sub(swap_cells);
    bar_segment(&mut spans, &mut buf, swap_cells, '|', Color::Red);
    bar_segment(&mut spans, &mut buf, swap_empty, ' ', Color::Reset);
    spans.push(Span::styled(swap_suffix, Style::default().fg(Color::Cyan)));

    Line::from(spans)
}

fn build_info_line(sys: &SystemStats) -> Line<'static> {
    let total_tasks = sys.tasks_running
        + sys.tasks_sleeping
        + sys.tasks_stopped
        + sys.tasks_zombie
        + sys.tasks_idle;
    let uptime = format_uptime(sys.uptime_secs);
    let text = format!(
        " Load: {:.2} {:.2} {:.2}  Tasks: {} ({} run, {} slp, {} stp, {} zmb)  Up: {}",
        f64::from(sys.load[0]) / 100.0,
        f64::from(sys.load[1]) / 100.0,
        f64::from(sys.load[2]) / 100.0,
        total_tasks,
        sys.tasks_running,
        sys.tasks_sleeping,
        sys.tasks_stopped,
        sys.tasks_zombie,
        uptime,
    );
    Line::styled(text, Style::default().fg(Color::White))
}

fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

// ---------------------------------------------------------------------------
// Column header
// ---------------------------------------------------------------------------

fn render_column_header(frame: &mut Frame, area: Rect) {
    let header = Line::from(Span::styled(
        format!(
            "  {:>7} {:<8} {} {:>3} {:>4} {:>7} {:>7} {:>8}  {}",
            "PID", "USER", "S", "NI", "THR", "CPU%", "PEAK", "RSS", "Command"
        ),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(header), area);
}

// ---------------------------------------------------------------------------
// Process list body
// ---------------------------------------------------------------------------

/// Update guide state for one row (depth 3+ tree connectors only).
/// `guides[d]` tracks whether depth `d` has a continuing `│` line.
/// Must be called for every row in order — including off-screen rows above the
/// viewport — so visible rows inherit correct state.
fn advance_guides(guides: &mut Vec<bool>, depth: usize, has_next: bool) {
    // Only depths ≥ 3 participate in guide tracking.
    if depth < 3 {
        guides.clear();
        return;
    }
    let rel = depth - 3;
    if guides.len() <= rel {
        guides.resize(rel + 1, false);
    }
    guides[rel] = has_next;
    guides.truncate(rel + 1);
}

/// Tree tiers:
///   depth 0 = root (init, kthreadd)           → no prefix
///   depth 1 = L1 (services, children of root)  → `● ` / `○ `
///   depth 2 = L2 (first-level workers)          → `  • ` / `  ◦ `
///   depth 3+= L3+ (deep nesting)               → 6-char base + `├─`/`└─`/`│` connectors
const L3_BASE: &str = "      "; // 6 chars (aligns with L1 bullet + L2 dot columns)

/// Write the tier-based tree prefix into `text`. Returns whether the collapse
/// state was already encoded in the prefix (L1/L2 circles).
#[allow(clippy::fn_params_excessive_bools)]
fn write_tree_prefix(
    text: &mut String,
    depth: usize,
    has_children: bool,
    collapsed: bool,
    has_next: bool,
    single_root: bool,
    guides: &[bool],
) -> bool {
    match depth {
        0 => false,
        1 => {
            text.push_str(if has_children && collapsed {
                "○ "
            } else {
                "● "
            });
            true
        }
        2 => {
            text.push_str(if has_children && collapsed {
                "  ◦ "
            } else {
                "  • "
            });
            true
        }
        _ => {
            text.push_str(L3_BASE);
            for d in 3..depth {
                let rel = d - 3;
                if rel < guides.len() && guides[rel] {
                    text.push_str("│  ");
                } else {
                    text.push_str("   ");
                }
            }
            let is_trivial_root = depth == 0 && !has_next && single_root;
            if !is_trivial_root {
                text.push_str(if has_next { "├─ " } else { "└─ " });
            }
            false
        }
    }
}

fn render_body(frame: &mut Frame, area: Rect, app: &App) {
    let visible = area.height as usize;
    let snap = app.snapshot();
    let scroll = app.scroll();
    let rows = app.rows();
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(visible);
    let mut guides: Vec<bool> = Vec::with_capacity(16);
    let single_root = {
        let first = snap.first_root;
        first != NONE && snap.procs[first as usize].next_sibling == NONE
    };

    // Phase 1: walk rows above the viewport to build guide state (no rendering).
    for row in rows.iter().take(scroll) {
        let p = &snap.procs[row.proc_idx];
        advance_guides(&mut guides, row.depth as usize, p.next_sibling != NONE);
    }

    // Phase 2: render the visible window with correct guide context.
    for (display_idx, row) in rows.iter().enumerate().skip(scroll).take(visible) {
        let p = &snap.procs[row.proc_idx];
        let selected = display_idx == app.selected();
        let has_children = p.first_child != NONE;
        let has_next = p.next_sibling != NONE;
        let depth = row.depth as usize;

        // --- numeric columns (fixed-width left part) ---
        let (cpu, peak, mem) = if row.collapsed && p.subtree_size > 0 {
            (p.subtree_cpu, p.cpu_peak, p.subtree_mem)
        } else {
            (p.cpu_pct, p.cpu_peak, p.mem_bytes)
        };

        let mut text = String::with_capacity(128);
        let user = app.uid_name(p.uid);
        let user = &user[..user.floor_char_boundary(8)];
        let _ = write!(
            text,
            "  {:>7} {:<8} {} {:>3} {:>4} ",
            p.pid, user, p.state as char, p.nice, p.num_threads
        );
        write_pct(&mut text, cpu);
        text.push(' ');
        write_pct(&mut text, peak);
        let _ = write!(text, " {:>8}  ", format_mem(mem));

        let collapse_shown = write_tree_prefix(
            &mut text,
            depth,
            has_children,
            row.collapsed,
            has_next,
            single_root,
            &guides,
        );
        advance_guides(&mut guides, depth, has_next);

        // Collapse marker for tiers that don't encode it in the prefix.
        if !collapse_shown && has_children {
            if row.collapsed {
                text.push('▸');
            } else {
                text.push('▾');
            }
            text.push(' ');
        }

        // comm is kernel-limited ASCII; cmdline had NULs cleaned to spaces.
        // Push bytes directly — no UTF-8 validation or Cow allocation needed.
        let cmdline = snap.strings.get(p.cmdline);
        if cmdline.is_empty() {
            text.push('[');
            push_ascii(&mut text, snap.strings.get(p.name));
            text.push(']');
        } else {
            push_ascii(&mut text, cmdline);
        }

        if row.collapsed && p.subtree_size > 0 {
            let _ = write!(text, " [+{}]", p.subtree_size);
        }

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

// ---------------------------------------------------------------------------
// Footer
// ---------------------------------------------------------------------------

fn render_footer(frame: &mut Frame, area: Rect, visible_rows: usize) {
    let footer = Line::from(Span::styled(
        format!(
            " Rows: {visible_rows} | q:quit k:kill ↑↓:scroll Enter/→:expand ←:collapse PgUp/PgDn Home/End"
        ),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(footer), area);
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// Push bytes that are expected to be ASCII. Non-ASCII bytes are replaced with `?`
/// (defensive — comm is kernel ASCII, cmdline is NUL-cleaned).
fn push_ascii(out: &mut String, bytes: &[u8]) {
    for &b in bytes {
        if b.is_ascii() && b != 0 {
            out.push(b as char);
        } else {
            out.push('?');
        }
    }
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

#[allow(clippy::cast_precision_loss)]
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

    #[test]
    fn format_uptime_ranges() {
        assert_eq!(format_uptime(0), "0m");
        assert_eq!(format_uptime(90), "1m");
        assert_eq!(format_uptime(3661), "1h 1m");
        assert_eq!(format_uptime(90061), "1d 1h 1m");
    }
}
