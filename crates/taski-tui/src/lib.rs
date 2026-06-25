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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use rusqlite::Connection;

use taski_db as db;
use taski_db::{NoteContent, PendingAction, Priority, Status, Task};

pub mod theme;

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
    // ADR-0014: resolve the inbox path once (config → default) and thread it
    // into the loop so the `a` key modal knows where to enqueue the write.
    let inbox_path = taski_config::resolve_inbox_path(&cfg);
    // ADR-0021: resolve the archive path (config → default) and thread it in so the
    // `A` key knows where to move completed tasks.
    let archive_path = taski_config::resolve_archive_path(&cfg);
    // Open-in-Obsidian (`o` key): resolve the vault name for `obsidian://` deep
    // links. An explicit `obsidian_vault` override wins; otherwise derive it from
    // the resolved vault path's basename. If no vault is configured at all this is
    // a graceful `None` (the `o` gesture becomes a logged no-op — the TUI still
    // works as a read-only browser). `use_advanced_uri` flows straight from config.
    let vault_name = if let Some(name) = cfg.obsidian_vault.as_deref() {
        Some(name.to_string())
    } else {
        taski_config::resolve_vault(None, &cfg).ok().and_then(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
    };
    let use_advanced_uri = cfg.use_advanced_uri;
    // ADR-0018 S2: resolve the TUI theme from the optional `[theme]` section.
    // Bad colour values fall back per-role with a tracing::warn!; resolution
    // happens before enter_terminal() so the alt screen is never garbled.
    let theme = theme::Theme::resolve_from(cfg.theme.as_ref());
    // ADR-0018 S3: resolve per-panel layout prefs from the optional `[ui]`
    // section. Bad list_pane_percent clamps; bad list_density variant errors
    // at config load (before alt screen).
    let layout = theme::LayoutPrefs::resolve_from(cfg.ui.as_ref());

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
    let result = run_loop(
        &mut terminal,
        &conn,
        quit_hook.as_ref(),
        &inbox_path,
        &archive_path,
        vault_name.as_deref(),
        use_advanced_uri,
        theme,
        layout,
    );
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

/// Percent-encode a string for use as a query parameter value in an `obsidian://`
/// URL. Uses RFC 3986 component encoding: unreserved chars (`A-Za-z0-9-._~`) are
/// kept; all other bytes are `%XX`-encoded (UTF-8 for non-ASCII). Critically this
/// encodes `/` as `%2F` and space as `%20`, as required by Obsidian's URL scheme.
fn percent_encode_query(s: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Build an `obsidian://` deep-link URL for a task's location.
/// - `advanced == false`: native `obsidian://open?vault=<v>&file=<f>` (opens the
///   file; no line targeting).
/// - `advanced == true`: `obsidian://advanced-uri?vault=<v>&filepath=<f>&line=<n>`
///   (jumps to the line; requires the Advanced URI community plugin).
fn obsidian_url(vault: &str, note_path: &str, line: usize, advanced: bool) -> String {
    let v = percent_encode_query(vault);
    let f = percent_encode_query(note_path);
    if advanced {
        format!("obsidian://advanced-uri?vault={v}&filepath={f}&line={line}")
    } else {
        format!("obsidian://open?vault={v}&file={f}")
    }
}

// ---------------------------------------------------------------------------
// View model: grouping + filtering over the raw task list.
// ---------------------------------------------------------------------------

/// Status filter cycled with `f`: All -> Open -> Done -> All. `Open` matches active
/// (not-done) tasks — both `Status::Open` and `Status::InProgress` — so an
/// in-progress task shows alongside unstarted ones under the default Open filter.
/// Done and other states appear only under `All`. This keeps the three-state
/// mapping (all / open / done) and the open/total counts consistent: an in-progress
/// task is treated as open for both visibility and counting (see [`is_open_like`]).
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
            StatusFilter::Open => is_open_like(status),
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

/// Whether a task counts as "open" for the `f` status filter and the open/total
/// counts in group headers and the title bar. Both `Open` and `InProgress` are
/// active (not-done) states, so an in-progress task shows alongside unstarted ones
/// under the default Open filter instead of being hidden. `Done` and other states
/// are excluded. The single predicate keeps the filter, the header counts and the
/// title count in agreement (ADR-0016 follow-on).
fn is_open_like(status: &Status) -> bool {
    matches!(status, Status::Open | Status::InProgress)
}

/// ADR-0021: whether a task is **closed** (done or cancelled) and thus archivable by
/// the `A` gesture. `[x]`/`[X]` is `Status::Done`; `[-]` cancelled is parsed as
/// `Status::Other("-")`, so it is detected via the raw checkbox char. Open (`[ ]`)
/// and in-progress (`[/]`) tasks are never archived — the inbox stays focused on
/// active work.
fn is_completed(status: &Status, raw_checkbox_char: &str) -> bool {
    matches!(status, Status::Done) || raw_checkbox_char == "-"
}

/// Grouping axis cycled with `G`: FolderNote → Note → Tag → Priority → Folder →
/// FolderNote. The default is FolderNote (the classic "one group per source
/// file path" view).
///
/// The three path-derived axes form a coarse→fine progression on the same note:
/// - **Folder** keys on the parent directory (`Projects/Work`).
/// - **FolderNote** keys on the full note path (`Projects/Work/standup.md`) and
///   renders the directory prefix dimmed so the filename pops.
/// - **Note** keys on the filename alone (`standup.md`), ignoring the directory —
///   so identically-named notes in different folders collapse into one group.
///
/// Tag fans out a single task to multiple groups; Priority, Folder, FolderNote,
/// and Note each produce exactly one key per task.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GroupBy {
    FolderNote,
    Note,
    Tag,
    Priority,
    Folder,
}

impl GroupBy {
    /// Cycle to the next axis: FolderNote → Note → Tag → Priority → Folder →
    /// FolderNote. The two note axes sit next to the folder axis so the
    /// coarse→fine path views are adjacent.
    fn next(self) -> Self {
        match self {
            GroupBy::FolderNote => GroupBy::Note,
            GroupBy::Note => GroupBy::Tag,
            GroupBy::Tag => GroupBy::Priority,
            GroupBy::Priority => GroupBy::Folder,
            GroupBy::Folder => GroupBy::FolderNote,
        }
    }

    /// Short label for the title-bar indicator.
    fn label(self) -> &'static str {
        match self {
            GroupBy::FolderNote => "folder+note",
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
/// no-folder (top-level note) to `(root)`. `FolderNote` keys on the full note
/// path; `Note` keys on the filename alone (so same-named notes in different
/// folders share a group).
fn group_keys(task: &Task, axis: GroupBy) -> Vec<String> {
    match axis {
        GroupBy::FolderNote => vec![task.note_path.clone()],
        GroupBy::Note => vec![filename_of(&task.note_path)],
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

/// The filename (last path component) of a note path, ignoring directories.
/// `Projects/Work/standup.md` → `standup.md`; a root-level note returns itself.
/// Used by the `Note` grouping axis (filename-only), the fine end of the
/// Folder → FolderNote → Note progression. Reuses [`split_note_header`] so the
/// "what counts as the filename" rule stays in one place.
fn filename_of(note_path: &str) -> String {
    split_note_header(note_path).1.to_string()
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
/// - **FolderNote** (default): one group per source note path.
/// - **Note**: one group per filename, ignoring directories — same-named notes in
///   different folders merge into one group.
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
/// `today_only` (ADR-0009 Phase 1, widened by ADR-0022) adds an orthogonal, stricter
/// predicate on top of `filter`: when true, only tasks whose `scheduled_date == today`
/// OR `due_date == today` are visible. It is kept independent of `filter` (today-ness
/// vs open/done) so the two compose — e.g. `today_only + Open` = today's open work.
/// `today` is a `YYYY-MM-DD` string; it is only consulted when `today_only` is true.
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
    let matches_today = |t: &Task| -> bool {
        !today_only
            || t.scheduled_date.as_deref() == Some(today)
            || t.due_date.as_deref() == Some(today)
    };
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
            && matches_today(t)
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
        let open_count = bucket.iter().filter(|t| is_open_like(&t.status)).count();
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
    /// tasks whose `scheduled_date == today` OR `due_date == today` (ADR-0022 widened
    /// this from scheduled-only). Independent of `filter` (today-ness and open/done
    /// are orthogonal axes): `today_only + Open` = today's open work.
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
    /// ADR-0014: when true, the quick-add text-entry modal is active (the `a`
    /// key). Keystrokes accumulate in `quick_add_query` instead of performing
    /// their normal action. Mirrors `searching`/`file_searching` but does NOT
    /// call `rebuild()` (no filter to recompute — it's a creation modal, not a
    /// filter).
    quick_adding: bool,
    /// ADR-0014: the text typed so far in the quick-add modal.
    quick_add_query: String,
    /// ADR-0019: when true, the add-note text-entry modal is active (the `n`
    /// key). Keystrokes accumulate in `note_query`. Mirrors `quick_adding`, but
    /// the action targets the selected task (append a note + add an in-page link)
    /// rather than creating a new inbox task.
    adding_note: bool,
    /// ADR-0019: the note text typed so far in the add-note modal.
    note_query: String,
    /// ADR-0020: when true, move mode is active (the `m` key). `j`/`k` bubble the
    /// selected task up/down among its note's tasks by swapping rows locally;
    /// `Enter` commits the new order as one `reorder` action, `Esc` restores the
    /// original order. Purely local until commit — the index refresh is suspended
    /// while moving so it can't clobber the in-progress reorder.
    moving: bool,
    /// ADR-0020: the surrogate id of the task being moved (so the selection and the
    /// reorder anchor follow it as it bubbles, and `Esc` can re-select it).
    move_task_id: i64,
    /// ADR-0020: the moved group's task ids in their order at move-start, used to
    /// restore the original order on `Esc` and to detect a no-op commit.
    move_initial_ids: Vec<i64>,
    /// ADR-0014: the resolved inbox path (vault-relative), threaded from
    /// `run_inner` via `taski_config::resolve_inbox_path`. Defaults to
    /// `"task-inbox.md"` for tests that construct `App::new()` directly.
    inbox_path: String,
    /// ADR-0021: the resolved archive path (vault-relative), threaded from
    /// `run_inner` via `taski_config::resolve_archive_path`. The `A` key moves a
    /// note's completed tasks here. Defaults to `"task-archive.md"` for tests.
    archive_path: String,
    /// The Obsidian vault name used for `obsidian://` deep links (the `o` key).
    /// `None` when no vault is resolvable from config (the `o` gesture is then a
    /// logged no-op). Threaded from `run_inner`; defaults to `None` for tests.
    vault_name: Option<String>,
    /// Whether the `o` gesture uses the Advanced URI plugin (`obsidian://advanced-uri`
    /// with `&line=<n>`) instead of the native `obsidian://open`. Threaded from
    /// `run_inner` via `Config::use_advanced_uri`; defaults to `false` for tests.
    use_advanced_uri: bool,
    /// ADR-0011: the last enqueued write action, for undo (`u` key).
    last_action: Option<LastAction>,
    /// Grouping axis cycled with `G` (Note → Tag → Priority → Folder). Defaults
    /// to Note. The `expanded` set keys match the active axis's group labels, so
    /// switching axes naturally starts every group collapsed (old keys won't
    /// match the new axis's labels) without needing to clear the set.
    group_by: GroupBy,
    /// Whether the floating "Keybindings" help overlay (`?`) is open. Modal:
    /// while true, [`run_loop`] intercepts keys before normal-mode dispatch —
    /// `?`/`Esc`/`q` dismiss it (notably `q` does NOT quit while help is open),
    /// `Ctrl-C` still quits (the emergency exit), and every other key is
    /// swallowed so nothing fires while the user is reading.
    show_help: bool,
    /// The active colour theme — 12 semantic roles used by every render call site.
    /// Defaults reproduce the hardcoded palette (S1 refactor); config overrides
    /// land in S2.
    theme: theme::Theme,
    /// Per-panel density preferences (pane split, spacing, wrapping).
    /// Defaults match current behaviour; wired in S3.
    layout: theme::LayoutPrefs,
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
    /// ADR-0014: a quick-add that appended a line to the inbox. Undo enqueues a
    /// `quick_add_undo` action (separate action_type, dispatched separately).
    QuickAdd { inbox_path: String, text: String },
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
            quick_adding: false,
            quick_add_query: String::new(),
            adding_note: false,
            note_query: String::new(),
            moving: false,
            move_task_id: 0,
            move_initial_ids: Vec::new(),
            inbox_path: "task-inbox.md".to_string(),
            archive_path: "task-archive.md".to_string(),
            vault_name: None,
            use_advanced_uri: false,
            last_action: None,
            group_by: GroupBy::FolderNote,
            show_help: false,
            theme: theme::Theme::default(),
            layout: theme::LayoutPrefs::default(),
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

    // ── ADR-0014 quick-add (a key) ───────────────────────────────────

    /// Enter the quick-add text-entry modal. Finishes any search prompt so only
    /// one modal is active at a time. Unlike search, does NOT call `rebuild()`
    /// (no filter to recompute — it's a creation modal, not a filter).
    fn start_quick_add(&mut self) {
        self.searching = false;
        self.file_searching = false;
        self.quick_adding = true;
        self.quick_add_query.clear();
    }

    /// Append a character to the quick-add query.
    fn push_quick_add_char(&mut self, c: char) {
        self.quick_add_query.push(c);
    }

    /// Pop the last character (Backspace).
    fn pop_quick_add_char(&mut self) {
        self.quick_add_query.pop();
    }

    /// Cancel quick-add: clear the query, exit the modal.
    fn clear_quick_add(&mut self) {
        self.quick_adding = false;
        self.quick_add_query.clear();
    }

    /// `Enter` in the quick-add modal: enqueue a `quick_add` action for the
    /// typed text (trimmed; empty text is a no-op that just dismisses the modal).
    /// Records `LastAction::QuickAdd` so `u` can undo. Exits the modal on both
    /// success and empty-text.
    fn submit_quick_add(&mut self, conn: &Connection) {
        let text = self.quick_add_query.trim();
        if text.is_empty() {
            self.clear_quick_add();
            return;
        }
        let text = text.to_string();
        let inbox = self.inbox_path.clone();
        let result = db::enqueue_quick_add(conn, &inbox, &text).context("enqueuing quick_add");
        // S4: only record an undoable action if the enqueue succeeded.
        if result.is_ok() {
            self.last_action = Some(LastAction::QuickAdd {
                inbox_path: inbox,
                text: text.clone(),
            });
        }
        self.quick_add_query.clear();
        self.quick_adding = false;
        self.track_enqueued(result);
    }

    /// ADR-0019: open the add-note modal for the task under the cursor (the `n`
    /// key). No-op on a header / empty list — a note must attach to a real task.
    /// Mirrors `start_quick_add`; suppresses the other modals.
    fn start_add_note(&mut self) {
        if self.selected_task().is_none() {
            return;
        }
        self.searching = false;
        self.file_searching = false;
        self.quick_adding = false;
        self.adding_note = true;
        self.note_query.clear();
    }

    /// Append a character to the note query.
    fn push_note_char(&mut self, c: char) {
        self.note_query.push(c);
    }

    /// Pop the last character (Backspace).
    fn pop_note_char(&mut self) {
        self.note_query.pop();
    }

    /// Cancel add-note: clear the query, exit the modal.
    fn clear_add_note(&mut self) {
        self.adding_note = false;
        self.note_query.clear();
    }

    /// `Enter` in the add-note modal: enqueue an `add_note` action (ADR-0019) for
    /// the selected task with the typed text (trimmed; empty text is a no-op that
    /// just dismisses the modal). No undo in v1 (no `LastAction` recorded). The
    /// daemon performs the vault write (note append + first-note link insertion).
    fn submit_add_note(&mut self, conn: &Connection) {
        let text = self.note_query.trim();
        if text.is_empty() {
            self.clear_add_note();
            return;
        }
        let text = text.to_string();
        let result = {
            let Some(task) = self.selected_task() else {
                self.clear_add_note();
                return;
            };
            db::enqueue_add_note(conn, task.id, &task.note_path, task.line_number, &text)
                .context("enqueuing add_note")
        };
        self.note_query.clear();
        self.adding_note = false;
        self.track_enqueued(result);
    }

    // ── ADR-0020 move mode (m key) ───────────────────────────────────────

    /// The `[start, end)` row-index span of the contiguous run of `Task` rows that
    /// contains the current selection — i.e. the selected task's group's task rows
    /// (groups are separated by `Header` rows). `None` if the cursor is not on a
    /// task row. Recomputable each call because rows are stable while moving.
    fn group_task_row_range(&self) -> Option<(usize, usize)> {
        let sel = self.state.selected()?;
        if !matches!(self.rows.get(sel)?, DisplayRow::Task { .. }) {
            return None;
        }
        let mut start = sel;
        while start > 0 && matches!(self.rows[start - 1], DisplayRow::Task { .. }) {
            start -= 1;
        }
        let mut end = sel + 1;
        while end < self.rows.len() && matches!(self.rows[end], DisplayRow::Task { .. }) {
            end += 1;
        }
        Some((start, end))
    }

    /// Enter move mode on the task under the cursor (`m`). Refused (with a notice,
    /// no state change) when the cursor isn't on a task, the group spans more than
    /// one note (reorder is a within-note permutation), or the note has nested
    /// tasks (flat-only in v1 — a content swap would orphan a parent's children).
    fn start_move(&mut self) {
        let Some((start, end)) = self.group_task_row_range() else {
            return;
        };
        // Owned snapshot of the run so the borrow of `self.rows` ends before the
        // eligibility checks (which set `self.notice`).
        let run: Vec<(i64, String)> = self.rows[start..end]
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Task { task } => Some((task.id, task.note_path.clone())),
                _ => None,
            })
            .collect();
        let Some((anchor_id, note_path)) = run.first().cloned() else {
            return;
        };
        if run.iter().any(|(_, np)| np != &note_path) {
            self.notice =
                Some("Reorder works within a single note (use folder+note grouping)".to_string());
            return;
        }
        if self
            .tasks
            .iter()
            .any(|t| t.note_path == note_path && t.indent != 0)
        {
            self.notice = Some(
                "Reorder supports flat task lists only (v1); this note has subtasks".to_string(),
            );
            return;
        }
        self.move_task_id = self.selected_task().map(|t| t.id).unwrap_or(anchor_id);
        self.move_initial_ids = run.into_iter().map(|(id, _)| id).collect();
        self.moving = true;
    }

    /// `j`/`k` (or `↑`/`↓`) while moving: bubble the selected task one row toward
    /// `delta`, swapping it with its neighbour. Clamped to the note's task run, so
    /// the task can't escape past the group's first/last task. Local only — nothing
    /// is written until `Enter`.
    fn move_task(&mut self, delta: i32) {
        let Some((start, end)) = self.group_task_row_range() else {
            return;
        };
        let Some(sel) = self.state.selected() else {
            return;
        };
        let target = sel as i32 + delta;
        if target < start as i32 || target >= end as i32 {
            return; // at the top/bottom of the note's task block
        }
        let target = target as usize;
        self.rows.swap(sel, target);
        self.state.select(Some(target));
        self.ctx_scroll = 0;
    }

    /// `Enter` while moving: commit the new order. Enqueues one `reorder` action
    /// carrying the run's task line numbers in their new top-to-bottom order, unless
    /// the order is unchanged from move-start (an idempotent no-op → no enqueue).
    /// Exits move mode either way; the next refresh (resumed now) reflects the write.
    fn commit_move(&mut self, conn: &Connection) {
        // Compute the enqueue inputs while only borrowing `self` immutably.
        let plan = self.group_task_row_range().and_then(|(start, end)| {
            let run: Vec<(i64, usize, String)> = self.rows[start..end]
                .iter()
                .filter_map(|r| match r {
                    DisplayRow::Task { task } => {
                        Some((task.id, task.line_number, task.note_path.clone()))
                    }
                    _ => None,
                })
                .collect();
            let current_ids: Vec<i64> = run.iter().map(|(id, _, _)| *id).collect();
            if run.is_empty() || current_ids == self.move_initial_ids {
                return None; // nothing selected, or no net change
            }
            let note_path = run[0].2.clone();
            let desired: Vec<usize> = run.iter().map(|(_, ln, _)| *ln).collect();
            let anchor_line = run
                .iter()
                .find(|(id, _, _)| *id == self.move_task_id)
                .map(|(_, ln, _)| *ln)
                .unwrap_or(run[0].1);
            Some((self.move_task_id, note_path, anchor_line, desired))
        });

        self.moving = false;
        self.move_initial_ids.clear();

        if let Some((anchor_id, note_path, anchor_line, desired)) = plan {
            let result = db::enqueue_reorder(conn, anchor_id, &note_path, anchor_line, &desired)
                .context("enqueuing reorder");
            self.track_enqueued(result);
        }
    }

    /// `Esc` while moving: cancel — restore the group's task rows to their order at
    /// move-start and re-select the moved task. Nothing was ever written, so this is
    /// a pure local revert.
    fn cancel_move(&mut self) {
        let order = std::mem::take(&mut self.move_initial_ids);
        if let Some((start, end)) = self.group_task_row_range() {
            let mut drained: Vec<DisplayRow> = self.rows.drain(start..end).collect();
            drained.sort_by_key(|r| match r {
                DisplayRow::Task { task } => order
                    .iter()
                    .position(|&id| id == task.id)
                    .unwrap_or(usize::MAX),
                _ => usize::MAX,
            });
            for (i, row) in drained.into_iter().enumerate() {
                self.rows.insert(start + i, row);
            }
            if let Some(pos) = self.rows.iter().position(
                |r| matches!(r, DisplayRow::Task { task } if task.id == self.move_task_id),
            ) {
                self.state.select(Some(pos));
            }
        }
        self.moving = false;
        self.ctx_scroll = 0;
    }

    /// ADR-0021: archive every completed (`[x]` done / `[-]` cancelled) flat task in
    /// the selected task's note into the configured archive note (`A` key). One
    /// `archive` action, one keypress — the copy-then-delete vault write is the
    /// daemon's job (`enqueue_archive`); the TUI never touches files. Archives all of
    /// the note's completed tasks regardless of the active status filter. Refused
    /// (with a notice, no state change) when the cursor isn't on a task, the note has
    /// nested tasks (flat-only in v1), or the note has no completed tasks.
    fn archive_completed(&mut self, conn: &Connection) {
        let Some(note_path) = self.selected_task().map(|t| t.note_path.clone()) else {
            return;
        };
        // Flat-only: a content move can't carry a parent's children (same orphaning
        // hazard reorder guards against, ADR-0020/0021).
        if self
            .tasks
            .iter()
            .any(|t| t.note_path == note_path && t.indent != 0)
        {
            self.notice = Some(
                "Archive supports flat task lists only (v1); this note has subtasks".to_string(),
            );
            return;
        }
        // This note's completed flat task lines, in line order (top-to-bottom).
        let mut completed: Vec<(i64, usize)> = self
            .tasks
            .iter()
            .filter(|t| {
                t.note_path == note_path
                    && t.indent == 0
                    && is_completed(&t.status, &t.raw_checkbox_char)
            })
            .map(|t| (t.id, t.line_number))
            .collect();
        completed.sort_by_key(|(_, ln)| *ln);
        if completed.is_empty() {
            self.notice = Some("No completed tasks to archive in this note".to_string());
            return;
        }
        let archive = self.archive_path.clone();
        if archive == note_path {
            self.notice = Some("Archive path cannot be the source note".to_string());
            return;
        }
        let anchor_id = completed[0].0;
        let anchor_line = completed[0].1;
        let lines: Vec<usize> = completed.iter().map(|(_, ln)| *ln).collect();
        let count = lines.len();
        let result =
            db::enqueue_archive(conn, anchor_id, &note_path, anchor_line, &archive, &lines)
                .context("enqueuing archive");
        let ok = result.is_ok();
        // `track_enqueued` clears `notice` on success, so set the confirmation after.
        self.track_enqueued(result);
        if ok {
            self.notice = Some(format!("Archiving {count} completed task(s) → {archive}"));
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

    /// `T`: toggle the ADR-0009 "Today" view (widened by ADR-0022) — when on,
    /// `build_view` additionally restricts the list to tasks whose
    /// `scheduled_date == today` OR `due_date == today`. Independent of the `f`
    /// status-cycle. Lowercase `t` is intentionally NOT bound (reserved for the
    /// Phase 2 mark gesture).
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

    /// `?`: toggle the floating keybindings help overlay. The overlay is modal —
    /// see [`run_loop`] for the dismissal semantics (`?`/`Esc`/`q` close it,
    /// `Ctrl-C` still quits, other keys are swallowed).
    fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
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
        self.track_enqueued(result);
    }

    /// `d` (ADR-0013): toggle the selected task's cancelled state — the `❌`
    /// sibling of the ADR-0012 `✅` done-date stamp. Reuses the `checkbox`
    /// action_type and the [`LastAction::CheckboxToggle`] variant (cancel IS a
    /// checkbox flip with `new_char = "-"`), so undo-of-cancel is free: `u`
    /// already reverses checkbox flips, and the composed stamp logic restores
    /// `✅`/clears `❌` as appropriate on the reverse flip. The actual vault
    /// write (flip + `❌` stamp composed into one byte splice) is the daemon's
    /// job via [`enqueue_cancel`]; the TUI never touches files directly.
    fn submit_cancel(&mut self, conn: &Connection) {
        let (result, last_action) = {
            let Some(task) = self.selected_task() else {
                return;
            };
            let new_char = cancel_target_char(&task.raw_checkbox_char);
            let action = LastAction::CheckboxToggle {
                task_id: task.id,
                note_path: task.note_path.clone(),
                line_number: task.line_number,
                expected_char: task.raw_checkbox_char.clone(),
                new_char: new_char.to_string(),
            };
            (enqueue_cancel(conn, task), Some(action))
        };
        // S4: only record an undoable action if the enqueue actually succeeded.
        if result.is_ok() {
            self.last_action = last_action;
        }
        self.track_enqueued(result);
    }

    /// `i` (ADR-0016): toggle the selected task's in-progress state — the
    /// third checkbox-flip sibling alongside `Space` (done, ADR-0003/0012) and
    /// `d` (cancelled, ADR-0013). Reuses the `checkbox` action_type and the
    /// [`LastAction::CheckboxToggle`] variant (in-progress IS a checkbox flip
    /// with `new_char = "/"`), so undo-of-in-progress is free: `u` already
    /// reverses checkbox flips. The daemon's `process_action_at` skips the
    /// `✅`/`❌` stamp oracles for in-progress flips (ADR-0012/0013 — leave
    /// dated stamps untouched). The actual vault write is the daemon's job via
    /// [`enqueue_in_progress`]; the TUI never touches files directly.
    fn submit_in_progress(&mut self, conn: &Connection) {
        let (result, last_action) = {
            let Some(task) = self.selected_task() else {
                return;
            };
            let new_char = in_progress_target_char(&task.raw_checkbox_char);
            let action = LastAction::CheckboxToggle {
                task_id: task.id,
                note_path: task.note_path.clone(),
                line_number: task.line_number,
                expected_char: task.raw_checkbox_char.clone(),
                new_char: new_char.to_string(),
            };
            (enqueue_in_progress(conn, task), Some(action))
        };
        // S4: only record an undoable action if the enqueue actually succeeded.
        if result.is_ok() {
            self.last_action = last_action;
        }
        self.track_enqueued(result);
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
        self.track_enqueued(result);
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
            LastAction::QuickAdd { inbox_path, text } => {
                db::enqueue_quick_add_undo(conn, &inbox_path, &text)
                    .context("enqueuing quick_add_undo")
            }
        };
        // Don't update last_action (undo doesn't get its own undo).
        self.track_enqueued(result);
    }

    /// Record the outcome of an enqueue ([`submit_toggle`] /
    /// [`submit_set_scheduled`]): on success, track the new id so its resolution is
    /// surfaced on a later refresh, clear any prior notice, and bound growth if the
    /// daemon stalls; on error, swallowed and never propagated (S1: the TUI owns the
    /// alternate screen, so writing to stderr would garble it; failures surface via
    /// the pending_actions resolution on the next refresh). Shared so both write
    /// gestures stay consistent.
    fn track_enqueued(&mut self, result: Result<i64>) {
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
        self.track_enqueued(result);
    }
}

// ---------------------------------------------------------------------------
// Event loop + rendering.
// ---------------------------------------------------------------------------

/// Main render+event loop. Holds one DB connection for the whole session and re-reads
/// the index on a ~750ms cadence so daemon writes appear live without blocking input.
/// Returns when the user requests to quit.
#[allow(clippy::too_many_arguments)]
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    conn: &Connection,
    quit_hook: Option<&QuitHook>,
    inbox_path: &str,
    archive_path: &str,
    vault_name: Option<&str>,
    use_advanced_uri: bool,
    theme: theme::Theme,
    layout: theme::LayoutPrefs,
) -> Result<()> {
    let mut app = App::new();
    // ADR-0014: thread the resolved inbox path from `run_inner` (which read
    // `Config`). `App::new()` defaults to `"task-inbox.md"` for tests.
    app.inbox_path = inbox_path.to_string();
    // ADR-0021: thread the resolved archive path (the `A` key's destination).
    app.archive_path = archive_path.to_string();
    // Open-in-Obsidian (`o` key): thread the resolved vault name + advanced-uri
    // flag from `run_inner`. `App::new()` defaults to `None` / `false` for tests.
    app.vault_name = vault_name.map(|s| s.to_string());
    app.use_advanced_uri = use_advanced_uri;
    // ADR-0018: override the default theme and layout with the config-resolved ones.
    // `App::new()` uses `Theme::default()` / `LayoutPrefs::default()` for standalone
    // test construction.
    app.theme = theme;
    app.layout = layout;
    // `None` => never refreshed yet, so the first iteration reads immediately.
    let mut last_refresh: Option<Instant> = None;

    loop {
        // Refresh the task list on the interval, independent of input — but NOT
        // while move mode is active (ADR-0020): a refresh rebuilds `rows` from the
        // on-disk order and would clobber the in-progress local reorder. The
        // refresh resumes (and fires immediately, since `last_refresh` didn't
        // advance) as soon as move mode exits on commit/cancel.
        let due = !app.moving && last_refresh.is_none_or(|t| t.elapsed() >= REFRESH_INTERVAL);
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
        } else if app.quick_adding {
            // ADR-0014: quick-add modal (a key). Keystrokes build the query;
            // Enter enqueues a `quick_add` action, Esc cancels. Unlike search
            // it does NOT call `rebuild()` (no filter to recompute — it's a
            // creation modal, not a filter).
            match key.code {
                KeyCode::Esc => app.clear_quick_add(),
                KeyCode::Enter => app.submit_quick_add(conn),
                KeyCode::Backspace => app.pop_quick_add_char(),
                KeyCode::Char(c) => app.push_quick_add_char(c),
                _ => {}
            }
        } else if app.adding_note {
            // ADR-0019: add-note modal (n key). Keystrokes build the note; Enter
            // enqueues an `add_note` action for the selected task, Esc cancels.
            // Like quick-add it does NOT call `rebuild()` (no filter to recompute).
            match key.code {
                KeyCode::Esc => app.clear_add_note(),
                KeyCode::Enter => app.submit_add_note(conn),
                KeyCode::Backspace => app.pop_note_char(),
                KeyCode::Char(c) => app.push_note_char(c),
                _ => {}
            }
        } else if app.moving {
            // ADR-0020: move mode (m key) is MODAL. `j`/`k`/`↑`/`↓` bubble the
            // selected task within its note; `Enter` commits the new order as one
            // `reorder` action; `Esc` restores the original order. Nothing is
            // written until commit, so `Esc` is a free local revert. `Ctrl-C`
            // stays the emergency exit; every other key is swallowed.
            if ctrl && key.code == KeyCode::Char('c') {
                if let Some(hook) = quit_hook {
                    hook();
                }
                return Ok(());
            }
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => app.move_task(1),
                KeyCode::Up | KeyCode::Char('k') => app.move_task(-1),
                KeyCode::Enter => app.commit_move(conn),
                KeyCode::Esc => app.cancel_move(),
                _ => {}
            }
        } else if app.show_help {
            // Help overlay is MODAL: it intercepts keys before normal-mode
            // dispatch. `?`/`Esc`/`q` close it and do nothing else — notably
            // `q` does NOT quit while help is open (avoids accidental quit
            // while reading). `Ctrl-C` stays the emergency exit and quits from
            // any state. Every other key is swallowed so nothing fires while
            // the user is reading.
            if ctrl && key.code == KeyCode::Char('c') {
                if let Some(hook) = quit_hook {
                    hook();
                }
                return Ok(());
            }
            if help_dismisses_on(key.code) {
                app.show_help = false;
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
                // ADR-0009 Phase 1 (widened by ADR-0022): `T` toggles the Today view
                // (read-only) — tasks whose `scheduled_date == today` OR
                // `due_date == today`. Lowercase `t` is the Phase 2 mark-for-today
                // write gesture (above).
                KeyCode::Char('T') => app.toggle_today(),
                // Overdue filter (`O`): toggle the "past-due only" view. A 5th
                // orthogonal filter axis (date-based, like `T` but for `due_date <
                // today` instead of `scheduled_date == today` OR `due_date == today`).
                KeyCode::Char('O') => app.toggle_overdue(),
                // ADR-0011: `b` toggles checkbox ↔ bullet; `u` undoes last write.
                KeyCode::Char('b') => app.submit_bullet_toggle(conn),
                KeyCode::Char('u') => app.submit_undo(conn),
                // ADR-0013: `d` cancels the selected task (`- [ ]` → `- [-]`), the
                // `❌` sibling of ADR-0012's `✅` done-date stamp. Reuses the
                // checkbox action_type; the daemon composes the `❌ <today>` stamp
                // into the same byte splice. Suppressed during a search prompt —
                // `d` falls through to `push_search_char` in the search arms above,
                // exactly like `b`.
                KeyCode::Char('d') => app.submit_cancel(conn),
                // ADR-0016: `i` marks the selected task in-progress (`- [ ]` →
                // `- [/]`), the third checkbox-flip sibling alongside `Space`
                // (done) and `d` (cancelled). Reuses the checkbox action_type;
                // the daemon skips the ✅/❌ stamp oracles for in-progress flips
                // (ADR-0012/0013 — leave dated stamps untouched). Suppressed
                // during a search prompt — `i` falls through to `push_search_char`
                // in the search arms above, exactly like `b`/`d`/`a`/`t`.
                KeyCode::Char('i') => app.submit_in_progress(conn),
                // ADR-0010 text search: `/` opens the search prompt.
                KeyCode::Char('/') => app.start_search(),
                // ADR-0010 file search: `F` opens the file/path search prompt.
                KeyCode::Char('F') => app.start_file_search(),
                // ADR-0014 quick-add: `a` opens the inbox text-entry modal. The
                // actual vault append is the daemon's job via `enqueue_quick_add`;
                // the TUI never touches files directly. Suppressed during a search
                // prompt — `a` falls through to `push_search_char` in the search
                // arms above, exactly like `b`/`d`/`t`.
                KeyCode::Char('a') => app.start_quick_add(),
                // ADR-0019 task notes: `n` opens the add-note modal for the
                // selected task. The vault write (note append + first-note link
                // insertion) is the daemon's job via `enqueue_add_note`; the TUI
                // never touches files directly. Suppressed during a search prompt —
                // `n` falls through to `push_search_char` in the search arms above.
                KeyCode::Char('n') => app.start_add_note(),
                // ADR-0020 reorder: `m` enters move mode on the selected task.
                // `j`/`k` then bubble it within its note; `Enter` commits, `Esc`
                // cancels. The vault write (a single line-content permutation) is
                // the daemon's job via `enqueue_reorder`; the TUI never touches
                // files. Suppressed during a search prompt — `m` falls through to
                // `push_search_char` in the search arms above, like `b`/`a`/`n`.
                KeyCode::Char('m') => app.start_move(),
                // ADR-0021 archive: `A` moves every completed (`[x]`/`[-]`) flat task
                // in the selected task's note into the configured archive note. One
                // keypress; the copy-then-delete vault write is the daemon's job via
                // `enqueue_archive`. Suppressed during a search prompt — `A` falls
                // through to `push_search_char` in the search arms above, like `a`.
                KeyCode::Char('A') => app.archive_completed(conn),
                KeyCode::Tab => app.expand_all(),
                KeyCode::BackTab => app.collapse_all(),
                // Uppercase J/K scroll the context pane (lowercase j/k move the task list).
                KeyCode::Char('J') => app.scroll_context(1),
                KeyCode::Char('K') => app.scroll_context(-1),
                KeyCode::Char('p') => app.toggle_pane(),
                // Open the selected task in Obsidian via an `obsidian://` deep link
                // (read-only and TUI-local — no vault mutation, no daemon involvement).
                // Suppressed during any search/quick-add prompt: `o` there builds the
                // query string in the prompt-specific arms above, exactly like `b`/`a`.
                KeyCode::Char('o') => open_in_obsidian(&app),
                // `?` toggles the floating keybindings help overlay. Suppressed
                // during any search/quick-add prompt (there `?` builds the query).
                KeyCode::Char('?') => app.toggle_help(),
                _ => {}
            }
        }

        // After any selection change, load the newly-selected note's content if the
        // selection moved to a different note (refreshes already force a re-read).
        app.sync_context(conn, false);
    }
}

/// Build an `obsidian://` URL for the selected task and hand it to macOS `open`.
/// Read-only and TUI-local: no vault mutation, no daemon involvement. Best-effort;
/// logs a `tracing::warn!` if the spawn fails (e.g. `open` missing) or if no vault
/// name is configured. Borrows `App` immutably — this gesture mutates nothing.
fn open_in_obsidian(app: &App) {
    let Some(vault) = app.vault_name.as_deref() else {
        tracing::warn!("open-in-obsidian: vault name unknown (no vault in config); skipping");
        return;
    };
    let Some(task) = app.selected_task() else {
        return; // nothing selected — silent no-op
    };
    let url = obsidian_url(
        vault,
        &task.note_path,
        task.line_number,
        app.use_advanced_uri,
    );
    match std::process::Command::new("open")
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "open-in-obsidian: failed to spawn `open`")
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

/// Decide the desired checkbox char for a CANCEL gesture (ADR-0013) of `raw`:
/// cancelled (`"-"`) -> open (`" "`); anything else (open, done, in-progress,
/// forwarded, …) -> cancelled (`"-"`). This is the cancel sibling of
/// [`toggle_target_char`]: pressing `d` always targets the cancelled state
/// unless the task is already cancelled, in which case `d` re-opens it.
fn cancel_target_char(raw: &str) -> &'static str {
    match raw {
        "-" => " ",
        _ => "-",
    }
}

/// Decide the desired checkbox char for an IN-PROGRESS gesture (ADR-0016) of
/// `raw`: in-progress (`"/"`) -> open (`" "`); anything else (open, done,
/// cancelled, forwarded, …) -> in-progress (`"/"`). This is the in-progress
/// sibling of [`cancel_target_char`]: pressing `i` always targets the
/// in-progress state unless the task is already in-progress, in which case `i`
/// re-opens it.
fn in_progress_target_char(raw: &str) -> &'static str {
    match raw {
        "/" => " ",
        _ => "/",
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

/// Enqueue a cancel-flip request (ADR-0013) for `task` into the shared
/// `pending_actions` table. This reuses the existing `checkbox` action type —
/// cancel IS a checkbox flip with `new_char = "-"` (the Obsidian cancelled char).
/// No new action_type, no schema change. The daemon's `process_action_at` stamps
/// `❌ <today>` (or clears it) as part of the same splice. Non-blocking: just
/// inserts a row; the daemon applies it. Returns the new row id so the caller can
/// track its resolution across refreshes.
fn enqueue_cancel(conn: &Connection, task: &Task) -> Result<i64> {
    let new_char = cancel_target_char(&task.raw_checkbox_char);
    let id = db::enqueue_action(
        conn,
        task.id,
        &task.note_path,
        task.line_number,
        &task.raw_checkbox_char,
        new_char,
    )
    .context("enqueuing cancel action")?;
    Ok(id)
}

/// Enqueue an in-progress-flip request (ADR-0016) for `task` into the shared
/// `pending_actions` table. This reuses the existing `checkbox` action type —
/// in-progress IS a checkbox flip with `new_char = "/"` (the Obsidian
/// in-progress char). No new action_type, no schema change. The daemon's
/// `process_action_at` skips the `✅`/`❌` stamp oracles for in-progress flips
/// (per ADR-0012/0013 — ambiguous, do not guess), so only the checkbox char
/// changes. Non-blocking: just inserts a row; the daemon applies it. Returns
/// the new row id so the caller can track its resolution across refreshes.
fn enqueue_in_progress(conn: &Connection, task: &Task) -> Result<i64> {
    let new_char = in_progress_target_char(&task.raw_checkbox_char);
    let id = db::enqueue_action(
        conn,
        task.id,
        &task.note_path,
        task.line_number,
        &task.raw_checkbox_char,
        new_char,
    )
    .context("enqueuing in-progress action")?;
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
    } else if e.contains('✅') {
        // ADR-0012: the done-date unparseable phrase ("existing ✅ is malformed
        // or unparseable ..."). Distinguish from the scheduled/cancelled phrases
        // by the distinctive ✅ glyph — all three share "malformed or unparseable".
        "the done date on this line couldn't be parsed".to_string()
    } else if e.contains('❌') {
        // ADR-0013: the cancelled-date unparseable phrase ("existing ❌ is
        // malformed or unparseable ..."). Distinguished by the ❌ glyph.
        "the cancelled date on this line couldn't be parsed".to_string()
    } else if e.contains("malformed or unparseable") {
        // ADR-0009 Phase 2: the set_scheduled unparseable phrase (no emoji).
        "the scheduled date on this line couldn't be parsed".to_string()
    } else if e.contains("could not be converted to a bullet") {
        "this line has no checkbox or bullet to toggle".to_string()
    } else if e.contains("completed task lines") {
        // ADR-0021: the archive-inconsistent phrase ("archive no longer matches the
        // note's completed task lines ...").
        "the completed tasks changed".to_string()
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
        // The checkbox path covers two gestures that share `action_type = "checkbox"`:
        // the `Space` done-toggle (ADR-0003/0012) and the `d` cancel gesture
        // (ADR-0013). Distinguish them by `new_char`: a cancel flip targets `-`,
        // so a failed cancel's retry key is `d`, not `Space`. No new action_type
        // or LastAction variant is introduced for this (per ADR-0013).
        _ if action.new_char == "-" => ("Cancel", "d"),
        // ADR-0016: the in-progress gesture (i key) is also a `checkbox` row,
        // distinguished by `new_char == "/"`. A failed in-progress flip's retry
        // key is `i`.
        _ if action.new_char == "/" => ("Mark in-progress", "i"),
        // ADR-0014: quick-add (a key) and its undo (u key). These carry their
        // own action_types, so they're matched here — not in the checkbox
        // wildcard. A refused quick-add surfaces "Quick add not applied — …
        // Press a to try again", not the done-toggle wording.
        "quick_add" => ("Quick add", "a"),
        "quick_add_undo" => ("Quick-add undo", "u"),
        // ADR-0019: add-note (n key). A refused note surfaces "Add note not
        // applied — … Press n to try again".
        "add_note" => ("Add note", "n"),
        // ADR-0021: archive (A key). A refused archive surfaces "Archive not
        // applied — … Press A to try again".
        "archive" => ("Archive", "A"),
        // The default/checkbox path (done-toggle).
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
    // ADR-0018 follow-on: paint the themed window background first, so every
    // widget (which sets `.fg(...)` only — never `.bg`) renders on top of it and
    // the background shows through. `Color::Reset` is the "use the terminal's own
    // background" sentinel: while it's Reset we skip the paint entirely, keeping
    // default rendering byte-identical.
    if app.theme.background != Color::Reset {
        frame.render_widget(
            Block::default().style(Style::default().bg(app.theme.background)),
            area,
        );
    }
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
        let cols = Layout::horizontal([
            Constraint::Percentage(app.layout.list_pane_percent),
            Constraint::Percentage(100 - app.layout.list_pane_percent),
        ])
        .split(list_area);
        (cols[0], Some(cols[1]))
    } else {
        (list_area, None)
    };

    let open_total = app.tasks.iter().filter(|t| is_open_like(&t.status)).count();
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
                .fg(app.theme.accent)
                .add_modifier(app.theme.bold_modifier()),
        ),
        Span::raw("  ·  "),
        Span::styled(
            format!("group: {}", app.group_by.label()),
            Style::default()
                .fg(app.theme.group_accent)
                .add_modifier(app.theme.bold_modifier()),
        ),
    ];
    // ADR-0009 Phase 1: surface the Today view state so the user can see it's on.
    if app.today_only {
        title_spans.push(Span::raw("  ·  "));
        title_spans.push(Span::styled(
            "today",
            Style::default()
                .fg(app.theme.accent_bright)
                .add_modifier(app.theme.bold_modifier()),
        ));
    }
    // ADR-0010: surface active search query in the title bar.
    if !app.search_query.is_empty() {
        title_spans.push(Span::raw("  ·  "));
        title_spans.push(Span::styled(
            format!("search: {}", app.search_query),
            Style::default()
                .fg(app.theme.success)
                .add_modifier(app.theme.bold_modifier()),
        ));
    }
    // ADR-0010: surface active file/path search in the title bar.
    if !app.file_query.is_empty() {
        title_spans.push(Span::raw("  ·  "));
        title_spans.push(Span::styled(
            format!("file: {}", app.file_query),
            Style::default()
                .fg(app.theme.success)
                .add_modifier(app.theme.bold_modifier()),
        ));
    }
    // Overdue filter indicator: red/bold to signal urgency.
    if app.overdue_only {
        title_spans.push(Span::raw("  ·  "));
        title_spans.push(Span::styled(
            "overdue",
            Style::default()
                .fg(app.theme.danger_bright)
                .add_modifier(app.theme.bold_modifier()),
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
            "No tasks scheduled or due today. Press `T` to leave the Today view."
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
        // ADR-0018 S3: insert density blank-line separators between groups.
        let items: Vec<ListItem> = app
            .rows
            .iter()
            .enumerate()
            .flat_map(|(i, r)| {
                let item = row_to_item(r, &app.today, &app.theme, app.group_by);
                if matches!(r, DisplayRow::Header { .. }) && i > 0 {
                    let mut gap: Vec<ListItem> = (0..app.layout.list_density)
                        .map(|_| ListItem::new(""))
                        .collect();
                    gap.push(item);
                    gap
                } else {
                    vec![item]
                }
            })
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
                Style::default()
                    .fg(app.theme.danger)
                    .add_modifier(app.theme.bold_modifier()),
            ),
            Span::styled(msg.clone(), Style::default().fg(app.theme.danger)),
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
            &app.theme,
            &app.layout,
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
            Span::styled(
                app.search_query.clone(),
                Style::default().fg(app.theme.success),
            ),
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
            Span::styled(
                app.file_query.clone(),
                Style::default().fg(app.theme.success),
            ),
            Span::styled(cursor, Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]))
        .style(Style::new())
    } else if app.quick_adding {
        // ADR-0014: quick-add modal prompt. Mirrors the search/file-search prompt
        // layout but uses a "+" prefix (the `➕` creation emoji used in the written
        // task line) and shows which inbox the write targets.
        let cursor = if (app.quick_add_query.len() as u16) < footer_area.width.saturating_sub(12) {
            "█"
        } else {
            ""
        };
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" + to {}  ", app.inbox_path),
                Style::default().fg(app.theme.accent),
            ),
            Span::styled(
                app.quick_add_query.clone(),
                Style::default().fg(app.theme.success),
            ),
            Span::styled(cursor, Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]))
        .style(Style::new())
    } else if app.moving {
        // ADR-0020: move-mode indicator. No text entry — just the live gesture help.
        Paragraph::new(Line::from(vec![
            Span::styled(
                " MOVE  ",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::REVERSED),
            ),
            Span::raw("  "),
            Span::styled("j/k", Style::default().fg(app.theme.warning)),
            Span::raw(" move  ·  "),
            Span::styled("Enter", Style::default().fg(app.theme.warning)),
            Span::raw(" place  ·  "),
            Span::styled("Esc", Style::default().fg(app.theme.warning)),
            Span::raw(" cancel "),
        ]))
        .style(Style::new())
    } else if app.adding_note {
        // ADR-0019: add-note modal prompt. Mirrors the quick-add layout but uses a
        // "note" label — the write targets the selected task's note, not the inbox.
        let cursor = if (app.note_query.len() as u16) < footer_area.width.saturating_sub(12) {
            "█"
        } else {
            ""
        };
        Paragraph::new(Line::from(vec![
            Span::styled(" note  ", Style::default().fg(app.theme.accent)),
            Span::styled(
                app.note_query.clone(),
                Style::default().fg(app.theme.success),
            ),
            Span::styled(cursor, Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]))
        .style(Style::new())
    } else {
        // Trimmed cheat-sheet: just the most-used gestures + the `? help`
        // affordance. The FULL keybinding list lives in the floating overlay
        // opened by `?` (see [`help_popup`]). Same yellow-keycap/DIM styling as
        // before so it reads as native; the footer's job is now "most common +
        // discover `?`", not "full list".
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled("j/k", Style::default().fg(app.theme.warning)),
            Span::raw(" move  ·  "),
            Span::styled("Space", Style::default().fg(app.theme.warning)),
            Span::raw(" toggle  ·  "),
            Span::styled("Enter", Style::default().fg(app.theme.warning)),
            Span::raw(" fold  ·  "),
            Span::styled("f", Style::default().fg(app.theme.warning)),
            Span::raw(" filter  ·  "),
            Span::styled("/", Style::default().fg(app.theme.warning)),
            Span::raw(" search  ·  "),
            Span::styled("?", Style::default().fg(app.theme.warning)),
            Span::raw(" help  ·  "),
            Span::styled("q", Style::default().fg(app.theme.warning)),
            Span::raw(" quit "),
        ]))
        .style(Style::new().add_modifier(Modifier::DIM))
    };
    frame.render_widget(footer, footer_area);

    // The help overlay is rendered LAST so it floats on top of everything else
    // (list, notice, footer). `Clear` wipes the cells beneath so the popup's
    // block reads cleanly against the underlying UI rather than blending into it.
    if app.show_help {
        // 95% tall: the full key list (grown by the `m` move-mode row, ADR-0020) is
        // 35 content lines + a border, so it needs the extra height to avoid
        // clipping the closing hint on a ~40-row terminal.
        let popup = popup_area(area, 70, 95);
        frame.render_widget(Clear, popup);
        // `Clear` resets the popup cells to the terminal default bg, so re-apply
        // the themed background there before drawing the help text.
        if app.theme.background != Color::Reset {
            frame.render_widget(
                Block::default().style(Style::default().bg(app.theme.background)),
                popup,
            );
        }
        frame.render_widget(help_popup(&app.theme), popup);
    }
}

/// Split a note path into `(dir_prefix, filename)` for Note-header styling.
///
/// The prefix includes its trailing `/` (e.g. `("Projects/Work/", "standup.md")`)
/// so the two parts concatenate back to the original. A root-level note with no
/// `/` returns `(None, whole_key)`. Used only for `GroupBy::FolderNote` headers, where
/// the prefix is dimmed (`theme.path_prefix`) so the filename pops.
fn split_note_header(key: &str) -> (Option<&str>, &str) {
    match key.rfind('/') {
        Some(i) => (Some(&key[..=i]), &key[i + 1..]),
        None => (None, key),
    }
}

/// Render one display row as a list item. Group headers are bold with a cyan
/// expand/collapse marker and dim counts; task rows are indented, with the checkbox
/// coloured by status, done tasks struck through, a yellow due date when present,
/// and a `⏳ <date>` scheduled-date suffix in cyan (bold/bright cyan when the
/// scheduled date is "today" — the ADR-0009 "this is a today task" affordance).
/// `today` is a `YYYY-MM-DD` string used only for the bold-today highlight.
/// `group_by` lets the Note-axis header dim its directory prefix (ADR-0018).
fn row_to_item(
    row: &DisplayRow,
    today: &str,
    theme: &theme::Theme,
    group_by: GroupBy,
) -> ListItem<'static> {
    match row {
        DisplayRow::Header {
            group_key,
            open_count,
            total_count,
            collapsed,
        } => {
            let marker = if *collapsed { "▸" } else { "▾" };
            let mut spans = vec![Span::styled(
                format!("{marker} "),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(theme.bold_modifier()),
            )];
            // ADR-0018: under Note grouping the key is a note path; dim the
            // directory prefix so the filename (bold/default) pops at a glance.
            // Other axes (Tag/Priority/Folder) render the key whole.
            match (group_by, split_note_header(group_key)) {
                (GroupBy::FolderNote, (Some(prefix), filename)) => {
                    spans.push(Span::styled(
                        prefix.to_string(),
                        Style::default().fg(theme.path_prefix),
                    ));
                    spans.push(Span::styled(
                        filename.to_string(),
                        Style::default().add_modifier(theme.bold_modifier()),
                    ));
                }
                _ => spans.push(Span::styled(
                    group_key.clone(),
                    Style::default().add_modifier(theme.bold_modifier()),
                )),
            }
            spans.push(Span::raw("   "));
            spans.push(Span::styled(
                format!("{open_count} open · {total_count} total"),
                Style::default().fg(theme.muted),
            ));
            ListItem::new(Line::from(spans))
        }
        DisplayRow::Task { task } => {
            let checkbox = format!("[{}]", task.raw_checkbox_char);
            let mut spans: Vec<Span> = Vec::with_capacity(8);
            // Conventional 4-space list indent under the header marker + the task's
            // source-line indentation, so subtasks render at proportional depth.
            spans.push(Span::raw(" ".repeat(4 + task.indent)));
            spans.push(Span::styled(checkbox, checkbox_style(&task.status, theme)));
            spans.push(Span::raw(format!(" {}", task.text)));
            if let Some(due) = &task.due_date {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("· {due}"),
                    Style::default().fg(theme.warning),
                ));
            }
            // ADR-0009 Phase 1: scheduled-date suffix. Parallel to (not replacing)
            // the yellow due-date above. Cyan normally; bold when == today.
            if let Some(sched) = &task.scheduled_date {
                let is_today = sched.as_str() == today;
                let style = if is_today {
                    Style::default()
                        .fg(theme.accent_bright)
                        .add_modifier(theme.bold_modifier())
                } else {
                    Style::default().fg(theme.scheduled)
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
fn checkbox_style(status: &Status, theme: &theme::Theme) -> Style {
    match status {
        Status::Open => Style::default().fg(theme.warning),
        Status::Done => Style::default().fg(theme.success),
        Status::InProgress => Style::default().fg(theme.accent),
        Status::Other(_) => Style::default().fg(theme.muted),
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

/// Expand tab characters to spaces using 4-column tab stops (matching
/// [`taski_core`]'s `count_indent` convention). Ratatui writes each character to a
/// single buffer cell, so a raw `\t` would occupy one invisible cell instead of
/// advancing to the next tab stop. Expanding before rendering makes tab-indented
/// subtasks visible in the context pane.
fn expand_tabs(s: &str) -> String {
    const TAB_WIDTH: usize = 4;
    let mut result = String::with_capacity(s.len());
    let mut col = 0usize;
    for c in s.chars() {
        if c == '\t' {
            let spaces = TAB_WIDTH - (col % TAB_WIDTH);
            result.push_str(&" ".repeat(spaces));
            col += spaces;
        } else {
            result.push(c);
            col += 1;
        }
    }
    result
}

/// Render the context pane into `area`: a bordered block titled with the note path,
/// showing a window of the note's content centered on `target_line` (or the top of the
/// note when the cursor is on a header), with a line-number gutter and the target line
/// highlighted. `scroll` shifts the window up/down from the auto-centered position.
/// Shows a graceful placeholder when content is unavailable.
#[allow(clippy::too_many_arguments)]
fn draw_context_pane(
    frame: &mut Frame,
    area: Rect,
    note_path: Option<&str>,
    content: Option<&NoteContent>,
    target_line: Option<usize>,
    scroll: i32,
    theme: &theme::Theme,
    prefs: &theme::LayoutPrefs,
) {
    let title = Line::from(vec![
        Span::raw(" Context — "),
        Span::styled(
            note_path.unwrap_or("(no note selected)"),
            Style::default()
                .fg(theme.accent)
                .add_modifier(theme.bold_modifier()),
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
                                .fg(theme.context_target)
                                .add_modifier(theme.bold_modifier())
                        } else {
                            Style::default()
                        };
                        Line::from(vec![
                            Span::styled(
                                format!("{:>width$} ", lineno, width = num_width),
                                Style::default().fg(theme.muted),
                            ),
                            Span::styled(if is_target { "▶ " } else { "  " }, style),
                            Span::styled(expand_tabs(raw), style),
                        ])
                    })
                    .collect()
            }
        }
    };

    let mut p = Paragraph::new(lines).block(block);
    if prefs.context_wrap {
        p = p.wrap(Wrap { trim: false });
    }
    frame.render_widget(p, area);
}

/// Whether `key` is one of the help-overlay dismissal keys (`?`, `Esc`, `q`).
/// Used by [`run_loop`] to decide when to close the modal help overlay. Kept as
/// a pure free function so the dismissal set is unit-testable in isolation from
/// the quit path (`Ctrl-C` is handled separately — it always remains able to
/// quit the app, never a dismissal key).
fn help_dismisses_on(key: KeyCode) -> bool {
    matches!(key, KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q'))
}

/// Centered popup `Rect` for floating overlays. `percent_x` / `percent_y` are
/// the popup's width / height as a percentage (0–100) of `area`; the rect is
/// centered on both axes. The well-known ratatui idiom: split the area into
/// [margin | content | margin] on each axis and take the middle chunk.
fn popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

/// The floating keybindings help overlay: a bordered `Paragraph` titled
/// "Keybindings" holding the full key list, grouped under bold cyan headers.
/// Two aligned columns — yellow keycap (left, padded to [`HELP_KEY_W`]
/// columns) and a raw description (right) — reusing the footer's visual
/// language so the overlay feels native. A blank line separates groups, and a
/// dim hint line at the bottom reminds the user how to dismiss. The keycap and
/// description wording is fixed copy.
fn help_popup(theme: &theme::Theme) -> Paragraph<'static> {
    /// Fixed column width (in chars) the keycap is left-padded to so the
    /// descriptions align into a clean right column. Wide enough for the longest
    /// keycap (`q / Esc / Ctrl-C`). Note: padding is by char count, so
    /// ambiguous-width glyphs (arrows, ⇧) may drift a column on terminals that
    /// render them 2-wide — acceptable for a personal TUI.
    const HELP_KEY_W: usize = 16;

    // Keycap colour, padded to the fixed column width (matches the footer).
    let key = |k: &'static str| {
        Span::styled(
            format!("{k:<w$}", w = HELP_KEY_W),
            Style::default().fg(theme.warning),
        )
    };
    // Bold accent group header — a distinct color from the keycaps so the
    // eye can scan groups quickly.
    let head = |h: &'static str| {
        Line::from(vec![Span::styled(
            h,
            Style::default()
                .fg(theme.accent)
                .add_modifier(theme.bold_modifier()),
        )])
    };
    // One aligned key/description row: leading indent, padded keycap, gap, desc.
    let row = |k: &'static str, d: &'static str| {
        Line::from(vec![Span::raw(" "), key(k), Span::raw("  "), Span::raw(d)])
    };

    let lines = vec![
        head("Navigation"),
        row("j/k ↑/↓", "Move selection up/down"),
        row("Enter", "Fold group"),
        row("←/→", "Collapse / expand group"),
        row("Tab / ⇧Tab", "Expand all / collapse all groups"),
        row("J / K", "Scroll context pane"),
        Line::raw(""),
        head("Filter & view"),
        row("f", "Cycle status filter (All / Open / Done)"),
        row("T", "Today view (scheduled or due == today)"),
        row("O", "Overdue view (due < today)"),
        row(
            "G",
            "Cycle group-by (folder+note / note / tag / priority / folder)",
        ),
        row("p", "Toggle context pane"),
        Line::raw(""),
        head("Task actions"),
        row("Space", "Toggle open ↔ done (stamps ✅)"),
        row("t", "Mark / unmark for today (⏳)"),
        row("b", "Toggle checkbox ↔ bullet"),
        row("d", "Cancel / un-cancel (stamps ❌)"),
        row("i", "Mark in-progress / re-open"),
        row("u", "Undo last flip / bullet / quick-add"),
        row("a", "Quick-add to inbox"),
        row("n", "Add a note to the task (links it)"),
        row(
            "m",
            "Move mode: reorder within note (Enter place / Esc cancel)",
        ),
        row("A", "Archive note's completed tasks (→ archive note)"),
        row("o", "Open in Obsidian"),
        Line::raw(""),
        head("Search"),
        row("/", "Text search (task body)"),
        row("F", "File / path search"),
        Line::raw(""),
        head("Other"),
        row("?", "Toggle this help"),
        row("q / Esc / Ctrl-C", "Quit"),
        Line::raw(""),
        Line::from(vec![Span::styled(
            " ? / Esc / q to close",
            Style::default().add_modifier(Modifier::DIM),
        )]),
    ];

    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Keybindings "),
    )
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
            indent: 0,
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

    #[test]
    fn obsidian_url_native_mode() {
        assert_eq!(
            obsidian_url("My Vault", "Inbox/note.md", 5, false),
            "obsidian://open?vault=My%20Vault&file=Inbox%2Fnote.md"
        );
    }

    #[test]
    fn obsidian_url_advanced_mode() {
        assert_eq!(
            obsidian_url("My Vault", "Inbox/note.md", 5, true),
            "obsidian://advanced-uri?vault=My%20Vault&filepath=Inbox%2Fnote.md&line=5"
        );
    }

    #[test]
    fn percent_encode_query_special_chars() {
        // Space -> %20, `/` -> %2F, `#` -> %23, `^` -> %5E, `é` -> %C3%A9 (UTF-8).
        assert_eq!(
            percent_encode_query("No#te^naïve.md"),
            "No%23te%5Ena%C3%AFve.md"
        );
    }

    #[test]
    fn obsidian_url_minimal_well_formed() {
        let url = obsidian_url("v", "f.md", 1, false);
        assert_eq!(url, "obsidian://open?vault=v&file=f.md");
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

    // ── ADR-0020 move mode ─────────────────────────────────────────────────

    /// Build an `App` with `n` open tasks (lines 1..=n) in one note, the group
    /// expanded, and the i-th task row selected. Returns the app ready for move
    /// mode. Rows are `[Header, Task1, Task2, …]`, so task row indices are 1-based.
    fn app_with_expanded_note(note: &str, n: usize, select_task_idx: usize) -> App {
        let mut app = App::new();
        app.tasks = (1..=n).map(|i| task(i as i64, " ", i, note)).collect();
        app.rebuild();
        app.expand_all();
        // Row 0 is the header; task rows follow in line order.
        app.state.select(Some(select_task_idx));
        app
    }

    /// Helper: the ids of the `Task` rows, top to bottom.
    fn task_row_ids(app: &App) -> Vec<i64> {
        app.rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Task { task } => Some(task.id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn move_task_bubbles_and_clamps() {
        // Select the first task (row 1); the run is rows [1, 4).
        let mut app = app_with_expanded_note("inbox.md", 3, 1);
        app.start_move();
        assert!(app.moving, "entered move mode on a flat single-note group");
        assert_eq!(task_row_ids(&app), vec![1, 2, 3]);

        // Up from the top task is clamped — no change.
        app.move_task(-1);
        assert_eq!(task_row_ids(&app), vec![1, 2, 3]);
        assert_eq!(app.state.selected(), Some(1));

        // Down swaps task 1 with task 2; selection follows the moved task.
        app.move_task(1);
        assert_eq!(task_row_ids(&app), vec![2, 1, 3]);
        assert_eq!(app.state.selected(), Some(2));

        // Down again swaps to the bottom; a further down is clamped.
        app.move_task(1);
        assert_eq!(task_row_ids(&app), vec![2, 3, 1]);
        app.move_task(1);
        assert_eq!(task_row_ids(&app), vec![2, 3, 1], "clamped at the bottom");
    }

    #[test]
    fn commit_move_enqueues_reorder_with_new_order() {
        let conn = db::open(":memory:").unwrap();
        let mut app = app_with_expanded_note("inbox.md", 3, 1);
        app.start_move();
        app.move_task(1); // 1 below 2 → order [2, 1, 3]
        app.commit_move(&conn);
        assert!(!app.moving, "commit exits move mode");

        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        let a = &pending[0];
        assert_eq!(a.action_type, "reorder");
        // New top-to-bottom order of the run's line numbers: task2(line2),
        // task1(line1), task3(line3).
        assert_eq!(a.payload.as_deref(), Some("2,1,3"));
        assert_eq!(a.task_id, 1, "anchor is the moved task");
        assert_eq!(a.note_path, "inbox.md");
    }

    #[test]
    fn commit_move_no_change_does_not_enqueue() {
        let conn = db::open(":memory:").unwrap();
        let mut app = app_with_expanded_note("inbox.md", 3, 1);
        app.start_move();
        app.move_task(-1); // clamped no-op
        app.commit_move(&conn);
        assert!(!app.moving);
        assert!(
            db::pending_actions(&conn).unwrap().is_empty(),
            "an unchanged order enqueues nothing"
        );
    }

    #[test]
    fn cancel_move_restores_original_order() {
        let mut app = app_with_expanded_note("inbox.md", 3, 1);
        app.start_move();
        app.move_task(1);
        app.move_task(1); // order now [2, 3, 1]
        assert_eq!(task_row_ids(&app), vec![2, 3, 1]);

        app.cancel_move();
        assert!(!app.moving, "cancel exits move mode");
        assert_eq!(task_row_ids(&app), vec![1, 2, 3], "original order restored");
        // The moved task (id 1) is re-selected at its restored position (row 1).
        assert_eq!(app.state.selected(), Some(1));
    }

    // ── ADR-0021 archive (A key) ───────────────────────────────────────────

    /// Build an `App` whose single note mixes statuses, expanded, with the first
    /// task row selected. `specs` is `(id, checkbox_char, line)` in line order.
    fn app_with_statuses(note: &str, specs: &[(i64, &str, usize)]) -> App {
        let mut app = App::new();
        app.tasks = specs
            .iter()
            .map(|&(id, raw, ln)| task(id, raw, ln, note))
            .collect();
        app.rebuild();
        app.expand_all();
        app.state.select(Some(1)); // first task row (row 0 is the header)
        app
    }

    #[test]
    fn archive_completed_enqueues_done_and_cancelled_only() {
        let conn = db::open(":memory:").unwrap();
        // open, done, in-progress, cancelled — only [x] and [-] should archive.
        let mut app = app_with_statuses(
            "task-inbox.md",
            &[(1, " ", 1), (2, "x", 2), (3, "/", 3), (4, "-", 4)],
        );
        app.archive_completed(&conn);

        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        let a = &pending[0];
        assert_eq!(a.action_type, "archive");
        assert_eq!(a.note_path, "task-inbox.md", "source note");
        // Lines 2 ([x]) and 4 ([-]) move; payload = "<archive>\t2,4".
        assert_eq!(a.payload.as_deref(), Some("task-archive.md\t2,4"));
        assert_eq!(a.task_id, 2, "anchor is the first completed task");
        assert_eq!(a.line_number, 2, "anchor line");
        assert!(app.notice.as_deref().unwrap().contains("Archiving 2"));
    }

    #[test]
    fn archive_completed_noop_when_nothing_completed() {
        let conn = db::open(":memory:").unwrap();
        let mut app = app_with_statuses("task-inbox.md", &[(1, " ", 1), (2, "/", 2)]);
        app.archive_completed(&conn);
        assert!(
            db::pending_actions(&conn).unwrap().is_empty(),
            "no completed tasks → nothing enqueued"
        );
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("No completed tasks")
        );
    }

    #[test]
    fn archive_completed_refused_on_note_with_subtasks() {
        let conn = db::open(":memory:").unwrap();
        let mut app = App::new();
        // An open parent (visible under the default Open filter, so it can be
        // selected) with a nested done child makes the note non-flat.
        let mut child = task(2, "x", 2, "task-inbox.md");
        child.indent = 4;
        app.tasks = vec![task(1, " ", 1, "task-inbox.md"), child];
        app.rebuild();
        app.expand_all();
        app.state.select(Some(1));

        app.archive_completed(&conn);
        assert!(
            db::pending_actions(&conn).unwrap().is_empty(),
            "flat-only: a note with subtasks refuses archive"
        );
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("flat task lists only")
        );
    }

    #[test]
    fn start_move_refused_on_note_with_subtasks() {
        let mut app = App::new();
        let mut child = task(2, " ", 2, "inbox.md");
        child.indent = 4; // a nested subtask
        app.tasks = vec![task(1, " ", 1, "inbox.md"), child];
        app.rebuild();
        app.expand_all();
        app.state.select(Some(1)); // first task row

        app.start_move();
        assert!(
            !app.moving,
            "flat-only: a note with subtasks refuses move mode"
        );
        assert!(app.notice.is_some(), "a notice explains the refusal");
    }

    #[test]
    fn start_move_noop_on_header() {
        let mut app = app_with_expanded_note("inbox.md", 2, 0); // row 0 is the header
        app.start_move();
        assert!(!app.moving, "no move mode when the cursor is on a header");
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
        );
        // Only alpha has an open task; beta is hidden under the Open filter.
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "alpha.md");
    }

    /// ADR-0016 follow-on: the `Open` status filter treats in-progress as active
    /// (not-done), so an in-progress task stays visible under Open (it is not
    /// pushed to `All` only) and counts toward the group's open count. A done-only
    /// group is still hidden under Open.
    #[test]
    fn build_view_open_filter_includes_in_progress() {
        let expanded = HashSet::new();

        // A group whose only task is in-progress stays visible under Open, and its
        // open count reflects it.
        let in_progress = vec![task(1, "/", 1, "alpha.md")];
        let rows = build_view(
            &in_progress,
            StatusFilter::Open,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::FolderNote,
        );
        assert!(
            !rows.is_empty(),
            "in-progress task is visible under Open filter"
        );
        let h = header(&rows[0]);
        assert_eq!(h.1, 1, "open_count includes the in-progress task");
        assert_eq!(h.2, 1);

        // A group with only a done task is still hidden under Open.
        let done = vec![task(2, "x", 1, "alpha.md")];
        let rows = build_view(
            &done,
            StatusFilter::Open,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::FolderNote,
        );
        assert!(rows.is_empty(), "done-only group is hidden under Open");
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
            GroupBy::FolderNote,
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

    /// ADR-0014: `submit_quick_add` enqueues a `quick_add` action carrying the
    /// inbox path + typed text (with sentinel `task_id = 0`), records
    /// `LastAction::QuickAdd` for undo, and exits the modal. End-to-end through
    /// the real DB queue.
    #[test]
    fn submit_quick_add_enqueues_and_records_last_action() {
        let conn = db::open(":memory:").unwrap();
        let mut app = App::new();
        app.inbox_path = "task-inbox.md".to_string();
        app.quick_add_query = "my new task".to_string();
        app.quick_adding = true;

        app.submit_quick_add(&conn);

        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1, "exactly one quick_add action enqueued");
        assert_eq!(pending[0].action_type, "quick_add");
        assert_eq!(pending[0].payload.as_deref(), Some("my new task"));
        assert_eq!(pending[0].note_path, "task-inbox.md");
        assert_eq!(pending[0].task_id, 0, "quick_add uses sentinel task_id = 0");

        // last_action recorded for undo.
        assert!(
            matches!(
                &app.last_action,
                Some(LastAction::QuickAdd { inbox_path, text })
                    if inbox_path == "task-inbox.md" && text == "my new task"
            ),
            "last_action must be QuickAdd with the inbox path + text"
        );

        // Modal exited + query cleared.
        assert!(!app.quick_adding, "modal must exit after submit");
        assert!(
            app.quick_add_query.is_empty(),
            "query must be cleared after submit"
        );
    }

    /// ADR-0014: whitespace-only text is a no-op — no action enqueued, but the
    /// modal still exits (so `a` + spaces + Enter dismisses the modal cleanly).
    #[test]
    fn quick_add_empty_text_is_noop() {
        let conn = db::open(":memory:").unwrap();
        let mut app = App::new();
        app.quick_add_query = "   ".to_string();
        app.quick_adding = true;

        app.submit_quick_add(&conn);

        assert!(
            db::pending_actions(&conn).unwrap().is_empty(),
            "whitespace-only text must not enqueue anything"
        );
        assert!(!app.quick_adding, "modal must still exit on empty text");
    }

    /// ADR-0014: `u` after a quick-add enqueues a `quick_add_undo` action (a
    /// separate action_type, dispatched separately by the daemon). This is the
    /// quick-add sibling of `submit_undo_after_toggle_enqueues_reversed_flip`.
    #[test]
    fn submit_undo_after_quick_add_enqueues_quick_add_undo() {
        let conn = db::open(":memory:").unwrap();
        let mut app = App::new();
        app.last_action = Some(LastAction::QuickAdd {
            inbox_path: "task-inbox.md".into(),
            text: "undo me".into(),
        });

        app.submit_undo(&conn);

        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "exactly one quick_add_undo action enqueued"
        );
        assert_eq!(pending[0].action_type, "quick_add_undo");
        assert_eq!(pending[0].payload.as_deref(), Some("undo me"));
        assert_eq!(pending[0].note_path, "task-inbox.md");
    }

    /// ADR-0014 Issue 1 lockdown: a failed quick_add surfaces a notice worded
    /// for the quick-add gesture ("Quick add not applied — … Press a to try
    /// again"), NOT the done-toggle wording ("Toggle … Press Space"). Follows
    /// the `failed_cancel_surfaces_cancel_notice` end-to-end pattern.
    #[test]
    fn failed_quick_add_surfaces_quick_add_notice() {
        let conn = db::open(":memory:").unwrap();
        let mut app = App::new();
        app.inbox_path = "task-inbox.md".to_string();
        app.quick_add_query = "my task".to_string();
        app.quick_adding = true;
        app.submit_quick_add(&conn);
        let id = db::pending_actions(&conn).unwrap()[0].id;

        // Daemon refuses it: the inbox changed externally since the action was
        // enqueued (the TOCTOU conflict path).
        db::resolve_action(
            &conn,
            id,
            "failed",
            Some("note changed externally since scan; quick_add not applied"),
        )
        .unwrap();
        app.refresh(&conn).unwrap();

        let notice = app
            .notice
            .as_deref()
            .expect("quick_add failure should surface a notice");
        assert!(
            notice.starts_with("Quick add not applied"),
            "quick_add notice should be worded for the quick-add gesture: {notice}"
        );
        assert!(
            notice.contains("task-inbox.md"),
            "notice should name the inbox path"
        );
        assert!(
            notice.contains("Press a"),
            "notice should hint the `a` retry key, not Space: {notice}"
        );
        assert!(
            !notice.contains("Space"),
            "quick_add notice must NOT hint the done-toggle retry key: {notice}"
        );
        assert!(
            !notice.contains("Toggle"),
            "quick_add notice must NOT carry the done-toggle verb: {notice}"
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
    fn cancel_target_char_maps_others_to_cancelled_and_reopens_cancelled() {
        // ADR-0013: `d` targets the cancelled char (`-`) from any non-cancelled
        // state, and re-opens (` `) a cancelled task. The mirror of
        // `toggle_target_char` on the cancel axis.
        assert_eq!(cancel_target_char(" "), "-"); // open -> cancelled
        assert_eq!(cancel_target_char("x"), "-"); // done -> cancelled
        assert_eq!(cancel_target_char("X"), "-"); // done -> cancelled
        assert_eq!(cancel_target_char("/"), "-"); // in-progress -> cancelled
        assert_eq!(cancel_target_char(">"), "-"); // forwarded -> cancelled
        assert_eq!(cancel_target_char("-"), " "); // cancelled -> re-open

        // Document the invariant that makes `render_failure_notice`'s
        // `new_char == "-"` seam robust: `toggle_target_char` (the `Space`
        // done-toggle path) NEVER produces `"-"`. So a failed checkbox action
        // with `new_char == "-"` is unambiguously a cancel gesture, never a
        // done-toggle. If this invariant ever breaks, the failure-notice retry
        // key for a refused done-toggle would wrongly say "Press d".
        for raw in [" ", "x", "X", "/", ">", "-", "!", "_"] {
            assert_ne!(
                toggle_target_char(raw),
                "-",
                "toggle_target_char must never target the cancel char (would break \
                 render_failure_notice's new_char == \"-\" seam); raw={raw:?}"
            );
        }

        // ADR-0016: document the parallel invariant that makes
        // `render_failure_notice`'s `new_char == "/"` seam robust:
        // `toggle_target_char` (the `Space` done-toggle path) NEVER produces
        // `"/"`. So a failed checkbox action with `new_char == "/"` is
        // unambiguously an in-progress gesture, never a done-toggle. If this
        // invariant ever breaks, the failure-notice retry key for a refused
        // done-toggle would wrongly say "Press i".
        for raw in [" ", "x", "X", "/", ">", "-", "!", "_"] {
            assert_ne!(
                toggle_target_char(raw),
                "/",
                "toggle_target_char must never target the in-progress char (would \
                 break render_failure_notice's new_char == \"/\" seam); raw={raw:?}"
            );
        }
    }

    /// ADR-0016: `i` targets the in-progress char (`/`) from any non-in-progress
    /// state, and re-opens (` `) an in-progress task. The mirror of
    /// `cancel_target_char` on the in-progress axis.
    #[test]
    fn in_progress_target_char_maps_others_to_in_progress_and_reopens_in_progress() {
        assert_eq!(in_progress_target_char(" "), "/"); // open -> in-progress
        assert_eq!(in_progress_target_char("x"), "/"); // done -> in-progress
        assert_eq!(in_progress_target_char("X"), "/"); // done -> in-progress
        assert_eq!(in_progress_target_char("-"), "/"); // cancelled -> in-progress
        assert_eq!(in_progress_target_char(">"), "/"); // forwarded -> in-progress
        assert_eq!(in_progress_target_char("/"), " "); // in-progress -> re-open
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
        // ADR-0012: the done-date unparseable phrase shares "malformed or
        // unparseable" with the scheduled/cancelled phrases — distinguish by the
        // ✅ glyph. (Without Issue 1's fix this would wrongly map to the scheduled
        // wording.)
        assert_eq!(
            friendly_failure_reason("existing ✅ is malformed or unparseable; toggle not applied"),
            "the done date on this line couldn't be parsed",
        );
        // ADR-0013: the cancelled-date unparseable phrase — distinguish by the ❌
        // glyph.
        assert_eq!(
            friendly_failure_reason("existing ❌ is malformed or unparseable; cancel not applied"),
            "the cancelled date on this line couldn't be parsed",
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

    /// ADR-0013: a refused cancel action (a `checkbox` row with `new_char='-'`)
    /// surfaces a notice worded for the cancel gesture ("Cancel not applied")
    /// that hints the `d` retry key (not `Space`), carrying the plain cancelled-
    /// date reason and the note name. This locks down (a) `render_failure_notice`'s
    /// `new_char == "-"` → `("Cancel", "d")` seam and (b) Issue 1's fix that the
    /// ❌ unparseable phrase maps to the cancelled-date wording (not the scheduled
    /// wording the old substring collision produced).
    #[test]
    fn failed_cancel_surfaces_cancel_notice() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1)); // on the task
        app.submit_cancel(&conn);
        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].action_type, "checkbox");
        assert_eq!(pending[0].new_char, "-", "cancel enqueues a flip to '-'");

        // Daemon refuses it: the line's existing ❌ is malformed/unparseable.
        db::resolve_action(
            &conn,
            pending[0].id,
            "failed",
            Some("existing ❌ is malformed or unparseable; cancel not applied"),
        )
        .unwrap();
        app.refresh(&conn).unwrap();

        let notice = app
            .notice
            .as_deref()
            .expect("cancel failure should surface a notice");
        assert!(
            notice.starts_with("Cancel not applied"),
            "cancel notice should be worded for the cancel gesture: {notice}"
        );
        assert!(notice.contains("alpha.md"), "notice should name the note");
        assert!(
            notice.contains("the cancelled date on this line couldn't be parsed"),
            "notice should carry the cancelled-date reason (Issue 1 fix): {notice}"
        );
        assert!(
            !notice.contains("the scheduled date"),
            "cancel notice must NOT carry the scheduled-date wording (the old \
             substring-collision bug): {notice}"
        );
        assert!(
            notice.contains("Press d"),
            "notice should hint the `d` retry key, not Space: {notice}"
        );
        assert!(
            !notice.contains("Space"),
            "cancel notice must NOT hint the done-toggle retry key: {notice}"
        );
    }

    /// ADR-0016: `submit_in_progress` enqueues a `checkbox` action row with
    /// `new_char == "/"` for the cursor task, and records `LastAction::CheckboxToggle`
    /// so `u` can undo it. Mirrors the DB + App setup used by the cancel enqueue
    /// path (`failed_cancel_surfaces_cancel_notice` above).
    #[test]
    fn submit_in_progress_enqueues_flip_to_in_progress_char() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1)); // on the open task
        app.submit_in_progress(&conn);

        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1, "one in-progress action enqueued");
        let p = &pending[0];
        assert_eq!(p.task_id, 1);
        assert_eq!(p.new_char, "/", "in-progress enqueues a flip to '/'");
        assert_eq!(p.expected_char, " ", "expected_char is the open char");
        assert_eq!(p.action_type, "checkbox", "reuses the checkbox action_type");
        assert!(p.error.is_none());

        // The action was tracked for resolution surfacing.
        assert_eq!(app.pending_session_actions, vec![p.id]);

        // LastAction recorded so `u` can undo it; the new_char is "/".
        match &app.last_action {
            Some(LastAction::CheckboxToggle { new_char, .. }) => {
                assert_eq!(new_char, "/", "undo record carries the in-progress char");
            }
            other => panic!("expected CheckboxToggle last_action, got {other:?}"),
        }
    }

    /// ADR-0016: a refused in-progress action (a `checkbox` row with
    /// `new_char='/'`) surfaces a notice worded for the in-progress gesture
    /// ("Mark in-progress not applied") that hints the `i` retry key (not
    /// `Space`, not `d`), carrying the plain reason and the note name. Mirrors
    /// `failed_cancel_surfaces_cancel_notice` assertion-for-assertion.
    #[test]
    fn failed_in_progress_surfaces_in_progress_notice() {
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "alpha.md")).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.expanded.insert("alpha.md".to_string());
        app.rebuild();
        app.state.select(Some(1)); // on the task
        app.submit_in_progress(&conn);
        let pending = db::pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].action_type, "checkbox");
        assert_eq!(
            pending[0].new_char, "/",
            "in-progress enqueues a flip to '/'"
        );

        // Daemon refuses it: the note changed externally.
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
            .expect("in-progress failure should surface a notice");
        assert!(
            notice.starts_with("Mark in-progress not applied"),
            "in-progress notice should be worded for the in-progress gesture: {notice}"
        );
        assert!(notice.contains("alpha.md"), "notice should name the note");
        assert!(
            notice.contains("Press i to try again"),
            "notice should hint the `i` retry key: {notice}"
        );
        assert!(
            !notice.contains("Cancel"),
            "in-progress notice must NOT carry the cancel wording: {notice}"
        );
        assert!(
            !notice.contains("Space"),
            "in-progress notice must NOT hint the done-toggle retry key: {notice}"
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

    /// `expand_tabs` advances to the next 4-column tab stop, matching
    /// `count_indent`'s convention. Leading tabs (the common case for subtask
    /// indentation) expand to 4 spaces; mid-line tabs advance relative to the
    /// current column.
    #[test]
    fn expand_tabs_advances_to_tab_stops() {
        // No tabs — unchanged.
        assert_eq!(expand_tabs("- [ ] plain task"), "- [ ] plain task");
        // Leading tab → 4 spaces (column 0 → next stop at 4).
        assert_eq!(expand_tabs("\t- [ ] subtask"), "    - [ ] subtask");
        // Two leading tabs → 8 spaces.
        assert_eq!(expand_tabs("\t\t- [ ] deep"), "        - [ ] deep");
        // Column 2 + tab → advances to column 4 (2 spaces).
        assert_eq!(expand_tabs("  \t- [ ] mixed"), "    - [ ] mixed");
        // Column 5 + tab → advances to column 8 (3 spaces).
        assert_eq!(expand_tabs("text1\trest"), "text1   rest");
        // Empty string.
        assert_eq!(expand_tabs(""), "");
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

    // --- ADR-0009 Phase 1 (widened by ADR-0022): "Today" view (scheduled_date == today OR due_date == today) ---

    /// With `today_only` on, `build_view` keeps only tasks whose `scheduled_date`
    /// OR `due_date` matches `today` (ADR-0022 widened this from scheduled-only),
    /// across notes; tasks with neither matching date drop out. (This case only
    /// exercises scheduled dates.)
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "alpha.md");
    }

    /// ADR-0022: a task whose `due_date == today` (and has no scheduled date) is kept
    /// by the Today view; tasks due tomorrow, due yesterday (overdue, a separate `O`
    /// axis), or with no dates are dropped.
    #[test]
    fn today_only_also_keeps_due_today_tasks() {
        let tasks = vec![
            task_with_due(1, " ", 1, "alpha.md", "2026-06-20"), // due today -> kept
            task_with_due(2, " ", 1, "beta.md", "2026-06-21"),  // due tomorrow -> dropped
            task_with_due(3, " ", 1, "gamma.md", "2026-06-19"), // overdue -> dropped (not today)
            task(4, " ", 1, "delta.md"),                        // no dates -> dropped
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
            GroupBy::FolderNote,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(header(&rows[0]).0, "alpha.md");
    }

    /// ADR-0022: scheduled-today and due-today tasks both surface under Today, even in
    /// the same note; a task with neither date is dropped.
    #[test]
    fn today_only_keeps_both_scheduled_today_and_due_today() {
        let tasks = vec![
            task_with_scheduled(1, " ", 1, "alpha.md", "2026-06-20"), // scheduled today
            task_with_due(2, " ", 2, "alpha.md", "2026-06-20"),       // due today
            task(3, " ", 3, "alpha.md"),                              // neither
        ];
        let expanded = HashSet::from(["alpha.md".to_string()]);
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            true,
            "2026-06-20",
            "",
            "",
            false,
            GroupBy::FolderNote,
        );
        assert_eq!(rows.len(), 3, "header + the two today tasks");
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Task { task } => Some(task.id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![1, 2]);
    }

    /// ADR-0022: a single task carrying BOTH `scheduled == today` AND `due == today`
    /// appears exactly once under Today (one row, one bucket — never double-counted,
    /// even though it satisfies `matches_today` via two clauses).
    #[test]
    fn today_only_single_task_with_both_dates_appears_once() {
        let mut t = task_with_scheduled(1, " ", 1, "alpha.md", "2026-06-20");
        t.due_date = Some("2026-06-20".to_string());
        let tasks = vec![t, task(2, " ", 2, "alpha.md")];
        let expanded = HashSet::from(["alpha.md".to_string()]);
        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            true,
            "2026-06-20",
            "",
            "",
            false,
            GroupBy::FolderNote,
        );
        // header + exactly one task row (id 1); not duplicated by matching twice.
        assert_eq!(rows.len(), 2);
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Task { task } => Some(task.id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![1]);
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

    // --- Note-header path/filename split (ADR-0018) ------------------------

    /// `split_note_header` peels the directory prefix (with trailing `/`) from
    /// the filename; a root-level note has no prefix.
    #[test]
    fn split_note_header_cases() {
        assert_eq!(
            split_note_header("Projects/Work/standup.md"),
            (Some("Projects/Work/"), "standup.md")
        );
        assert_eq!(
            split_note_header("inbox/today.md"),
            (Some("inbox/"), "today.md")
        );
        // Root-level note: no prefix.
        assert_eq!(split_note_header("standup.md"), (None, "standup.md"));
        // Trailing slash (defensive): empty filename, whole thing is prefix.
        assert_eq!(split_note_header("a/b/"), (Some("a/b/"), ""));
    }

    /// Under Note grouping, the directory prefix renders dimmed (`path_prefix`)
    /// while the filename keeps the default (brighter) fg, so the filename pops
    /// by color contrast alone — bold is off by default (global `bold` toggle).
    /// Other axes are unaffected (covered implicitly — the split is Note-only).
    #[test]
    fn note_header_dims_dir_prefix() {
        use ratatui::backend::TestBackend;
        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "inbox/today.md")).unwrap();

        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        // Hide the context pane so the note path appears exactly once (the list
        // header) — the pane title also renders the path, in the same accent.
        app.pane_visible = false;
        app.rebuild();

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let cells: Vec<&ratatui::buffer::Cell> = buf.content.iter().collect();

        // Locate the contiguous run of cells spelling the note path in the header.
        let needle: Vec<char> = "inbox/today.md".chars().collect();
        let start = (0..cells.len())
            .find(|&i| {
                needle.iter().enumerate().all(|(k, ch)| {
                    cells
                        .get(i + k)
                        .is_some_and(|c| c.symbol() == ch.to_string())
                })
            })
            .expect("list header must render");

        // "inbox/" (6 chars) is the dimmed prefix; "today.md" is the filename.
        use ratatui::style::Color;
        let prefix_cell = cells[start]; // 'i'
        let filename_cell = cells[start + 6]; // 't'
        assert_eq!(
            prefix_cell.fg,
            Color::DarkGray,
            "dir prefix must use path_prefix (DarkGray default)"
        );
        assert_ne!(
            filename_cell.fg,
            Color::DarkGray,
            "filename must not be dimmed"
        );
        assert!(
            !filename_cell.modifier.contains(Modifier::BOLD),
            "filename must not be bold by default (global bold toggle is off)"
        );
    }

    /// End-to-end: a user-configured `[theme]` color must reach the actual
    /// rendered cells. Exercises the full chain `ThemeConfig` →
    /// `Theme::resolve_from` → `app.theme` → `draw`, then asserts the title-bar
    /// filter label (an `accent`-styled surface) carries the custom color rather
    /// than the compiled default (`Cyan`).
    #[test]
    fn custom_theme_color_reaches_rendered_cells() {
        use ratatui::backend::TestBackend;
        use ratatui::style::Color;

        // A distinctive accent unlikely to collide with any default named color.
        let cfg = taski_config::ThemeConfig {
            accent: Some("#ff00ff".into()),
            ..taski_config::ThemeConfig::default()
        };
        let resolved = theme::Theme::resolve_from(Some(&cfg));
        assert_eq!(
            resolved.accent,
            Color::Rgb(255, 0, 255),
            "resolve must parse the configured hex"
        );

        let conn = db::open(":memory:").unwrap();
        db::upsert_task(&conn, &task(1, " ", 1, "a.md")).unwrap();
        let mut app = App::new();
        app.filter = StatusFilter::All;
        app.refresh(&conn).unwrap();
        app.theme = resolved; // what run_loop does with the config-resolved theme
        app.rebuild();

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let cells: Vec<&ratatui::buffer::Cell> = buf.content.iter().collect();

        // The title bar renders `filter: <label>` styled with theme.accent.
        let needle: Vec<char> = "filter:".chars().collect();
        let start = (0..cells.len())
            .find(|&i| {
                needle.iter().enumerate().all(|(k, ch)| {
                    cells
                        .get(i + k)
                        .is_some_and(|c| c.symbol() == ch.to_string())
                })
            })
            .expect("title bar filter label must render");
        assert_eq!(
            cells[start].fg,
            Color::Rgb(255, 0, 255),
            "the configured accent color must reach the rendered cell"
        );
    }

    /// A configured `background` must fill every cell — including cells no widget
    /// writes text into — while foreground colors are untouched. With the default
    /// (`Reset`) background, `draw` must paint nothing (cells keep `Reset` bg).
    #[test]
    fn theme_background_fills_the_screen() {
        use ratatui::backend::TestBackend;

        let render_bg = |bg: Color| {
            let conn = db::open(":memory:").unwrap();
            db::upsert_task(&conn, &task(1, " ", 1, "a.md")).unwrap();
            let mut app = App::new();
            app.filter = StatusFilter::All;
            app.refresh(&conn).unwrap();
            app.theme.background = bg;
            app.rebuild();
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            terminal.backend().buffer().clone()
        };

        // Custom background: every cell's bg is the configured color.
        let nord_bg = Color::Rgb(0x2e, 0x34, 0x40);
        let buf = render_bg(nord_bg);
        assert!(
            buf.content.iter().all(|c| c.bg == nord_bg),
            "a configured background must fill all cells (incl. blank ones)"
        );

        // Default (Reset) background: draw paints no bg, so cells stay Reset.
        let buf_default = render_bg(Color::Reset);
        assert!(
            buf_default.content.iter().all(|c| c.bg == Color::Reset),
            "default (Reset) background must leave cells unpainted"
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
        );
        assert_eq!(rows.len(), 2, "header + one done past-due task");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 2));
    }

    /// `overdue_only` is orthogonal to `today_only`: both on → tasks that are BOTH
    /// overdue (`due < today`) AND today-matching (`scheduled == today` OR `due == today`,
    /// per ADR-0022). Those two due-axis conditions are disjoint, so the AND can only
    /// resolve via scheduled == today — as this fixture exercises (task 1 is past-due
    /// + scheduled-today).
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
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
            GroupBy::FolderNote,
        );
        assert_eq!(rows.len(), 2, "header + one task");
        assert!(matches!(&rows[1], DisplayRow::Task { task } if task.id == 1));
    }

    // ── Group-by axis (G key) tests ──────────────────────────────────

    /// `group_keys` returns exactly one key for the Note axis.
    #[test]
    fn group_keys_note_axis_returns_note_path() {
        let t = task(1, " ", 1, "dir/note.md");
        let keys = group_keys(&t, GroupBy::FolderNote);
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

    /// `GroupBy::next` cycles FolderNote → Note → Tag → Priority → Folder →
    /// FolderNote.
    #[test]
    fn group_by_cycles_through_all_axes() {
        assert_eq!(GroupBy::FolderNote.next(), GroupBy::Note);
        assert_eq!(GroupBy::Note.next(), GroupBy::Tag);
        assert_eq!(GroupBy::Tag.next(), GroupBy::Priority);
        assert_eq!(GroupBy::Priority.next(), GroupBy::Folder);
        assert_eq!(GroupBy::Folder.next(), GroupBy::FolderNote);
    }

    /// `GroupBy::label` returns the short title-bar string.
    #[test]
    fn group_by_labels() {
        assert_eq!(GroupBy::FolderNote.label(), "folder+note");
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
        assert_eq!(app.group_by, GroupBy::FolderNote);
        assert_eq!(header(&app.rows[0]).0, "a.md");

        // FolderNote → Note: still one group, now keyed on the filename.
        app.cycle_group_by();
        assert_eq!(app.group_by, GroupBy::Note);
        assert_eq!(header(&app.rows[0]).0, "a.md");

        // Note → Tag.
        app.cycle_group_by();
        assert_eq!(app.group_by, GroupBy::Tag);
        assert_eq!(header(&app.rows[0]).0, "work");
    }

    /// `cycle_group_by` wraps around from Folder back to FolderNote.
    #[test]
    fn cycle_group_by_wraps_around() {
        let mut app = App::new();
        app.group_by = GroupBy::Folder;
        app.tasks = vec![task(1, " ", 1, "a.md")];
        app.rebuild();
        app.cycle_group_by();
        assert_eq!(app.group_by, GroupBy::FolderNote);
    }

    /// Cycling the axis does NOT clear `expanded` — stale keys naturally don't match.
    #[test]
    fn cycle_group_by_does_not_clear_expanded() {
        let mut app = App::new();
        // A sub-folder note so the FolderNote key ("sub/a.md") differs from the
        // Note key ("a.md") — otherwise a root note's two keys coincide and the
        // group would (correctly) stay expanded across the cycle.
        app.tasks = vec![task(1, " ", 1, "sub/a.md")];
        app.expanded.insert("sub/a.md".to_string());
        app.rebuild();
        assert!(app.expanded.contains("sub/a.md"));

        app.cycle_group_by(); // FolderNote → Note
        // expanded set still has the old full-path key, but it doesn't match the
        // filename-only key "a.md", so the new group starts collapsed.
        assert!(app.expanded.contains("sub/a.md"));
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

    /// Default `App` starts with `GroupBy::FolderNote`.
    #[test]
    fn app_default_group_by_is_folder_note() {
        let app = App::new();
        assert_eq!(app.group_by, GroupBy::FolderNote);
    }

    /// `filename_of` strips directories; root notes return themselves.
    #[test]
    fn filename_of_strips_directories() {
        assert_eq!(filename_of("Projects/Work/standup.md"), "standup.md");
        assert_eq!(filename_of("inbox.md"), "inbox.md");
    }

    /// The `Note` axis keys on the filename alone, so identically-named notes in
    /// different folders collapse into a single group (vs `FolderNote`, which
    /// keys on the full path and keeps them separate).
    #[test]
    fn note_axis_merges_same_filename_across_folders() {
        let tasks = vec![
            task(1, " ", 1, "Work/standup.md"),
            task(2, " ", 1, "Personal/standup.md"),
        ];
        // FolderNote: two distinct full-path groups.
        let fk1 = group_keys(&tasks[0], GroupBy::FolderNote);
        let fk2 = group_keys(&tasks[1], GroupBy::FolderNote);
        assert_ne!(fk1, fk2);
        // Note: both collapse to the same "standup.md" key.
        assert_eq!(group_keys(&tasks[0], GroupBy::Note), vec!["standup.md"]);
        assert_eq!(group_keys(&tasks[1], GroupBy::Note), vec!["standup.md"]);

        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &HashSet::new(),
            false,
            "",
            "",
            "",
            false,
            GroupBy::Note,
        );
        // Exactly one header (the merged "standup.md" group), counting both tasks.
        let headers: Vec<_> = rows
            .iter()
            .filter(|r| matches!(r, DisplayRow::Header { .. }))
            .collect();
        assert_eq!(headers.len(), 1);
        let (key, _open, total, _collapsed) = header(headers[0]);
        assert_eq!(key, "standup.md");
        assert_eq!(total, 2);
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

    /// Tasks with different `indent` values flow through `build_view` unchanged so
    /// `row_to_item` can render proportional subtask indentation.
    #[test]
    fn build_view_preserves_task_indent() {
        let mut t1 = task(1, " ", 1, "a.md");
        t1.indent = 0;
        let mut t2 = task(2, " ", 2, "a.md");
        t2.indent = 2;
        let mut t3 = task(3, " ", 3, "a.md");
        t3.indent = 4;
        let tasks = vec![t1, t2, t3];
        let expanded = HashSet::from(["a.md".to_string()]);

        let rows = build_view(
            &tasks,
            StatusFilter::All,
            &expanded,
            false,
            "",
            "",
            "",
            false,
            GroupBy::FolderNote,
        );
        assert_eq!(rows.len(), 4, "header + three tasks");
        let indents: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                DisplayRow::Task { task } => Some(task.indent),
                _ => None,
            })
            .collect();
        assert_eq!(indents, vec![0, 2, 4]);
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

    // --- `?` keybindings help overlay -------------------------------------

    /// `toggle_help` flips the `show_help` flag (mirrors how `toggle_pane` /
    /// `toggle_today` are unit-tested).
    #[test]
    fn toggle_help_flips_flag() {
        let mut app = App::new();
        assert!(!app.show_help, "help is hidden by default");
        app.toggle_help();
        assert!(app.show_help);
        app.toggle_help();
        assert!(!app.show_help, "toggling again hides it");
    }

    /// The help-overlay dismissal set is exactly `?`, `Esc`, `q` — and NOT
    /// arbitrary other keys (those are swallowed while help is open). This is
    /// the pure decision function [`run_loop`] consults; Ctrl-C is handled
    /// separately (it quits) and is correctly NOT a dismissal key here.
    #[test]
    fn help_dismiss_keys() {
        assert!(help_dismisses_on(KeyCode::Char('?')));
        assert!(help_dismisses_on(KeyCode::Esc));
        assert!(help_dismisses_on(KeyCode::Char('q')));
        // Non-dismiss keys are swallowed (do NOT close help).
        assert!(!help_dismisses_on(KeyCode::Char('j')));
        assert!(!help_dismisses_on(KeyCode::Char(' ')));
        assert!(!help_dismisses_on(KeyCode::Enter));
        assert!(
            !help_dismisses_on(KeyCode::Char('c')),
            "Ctrl-C quits, not dismisses"
        );
    }

    /// Headless render smoke (TestBackend): with `show_help` on, `draw` renders
    /// the floating "Keybindings" overlay ON TOP of everything else (mirrors
    /// the `draw_renders_context_pane_without_panic` style). Proves the popup
    /// block title and at least one keycap land in the buffer without panicking.
    #[test]
    fn draw_renders_help_overlay_when_show_help() {
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        app.show_help = true;

        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        // Must not panic.
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            rendered.contains("Keybindings"),
            "the help overlay title should render on top"
        );
        assert!(
            rendered.contains("Space"),
            "a keycap from the key list should render"
        );
        assert!(
            rendered.contains("Navigation"),
            "a group header should render"
        );
        assert!(
            rendered.contains("to close"),
            "the dim dismissal hint should render"
        );
    }
}
