//! taski-tui — live, browsable task list reader for the shared SQLite index.
//!
//! Opens the same `./taski.db` the daemon writes and holds the connection open for
//! the whole session, re-reading the index on a ~750ms cadence so daemon updates
//! appear live without restarting. Tasks are grouped by their source note (each group
//! collapsible) and filtered by status. Quit with `q`, `Esc`, or `Ctrl-C`. The
//! terminal is restored on normal exit and on panic.
//!
//! The TUI only ever reads via `db::all_tasks` and writes via `db::enqueue_action`
//! (a row in `pending_actions` the daemon drains); it never touches vault files.

use std::collections::HashSet;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, terminal::EnterAlternateScreen, terminal::LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use rusqlite::Connection;

use taski_db as db;
use taski_db::{Status, Task};

/// CLI configuration. `--db` mirrors the daemon's; the TUI reads, the daemon writes.
#[derive(Parser, Debug)]
#[command(
    name = "taski-tui",
    version,
    about = "Live, browsable task list reader for the taski SQLite index"
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

// ---------------------------------------------------------------------------
// View model: grouping + filtering over the raw task list.
// ---------------------------------------------------------------------------

/// Status filter cycled with `f`: All -> Open -> Done -> All. `Open` matches only
/// `Status::Open` (in-progress and other states appear only under `All`) — a
/// predictable three-state mapping to the labels all / open / done.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StatusFilter {
    All,
    Open,
    Done,
}

impl StatusFilter {
    fn matches(self, status: &Status) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Open => matches!(status, Status::Open),
            StatusFilter::Done => matches!(status, Status::Done),
        }
    }

    fn next(self) -> Self {
        match self {
            StatusFilter::All => StatusFilter::Open,
            StatusFilter::Open => StatusFilter::Done,
            StatusFilter::Done => StatusFilter::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "all",
            StatusFilter::Open => "open",
            StatusFilter::Done => "done",
        }
    }
}

/// One renderable row in the grouped list. `Header` carries per-note counts computed
/// from the full (unfiltered) task set so the triage overview stays accurate under any
/// filter; `Task` carries the task the cursor can act on.
#[derive(Debug, Clone)]
enum DisplayRow {
    Header {
        note_path: String,
        open_count: usize,
        total_count: usize,
        collapsed: bool,
    },
    Task {
        task: Task,
    },
}

impl DisplayRow {
    /// The note this row belongs to (the header's note, or the task's source note).
    fn note_path(&self) -> &str {
        match self {
            DisplayRow::Header { note_path, .. } => note_path,
            DisplayRow::Task { task } => &task.note_path,
        }
    }
}

/// Build the flat list of display rows from the raw task list, the active filter, and
/// the set of expanded note paths. Tasks are assumed sorted by `(note_path,
/// line_number)` — the order `db::all_tasks` returns — so each note's tasks form a
/// contiguous run, naturally in line order within the group.
///
/// Groups default to **collapsed**: a note not present in `expanded` is folded. This
/// inverts the natural "track what's open" model so newly-appearing notes (added by
/// the daemon between refreshes) also start collapsed without special handling.
///
/// Groups with no filter-matching task are hidden entirely (no empty headers).
/// Headers always carry the true open/total counts (from the full group, ignoring the
/// filter); task rows are emitted only when the group is expanded.
fn build_view(tasks: &[Task], filter: StatusFilter, expanded: &HashSet<String>) -> Vec<DisplayRow> {
    let mut rows = Vec::new();
    let mut i = 0;
    while i < tasks.len() {
        let note_path = tasks[i].note_path.clone();
        let mut j = i;
        while j < tasks.len() && tasks[j].note_path == note_path {
            j += 1;
        }
        let group = &tasks[i..j];
        let total_count = group.len();
        let open_count = group.iter().filter(|t| t.status == Status::Open).count();
        let visible: Vec<&Task> = group.iter().filter(|t| filter.matches(&t.status)).collect();
        if !visible.is_empty() {
            let is_expanded = expanded.contains(&note_path);
            rows.push(DisplayRow::Header {
                note_path,
                open_count,
                total_count,
                collapsed: !is_expanded,
            });
            if is_expanded {
                for t in visible {
                    rows.push(DisplayRow::Task { task: t.clone() });
                }
            }
        }
        i = j;
    }
    rows
}

/// Adjust the selection to survive a view rebuild (refresh, filter change, or
/// collapse/expand). In priority order:
///
/// 1. If the previously-selected task is still visible, keep the cursor on it (stable
///    across refreshes and line-order shifts).
/// 2. Else if the previously-selected note still has any visible row, land on its
///    header — so collapsing a group drops the cursor on that group's header, and
///    filtering a task out but leaving siblings visible keeps the cursor in-note.
/// 3. Else clamp the previous display index into range (lands on a sensible nearby
///    row); `None` selects row 0 when the view is newly non-empty.
///
/// An empty view clears the selection. This generalizes the original
/// `reconcile_selection` to the grouped/filtered view model.
fn reconcile_view_selection(
    rows: &[DisplayRow],
    prev_note: Option<&str>,
    prev_task_id: Option<i64>,
    prev_index: Option<usize>,
    state: &mut ListState,
) {
    if rows.is_empty() {
        state.select(None);
        return;
    }
    if let Some(id) = prev_task_id
        && let Some(idx) = rows
            .iter()
            .position(|r| matches!(r, DisplayRow::Task { task } if task.id == id))
    {
        state.select(Some(idx));
        return;
    }
    if let Some(note) = prev_note
        && let Some(idx) = rows.iter().position(|r| r.note_path() == note)
    {
        state.select(Some(idx));
        return;
    }
    let n = rows.len();
    match prev_index {
        None => state.select(Some(0)),
        Some(i) => state.select(Some(i.min(n - 1))),
    }
}

/// Which way to nudge a group's expand state from a keypress.
#[derive(Clone, Copy)]
enum ToggleMode {
    /// Flip collapsed<->expanded (`Enter`).
    Toggle,
    /// Force expanded (`→`).
    Expand,
    /// Force collapsed (`←`).
    Collapse,
}

/// The TUI's mutable session state: the raw task list, the derived display rows, the
/// list-state selection, the active status filter, and the set of notes the user has
/// expanded (empty = all collapsed). Kept in one struct so every mutation rebuilds the
/// view and reconciles selection through a single path.
struct App {
    tasks: Vec<Task>,
    rows: Vec<DisplayRow>,
    state: ListState,
    filter: StatusFilter,
    expanded: HashSet<String>,
}

impl App {
    fn new() -> Self {
        App {
            tasks: Vec::new(),
            rows: Vec::new(),
            state: ListState::default(),
            // Open-only default: a task list is for seeing what needs doing.
            filter: StatusFilter::Open,
            // Empty = every group starts collapsed. Stable across refreshes: notes the
            // daemon adds later also start collapsed.
            expanded: HashSet::new(),
        }
    }

    /// Snapshot what the cursor is on before a rebuild so selection can be preserved.
    fn snapshot(&self) -> (Option<String>, Option<i64>, Option<usize>) {
        let idx = self.state.selected();
        let row = idx.and_then(|i| self.rows.get(i));
        let note = row.map(|r| r.note_path().to_string());
        let task_id = match row {
            Some(DisplayRow::Task { task }) => Some(task.id),
            _ => None,
        };
        (note, task_id, idx)
    }

    /// Rebuild rows from the current tasks/filter/expanded and preserve selection.
    fn rebuild(&mut self) {
        let (note, task_id, idx) = self.snapshot();
        self.rows = build_view(&self.tasks, self.filter, &self.expanded);
        reconcile_view_selection(&self.rows, note.as_deref(), task_id, idx, &mut self.state);
    }

    /// Re-read the index from the DB, then rebuild the view.
    fn refresh(&mut self, conn: &Connection) -> Result<()> {
        self.tasks = db::all_tasks(conn).context("reading tasks from index")?;
        self.rebuild();
        Ok(())
    }

    /// `f`: cycle All -> Open -> Done -> All, preserving selection.
    fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.rebuild();
    }

    /// Toggle / expand / collapse the group under the cursor. `Enter` toggles a
    /// header; `→` forces expand; `←` forces collapse and, when pressed on a task row,
    /// collapses that task's parent group (fold from inside). All other key/row
    /// combinations are no-ops.
    fn toggle_at_cursor(&mut self, mode: ToggleMode) {
        let action: Option<(String, bool)> = {
            let Some(idx) = self.state.selected() else {
                return;
            };
            let Some(row) = self.rows.get(idx) else {
                return;
            };
            match row {
                DisplayRow::Header { note_path, .. } => {
                    let is_expanded = self.expanded.contains(note_path.as_str());
                    let want_expanded = match mode {
                        ToggleMode::Toggle => !is_expanded,
                        ToggleMode::Expand => true,
                        ToggleMode::Collapse => false,
                    };
                    Some((note_path.clone(), want_expanded))
                }
                DisplayRow::Task { task } => {
                    // Only `←` (Collapse) is meaningful on a task: fold its parent.
                    if matches!(mode, ToggleMode::Collapse) {
                        Some((task.note_path.clone(), false))
                    } else {
                        None
                    }
                }
            }
        };
        let Some((note, want_expanded)) = action else {
            return;
        };
        if want_expanded {
            self.expanded.insert(note);
        } else {
            self.expanded.remove(&note);
        }
        self.rebuild();
    }

    /// `Tab`: expand every group currently visible.
    fn expand_all(&mut self) {
        for row in &self.rows {
            if let DisplayRow::Header { note_path, .. } = row {
                self.expanded.insert(note_path.clone());
            }
        }
        self.rebuild();
    }

    /// `Shift-Tab`: collapse every group.
    fn collapse_all(&mut self) {
        self.expanded.clear();
        self.rebuild();
    }

    /// Shift the selection by `delta` display rows, clamping at the ends.
    fn move_selection(&mut self, delta: i32) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let current = self.state.selected().unwrap_or(0) as i32;
        let next = (current + delta).clamp(0, len as i32 - 1) as usize;
        self.state.select(Some(next));
    }

    /// The task under the cursor, if the cursor is on a task row (never a header).
    fn selected_task(&self) -> Option<&Task> {
        let idx = self.state.selected()?;
        match self.rows.get(idx)? {
            DisplayRow::Task { task } => Some(task),
            _ => None,
        }
    }

    /// Enqueue a checkbox-flip for the task under the cursor. No-op on a header or an
    /// empty list — the flip must always resolve to the exact task the user sees, never
    /// a header row. Enqueue errors are logged to stderr and never propagate.
    fn submit_toggle(&self, conn: &Connection) {
        let Some(task) = self.selected_task() else {
            return;
        };
        if let Err(e) = enqueue_toggle(conn, task) {
            eprintln!("taski: could not enqueue toggle: {e:#}");
        }
    }
}

// ---------------------------------------------------------------------------
// Event loop + rendering.
// ---------------------------------------------------------------------------

/// Main render+event loop. Holds one DB connection for the whole session and re-reads
/// the index on a ~750ms cadence so daemon writes appear live without blocking input.
/// Returns when the user requests to quit.
fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, conn: &Connection) -> Result<()> {
    let mut app = App::new();
    // `None` => never refreshed yet, so the first iteration reads immediately.
    let mut last_refresh: Option<Instant> = None;

    loop {
        // Refresh the task list on the interval, independent of input.
        let due = last_refresh.is_none_or(|t| t.elapsed() >= REFRESH_INTERVAL);
        if due {
            app.refresh(conn)?;
            last_refresh = Some(Instant::now());
        }

        terminal.draw(|frame| draw(frame, &mut app))?;

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
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
            // Space toggles the selected task open<->done via the daemon's write-back
            // queue (ADR-0002). The TUI never touches vault files directly.
            KeyCode::Char(' ') => app.submit_toggle(conn),
            KeyCode::Enter => app.toggle_at_cursor(ToggleMode::Toggle),
            KeyCode::Right => app.toggle_at_cursor(ToggleMode::Expand),
            KeyCode::Left => app.toggle_at_cursor(ToggleMode::Collapse),
            KeyCode::Char('f') => app.cycle_filter(),
            KeyCode::Tab => app.expand_all(),
            KeyCode::BackTab => app.collapse_all(),
            _ => {}
        }
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
        task.id,
        &task.note_path,
        task.line_number,
        &task.raw_checkbox_char,
        new_char,
    )
    .context("enqueuing toggle action")?;
    Ok(())
}

/// Render the grouped task list (or the empty placeholder), a title bar reflecting
/// the live counts and active filter, and a footer keybinding cheat-sheet.
fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

    let open_total = app
        .tasks
        .iter()
        .filter(|t| t.status == Status::Open)
        .count();
    let total = app.tasks.len();
    let notes = app
        .rows
        .iter()
        .filter(|r| matches!(r, DisplayRow::Header { .. }))
        .count();

    let title = Line::from(vec![
        Span::raw(" Taski — "),
        Span::styled(
            format!("filter: {}", app.filter.label()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "   {open_total} open of {total} total   ·   {notes} notes "
        )),
    ]);
    let block = Block::default().borders(Borders::ALL).title(title);

    let list_area = chunks[0];
    let footer_area = chunks[1];

    if app.rows.is_empty() {
        let msg = match (app.tasks.is_empty(), app.filter) {
            (true, _) => "No tasks — run `cargo run -p taski-daemon` first to populate the index.",
            (false, StatusFilter::Open) => "No open tasks. Press `f` to change the filter.",
            (false, StatusFilter::Done) => "No done tasks. Press `f` to change the filter.",
            (false, StatusFilter::All) => "No tasks match.",
        };
        frame.render_widget(Paragraph::new(msg).block(block), list_area);
    } else {
        let items: Vec<ListItem> = app.rows.iter().map(row_to_item).collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, list_area, &mut app.state);
    }

    let footer = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled("j/k", Style::default().fg(Color::Yellow)),
        Span::raw(" move  ·  "),
        Span::styled("Space", Style::default().fg(Color::Yellow)),
        Span::raw(" toggle  ·  "),
        Span::styled("Enter", Style::default().fg(Color::Yellow)),
        Span::raw(" fold group  ·  "),
        Span::styled("←/→", Style::default().fg(Color::Yellow)),
        Span::raw(" collapse/expand  ·  "),
        Span::styled("f", Style::default().fg(Color::Yellow)),
        Span::raw(" filter  ·  "),
        Span::styled("Tab/⇧Tab", Style::default().fg(Color::Yellow)),
        Span::raw(" expand/collapse all  ·  "),
        Span::styled("q", Style::default().fg(Color::Yellow)),
        Span::raw(" quit "),
    ]))
    .style(Style::new().add_modifier(Modifier::DIM));
    frame.render_widget(footer, footer_area);
}

/// Render one display row as a list item. Group headers are bold with a cyan
/// expand/collapse marker and dim counts; task rows are indented, with the checkbox
/// coloured by status, done tasks struck through, and a yellow due date when present.
fn row_to_item(row: &DisplayRow) -> ListItem<'static> {
    match row {
        DisplayRow::Header {
            note_path,
            open_count,
            total_count,
            collapsed,
        } => {
            let marker = if *collapsed { "▸" } else { "▾" };
            let line = Line::from(vec![
                Span::styled(
                    format!("{marker} "),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    note_path.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(
                    format!("{open_count} open · {total_count} total"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            ListItem::new(line)
        }
        DisplayRow::Task { task } => {
            let checkbox = format!("[{}]", task.raw_checkbox_char);
            let mut spans: Vec<Span> = Vec::with_capacity(6);
            spans.push(Span::raw("    ")); // indent under the header marker
            spans.push(Span::styled(checkbox, checkbox_style(&task.status)));
            spans.push(Span::raw(format!(" {}", task.text)));
            if let Some(due) = &task.due_date {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("· {due}"),
                    Style::default().fg(Color::Yellow),
                ));
            }
            let item_style = match task.status {
                Status::Done => Style::new()
                    .add_modifier(Modifier::CROSSED_OUT)
                    .add_modifier(Modifier::DIM),
                _ => Style::new(),
            };
            ListItem::new(Line::from(spans)).style(item_style)
        }
    }
}

/// Colour for the `[x]` checkbox, by status: open=amber (attention), done=green,
/// in-progress=cyan, other=dim.
fn checkbox_style(status: &Status) -> Style {
    match status {
        Status::Open => Style::default().fg(Color::Yellow),
        Status::Done => Style::default().fg(Color::Green),
        Status::InProgress => Style::default().fg(Color::Cyan),
        Status::Other(_) => Style::default().fg(Color::DarkGray),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taski_db::Status;

    fn task(id: i64, raw: &str, line: usize, note: &str) -> Task {
        Task {
            id,
            note_path: note.to_string(),
            line_number: line,
            text: format!("task {id}"),
            text_hash: format!("h{id}"),
            status: Status::from_checkbox_char(raw),
            raw_checkbox_char: raw.to_string(),
            note_hash: None,
            note_mtime: None,
            due_date: None,
            updated_at: 1,
        }
    }

    fn task_with_due(id: i64, raw: &str, line: usize, note: &str, due: &str) -> Task {
        let mut t = task(id, raw, line, note);
        t.due_date = Some(due.to_string());
        t
    }

    /// Unpack a header row for assertions.
    fn header(row: &DisplayRow) -> (&str, usize, usize, bool) {
        match row {
            DisplayRow::Header {
                note_path,
                open_count,
                total_count,
                collapsed,
            } => (note_path, *open_count, *total_count, *collapsed),
            _ => panic!("expected a header row"),
        }
    }

    /// The data path the live loop relies on: a held `all_tasks` query reflects
    /// subsequent writes on the same DB, including status mutations via upsert.
    #[test]
    fn held_query_reflects_db_changes() {
        let conn = db::open(":memory:").unwrap();
        assert!(db::all_tasks(&conn).unwrap().is_empty());

        db::upsert_task(&conn, &task(1, " ", 1, "n.md")).unwrap();
        db::upsert_task(&conn, &task(2, "x", 2, "n.md")).unwrap();
        assert_eq!(db::all_tasks(&conn).unwrap().len(), 2);

        // Mutate a's status via upsert-on-same-id, then re-query.
        db::upsert_task(&conn, &task(1, "/", 1, "n.md")).unwrap();
        let got = db::all_tasks(&conn).unwrap();
        assert_eq!(got.len(), 2, "upsert on same id must not grow the table");
        let a = got.iter().find(|t| t.id == 1).unwrap();
        assert_eq!(a.status, Status::InProgress);
    }

    /// Headless refresh smoke: `App::refresh` pulls DB changes into the live view
    /// state and preserves/clamps/clears the selection as the view shape changes.
    #[test]
    fn refresh_updates_view_and_preserves_selection() {
        let conn = db::open(":memory:").unwrap();
        let mut app = App::new();

        // Start empty: refresh -> empty, no selection.
        app.refresh(&conn).unwrap();
        assert!(app.rows.is_empty());
        assert_eq!(app.state.selected(), None);

        // Add 3 open tasks across 3 distinct notes: refresh -> 3 collapsed headers
        // (default filter Open, default all-collapsed), selection jumps to 0.
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();
        db::upsert_task(&conn, &task(2, " ", 2, "beta.md")).unwrap();
        db::upsert_task(&conn, &task(3, " ", 3, "gamma.md")).unwrap();
        app.refresh(&conn).unwrap();
        assert_eq!(app.tasks.len(), 3);
        assert_eq!(app.rows.len(), 3, "three collapsed headers, no task rows");
        assert_eq!(app.state.selected(), Some(0));

        // Select the last header, then shrink to one note: selection must clamp to 0.
        app.state.select(Some(2));
        db::delete_tasks_for_note(&conn, "alpha.md").unwrap();
        db::delete_tasks_for_note(&conn, "gamma.md").unwrap();
        app.refresh(&conn).unwrap();
        assert_eq!(app.rows.len(), 1);
        assert_eq!(
            app.state.selected(),
            Some(0),
            "out-of-range selection must clamp to the last valid row"
        );

        // A still-valid selection is preserved across a refresh.
        app.state.select(Some(0));
        app.refresh(&conn).unwrap();
        assert_eq!(app.state.selected(), Some(0));

        // Emptied list clears the selection.
        db::delete_tasks_for_note(&conn, "beta.md").unwrap();
        app.refresh(&conn).unwrap();
        assert!(app.rows.is_empty());
        assert_eq!(app.state.selected(), None);
    }

    /// `build_view` groups tasks by note with accurate open/total counts and emits
    /// only headers when groups are collapsed.
    #[test]
    fn build_view_groups_by_note_with_counts() {
        let tasks = vec![
            task(1, " ", 1, "alpha.md"),
            task(2, "x", 2, "alpha.md"),
            task(3, " ", 1, "beta.md"),
        ];
        let expanded = HashSet::new();
        let rows = build_view(&tasks, StatusFilter::All, &expanded);
        // Two collapsed groups -> two headers, no task rows.
        assert_eq!(rows.len(), 2);
        let (note, open, total, collapsed) = header(&rows[0]);
        assert_eq!(note, "alpha.md");
        assert_eq!(open, 1);
        assert_eq!(total, 2);
        assert!(collapsed, "groups default to collapsed");
        let (note, open, total, _) = header(&rows[1]);
        assert_eq!(note, "beta.md");
        assert_eq!(open, 1);
        assert_eq!(total, 1);
    }

    /// Expanding a group emits its task rows in line order.
    #[test]
    fn build_view_expanded_emits_task_rows_in_line_order() {
        let tasks = vec![task(1, " ", 1, "alpha.md"), task(2, "x", 2, "alpha.md")];
        let expanded = HashSet::from(["alpha.md".to_string()]);
        let rows = build_view(&tasks, StatusFilter::All, &expanded);
        assert_eq!(rows.len(), 3, "header + two tasks");
        assert!(matches!(rows[0], DisplayRow::Header { .. }));
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
        assert!(matches!(&rows[2], DisplayRow::Task { task } if task.id == 2));
    }

    /// The Open filter hides done tasks within an expanded group but keeps the header.
    #[test]
    fn build_view_open_filter_hides_done_tasks() {
        let tasks = vec![task(1, " ", 1, "alpha.md"), task(2, "x", 2, "alpha.md")];
        let expanded = HashSet::from(["alpha.md".to_string()]);
        let rows = build_view(&tasks, StatusFilter::Open, &expanded);
        assert_eq!(rows.len(), 2, "header + only the open task");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
    }

    /// A group with no filter-matching task is hidden entirely (no empty header).
    #[test]
    fn build_view_hides_group_with_no_matching_tasks() {
        let tasks = vec![
            task(1, " ", 1, "alpha.md"), // open
            task(2, "x", 1, "beta.md"),  // done
        ];
        let expanded = HashSet::new();
        let rows = build_view(&tasks, StatusFilter::Open, &expanded);
        // Only alpha has an open task; beta is hidden under the Open filter.
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "alpha.md");
    }

    /// The due-date column flows through to the task row data (rendered separately).
    #[test]
    fn build_view_preserves_due_date_on_task_row() {
        let tasks = vec![task_with_due(1, " ", 1, "alpha.md", "2026-07-01")];
        let expanded = HashSet::from(["alpha.md".to_string()]);
        let rows = build_view(&tasks, StatusFilter::All, &expanded);
        match &rows[1] {
            DisplayRow::Task { task } => {
                assert_eq!(task.due_date.as_deref(), Some("2026-07-01"));
            }
            _ => panic!("expected a task row"),
        }
    }

    /// The display-index -> Task resolution (the Space-toggle correctness core): the
    /// task under a given cursor index is the exact one that will be toggled, and a
    /// header index resolves to no task.
    #[test]
    fn selected_task_resolves_to_underlying_task() {
        let mut app = App::new();
        app.tasks = vec![
            task(1, " ", 1, "alpha.md"),
            task(2, " ", 2, "alpha.md"),
            task(3, " ", 1, "beta.md"),
        ];
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        // rows: [H alpha, T1, T2, H beta]
        app.state.select(Some(2));
        assert_eq!(app.selected_task().map(|t| t.id), Some(2));
        // A header resolves to None.
        app.state.select(Some(3));
        assert!(app.selected_task().is_none());
    }

    /// `submit_toggle` enqueues the flip for the task under the cursor and never for a
    /// header. Verifies the trickiest invariant end-to-end through the real DB queue.
    #[test]
    fn submit_toggle_targets_cursor_task_not_header() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();
        db::upsert_task(&conn, &task(2, "x", 2, "alpha.md")).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All; // so the done task is also visible
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        // rows: [H alpha, T1(open), T2(done)]

        // Cursor on the header -> no enqueue.
        app.state.select(Some(0));
        app.submit_toggle(&conn);
        assert!(
            db::pending_actions(&conn).unwrap().is_empty(),
            "header must not toggle"
        );

        // Cursor on the done task (T2) -> enqueue flip back to open.
        app.state.select(Some(2));
        app.submit_toggle(&conn);
        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].task_id, 2);
        assert_eq!(pending[0].expected_char, "x");
        assert_eq!(pending[0].new_char, " ");
    }

    /// Collapsing a group from inside it (via `←`) drops the cursor on that group's
    /// header rather than letting it drift to an unrelated row.
    #[test]
    fn collapse_lands_selection_on_group_header() {
        let mut app = App::new();
        app.tasks = vec![
            task(1, " ", 1, "alpha.md"),
            task(2, " ", 2, "alpha.md"),
            task(3, " ", 1, "beta.md"),
        ];
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        // rows: [H alpha, T1, T2, H beta]; cursor on T2 (index 2).
        app.state.select(Some(2));
        app.toggle_at_cursor(ToggleMode::Collapse);
        // alpha now collapsed -> rows = [H alpha, H beta]; cursor on H alpha (index 0).
        assert_eq!(app.rows.len(), 2);
        assert_eq!(app.state.selected(), Some(0));
        assert!(matches!(app.rows[0], DisplayRow::Header { .. }));
    }

    /// Cycling the filter keeps the cursor in the same note when the selected task is
    /// filtered out, and keeps it on the exact task when it remains visible.
    #[test]
    fn cycle_filter_keeps_cursor_in_note() {
        let mut app = App::new();
        app.tasks = vec![task(1, " ", 1, "alpha.md"), task(2, "x", 2, "alpha.md")];
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        // Default Open -> rows: [H alpha, T1]; land on T1.
        assert_eq!(app.filter, StatusFilter::Open);
        app.state.select(Some(1));
        assert_eq!(app.selected_task().map(|t| t.id), Some(1));

        // Cycle to Done: T1 (open) hidden; alpha still has T2 (done) -> cursor moves to
        // alpha's header (same note), not a stranger row.
        app.cycle_filter();
        assert_eq!(app.filter, StatusFilter::Done);
        assert_eq!(app.state.selected(), Some(0));
        assert!(matches!(app.rows[0], DisplayRow::Header { .. }));

        // Cycle to All: T1 reappears; cursor stays on the alpha header (its current
        // position), predictably, rather than snapping back.
        app.cycle_filter();
        assert_eq!(app.filter, StatusFilter::All);
        assert_eq!(app.state.selected(), Some(0));
    }

    /// Toggling a header flips just that group and leaves the cursor on it.
    #[test]
    fn toggle_header_flips_its_group_only() {
        let mut app = App::new();
        app.tasks = vec![task(1, " ", 1, "alpha.md"), task(2, " ", 1, "beta.md")];
        app.rebuild();
        // rows: [H alpha, H beta]; cursor on alpha header.
        app.state.select(Some(0));
        app.toggle_at_cursor(ToggleMode::Toggle); // expand alpha
        assert_eq!(app.rows.len(), 3, "alpha now shows its task row");
        assert!(matches!(app.rows[1], DisplayRow::Task { .. }));
        assert_eq!(
            app.state.selected(),
            Some(0),
            "cursor stays on alpha header"
        );
        // Beta stays collapsed.
        assert!(matches!(app.rows[2], DisplayRow::Header { collapsed, .. } if collapsed));
    }

    /// expand_all / collapse_all affect every visible group.
    #[test]
    fn expand_all_and_collapse_all() {
        let mut app = App::new();
        app.tasks = vec![task(1, " ", 1, "alpha.md"), task(2, " ", 1, "beta.md")];
        app.rebuild();
        assert_eq!(app.rows.len(), 2, "collapsed by default");

        app.expand_all();
        assert_eq!(app.rows.len(), 4, "two headers + two task rows");

        app.collapse_all();
        assert_eq!(app.rows.len(), 2, "back to two headers");
    }

    /// Reconcile clears the selection when the view is empty.
    #[test]
    fn reconcile_view_selection_clears_when_empty() {
        let mut state = ListState::default();
        state.select(Some(2));
        reconcile_view_selection(&[], None, None, Some(2), &mut state);
        assert_eq!(state.selected(), None);
    }

    /// Reconcile selects row 0 when the view is newly non-empty.
    #[test]
    fn reconcile_view_selection_picks_first_when_was_none() {
        let rows = vec![DisplayRow::Header {
            note_path: "a.md".to_string(),
            open_count: 1,
            total_count: 1,
            collapsed: true,
        }];
        let mut state = ListState::default();
        reconcile_view_selection(&rows, None, None, None, &mut state);
        assert_eq!(state.selected(), Some(0));
    }

    /// Reconcile follows a task across a rebuild when it is still visible (stable
    /// cursor across refreshes even if its line position shifts).
    #[test]
    fn reconcile_view_selection_follows_visible_task() {
        let rows1 = vec![
            DisplayRow::Header {
                note_path: "a.md".to_string(),
                open_count: 2,
                total_count: 2,
                collapsed: false,
            },
            DisplayRow::Task {
                task: task(10, " ", 1, "a.md"),
            },
            DisplayRow::Task {
                task: task(11, " ", 2, "a.md"),
            },
        ];
        let mut state = ListState::default();
        state.select(Some(2)); // on task id 11

        // A rebuild where task 11 moved up to index 1 (e.g. task 10 deleted).
        let rows2 = vec![
            DisplayRow::Header {
                note_path: "a.md".to_string(),
                open_count: 1,
                total_count: 1,
                collapsed: false,
            },
            DisplayRow::Task {
                task: task(11, " ", 2, "a.md"),
            },
        ];
        reconcile_view_selection(&rows2, Some("a.md"), Some(11), Some(2), &mut state);
        assert_eq!(
            state.selected(),
            Some(1),
            "should follow task 11 to its new index"
        );
        let _ = rows1; // silence unused warning; rows1 documents the "before" state
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

        let t = task(1, " ", 3, "n.md");
        enqueue_toggle(&conn, &t).unwrap();

        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        let p = &pending[0];
        assert_eq!(p.task_id, 1);
        assert_eq!(p.note_path, "n.md");
        assert_eq!(p.line_number, 3);
        assert_eq!(p.expected_char, " ");
        assert_eq!(p.new_char, "x", "open -> done");
        assert_eq!(p.state, "pending");
        assert!(p.error.is_none());

        // A done task enqueues a flip back to open.
        db::resolve_action(&conn, p.id, "done", None).unwrap(); // clear the queue
        let done_task = task(2, "x", 7, "n.md");
        enqueue_toggle(&conn, &done_task).unwrap();
        let p2 = &db::pending_actions(&conn).unwrap()[0];
        assert_eq!(p2.expected_char, "x");
        assert_eq!(p2.new_char, " ");
    }
}
