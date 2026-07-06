//! `etch` — a retained-mode terminal renderer.
//!
//! You declare *structure* once (a [`Schema`] of columns; lines built from spans) and
//! *bind values* every frame. A cell's bound value is hashed and compared to the
//! value that produced its last output — equal means skip formatting and I/O
//! entirely. Work is proportional to what changed, not to screen size. No intermediate
//! cell buffer, no blind diff, no grapheme segmentation on the ASCII fast path.
//!
//! ```ignore
//! let schema = Schema::new(vec![
//!     ColSpec::right("PID", 2, 7),
//!     ColSpec::fill("Command", 2),
//! ]);
//! let mut display = Display::new(io::stdout());
//!
//! let cyan = Rgb(0, 200, 200);
//! let mut frame = display.begin_frame(width, height);
//! frame.line(0, |l| { l.span("CPU[", cyan); l.bar(8, '|', Rgb(0, 200, 0)); l.fill(' '); });
//! frame.header(1, &schema, Style::fg(cyan).bold());
//! frame.table(&schema, 2, height - 3, |t| {
//!     for p in &procs {
//!         t.row(u64::from(p.pid), Style::NONE, |r| {
//!             r.field(p.pid);
//!             r.styled_field(p.cpu, Style::fg(cpu_color(p.cpu)));
//!             r.fill(p.cmdline_ref, |c| c.ascii(p.cmdline));
//!         });
//!     }
//! });
//! frame.commit()?;
//! ```

mod cell;
mod display;
mod hash;
mod line;
mod schema;
mod style;

pub use cell::Cell;
pub use display::{Display, Frame, Row, Table};
pub use line::Line;
pub use schema::{Align, ColSpec, Schema};
pub use style::{Rgb, Style};
