//! End-to-end rendering tests: render to an in-memory writer, parse the emitted
//! bytes with a real terminal emulator (`vt100`), and assert both the resulting
//! screen *and* that work is proportional to what changed (the whole point of the
//! gate mechanism).

use etch::{ColSpec, Color, Display, Schema, Style};

fn schema() -> Schema {
    Schema::new(vec![
        ColSpec::right("PID", 1, 5),
        ColSpec::right("CPU%", 1, 6),
        ColSpec::fill("CMD", 1),
    ])
}

/// One screen row as a string, trailing blanks trimmed.
fn row(parser: &vt100::Parser, r: u16, cols: u16) -> String {
    let screen = parser.screen();
    let s: String = (0..cols)
        .map(|c| screen.cell(r, c).map_or("", vt100::Cell::contents))
        .collect();
    s.trim_end().to_string()
}

fn parse(bytes: &[u8], rows: u16, cols: u16) -> vt100::Parser {
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(bytes);
    p
}

#[test]
fn renders_header_and_rows() {
    let schema = schema();
    let mut d = Display::new(Vec::new());

    let mut f = d.begin_frame(40, 6);
    f.header(0, &schema, Style::fg(Color::Cyan).bold());
    f.table(&schema, 1, 5, |t| {
        t.row(1, Style::NONE, |r| {
            r.field(1u32);
            r.field("0.00%");
            r.fill("init", |c| c.ascii(b"init"));
        });
        t.row(42, Style::NONE, |r| {
            r.field(42u32);
            r.field("12.34%");
            r.fill("bash", |c| c.ascii(b"bash"));
        });
    });
    f.commit().unwrap();

    let p = parse(d.get_ref(), 6, 40);
    // PID right in sep1+body5; CPU right in sep1+body6; CMD sep1 then text.
    assert_eq!(row(&p, 0, 40), "   PID   CPU% CMD");
    assert_eq!(row(&p, 1, 40), "     1  0.00% init");
    assert_eq!(row(&p, 2, 40), "    42 12.34% bash");
}

#[test]
fn identical_frame_emits_nothing() {
    let schema = schema();
    let mut d = Display::new(Vec::new());

    let draw = |d: &mut Display<Vec<u8>>| {
        let mut f = d.begin_frame(40, 6);
        f.table(&schema, 0, 6, |t| {
            t.row(1, Style::NONE, |r| {
                r.field(1u32);
                r.field("0.00%");
                r.fill("init", |c| c.ascii(b"init"));
            });
        });
        f.commit().unwrap();
    };

    draw(&mut d);
    let after_first = d.get_ref().len();
    assert!(after_first > 0, "first frame must paint");

    draw(&mut d);
    assert_eq!(
        d.get_ref().len(),
        after_first,
        "an identical frame must emit zero bytes"
    );
}

#[test]
fn changing_one_field_repaints_only_that_cell() {
    let schema = schema();
    let mut d = Display::new(Vec::new());

    let draw = |d: &mut Display<Vec<u8>>, cpu2: &str| {
        let mut f = d.begin_frame(40, 6);
        f.table(&schema, 0, 6, |t| {
            t.row(1, Style::NONE, |r| {
                r.field(1u32);
                r.field("0.00%");
                r.fill("init", |c| c.ascii(b"init"));
            });
            t.row(2, Style::NONE, |r| {
                r.field(2u32);
                r.field(cpu2);
                r.fill("bash", |c| c.ascii(b"bash"));
            });
        });
        f.commit().unwrap();
    };

    draw(&mut d, "1.00%");
    let before = d.get_ref().len();
    draw(&mut d, "99.99%");
    let delta = String::from_utf8_lossy(&d.get_ref()[before..]).into_owned();

    assert!(
        delta.contains("99.99%"),
        "the changed value must be emitted"
    );
    assert!(
        !delta.contains("init") && !delta.contains("bash"),
        "unchanged Command cells must not repaint: {delta:?}"
    );
    assert!(
        !delta.contains("0.00%"),
        "unchanged row's CPU must not repaint: {delta:?}"
    );

    // Final screen is still correct.
    let p = parse(d.get_ref(), 6, 40);
    assert_eq!(row(&p, 1, 40), "     2 99.99% bash");
}

#[test]
fn moving_selection_repaints_two_rows_not_the_table() {
    let schema = schema();
    let mut d = Display::new(Vec::new());
    let sel = Style::fg(Color::White).bg(Color::DarkGrey);

    let draw = |d: &mut Display<Vec<u8>>, selected: u64| {
        let mut f = d.begin_frame(40, 6);
        f.table(&schema, 0, 6, |t| {
            for pid in 1u64..=3 {
                let style = if pid == selected { sel } else { Style::NONE };
                t.row(pid, style, |r| {
                    r.field(u32::try_from(pid).unwrap());
                    r.field("0.00%");
                    r.fill("proc", |c| c.ascii(b"proc"));
                });
            }
        });
        f.commit().unwrap();
    };

    draw(&mut d, 1);
    let before = d.get_ref().len();
    draw(&mut d, 2); // selection 1 -> 2
    let delta = &d.get_ref()[before..];
    // Two rows repaint (old + new selection). Content is unchanged, so the bytes are
    // mostly escape sequences; assert it's far smaller than a full 3-row table repaint.
    assert!(!delta.is_empty(), "selection move must emit something");
    let full = before; // first frame painted 3 rows from a clear
    assert!(
        delta.len() < full,
        "selection move ({}) must be cheaper than a full repaint ({})",
        delta.len(),
        full
    );
}

#[test]
fn shrinking_table_clears_stale_rows() {
    let schema = schema();
    let mut d = Display::new(Vec::new());

    {
        let mut f = d.begin_frame(40, 6);
        f.table(&schema, 0, 6, |t| {
            for pid in 1u64..=4 {
                t.row(pid, Style::NONE, |r| {
                    r.field(u32::try_from(pid).unwrap());
                    r.field("0.00%");
                    r.fill("proc", |c| c.ascii(b"proc"));
                });
            }
        });
        f.commit().unwrap();
    }
    {
        let mut f = d.begin_frame(40, 6);
        f.table(&schema, 0, 6, |t| {
            for pid in 1u64..=2 {
                t.row(pid, Style::NONE, |r| {
                    r.field(u32::try_from(pid).unwrap());
                    r.field("0.00%");
                    r.fill("proc", |c| c.ascii(b"proc"));
                });
            }
        });
        f.commit().unwrap();
    }

    let p = parse(d.get_ref(), 6, 40);
    assert_eq!(row(&p, 1, 40), "     2  0.00% proc");
    assert_eq!(row(&p, 2, 40), "", "row 3 must be blanked");
    assert_eq!(row(&p, 3, 40), "", "row 4 must be blanked");
}

#[test]
fn resize_forces_full_repaint() {
    let schema = schema();
    let mut d = Display::new(Vec::new());

    {
        let mut f = d.begin_frame(40, 6);
        f.table(&schema, 0, 6, |t| {
            t.row(1, Style::NONE, |r| {
                r.field(1u32);
                r.field("0.00%");
                r.fill("init", |c| c.ascii(b"init"));
            });
        });
        f.commit().unwrap();
    }
    let before = d.get_ref().len();
    {
        // Same data, new width — must repaint everything.
        let mut f = d.begin_frame(80, 6);
        f.table(&schema, 0, 6, |t| {
            t.row(1, Style::NONE, |r| {
                r.field(1u32);
                r.field("0.00%");
                r.fill("init", |c| c.ascii(b"init"));
            });
        });
        f.commit().unwrap();
    }
    assert!(
        d.get_ref().len() > before,
        "resize must re-emit despite identical values"
    );
    let p = parse(d.get_ref(), 6, 80);
    assert_eq!(row(&p, 0, 80), "     1  0.00% init");
}

#[test]
fn unchanged_line_skips_the_build_closure() {
    use std::cell::Cell;
    let mut d = Display::new(Vec::new());
    {
        let mut f = d.begin_frame(20, 2);
        f.line(0, 7u32, |l| l.span("hello", Color::Cyan));
        f.commit().unwrap();
    }
    let after1 = d.get_ref().len();

    // Same gate value: the closure must not run at all (no formatting), no output.
    let ran = Cell::new(false);
    {
        let mut f = d.begin_frame(20, 2);
        f.line(0, 7u32, |l| {
            ran.set(true);
            l.span("hello", Color::Cyan);
        });
        f.commit().unwrap();
    }
    assert!(!ran.get(), "gate hit must skip the build closure entirely");
    assert_eq!(d.get_ref().len(), after1, "gate hit must emit nothing");

    // Different gate value: repaints.
    {
        let mut f = d.begin_frame(20, 2);
        f.line(0, 8u32, |l| l.span("hello", Color::Cyan));
        f.commit().unwrap();
    }
    assert!(d.get_ref().len() > after1, "gate miss must repaint");
}

#[test]
fn tiny_terminal_does_not_panic() {
    let schema = schema();
    let mut d = Display::new(Vec::new());
    let mut f = d.begin_frame(20, 2);
    f.line(0, "hi", |l| {
        l.span("hi", Color::Cyan);
        l.fill(' ');
    });
    f.line(1, "x", |l| l.span("x", Color::White));
    f.header(3, &schema, Style::NONE); // row 3 ≥ height 2 → no-op
    f.table(&schema, 4, 0, |t| {
        t.row(1, Style::NONE, |r| {
            r.field(1u32);
            r.field("0.00%");
            r.fill("z", |c| c.ascii(b"z"));
        });
    });
    f.commit().unwrap();

    let p = parse(d.get_ref(), 2, 20);
    assert_eq!(row(&p, 0, 20), "hi");
    assert_eq!(row(&p, 1, 20), "x");
}

#[test]
fn fill_truncates_at_terminal_edge() {
    let schema = Schema::new(vec![ColSpec::right("PID", 1, 3), ColSpec::fill("CMD", 1)]);
    let mut d = Display::new(Vec::new());
    let mut f = d.begin_frame(12, 2);
    f.table(&schema, 0, 2, |t| {
        t.row(1, Style::NONE, |r| {
            r.field(1u32);
            // 7 cols available (12 - 3 pid - 1 sep - 1 sep). Text is longer.
            r.fill("supercalifragilistic", |c| c.ascii(b"supercalifragilistic"));
        });
    });
    f.commit().unwrap();

    let p = parse(d.get_ref(), 2, 12);
    let line = row(&p, 0, 12);
    assert!(
        line.chars().count() <= 12,
        "must not overflow the line: {line:?}"
    );
    assert_eq!(line, "   1 superca", "7 cols of cmd after '   1 '");
}
