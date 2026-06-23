//! SQLite storage layer for Taski: owns the canonical schema and exposes read/write
//! APIs over the shared `tasks` index.
//!
//! The DB is opened in WAL mode with `synchronous=NORMAL`, the standard one-writer +
//! many-readers configuration that lets the daemon (writer) and the TUI (reader)
//! operate against the same file across processes — see ADR-0001.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Context;
use rusqlite::Connection;

// Re-export the shared domain types so downstream crates (e.g. the TUI) can depend on
// `taski-db` alone without a direct `taski-core` dependency. This also brings `Status`
// and `Task` into scope within this module. `ymd_from_unix` is re-exported for the
// TUI's "today" derivation (ADR-0009 Phase 1).
pub use taski_core::{Priority, Status, Task, ymd_from_unix};

/// The canonical schema v7. v2 added surrogate rowid identity + content-hash
/// reconciliation (ADR-0005); v3 added the `note_contents` cache that backs the
/// read-only TUI context pane (ADR-0006); v4 added `tasks.scheduled_date` for the
/// Obsidian Tasks-plugin `⏳` read path (ADR-0009 Phase 1); v5 extended
/// `pending_actions` with `action_type`/`payload` so the same queue carries both
/// checkbox flips and `set_scheduled` writes (ADR-0009 Phase 2); v6 added the
/// Tier 1 read-only metadata columns to `tasks` — `tags`, `priority`,
/// `start_date`, `created_date`, `done_date`, `cancelled_date` — parsed from
/// Obsidian Tasks-plugin inline syntax (no vault writes; read path only);
/// v7 added `tasks.indent` — the leading-whitespace column of the source line,
/// capturing subtask nesting depth for visual indentation in the TUI.
/// Created with `IF NOT EXISTS` so [`ensure_schema`] is idempotent.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  note_path         TEXT NOT NULL,
  line_number       INTEGER NOT NULL,
  indent            INTEGER NOT NULL DEFAULT 0,
  text              TEXT NOT NULL,
  text_hash         TEXT NOT NULL,
  status            TEXT NOT NULL,
  raw_checkbox_char TEXT NOT NULL,
  note_hash         TEXT,
  note_mtime        INTEGER,
  due_date          TEXT,
  scheduled_date    TEXT,
  tags              TEXT NOT NULL DEFAULT '',
  priority          TEXT,
  start_date        TEXT,
  created_date      TEXT,
  done_date         TEXT,
  cancelled_date    TEXT,
  updated_at        INTEGER NOT NULL
);

-- PRD §8 / ADR-0002: the queue the TUI writes action requests into and the daemon
-- (sole vault writer) drains. A row's lifecycle is pending -> done | failed.
CREATE TABLE IF NOT EXISTS pending_actions (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id           INTEGER NOT NULL,
  note_path         TEXT NOT NULL,
  line_number       INTEGER NOT NULL,
  expected_char     TEXT NOT NULL,   -- raw_checkbox_char the user saw (byte verify); unused for non-checkbox action types
  new_char          TEXT NOT NULL,   -- desired checkbox char after flip; unused for non-checkbox action types
  state             TEXT NOT NULL DEFAULT 'pending',  -- pending | done | failed
  created_at        INTEGER NOT NULL,
  resolved_at       INTEGER,
  error             TEXT,
  -- ADR-0009 Phase 2: dispatch key + payload so one queue serves every action kind.
  -- 'checkbox' rows carry payload=NULL (expected/new_char hold the flip); a new
  -- 'set_scheduled' row carries payload = the desired YYYY-MM-DD (or NULL to unmark).
  action_type       TEXT NOT NULL DEFAULT 'checkbox',
  payload           TEXT
);

-- ADR-0006: full-text cache of each indexed note, so the read-only TUI can render a
-- task's surrounding context without ever opening a vault file. One row per note
-- (deduped across the note's tasks). The daemon writes it in the same `index_note`
-- pass that parses tasks, so `content`/`note_hash`/task `line_number` all come from the
-- same byte snapshot. The TUI picks the rendered window; windowing is not stored.
CREATE TABLE IF NOT EXISTS note_contents (
  note_path   TEXT PRIMARY KEY,
  content     TEXT NOT NULL,
  note_hash   TEXT,
  updated_at  INTEGER NOT NULL
);
";

/// Schema version tag stored in `PRAGMA user_version`. Increment when the schema
/// changes in a backward-incompatible way.
const SCHEMA_VERSION: i64 = 7;

/// Ensure the database schema exists and is at the current version. If the DB predates
/// the current version, the old tables are dropped and recreated (pre-MVP: no data to
/// preserve). A one-line note is logged via `eprintln` so it's visible even before
/// tracing is set up.
fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        if version > 0 {
            eprintln!(
                "taski-db: schema v{version} -> v{SCHEMA_VERSION}; recreating tables (any pending actions will be lost)"
            );
        }
        // Drop all tables so the fresh CREATE is clean. Order is harmless (no FKs).
        conn.execute_batch(
            "DROP TABLE IF EXISTS pending_actions;
             DROP TABLE IF EXISTS note_contents;
             DROP TABLE IF EXISTS tasks;",
        )?;
    }
    conn.execute_batch(SCHEMA)?;
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    Ok(())
}

/// Open (or create) the database at `path`, configure WAL multi-process access, and
/// ensure the schema exists at the current version. Parent directories of `path` are
/// created if missing (SQLite itself will not), so a configured path like
/// `~/.local/share/taski/taski.db` works on first run. A bare filename or `:memory:`
/// has no usable parent and is left untouched.
pub fn open(path: &str) -> anyhow::Result<Connection> {
    if let Some(parent) = std::path::Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating database directory {}", parent.display()))?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    ensure_schema(&conn)?;
    Ok(conn)
}

/// Insert or replace a task keyed on its `id` (upsert). Used by tests to set up
/// controlled row state; the daemon uses [`reconcile_note`] instead.
pub fn upsert_task(conn: &Connection, task: &Task) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO tasks (
            id, note_path, line_number, indent, text, text_hash, status,
            raw_checkbox_char, note_hash, note_mtime, due_date, scheduled_date,
            tags, priority, start_date, created_date, done_date, cancelled_date,
            updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        rusqlite::params![
            task.id,
            task.note_path,
            task.line_number as i64,
            task.indent as i64,
            task.text,
            task.text_hash,
            task.status.to_checkbox_char(),
            task.raw_checkbox_char,
            task.note_hash,
            task.note_mtime,
            task.due_date,
            task.scheduled_date,
            encode_tags(&task.tags),
            task.priority.as_ref().and_then(Priority::to_emoji),
            task.start_date,
            task.created_date,
            task.done_date,
            task.cancelled_date,
            task.updated_at,
        ],
    )?;
    Ok(())
}

/// Encode a `Vec<String>` of tags into the storage TEXT form: space-separated,
/// with leading + trailing single-space sentinels. Empty Vec → `""`;
/// `["foo","bar"]` → `" foo bar "`. The sentinels make Tier 2 whole-tag SQL
/// matching cheap (`WHERE ' ' || tags || ' ' LIKE '% foo %'`) and reverse
/// cleanly via [`decode_tags`] (the sentinels are whitespace, trimmed away).
fn encode_tags(tags: &[String]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(tags.iter().map(|t| t.len() + 1).sum::<usize>() + 2);
    out.push(' ');
    out.push_str(&tags.join(" "));
    out.push(' ');
    out
}

/// Reverse of [`encode_tags`]: split the stored TEXT on any whitespace run and
/// collect the non-empty pieces into a `Vec<String>`. The leading/trailing
/// sentinel spaces are naturally skipped (`split_whitespace` ignores leading /
/// trailing whitespace); tolerates an empty column (returns `Vec::new()`).
fn decode_tags(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// Delete every task belonging to a single note, keyed on its `note_path` (relative
/// to the vault root). Used by the daemon on note removal so the index never carries
/// stale rows for a deleted note.
pub fn delete_tasks_for_note(conn: &Connection, note_path: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM tasks WHERE note_path = ?1",
        rusqlite::params![note_path],
    )?;
    Ok(())
}

/// Delete every task (and cached note content) whose `note_path` matches (or starts
/// with) one of the given excluded directory prefixes. Called at daemon startup when
/// `exclude_dirs` has changed so the index doesn't carry stale entries from directories
/// the user no longer wants indexed. Each prefix is matched exactly (`note_path = ?`)
/// and as a prefix (`note_path LIKE ?/`).
pub fn delete_tasks_for_excluded_dirs(
    conn: &Connection,
    excluded: &[String],
) -> rusqlite::Result<()> {
    for excl in excluded {
        let excl = excl.trim_end_matches('/');
        let prefix = format!("{}/%", excl);
        conn.execute(
            "DELETE FROM tasks WHERE note_path = ?1 OR note_path LIKE ?2",
            rusqlite::params![excl, prefix],
        )?;
        conn.execute(
            "DELETE FROM note_contents WHERE note_path = ?1 OR note_path LIKE ?2",
            rusqlite::params![excl, prefix],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// note_contents: per-note full-text cache for the read-only TUI context pane
// (ADR-0006). The daemon writes it in the same `index_note` pass that parses
// tasks; the TUI reads it like any other index data.
// ---------------------------------------------------------------------------

/// One indexed note's cached content (ADR-0006). The TUI reads this to render a task's
/// surrounding context without ever opening a vault file. `note_hash` mirrors the hash
/// stored on the note's task rows (same scan), so the TUI can cheaply tell when its
/// cached content is stale and needs re-reading.
#[derive(Debug, Clone)]
pub struct NoteContent {
    /// Note path relative to the vault root (the same key used by `tasks.note_path`).
    pub note_path: String,
    /// Full UTF-8 text of the note at last scan.
    pub content: String,
    /// Content hash captured at the same scan (mirrors the note's `tasks.note_hash`).
    pub note_hash: Option<String>,
    /// Unix seconds the row was last written. Informational.
    pub updated_at: i64,
}

/// Insert or replace the cached content for one note (ADR-0006). Called by the daemon
/// during `index_note`, in the same pass that reconciles the note's tasks, so the
/// content and the task rows always reflect the same byte snapshot.
pub fn upsert_note_content(
    conn: &Connection,
    note_path: &str,
    content: &str,
    note_hash: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO note_contents (note_path, content, note_hash, updated_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![note_path, content, note_hash, unix_now()],
    )?;
    Ok(())
}

/// Read the cached content for one note, or `None` if the note is not cached (e.g. a
/// pre-v3 DB, a note that failed to scan, or a note path the TUI has no task for).
pub fn note_content(conn: &Connection, note_path: &str) -> rusqlite::Result<Option<NoteContent>> {
    let row = conn.query_row(
        "SELECT note_path, content, note_hash, updated_at
         FROM note_contents
         WHERE note_path = ?1",
        rusqlite::params![note_path],
        |row| {
            Ok(NoteContent {
                note_path: row.get(0)?,
                content: row.get(1)?,
                note_hash: row.get(2)?,
                updated_at: row.get(3)?,
            })
        },
    );
    match row {
        Ok(nc) => Ok(Some(nc)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Delete the cached content for one note. Called by the daemon on note removal,
/// alongside [`delete_tasks_for_note`], so the index never carries content for a
/// deleted note.
pub fn delete_note_content(conn: &Connection, note_path: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM note_contents WHERE note_path = ?1",
        rusqlite::params![note_path],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reconciliation: content-hash matching on re-scan (ADR-0005 §3).
// ---------------------------------------------------------------------------

/// Summary of a single-note reconciliation pass — how many rows were kept (matched by
/// `text_hash`, rowid preserved), inserted (new), and deleted (gone).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileSummary {
    /// Tasks matched by `text_hash` and kept (updated in place, rowid preserved).
    pub kept: usize,
    /// Tasks inserted (no matching old row).
    pub inserted: usize,
    /// Old rows deleted (no matching new task).
    pub deleted: usize,
}

/// Reconcile the index for one note per ADR-0005 §3. Old rows are matched to
/// freshly-parsed tasks by `text_hash` (greedy, FIFO within each hash, in line order).
/// Matched rows are UPDATEd in place (preserving their surrogate rowid); their
/// `line_number`, `text`, `status`, `raw_checkbox_char`, `note_hash`, `note_mtime`,
/// and `updated_at` are refreshed. Unmatched old rows are deleted; unmatched new
/// parses are inserted. The whole operation runs in one transaction so a note's
/// re-scan is atomic.
pub fn reconcile_note(
    conn: &Connection,
    note_path: &str,
    new_tasks: &[Task],
    note_hash: Option<&str>,
    note_mtime: Option<i64>,
) -> rusqlite::Result<ReconcileSummary> {
    // 1. Fetch old rows for this note, ordered by line_number for deterministic FIFO.
    //    This SELECT runs before BEGIN; it is safe only because the daemon is the sole
    //    writer to `tasks` (ADR-0002) and reconciliation is single-threaded in the watch
    //    loop — no concurrent writer can change these rows between read and the txn below.
    let mut stmt = conn
        .prepare("SELECT id, text_hash FROM tasks WHERE note_path = ?1 ORDER BY line_number ASC")?;
    let old_rows: Vec<(i64, String)> = stmt
        .query_map(rusqlite::params![note_path], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let total_old = old_rows.len();

    // 2. Build old_by_hash: HashMap<text_hash, VecDeque<id>> (FIFO per hash).
    let mut old_by_hash: HashMap<String, VecDeque<i64>> = HashMap::new();
    for (id, text_hash) in &old_rows {
        old_by_hash
            .entry(text_hash.clone())
            .or_default()
            .push_back(*id);
    }

    let mut matched_ids: HashSet<i64> = HashSet::new();
    let now = unix_now();

    // 3–4 in a transaction so the whole reconcile is atomic.
    conn.execute_batch("BEGIN")?;
    let result = (|| -> rusqlite::Result<()> {
        // 3. For each new task (line order): match → UPDATE in place; else INSERT.
        for task in new_tasks {
            if let Some(queue) = old_by_hash.get_mut(&task.text_hash)
                && let Some(old_id) = queue.pop_front()
            {
                conn.execute(
                    "UPDATE tasks SET
                        line_number = ?2, indent = ?3, text = ?4, status = ?5,
                        raw_checkbox_char = ?6, note_hash = ?7, note_mtime = ?8,
                        due_date = ?9, scheduled_date = ?10, tags = ?11,
                        priority = ?12, start_date = ?13, created_date = ?14,
                        done_date = ?15, cancelled_date = ?16, updated_at = ?17
                     WHERE id = ?1",
                    rusqlite::params![
                        old_id,
                        task.line_number as i64,
                        task.indent as i64,
                        task.text,
                        task.status.to_checkbox_char(),
                        task.raw_checkbox_char,
                        note_hash,
                        note_mtime,
                        task.due_date,
                        task.scheduled_date,
                        encode_tags(&task.tags),
                        task.priority.as_ref().and_then(Priority::to_emoji),
                        task.start_date,
                        task.created_date,
                        task.done_date,
                        task.cancelled_date,
                        now,
                    ],
                )?;
                matched_ids.insert(old_id);
                continue;
            }
            // No match — INSERT. The DB assigns the surrogate rowid.
            conn.execute(
                "INSERT INTO tasks (
                    note_path, line_number, indent, text, text_hash, status,
                    raw_checkbox_char, note_hash, note_mtime, due_date, scheduled_date,
                    tags, priority, start_date, created_date, done_date, cancelled_date,
                    updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                rusqlite::params![
                    note_path,
                    task.line_number as i64,
                    task.indent as i64,
                    task.text,
                    task.text_hash,
                    task.status.to_checkbox_char(),
                    task.raw_checkbox_char,
                    note_hash,
                    note_mtime,
                    task.due_date,
                    task.scheduled_date,
                    encode_tags(&task.tags),
                    task.priority.as_ref().and_then(Priority::to_emoji),
                    task.start_date,
                    task.created_date,
                    task.done_date,
                    task.cancelled_date,
                    now,
                ],
            )?;
        }

        // 4. Delete old rows whose id was never matched (task removed from the note).
        for queue in old_by_hash.values() {
            for id in queue {
                conn.execute("DELETE FROM tasks WHERE id = ?1", rusqlite::params![id])?;
            }
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            let kept = matched_ids.len();
            let inserted = new_tasks.len().saturating_sub(kept);
            let deleted = total_old.saturating_sub(kept);
            Ok(ReconcileSummary {
                kept,
                inserted,
                deleted,
            })
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// pending_actions: the TUI -> daemon write-back queue (ADR-0002 / ADR-0003).
// ---------------------------------------------------------------------------

/// One requested write-back action, queued by the TUI for the daemon to execute.
/// The lifecycle is `pending` -> `done` (applied) or `failed` (refused/errored).
/// ADR-0009 Phase 2: the queue now carries two action kinds via `action_type` —
/// `'checkbox'` (a flip; `expected_char`/`new_char` hold the chars) and
/// `'set_scheduled'` (a `⏳` write; `payload` holds the desired `YYYY-MM-DD`, or
/// `NULL` to unmark). The daemon dispatches on `action_type`.
#[derive(Debug, Clone)]
pub struct PendingAction {
    /// Row id (AUTOINCREMENT).
    pub id: i64,
    /// `tasks.id` of the target task (the surrogate rowid, ADR-0005).
    pub task_id: i64,
    /// Note the task lives in (relative to vault root).
    pub note_path: String,
    /// 1-based line of the checkbox within the note.
    pub line_number: usize,
    /// The `raw_checkbox_char` the user saw when requesting the flip; the daemon
    /// re-verifies this exact byte is still present before writing (ADR-0004).
    /// Unused for non-checkbox action types (empty string).
    pub expected_char: String,
    /// Desired checkbox char after the flip (e.g. `"x"`, `" "`, `"/"`). Unused
    /// for non-checkbox action types (empty string).
    pub new_char: String,
    /// `pending` | `done` | `failed`.
    pub state: String,
    /// Unix seconds the action was enqueued.
    pub created_at: i64,
    /// Unix seconds the daemon resolved the action (`None` while pending).
    pub resolved_at: Option<i64>,
    /// Error/explanation message set when `state = "failed"`.
    pub error: Option<String>,
    /// Action kind: `'checkbox'` (default) or `'set_scheduled'` (ADR-0009 P2).
    pub action_type: String,
    /// Type-specific payload. For `'set_scheduled'`: the desired `YYYY-MM-DD`,
    /// or `None` to unmark. For `'checkbox'`: always `None`.
    pub payload: Option<String>,
}

/// Enqueue a checkbox-flip request from the TUI. Inserts with `state='pending'`,
/// `action_type='checkbox'`, `payload=NULL`, and returns the new row id.
pub fn enqueue_action(
    conn: &Connection,
    task_id: i64,
    note_path: &str,
    line_number: usize,
    expected_char: &str,
    new_char: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO pending_actions
            (task_id, note_path, line_number, expected_char, new_char, state,
             created_at, action_type, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, 'checkbox', NULL)",
        rusqlite::params![
            task_id,
            note_path,
            line_number as i64,
            expected_char,
            new_char,
            unix_now(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Enqueue a "set scheduled date" write request (ADR-0009 Phase 2). `desired =
/// Some("YYYY-MM-DD")` marks/re-schedules the task for that date; `desired = None`
/// removes an existing `⏳`. The daemon's `process_metadata_action` performs the
/// vault write via `taski_core::rewrite_scheduled` + `atomic_write`. `expected_char`
/// and `new_char` are unused for this action type (stored empty; the daemon
/// dispatches on `action_type='set_scheduled'` and reads `payload`). Returns the
/// new row id, like [`enqueue_action`].
pub fn enqueue_set_scheduled(
    conn: &Connection,
    task_id: i64,
    note_path: &str,
    line_number: usize,
    desired: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO pending_actions
            (task_id, note_path, line_number, expected_char, new_char, state,
             created_at, action_type, payload)
         VALUES (?1, ?2, ?3, '', '', 'pending', ?4, 'set_scheduled', ?5)",
        rusqlite::params![task_id, note_path, line_number as i64, unix_now(), desired],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Enqueue a "toggle bullet" request (ADR-0011). Converts the task line between
/// checkbox and bullet format. `expected_char` and `new_char` are unused (stored
/// empty; the daemon dispatches on `action_type='toggle_bullet'`). Returns the
/// new row id.
pub fn enqueue_bullet_toggle(
    conn: &Connection,
    task_id: i64,
    note_path: &str,
    line_number: usize,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO pending_actions
            (task_id, note_path, line_number, expected_char, new_char, state,
             created_at, action_type, payload)
         VALUES (?1, ?2, ?3, '', '', 'pending', ?4, 'toggle_bullet', NULL)",
        rusqlite::params![task_id, note_path, line_number as i64, unix_now()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// ADR-0014: enqueue a quick-add request — append a new task line
/// (`- [ ] <text> ➕ <today>`) to the designated inbox note. No schema change:
/// sentinel values fill the NOT NULL columns that are unused for this action
/// type (`task_id=0`, `line_number=0`, `expected_char=''`, `new_char=''`), per
/// the established "unused for non-checkbox action types" pattern. The inbox
/// path travels in `note_path`; the user-typed text travels in `payload`.
/// Returns the new row id, like [`enqueue_action`].
pub fn enqueue_quick_add(conn: &Connection, inbox_path: &str, text: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO pending_actions
            (task_id, note_path, line_number, expected_char, new_char, state,
             created_at, action_type, payload)
         VALUES (0, ?1, 0, '', '', 'pending', ?2, 'quick_add', ?3)",
        rusqlite::params![inbox_path, unix_now(), text],
    )?;
    Ok(conn.last_insert_rowid())
}

/// ADR-0014: enqueue a quick-add UNDO request — remove the last line from the
/// inbox if it matches the expected `- [ ] <text> ➕ <today>` content. The daemon
/// verifies the last line before removing (refuses on mismatch). The inbox path
/// travels in `note_path`; the original task text travels in `payload`.
pub fn enqueue_quick_add_undo(
    conn: &Connection,
    inbox_path: &str,
    text: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO pending_actions
            (task_id, note_path, line_number, expected_char, new_char, state,
             created_at, action_type, payload)
         VALUES (0, ?1, 0, '', '', 'pending', ?2, 'quick_add_undo', ?3)",
        rusqlite::params![inbox_path, unix_now(), text],
    )?;
    Ok(conn.last_insert_rowid())
}

/// ADR-0019: enqueue an "add task note" request — append a free-text note under
/// the task's `### notes-<id>` heading inside a `## task-notes` section in the
/// note the task lives in, and (on the first note) insert one aliased in-page
/// wikilink into the task line. The daemon owns the first-vs-append decision and
/// `<id>` assignment; the TUI supplies only the task's identity and the typed
/// text. `expected_char`/`new_char` are unused (stored empty); the daemon
/// dispatches on `action_type='add_note'` and verifies via the cached note hash.
/// The note text travels in `payload`. Returns the new row id.
pub fn enqueue_add_note(
    conn: &Connection,
    task_id: i64,
    note_path: &str,
    line_number: usize,
    text: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO pending_actions
            (task_id, note_path, line_number, expected_char, new_char, state,
             created_at, action_type, payload)
         VALUES (?1, ?2, ?3, '', '', 'pending', ?4, 'add_note', ?5)",
        rusqlite::params![task_id, note_path, line_number as i64, unix_now(), text],
    )?;
    Ok(conn.last_insert_rowid())
}

/// All actions still awaiting processing, oldest first.
pub fn pending_actions(conn: &Connection) -> rusqlite::Result<Vec<PendingAction>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, note_path, line_number, expected_char, new_char,
                state, created_at, resolved_at, error, action_type, payload
         FROM pending_actions
         WHERE state = 'pending'
         ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(PendingAction {
            id: row.get(0)?,
            task_id: row.get::<_, i64>(1)?,
            note_path: row.get(2)?,
            line_number: row.get::<_, i64>(3)? as usize,
            expected_char: row.get(4)?,
            new_char: row.get(5)?,
            state: row.get(6)?,
            created_at: row.get(7)?,
            resolved_at: row.get(8)?,
            error: row.get(9)?,
            action_type: row.get(10)?,
            payload: row.get(11)?,
        })
    })?;
    rows.collect()
}

/// The most-recently resolved actions (`state` in `done`/`failed`), newest first, up
/// to `limit`. Ordering is by `resolved_at` descending (tie-broken by `id` desc so a
/// burst of resolutions within the same second stays enqueue-ordered). Pending
/// actions are excluded — this is the read-back path the TUI uses to learn how the
/// actions it enqueued this session were resolved (see ADR-0002).
pub fn recent_actions(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<PendingAction>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, note_path, line_number, expected_char, new_char,
                state, created_at, resolved_at, error, action_type, payload
         FROM pending_actions
         WHERE state IN ('done', 'failed')
         ORDER BY resolved_at DESC NULLS LAST, id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit], |row| {
        Ok(PendingAction {
            id: row.get(0)?,
            task_id: row.get::<_, i64>(1)?,
            note_path: row.get(2)?,
            line_number: row.get::<_, i64>(3)? as usize,
            expected_char: row.get(4)?,
            new_char: row.get(5)?,
            state: row.get(6)?,
            created_at: row.get(7)?,
            resolved_at: row.get(8)?,
            error: row.get(9)?,
            action_type: row.get(10)?,
            payload: row.get(11)?,
        })
    })?;
    rows.collect()
}

/// Mark an action resolved: `state` becomes `done` or `failed`, `resolved_at` is
/// stamped, and an optional explanation `error` is recorded.
pub fn resolve_action(
    conn: &Connection,
    id: i64,
    state: &str,
    error: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE pending_actions SET state = ?1, resolved_at = ?2, error = ?3 WHERE id = ?4",
        rusqlite::params![state, unix_now(), error, id],
    )?;
    Ok(())
}

/// Delete resolved (`done`/`failed`) actions whose `resolved_at` is strictly older
/// than `older_than` (unix seconds). Pending actions are never deleted, nor are
/// resolved actions that resolved at or after the cutoff. Returns the row count
/// removed. Called once on daemon startup to bound the `pending_actions` table's
/// growth (M2 housekeeping).
pub fn prune_old_actions(conn: &Connection, older_than: i64) -> rusqlite::Result<usize> {
    let deleted = conn.execute(
        "DELETE FROM pending_actions
         WHERE state != 'pending' AND resolved_at IS NOT NULL AND resolved_at < ?1",
        rusqlite::params![older_than],
    )?;
    Ok(deleted)
}

/// Current unix time in seconds, or 0 if the clock is before the epoch.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read every task from the index, ordered by note path then line number for a stable
/// display. Status is reconstructed from `raw_checkbox_char` via
/// [`Status::from_checkbox_char`].
pub fn all_tasks(conn: &Connection) -> rusqlite::Result<Vec<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id, note_path, line_number, indent, text, text_hash, status,
                raw_checkbox_char, note_hash, note_mtime, due_date, scheduled_date,
                tags, priority, start_date, created_date, done_date, cancelled_date,
                updated_at
         FROM tasks
         ORDER BY note_path ASC, line_number ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        // Column → index map (verify every index when adding/removing columns):
        //  0: id              7: raw_checkbox_    14: created_date
        //  1: note_path         char              15: done_date
        //  2: line_number     8: note_hash        16: cancelled_date
        //  3: indent          9: note_mtime       17: updated_at
        //  4: text           10: due_date
        //  5: text_hash      11: scheduled_date
        //  6: status         12: tags
        //                   13: priority
        let raw_checkbox_char: String = row.get(7)?;
        let status = Status::from_checkbox_char(&raw_checkbox_char);
        let tags_text: String = row.get(12)?;
        let priority_text: Option<String> = row.get(13)?;
        Ok(Task {
            id: row.get::<_, i64>(0)?,
            note_path: row.get(1)?,
            line_number: row.get::<_, i64>(2)? as usize,
            indent: row.get::<_, i64>(3)? as usize,
            text: row.get(4)?,
            text_hash: row.get(5)?,
            // Column 6 (stored status) is intentionally unused on read; status is
            // reconstructed from raw_checkbox_char so the two never drift.
            status,
            raw_checkbox_char,
            note_hash: row.get(8)?,
            note_mtime: row.get(9)?,
            due_date: row.get(10)?,
            scheduled_date: row.get(11)?,
            tags: decode_tags(&tags_text),
            priority: priority_text.and_then(|s| Priority::from_emoji_str(&s)),
            start_date: row.get(14)?,
            created_date: row.get(15)?,
            done_date: row.get(16)?,
            cancelled_date: row.get(17)?,
            updated_at: row.get(18)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use taski_core::Status;

    fn sample_task(id: i64, raw: &str, line: usize) -> Task {
        Task {
            id,
            note_path: "n.md".to_string(),
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
            updated_at: 123,
        }
    }

    #[test]
    fn upsert_then_read_round_trips_and_replaces_on_id() {
        let conn = open(":memory:").unwrap();
        assert!(all_tasks(&conn).unwrap().is_empty());

        upsert_task(&conn, &sample_task(1, " ", 1)).unwrap();
        upsert_task(&conn, &sample_task(2, "x", 2)).unwrap();
        upsert_task(&conn, &sample_task(3, "/", 3)).unwrap();

        let got = all_tasks(&conn).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, 1);
        assert_eq!(got[0].status, Status::Open);
        assert_eq!(got[0].raw_checkbox_char, " ");
        assert_eq!(got[1].status, Status::Done);
        assert_eq!(got[2].status, Status::InProgress);

        // Same id -> replace.
        let mut updated = sample_task(1, "/", 9);
        updated.text = "changed".to_string();
        upsert_task(&conn, &updated).unwrap();
        let got2 = all_tasks(&conn).unwrap();
        assert_eq!(got2.len(), 3, "replace should not grow the table");
        let a = got2.iter().find(|t| t.id == 1).unwrap();
        assert_eq!(a.text, "changed");
        assert_eq!(a.line_number, 9);
        assert_eq!(a.status, Status::InProgress);
    }

    /// The `indent` column round-trips through upsert/read.
    #[test]
    fn indent_round_trips() {
        let conn = open(":memory:").unwrap();
        let mut t = sample_task(1, " ", 1);
        t.indent = 4;
        upsert_task(&conn, &t).unwrap();
        let got = all_tasks(&conn).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].indent, 4, "indent should round-trip");

        // Update via replace-on-id changes indent too.
        let mut updated = sample_task(1, " ", 1);
        updated.indent = 2;
        upsert_task(&conn, &updated).unwrap();
        let got2 = all_tasks(&conn).unwrap();
        assert_eq!(got2[0].indent, 2, "indent should update on replace");
    }

    #[test]
    fn delete_tasks_for_note_removes_only_that_note() {
        let conn = open(":memory:").unwrap();
        // Two notes, distinct note_path values.
        let mut a1 = sample_task(1, " ", 1);
        a1.note_path = "alpha.md".to_string();
        let mut a2 = sample_task(2, "x", 2);
        a2.note_path = "alpha.md".to_string();
        let mut b1 = sample_task(3, "/", 1);
        b1.note_path = "beta.md".to_string();

        upsert_task(&conn, &a1).unwrap();
        upsert_task(&conn, &a2).unwrap();
        upsert_task(&conn, &b1).unwrap();
        assert_eq!(all_tasks(&conn).unwrap().len(), 3);

        // Delete alpha.md only.
        delete_tasks_for_note(&conn, "alpha.md").unwrap();

        let got = all_tasks(&conn).unwrap();
        assert_eq!(
            got.len(),
            1,
            "alpha's rows should be gone, beta should remain"
        );
        assert_eq!(got[0].id, 3);
        assert_eq!(got[0].note_path, "beta.md");

        // Deleting a note with no rows is a no-op (and not an error).
        delete_tasks_for_note(&conn, "nonexistent.md").unwrap();
        assert_eq!(all_tasks(&conn).unwrap().len(), 1);
    }

    /// ADR-0006: `note_contents` round-trips and `note_content` returns `None` for an
    /// uncached note. A second upsert on the same `note_path` replaces in place.
    #[test]
    fn note_contents_upsert_read_round_trip() {
        let conn = open(":memory:").unwrap();
        // Absent note -> None (not an error).
        assert!(note_content(&conn, "missing.md").unwrap().is_none());

        upsert_note_content(&conn, "a.md", "# Heading\n- [ ] task\n", Some("hash-a")).unwrap();
        let nc = note_content(&conn, "a.md")
            .unwrap()
            .expect("cached note should be present");
        assert_eq!(nc.note_path, "a.md");
        assert_eq!(nc.content, "# Heading\n- [ ] task\n");
        assert_eq!(nc.note_hash.as_deref(), Some("hash-a"));
        assert!(nc.updated_at > 0, "updated_at should be a real timestamp");

        // Same note_path -> replace (no row growth, fields updated).
        upsert_note_content(&conn, "a.md", "rewritten\n", Some("hash-a2")).unwrap();
        let nc2 = note_content(&conn, "a.md").unwrap().unwrap();
        assert_eq!(nc2.content, "rewritten\n");
        assert_eq!(nc2.note_hash.as_deref(), Some("hash-a2"));

        // Exactly one row for a.md.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM note_contents WHERE note_path = 'a.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "upsert should replace, not insert a second row");
    }

    /// ADR-0006: `delete_note_content` removes only the targeted note's cached content.
    #[test]
    fn delete_note_content_removes_only_target() {
        let conn = open(":memory:").unwrap();
        upsert_note_content(&conn, "a.md", "a", None).unwrap();
        upsert_note_content(&conn, "b.md", "b", None).unwrap();
        assert!(note_content(&conn, "a.md").unwrap().is_some());
        assert!(note_content(&conn, "b.md").unwrap().is_some());

        delete_note_content(&conn, "a.md").unwrap();
        assert!(note_content(&conn, "a.md").unwrap().is_none());
        assert!(
            note_content(&conn, "b.md").unwrap().is_some(),
            "b.md must survive deleting a.md"
        );

        // Deleting a never-cached note is a no-op (not an error).
        delete_note_content(&conn, "never-existed.md").unwrap();
    }

    #[test]
    fn pending_actions_enqueue_resolve_round_trip() {
        let conn = open(":memory:").unwrap();
        assert!(pending_actions(&conn).unwrap().is_empty());

        let id = enqueue_action(&conn, 42, "note.md", 4, " ", "x").unwrap();
        let pending = pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        let p = &pending[0];
        assert_eq!(p.id, id);
        assert_eq!(p.task_id, 42);
        assert_eq!(p.note_path, "note.md");
        assert_eq!(p.line_number, 4);
        assert_eq!(p.expected_char, " ");
        assert_eq!(p.new_char, "x");
        assert_eq!(p.state, "pending");
        assert!(p.error.is_none());
        assert!(p.resolved_at.is_none());

        // Resolved actions drop out of the pending view; their final state is
        // recorded. Resolve as done (no error)...
        resolve_action(&conn, id, "done", None).unwrap();
        assert!(pending_actions(&conn).unwrap().is_empty());

        // ...and a failed resolution carries an explanation. Re-enqueue first since
        // the prior one is no longer pending.
        let id2 = enqueue_action(&conn, 99, "note2.md", 1, "x", " ").unwrap();
        resolve_action(&conn, id2, "failed", Some("note changed externally")).unwrap();
        assert!(pending_actions(&conn).unwrap().is_empty());
    }

    /// `recent_actions` returns resolved (`done`/`failed`) actions newest-first,
    /// excludes pending ones, honours the limit, and carries the error text.
    #[test]
    fn recent_actions_returns_resolved_newest_first() {
        let conn = open(":memory:").unwrap();

        // Helper: enqueue an action, then resolve it with a backdated `resolved_at`
        // (so ordering is deterministic regardless of wall-clock granularity).
        let enqueue_resolved =
            |task_id: i64, state: &str, resolved_at: i64, error: Option<&str>| {
                let id = enqueue_action(&conn, task_id, "note.md", 1, " ", "x").unwrap();
                conn.execute(
                "UPDATE pending_actions SET state = ?1, resolved_at = ?2, error = ?3 WHERE id = ?4",
                rusqlite::params![state, resolved_at, error, id],
            )
            .unwrap();
                id
            };

        // id1: done @100 ; id2: failed @300 (newest) ; id3: failed @200.
        let id1 = enqueue_resolved(1, "done", 100, None);
        let id2 = enqueue_resolved(2, "failed", 300, Some("note changed externally"));
        let id3 = enqueue_resolved(3, "failed", 200, Some("task no longer in index"));
        // A still-pending action — must be excluded.
        let pending_id = enqueue_action(&conn, 9, "note.md", 1, " ", "x").unwrap();

        let got = recent_actions(&conn, 64).unwrap();
        // Newest resolved_at first: id2 (300), id3 (200), id1 (100).
        assert_eq!(
            got.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![id2, id3, id1],
            "should be ordered newest-resolved first"
        );
        assert_eq!(got[0].state, "failed");
        assert_eq!(got[0].error.as_deref(), Some("note changed externally"));
        assert_eq!(got[2].state, "done");
        assert!(
            got.iter().all(|a| a.id != pending_id),
            "pending actions must be excluded"
        );

        // LIMIT is honoured.
        let limited = recent_actions(&conn, 2).unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].id, id2);
        assert_eq!(limited[1].id, id3);

        // An empty table yields an empty result (not an error).
        let empty = recent_actions(&open(":memory:").unwrap(), 10).unwrap();
        assert!(empty.is_empty());
    }

    /// M2 housekeeping: `prune_old_actions` deletes only resolved actions older than
    /// the cutoff. Pending actions and recently-resolved actions are kept.
    #[test]
    fn prune_old_actions_deletes_only_old_resolved() {
        let conn = open(":memory:").unwrap();

        // Helper: enqueue + resolve at a specific resolved_at timestamp.
        let enqueue_resolved = |task_id: i64, state: &str, resolved_at: i64| {
            let id = enqueue_action(&conn, task_id, "note.md", 1, " ", "x").unwrap();
            // Resolve, then backdate resolved_at to control the cutoff test.
            conn.execute(
                "UPDATE pending_actions SET state = ?1, resolved_at = ?2 WHERE id = ?3",
                rusqlite::params![state, resolved_at, id],
            )
            .unwrap();
        };

        // Two OLD resolved actions (one done, one failed) — both should be pruned.
        enqueue_resolved(1, "done", 1_000);
        enqueue_resolved(2, "failed", 1_500);
        // One RECENT resolved action — must survive.
        enqueue_resolved(3, "done", 9_000);
        // One action resolved exactly AT the cutoff — must survive (strict `<`).
        enqueue_resolved(4, "done", 5_000);
        // One still-PENDING action (enqueued, never resolved) — must survive.
        let pending_id = enqueue_action(&conn, 5, "note.md", 1, " ", "x").unwrap();

        // Cutoff: 5_000. Only resolved_at < 5_000 is pruned → ids 1 and 2.
        let pruned = prune_old_actions(&conn, 5_000).unwrap();
        assert_eq!(
            pruned, 2,
            "only the two old resolved actions should be pruned"
        );

        // The two old ones are gone; recent/at-cutoff/pending survive.
        let mut stmt = conn
            .prepare("SELECT id FROM pending_actions ORDER BY id ASC")
            .unwrap();
        let survivors: Vec<i64> = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        drop(stmt);
        assert_eq!(survivors, vec![3, 4, pending_id], "survivors mismatch");

        // The pending action is still fetchable as pending.
        let pending = pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, pending_id);
        assert_eq!(pending[0].state, "pending");

        // A second prune with a fresh cutoff is a no-op when nothing qualifies.
        let pruned_again = prune_old_actions(&conn, 0).unwrap();
        assert_eq!(pruned_again, 0);
    }

    /// Regression: `open` creates missing parent directories so a configured path like
    /// `~/.local/share/taski/taski.db` works on first run (SQLite itself returns
    /// SQLITE_CANTOPEN when the directory is absent).
    #[test]
    fn open_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A nested path whose parent directories do not exist yet.
        let db_path = dir.path().join("a/b/c/deep.db");
        let conn = open(&db_path.to_string_lossy()).expect("open creates parent dirs");

        assert!(db_path.exists(), "db file should have been created");
        // The connection is usable.
        assert!(all_tasks(&conn).unwrap().is_empty());
    }

    /// ADR-0009 Phase 1: `scheduled_date` round-trips through the index (Some and
    /// None), exercising `upsert_task` + `all_tasks` for the new column. A Task
    /// with `scheduled_date = None` must read back as None (not a stale value
    /// from a sibling row).
    #[test]
    fn scheduled_date_round_trips_some_and_none() {
        let conn = open(":memory:").unwrap();

        // A task with a scheduled date set.
        let mut with_sched = sample_task(1, " ", 1);
        with_sched.scheduled_date = Some("2026-06-20".to_string());
        // A task with no scheduled date (and a due date, to prove the two columns
        // are independent).
        let mut without_sched = sample_task(2, " ", 2);
        without_sched.due_date = Some("2026-07-01".to_string());
        upsert_task(&conn, &with_sched).unwrap();
        upsert_task(&conn, &without_sched).unwrap();

        let got = all_tasks(&conn).unwrap();
        assert_eq!(got.len(), 2);
        let a = got.iter().find(|t| t.id == 1).unwrap();
        assert_eq!(a.scheduled_date.as_deref(), Some("2026-06-20"));
        assert!(
            a.due_date.is_none(),
            "scheduled_date must not bleed into due_date"
        );
        let b = got.iter().find(|t| t.id == 2).unwrap();
        assert!(
            b.scheduled_date.is_none(),
            "a None scheduled_date must round-trip as None"
        );
        assert_eq!(b.due_date.as_deref(), Some("2026-07-01"));
    }

    /// `reconcile_note` carries `indent` through both its INSERT and UPDATE paths.
    /// This is the production write path (the daemon calls `reconcile_note`, not
    /// `upsert_task`), so a missing column here would mean indent is never persisted.
    #[test]
    fn reconcile_note_carries_indent_through_insert_and_update() {
        let conn = open(":memory:").unwrap();

        // INSERT path: a nested task is first-seen.
        let initial = taski_core::parse_tasks("  - [ ] nested\n", "n.md");
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].indent, 2);
        reconcile_note(&conn, "n.md", &initial, Some("h1"), None).unwrap();
        let got = all_tasks(&conn).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].indent, 2, "indent must persist through INSERT path");
        let preserved_id = got[0].id;

        // UPDATE path: same body text, deeper indentation. Same text_hash → the row
        // is matched and UPDATEd in place (id preserved), indent refreshed.
        let reindented = taski_core::parse_tasks("    - [ ] nested\n", "n.md");
        assert_eq!(reindented[0].indent, 4);
        reconcile_note(&conn, "n.md", &reindented, Some("h2"), None).unwrap();
        let got2 = all_tasks(&conn).unwrap();
        assert_eq!(got2.len(), 1);
        assert_eq!(
            got2[0].id, preserved_id,
            "identity preserved across a matched UPDATE"
        );
        assert_eq!(got2[0].indent, 4, "indent must refresh through UPDATE path");
    }

    /// ADR-0009 Phase 1: `reconcile_note` carries `scheduled_date` through its
    /// UPDATE path (matched row keeps its id, scheduled_date refreshed).
    #[test]
    fn reconcile_note_carries_scheduled_date_through_update() {
        let conn = open(":memory:").unwrap();
        // Initial parse: task has a scheduled date.
        let initial = taski_core::parse_tasks("- [ ] plan ⏳ 2026-06-20\n", "n.md");
        assert_eq!(initial.len(), 1);
        reconcile_note(&conn, "n.md", &initial, Some("h1"), None).unwrap();
        let got = all_tasks(&conn).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].scheduled_date.as_deref(), Some("2026-06-20"));
        let preserved_id = got[0].id;

        // Re-parse with a different scheduled date: same text_hash? No — text_hash
        // is over the body, and the date changed, so text_hash changes and this is
        // a delete+insert. To exercise the UPDATE path we keep the body identical
        // (identical text_hash) by editing only a non-body attribute via upsert is
        // not the path here; instead reconcile the SAME body again — the row is
        // matched by text_hash and UPDATEd in place (id preserved). The
        // scheduled_date carried in the new parse is re-written unchanged.
        reconcile_note(&conn, "n.md", &initial, Some("h2"), None).unwrap();
        let got2 = all_tasks(&conn).unwrap();
        assert_eq!(got2.len(), 1);
        assert_eq!(
            got2[0].id, preserved_id,
            "identity preserved across a matched UPDATE"
        );
        assert_eq!(got2[0].scheduled_date.as_deref(), Some("2026-06-20"));
    }

    /// ADR-0009 Phase 2: the queue carries both action kinds via `action_type`/
    /// `payload`. A checkbox flip enqueues `action_type='checkbox'`, `payload=None`;
    /// `enqueue_set_scheduled` enqueues `action_type='set_scheduled'` with the date
    /// (or NULL) in `payload`. Both are read back verbatim by `pending_actions`.
    #[test]
    fn enqueue_set_scheduled_round_trips_with_checkbox_actions() {
        let conn = open(":memory:").unwrap();

        // A checkbox action (the proven path) — action_type defaults/sets to checkbox.
        let cb_id = enqueue_action(&conn, 1, "n.md", 3, " ", "x").unwrap();
        // A set_scheduled mark (Some date).
        let mark_id = enqueue_set_scheduled(&conn, 2, "n.md", 4, Some("2026-06-20")).unwrap();
        // A set_scheduled unmark (None).
        let unmark_id = enqueue_set_scheduled(&conn, 3, "n.md", 5, None).unwrap();

        let pending = pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 3);

        let cb = pending.iter().find(|a| a.id == cb_id).unwrap();
        assert_eq!(cb.action_type, "checkbox");
        assert!(cb.payload.is_none(), "checkbox payload is always NULL");
        assert_eq!(cb.expected_char, " ");
        assert_eq!(cb.new_char, "x");

        let mark = pending.iter().find(|a| a.id == mark_id).unwrap();
        assert_eq!(mark.action_type, "set_scheduled");
        assert_eq!(mark.payload.as_deref(), Some("2026-06-20"));
        assert_eq!(mark.expected_char, "", "unused for set_scheduled");
        assert_eq!(mark.new_char, "", "unused for set_scheduled");

        let unmark = pending.iter().find(|a| a.id == unmark_id).unwrap();
        assert_eq!(unmark.action_type, "set_scheduled");
        assert!(
            unmark.payload.is_none(),
            "None desired round-trips as NULL payload"
        );

        // recent_actions surfaces the same fields after resolution.
        resolve_action(
            &conn,
            mark_id,
            "failed",
            Some("scheduled date is malformed"),
        )
        .unwrap();
        let recent = recent_actions(&conn, 8).unwrap();
        let r = recent.iter().find(|a| a.id == mark_id).unwrap();
        assert_eq!(r.action_type, "set_scheduled");
        assert_eq!(r.payload.as_deref(), Some("2026-06-20"));
        assert_eq!(r.error.as_deref(), Some("scheduled date is malformed"));
    }

    /// ADR-0014: `enqueue_quick_add` / `enqueue_quick_add_undo` round-trip with
    /// sentinel values (`task_id=0`, `line_number=0`, `expected_char=''`,
    /// `new_char=''`), the inbox path in `note_path`, and the text in `payload`.
    #[test]
    fn enqueue_quick_add_round_trips_with_sentinel_values() {
        let conn = open(":memory:").unwrap();

        let qa_id = enqueue_quick_add(&conn, "task-inbox.md", "buy milk").unwrap();
        let undo_id = enqueue_quick_add_undo(&conn, "task-inbox.md", "buy milk").unwrap();

        let pending = pending_actions(&conn).unwrap();
        assert_eq!(pending.len(), 2);

        let qa = pending.iter().find(|a| a.id == qa_id).unwrap();
        assert_eq!(qa.action_type, "quick_add");
        assert_eq!(qa.note_path, "task-inbox.md", "inbox path in note_path");
        assert_eq!(qa.payload.as_deref(), Some("buy milk"), "text in payload");
        assert_eq!(qa.task_id, 0, "sentinel task_id");
        assert_eq!(qa.line_number, 0, "sentinel line_number");
        assert_eq!(qa.expected_char, "", "unused for quick_add");
        assert_eq!(qa.new_char, "", "unused for quick_add");
        assert_eq!(qa.state, "pending");

        let undo = pending.iter().find(|a| a.id == undo_id).unwrap();
        assert_eq!(undo.action_type, "quick_add_undo");
        assert_eq!(undo.payload.as_deref(), Some("buy milk"));
        assert_eq!(undo.note_path, "task-inbox.md");
    }

    // --- Tier 1: parsed-metadata round-trips (tags, priority, dates) -------

    /// Tier 1: all six parsed-metadata fields round-trip through
    /// `upsert_task` and `all_tasks`, exercising both the Some- and None-paths.
    /// Mirrors the existing `scheduled_date_round_trips_some_and_none` pattern.
    #[test]
    fn tier1_metadata_round_trips_some_and_none() {
        let conn = open(":memory:").unwrap();

        // Task A: every Tier 1 field populated.
        let mut with_meta = sample_task(1, " ", 1);
        with_meta.tags = vec!["a".to_string(), "b".to_string()];
        with_meta.priority = Priority::from_emoji('\u{23EB}'); // ⏫ → High
        with_meta.start_date = Some("2026-06-15".to_string());
        with_meta.created_date = Some("2026-01-01".to_string());
        with_meta.done_date = Some("2026-06-20".to_string());
        with_meta.cancelled_date = Some("2026-06-21".to_string());

        // Task B: every Tier 1 field empty / None (must not bleed from A).
        let without_meta = sample_task(2, " ", 2);

        upsert_task(&conn, &with_meta).unwrap();
        upsert_task(&conn, &without_meta).unwrap();

        let got = all_tasks(&conn).unwrap();
        assert_eq!(got.len(), 2);

        let a = got.iter().find(|t| t.id == 1).unwrap();
        assert_eq!(a.tags, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(a.priority, Some(Priority::High));
        assert_eq!(a.start_date.as_deref(), Some("2026-06-15"));
        assert_eq!(a.created_date.as_deref(), Some("2026-01-01"));
        assert_eq!(a.done_date.as_deref(), Some("2026-06-20"));
        assert_eq!(a.cancelled_date.as_deref(), Some("2026-06-21"));

        let b = got.iter().find(|t| t.id == 2).unwrap();
        assert!(b.tags.is_empty(), "tags must not bleed between rows");
        assert!(b.priority.is_none(), "priority must not bleed between rows");
        assert!(b.start_date.is_none());
        assert!(b.created_date.is_none());
        assert!(b.done_date.is_none());
        assert!(b.cancelled_date.is_none());
    }

    /// Tier 1: `reconcile_note` carries all six parsed-metadata fields through
    /// its UPDATE path (matched row keeps its id; Tier 1 fields refreshed), and
    /// re-reconciling the same parse is stable. Mirrors the existing
    /// `reconcile_note_carries_scheduled_date_through_update` test.
    #[test]
    fn reconcile_note_carries_tier1_metadata_through_update() {
        let conn = open(":memory:").unwrap();
        let body = "ship it \u{1F4C5} 2026-07-01 \u{23F3} 2026-06-20 \u{1F6EB} 2026-06-15 \
                    \u{2795} 2026-01-01 \u{2705} 2026-06-30 #urgent #backend \u{1F53C}";
        let initial = taski_core::parse_tasks(&format!("- [ ] {body}\n"), "n.md");
        assert_eq!(initial.len(), 1);
        reconcile_note(&conn, "n.md", &initial, Some("h1"), None).unwrap();
        let got = all_tasks(&conn).unwrap();
        assert_eq!(got.len(), 1);
        let preserved_id = got[0].id;
        assert_eq!(
            got[0].tags,
            vec!["urgent".to_string(), "backend".to_string()]
        );
        assert_eq!(got[0].priority, Some(Priority::Medium));
        assert_eq!(got[0].start_date.as_deref(), Some("2026-06-15"));
        assert_eq!(got[0].created_date.as_deref(), Some("2026-01-01"));
        assert_eq!(got[0].done_date.as_deref(), Some("2026-06-30"));
        assert!(got[0].cancelled_date.is_none());

        // Re-reconcile the same parse (identical text_hash → UPDATE in place).
        // Identity preserved; Tier 1 fields stable.
        reconcile_note(&conn, "n.md", &initial, Some("h2"), None).unwrap();
        let got2 = all_tasks(&conn).unwrap();
        assert_eq!(got2.len(), 1);
        assert_eq!(got2[0].id, preserved_id, "identity preserved across UPDATE");
        assert_eq!(
            got2[0].tags,
            vec!["urgent".to_string(), "backend".to_string()]
        );
        assert_eq!(got2[0].priority, Some(Priority::Medium));
        assert_eq!(got2[0].start_date.as_deref(), Some("2026-06-15"));
        assert_eq!(got2[0].created_date.as_deref(), Some("2026-01-01"));
        assert_eq!(got2[0].done_date.as_deref(), Some("2026-06-30"));
    }

    /// Tier 1: the sentinel tag-storage format round-trips through
    /// `encode_tags` and `decode_tags`, AND the raw stored bytes are exactly
    /// the sentinel form (`" foo bar "` for `["foo","bar"]`). This pins the
    /// on-disk format so a future change can't silently break Tier 2 whole-tag
    /// SQL matching.
    #[test]
    fn tags_storage_sentinel_format_round_trips() {
        // Empty Vec ↔ "".
        assert_eq!(encode_tags(&[]), "");
        assert!(decode_tags("").is_empty());

        // Single tag.
        assert_eq!(encode_tags(&["foo".to_string()]), " foo ");
        assert_eq!(decode_tags(" foo "), vec!["foo".to_string()]);

        // Multi-tag round-trip preserves order.
        let tags = vec!["foo".to_string(), "bar".to_string()];
        assert_eq!(encode_tags(&tags), " foo bar ");
        assert_eq!(decode_tags(" foo bar "), tags);

        // End-to-end: store via upsert_task, read raw bytes back directly.
        let conn = open(":memory:").unwrap();
        let mut t = sample_task(1, " ", 1);
        t.tags = tags.clone();
        upsert_task(&conn, &t).unwrap();
        let raw: String = conn
            .query_row("SELECT tags FROM tasks WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(raw, " foo bar ", "stored bytes must be the sentinel form");

        // And read back through all_tasks.
        let got = all_tasks(&conn).unwrap();
        assert_eq!(got[0].tags, tags);
    }
}
