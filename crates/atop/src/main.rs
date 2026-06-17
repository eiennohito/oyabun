mod app;
mod gather;
mod procs;
mod sys;
mod tree;
mod ui;

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use etch::Display;

use crate::app::App;
use crate::gather::REFRESH_INTERVAL;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));

    let page_size = sys::page_size();
    // `/proc` open is the one fallible bit of setup; do it here so it propagates with `?`.
    let proc_dir = sys::ProcDir::open()?;
    let uid_names = sys::read_uid_names();

    // One thread, one owner: the arena + `!Send` stores + the io_uring ring all live on this
    // thread, which both gathers and renders. Prime the first buffer before entering raw mode.
    let mut app = App::new(page_size, proc_dir, uid_names);
    app.gather();

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    let display = Display::new(io::stdout());

    let result = run(display, app);

    restore_terminal()?;
    result
}

fn restore_terminal() -> io::Result<()> {
    execute!(io::stdout(), cursor::Show, LeaveAlternateScreen)?;
    disable_raw_mode()
}

/// The serialized loop: gather on the interval, render only when something changed, and block
/// for input the rest of the time (so idle CPU is near zero — `poll` sleeps in the kernel).
/// A gather runs to completion before the next render; the borrow checker proves they never
/// alias. The UI cannot service input *during* a gather (sub-ms normally), which is well inside
/// the latency budget.
fn run(mut display: Display<io::Stdout>, mut app: App) -> Result<(), Box<dyn std::error::Error>> {
    let schema = ui::columns();
    let mut dirty = true;
    // First buffer was primed in `main`; the next gather is one interval out.
    let mut next_gather = Instant::now() + REFRESH_INTERVAL;

    loop {
        if Instant::now() >= next_gather {
            app.gather();
            next_gather = Instant::now() + REFRESH_INTERVAL;
            dirty = true;
        }

        let (width, height) = crossterm::terminal::size()?;
        let visible_height = height.saturating_sub(ui::CHROME_LINES) as usize;
        if app.adjust_scroll(visible_height) {
            dirty = true;
        }
        if dirty {
            let mut frame = display.begin_frame(width, height);
            ui::render(&mut frame, &app, &schema);
            frame.commit()?;
            dirty = false;
        }

        // Block for input until the next gather is due (near-zero idle CPU).
        let timeout = next_gather.saturating_duration_since(Instant::now());
        if event::poll(timeout)? {
            let mut force_gather = false;
            loop {
                if let Event::Key(key) = event::read()?
                    && key.kind == KeyEventKind::Press
                {
                    match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Up => app.move_up(),
                        KeyCode::Down => app.move_down(),
                        KeyCode::Enter | KeyCode::Right => app.toggle_collapse(),
                        KeyCode::Left => app.collapse_selected(),
                        KeyCode::Char('k') => force_gather |= app.kill_selected(),
                        KeyCode::PageUp => app.page_up(visible_height),
                        KeyCode::PageDown => app.page_down(visible_height),
                        KeyCode::Home => app.select_first(),
                        KeyCode::End => app.select_last(),
                        _ => {}
                    }
                    dirty = true;
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
            // A kill nudges the next gather to now, so the change shows without a full interval.
            if force_gather {
                next_gather = Instant::now();
            }
        }
    }
}
