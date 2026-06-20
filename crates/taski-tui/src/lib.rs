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

use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, terminal::EnterAlternateScreen, terminal::LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use rusqlite::Connection;

use taski_db as db;
use taski_db::{NoteContent, PendingAction, Priority, Status, Task};

/// CLI configuration. `--db` is optional and overrides `db` in the config file
/// (`~/.config/taski/config.toml`, overridable via `TASKI_CONFIG`); see
/// `taski_config`. The TUI reads the same DB the daemon writes.
#[derive(Parser, Debug)]
#[command(
    name = "taski-tui",
    version,
    about = "Live, browsable task list reader for the taski SQLite index"
)]
struct Cli {
    /// Path to the taski SQLite index database. Overrides `db` in the config file;
    /// defaults to `./taski.db` if absent everywhere.
    #[arg(long)]
    db: Option<PathBuf>,
}

/// How long `event::poll` blocks waiting for input between redraws.
const POLL_TIMEOUT: Duration = Duration::from_millis(250);
/// Re-read the index at least this often, independent of input.
const REFRESH_INTERVAL: Duration = Duration::from_millis(750);
/// How many of the most-recent action resolutions to read back each refresh, to learn
/// whether actions the TUI enqueued this session were applied or refused.
const RECENT_ACTION_LIMIT: i64 = 64;
/// Upper bound on the number of unresolved session actions we track. The set is
/// drained as the daemon resolves them, so this only bounds growth if the daemon
/// stalls; oldest entries are dropped first.
const TRACK_CAP: usize = 64;
/// Minimum terminal width (in columns) at which the context pane is shown alongside
/// the list. Below this the list takes the full width so neither side is unreadably
/// narrow. The pane can also be toggled off explicitly with `p`.
const MIN_SPLIT_WIDTH: u16 = 60;

/// A caller-provided quit callback. The unified launcher passes one (in combined mode)
/// that signals the daemon's `ShutdownSignal` so the daemon drains and exits when the
/// user quits the TUI. Standalone runs pass `None`.
type QuitHook = Arc<dyn Fn() + Send + Sync>;

/// Run the TUI standalone, parsing its own CLI (`--db`). Used by the `taski-tui`
/// binary's thin `main`. Returns `anyhow::Result` so the caller propagates errors.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    run_inner(cli.db, None)
}

/// Run the TUI with a resolved `db` override and NO quit hook. Used by the unified
/// launcher's `taski tui` subcommand (and, in Phase C, its attach path: TUI-only
/// against an already-running daemon). `db_override` is the launcher's `--db` flag
/// (`None` ⇒ resolve from config / default). This entry does **not** re-parse argv, so
/// it composes cleanly under the launcher's subcommand dispatch.
pub fn run_with_db(db_override: Option<PathBuf>) -> Result<()> {
    run_inner(db_override, None)
}

/// Run the TUI in combined mode: with a `db` override AND a quit hook the launcher uses
/// to trigger cooperative daemon shutdown (ADR-0007) when the user quits (`q` / `Esc` /
/// `Ctrl-C`). The TUI stays decoupled from `taski-daemon` — the hook is an opaque
/// callback, so this crate takes no new dependency.
pub fn run_combined(
    db_override: Option<PathBuf>,
    quit_hook: impl Fn() + Send + Sync + 'static,
) -> Result<()> {
    run_inner(db_override, Some(Arc::new(quit_hook)))
}

/// Shared TUI lifecycle: load config, resolve the db (honoring `db_override`), open the
/// reader connection, install the panic hook (restore the terminal on any panic), enter
/// the terminal, run the loop (invoking `quit_hook` on quit if present), and restore the
/// terminal. `db_override` flows from the caller so the launcher never re-parses argv.
fn run_inner(db_override: Option<PathBuf>, quit_hook: Option<QuitHook>) -> Result<()> {
    // Config is optional (a missing file yields defaults); a malformed file is a
    // hard error. Resolve db: override → config → ./taski.db.
    let cfg = taski_config::load().context("loading taski config")?;
    let db_path = taski_config::resolve_db(db_override.as_deref().and_then(Path::to_str), &cfg);

    // Restore the terminal even if a panic occurs mid-render.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));

    // One long-lived reader connection: WAL lets it coexist with the daemon's writer
    // (separate process, or a separate thread in combined mode) for the whole session.
    let conn = db::open(&db_path.to_string_lossy()).context("opening taski database")?;

    let mut terminal = enter_terminal()?;
    let result = run_loop(&mut terminal, &conn, quit_hook.as_ref());
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

/// Derive "today" as a `YYYY-MM-DD` string from the wall clock, via the pure
/// `taski_core::ymd_from_unix` (no date crate — ADR-0009 Phase 1). The TUI calls
/// this on `App` construction and on each index refresh so a session spanning
/// midnight keeps the Today view correct. Falls back to the epoch on a
/// pre-epoch clock (matching `taski_core`'s convention).
fn today_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // `ymd_from_unix` is re-exported by `taski-db` so the TUI takes no direct
    // `taski-core` dependency (the established re-export pattern).
    db::ymd_from_unix(secs)
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

/// Grouping axis cycled with `G`: Note → Tag → Priority → Folder → Note. The
/// default is Note (the classic "one group per source file" view). Tag fans out
/// a single task to multiple groups; Priority and Folder produce exactly one key
/// per task.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GroupBy {
    Note,
    Tag,
    Priority,
    Folder,
}

impl GroupBy {
    /// Cycle to the next axis: Note → Tag → Priority → Folder → Note.
    fn next(self) -> Self {
        match self {
            GroupBy::Note => GroupBy::Tag,
            GroupBy::Tag => GroupBy::Priority,
            GroupBy::Priority => GroupBy::Folder,
            GroupBy::Folder => GroupBy::Note,
        }
    }

    /// Short label for the title-bar indicator.
    fn label(self) -> &'static str {
        match self {
            GroupBy::Note => "note",
            GroupBy::Tag => "tag",
            GroupBy::Priority => "priority",
            GroupBy::Folder => "folder",
        }
    }
}

/// Return the group key(s) for a task under the given axis. `Tag` can fan out to
/// multiple keys (one per tag); the other axes always return exactly one.
/// Untagged tasks go to `(untagged)`, no-priority/unknown to `(no priority)`,
/// no-folder (top-level note) to `(root)`.
fn group_keys(task: &Task, axis: GroupBy) -> Vec<String> {
    match axis {
        GroupBy::Note => vec![task.note_path.clone()],
        GroupBy::Tag => {
            if task.tags.is_empty() {
                vec!["(untagged)".to_string()]
            } else {
                task.tags.to_vec()
            }
        }
        GroupBy::Priority => vec![priority_group_label(task.priority.as_ref())],
        GroupBy::Folder => vec![folder_of(&task.note_path)],
    }
}

/// Human-readable label for a task's priority bucket. `Other` (unknown glyph)
/// and `None` both collapse to `(no priority)` so the group list stays clean.
fn priority_group_label(priority: Option<&Priority>) -> String {
    match priority {
        Some(Priority::Highest) => "Highest".to_string(),
        Some(Priority::High) => "High".to_string(),
        Some(Priority::Medium) => "Medium".to_string(),
        Some(Priority::Low) => "Low".to_string(),
        Some(Priority::Lowest) => "Lowest".to_string(),
        Some(Priority::Other(_)) | None => "(no priority)".to_string(),
    }
}

/// The parent directory of a note path, or `(root)` for top-level notes.
fn folder_of(note_path: &str) -> String {
    Path::new(note_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("(root)")
        .to_string()
}

/// Sort rank for priority-group headers: 0 = Highest … 4 = Lowest, 5 = (no
/// priority). Used to order Priority-axis groups by importance (alphabetical
/// would wrongly put "Lowest" before "Medium").
fn priority_sort_rank(label: &str) -> u8 {
    match label {
        "Highest" => 0,
        "High" => 1,
        "Medium" => 2,
        "Low" => 3,
        "Lowest" => 4,
        _ => 5,
    }
}

/// One renderable row in the grouped list. `Header` carries per-note counts computed
/// from the full (unfiltered) task set so the triage overview stays accurate under any
/// filter; `Task` carries the task the cursor can act on. The `Task` arm is boxed so
/// `Task`'s size (it grew substantially after the Tier 1 metadata fields landed)
/// doesn't dominate the enum's stack footprint.
#[derive(Debug, Clone)]
enum DisplayRow {
    Header {
        group_key: String,
        open_count: usize,
        total_count: usize,
        collapsed: bool,
    },
    Task {
        task: Box<Task>,
    },
}

impl DisplayRow {
    /// The group key this row belongs to (the header's key, or the task's source
    /// note path). For non-Note axes the header's key is the group label (tag,
    /// priority, folder) rather than a file path.
    fn note_path(&self) -> &str {
        match self {
            DisplayRow::Header { group_key, .. } => group_key,
            DisplayRow::Task { task } => &task.note_path,
        }
    }
}

/// Build the flat list of display rows from the raw task list, the active filter, and
/// the set of expanded group keys. Tasks are assumed sorted by `(note_path,
/// line_number)` — the order `db::all_tasks` returns — so within each group the task
/// order is preserved from the input (line order for the Note axis).
///
/// `group_by` controls the grouping axis (cycled with `G`):
/// - **Note** (default): one group per source note path.
/// - **Tag**: one group per tag; an untagged task goes to `(untagged)`. A task with
///   multiple tags appears in every matching group (fan-out).
/// - **Priority**: one group per priority level (`Highest` … `Lowest`), ordered by
///   importance; no-priority / unknown goes to `(no priority)`.
/// - **Folder**: one group per parent directory; top-level notes go to `(root)`.
///
/// Groups default to **collapsed**: a key not present in `expanded` is folded. This
/// inverts the natural "track what's open" model so newly-appearing groups (added by
/// the daemon between refreshes) also start collapsed without special handling.
///
/// Groups with no filter-matching task are hidden entirely (no empty headers).
/// Headers always carry the true open/total counts (from the full group, ignoring the
/// filter); task rows are emitted only when the group is expanded.
///
/// `today_only` (ADR-0009 Phase 1) adds an orthogonal, stricter predicate on top of
/// `filter`: when true, only tasks whose `scheduled_date == Some(today)` are visible.
/// It is kept independent of `filter` (today-ness vs open/done) so the two compose —
/// e.g. `today_only + Open` = today's open work. `today` is a `YYYY-MM-DD` string; it
/// is only consulted when `today_only` is true.
///
/// `overdue_only` adds a fifth orthogonal predicate: when true, only tasks whose
/// `due_date` is set and strictly before `today` are visible. A task with no
/// `due_date` is never overdue. Purely date-based (does NOT additionally require
/// `status == Open` — that's the status filter's job) so it composes predictably:
/// `overdue_only + Open` = open past-due; `overdue_only + Done` = completed-was-
/// overdue review. String comparison `d < today` is valid for `YYYY-MM-DD`
/// (lexicographic == chronological for zero-padded ISO dates).
//
// `too_many_arguments`: each parameter is an independent filter/grouping axis (status,
// today, search, file, overdue, group-by) plus its required context (tasks, expanded,
// today-string). A parameter struct was considered but would churn every call
// site for no clarity gain on an internal, heavily-tested function. The allow
// is intentional.
#[allow(clippy::too_many_arguments)]
fn build_view(
    tasks: &[Task],
    filter: StatusFilter,
    expanded: &HashSet<String>,
    today_only: bool,
    today: &str,
    search_query: &str,
    file_query: &str,
    overdue_only: bool,
    group_by: GroupBy,
) -> Vec<DisplayRow> {
    let scheduled_today =
        |t: &Task| -> bool { !today_only || t.scheduled_date.as_deref() == Some(today) };
    let matches_search = |t: &Task| -> bool {
        search_query.is_empty() || t.text.to_lowercase().contains(&search_query.to_lowercase())
    };
    let matches_file = |t: &Task| -> bool {
        file_query.is_empty()
            || t.note_path
                .to_lowercase()
                .contains(&file_query.to_lowercase())
    };
    let not_overdue =
        |t: &Task| -> bool { !overdue_only || t.due_date.as_deref().is_some_and(|d| d < today) };
    let passes_filters = |t: &Task| -> bool {
        filter.matches(&t.status)
            && scheduled_today(t)
            && matches_search(t)
            && matches_file(t)
            && not_overdue(t)
    };

    // Bucket tasks by group key.  `order` preserves first-seen ordering (so
    // within a given sort the relative position of same-rank keys is stable);
    // `index` maps key → position in `buckets`.  For the Tag axis a task may
    // land in multiple buckets.
    let mut order: Vec<String> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut buckets: Vec<Vec<&Task>> = Vec::new();

    for t in tasks {
        for key in group_keys(t, group_by) {
            if let Some(&i) = index.get(&key) {
                buckets[i].push(t);
            } else {
                index.insert(key.clone(), buckets.len());
                order.push(key);
                buckets.push(vec![t]);
            }
        }
    }

    // Sort group order: Priority by importance, everything else alphabetical.
    if group_by == GroupBy::Priority {
        order.sort_by_key(|k| priority_sort_rank(k));
    } else {
        order.sort();
    }

    let mut rows = Vec::new();
    for key in &order {
        let bucket = &buckets[index[key]];
        let total_count = bucket.len();
        let open_count = bucket.iter().filter(|t| t.status == Status::Open).count();
        let visible: Vec<&Task> = bucket
            .iter()
            .copied()
            .filter(|t| passes_filters(t))
            .collect();
        if visible.is_empty() {
            continue;
        }
        let is_expanded = expanded.contains(key.as_str());
        rows.push(DisplayRow::Header {
            group_key: key.clone(),
            open_count,
            total_count,
            collapsed: !is_expanded,
        });
        if is_expanded {
            for t in visible {
                rows.push(DisplayRow::Task {
                    task: Box::new(t.clone()),
                });
            }
        }
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
    /// Ids of actions enqueued this session that the daemon hasn't resolved yet. Each
    /// refresh we read back their resolution: `done` drops them silently (the flip is
    /// already visible), `failed` surfaces a notice, `pending` stays tracked.
    pending_session_actions: Vec<i64>,
    /// The current write-back failure notice to display, if any. `None` means nothing
    /// to report. Set when a tracked action resolves `failed`; cleared the next time
    /// the user enqueues an action (the natural "try again / move on" gesture).
    notice: Option<String>,
    /// The note whose content is currently cached for the context pane (ADR-0006).
    /// `None` when nothing is selected / the view is empty. The TUI reads note content
    /// only through the index (`db::note_content`), never from the vault.
    ctx_note_path: Option<String>,
    /// Cached full text of `ctx_note_path` (or `None` if that note isn't cached in the
    /// index). Re-fetched when the selection moves to a different note and force-read on
    /// each refresh so toggles/edits land within ~1 poll.
    ctx_content: Option<NoteContent>,
    /// Whether the context pane is shown. Toggled with `p`; also hidden automatically
    /// below [`MIN_SPLIT_WIDTH`] columns so neither pane is unreadably narrow.
    pane_visible: bool,
    /// Manual scroll offset (in lines) applied on top of the auto-centered context
    /// window. `J`/`K` adjust it; any task navigation resets it to 0 (recenter on the
    /// new task). It survives index refreshes so a scroll isn't undone ~750ms later.
    ctx_scroll: i32,
    /// ADR-0009 Phase 1: when true, `build_view` additionally restricts the list to
    /// tasks whose `scheduled_date == today`. Independent of `filter` (today-ness and
    /// open/done are orthogonal axes): `today_only + Open` = today's open work.
    /// Toggled with `T` (lowercase `t` is reserved for the Phase 2 mark gesture).
    today_only: bool,
    /// The wall-clock "today" as `YYYY-MM-DD`, derived via `taski_core::ymd_from_unix`
    /// (pure, no date crate). Stored on the struct (not recomputed each render) so it
    /// stays stable across a single draw and is straightforward to pin in tests.
    today: String,
    /// ADR-0010: search query for filtering tasks by text. Empty = inactive.
    /// Populated while the user is typing at the `/` prompt and stays applied
    /// after the prompt is dismissed (until cleared with `Esc`).
    search_query: String,
    /// Whether the `/` search prompt is active (capturing keystrokes).
    /// When true, most key events build the query instead of performing
    /// their normal action.
    searching: bool,
    /// ADR-0010: file/path search query. Empty = inactive. Populated while
    /// typing at the `F` prompt and stays applied until dismissed with `Esc`.
    file_query: String,
    /// Whether the `F` file-search prompt is active.
    file_searching: bool,
    /// Overdue filter (`O`): when true, `build_view` additionally restricts the
    /// list to tasks whose `due_date` is set and strictly before `today`.
    /// Independent of `filter`, `today_only`, and the search axes (orthogonal).
    /// Purely date-based — does NOT additionally require `status == Open`.
    overdue_only: bool,
    /// ADR-0011: the last enqueued write action, for undo (`u` key).
    last_action: Option<LastAction>,
    /// Grouping axis cycled with `G` (Note → Tag → Priority → Folder). Defaults
    /// to Note. The `expanded` set keys match the active axis's group labels, so
    /// switching axes naturally starts every group collapsed (old keys won't
    /// match the new axis's labels) without needing to clear the set.
    group_by: GroupBy,
}

/// ADR-0011: information needed to reverse the last write action.
#[derive(Clone, Debug)]
enum LastAction {
    CheckboxToggle {
        task_id: i64,
        note_path: String,
        line_number: usize,
        expected_char: String,
        new_char: String,
    },
    BulletToggle {
        task_id: i64,
        note_path: String,
        line_number: usize,
    },
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
            pending_session_actions: Vec::new(),
            notice: None,
            ctx_note_path: None,
            ctx_content: None,
            pane_visible: true,
            ctx_scroll: 0,
            today_only: false,
            today: today_string(),
            search_query: String::new(),
            searching: false,
            file_query: String::new(),
            file_searching: false,
            overdue_only: false,
            last_action: None,
            group_by: GroupBy::Note,
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

    /// Rebuild rows from the current tasks/filter/expanded/search and preserve selection.
    fn rebuild(&mut self) {
        let (note, task_id, idx) = self.snapshot();
        self.rows = build_view(
            &self.tasks,
            self.filter,
            &self.expanded,
            self.today_only,
            &self.today,
            &self.search_query,
            &self.file_query,
            self.overdue_only,
            self.group_by,
        );
        reconcile_view_selection(&self.rows, note.as_deref(), task_id, idx, &mut self.state);
    }

    // --- ADR-0010: text search / `/` prompt ---------------------------------

    /// Enter search mode. If a query already existed (e.g. from a prior search
    /// that was dismissed with `Enter`), it stays at the prompt so the user can
    /// edit or extend it.
    fn start_search(&mut self) {
        self.file_searching = false;
        self.searching = true;
    }

    /// Append a character to the search query and re-filter live.
    fn push_search_char(&mut self, c: char) {
        self.search_query.push(c);
        self.rebuild();
    }

    /// Pop the last character (Backspace) and re-filter live.
    fn pop_search_char(&mut self) {
        self.search_query.pop();
        self.rebuild();
    }

    /// Exit search mode, keeping the current query applied as a filter.
    fn finish_search(&mut self) {
        self.searching = false;
    }

    /// Cancel search: clear the query, exit search mode, return to full list.
    fn clear_search(&mut self) {
        self.searching = false;
        if !self.search_query.is_empty() {
            self.search_query.clear();
            self.rebuild();
        }
    }

    // ── ADR-0010 file search (F key) ──────────────────────────────────

    /// Activate the file-search prompt (`F` gesture). Finishes any text-search
    /// prompt so only one prompt is active at a time.
    fn start_file_search(&mut self) {
        self.searching = false;
        self.file_searching = true;
    }

    /// Append a character to the file query and re-filter live.
    fn push_file_search_char(&mut self, c: char) {
        self.file_query.push(c);
        self.rebuild();
    }

    /// Pop the last character (Backspace) and re-filter live.
    fn pop_file_search_char(&mut self) {
        self.file_query.pop();
        self.rebuild();
    }

    /// Exit file-search mode, keeping the current file query applied.
    fn finish_file_search(&mut self) {
        self.file_searching = false;
    }

    /// Cancel file search: clear the file query, exit file-search mode.
    fn clear_file_search(&mut self) {
        self.file_searching = false;
        if !self.file_query.is_empty() {
            self.file_query.clear();
            self.rebuild();
        }
    }

    /// Re-read the index from the DB, then rebuild the view and poll the resolution
    /// of actions enqueued this session so a refused write-back gets surfaced.
    fn refresh(&mut self, conn: &Connection) -> Result<()> {
        // Refresh "today" so a session spanning midnight keeps the Today view correct.
        self.today = today_string();
        self.tasks = db::all_tasks(conn).context("reading tasks from index")?;
        self.rebuild();
        self.poll_action_resolutions(conn)?;
        // Force a re-read of the selected note's cached content so the pane reflects
        // toggles/edits the daemon re-indexed since the last refresh (ADR-0006).
        self.sync_context(conn, true);
        Ok(())
    }

    /// Read back the resolution of actions this session enqueued. For each tracked id
    /// that has now resolved: drop it from tracking; successes are silent (the flip is
    /// already visible on refresh), failures surface a notice. Still-pending actions
    /// stay tracked. If several fail in one cycle, the newest is surfaced.
    fn poll_action_resolutions(&mut self, conn: &Connection) -> Result<()> {
        if self.pending_session_actions.is_empty() {
            return Ok(());
        }
        let recent = db::recent_actions(conn, RECENT_ACTION_LIMIT)
            .context("reading recent action resolutions")?;
        let tracked: HashSet<i64> = self.pending_session_actions.iter().copied().collect();

        // `recent` is newest-resolved first; the first failed tracked row is the newest
        // failure to surface. Compute the notice before mutating self so the borrows
        // don't overlap.
        let notice = recent
            .iter()
            .find(|a| tracked.contains(&a.id) && a.state == "failed")
            .map(render_failure_notice);
        // Every tracked id present in `recent` has resolved (recent only holds
        // done/failed rows) — drop those from tracking.
        let resolved: HashSet<i64> = recent
            .iter()
            .map(|a| a.id)
            .filter(|id| tracked.contains(id))
            .collect();
        self.pending_session_actions
            .retain(|id| !resolved.contains(id));
        if let Some(msg) = notice {
            self.notice = Some(msg);
        }
        Ok(())
    }

    /// `f`: cycle All -> Open -> Done -> All, preserving selection.
    fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.rebuild();
        self.ctx_scroll = 0;
    }

    /// `G`: cycle the grouping axis Note → Tag → Priority → Folder → Note.
    /// Does not clear `expanded` — stale keys from the old axis naturally won't
    /// match the new axis's group labels, so every group starts collapsed.
    fn cycle_group_by(&mut self) {
        self.group_by = self.group_by.next();
        self.rebuild();
        self.ctx_scroll = 0;
    }

    /// `T`: toggle the ADR-0009 "Today" view — when on, `build_view` additionally
    /// restricts the list to tasks whose `scheduled_date == today`. Independent of
    /// the `f` status-cycle. Lowercase `t` is intentionally NOT bound (reserved for
    /// the Phase 2 mark gesture).
    fn toggle_today(&mut self) {
        self.today_only = !self.today_only;
        self.rebuild();
        self.ctx_scroll = 0;
    }

    /// `O`: toggle the overdue filter — when on, `build_view` additionally
    /// restricts the list to tasks whose `due_date` is set and strictly before
    /// `today`. Independent of `today_only` and the status filter (orthogonal
    /// axes): `O + Open` = open past-due; `O + Done` = completed-was-overdue
    /// review; `O + All` = all past-due. A task with no `due_date` is never
    /// overdue.
    fn toggle_overdue(&mut self) {
        self.overdue_only = !self.overdue_only;
        self.rebuild();
        self.ctx_scroll = 0;
    }

    /// Toggle / expand / collapse the group under the cursor. `Enter` toggles a
    /// header; `→` forces expand; `←` forces collapse and, when pressed on a task row,
    /// collapses that task's parent group (fold from inside). All other key/row
    /// combinations are no-ops.
    ///
    /// For the fold-from-task gesture (`←` on a task), the parent group key is
    /// found by scanning backwards from the cursor for the nearest preceding
    /// Header — under non-Note axes the group key is no longer the task's own
    /// `note_path`.
    fn toggle_at_cursor(&mut self, mode: ToggleMode) {
        let action: Option<(String, bool)> = {
            let Some(idx) = self.state.selected() else {
                return;
            };
            let Some(row) = self.rows.get(idx) else {
                return;
            };
            match row {
                DisplayRow::Header { group_key, .. } => {
                    let is_expanded = self.expanded.contains(group_key.as_str());
                    let want_expanded = match mode {
                        ToggleMode::Toggle => !is_expanded,
                        ToggleMode::Expand => true,
                        ToggleMode::Collapse => false,
                    };
                    Some((group_key.clone(), want_expanded))
                }
                DisplayRow::Task { .. } => {
                    // Only `←` (Collapse) is meaningful on a task: fold its
                    // parent group. Scan backwards for the nearest preceding
                    // Header to find the group key (works under any axis).
                    if matches!(mode, ToggleMode::Collapse) {
                        let key = self.rows[..idx].iter().rev().find_map(|r| match r {
                            DisplayRow::Header { group_key, .. } => Some(group_key.clone()),
                            _ => None,
                        });
                        key.map(|k| (k, false))
                    } else {
                        None
                    }
                }
            }
        };
        let Some((key, want_expanded)) = action else {
            return;
        };
        if want_expanded {
            self.expanded.insert(key);
        } else {
            self.expanded.remove(&key);
        }
        self.rebuild();
        self.ctx_scroll = 0;
    }

    /// `Tab`: expand every group currently visible.
    fn expand_all(&mut self) {
        for row in &self.rows {
            if let DisplayRow::Header { group_key, .. } = row {
                self.expanded.insert(group_key.clone());
            }
        }
        self.rebuild();
        self.ctx_scroll = 0;
    }

    /// `Shift-Tab`: collapse every group.
    fn collapse_all(&mut self) {
        self.expanded.clear();
        self.rebuild();
        self.ctx_scroll = 0;
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
        // Recenter the context pane on the newly-selected task.
        self.ctx_scroll = 0;
    }

    /// `J`/`K`: scroll the context pane by `delta` lines (positive = down). Bounded at
    /// draw time against the note length, so the key handler need not know the pane size.
    fn scroll_context(&mut self, delta: i32) {
        self.ctx_scroll = self.ctx_scroll.saturating_add(delta);
    }

    /// `p`: show/hide the context pane. When hidden (or below [`MIN_SPLIT_WIDTH`]) the
    /// list reclaims the full width.
    fn toggle_pane(&mut self) {
        self.pane_visible = !self.pane_visible;
    }

    /// The task under the cursor, if the cursor is on a task row (never a header).
    fn selected_task(&self) -> Option<&Task> {
        let idx = self.state.selected()?;
        match self.rows.get(idx)? {
            DisplayRow::Task { task } => Some(task.as_ref()),
            _ => None,
        }
    }

    /// The note under the cursor — the selected task's note, or the selected header's
    /// note. Drives which note's content the context pane shows. `None` when the view
    /// is empty.
    fn selected_note_path(&self) -> Option<String> {
        let idx = self.state.selected()?;
        Some(self.rows.get(idx)?.note_path().to_string())
    }

    /// Keep the context pane's cached note content in step with the selection (ADR-0006).
    /// Reads `db::note_content` only when the selection moved to a *different* note (or
    /// `force` is set, used on refresh to pick up re-indexed content). The TUI still
    /// never opens a vault file — this is an index read. Read errors are swallowed
    /// and never propagated (a stale/blank pane must not kill the session; S1: the
    /// TUI owns the alternate screen, so it must not write to stderr).
    fn sync_context(&mut self, conn: &Connection, force: bool) {
        let target = self.selected_note_path();
        let changed = target.as_deref() != self.ctx_note_path.as_deref();
        match target {
            None => {
                self.ctx_note_path = None;
                self.ctx_content = None;
                self.ctx_scroll = 0;
            }
            Some(np) => {
                if force || changed {
                    // S1: never write to stderr from the TUI thread — it owns the
                    // alternate screen, so any stderr output garbles the display.
                    // A failed read just leaves the context pane empty.
                    self.ctx_content = db::note_content(conn, &np).unwrap_or_default();
                    self.ctx_note_path = Some(np);
                    if changed {
                        // Selection moved to a different note: recenter the pane.
                        self.ctx_scroll = 0;
                    }
                }
            }
        }
    }

    /// Enqueue a checkbox-flip for the task under the cursor. No-op on a header or an
    /// empty list — the flip must always resolve to the exact task the user sees, never
    /// a header row. On success the new action id is tracked so its resolution is
    /// surfaced on a later refresh, and any prior notice is cleared (enqueueing again
    /// is the natural "try again / move on" gesture). Enqueue errors are swallowed
    /// and never propagated (the TUI owns the alternate screen; writing to stderr
    /// would garble it).
    fn submit_toggle(&mut self, conn: &Connection) {
        let (result, last_action) = {
            let Some(task) = self.selected_task() else {
                return;
            };
            let new_char = toggle_target_char(&task.raw_checkbox_char);
            let action = LastAction::CheckboxToggle {
                task_id: task.id,
                note_path: task.note_path.clone(),
                line_number: task.line_number,
                expected_char: task.raw_checkbox_char.clone(),
                new_char: new_char.to_string(),
            };
            (enqueue_toggle(conn, task), Some(action))
        };
        // S4: only record an undoable action if the enqueue actually succeeded —
        // otherwise `u` would try to reverse a write that never landed.
        if result.is_ok() {
            self.last_action = last_action;
        }
        self.track_enqueued(result, "toggle");
    }

    /// `b` (ADR-0011): toggle the selected task between checkbox and bullet format.
    /// If the line has a checkbox (`- [ ] text` / `- [x] text`), it becomes a plain
    /// bullet (`- text`). If it's already a bullet, it becomes an open checkbox
    /// (`- [ ] text`). The actual vault write is the daemon's job via
    /// [`db::enqueue_bullet_toggle`]; the TUI never touches files directly.
    fn submit_bullet_toggle(&mut self, conn: &Connection) {
        let (result, last_action) = {
            let Some(task) = self.selected_task() else {
                return;
            };
            let action = LastAction::BulletToggle {
                task_id: task.id,
                note_path: task.note_path.clone(),
                line_number: task.line_number,
            };
            (enqueue_bullet_toggle(conn, task), Some(action))
        };
        if result.is_ok() {
            self.last_action = last_action;
        }
        self.track_enqueued(result, "bullet toggle");
    }

    /// `u` (ADR-0011): undo the last write action. If the last action was a checkbox
    /// toggle (`Space`), queue the reverse flip (swapped expected/new chars). If it was
    /// a bullet toggle (`b`), queue another bullet toggle (it's self-inverse). If there
    /// is no tracked action, this is a no-op.
    fn submit_undo(&mut self, conn: &Connection) {
        let action = match self.last_action.clone() {
            Some(a) => a,
            None => return,
        };
        let result = match action {
            LastAction::CheckboxToggle {
                task_id,
                note_path,
                line_number,
                expected_char,
                new_char,
            } => enqueue_undo_checkbox(
                conn,
                task_id,
                &note_path,
                line_number,
                // M1: swap expected/new so the undo enqueues the *reverse* flip —
                // the undo's expected_char is the original toggle's new_char, and
                // vice versa. (`enqueue_undo_checkbox` takes them in the same
                // parameter order as `enqueue_action`; the swap happens here.)
                &new_char,
                &expected_char,
            ),
            LastAction::BulletToggle {
                task_id,
                note_path,
                line_number,
            } => db::enqueue_bullet_toggle(conn, task_id, &note_path, line_number)
                .context("enqueuing undo bullet toggle"),
        };
        // Don't update last_action (undo doesn't get its own undo).
        self.track_enqueued(result, "undo");
    }

    /// Record the outcome of an enqueue ([`submit_toggle`] /
    /// [`submit_set_scheduled`]): on success, track the new id so its resolution is
    /// surfaced on a later refresh, clear any prior notice, and bound growth if the
    /// daemon stalls; on error, swallowed and never propagated (S1: the TUI owns the
    /// alternate screen, so writing to stderr would garble it; failures surface via
    /// the pending_actions resolution on the next refresh). Shared so both write
    /// gestures stay consistent.
    fn track_enqueued(&mut self, result: Result<i64>, _label: &str) {
        // S1: the Err arm is intentionally a no-op (see the doc comment above).
        if let Ok(id) = result {
            self.notice = None;
            self.pending_session_actions.push(id);
            // Bound growth if the daemon stalls: drop the oldest beyond the cap.
            if self.pending_session_actions.len() > TRACK_CAP {
                let drop_count = self.pending_session_actions.len() - TRACK_CAP;
                self.pending_session_actions.drain(0..drop_count);
            }
        }
    }

    /// `t` (ADR-0009 Phase 2): toggle the selected task's "mark for today" gesture.
    /// If the task's `scheduled_date` already equals `today`, the `⏳` token is
    /// cleared (desired = `None`); otherwise it's set to today (desired =
    /// `Some(today)`). No-op on a header or empty list, exactly like
    /// [`submit_toggle`]. The actual vault write is the daemon's job via
    /// [`db::enqueue_set_scheduled`]; the TUI never touches files directly.
    fn submit_set_scheduled(&mut self, conn: &Connection) {
        let result = {
            let Some(task) = self.selected_task() else {
                return;
            };
            // Toggle semantics: already-scheduled-today -> clear; otherwise mark.
            let desired = if task.scheduled_date.as_deref() == Some(self.today.as_str()) {
                None
            } else {
                Some(self.today.clone())
            };
            enqueue_set_scheduled(conn, task, desired.as_deref())
        };
        self.track_enqueued(result, "set scheduled date");
    }
}

// ---------------------------------------------------------------------------
// Event loop + rendering.
// ---------------------------------------------------------------------------

/// Main render+event loop. Holds one DB connection for the whole session and re-reads
/// the index on a ~750ms cadence so daemon writes appear live without blocking input.
/// Returns when the user requests to quit.
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    conn: &Connection,
    quit_hook: Option<&QuitHook>,
) -> Result<()> {
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

        // ADR-0010: when a search prompt is active, keystrokes build/clear the
        // query instead of performing their normal action. Only one prompt is
        // active at a time.
        if app.searching {
            match key.code {
                KeyCode::Esc => app.clear_search(),
                KeyCode::Enter => app.finish_search(),
                KeyCode::Backspace => app.pop_search_char(),
                KeyCode::Char(c) => app.push_search_char(c),
                _ => {}
            }
        } else if app.file_searching {
            match key.code {
                KeyCode::Esc => app.clear_file_search(),
                KeyCode::Enter => app.finish_file_search(),
                KeyCode::Backspace => app.pop_file_search_char(),
                KeyCode::Char(c) => app.push_file_search_char(c),
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    if let Some(hook) = quit_hook {
                        hook();
                    }
                    return Ok(());
                }
                KeyCode::Char('c') if ctrl => {
                    if let Some(hook) = quit_hook {
                        hook();
                    }
                    return Ok(());
                }
                KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
                // Space toggles the selected task open<->done via the daemon's write-back
                // queue (ADR-0002). The TUI never touches vault files directly.
                KeyCode::Char(' ') => app.submit_toggle(conn),
                KeyCode::Enter => app.toggle_at_cursor(ToggleMode::Toggle),
                KeyCode::Right => app.toggle_at_cursor(ToggleMode::Expand),
                KeyCode::Left => app.toggle_at_cursor(ToggleMode::Collapse),
                KeyCode::Char('f') => app.cycle_filter(),
                // `G`: cycle the grouping axis (Note → Tag → Priority → Folder).
                KeyCode::Char('G') => app.cycle_group_by(),
                // ADR-0009 Phase 2: `t` marks the selected task for today (or clears it
                // if already scheduled today) via the daemon's write-back queue — the
                // first non-checkbox vault write. Uppercase `T` toggles the read-only
                // Today view (below).
                KeyCode::Char('t') => app.submit_set_scheduled(conn),
                // ADR-0009 Phase 1: `T` toggles the Today view (read-only). Lowercase
                // `t` is the Phase 2 mark-for-today write gesture (above).
                KeyCode::Char('T') => app.toggle_today(),
                // Overdue filter (`O`): toggle the "past-due only" view. A 5th
                // orthogonal filter axis (date-based, like `T` but for `due_date <
                // today` instead of `scheduled_date == today`).
                KeyCode::Char('O') => app.toggle_overdue(),
                // ADR-0011: `b` toggles checkbox ↔ bullet; `u` undoes last write.
                KeyCode::Char('b') => app.submit_bullet_toggle(conn),
                KeyCode::Char('u') => app.submit_undo(conn),
                // ADR-0010 text search: `/` opens the search prompt.
                KeyCode::Char('/') => app.start_search(),
                // ADR-0010 file search: `F` opens the file/path search prompt.
                KeyCode::Char('F') => app.start_file_search(),
                KeyCode::Tab => app.expand_all(),
                KeyCode::BackTab => app.collapse_all(),
                // Uppercase J/K scroll the context pane (lowercase j/k move the task list).
                KeyCode::Char('J') => app.scroll_context(1),
                KeyCode::Char('K') => app.scroll_context(-1),
                KeyCode::Char('p') => app.toggle_pane(),
                _ => {}
            }
        }

        // After any selection change, load the newly-selected note's content if the
        // selection moved to a different note (refreshes already force a re-read).
        app.sync_context(conn, false);
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
/// table. Non-blocking: just inserts a row; the daemon applies it. Returns the new
/// row id so the caller can track its resolution across refreshes.
fn enqueue_toggle(conn: &Connection, task: &Task) -> Result<i64> {
    let new_char = toggle_target_char(&task.raw_checkbox_char);
    let id = db::enqueue_action(
        conn,
        task.id,
        &task.note_path,
        task.line_number,
        &task.raw_checkbox_char,
        new_char,
    )
    .context("enqueuing toggle action")?;
    Ok(id)
}

/// Enqueue a "set scheduled date" request (ADR-0009 Phase 2) for `task` into the
/// shared `pending_actions` table. `desired` is `Some(YYYY-MM-DD)` to mark (or
/// re-schedule) the `⏳` token, or `None` to clear it. Non-blocking: just inserts a
/// row; the daemon applies it. Returns the new row id so the caller can track its
/// resolution across refreshes.
fn enqueue_set_scheduled(conn: &Connection, task: &Task, desired: Option<&str>) -> Result<i64> {
    let id = db::enqueue_set_scheduled(conn, task.id, &task.note_path, task.line_number, desired)
        .context("enqueuing set-scheduled action")?;
    Ok(id)
}

/// Enqueue a bullet-toggle request (ADR-0011): `- [ ] text` ↔ `- text`. Non-blocking;
/// the daemon applies it. Returns the new row id so the caller can track resolution.
fn enqueue_bullet_toggle(conn: &Connection, task: &Task) -> Result<i64> {
    let id = db::enqueue_bullet_toggle(conn, task.id, &task.note_path, task.line_number)
        .context("enqueuing bullet-toggle action")?;
    Ok(id)
}

/// Enqueue an undo for a previous checkbox flip — same as `enqueue_toggle` but the
/// `expected_char` and `new_char` are explicitly provided (swapped from the original).
fn enqueue_undo_checkbox(
    conn: &Connection,
    task_id: i64,
    note_path: &str,
    line_number: usize,
    expected_char: &str,
    new_char: &str,
) -> Result<i64> {
    let id = db::enqueue_action(
        conn,
        task_id,
        note_path,
        line_number,
        expected_char,
        new_char,
    )
    .context("enqueuing undo checkbox action")?;
    Ok(id)
}

/// Translate a daemon failure `error` string into short, plain wording the user can
/// act on. Keys off the stable phrases produced by the daemon's `ApplyOutcome` arms;
/// unknown errors fall back to a trimmed copy of the daemon message.
fn friendly_failure_reason(error: &str) -> String {
    let e = error.trim();
    if e.contains("note changed externally") {
        "this note changed in Obsidian".to_string()
    } else if e.contains("no longer in index") || e.contains("note gone") {
        "this task is no longer in the note".to_string()
    } else if e.contains("no longer matches expected bytes") {
        "the checkbox line changed".to_string()
    } else if e.contains("invalid new_char") {
        "the request was not valid".to_string()
    } else if e.contains("malformed or unparseable") {
        "the scheduled date on this line couldn't be parsed".to_string()
    } else if e.contains("could not be converted to a bullet") {
        "this line has no checkbox or bullet to toggle".to_string()
    } else if e.is_empty() {
        "it could not be applied".to_string()
    } else {
        e.to_string()
    }
}

/// Compose the one-line failure notice for a refused action: the outcome, the plain
/// reason, and the source note for context. The verb and the "try again" key are
/// chosen by action kind — each write gesture has its own retry key.
fn render_failure_notice(action: &PendingAction) -> String {
    let reason = action
        .error
        .as_deref()
        .map(friendly_failure_reason)
        .unwrap_or_else(|| "it could not be applied".to_string());
    let (verb, retry_key) = match action.action_type.as_str() {
        "set_scheduled" => ("Mark", "t"),
        "toggle_bullet" => ("Bullet", "b"),
        // The default/checkbox path.
        _ => ("Toggle", "Space"),
    };
    format!(
        "{verb} not applied — {reason} ({}). Press {retry_key} to try again.",
        action.note_path
    )
}

/// Render the grouped task list (or the empty placeholder), a title bar reflecting
/// the live counts and active filter, an optional write-back failure notice, and a
/// footer keybinding cheat-sheet. The notice row appears only when there's a failure
/// to surface, so the list keeps its full height when there's nothing to report.
fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    // The notice row is reserved only when a notice is present.
    let notice_present = app.notice.is_some();
    let constraints: Vec<Constraint> = if notice_present {
        vec![
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    } else {
        vec![Constraint::Min(1), Constraint::Length(1)]
    };
    let chunks = Layout::vertical(constraints).split(area);
    let list_area = chunks[0];
    let footer_area = chunks[chunks.len() - 1];
    let notice_area = notice_present.then(|| chunks[1]);

    // Split the list region into [task list | context pane] (ADR-0006), unless the pane
    // is toggled off (`p`) or the terminal is too narrow for both to be readable. The
    // list keeps its title/counts block; the pane gets its own block (note-path title).
    let show_pane = app.pane_visible && list_area.width >= MIN_SPLIT_WIDTH;
    let (list_col, ctx_col) = if show_pane {
        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(list_area);
        (cols[0], Some(cols[1]))
    } else {
        (list_area, None)
    };

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

    let mut title_spans: Vec<Span> = vec![
        Span::raw(" Taski — "),
        Span::styled(
            format!("filter: {}", app.filter.label()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  ·  "),
        Span::styled(
            format!("group: {}", app.group_by.label()),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    // ADR-0009 Phase 1: surface the Today view state so the user can see it's on.
    if app.today_only {
        title_spans.push(Span::raw("  ·  "));
        title_spans.push(Span::styled(
            "today",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // ADR-0010: surface active search query in the title bar.
    if !app.search_query.is_empty() {
        title_spans.push(Span::raw("  ·  "));
        title_spans.push(Span::styled(
            format!("search: {}", app.search_query),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // ADR-0010: surface active file/path search in the title bar.
    if !app.file_query.is_empty() {
        title_spans.push(Span::raw("  ·  "));
        title_spans.push(Span::styled(
            format!("file: {}", app.file_query),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Overdue filter indicator: red/bold to signal urgency.
    if app.overdue_only {
        title_spans.push(Span::raw("  ·  "));
        title_spans.push(Span::styled(
            "overdue",
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        ));
    }
    title_spans.push(Span::raw(format!(
        "   {open_total} open of {total} total   ·   {notes} notes "
    )));
    let title = Line::from(title_spans);
    let block = Block::default().borders(Borders::ALL).title(title);

    if app.rows.is_empty() {
        let msg = if app.tasks.is_empty() {
            "No tasks — run `cargo run -p taski-daemon` first to populate the index."
        } else if !app.search_query.is_empty() || !app.file_query.is_empty() {
            // ADR-0010: search or file search yielded no matches.
            "No tasks match the current search."
        } else if app.today_only {
            "No tasks scheduled for today. Press `T` to leave the Today view."
        } else if app.overdue_only {
            "No overdue tasks — nothing past its due date. Press `O` to leave the overdue view."
        } else {
            match app.filter {
                StatusFilter::Open => "No open tasks. Press `f` to change the filter.",
                StatusFilter::Done => "No done tasks. Press `f` to change the filter.",
                StatusFilter::All => "No tasks match.",
            }
        };
        frame.render_widget(Paragraph::new(msg).block(block), list_col);
    } else {
        let items: Vec<ListItem> = app
            .rows
            .iter()
            .map(|r| row_to_item(r, &app.today))
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, list_col, &mut app.state);
    }

    // Write-back failure notice: red, between the list and the footer, only when set.
    if let (Some(area), Some(msg)) = (notice_area, &app.notice) {
        let line = Line::from(vec![
            Span::styled(
                " ⚠  ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(msg.clone(), Style::default().fg(Color::Red)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    // Context pane (ADR-0006): show the selected task's note content in situ, with the
    // task's line highlighted. Read-only — the TUI gets this from the index, never the
    // vault. `target_line` is None when the cursor is on a group header. Skipped when the
    // pane is toggled off or the terminal is too narrow.
    if let Some(ctx_col) = ctx_col {
        let target_line = app.selected_task().map(|t| t.line_number);
        draw_context_pane(
            frame,
            ctx_col,
            app.ctx_note_path.as_deref(),
            app.ctx_content.as_ref(),
            target_line,
            app.ctx_scroll,
        );
    }

    // ADR-0010: when a search prompt is active, show it in the footer
    // instead of the normal keybinding help.
    let footer: Paragraph = if app.searching {
        let cursor = if (app.search_query.len() as u16) < footer_area.width.saturating_sub(4) {
            "█"
        } else {
            ""
        };
        Paragraph::new(Line::from(vec![
            Span::raw(" /"),
            Span::styled(app.search_query.clone(), Style::default().fg(Color::Green)),
            Span::styled(cursor, Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]))
        .style(Style::new())
    } else if app.file_searching {
        let cursor = if (app.file_query.len() as u16) < footer_area.width.saturating_sub(8) {
            "█"
        } else {
            ""
        };
        Paragraph::new(Line::from(vec![
            Span::raw(" File: /"),
            Span::styled(app.file_query.clone(), Style::default().fg(Color::Green)),
            Span::styled(cursor, Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]))
        .style(Style::new())
    } else {
        Paragraph::new(Line::from(vec![
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
            Span::styled("G", Style::default().fg(Color::Yellow)),
            Span::raw(" group by  ·  "),
            Span::styled("T", Style::default().fg(Color::Yellow)),
            Span::raw(" today  ·  "),
            Span::styled("O", Style::default().fg(Color::Yellow)),
            Span::raw(" overdue  ·  "),
            Span::styled("t", Style::default().fg(Color::Yellow)),
            Span::raw(" mark today  ·  "),
            Span::styled("b", Style::default().fg(Color::Yellow)),
            Span::raw(" bullet  ·  "),
            Span::styled("u", Style::default().fg(Color::Yellow)),
            Span::raw(" undo  ·  "),
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::raw(" search  ·  "),
            Span::styled("F", Style::default().fg(Color::Yellow)),
            Span::raw(" file search  ·  "),
            Span::styled("Tab/⇧Tab", Style::default().fg(Color::Yellow)),
            Span::raw(" expand/collapse all  ·  "),
            Span::styled("J/K", Style::default().fg(Color::Yellow)),
            Span::raw(" scroll context  ·  "),
            Span::styled("p", Style::default().fg(Color::Yellow)),
            Span::raw(" toggle pane  ·  "),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::raw(" quit "),
        ]))
        .style(Style::new().add_modifier(Modifier::DIM))
    };
    frame.render_widget(footer, footer_area);
}

/// Render one display row as a list item. Group headers are bold with a cyan
/// expand/collapse marker and dim counts; task rows are indented, with the checkbox
/// coloured by status, done tasks struck through, a yellow due date when present,
/// and a `⏳ <date>` scheduled-date suffix in cyan (bold/bright cyan when the
/// scheduled date is "today" — the ADR-0009 "this is a today task" affordance).
/// `today` is a `YYYY-MM-DD` string used only for the bold-today highlight.
fn row_to_item(row: &DisplayRow, today: &str) -> ListItem<'static> {
    match row {
        DisplayRow::Header {
            group_key,
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
                    group_key.clone(),
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
            let mut spans: Vec<Span> = Vec::with_capacity(8);
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
            // ADR-0009 Phase 1: scheduled-date suffix. Parallel to (not replacing)
            // the yellow due-date above. Cyan normally; bold when == today.
            if let Some(sched) = &task.scheduled_date {
                let is_today = sched.as_str() == today;
                let style = if is_today {
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                spans.push(Span::raw("  "));
                spans.push(Span::styled(format!("⏳ {sched}"), style));
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

// ---------------------------------------------------------------------------
// Context pane (ADR-0006): render the selected task's note content in situ.
// ---------------------------------------------------------------------------

/// Pure windowing: given a note's total line count, an optional target line (the
/// selected task's 1-based line; `None` for a group header), and the available pane
/// height in rows, decide which lines to show and where the highlight sits.
///
/// Returns `(start_line, count, highlight_offset)`:
/// - `start_line` — 1-based index of the first line to show.
/// - `count` — how many lines (from `start_line`) to show.
/// - `highlight_offset` — 0-based offset within the shown window of the target line, or
///   `None` when there is no target (header selected).
///
/// The target is centered within the window and clamped so the window never runs past
/// the note bounds. Pure (no I/O, no ratatui types) so it is unit-testable.
fn context_view(
    note_line_count: usize,
    target_line: Option<usize>,
    pane_height: usize,
) -> (usize, usize, Option<usize>) {
    if note_line_count == 0 || pane_height == 0 {
        return (1, 0, None);
    }
    // Never show more lines than the note has.
    let height = pane_height.min(note_line_count);
    let (start, highlight) = match target_line {
        None => (1, None),
        Some(t) => {
            // Clamp the target into range first (a stale line_number could exceed bounds).
            let t = t.clamp(1, note_line_count);
            // Center on t, then clamp start so the window fits within the note.
            let mut start = t.saturating_sub(height / 2);
            let max_start = note_line_count.saturating_sub(height) + 1;
            if start > max_start {
                start = max_start;
            }
            if start < 1 {
                start = 1;
            }
            (start, Some(t - start))
        }
    };
    let count = height.min(note_line_count - start + 1);
    (start, count, highlight)
}

/// Render the context pane into `area`: a bordered block titled with the note path,
/// showing a window of the note's content centered on `target_line` (or the top of the
/// note when the cursor is on a header), with a line-number gutter and the target line
/// highlighted. `scroll` shifts the window up/down from the auto-centered position.
/// Shows a graceful placeholder when content is unavailable.
fn draw_context_pane(
    frame: &mut Frame,
    area: Rect,
    note_path: Option<&str>,
    content: Option<&NoteContent>,
    target_line: Option<usize>,
    scroll: i32,
) {
    let title = Line::from(vec![
        Span::raw(" Context — "),
        Span::styled(
            note_path.unwrap_or("(no note selected)"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);

    let lines: Vec<Line> = match content {
        None => vec![Line::from(Span::styled(
            " Context not available for this note.",
            Style::new().add_modifier(Modifier::DIM),
        ))],
        Some(nc) => {
            let note_lines: Vec<&str> = nc.content.lines().collect();
            let line_count = note_lines.len();
            if line_count == 0 {
                vec![Line::from(Span::styled(
                    " (empty note)",
                    Style::new().add_modifier(Modifier::DIM),
                ))]
            } else {
                // Center on the target line (no scroll), then apply the manual scroll
                // offset and clamp the start so the window always fits within the note.
                let (centered, count, _) =
                    context_view(line_count, target_line, inner.height as usize);
                let rows = count;
                let max_start = line_count.saturating_sub(rows) + 1;
                let start = ((centered as i32) + scroll).clamp(1, max_start as i32) as usize;
                // The highlight sits at the target line if it is still in view after scroll.
                let highlight = target_line.and_then(|t| {
                    let t = t.clamp(1, line_count);
                    (t >= start && t < start + rows).then_some(t - start)
                });
                let num_width = line_count.to_string().len();
                (0..count)
                    .map(|i| {
                        let lineno = start + i;
                        let raw = note_lines[start - 1 + i];
                        let is_target = highlight == Some(i);
                        let style = if is_target {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        Line::from(vec![
                            Span::styled(
                                format!("{:>width$} ", lineno, width = num_width),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(if is_target { "▶ " } else { "  " }, style),
                            Span::styled(raw.to_string(), style),
                        ])
                    })
                    .collect()
            }
        }
    };

    frame.render_widget(Paragraph::new(lines).block(block), area);
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
            scheduled_date: None,
            tags: Vec::new(),
            priority: None,
            start_date: None,
            created_date: None,
            done_date: None,
            cancelled_date: None,
            updated_at: 1,
        }
    }

    fn task_with_due(id: i64, raw: &str, line: usize, note: &str, due: &str) -> Task {
        let mut t = task(id, raw, line, note);
        t.due_date = Some(due.to_string());
        t
    }

    /// Build a task with a scheduled date set (ADR-0009 Phase 1).
    fn task_with_scheduled(id: i64, raw: &str, line: usize, note: &str, sched: &str) -> Task {
        let mut t = task(id, raw, line, note);
        t.scheduled_date = Some(sched.to_string());
        t
    }

    /// Build a task with the given tags (for Tag-axis tests).
    fn task_with_tags(id: i64, raw: &str, line: usize, note: &str, tags: &[&str]) -> Task {
        let mut t = task(id, raw, line, note);
        t.tags = tags.iter().map(|s| s.to_string()).collect();
        t
    }

    /// Build a task with the given priority (for Priority-axis tests).
    fn task_with_priority(id: i64, raw: &str, line: usize, note: &str, p: Priority) -> Task {
        let mut t = task(id, raw, line, note);
        t.priority = Some(p);
        t
    }

    /// Unpack a header row for assertions.
    fn header(row: &DisplayRow) -> (&str, usize, usize, bool) {
        match row {
            DisplayRow::Header {
                group_key,
                open_count,
                total_count,
                collapsed,
            } => (group_key, *open_count, *total_count, *collapsed),
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
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Note,
        );
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
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Note,
        );
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
        let rows = build_view(
            &tasks,
            StatusFilter::Open,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Note,
        );
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
        let rows = build_view(
            &tasks,
            StatusFilter::Open,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Note,
        );
        // Only alpha has an open task; beta is hidden under the Open filter.
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "alpha.md");
    }

    /// The due-date column flows through to the task row data (rendered separately).
    #[test]
    fn build_view_preserves_due_date_on_task_row() {
        let tasks = vec![task_with_due(1, " ", 1, "alpha.md", "2026-07-01")];
        let expanded = HashSet::from(["alpha.md".to_string()]);
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Note,
        );
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

    /// M1 regression: `u` after `Space` must enqueue the *reversed* checkbox flip
    /// (expected/new swapped from the original toggle). Before the fix, `submit_undo`
    /// passed the chars through un-swapped, so the undo either silently no-op'd (the
    /// daemon's idempotency check short-circuited the identical flip when the original
    /// toggle succeeded) or actually re-applied the original flip (when it had failed)
    /// — in both cases the opposite of "undo". End-to-end through the real DB queue.
    #[test]
    fn submit_undo_after_toggle_enqueues_reversed_flip() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        // rows: [H alpha, T1(open)]; land on the open task.
        app.state.select(Some(1));

        // Space: enqueue the forward flip ([ ] -> [x]).
        app.submit_toggle(&conn);
        let forward = &db::pending_actions(&conn).unwrap()[0];
        assert_eq!(forward.expected_char, " ");
        assert_eq!(forward.new_char, "x");

        // u: undo must enqueue the *reverse* flip ([x] -> [ ]). The undo's
        // expected_char is the forward flip's new_char, and vice versa.
        app.submit_undo(&conn);

        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 2, "forward flip + undo flip both pending");
        let undo = &pending[1];
        assert_eq!(undo.task_id, 1);
        assert_eq!(
            undo.expected_char, "x",
            "undo expected_char must be the forward flip's new_char (swapped)"
        );
        assert_eq!(
            undo.new_char, " ",
            "undo new_char must be the forward flip's expected_char (swapped)"
        );
    }

    /// `t` (ADR-0009 Phase 2) marks the cursor task for today when it isn't
    /// scheduled today, clears the `⏳` when it already is, and never fires on a
    /// header. End-to-end through the real DB queue.
    #[test]
    fn submit_set_scheduled_marks_and_unmarks() {
        let conn = db::open(":memory:").unwrap();
        let mut app = App::new();
        let today = app.today.clone();
        // T1: not scheduled -> `t` should mark for today.
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();
        // T2: already scheduled today -> `t` should clear.
        db::upsert_task(&conn, &task_with_scheduled(2, " ", 2, "alpha.md", &today)).unwrap();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        // rows: [H alpha, T1, T2]

        // Header -> no enqueue.
        app.state.select(Some(0));
        app.submit_set_scheduled(&conn);
        assert!(
            db::pending_actions(&conn).unwrap().is_empty(),
            "header must not mark"
        );

        // T1 (not scheduled) -> enqueue mark = Some(today).
        app.state.select(Some(1));
        app.submit_set_scheduled(&conn);
        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].action_type, "set_scheduled");
        assert_eq!(pending[0].task_id, 1);
        assert_eq!(pending[0].payload.as_deref(), Some(today.as_str()));

        // T2 (already scheduled today) -> enqueue clear = None.
        app.state.select(Some(2));
        app.submit_set_scheduled(&conn);
        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 2);
        let clear = pending.iter().find(|a| a.task_id == 2).unwrap();
        assert_eq!(clear.action_type, "set_scheduled");
        assert!(clear.payload.is_none(), "already-today -> clear (None)");
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
            group_key: "a.md".to_string(),
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
                group_key: "a.md".to_string(),
                open_count: 2,
                total_count: 2,
                collapsed: false,
            },
            DisplayRow::Task {
                task: Box::new(task(10, " ", 1, "a.md")),
            },
            DisplayRow::Task {
                task: Box::new(task(11, " ", 2, "a.md")),
            },
        ];
        let mut state = ListState::default();
        state.select(Some(2)); // on task id 11

        // A rebuild where task 11 moved up to index 1 (e.g. task 10 deleted).
        let rows2 = vec![
            DisplayRow::Header {
                group_key: "a.md".to_string(),
                open_count: 1,
                total_count: 1,
                collapsed: false,
            },
            DisplayRow::Task {
                task: Box::new(task(11, " ", 2, "a.md")),
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
        let returned_id = enqueue_toggle(&conn, &t).unwrap();

        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        let p = &pending[0];
        // The returned id must be the row's id — the session-tracking feature relies
        // on this contract to follow an action's resolution across refreshes.
        assert_eq!(returned_id, p.id);
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
        let returned_id2 = enqueue_toggle(&conn, &done_task).unwrap();
        let p2 = &db::pending_actions(&conn).unwrap()[0];
        assert_eq!(returned_id2, p2.id);
        assert_eq!(p2.expected_char, "x");
        assert_eq!(p2.new_char, " ");
    }

    /// The failure-notice mapping translates each daemon `ApplyOutcome` phrase into
    /// plain wording, and falls back to a trimmed copy of anything unknown.
    #[test]
    fn friendly_failure_reason_maps_each_category() {
        assert_eq!(
            friendly_failure_reason("note changed externally since scan; action not applied"),
            "this note changed in Obsidian",
        );
        assert_eq!(
            friendly_failure_reason("task no longer in index (or note gone); action not applied"),
            "this task is no longer in the note",
        );
        assert_eq!(
            friendly_failure_reason(
                "checkbox line no longer matches expected bytes; action not applied"
            ),
            "the checkbox line changed",
        );
        assert_eq!(
            friendly_failure_reason("invalid new_char; action not applied"),
            "the request was not valid",
        );
        // ADR-0009 Phase 2: the set_scheduled unparseable phrase.
        assert_eq!(
            friendly_failure_reason(
                "scheduled date is malformed or unparseable; action not applied"
            ),
            "the scheduled date on this line couldn't be parsed",
        );
        // Unknown -> trimmed copy.
        assert_eq!(
            friendly_failure_reason("  something unusual happened  "),
            "something unusual happened"
        );
        // Empty -> generic.
        assert_eq!(friendly_failure_reason(""), "it could not be applied");
        assert_eq!(friendly_failure_reason("   "), "it could not be applied");
    }

    /// A refused action surfaces a notice naming the note and the plain reason.
    #[test]
    fn failed_action_surfaces_notice_on_refresh() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1)); // on the task
        app.submit_toggle(&conn);
        // Sanity: exactly one action was enqueued and is tracked.
        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(app.pending_session_actions, vec![pending[0].id]);
        assert!(app.notice.is_none(), "no notice before resolution");

        // Daemon refuses it: note changed externally.
        db::resolve_action(
            &conn,
            pending[0].id,
            "failed",
            Some("note changed externally since scan; action not applied"),
        )
        .unwrap();
        app.refresh(&conn).unwrap();

        let notice = app
            .notice
            .as_deref()
            .expect("failure should surface a notice");
        assert!(notice.contains("alpha.md"), "notice should name the note");
        assert!(
            notice.contains("this note changed in Obsidian"),
            "notice should carry the plain reason"
        );
        assert!(notice.contains("Space"), "notice should hint the retry key");
        // The resolved id is no longer tracked.
        assert!(
            app.pending_session_actions.is_empty(),
            "resolved id should drop from tracking"
        );
    }

    /// ADR-0009 Phase 2: a refused `set_scheduled` action surfaces a notice worded
    /// for the mark gesture ("Mark not applied") that hints the `t` retry key (not
    /// `Space`), carrying the plain unparseable reason and the note name.
    #[test]
    fn failed_set_scheduled_surfaces_mark_notice() {
        let conn = db::open(":memory:").unwrap();
        let mut app = App::new();
        let today = app.today.clone();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1)); // on the task
        app.submit_set_scheduled(&conn);
        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].action_type, "set_scheduled");
        assert_eq!(pending[0].payload.as_deref(), Some(today.as_str()));

        // Daemon refuses it: the line's scheduled date is malformed/unparseable.
        db::resolve_action(
            &conn,
            pending[0].id,
            "failed",
            Some("scheduled date is malformed or unparseable; action not applied"),
        )
        .unwrap();
        app.refresh(&conn).unwrap();

        let notice = app
            .notice
            .as_deref()
            .expect("set_scheduled failure should surface a notice");
        assert!(
            notice.starts_with("Mark not applied"),
            "set_scheduled notice should be worded for the mark gesture: {notice}"
        );
        assert!(notice.contains("alpha.md"), "notice should name the note");
        assert!(
            notice.contains("couldn't be parsed"),
            "notice should carry the plain reason"
        );
        assert!(
            notice.contains("Press t"),
            "notice should hint the `t` retry key, not Space: {notice}"
        );
        assert!(
            !notice.contains("Space"),
            "set_scheduled notice must NOT hint the checkbox retry key: {notice}"
        );
    }

    /// A successfully-applied action surfaces NO notice (the flip is already visible
    /// on the refresh); its id drops from tracking.
    #[test]
    fn done_action_surfaces_no_notice() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1));
        app.submit_toggle(&conn);
        let id = db::pending_actions(&conn).unwrap()[0].id;

        db::resolve_action(&conn, id, "done", None).unwrap();
        app.refresh(&conn).unwrap();

        assert!(
            app.notice.is_none(),
            "a successful flip must not show a notice"
        );
        assert!(
            app.pending_session_actions.is_empty(),
            "done id should drop from tracking"
        );
    }

    /// A still-pending action surfaces nothing and stays tracked.
    #[test]
    fn pending_action_surfaces_no_notice_and_stays_tracked() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1));
        app.submit_toggle(&conn);
        let id = db::pending_actions(&conn).unwrap()[0].id;

        // No resolution yet.
        app.refresh(&conn).unwrap();

        assert!(
            app.notice.is_none(),
            "pending action must not show a notice"
        );
        assert_eq!(
            app.pending_session_actions,
            vec![id],
            "pending id should stay tracked"
        );
    }

    /// Enqueueing a new action clears any existing notice (the "try again / move on"
    /// gesture) and tracks the new id.
    #[test]
    fn notice_clears_on_next_enqueue() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();
        db::upsert_task(&conn, &task(2, " ", 2, "alpha.md")).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();

        // First toggle: refused -> notice shown.
        app.state.select(Some(1));
        app.submit_toggle(&conn);
        let id1 = db::pending_actions(&conn).unwrap()[0].id;
        db::resolve_action(
            &conn,
            id1,
            "failed",
            Some("note changed externally since scan"),
        )
        .unwrap();
        app.refresh(&conn).unwrap();
        assert!(app.notice.is_some(), "notice should be set after a refusal");

        // Second toggle: clears the notice immediately, tracks the new id.
        app.state.select(Some(2));
        app.submit_toggle(&conn);
        assert!(
            app.notice.is_none(),
            "new enqueue should clear the prior notice"
        );
        // id1 was resolved `failed` so only the new id2 is still pending.
        let id2 = db::pending_actions(&conn).unwrap()[0].id;
        assert_eq!(
            app.pending_session_actions,
            vec![id2],
            "only the new id should be tracked (resolved id1 dropped on prior refresh)"
        );
    }

    /// With no session actions, a refresh surfaces nothing.
    #[test]
    fn no_session_action_means_no_notice() {
        let conn = db::open(":memory:").unwrap();
        // A failed action exists in the DB but was NOT enqueued this session.
        let id = db::enqueue_action(&conn, 5, "other.md", 1, " ", "x").unwrap();
        db::resolve_action(
            &conn,
            id,
            "failed",
            Some("note changed externally since scan"),
        )
        .unwrap();

        let mut app = App::new();
        app.refresh(&conn).unwrap();
        assert!(
            app.notice.is_none(),
            "failures from outside this session must not surface"
        );
        assert!(app.pending_session_actions.is_empty());
    }

    /// `render_failure_notice` produces a single-line message with reason + note.
    #[test]
    fn render_failure_notice_includes_reason_and_note() {
        let conn = db::open(":memory:").unwrap();
        let id = db::enqueue_action(&conn, 1, "Daily.md", 3, " ", "x").unwrap();
        db::resolve_action(
            &conn,
            id,
            "failed",
            Some("note changed externally since scan; action not applied"),
        )
        .unwrap();
        let action = db::recent_actions(&conn, 8)
            .unwrap()
            .into_iter()
            .find(|a| a.id == id)
            .unwrap();
        let msg = render_failure_notice(&action);
        assert!(msg.contains("Daily.md"));
        assert!(msg.contains("this note changed in Obsidian"));
        assert!(msg.contains("Space"));
    }

    /// `context_view` centers the target within the window and clamps it to the note.
    #[test]
    fn context_view_centers_and_clamps() {
        // Plenty of room both sides: target 10 in a 20-line note, 5 rows -> centered.
        let (start, count, hl) = context_view(20, Some(10), 5);
        assert_eq!(start, 8, "window = [8..12], target centered at offset 2");
        assert_eq!(count, 5);
        assert_eq!(hl, Some(2));

        // Target near the top: clamp start to 1.
        let (start, count, hl) = context_view(20, Some(2), 5);
        assert_eq!(start, 1);
        assert_eq!(count, 5);
        assert_eq!(hl, Some(1));

        // Target near the bottom: clamp so the window fits.
        let (start, count, hl) = context_view(20, Some(19), 5);
        assert_eq!(start, 16, "max_start = 20 - 5 + 1 = 16");
        assert_eq!(count, 5);
        assert_eq!(hl, Some(3));

        // Note shorter than the pane: show the whole note, highlight preserved.
        let (start, count, hl) = context_view(3, Some(2), 10);
        assert_eq!(start, 1);
        assert_eq!(count, 3);
        assert_eq!(hl, Some(1));

        // An out-of-range target (stale line_number) is clamped into range.
        let (start, count, hl) = context_view(5, Some(99), 3);
        assert_eq!(start, 3, "max_start = 5 - 3 + 1 = 3");
        assert_eq!(count, 3);
        assert_eq!(hl, Some(2), "clamped target 5 - start 3 = offset 2");
    }

    /// `context_view` with no target (a group header) starts at the top, no highlight.
    #[test]
    fn context_view_no_target_starts_at_top() {
        let (start, count, hl) = context_view(20, None, 5);
        assert_eq!(start, 1);
        assert_eq!(count, 5);
        assert_eq!(hl, None);
    }

    /// `context_view` degenerates safely on an empty note or zero-height pane.
    #[test]
    fn context_view_empty_degenerates() {
        assert_eq!(context_view(0, Some(1), 5), (1, 0, None));
        assert_eq!(context_view(5, Some(1), 0), (1, 0, None));
    }

    /// `sync_context` loads the selected note's cached content and `refresh` keeps it
    /// live (force re-read). The TUI reads only from the index, never the vault.
    #[test]
    fn sync_context_loads_selected_note_content() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 3, "alpha.md")).unwrap();
        db::upsert_note_content(
            &conn,
            "alpha.md",
            "line1\nline2\n- [ ] task\nline4",
            Some("h"),
        )
        .unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        // Default: group collapsed -> cursor on the alpha.md header (no target line).
        assert_eq!(app.selected_note_path().as_deref(), Some("alpha.md"));
        assert_eq!(app.ctx_note_path.as_deref(), Some("alpha.md"));
        let nc = app
            .ctx_content
            .as_ref()
            .expect("content should be cached for the selected note");
        assert_eq!(nc.content, "line1\nline2\n- [ ] task\nline4");

        // Expand the group and land on the task -> window centers on line 3.
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1)); // the task row
        app.sync_context(&conn, false); // same note -> no refetch needed, still cached
        assert_eq!(app.selected_task().map(|t| t.line_number), Some(3));
        let (start, count, hl) = context_view(4, Some(3), 10);
        assert_eq!((start, count, hl), (1, 4, Some(2)));

        // An edit to the note's content lands on the next refresh (force re-read).
        db::upsert_note_content(&conn, "alpha.md", "rewritten\n- [ ] task", Some("h2")).unwrap();
        app.refresh(&conn).unwrap();
        assert_eq!(
            app.ctx_content.as_ref().unwrap().content,
            "rewritten\n- [ ] task"
        );
    }

    /// `sync_context` swaps content when the selection moves to a different note, and
    /// clears the pane when the view empties.
    #[test]
    fn sync_context_swaps_and_clears_with_selection() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();
        db::upsert_task(&conn, &task(2, " ", 1, "beta.md")).unwrap();
        db::upsert_note_content(&conn, "alpha.md", "alpha body", None).unwrap();
        db::upsert_note_content(&conn, "beta.md", "beta body", None).unwrap();

        let mut app = App::new();
        app.refresh(&conn).unwrap();
        // Cursor on the alpha header.
        assert_eq!(app.ctx_note_path.as_deref(), Some("alpha.md"));
        assert_eq!(app.ctx_content.as_ref().unwrap().content, "alpha body");

        // Move to the beta header -> content swaps on sync(force=false) (note changed).
        app.move_selection(1);
        app.sync_context(&conn, false);
        assert_eq!(app.ctx_note_path.as_deref(), Some("beta.md"));
        assert_eq!(app.ctx_content.as_ref().unwrap().content, "beta body");

        // Delete all tasks -> view empties -> pane clears.
        db::delete_tasks_for_note(&conn, "alpha.md").unwrap();
        db::delete_tasks_for_note(&conn, "beta.md").unwrap();
        app.refresh(&conn).unwrap();
        assert!(app.ctx_note_path.is_none());
        assert!(app.ctx_content.is_none());
    }

    /// A note with no cached content (e.g. a note the daemon hasn't cached) shows a
    /// placeholder, not a crash: `sync_context` records the note_path with `None`.
    #[test]
    fn sync_context_handles_uncached_note() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();
        // Deliberately no note_contents row for alpha.md.
        let mut app = App::new();
        app.refresh(&conn).unwrap();
        assert_eq!(app.ctx_note_path.as_deref(), Some("alpha.md"));
        assert!(
            app.ctx_content.is_none(),
            "uncached note should yield None content, not an error"
        );
    }

    /// Headless render smoke (TestBackend): `draw` with the split-pane layout must not
    /// panic, and the context pane must actually render its title and the selected
    /// task's line text. Exercises the full draw path (incl. `draw_context_pane`)
    /// without a real terminal.
    #[test]
    fn draw_renders_context_pane_without_panic() {
        use ratatui::backend::TestBackend;

        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 3, "alpha.md")).unwrap();
        db::upsert_note_content(
            &conn,
            "alpha.md",
            "# Alpha\nsome lead-in\n- [ ] the actual task text here\nfollow-up\n",
            Some("h"),
        )
        .unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1)); // on the task row
        app.sync_context(&conn, false);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        // Must not panic.
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let rendered: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        assert!(rendered.contains("Context"), "pane title should render");
        assert!(
            rendered.contains("the actual task text here"),
            "the task line should appear in the context pane"
        );
    }

    /// `scroll_context` adjusts the pane offset; any task navigation recenters it to 0,
    /// and the offset saturates at the i32 bounds (no overflow panic).
    #[test]
    fn scroll_context_adjusts_and_resets_on_navigation() {
        let mut app = App::new();
        app.tasks = vec![task(1, " ", 1, "alpha.md"), task(2, " ", 2, "alpha.md")];
        app.expanded.insert("alpha.md".to_string());
        app.filter = StatusFilter::All;
        app.rebuild();
        app.state.select(Some(1)); // on task 2

        app.scroll_context(3);
        assert_eq!(app.ctx_scroll, 3);
        app.scroll_context(-1);
        assert_eq!(app.ctx_scroll, 2);

        // Saturation at the bounds (no overflow panic).
        app.ctx_scroll = i32::MAX;
        app.scroll_context(1);
        assert_eq!(app.ctx_scroll, i32::MAX);
        app.ctx_scroll = i32::MIN;
        app.scroll_context(-1);
        assert_eq!(app.ctx_scroll, i32::MIN);

        // Any task navigation recenters the pane.
        for reset_in in ["move", "filter", "collapse", "expand"] {
            app.ctx_scroll = 5;
            match reset_in {
                "move" => app.move_selection(-1),
                "filter" => app.cycle_filter(),
                "collapse" => app.collapse_all(),
                "expand" => app.expand_all(),
                _ => unreachable!(),
            }
            assert_eq!(app.ctx_scroll, 0, "{reset_in} should recenter the pane");
        }
    }

    /// `toggle_pane` flips pane visibility.
    #[test]
    fn toggle_pane_flips_visibility() {
        let mut app = App::new();
        assert!(app.pane_visible, "pane is visible by default");
        app.toggle_pane();
        assert!(!app.pane_visible);
        app.toggle_pane();
        assert!(app.pane_visible);
    }

    /// A manual scroll survives an index refresh of the same note (so the pane isn't
    /// yanked back ~750ms later), but moving to another note recenters it.
    #[test]
    fn scroll_survives_refresh_but_resets_on_navigation() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();
        db::upsert_task(&conn, &task(2, " ", 1, "beta.md")).unwrap();
        db::upsert_note_content(&conn, "alpha.md", "a", None).unwrap();
        db::upsert_note_content(&conn, "beta.md", "b", None).unwrap();

        let mut app = App::new();
        app.refresh(&conn).unwrap(); // cursor on the alpha header
        app.ctx_scroll = 7;
        // Refresh the SAME note (force) -> scroll must survive.
        app.refresh(&conn).unwrap();
        assert_eq!(app.ctx_scroll, 7, "refresh must not wipe a manual scroll");

        // Move to the beta header -> recenters.
        app.move_selection(1);
        assert_eq!(app.ctx_scroll, 0, "moving to another note recenters");
    }

    /// With the pane toggled off (`p`), `draw` renders a full-width list and no pane —
    /// the "Context" title is absent while the task row is still shown.
    #[test]
    fn draw_hides_pane_when_toggled_off() {
        use ratatui::backend::TestBackend;

        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 3, "alpha.md")).unwrap();
        db::upsert_note_content(
            &conn,
            "alpha.md",
            "# Alpha\n- [ ] do the thing\n",
            Some("h"),
        )
        .unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1));
        app.sync_context(&conn, false);
        app.toggle_pane(); // hide the pane

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            !rendered.contains("Context"),
            "pane title must not render when toggled off"
        );
        assert!(rendered.contains("task 1"), "the list still shows the task");
    }

    /// Below [`MIN_SPLIT_WIDTH`] the pane auto-hides even when `pane_visible` is true,
    /// so neither pane is unreadably narrow.
    #[test]
    fn draw_hides_pane_below_min_width() {
        use ratatui::backend::TestBackend;

        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();
        db::upsert_note_content(&conn, "alpha.md", "- [ ] short task\n", None).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1));
        app.sync_context(&conn, false);
        assert!(app.pane_visible); // still visible in principle...

        // ...but width 40 < MIN_SPLIT_WIDTH(60) -> the pane is auto-hidden.
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            !rendered.contains("Context"),
            "pane must auto-hide below MIN_SPLIT_WIDTH"
        );
    }

    // --- ADR-0009 Phase 1: "Today" view (scheduled_date == today) ----------

    /// With `today_only` on, `build_view` keeps only tasks whose `scheduled_date`
    /// matches `today`, across notes; tasks with other/None scheduled dates drop out.
    #[test]
    fn today_only_keeps_only_scheduled_today_tasks() {
        let tasks = vec![
            task_with_scheduled(1, " ", 1, "alpha.md", "2026-06-20"), // today
            task_with_scheduled(2, " ", 1, "beta.md", "2026-06-21"),  // not today
            task(3, " ", 1, "gamma.md"),                              // no scheduled date
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            true,
            "2026-06-20",
            "",
            "",
            false,
            GroupBy::Note,
        );
        // Only alpha.md has a today-matching task; one collapsed header.
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "alpha.md");
    }

    /// `today_only` composes with the status filter (orthogonal axes): today_only +
    /// Done still hides a done today-task under the Open filter. Expanding shows the
    /// right task row.
    #[test]
    fn today_only_composes_with_status_filter() {
        let tasks = vec![
            task_with_scheduled(1, " ", 1, "alpha.md", "2026-06-20"), // today, open
            task_with_scheduled(2, "x", 2, "alpha.md", "2026-06-20"), // today, done
        ];
        let expanded = HashSet::from(["alpha.md".to_string()]);
        // Open + today -> only the open today task.
        let rows = build_view(
            &tasks,
            StatusFilter::Open,
            &expanded,
            true,
            "2026-06-20",
            "",
            "",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 2, "header + one task");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
        // All + today -> both today tasks.
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            true,
            "2026-06-20",
            "",
            "",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 3, "header + two tasks");
    }

    /// Toggling `today_only` off shows all tasks again (the filter is a pure
    /// function of app state; `toggle_today` flips the flag and rebuilds).
    #[test]
    fn toggle_today_off_shows_all_tasks() {
        let mut app = App::new();
        app.today = "2026-06-20".to_string();
        app.filter = StatusFilter::All;
        app.tasks = vec![
            task_with_scheduled(1, " ", 1, "alpha.md", "2026-06-20"),
            task(2, " ", 1, "beta.md"), // no scheduled date
        ];
        app.expanded.insert("alpha.md".to_string());
        app.expanded.insert("beta.md".to_string());
        app.rebuild();
        // today_only defaults to off -> both notes visible.
        assert!(!app.today_only);
        let off_count = app.rows.len();

        // Turn it on -> only alpha (today's task).
        app.toggle_today();
        assert!(app.today_only);
        let today_rows: Vec<_> = app
            .rows
            .iter()
            .filter(|r| matches!(r, DisplayRow::Task { .. }))
            .collect();
        assert_eq!(today_rows.len(), 1);
        assert!(matches!(&today_rows[0], DisplayRow::Task { task } if task.id == 1));

        // Turn it back off -> back to the full set.
        app.toggle_today();
        assert!(!app.today_only);
        assert_eq!(app.rows.len(), off_count);
    }

    /// A Today-task group with no matching status is hidden entirely (no empty
    /// header), mirroring the status-filter behaviour.
    #[test]
    fn today_only_hides_group_with_no_today_task() {
        let tasks = vec![
            task_with_scheduled(1, " ", 1, "alpha.md", "2026-06-20"), // today
            task(2, " ", 1, "beta.md"),                               // no scheduled date
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            true,
            "2026-06-20",
            "",
            "",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "alpha.md");
    }

    /// `row_to_item` renders the `⏳ <date>` suffix, and bolds it when the date is
    /// today. Headless smoke against the buffer symbols.
    #[test]
    fn row_to_item_renders_scheduled_suffix_bold_for_today() {
        use ratatui::backend::TestBackend;
        let conn = db::open(":memory:").unwrap();
        let today_today = task_with_scheduled(1, " ", 1, "alpha.md", "2026-06-20");
        let other_day = task_with_scheduled(2, " ", 2, "alpha.md", "2026-06-21");
        db::upsert_task(&conn, &today_today).unwrap();
        db::upsert_task(&conn, &other_day).unwrap();

        let mut app = App::new();
        app.today = "2026-06-20".to_string();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(rendered.contains('⏳'), "scheduled glyph must render");
        assert!(
            rendered.contains("2026-06-20"),
            "today's scheduled date must render"
        );
        assert!(
            rendered.contains("2026-06-21"),
            "the other scheduled date must render"
        );
    }

    // --- Overdue filter (`O`): due_date < today ---------------------------

    /// With `overdue_only` on, `build_view` keeps only tasks whose `due_date`
    /// is set and strictly before `today`. Tasks due today, due in the future,
    /// or with no due date are excluded.
    #[test]
    fn overdue_only_keeps_past_due_tasks() {
        let tasks = vec![
            task_with_due(1, " ", 1, "alpha.md", "2026-06-18"), // past due
            task_with_due(2, " ", 1, "beta.md", "2026-06-20"),  // today (not overdue)
            task_with_due(3, " ", 1, "gamma.md", "2026-06-22"), // future
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "2026-06-20",
            "",
            "",
            true,
            GroupBy::Note,
        );
        // Only alpha.md (past-due) survives; one collapsed header.
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "alpha.md");
    }

    /// Tasks with no due date, due today, or due in the future are all excluded
    /// when `overdue_only` is on.
    #[test]
    fn overdue_only_excludes_future_today_and_no_due() {
        let tasks = vec![
            task_with_due(1, " ", 1, "alpha.md", "2026-06-20"), // today
            task_with_due(2, " ", 1, "beta.md", "2026-06-22"),  // future
            task(3, " ", 1, "gamma.md"),                        // no due date
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "2026-06-20",
            "",
            "",
            true,
            GroupBy::Note,
        );
        assert!(rows.is_empty(), "none are past-due");
    }

    /// With `overdue_only` off, no date-based filtering happens (beyond the
    /// existing axes) — all tasks are visible.
    #[test]
    fn overdue_only_off_shows_all() {
        let tasks = vec![
            task_with_due(1, " ", 1, "alpha.md", "2026-06-18"),
            task_with_due(2, " ", 1, "beta.md", "2026-06-22"),
            task(3, " ", 1, "gamma.md"),
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "2026-06-20",
            "",
            "",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 3, "all three note headers visible");
    }

    /// `overdue_only` composes with the status filter (orthogonal axes):
    /// overdue + Open shows only open past-due; overdue + Done shows done
    /// past-due (the "completed-was-overdue" review).
    #[test]
    fn overdue_only_composes_with_status_filter() {
        let tasks = vec![
            task_with_due(1, " ", 1, "alpha.md", "2026-06-18"), // past due, open
            task_with_due(2, "x", 2, "alpha.md", "2026-06-18"), // past due, done
        ];
        let expanded = HashSet::from(["alpha.md".to_string()]);

        // overdue + Open -> only the open past-due task.
        let rows = build_view(
            &tasks,
            StatusFilter::Open,
            &expanded,
            false,
            "2026-06-20",
            "",
            "",
            true,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 2, "header + one open past-due task");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));

        // overdue + Done -> only the done past-due task.
        let rows = build_view(
            &tasks,
            StatusFilter::Done,
            &expanded,
            false,
            "2026-06-20",
            "",
            "",
            true,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 2, "header + one done past-due task");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 2));
    }

    /// `overdue_only` is orthogonal to `today_only`: both on → tasks that are
    /// BOTH due < today AND scheduled == today (the AND composes).
    #[test]
    fn overdue_only_orthogonal_to_today_filter() {
        // A task that has BOTH a past due_date AND today's scheduled_date —
        // it passes both filters.
        let mut t = task_with_due(1, " ", 1, "alpha.md", "2026-06-18");
        t.scheduled_date = Some("2026-06-20".to_string());
        let tasks = vec![
            t,                                                        // both
            task_with_due(2, " ", 2, "alpha.md", "2026-06-18"), // past due, not scheduled today
            task_with_scheduled(3, " ", 3, "alpha.md", "2026-06-20"), // scheduled today, no past due
        ];
        let expanded = HashSet::from(["alpha.md".to_string()]);

        // Both filters on -> only task 1 (past due AND scheduled today).
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            true,
            "2026-06-20",
            "",
            "",
            true,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 2, "header + one task matching both filters");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
    }

    /// `toggle_overdue` flips the flag and rebuilds the view.
    #[test]
    fn toggle_overdue_flips_state_and_rebuilds() {
        let mut app = App::new();
        app.today = "2026-06-20".to_string();
        app.filter = StatusFilter::All;
        app.tasks = vec![
            task_with_due(1, " ", 1, "alpha.md", "2026-06-18"), // past due
            task_with_due(2, " ", 1, "beta.md", "2026-06-22"),  // future
        ];
        app.expanded.insert("alpha.md".to_string());
        app.expanded.insert("beta.md".to_string());
        app.rebuild();
        assert!(!app.overdue_only);
        let off_count = app.rows.len();

        // Turn it on -> only alpha (past-due).
        app.toggle_overdue();
        assert!(app.overdue_only);
        let overdue_rows: Vec<_> = app
            .rows
            .iter()
            .filter(|r| matches!(r, DisplayRow::Task { .. }))
            .collect();
        assert_eq!(overdue_rows.len(), 1);
        assert!(matches!(
            &overdue_rows[0],
            DisplayRow::Task { task } if task.id == 1
        ));

        // Turn it back off -> full set restored.
        app.toggle_overdue();
        assert!(!app.overdue_only);
        assert_eq!(app.rows.len(), off_count);
    }

    // --- ADR-0010: text search tests --------------------------------------------

    /// Search filters tasks whose text contains the query (case-insensitive,
    /// substring). Non-matching tasks are hidden; matching tasks survive.
    #[test]
    fn build_view_search_filters_by_text_substring() {
        // Give each task a distinct text (the test helper uses "task {id}").
        let with_text = |id: i64, text: &str| -> Task {
            let mut t = task(id, " ", id as usize, "alpha.md");
            t.text = text.to_string();
            t.text_hash = text.to_string();
            t
        };
        let tasks = vec![
            with_text(1, "deploy the database migration"),
            with_text(2, "write documentation for the API"),
            with_text(3, "review deployment checklist"),
        ];
        let expanded = HashSet::from(["alpha.md".to_string()]);

        // Search "deploy" should match tasks 1 and 3 (case-insensitive substring).
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "deploy",
            "",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 3, "header + two matching tasks");
        let task_ids: Vec<i64> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Task { task } => Some(task.id),
                _ => None,
            })
            .collect();
        assert_eq!(task_ids, vec![1, 3], "tasks 1 and 3 should match 'deploy'");
    }

    /// Search is case-insensitive: uppercase query matches lowercase text.
    #[test]
    fn build_view_search_case_insensitive() {
        let with_text = |id: i64, text: &str| -> Task {
            let mut t = task(id, " ", id as usize, "alpha.md");
            t.text = text.to_string();
            t.text_hash = text.to_string();
            t
        };
        let tasks = vec![
            with_text(1, "Deploy the database"),
            with_text(2, "write documentation"),
        ];
        let expanded = HashSet::from(["alpha.md".to_string()]);

        // Uppercase query "DEPLOY" should match lowercase "Deploy".
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "DEPLOY",
            "",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 2, "header + one matching task");
    }

    /// File search (`F` key) matches `note_path`: a query matching a filename
    /// shows all tasks from that file, even when the task text doesn't contain
    /// the query.
    #[test]
    fn build_view_file_search_matches_note_path() {
        let tasks = vec![
            task(1, " ", 1, "deployment-notes.md"),
            task(2, " ", 2, "daily.md"),
        ];
        let expanded = HashSet::new();
        // File search "deploy" via file_query — should match all tasks in
        // deployment-notes.md even though the task text is "task 1" / "task 2".
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "deploy",
            false,
            GroupBy::Note,
        );
        assert_eq!(
            rows.len(),
            1,
            "only the deployment-notes.md group should match"
        );
        assert_eq!(
            header(&rows[0]).0,
            "deployment-notes.md",
            "should match by note_path"
        );
    }

    /// Empty search query shows all tasks (no filtering).
    #[test]
    fn build_view_empty_search_shows_all() {
        let tasks = vec![task(1, " ", 1, "alpha.md"), task(2, " ", 2, "alpha.md")];
        let expanded = HashSet::from(["alpha.md".to_string()]);
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 3, "header + two tasks — no filtering");
    }

    /// Search composes with the status filter: search + Open shows only open
    /// tasks that match the query.
    #[test]
    fn build_view_search_composes_with_status_filter() {
        let with_text = |id: i64, text: &str, raw: &str| -> Task {
            let mut t = task(id, raw, id as usize, "alpha.md");
            t.text = text.to_string();
            t.text_hash = text.to_string();
            t
        };
        let tasks = vec![
            with_text(1, "deploy the migration", " "),     // open
            with_text(2, "deploy the rollback plan", "x"), // done
        ];
        let expanded = HashSet::from(["alpha.md".to_string()]);

        // Search "deploy" + Open filter: only task 1 (open) survives.
        let rows = build_view(
            &tasks,
            StatusFilter::Open,
            &expanded,
            false,
            "",
            "deploy",
            "",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 2, "header + one task (open)");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
    }

    /// Search composes with the Today view: search + today_only shows only
    /// today's tasks that match the query.
    #[test]
    fn build_view_search_composes_with_today_filter() {
        let with_text = |id: i64, text: &str, sched: Option<&str>| -> Task {
            let mut t = task(id, " ", id as usize, "alpha.md");
            t.text = text.to_string();
            t.text_hash = text.to_string();
            t.scheduled_date = sched.map(|s| s.to_string());
            t
        };
        let tasks = vec![
            with_text(1, "deploy the migration", Some("2026-06-20")), // today, matches
            with_text(2, "write documentation", Some("2026-06-20")),  // today, no match
            with_text(3, "review deploy checklist", Some("2026-06-21")), // not today
        ];
        let expanded = HashSet::from(["alpha.md".to_string()]);

        // Search "deploy" + today_only (2026-06-20): only task 1 survives.
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            true,
            "2026-06-20",
            "deploy",
            "",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 2, "header + one task (today + search)");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
    }

    /// `App::clear_search` clears the query and restores the full unfiltered list.
    #[test]
    fn clear_search_restores_full_list() {
        let mut app = App::new();
        // Populate with two notes, expanded.
        let with_text = |id: i64, text: &str| -> Task {
            let mut t = task(id, " ", id as usize, "alpha.md");
            t.text = text.to_string();
            t.text_hash = text.to_string();
            t
        };
        app.tasks = vec![
            with_text(1, "deploy the migration"),
            with_text(2, "write documentation"),
        ];
        app.filter = StatusFilter::All;
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        assert_eq!(app.rows.len(), 3, "full list: header + two tasks");

        // Search for "deploy".
        app.search_query = "deploy".to_string();
        app.rebuild();
        assert_eq!(app.rows.len(), 2, "filtered: header + one task");

        // Clear search.
        app.clear_search();
        assert!(!app.searching);
        assert!(app.search_query.is_empty());
        assert_eq!(app.rows.len(), 3, "full list restored after clear");
    }

    /// `App::clear_file_search` clears the file query and restores the full
    /// unfiltered list without affecting the text search.
    #[test]
    fn clear_file_search_restores_full_list() {
        let mut app = App::new();
        let with_text = |id: i64, text: &str, path: &str| -> Task {
            let mut t = task(id, " ", id as usize, path);
            t.text = text.to_string();
            t.text_hash = text.to_string();
            t
        };
        app.tasks = vec![
            with_text(1, "deploy the migration", "alpha.md"),
            with_text(2, "write documentation", "beta.md"),
        ];
        app.filter = StatusFilter::All;
        app.expanded.insert("alpha.md".to_string());
        app.expanded.insert("beta.md".to_string());
        app.rebuild();
        assert_eq!(app.rows.len(), 4, "full list: two headers + two tasks");

        // File search for "alpha".
        app.file_query = "alpha".to_string();
        app.rebuild();
        assert_eq!(app.rows.len(), 2, "filtered: alpha header + its task");

        // Clear file search.
        app.clear_file_search();
        assert!(!app.file_searching);
        assert!(app.file_query.is_empty());
        assert_eq!(app.rows.len(), 4, "full list restored after clear");
    }

    /// File search is case-insensitive: uppercase query matches lowercase path.
    #[test]
    fn build_view_file_search_case_insensitive() {
        let tasks = vec![
            task(1, " ", 1, "Deployment-Notes.md"),
            task(2, " ", 2, "daily.md"),
        ];
        let expanded = HashSet::new();
        // Uppercase file query "DEPLOYMENT" should match lowercase "deployment-notes".
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "DEPLOYMENT",
            false,
            GroupBy::Note,
        );
        assert_eq!(
            rows.len(),
            1,
            "only the Deployment-Notes.md group should match"
        );
    }

    /// File search narrows within a text search: both filters are AND-ed.
    #[test]
    fn build_view_file_search_narrows_within_text_search() {
        let with_text = |id: i64, text: &str, path: &str| -> Task {
            let mut t = task(id, " ", id as usize, path);
            t.text = text.to_string();
            t.text_hash = text.to_string();
            t
        };
        let tasks = vec![
            with_text(1, "common task", "alpha.md"),
            with_text(2, "common task", "beta.md"),
        ];
        let expanded = HashSet::from(["alpha.md".to_string(), "beta.md".to_string()]);
        // Text search "common" matches both; file search "alpha" narrows to one.
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "common",
            "alpha",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 2, "alpha header + its task");
    }

    /// File search composes with the status filter.
    #[test]
    fn build_view_file_search_composes_with_status_filter() {
        let tasks = vec![
            task(1, " ", 1, "alpha.md"), // open
            task(2, " ", 2, "beta.md"),  // open
            task(3, "x", 3, "beta.md"),  // done
        ];
        let expanded = HashSet::from(["beta.md".to_string()]);
        // File search "beta" + Open filter: beta header + only the open beta task.
        let rows = build_view(
            &tasks,
            StatusFilter::Open,
            &expanded,
            false,
            "",
            "",
            "beta",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 2, "beta header + one open task");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 2));
    }

    /// File search composes with text search and status filter (triple compose).
    #[test]
    fn build_view_triple_compose_file_text_status() {
        let alpha_task = |id: i64, text: &str, raw: &str| -> Task {
            let mut t = task(id, raw, id as usize, "alpha.md");
            t.text = text.to_string();
            t.text_hash = text.to_string();
            t
        };
        let tasks = vec![
            alpha_task(1, "deploy the migration", " "), // open
            alpha_task(2, "deploy the rollback", "x"),  // done
            alpha_task(3, "write documentation", " "),  // open
        ];
        // File "alpha" + text "deploy" + Open: only task 1 survives.
        let rows = build_view(
            &tasks,
            StatusFilter::Open,
            &HashSet::from(["alpha.md".to_string()]),
            false,
            "",
            "deploy",
            "alpha",
            false,
            GroupBy::Note,
        );
        assert_eq!(rows.len(), 2, "header + one task");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
    }

    // ── Group-by axis (G key) tests ──────────────────────────────────

    /// `group_keys` returns exactly one key for the Note axis.
    #[test]
    fn group_keys_note_axis_returns_note_path() {
        let t = task(1, " ", 1, "dir/note.md");
        let keys = group_keys(&t, GroupBy::Note);
        assert_eq!(keys, vec!["dir/note.md"]);
    }

    /// `group_keys` returns one key per tag for the Tag axis.
    #[test]
    fn group_keys_tag_axis_fans_out() {
        let t = task_with_tags(1, " ", 1, "n.md", &["work", "urgent"]);
        let keys = group_keys(&t, GroupBy::Tag);
        assert_eq!(keys, vec!["work", "urgent"]);
    }

    /// `group_keys` sends untagged tasks to `(untagged)`.
    #[test]
    fn group_keys_tag_axis_untagged_bucket() {
        let t = task(1, " ", 1, "n.md");
        let keys = group_keys(&t, GroupBy::Tag);
        assert_eq!(keys, vec!["(untagged)"]);
    }

    /// `group_keys` returns the priority label for the Priority axis.
    #[test]
    fn group_keys_priority_axis() {
        let t = task_with_priority(1, " ", 1, "n.md", Priority::High);
        let keys = group_keys(&t, GroupBy::Priority);
        assert_eq!(keys, vec!["High"]);
    }

    /// `group_keys` sends no-priority tasks to `(no priority)`.
    #[test]
    fn group_keys_priority_axis_no_priority() {
        let t = task(1, " ", 1, "n.md");
        let keys = group_keys(&t, GroupBy::Priority);
        assert_eq!(keys, vec!["(no priority)"]);
    }

    /// `group_keys` returns the parent directory for the Folder axis.
    #[test]
    fn group_keys_folder_axis_nested() {
        let t = task(1, " ", 1, "projects/work/note.md");
        let keys = group_keys(&t, GroupBy::Folder);
        assert_eq!(keys, vec!["projects/work"]);
    }

    /// `group_keys` sends top-level notes to `(root)` for the Folder axis.
    #[test]
    fn group_keys_folder_axis_root() {
        let t = task(1, " ", 1, "note.md");
        let keys = group_keys(&t, GroupBy::Folder);
        assert_eq!(keys, vec!["(root)"]);
    }

    /// `folder_of` extracts parent dirs and returns `(root)` for top-level notes.
    #[test]
    fn folder_of_handles_various_paths() {
        assert_eq!(folder_of("note.md"), "(root)");
        assert_eq!(folder_of("dir/note.md"), "dir");
        assert_eq!(folder_of("a/b/c/note.md"), "a/b/c");
    }

    /// `priority_group_label` maps each priority variant to its display label.
    #[test]
    fn priority_group_label_maps_all_variants() {
        assert_eq!(priority_group_label(Some(&Priority::Highest)), "Highest");
        assert_eq!(priority_group_label(Some(&Priority::High)), "High");
        assert_eq!(priority_group_label(Some(&Priority::Medium)), "Medium");
        assert_eq!(priority_group_label(Some(&Priority::Low)), "Low");
        assert_eq!(priority_group_label(Some(&Priority::Lowest)), "Lowest");
        assert_eq!(
            priority_group_label(Some(&Priority::Other("??".to_string()))),
            "(no priority)"
        );
        assert_eq!(priority_group_label(None), "(no priority)");
    }

    /// `priority_sort_rank` orders by importance (Highest first).
    #[test]
    fn priority_sort_rank_orders_by_importance() {
        assert_eq!(priority_sort_rank("Highest"), 0);
        assert_eq!(priority_sort_rank("High"), 1);
        assert_eq!(priority_sort_rank("Medium"), 2);
        assert_eq!(priority_sort_rank("Low"), 3);
        assert_eq!(priority_sort_rank("Lowest"), 4);
        assert_eq!(priority_sort_rank("(no priority)"), 5);
    }

    /// `GroupBy::next` cycles Note → Tag → Priority → Folder → Note.
    #[test]
    fn group_by_cycles_through_all_axes() {
        assert_eq!(GroupBy::Note.next(), GroupBy::Tag);
        assert_eq!(GroupBy::Tag.next(), GroupBy::Priority);
        assert_eq!(GroupBy::Priority.next(), GroupBy::Folder);
        assert_eq!(GroupBy::Folder.next(), GroupBy::Note);
    }

    /// `GroupBy::label` returns the short title-bar string.
    #[test]
    fn group_by_labels() {
        assert_eq!(GroupBy::Note.label(), "note");
        assert_eq!(GroupBy::Tag.label(), "tag");
        assert_eq!(GroupBy::Priority.label(), "priority");
        assert_eq!(GroupBy::Folder.label(), "folder");
    }

    /// Tag axis: tasks with tags produce one group per tag, alphabetically sorted.
    #[test]
    fn build_view_tag_axis_groups_by_tag() {
        let tasks = vec![
            task_with_tags(1, " ", 1, "a.md", &["zebra"]),
            task_with_tags(2, " ", 1, "b.md", &["alpha"]),
            task_with_tags(3, " ", 2, "a.md", &["zebra"]),
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Tag,
        );
        // Two groups: "alpha" (1 task), "zebra" (2 tasks) — alphabetical.
        assert_eq!(rows.len(), 2, "two collapsed tag-headers");
        let (key, _, _, _) = header(&rows[0]);
        assert_eq!(key, "alpha");
        let (key, _, total, _) = header(&rows[1]);
        assert_eq!(key, "zebra");
        assert_eq!(total, 2);
    }

    /// Tag axis: a task with multiple tags appears in every matching group (fan-out).
    #[test]
    fn build_view_tag_axis_fan_out_multiple_tags() {
        let tasks = vec![task_with_tags(1, " ", 1, "a.md", &["work", "urgent"])];
        let expanded = HashSet::from(["work".to_string(), "urgent".to_string()]);
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Tag,
        );
        // Two expanded groups, each with one task row (the same task, fanned out).
        assert_eq!(rows.len(), 4, "2 headers + 2 task rows (same task in both)");
        assert_eq!(header(&rows[0]).0, "urgent");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
        assert_eq!(header(&rows[2]).0, "work");
        assert!(matches!(&rows[3], DisplayRow::Task { task } if task.id == 1));
    }

    /// Tag axis: untagged tasks go to the `(untagged)` bucket.
    #[test]
    fn build_view_tag_axis_untagged_bucket() {
        let tasks = vec![
            task(1, " ", 1, "a.md"), // no tags
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Tag,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "(untagged)");
    }

    /// Priority axis: groups are ordered by importance (Highest first), not alphabetical.
    #[test]
    fn build_view_priority_axis_sorted_by_importance() {
        let tasks = vec![
            task_with_priority(3, " ", 1, "c.md", Priority::Low),
            task_with_priority(1, " ", 1, "a.md", Priority::Highest),
            task_with_priority(2, " ", 1, "b.md", Priority::Medium),
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Priority,
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(header(&rows[0]).0, "Highest");
        assert_eq!(header(&rows[1]).0, "Medium");
        assert_eq!(header(&rows[2]).0, "Low");
    }

    /// Priority axis: no-priority tasks go to `(no priority)` at the end.
    #[test]
    fn build_view_priority_axis_no_priority_at_end() {
        let tasks = vec![
            task(1, " ", 1, "a.md"), // no priority
            task_with_priority(2, " ", 1, "b.md", Priority::High),
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Priority,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(header(&rows[0]).0, "High");
        assert_eq!(header(&rows[1]).0, "(no priority)");
    }

    /// Folder axis: groups by parent directory, top-level notes go to `(root)`.
    #[test]
    fn build_view_folder_axis_groups_by_directory() {
        let tasks = vec![
            task(1, " ", 1, "note.md"),       // (root)
            task(2, " ", 1, "projects/a.md"), // projects
            task(3, " ", 1, "projects/b.md"), // projects
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Folder,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(header(&rows[0]).0, "(root)");
        let (key, _, total, _) = header(&rows[1]);
        assert_eq!(key, "projects");
        assert_eq!(total, 2);
    }

    /// Folder axis: nested directories produce nested group keys.
    #[test]
    fn build_view_folder_axis_nested_dirs() {
        let tasks = vec![task(1, " ", 1, "a/b/c.md"), task(2, " ", 1, "a/d.md")];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Folder,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(header(&rows[0]).0, "a");
        assert_eq!(header(&rows[1]).0, "a/b");
    }

    /// Tag axis: expanding a tag group shows its task rows.
    #[test]
    fn build_view_tag_axis_expanded_shows_tasks() {
        let tasks = vec![
            task_with_tags(1, " ", 1, "a.md", &["work"]),
            task_with_tags(2, " ", 2, "b.md", &["work"]),
        ];
        let expanded = HashSet::from(["work".to_string()]);
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Tag,
        );
        assert_eq!(rows.len(), 3, "header + two tasks");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
        assert!(matches!(&rows[2], DisplayRow::Task { task } if task.id == 2));
    }

    /// Tag axis: status filter hides done tasks within a tag group but keeps the header.
    #[test]
    fn build_view_tag_axis_status_filter() {
        let tasks = vec![
            task_with_tags(1, " ", 1, "a.md", &["work"]),
            task_with_tags(2, "x", 2, "b.md", &["work"]),
        ];
        let expanded = HashSet::from(["work".to_string()]);
        let rows = build_view(
            &tasks,
            StatusFilter::Open,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Tag,
        );
        assert_eq!(rows.len(), 2, "header + only the open task");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
    }

    /// Tag axis: a tag group with no filter-matching task is hidden.
    #[test]
    fn build_view_tag_axis_hides_empty_group() {
        let tasks = vec![
            task_with_tags(1, " ", 1, "a.md", &["work"]),
            task_with_tags(2, "x", 1, "b.md", &["done-only"]),
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::Open,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Tag,
        );
        // Only "work" has an open task; "done-only" is hidden.
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "work");
    }

    /// Header open/total counts come from the full bucket (pre-filter).
    #[test]
    fn build_view_tag_axis_counts_from_full_bucket() {
        let tasks = vec![
            task_with_tags(1, " ", 1, "a.md", &["work"]), // open
            task_with_tags(2, "x", 2, "b.md", &["work"]), // done
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Tag,
        );
        let (_, open, total, _) = header(&rows[0]);
        assert_eq!(open, 1);
        assert_eq!(total, 2);
    }

    /// `cycle_group_by` advances the axis and rebuilds the view.
    #[test]
    fn cycle_group_by_advances_axis() {
        let mut app = App::new();
        app.tasks = vec![task_with_tags(1, " ", 1, "a.md", &["work"])];
        app.rebuild();
        assert_eq!(app.group_by, GroupBy::Note);
        assert_eq!(header(&app.rows[0]).0, "a.md");

        app.cycle_group_by();
        assert_eq!(app.group_by, GroupBy::Tag);
        assert_eq!(header(&app.rows[0]).0, "work");
    }

    /// `cycle_group_by` wraps around from Folder back to Note.
    #[test]
    fn cycle_group_by_wraps_around() {
        let mut app = App::new();
        app.group_by = GroupBy::Folder;
        app.tasks = vec![task(1, " ", 1, "a.md")];
        app.rebuild();
        app.cycle_group_by();
        assert_eq!(app.group_by, GroupBy::Note);
    }

    /// Cycling the axis does NOT clear `expanded` — stale keys naturally don't match.
    #[test]
    fn cycle_group_by_does_not_clear_expanded() {
        let mut app = App::new();
        app.tasks = vec![task(1, " ", 1, "a.md")];
        app.expanded.insert("a.md".to_string());
        app.rebuild();
        assert!(app.expanded.contains("a.md"));

        app.cycle_group_by(); // Note → Tag
        // expanded set still has the old key, but it doesn't match "(untagged)"
        assert!(app.expanded.contains("a.md"));
        assert!(matches!(app.rows[0], DisplayRow::Header { collapsed, .. } if collapsed));
    }

    /// Collapsing from a task row under the Tag axis scans backwards for the
    /// nearest preceding Header's group_key.
    #[test]
    fn toggle_collapse_from_task_under_tag_axis() {
        let mut app = App::new();
        app.tasks = vec![task_with_tags(1, " ", 1, "a.md", &["work"])];
        app.group_by = GroupBy::Tag;
        app.rebuild();
        app.expanded.insert("work".to_string());
        app.rebuild();
        // rows: [H "work", T1]
        assert_eq!(app.rows.len(), 2);

        // Cursor on task row, press ← to collapse parent.
        app.state.select(Some(1));
        app.toggle_at_cursor(ToggleMode::Collapse);
        assert!(
            !app.expanded.contains("work"),
            "tag group should be collapsed"
        );
        assert_eq!(app.rows.len(), 1, "only header remains");
    }

    /// Collapsing from a task row under the Folder axis works the same way.
    #[test]
    fn toggle_collapse_from_task_under_folder_axis() {
        let mut app = App::new();
        app.tasks = vec![task(1, " ", 1, "projects/a.md")];
        app.group_by = GroupBy::Folder;
        app.rebuild();
        app.expanded.insert("projects".to_string());
        app.rebuild();
        // rows: [H "projects", T1]
        assert_eq!(app.rows.len(), 2);

        app.state.select(Some(1));
        app.toggle_at_cursor(ToggleMode::Collapse);
        assert!(!app.expanded.contains("projects"));
    }

    /// Expanding a tag group under the Tag axis works via the group_key.
    #[test]
    fn toggle_expand_header_under_tag_axis() {
        let mut app = App::new();
        app.tasks = vec![task_with_tags(1, " ", 1, "a.md", &["work"])];
        app.group_by = GroupBy::Tag;
        app.rebuild();
        // rows: [H "work" collapsed]
        assert_eq!(app.rows.len(), 1);

        app.state.select(Some(0));
        app.toggle_at_cursor(ToggleMode::Expand);
        assert!(app.expanded.contains("work"));
        assert_eq!(app.rows.len(), 2, "header + task");
    }

    /// `expand_all` under the Tag axis adds all visible tag-group keys.
    #[test]
    fn expand_all_under_tag_axis() {
        let mut app = App::new();
        app.tasks = vec![
            task_with_tags(1, " ", 1, "a.md", &["alpha"]),
            task_with_tags(2, " ", 1, "b.md", &["beta"]),
        ];
        app.group_by = GroupBy::Tag;
        app.rebuild();
        assert_eq!(app.rows.len(), 2, "two collapsed tag-headers");

        app.expand_all();
        assert_eq!(app.rows.len(), 4, "two headers + two task rows");
        assert!(app.expanded.contains("alpha"));
        assert!(app.expanded.contains("beta"));
    }

    /// `collapse_all` clears all expanded keys under any axis.
    #[test]
    fn collapse_all_under_priority_axis() {
        let mut app = App::new();
        app.tasks = vec![
            task_with_priority(1, " ", 1, "a.md", Priority::High),
            task_with_priority(2, " ", 1, "b.md", Priority::Low),
        ];
        app.group_by = GroupBy::Priority;
        app.rebuild();
        app.expand_all();
        assert_eq!(app.rows.len(), 4);

        app.collapse_all();
        assert_eq!(app.rows.len(), 2, "back to two headers");
        assert!(app.expanded.is_empty());
    }

    /// Default `App` starts with `GroupBy::Note`.
    #[test]
    fn app_default_group_by_is_note() {
        let app = App::new();
        assert_eq!(app.group_by, GroupBy::Note);
    }

    /// Header counts are accurate under the Tag axis when a task fans out
    /// to multiple tags — each group counts the task independently.
    #[test]
    fn build_view_tag_axis_fan_out_counts() {
        let tasks = vec![
            task_with_tags(1, " ", 1, "a.md", &["work", "urgent"]),
            task_with_tags(2, "x", 2, "b.md", &["work"]),
        ];
        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Tag,
        );
        // "urgent" group: 1 task (id 1), 1 open
        // "work" group: 2 tasks (id 1 + 2), 1 open (id 2 is done)
        assert_eq!(rows.len(), 2);
        let (key, open, total, _) = header(&rows[0]);
        assert_eq!(key, "urgent");
        assert_eq!(open, 1);
        assert_eq!(total, 1);
        let (key, open, total, _) = header(&rows[1]);
        assert_eq!(key, "work");
        assert_eq!(open, 1);
        assert_eq!(total, 2);
    }

    /// Folder axis composes with the status filter.
    #[test]
    fn build_view_folder_axis_with_status_filter() {
        let tasks = vec![
            task(1, " ", 1, "projects/a.md"), // open
            task(2, "x", 2, "projects/b.md"), // done
        ];
        let expanded = HashSet::from(["projects".to_string()]);
        let rows = build_view(
            &tasks,
            StatusFilter::Open,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::Folder,
        );
        assert_eq!(rows.len(), 2, "header + only the open task");
        assert_eq!(header(&rows[0]).0, "projects");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
    }

    /// Priority axis composes with the search filter.
    #[test]
    fn build_view_priority_axis_with_search() {
        let t1 = {
            let mut t = task_with_priority(1, " ", 1, "a.md", Priority::High);
            t.text = "deploy server".to_string();
            t
        };
        let t2 = {
            let mut t = task_with_priority(2, " ", 2, "b.md", Priority::Low);
            t.text = "write docs".to_string();
            t
        };
        let tasks = vec![t1, t2];

        let expanded = HashSet::new();
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "deploy",
            "",
            false,
            GroupBy::Priority,
        );
        // Only "High" group matches the search.
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "High");
    }
}
