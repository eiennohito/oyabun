//! Rendering via `etch` (retained-mode, value-gated).
//!
//! Layout (top → bottom):
//! - System stats header (3 lines): CPU bar, Mem/Swap bars, Load/Tasks/Uptime.
//! - Column header (1 line).
//! - Process list (fills remaining space): htop-style tree in the Command column.
//! - Footer (1 line): keybinds.
//!
//! The table's column geometry lives in one [`Schema`] (see [`columns`]) that drives
//! both the header and every body row. Each body cell binds a value that is *both*
//! formatted and gated, so an unchanged column does zero work; the Command column
//! gates on a content hash (the arena offset is not stable across snapshots) plus the
//! tree prefix. Collapsed rows show subtree-aggregate CPU%/MEM and a `▸ [+N]` marker.

use std::fmt;
use std::fmt::Write as _;
use std::io::Write;

use etch::{Cell, ColSpec, Color, Frame, Schema, Style};

use crate::app::App;
use crate::procs::{NONE, SystemStats};

/// Number of terminal rows consumed by the system stats header.
pub const HEADER_LINES: u16 = 3;
/// Total non-process rows: header + column header + footer.
pub const CHROME_LINES: u16 = HEADER_LINES + 2;
/// First screen row of the process list (after the 3 stat lines + column header).
const BODY_TOP: u16 = HEADER_LINES + 1;

const HEADER_STYLE: Style = Style::fg(Color::Cyan).bold();

/// The table's column structure — declared once, shared by header and body rows.
#[must_use]
pub fn columns() -> Schema {
    Schema::new(vec![
        ColSpec::right("PID", 2, 7),
        ColSpec::left("USER", 1, 8),
        ColSpec::left("S", 1, 1),
        ColSpec::right("NI", 1, 3),
        ColSpec::right("THR", 1, 4),
        ColSpec::right("CPU%", 1, 7),
        ColSpec::right("PEAK", 1, 7),
        ColSpec::right("RSS", 1, 8),
        ColSpec::fill("Command", 2),
    ])
}

pub fn render<W: Write>(frame: &mut Frame<W>, app: &App, schema: &Schema) {
    let (width, height) = frame.size();
    let sys = *app.sys();
    let wsz = width as usize;

    // Each stat line is gated on the values that feed it: a width change forces a full
    // repaint anyway, so the data alone is the gate. On interactive (non-gather) frames
    // the build closures never run — no `format!`, no allocation.
    frame.line(0, sys, |l| build_cpu_line(l, &sys, wsz));
    frame.line(1, sys, |l| build_mem_line(l, &sys, wsz));
    frame.line(2, sys, |l| build_info_line(l, &sys));
    frame.header(HEADER_LINES, schema, HEADER_STYLE);

    let body_height = height.saturating_sub(CHROME_LINES);
    render_body(frame, app, schema, body_height);

    let footer_row = height.saturating_sub(1);
    let row_count = app.rows().len();
    let overflow = app.pool_overflow();
    // Gate on both, so an overflow change repaints the footer.
    frame.line(footer_row, (row_count, overflow), |l| {
        build_footer(l, row_count, overflow);
    });
}

// ---------------------------------------------------------------------------
// System stats header
// ---------------------------------------------------------------------------

fn build_cpu_line(l: &mut etch::Line, sys: &SystemStats, width: usize) {
    let suffix = format!(
        " {:.1}%]  ({} cores)",
        f64::from(sys.cpu_user_bp + sys.cpu_sys_bp + sys.cpu_iowait_bp) / 100.0,
        sys.num_cores
    );
    let label = "CPU[";
    let bar_width = width.saturating_sub(label.len() + suffix.len()).max(4);

    let user = (sys.cpu_user_bp as usize * bar_width / 10000).min(bar_width);
    let system = (sys.cpu_sys_bp as usize * bar_width / 10000).min(bar_width - user);
    let io = (sys.cpu_iowait_bp as usize * bar_width / 10000).min(bar_width - user - system);
    let empty = bar_width - user - system - io;

    l.span(label, Color::Cyan);
    l.bar(user, '|', Color::Green);
    l.bar(system, '|', Color::Red);
    l.bar(io, '|', Color::Blue);
    l.bar(empty, ' ', Color::Reset);
    l.span(&suffix, Color::Cyan);
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn build_mem_line(l: &mut etch::Line, sys: &SystemStats, width: usize) {
    let mem_suffix = format!(" {}/{}]", Mem(sys.mem_used), Mem(sys.mem_total));
    let swap_suffix = format!(" {}/{}]", Mem(sys.swap_used), Mem(sys.swap_total));
    let mem_prefix = "Mem[";
    let swap_prefix = "  Swp[";

    let chrome = mem_prefix.len() + mem_suffix.len() + swap_prefix.len() + swap_suffix.len();
    let total_bar = width.saturating_sub(chrome).max(8);
    let mem_bar = total_bar * 2 / 3;
    let swap_bar = total_bar - mem_bar;

    let used_frac = frac(sys.mem_used, sys.mem_total, 1.0);
    let cached_frac = frac(sys.mem_cached, sys.mem_total, 1.0 - used_frac);
    let used_cells = (used_frac * mem_bar as f64) as usize;
    let cached_cells = (cached_frac * mem_bar as f64) as usize;
    let mem_empty = mem_bar.saturating_sub(used_cells + cached_cells);

    l.span(mem_prefix, Color::Cyan);
    l.bar(used_cells, '|', Color::Green);
    l.bar(cached_cells, '|', Color::Yellow);
    l.bar(mem_empty, ' ', Color::Reset);
    l.span(&mem_suffix, Color::Cyan);

    let swap_frac = frac(sys.swap_used, sys.swap_total, 1.0);
    let swap_cells = (swap_frac * swap_bar as f64) as usize;
    let swap_empty = swap_bar.saturating_sub(swap_cells);

    l.span(swap_prefix, Color::Cyan);
    l.bar(swap_cells, '|', Color::Red);
    l.bar(swap_empty, ' ', Color::Reset);
    l.span(&swap_suffix, Color::Cyan);
}

/// `num / den` clamped to `[0, cap]`; 0 when `den == 0`.
#[allow(clippy::cast_precision_loss)]
fn frac(num: u64, den: u64, cap: f64) -> f64 {
    if den == 0 {
        0.0
    } else {
        (num as f64 / den as f64).min(cap)
    }
}

fn build_info_line(l: &mut etch::Line, sys: &SystemStats) {
    let total_tasks = sys.tasks_running
        + sys.tasks_sleeping
        + sys.tasks_stopped
        + sys.tasks_zombie
        + sys.tasks_idle;
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
        format_uptime(sys.uptime_secs),
    );
    l.span(&text, Color::White);
    l.fill(' ');
}

fn build_footer(l: &mut etch::Line, visible_rows: usize, pool_overflow: u32) {
    let text = format!(
        " Rows: {visible_rows} | q:quit k:kill ↑↓:scroll Enter/→:expand ←:collapse PgUp/PgDn Home/End"
    );
    l.span(&text, Color::DarkGrey);
    // A low `RLIMIT_NOFILE` is the only realistic cause; surface it rather than degrade silently.
    if pool_overflow > 0 {
        l.span(
            &format!(" [!{pool_overflow} fd-overflow: raise ulimit -n]"),
            Color::Yellow,
        );
    }
    l.fill(' ');
}

// ---------------------------------------------------------------------------
// Process list body
// ---------------------------------------------------------------------------

fn render_body<W: Write>(frame: &mut Frame<W>, app: &App, schema: &Schema, body_height: u16) {
    let procs = app.procs().as_slice();
    let scroll = app.scroll();
    let rows = app.rows();

    // Phase 1: advance guide state over rows above the viewport (no rendering).
    let mut guides: Vec<bool> = Vec::with_capacity(16);
    for row in rows.iter().take(scroll) {
        let p = &procs[row.proc_idx];
        advance_guides(&mut guides, row.depth as usize, p.next_sibling != NONE);
    }

    let mut prefix = String::with_capacity(64);
    frame.table(schema, BODY_TOP, body_height, |table| {
        for (display_idx, row) in rows
            .iter()
            .enumerate()
            .skip(scroll)
            .take(body_height as usize)
        {
            let p = &procs[row.proc_idx];
            let depth = row.depth as usize;
            let has_children = p.first_child != NONE;
            let has_next = p.next_sibling != NONE;
            let selected = display_idx == app.selected();

            let (cpu, peak, mem) = if row.collapsed && p.subtree_size > 0 {
                (p.subtree_cpu, p.cpu_peak, p.subtree_mem)
            } else {
                (p.cpu_pct, p.cpu_peak, p.mem_bytes)
            };

            prefix.clear();
            let prefix_cols = build_prefix(
                &mut prefix,
                depth,
                has_children,
                row.collapsed,
                has_next,
                &guides,
            );
            advance_guides(&mut guides, depth, has_next);

            let cmdline = app.cmdline(p);
            let (text, is_name) = if cmdline.is_empty() {
                (p.comm(), true)
            } else {
                (cmdline, false)
            };
            let suffix_n = if row.collapsed && p.subtree_size > 0 {
                p.subtree_size
            } else {
                0
            };
            let non_ascii = p.non_ascii;

            let style = row_style(selected, p.state);
            let user = app.uid_name(p.uid);

            table.row(u64::from(p.pid), style, |r| {
                r.field(p.pid);
                r.field(user);
                r.field(p.state as char);
                r.field(p.nice);
                r.field(p.num_threads);
                r.field(Pct(cpu));
                r.field(Pct(peak));
                r.field(Mem(mem));
                // Command: content hash (a changed cmdline reuses a freed store slot, so the
                // handle isn't a stable content identity) + tree prefix + collapse suffix.
                r.fill(
                    (prefix.as_str(), text, is_name, suffix_n),
                    |c: &mut Cell| {
                        write_command(c, &prefix, prefix_cols, text, is_name, non_ascii, suffix_n);
                    },
                );
            });
        }
    });
}

#[allow(clippy::similar_names)]
fn write_command(
    c: &mut Cell,
    prefix: &str,
    prefix_cols: usize,
    text: &[u8],
    is_name: bool,
    non_ascii: bool,
    suffix_n: u32,
) {
    c.glyph(prefix, prefix_cols);
    if is_name {
        c.ascii(b"[");
        push_text(c, text, non_ascii);
        c.ascii(b"]");
    } else {
        push_text(c, text, non_ascii);
    }
    if suffix_n > 0 {
        let _ = write!(c, " [+{suffix_n}]");
    }
}

fn push_text(c: &mut Cell, text: &[u8], non_ascii: bool) {
    if non_ascii {
        c.unicode(text);
    } else {
        c.ascii(text);
    }
}

fn row_style(selected: bool, state: u8) -> Style {
    if selected {
        Style::fg(Color::White).bg(Color::DarkGrey)
    } else {
        match state {
            b'R' => Style::fg(Color::Green),
            b'Z' => Style::fg(Color::Red),
            b'T' | b't' => Style::fg(Color::Yellow),
            _ => Style::NONE,
        }
    }
}

// ---------------------------------------------------------------------------
// Tree prefix (htop-style connectors)
// ---------------------------------------------------------------------------

/// Update guide state for one row (depth 3+ tree connectors only).
/// `guides[d]` tracks whether depth `d` has a continuing `│` line.
/// Must be called for every row in order — including off-screen rows above the
/// viewport — so visible rows inherit correct state.
fn advance_guides(guides: &mut Vec<bool>, depth: usize, has_next: bool) {
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

/// 6-char base for depth 3+ (aligns with the L1 bullet + L2 dot columns).
const L3_BASE: &str = "      ";

/// Write the full tree prefix (tier marker + connectors + collapse marker) for a row.
/// Returns its display width in columns — every glyph here is width-1, so the width is
/// accumulated from the known chunk lengths as we build, with no after-the-fact scan.
fn build_prefix(
    out: &mut String,
    depth: usize,
    has_children: bool,
    collapsed: bool,
    has_next: bool,
    guides: &[bool],
) -> usize {
    let mut cols = 0usize;
    let collapse_shown = match depth {
        0 => false,
        1 => {
            out.push_str(if has_children && collapsed {
                "○ "
            } else {
                "● "
            });
            cols += 2;
            true
        }
        2 => {
            out.push_str(if has_children && collapsed {
                "  ◦ "
            } else {
                "  • "
            });
            cols += 4;
            true
        }
        _ => {
            out.push_str(L3_BASE);
            cols += 6;
            for d in 3..depth {
                let rel = d - 3;
                out.push_str(if rel < guides.len() && guides[rel] {
                    "│  "
                } else {
                    "   "
                });
                cols += 3;
            }
            out.push_str(if has_next { "├─ " } else { "└─ " });
            cols += 3;
            false
        }
    };

    if !collapse_shown && has_children {
        out.push(if collapsed { '▸' } else { '▾' });
        out.push(' ');
        cols += 2;
    }
    cols
}

// ---------------------------------------------------------------------------
// Value formatters: Display (what's shown) + Hash (the gate). Same value for both,
// so the gate can never drift from the rendered text.
// ---------------------------------------------------------------------------

/// CPU% in basis points, rendered as `N.NN%`.
#[derive(Hash)]
struct Pct(u32);

impl fmt::Display for Pct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}%", self.0 / 100, self.0 % 100)
    }
}

/// Resident memory in bytes, rendered in human units.
#[derive(Hash)]
struct Mem(u64);

impl fmt::Display for Mem {
    #[allow(clippy::cast_precision_loss)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * 1024;
        const GIB: u64 = 1024 * 1024 * 1024;
        let b = self.0;
        if b >= GIB {
            write!(f, "{:.1}G", b as f64 / GIB as f64)
        } else if b >= MIB {
            write!(f, "{:.1}M", b as f64 / MIB as f64)
        } else if b >= KIB {
            write!(f, "{}K", b / KIB)
        } else {
            write!(f, "{b}B")
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_format() {
        assert_eq!(Pct(0).to_string(), "0.00%");
        assert_eq!(Pct(1234).to_string(), "12.34%");
        assert_eq!(Pct(40000).to_string(), "400.00%");
    }

    #[test]
    fn mem_units() {
        assert_eq!(Mem(0).to_string(), "0B");
        assert_eq!(Mem(512).to_string(), "512B");
        assert_eq!(Mem(1024).to_string(), "1K");
        assert_eq!(Mem(1024 * 1024).to_string(), "1.0M");
        assert_eq!(Mem(1024 * 1024 * 1024).to_string(), "1.0G");
        assert_eq!(Mem(1536 * 1024).to_string(), "1.5M");
    }

    #[test]
    fn uptime_ranges() {
        assert_eq!(format_uptime(0), "0m");
        assert_eq!(format_uptime(90), "1m");
        assert_eq!(format_uptime(3661), "1h 1m");
        assert_eq!(format_uptime(90061), "1d 1h 1m");
    }
}
