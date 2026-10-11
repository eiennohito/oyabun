//! Rendering via `etch` (retained-mode, change-tracked).
//!
//! Layout (top → bottom):
//! - System header (2 lines, +1 with a GPU): CPU numbers + history graph, then memory and
//!   pressure leads (see [`crate::header`]; the GPU line is a pair of usage bars).
//! - Column header (1 line).
//! - Process list (fills remaining space): htop-style tree in the Command column.
//! - Footer (1 line): keybinds.
//!
//! The table's column geometry lives in one [`Schema`] (see [`columns`]) that drives
//! both the header and every body row. Each body cell binds a value that is *both*
//! formatted and change-tracked, so an unchanged column does zero work; the Command
//! column uses a content hash as its change key (the arena offset is not stable across
//! snapshots) plus the tree prefix.
//!
//! A collapsed row (a folded process or application group) shows the sum over everything it
//! hides, its own TREND sparkline, and a hidden-process badge. A folded group's other top members
//! emit no row of their own (the group row counts them). The memory column is PSS;
//! a `~` marker flags a total that includes a per-process resident-set fallback.

use std::fmt;
use std::fmt::Write as _;
use std::io::Write;

use etch::{Cell, ColSpec, Frame, Rgb, Row, Schema, Style};

use crate::app::App;
use crate::gather::{CpuSeries, SPARK_CELLS};
use crate::header;
use crate::palette;
use crate::procs::{GpuMetric, SystemStats};
use crate::sparkline::{Level, Scale};
use crate::view::RowKind;

const CPU_HEADER_LINES: u16 = 2;

/// Number of terminal rows consumed by the fixed-startup system header.
#[must_use]
pub const fn header_lines(gpu: bool) -> u16 {
    CPU_HEADER_LINES + gpu as u16
}

/// Total non-process rows: system header + column header + footer.
#[must_use]
pub const fn chrome_lines(gpu: bool) -> u16 {
    header_lines(gpu) + 2
}

/// Sparkline column width: one terminal column per history cell.
#[allow(clippy::cast_possible_truncation)] // bounded by the assert
const SPARK_COLS: u16 = {
    assert!(SPARK_CELLS <= u16::MAX as usize);
    SPARK_CELLS as u16
};

const HEADER_STYLE: Style = Style::fg(palette::LABEL).bold();

/// The table's column structure — declared once, shared by header and body rows.
#[must_use]
pub fn columns(gpu: bool) -> Schema {
    Schema::new(column_specs(gpu))
}

fn column_specs(gpu: bool) -> Vec<ColSpec> {
    let mut columns = vec![
        ColSpec::right("PID", 2, 7),
        ColSpec::left("USER", 1, 8),
        ColSpec::left("S", 1, 1),
        ColSpec::right("NI", 1, 3),
        ColSpec::right("THR", 1, 4),
        ColSpec::right("CPU%", 1, 7),
        ColSpec::left("TREND", 1, SPARK_COLS),
    ];
    if gpu {
        columns.push(ColSpec::right("GPU%", 1, 7));
        columns.push(ColSpec::right("VRAM", 1, 8));
    }
    columns.push(ColSpec::right("PSS", 1, 8));
    columns.push(ColSpec::fill("Command", 2));
    columns
}

pub fn render<W: Write>(frame: &mut Frame<W>, app: &App, schema: &Schema) {
    let (width, height) = frame.size();
    let sys = *app.sys();
    let wsz = width as usize;
    let gpu = app.gpu_available();
    let header_lines = header_lines(gpu);

    // Each stat line is keyed on exactly what it draws — the formatted values and, for the
    // graph, the drawn glyph levels and colors — so an idle machine (every cycle pushes a
    // graph cell, but the picture is the same) and unrelated changes (CPU numbers vs. the
    // memory line) repaint nothing. On unchanged frames the build closures never run.
    let history = app.cpu_history();
    let layout = app.header_layout();
    let cpu_line = header::CpuLine::new(&sys, layout, history);
    frame.line(0, &cpu_line, |l| cpu_line.build(l));
    let mem_line = header::MemLine::new(&sys, layout);
    frame.line(1, &mem_line, |l| mem_line.build(l));
    if gpu {
        frame.line(2, (sys, app.empty_gpus()), |l| {
            build_gpu_line(l, &sys, app.empty_gpus(), wsz);
        });
    }
    frame.header(header_lines, schema, HEADER_STYLE);

    let body_height = height.saturating_sub(chrome_lines(gpu));
    render_body(frame, app, schema, header_lines + 1, body_height);

    let footer_row = height.saturating_sub(1);
    let row_count = app.rows().len();
    let overflow = app.pool_overflow();
    let privileged = app.is_privileged();
    let short_lived = app.short_lived();
    let log_path = crate::log::log_path();
    // Change key covers every value the footer shows, so a change in any repaints it.
    frame.line(
        footer_row,
        (
            row_count,
            overflow,
            privileged,
            short_lived,
            log_path.is_some(),
        ),
        |l| build_footer(l, row_count, overflow, privileged, short_lived, log_path),
    );
}

// ---------------------------------------------------------------------------
// System stats header
// ---------------------------------------------------------------------------

#[allow(clippy::cast_possible_truncation)]
fn build_gpu_line(
    l: &mut etch::Line,
    sys: &SystemStats,
    empty_devices: Option<&[u32]>,
    width: usize,
) {
    let gpu_value = sys.gpu_util_bp.map_or_else(
        || " -]".to_owned(),
        |bp| format!(" {:.1}%]", f64::from(bp) / 100.0),
    );
    let vram_value = sys.vram.map_or_else(
        || " -]".to_owned(),
        |vram| format!(" {}/{}]", Mem(vram.used), Mem(vram.total)),
    );
    let empty = empty_devices
        .filter(|devices| !devices.is_empty())
        .map(|devices| format!("  Empty: {}", format_ranges(devices)))
        .unwrap_or_default();
    let gpu_prefix = "GPU[";
    let vram_prefix = "  VRAM[";
    let chrome =
        gpu_prefix.len() + gpu_value.len() + vram_prefix.len() + vram_value.len() + empty.len();
    let total_bar = width.saturating_sub(chrome).max(8);
    let gpu_bar = total_bar / 2;
    let vram_bar = total_bar - gpu_bar;

    let gpu_den = u64::from(sys.gpu_count).saturating_mul(10_000);
    let gpu_cells = if let Some(gpu_util_bp) = sys.gpu_util_bp {
        normalized_cells(u64::from(gpu_util_bp), gpu_den, gpu_bar)
    } else {
        0
    };
    let vram_cells = if let Some(vram) = sys.vram {
        normalized_cells(vram.used, vram.total, vram_bar)
    } else {
        0
    };

    l.span(gpu_prefix, palette::LABEL);
    l.bar(gpu_cells, '|', palette::BAR_BUSY);
    l.gap(gpu_bar - gpu_cells);
    l.span(&gpu_value, palette::LABEL);
    l.span(vram_prefix, palette::LABEL);
    l.bar(vram_cells, '|', palette::BAR_SYS);
    l.gap(vram_bar - vram_cells);
    l.span(&vram_value, palette::LABEL);
    if !empty.is_empty() {
        l.span(&empty, palette::INFO);
    }
}

fn normalized_cells(value: u64, capacity: u64, width: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    usize::try_from((value.saturating_mul(width as u64) / capacity).min(width as u64))
        .unwrap_or(width)
}

fn format_ranges(indices: &[u32]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < indices.len() {
        let start = indices[i];
        let mut end = start;
        while i + 1 < indices.len() && indices[i + 1] == end.saturating_add(1) {
            i += 1;
            end = indices[i];
        }
        if !out.is_empty() {
            out.push(',');
        }
        let _ = write!(out, "{start}");
        if end != start {
            let _ = write!(out, "-{end}");
        }
        i += 1;
    }
    out
}

fn build_footer(
    l: &mut etch::Line,
    visible_rows: usize,
    pool_overflow: u32,
    privileged: bool,
    short_lived: u32,
    log_path: Option<&std::path::Path>,
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
        "Rows: {visible_rows} | q:quit k:kill ↑↓:scroll Enter/→:expand ←:collapse g:fold app PgUp/PgDn Home/End"
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
    if let Some(path) = log_path {
        l.span(&format!(" [log: {}]", path.display()), palette::FOOTER);
    }
    l.fill(' ');
}

// ---------------------------------------------------------------------------
// Process list body
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn render_body<W: Write>(
    frame: &mut Frame<W>,
    app: &App,
    schema: &Schema,
    body_top: u16,
    body_height: u16,
) {
    let procs = app.procs().as_slice();
    let scroll = app.scroll();
    let rows = app.rows();

    // Phase 1: advance guide state over rows above the viewport (no rendering).
    let mut guides: Vec<bool> = Vec::with_capacity(16);
    for row in rows.iter().take(scroll) {
        advance_guides(&mut guides, row.depth as usize, row.has_next);
    }

    let mut prefix = String::with_capacity(64);
    frame.table(schema, body_top, body_height, |table| {
        for (display_idx, row) in rows
            .iter()
            .enumerate()
            .skip(scroll)
            .take(body_height as usize)
        {
            let depth = row.depth as usize;
            let selected = display_idx == app.selected();

            let totals = app.row_totals(display_idx);
            prefix.clear();
            let prefix_cols = build_prefix(
                &mut prefix,
                depth,
                row.has_children(),
                row.is_collapsed(),
                row.has_next,
                &guides,
                if matches!(row.kind, RowKind::Group { .. }) {
                    PrefixKind::Application
                } else {
                    PrefixKind::Process
                },
            );
            advance_guides(&mut guides, depth, row.has_next);

            let bg = selected.then_some(palette::SELECTION_BG);
            let RowView {
                p,
                uid,
                text,
                suffix_n,
                key,
                is_kthread,
                basename_fg,
            } = match row.kind {
                RowKind::Process { proc_idx } => {
                    let p = &procs[proc_idx];
                    RowView {
                        p,
                        uid: p.uid,
                        text: row_text(app, p),
                        // The row itself is shown, so only the descendants are "hidden".
                        suffix_n: if row.is_collapsed() {
                            totals.count.saturating_sub(1)
                        } else {
                            0
                        },
                        key: u64::from(p.pid),
                        is_kthread: p.is_kthread,
                        basename_fg: basename_color(p.exe_deleted, p.uses_deleted_lib),
                    }
                }
                RowKind::Group { group_idx } => {
                    let group = app.app_group(group_idx);
                    RowView {
                        p: &procs[group.representative],
                        uid: group.owner_uid,
                        text: RowText::GroupLabel {
                            bytes: group.label.as_bytes(),
                            non_ascii: !group.label.is_ascii(),
                        },
                        suffix_n: totals.count,
                        key: application_row_key(&group.key),
                        is_kthread: false,
                        basename_fg: palette::BASENAME,
                    }
                }
            };
            let (state, nice) = (p.display_state, p.nice);
            let (threads, series) = (totals.threads, app.row_series(display_idx));

            // Selection is the only row-level color: a distinct background, applied to every
            // cell. A change flips the row-level style below, forcing a full-row repaint (so
            // the bg reaches every cell); otherwise each cell is change-tracked and colored independently.
            let cmd = CommandCell {
                prefix: prefix.as_str(),
                prefix_cols,
                text,
                is_kthread,
                suffix_n,
                basename_fg,
            };

            table.row(key, Style::NONE.with_bg(bg), |r| {
                r.field(p.pid); // PID inherits the row style (no per-cell color)
                r.styled_field(app.uid_name(uid), cell(Some(palette::caps(p.caps)), bg));
                r.styled_field(state as char, cell(Some(palette::state(state)), bg));
                r.styled_field(nice, cell(Some(palette::nice(nice)), bg));
                r.styled_field(threads, cell(palette::threads(threads), bg));
                r.styled_field(
                    Pct(totals.cpu_pct),
                    cell(Some(palette::cpu(totals.cpu_pct)), bg),
                );
                // Style = the row's bg only; each glyph carries its own foreground.
                r.cell(Spark(series), |c: &mut Cell| Spark(series).write(c));
                if app.gpu_process_available() {
                    write_gpu_fields(r, totals.gpu, app.gpu_process_sample_available(), bg);
                }
                // PSS; a dim `~` marks a sum that includes a process still reported by its
                // resident set (no proportional measurement yet).
                r.cell((totals.mem_bytes, totals.mem_estimated), |c: &mut Cell| {
                    if totals.mem_estimated {
                        c.set_fg(palette::TREE);
                        c.ascii(b"~");
                    }
                    c.set_fg(palette::bytes(totals.mem_bytes));
                    let _ = write!(c, "{}", Mem(totals.mem_bytes));
                });
                // Command change key: content hash (a changed cmdline reuses a freed store
                // slot, so the handle isn't a stable content identity) + tree prefix +
                // collapse suffix + the flags that drive its per-span colors.
                r.fill(
                    (
                        cmd.prefix,
                        cmd.text,
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

fn write_gpu_fields<W: Write>(
    r: &mut Row<'_, '_, W>,
    gpu: Option<crate::procs::GpuMetrics>,
    sample_available: bool,
    bg: Option<Rgb>,
) {
    if !sample_available {
        r.field("");
        r.field("");
        return;
    }
    let gpu_pct = gpu.map(|gpu| gpu.pct);
    let gpu_mem = gpu.map(|gpu| gpu.mem_bytes);
    r.styled_field(MaybePct(gpu_pct), cell(metric_cpu_color(gpu_pct), bg));
    r.styled_field(MaybeMem(gpu_mem), cell(metric_mem_color(gpu_mem), bg));
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

/// What differs per row between a process and a group row, resolved into one named shape. A
/// struct (not a positional tuple) so both construction arms name every field. The numbers are
/// not here: every row kind reads them from the view's per-row totals.
struct RowView<'a> {
    /// The process backing this row: the process itself, or an application group's representative.
    p: &'a crate::procs::ProcessEntry,
    uid: u32,
    text: RowText<'a>,
    /// Collapsed-row count badge: how many processes the row hides.
    suffix_n: u32,
    /// Stable per-row identity for the retained renderer's change detection.
    key: u64,
    is_kthread: bool,
    basename_fg: Rgb,
}

/// Everything the Command cell needs to render itself, gathered once per row so the fill
/// closure (and its change key) has a single value to close over.
struct CommandCell<'a> {
    prefix: &'a str,
    prefix_cols: usize,
    text: RowText<'a>,
    is_kthread: bool,
    suffix_n: u32,
    basename_fg: Rgb,
}

#[derive(Clone, Copy, Hash)]
enum RowText<'a> {
    Comm { bytes: &'a [u8], non_ascii: bool },
    Cmdline { bytes: &'a [u8], non_ascii: bool },
    GroupLabel { bytes: &'a [u8], non_ascii: bool },
}

fn row_text<'a>(app: &'a App, p: &'a crate::procs::ProcessEntry) -> RowText<'a> {
    let cmdline = app.cmdline(p);
    if cmdline.is_empty() {
        RowText::Comm {
            bytes: p.comm(),
            non_ascii: p.non_ascii,
        }
    } else {
        RowText::Cmdline {
            bytes: cmdline,
            non_ascii: p.non_ascii,
        }
    }
}

fn write_command(c: &mut Cell, cmd: &CommandCell) {
    c.set_fg(palette::TREE);
    c.glyph(cmd.prefix, cmd.prefix_cols);

    match cmd.text {
        RowText::Comm { bytes, non_ascii } if cmd.is_kthread => {
            // Kernel thread (no cmdline): synthesize the familiar `[name]` bracket convention
            // (the kernel stores the name unbracketed), dim.
            c.set_fg(palette::KTHREAD);
            c.ascii(b"[");
            push_text(c, bytes, non_ascii);
            c.ascii(b"]");
        }
        RowText::Comm { bytes, non_ascii } => {
            // Userspace with an empty/inaccessible cmdline: show `comm` plainly — no brackets,
            // it is a real process — colored as a basename.
            c.set_fg(cmd.basename_fg);
            push_text(c, bytes, non_ascii);
        }
        RowText::Cmdline { bytes, non_ascii } | RowText::GroupLabel { bytes, non_ascii } => {
            write_cmdline(c, bytes, non_ascii, cmd.basename_fg);
        }
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
    kind: PrefixKind,
) -> usize {
    let diamond = kind == PrefixKind::Application;
    let mut cols = 0usize;
    let collapse_shown = match depth {
        0 => false,
        1 => {
            out.push_str(if diamond {
                "◇ "
            } else if has_children && collapsed {
                "○ "
            } else {
                "● "
            });
            cols += 2;
            true
        }
        2 => {
            out.push_str(if diamond {
                "  ◇ "
            } else if has_children && collapsed {
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
        out.push(if diamond {
            '◇'
        } else if collapsed {
            '▸'
        } else {
            '▾'
        });
        out.push(' ');
        cols += 2;
    }
    cols
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrefixKind {
    Process,
    Application,
}

fn application_row_key(key: &crate::application::AppGroupKey) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish() | (1_u64 << 63)
}

// ---------------------------------------------------------------------------
// Value formatters: Display (what's shown) + Hash (the change key). Same value for
// both, so the change key can never drift from the rendered text.
// ---------------------------------------------------------------------------

/// CPU% in basis points, rendered as `N.NN%`.
#[derive(Hash)]
struct Pct(u32);

impl fmt::Display for Pct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}%", self.0 / 100, self.0 % 100)
    }
}

/// A row's CPU history as block glyphs, oldest → newest. Height is CPU used, on the shared log
/// ladder (the same for every row, so rows are comparable); color is how much of the time the
/// work wanted a CPU it spent queued for one, on the same ramp as the header graph's pressure.
/// An interval whose wait was not measured is neutral, never calm. Exactly-zero intervals are
/// blank, except one that waited without running, which shows the lowest block; `None` (a row
/// with no series of its own) is blank throughout.
#[derive(Hash)]
struct Spark(Option<CpuSeries>);

impl Spark {
    fn write(&self, c: &mut Cell) {
        let Some(series) = self.0 else { return };
        for cell in series.0 {
            let level = if cell.rate == 0 && cell.is_active() {
                Level::LOWEST
            } else {
                Level::of(u64::from(cell.rate), Scale::Log)
            };
            if level.is_blank() {
                c.reset_fg();
            } else {
                c.set_fg(
                    cell.waiting
                        .share_bp()
                        .map_or(palette::WAIT_UNKNOWN, palette::pressure),
                );
            }
            let mut utf8 = [0u8; 4];
            c.glyph(level.glyph().encode_utf8(&mut utf8), 1);
        }
    }
}

/// Memory in bytes, rendered in human units, at most [`Mem::MAX_WIDTH`] columns. A unit is
/// used up to 1000 of itself, then the next takes over (`999.9M`, then `1.0G`): rounding to a
/// tenth is what decides, so a value that would print as `1000.0` moves up instead of widening.
#[derive(Hash)]
pub(crate) struct Mem(pub u64);

impl Mem {
    /// Widest text any value prints: `999.9G`.
    pub(crate) const MAX_WIDTH: usize = 6;
}

impl fmt::Display for Mem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [char; 6] = ['K', 'M', 'G', 'T', 'P', 'E'];
        let b = self.0;
        if b < 1024 {
            return write!(f, "{b}B");
        }
        // Rounded tenths of the unit at `shift` bits.
        let tenths = |shift: u32| ((u128::from(b) * 10) + (1 << (shift - 1))) >> shift;
        let mut unit = 0;
        let mut shift = 10;
        // K is whole numbers; from M up, a count that would round to 1000.0 moves up a unit.
        while unit + 1 < UNITS.len()
            && if unit == 0 {
                b >> 10 >= 1000
            } else {
                tenths(shift) >= 10_000
            }
        {
            unit += 1;
            shift += 10;
        }
        if unit == 0 {
            return write!(f, "{}K", b >> 10);
        }
        let t = tenths(shift);
        write!(f, "{}.{}{}", t / 10, t % 10, UNITS[unit])
    }
}

fn metric_cpu_color(metric: Option<GpuMetric<u32>>) -> Option<Rgb> {
    match metric {
        Some(GpuMetric::Value(value)) => Some(palette::cpu(value)),
        _ => None,
    }
}

fn metric_mem_color(metric: Option<GpuMetric<u64>>) -> Option<Rgb> {
    match metric {
        Some(GpuMetric::Value(value)) => Some(palette::bytes(value)),
        _ => None,
    }
}

#[derive(Hash)]
struct MaybePct(Option<GpuMetric<u32>>);

impl fmt::Display for MaybePct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(GpuMetric::Value(value)) => Pct(value).fmt(f),
            Some(GpuMetric::Unknown) => f.write_str("??"),
            None => f.write_str("--"),
        }
    }
}

#[derive(Hash)]
struct MaybeMem(Option<GpuMetric<u64>>);

impl fmt::Display for MaybeMem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(GpuMetric::Value(value)) => Mem(value).fmt(f),
            Some(GpuMetric::Unknown) => f.write_str("??"),
            None => f.write_str("--"),
        }
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
    fn mem_switches_unit_at_a_thousand_and_stays_bounded() {
        const K: u64 = 1024;
        assert_eq!(Mem(1023).to_string(), "1023B");
        assert_eq!(Mem(999 * K).to_string(), "999K");
        assert_eq!(Mem(1000 * K).to_string(), "1.0M");
        assert_eq!(Mem(999 * K * K + K * K * 9 / 10).to_string(), "999.9M");
        assert_eq!(
            Mem(1000 * K * K - 1).to_string(),
            "1.0G",
            "would round to 1000.0M"
        );
        assert_eq!(Mem(1000 * K * K * K).to_string(), "1.0T");
        assert_eq!(Mem(999 * K * K * K).to_string(), "999.0G");
        assert_eq!(Mem(u64::MAX).to_string(), "16.0E");
        let mut b = 1u64;
        while b < u64::MAX / 3 {
            for v in [b - 1, b, b + 1, b / 3 * 2] {
                assert!(Mem(v).to_string().len() <= Mem::MAX_WIDTH, "{v}");
            }
            b *= 3;
        }
        for v in (0..64).flat_map(|s| [(1u64 << s) - 1, 1 << s, (1 << s) + 1]) {
            assert!(Mem(v).to_string().len() <= Mem::MAX_WIDTH, "{v}");
        }
        assert!(Mem(u64::MAX).to_string().len() <= Mem::MAX_WIDTH);
    }

    #[test]
    fn compact_device_ranges() {
        assert_eq!(format_ranges(&[]), "");
        assert_eq!(format_ranges(&[1, 3, 4, 5, 8]), "1,3-5,8");
    }

    #[test]
    fn geometry_changes_only_when_gpu_is_present() {
        assert_eq!(header_lines(false), 2);
        assert_eq!(chrome_lines(false), 4);
        assert_eq!(header_lines(true), 3);
        assert_eq!(chrome_lines(true), 5);
        let cpu = column_specs(false);
        let gpu = column_specs(true);
        assert_eq!(cpu.len(), 9);
        assert_eq!(gpu.len(), 11);
        assert_eq!(gpu[7].title, "GPU%");
        assert_eq!(gpu[8].title, "VRAM");
    }

    #[test]
    fn multi_gpu_bar_uses_total_device_capacity() {
        assert_eq!(normalized_cells(15_000, 20_000, 40), 30);
    }
}
