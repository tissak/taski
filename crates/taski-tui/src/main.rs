//! taski-tui — live task list reader for the shared SQLite index.
//!
//! Opens the same `./taski.db` the daemon writes and holds the connection open for
//! the whole session, re-reading the index on a ~750ms cadence so daemon updates
//! appear live without restarting. Quit with `q`, `Esc`, or `Ctrl-C`. The terminal is
//! restored on normal exit and on panic.

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, terminal::EnterAlternateScreen, terminal::LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use rusqlite::Connection;

use taski_db as db;
use taski_db::Task;

/// CLI configuration. `--db` mirrors the daemon's; the TUI reads, the daemon writes.
#[derive(Parser, Debug)]
#[command(
    name = "taski-tui",
    version,
    about = "Live task list reader for the taski SQLite index"
)]
struct Cli {
    /// Path to the taski SQLite index database.
    #[arg(long, default_value = "./taski.db")]
    db: PathBuf,
}

/// How long `event::poll` blocks waiting for input between redraws.
const POLL_TIMEOUT: Duration = Duration::from_millis(250);
/// Re-read the index at least this often, independent of input.
const REFRESH_INTERVAL: Duration = Duration::from_millis(750);

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Restore the terminal even if a panic occurs mid-render.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));

    // One long-lived reader connection: WAL lets it coexist with the daemon's writer
    // (separate process) for the whole session.
    let conn = db::open(&cli.db.to_string_lossy()).context("opening taski database")?;

    let mut terminal = enter_terminal()?;
    let result = run(&mut terminal, &conn);
    restore_terminal()?;
    result
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

/// Main render+event loop. Holds one DB connection for the whole session and re-reads
/// the index on a ~750ms cadence so daemon writes appear live without blocking input.
/// Returns when the user requests to quit.
fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, conn: &Connection) -> Result<()> {
    let mut tasks: Vec<Task> = Vec::new();
    let mut state = ListState::default();
    // `None` => never refreshed yet, so the first iteration reads immediately.
    let mut last_refresh: Option<Instant> = None;

    loop {
        // Refresh the task list on the interval, independent of input.
        let due = last_refresh.is_none_or(|t| t.elapsed() >= REFRESH_INTERVAL);
        if due {
            refresh_tasks(conn, &mut tasks, &mut state)?;
            last_refresh = Some(Instant::now());
        }

        terminal.draw(|frame| draw(frame, &tasks, &mut state))?;

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
            // Space toggles the selected task open<->done via the daemon's write-back
            // queue (ADR-0002). The TUI never touches vault files directly.
            KeyCode::Char(' ') => submit_toggle(conn, &tasks, &state),
            _ => {}
        }
    }
}

/// Enqueue a checkbox-flip request for the currently-selected task. Nothing happens
/// if nothing is selected. Enqueue errors are logged to stderr and never propagate —
/// a failed enqueue must not block or crash the UI.
fn submit_toggle(conn: &Connection, tasks: &[Task], state: &ListState) {
    let Some(task) = state.selected().and_then(|sel| tasks.get(sel)) else {
        return;
    };
    if let Err(e) = enqueue_toggle(conn, task) {
        eprintln!("taski: could not enqueue toggle: {e:#}");
    }
}

/// Decide the desired checkbox char for a toggle of `raw` (PRD §10.2 / ADR-0003):
/// open (`" "`) -> done (`"x"`); done (`"x"`/`"X"`) -> open (`" "`); anything else
/// (in-progress, forwarded, …) resets to open.
fn toggle_target_char(raw: &str) -> &'static str {
    match raw {
        " " => "x",
        "x" | "X" => " ",
        _ => " ",
    }
}

/// Enqueue a checkbox-flip request for `task` into the shared `pending_actions`
/// table. Non-blocking: just inserts a row; the daemon applies it.
fn enqueue_toggle(conn: &Connection, task: &Task) -> Result<()> {
    let new_char = toggle_target_char(&task.raw_checkbox_char);
    db::enqueue_action(
        conn,
        &task.id,
        &task.note_path,
        task.line_number,
        &task.raw_checkbox_char,
        new_char,
    )
    .context("enqueuing toggle action")?;
    Ok(())
}

/// Re-read the index into `tasks` and adjust `state` so the selection survives the
/// refresh: kept if still valid, clamped to the last row otherwise, cleared when the
/// list is empty, and set to the first row if it was empty and is now non-empty.
fn refresh_tasks(conn: &Connection, tasks: &mut Vec<Task>, state: &mut ListState) -> Result<()> {
    *tasks = db::all_tasks(conn).context("reading tasks from index")?;
    reconcile_selection(state, tasks.len());
    Ok(())
}

/// Adjust `state`'s selected index to remain valid for `len` items.
fn reconcile_selection(state: &mut ListState, len: usize) {
    match (len, state.selected()) {
        (0, _) => state.select(None),
        (n, Some(i)) if i < n => {}                // still valid; keep
        (n, Some(_)) => state.select(Some(n - 1)), // past the end; clamp
        (_, None) => state.select(Some(0)),        // was empty, now non-empty -> first row
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

/// Render the current task list (or the empty placeholder). The title reflects the
/// live task count.
fn draw(frame: &mut Frame, tasks: &[Task], state: &mut ListState) {
    let area = frame.area();
    let title = format!("Taski — {} tasks (live)", tasks.len());
    let block = Block::default().borders(Borders::ALL).title(title);

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

#[cfg(test)]
mod tests {
    use super::*;
    use taski_db::Status;

    fn task(id: &str, raw: &str, line: usize, note: &str) -> Task {
        Task {
            id: id.to_string(),
            note_path: note.to_string(),
            line_number: line,
            text: format!("task {id}"),
            text_hash: "h".to_string(),
            status: Status::from_checkbox_char(raw),
            raw_checkbox_char: raw.to_string(),
            note_hash: None,
            note_mtime: None,
            due_date: None,
            updated_at: 1,
        }
    }

    /// The data path the live loop relies on: a held `all_tasks` query reflects
    /// subsequent writes on the same DB, including status mutations via upsert.
    #[test]
    fn held_query_reflects_db_changes() {
        let conn = db::open(":memory:").unwrap();
        assert!(db::all_tasks(&conn).unwrap().is_empty());

        db::upsert_task(&conn, &task("a", " ", 1, "n.md")).unwrap();
        db::upsert_task(&conn, &task("b", "x", 2, "n.md")).unwrap();
        assert_eq!(db::all_tasks(&conn).unwrap().len(), 2);

        // Mutate a's status via upsert-on-same-id, then re-query.
        db::upsert_task(&conn, &task("a", "/", 1, "n.md")).unwrap();
        let got = db::all_tasks(&conn).unwrap();
        assert_eq!(got.len(), 2, "upsert on same id must not grow the table");
        let a = got.iter().find(|t| t.id == "a").unwrap();
        assert_eq!(a.status, Status::InProgress);
    }

    /// Headless refresh smoke: `refresh_tasks` pulls DB changes into the live view
    /// state and preserves/clamps/clears the selection as the list shape changes.
    #[test]
    fn refresh_tasks_updates_view_and_preserves_selection() {
        let conn = db::open(":memory:").unwrap();

        // Start empty: refresh -> empty, no selection.
        let mut tasks = Vec::new();
        let mut state = ListState::default();
        refresh_tasks(&conn, &mut tasks, &mut state).unwrap();
        assert!(tasks.is_empty());
        assert_eq!(state.selected(), None);

        // Add 3 tasks (distinct notes so they can be removed selectively): refresh ->
        // 3 tasks, selection jumps to 0 (was empty/None).
        db::upsert_task(&conn, &task("a", " ", 1, "alpha.md")).unwrap();
        db::upsert_task(&conn, &task("b", " ", 2, "beta.md")).unwrap();
        db::upsert_task(&conn, &task("c", " ", 3, "gamma.md")).unwrap();
        refresh_tasks(&conn, &mut tasks, &mut state).unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(state.selected(), Some(0));

        // Select the last row, then shrink the list to 1: selection must clamp into
        // range (to index 0), not stay at the now-invalid index.
        state.select(Some(2));
        db::delete_tasks_for_note(&conn, "alpha.md").unwrap();
        db::delete_tasks_for_note(&conn, "gamma.md").unwrap();
        refresh_tasks(&conn, &mut tasks, &mut state).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            state.selected(),
            Some(0),
            "out-of-range selection must clamp to the last valid row"
        );

        // A still-valid selection is preserved across a refresh.
        state.select(Some(0));
        refresh_tasks(&conn, &mut tasks, &mut state).unwrap();
        assert_eq!(state.selected(), Some(0));

        // Emptied list clears the selection.
        db::delete_tasks_for_note(&conn, "beta.md").unwrap();
        refresh_tasks(&conn, &mut tasks, &mut state).unwrap();
        assert!(tasks.is_empty());
        assert_eq!(state.selected(), None);
    }

    #[test]
    fn reconcile_selection_clears_when_empty() {
        let mut state = ListState::default();
        state.select(Some(2));
        reconcile_selection(&mut state, 0);
        assert_eq!(state.selected(), None);
    }

    #[test]
    fn toggle_target_char_maps_open_done_and_resets_others() {
        assert_eq!(toggle_target_char(" "), "x");
        assert_eq!(toggle_target_char("x"), " ");
        assert_eq!(toggle_target_char("X"), " ");
        assert_eq!(toggle_target_char("/"), " "); // in-progress -> reset to open
        assert_eq!(toggle_target_char(">"), " "); // forwarded -> reset to open
    }

    #[test]
    fn enqueue_toggle_inserts_pending_action_with_expected_bytes() {
        let conn = db::open(":memory:").unwrap();
        assert!(db::pending_actions(&conn).unwrap().is_empty());

        let t = task("a", " ", 3, "n.md");
        enqueue_toggle(&conn, &t).unwrap();

        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        let p = &pending[0];
        assert_eq!(p.task_id, "a");
        assert_eq!(p.note_path, "n.md");
        assert_eq!(p.line_number, 3);
        assert_eq!(p.expected_char, " ");
        assert_eq!(p.new_char, "x", "open -> done");
        assert_eq!(p.state, "pending");
        assert!(p.error.is_none());

        // A done task enqueues a flip back to open.
        db::resolve_action(&conn, p.id, "done", None).unwrap(); // clear the queue
        let done_task = task("b", "x", 7, "n.md");
        enqueue_toggle(&conn, &done_task).unwrap();
        let p2 = &db::pending_actions(&conn).unwrap()[0];
        assert_eq!(p2.expected_char, "x");
        assert_eq!(p2.new_char, " ");
    }
}
