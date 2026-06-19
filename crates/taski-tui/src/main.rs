//! taski-tui — Slice 0 walking skeleton.
//!
//! Reads every task from the shared SQLite index at `./taski.db` (the same file the
//! daemon writes) and renders a minimal full-screen list. Quit with `q`, `Esc`, or
//! `Ctrl-C`. The terminal is restored on normal exit and on panic.

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, terminal::EnterAlternateScreen, terminal::LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use taski_db as db;
use taski_db::Task;

/// Fixed database path; must match the daemon's. Read access here, write access there.
const DB_PATH: &str = "./taski.db";
/// How long `poll` blocks waiting for input before redrawing.
const POLL_TIMEOUT: Duration = Duration::from_millis(250);

fn main() -> Result<()> {
    let tasks = read_tasks()?;

    // Restore the terminal even if a panic occurs mid-render.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));

    let mut terminal = enter_terminal()?;
    let result = run(&mut terminal, &tasks);
    restore_terminal()?;
    result
}

/// Read the current task index. The connection is dropped immediately so the reader
/// only briefly holds the file.
fn read_tasks() -> Result<Vec<Task>> {
    let conn = db::open(DB_PATH).context("opening taski database")?;
    db::all_tasks(&conn).context("reading tasks from index")
}

/// Enter raw mode + the alternate screen and build the terminal.
fn enter_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Leave the alternate screen and disable raw mode. Safe to call even if setup was
/// only partial (each step is independent).
fn restore_terminal() -> Result<()> {
    execute!(io::stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

/// Main render+event loop. Returns when the user requests to quit.
fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, tasks: &[Task]) -> Result<()> {
    let mut state = ListState::default();
    if !tasks.is_empty() {
        state.select(Some(0));
    }

    loop {
        terminal.draw(|frame| draw(frame, tasks, &mut state))?;

        if !event::poll(POLL_TIMEOUT)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        // On several terminals Release/Repeat events also fire; only act on Press.
        if key.kind != KeyEventKind::Press {
            continue;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if ctrl => return Ok(()),
            KeyCode::Down | KeyCode::Char('j') => move_selection(&mut state, tasks.len(), 1),
            KeyCode::Up | KeyCode::Char('k') => move_selection(&mut state, tasks.len(), -1),
            _ => {}
        }
    }
}

/// Shift the list selection by `delta` positions, clamping at the ends.
fn move_selection(state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0) as i32;
    let next = (current + delta).clamp(0, len as i32 - 1) as usize;
    state.select(Some(next));
}

/// Render the current task list (or the empty placeholder).
fn draw(frame: &mut Frame, tasks: &[Task], state: &mut ListState) {
    let area = frame.area();
    let block = Block::default().borders(Borders::ALL).title("Taski");

    if tasks.is_empty() {
        let placeholder =
            Paragraph::new("No tasks — run `cargo run -p taski-daemon` first.").block(block);
        frame.render_widget(placeholder, area);
        return;
    }

    let items: Vec<ListItem> = tasks
        .iter()
        .map(|t| {
            ListItem::new(format!(
                "[{}] {}  ({}:{})",
                t.raw_checkbox_char, t.text, t.note_path, t.line_number
            ))
        })
        .collect();

    let list = List::new(items).block(block).highlight_symbol("> ");

    frame.render_stateful_widget(list, area, state);
}
