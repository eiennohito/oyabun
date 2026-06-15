mod app;
mod arena;
mod gather;
mod snapshot;
mod sys;
mod tree;
mod ui;

use std::io;
use std::sync::mpsc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use etch::Display;

use crate::app::App;
use crate::gather::{Ctrl, Gatherer, REFRESH_INTERVAL};

/// UI wake-up cadence: how often we re-check the cell for a new snapshot when idle.
const UI_POLL: Duration = Duration::from_millis(200);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));

    let page_size = sys::page_size();
    let (mut gatherer, cell) = Gatherer::new(page_size)?;
    gatherer.prime(); // first snapshot ready before the UI reads it — no startup poll
    let uid_names = sys::read_uid_names();

    let (tx, rx) = mpsc::channel::<Ctrl>();
    let gather_handle = std::thread::Builder::new()
        .name("gatherer".into())
        .spawn(move || gatherer.run(&rx, REFRESH_INTERVAL))?;

    let app = App::new(cell, tx.clone(), uid_names);

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    let display = Display::new(io::stdout());

    let result = run(display, app);

    restore_terminal()?;

    let _ = tx.send(Ctrl::Quit);
    let _ = gather_handle.join();

    result
}

fn restore_terminal() -> io::Result<()> {
    execute!(io::stdout(), cursor::Show, LeaveAlternateScreen)?;
    disable_raw_mode()
}

fn run(mut display: Display<io::Stdout>, mut app: App) -> Result<(), Box<dyn std::error::Error>> {
    let schema = ui::columns();
    let mut dirty = true;
    loop {
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

        if event::poll(UI_POLL)? {
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
                        KeyCode::Char('k') => app.kill_selected(),
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
        }

        if app.refresh_view() {
            dirty = true;
        }
    }
}
