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

use etch::{Cell, ColSpec, Frame, Rgb, Schema, Style};

use crate::app::App;
use crate::palette;
use crate::procs::{NONE, SystemStats};

/// Number of terminal rows consumed by the system stats header.
pub const HEADER_LINES: u16 = 3;
/// Total non-process rows: header + column header + footer.
pub const CHROME_LINES: u16 = HEADER_LINES + 2;
/// First screen row of the process list (after the 3 stat lines + column header).
const BODY_TOP: u16 = HEADER_LINES + 1;

const HEADER_STYLE: Style = Style::fg(palette::LABEL).bold();

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
    let privileged = app.is_privileged();
    let short_lived = app.short_lived();
    // Gate on every value the footer shows, so a change in any of them repaints it.
    frame.line(
        footer_row,
        (row_count, overflow, privileged, short_lived),
        |l| build_footer(l, row_count, overflow, privileged, short_lived),
    );
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

    l.span(label, palette::LABEL);
    l.bar(user, '|', palette::BAR_BUSY);
    l.bar(system, '|', palette::BAR_SYS);
    l.bar(io, '|', palette::BAR_IO);
    l.gap(empty);
    l.span(&suffix, palette::LABEL);
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

    l.span(mem_prefix, palette::LABEL);
    l.bar(used_cells, '|', palette::BAR_BUSY);
    l.bar(cached_cells, '|', palette::BAR_CACHE);
    l.gap(mem_empty);
    l.span(&mem_suffix, palette::LABEL);

    let swap_frac = frac(sys.swap_used, sys.swap_total, 1.0);
    let swap_cells = (swap_frac * swap_bar as f64) as usize;
    let swap_empty = swap_bar.saturating_sub(swap_cells);

    l.span(swap_prefix, palette::LABEL);
    l.bar(swap_cells, '|', palette::BAR_SYS);
    l.gap(swap_empty);
    l.span(&swap_suffix, palette::LABEL);
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
    l.span(&text, palette::INFO);
    l.fill(' ');
}

fn build_footer(
    l: &mut etch::Line,
    visible_rows: usize,
    pool_overflow: u32,
    privileged: bool,
    short_lived: u32,
) {
    // Mode tag: which observation source is live (BPF privileged vs /proc).
    let (tag, tag_color) = if privileged {
        ("bpf", palette::BAR_BUSY)
    } else {
        ("proc", palette::FOOTER)
    };
    l.span(" [", palette::FOOTER);
    l.span(tag, tag_color);
    l.span("] ", palette::FOOTER);
    let text = format!(
        "Rows: {visible_rows} | q:quit k:kill ↑↓:scroll Enter/→:expand ←:collapse PgUp/PgDn Home/End"
    );
    l.span(&text, palette::FOOTER);
    // A low `RLIMIT_NOFILE` is the only realistic cause; surface it rather than degrade silently.
    if pool_overflow > 0 {
        l.span(
            &format!(" [!{pool_overflow} fd-overflow: raise ulimit -n]"),
            palette::WARN,
        );
    }
    // Short-lived processes caught only by the BPF fork/exit events this cycle.
    if short_lived > 0 {
        l.span(&format!(" [+{short_lived} short-lived]"), palette::NOTICE);
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

            // Selection is the only row-level color: a distinct background, applied to every
            // cell. A change flips the row-level style below, forcing a full-row repaint (so
            // the bg reaches every cell); otherwise each cell gates and colors independently.
            let bg = selected.then_some(palette::SELECTION_BG);
            let cmd = CommandCell {
                prefix: prefix.as_str(),
                prefix_cols,
                text,
                is_name,
                is_kthread: p.is_kthread,
                non_ascii: p.non_ascii,
                suffix_n,
                basename_fg: basename_color(p.exe_deleted, p.uses_deleted_lib),
            };

            table.row(u64::from(p.pid), Style::NONE.with_bg(bg), |r| {
                r.field(p.pid); // PID inherits the row style (no per-cell color)
                r.styled_field(app.uid_name(p.uid), cell(Some(palette::caps(p.caps)), bg));
                r.styled_field(
                    p.display_state as char,
                    cell(Some(palette::state(p.display_state)), bg),
                );
                r.styled_field(p.nice, cell(Some(palette::nice(p.nice)), bg));
                r.styled_field(p.num_threads, cell(palette::threads(p.num_threads), bg));
                r.styled_field(Pct(cpu), cell(Some(palette::cpu(cpu)), bg));
                r.styled_field(Pct(peak), cell(Some(palette::cpu(peak)), bg));
                r.styled_field(Mem(mem), cell(Some(palette::rss(mem)), bg));
                // Command gate: content hash (a changed cmdline reuses a freed store slot, so
                // the handle isn't a stable content identity) + tree prefix + collapse suffix +
                // the flags that drive its per-span colors.
                r.fill(
                    (
                        cmd.prefix,
                        cmd.text,
                        cmd.is_name,
                        cmd.is_kthread,
                        cmd.suffix_n,
                        cmd.basename_fg,
                    ),
                    |c: &mut Cell| write_command(c, &cmd),
                );
            });
        }
    });
}

/// A per-cell style: a semantic foreground over the row's (selection) background, built
/// through etch's `Style` API rather than a raw struct literal.
fn cell(fg: Option<Rgb>, bg: Option<Rgb>) -> Style {
    Style::NONE.with_fg(fg).with_bg(bg)
}

/// The Command-basename color, exe-deleted taking visual priority over deleted-lib.
fn basename_color(exe_deleted: bool, uses_deleted_lib: bool) -> Rgb {
    if exe_deleted {
        palette::EXE_DELETED
    } else if uses_deleted_lib {
        palette::LIB_DELETED
    } else {
        palette::BASENAME
    }
}

/// Everything the Command cell needs to render itself, gathered once per row so the fill
/// closure (and its gate) has a single value to close over.
struct CommandCell<'a> {
    prefix: &'a str,
    prefix_cols: usize,
    text: &'a [u8],
    /// The cmdline was empty/inaccessible, so `text` is the `comm` name, not a command line.
    is_name: bool,
    is_kthread: bool,
    non_ascii: bool,
    suffix_n: u32,
    basename_fg: Rgb,
}

fn write_command(c: &mut Cell, cmd: &CommandCell) {
    c.set_fg(palette::TREE);
    c.glyph(cmd.prefix, cmd.prefix_cols);

    if cmd.is_kthread {
        // Kernel thread (no cmdline): synthesize the familiar `[name]` bracket convention
        // (the kernel stores the name unbracketed), dim.
        c.set_fg(palette::KTHREAD);
        c.ascii(b"[");
        push_text(c, cmd.text, cmd.non_ascii);
        c.ascii(b"]");
    } else if cmd.is_name {
        // Userspace with an empty/inaccessible cmdline: show `comm` plainly — no brackets, it
        // is a real process — colored as a basename.
        c.set_fg(cmd.basename_fg);
        push_text(c, cmd.text, cmd.non_ascii);
    } else {
        write_cmdline(c, cmd.text, cmd.non_ascii, cmd.basename_fg);
    }

    if cmd.suffix_n > 0 {
        c.set_fg(palette::TREE);
        let _ = write!(c, " [+{}]", cmd.suffix_n);
    }
}

/// A userspace command line: dim path prefix, bright (or exe/lib-tinted) basename, and the
/// arguments in the default color. `argv[0]` runs to the first space; its basename is what
/// follows the last `/`. Both split points are ASCII bytes, so the slices stay UTF-8-valid.
fn write_cmdline(c: &mut Cell, text: &[u8], non_ascii: bool, basename_fg: Rgb) {
    let argv0_end = text.iter().position(|&b| b == b' ').unwrap_or(text.len());
    let base_start = text[..argv0_end]
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(0, |i| i + 1);

    if base_start > 0 {
        c.set_fg(palette::PATH);
        push_text(c, &text[..base_start], non_ascii);
    }
    c.set_fg(basename_fg);
    push_text(c, &text[base_start..argv0_end], non_ascii);
    if argv0_end < text.len() {
        c.reset_fg(); // arguments in the terminal default color
        push_text(c, &text[argv0_end..], non_ascii);
    }
}

fn push_text(c: &mut Cell, text: &[u8], non_ascii: bool) {
    if non_ascii {
        c.unicode(text);
    } else {
        c.ascii(text);
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
