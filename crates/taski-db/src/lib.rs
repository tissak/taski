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
// and `Task` into scope within this module.
pub use taski_core::{Status, Task};

/// The canonical schema v3. v2 added surrogate rowid identity + content-hash
/// reconciliation (ADR-0005); v3 adds the `note_contents` cache that backs the read-only
/// TUI context pane (ADR-0006). Created with `IF NOT EXISTS` so [`ensure_schema`] is
/// idempotent.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  note_path         TEXT NOT NULL,
  line_number       INTEGER NOT NULL,
  text              TEXT NOT NULL,
  text_hash         TEXT NOT NULL,
  status            TEXT NOT NULL,
  raw_checkbox_char TEXT NOT NULL,
  note_hash         TEXT,
  note_mtime        INTEGER,
  due_date          TEXT,
  updated_at        INTEGER NOT NULL
);

-- PRD §8 / ADR-0002: the queue the TUI writes action requests into and the daemon
-- (sole vault writer) drains. A row's lifecycle is pending -> done | failed.
CREATE TABLE IF NOT EXISTS pending_actions (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id           INTEGER NOT NULL,
  note_path         TEXT NOT NULL,
  line_number       INTEGER NOT NULL,
  expected_char     TEXT NOT NULL,   -- raw_checkbox_char the user saw (byte verify)
  new_char          TEXT NOT NULL,   -- desired checkbox char after flip
  state             TEXT NOT NULL DEFAULT 'pending',  -- pending | done | failed
  created_at        INTEGER NOT NULL,
  resolved_at       INTEGER,
  error             TEXT
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
const SCHEMA_VERSION: i64 = 3;

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
            id, note_path, line_number, text, text_hash, status,
            raw_checkbox_char, note_hash, note_mtime, due_date, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            task.id,
            task.note_path,
            task.line_number as i64,
            task.text,
            task.text_hash,
            task.status.to_checkbox_char(),
            task.raw_checkbox_char,
            task.note_hash,
            task.note_mtime,
            task.due_date,
            task.updated_at,
        ],
    )?;
    Ok(())
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
                        line_number = ?2, text = ?3, status = ?4,
                        raw_checkbox_char = ?5, note_hash = ?6, note_mtime = ?7,
                        due_date = ?8, updated_at = ?9
                     WHERE id = ?1",
                    rusqlite::params![
                        old_id,
                        task.line_number as i64,
                        task.text,
                        task.status.to_checkbox_char(),
                        task.raw_checkbox_char,
                        note_hash,
                        note_mtime,
                        task.due_date,
                        now,
                    ],
                )?;
                matched_ids.insert(old_id);
                continue;
            }
            // No match — INSERT. The DB assigns the surrogate rowid.
            conn.execute(
                "INSERT INTO tasks (
                    note_path, line_number, text, text_hash, status,
                    raw_checkbox_char, note_hash, note_mtime, due_date, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    note_path,
                    task.line_number as i64,
                    task.text,
                    task.text_hash,
                    task.status.to_checkbox_char(),
                    task.raw_checkbox_char,
                    note_hash,
                    note_mtime,
                    task.due_date,
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

/// One requested checkbox flip, queued by the TUI for the daemon to execute. The
/// lifecycle is `pending` -> `done` (applied) or `failed` (refused/errored).
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
    pub expected_char: String,
    /// Desired checkbox char after the flip (e.g. `"x"`, `" "`, `"/"`).
    pub new_char: String,
    /// `pending` | `done` | `failed`.
    pub state: String,
    /// Unix seconds the action was enqueued.
    pub created_at: i64,
    /// Unix seconds the daemon resolved the action (`None` while pending).
    pub resolved_at: Option<i64>,
    /// Error/explanation message set when `state = "failed"`.
    pub error: Option<String>,
}

/// Enqueue a checkbox-flip request from the TUI. Inserts with `state='pending'` and
/// returns the new row id.
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
            (task_id, note_path, line_number, expected_char, new_char, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
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

/// All actions still awaiting processing, oldest first.
pub fn pending_actions(conn: &Connection) -> rusqlite::Result<Vec<PendingAction>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, note_path, line_number, expected_char, new_char,
                state, created_at, resolved_at, error
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
                state, created_at, resolved_at, error
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
        "SELECT id, note_path, line_number, text, text_hash, status,
                raw_checkbox_char, note_hash, note_mtime, due_date, updated_at
         FROM tasks
         ORDER BY note_path ASC, line_number ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        let raw_checkbox_char: String = row.get(6)?;
        let status = Status::from_checkbox_char(&raw_checkbox_char);
        Ok(Task {
            id: row.get::<_, i64>(0)?,
            note_path: row.get(1)?,
            line_number: row.get::<_, i64>(2)? as usize,
            text: row.get(3)?,
            text_hash: row.get(4)?,
            // Column 5 (stored status) is intentionally unused on read; status is
            // reconstructed from raw_checkbox_char so the two never drift.
            status,
            raw_checkbox_char,
            note_hash: row.get(7)?,
            note_mtime: row.get(8)?,
            due_date: row.get(9)?,
            updated_at: row.get(10)?,
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
            text: format!("task {id}"),
            text_hash: format!("h{id}"),
            status: Status::from_checkbox_char(raw),
            raw_checkbox_char: raw.to_string(),
            note_hash: None,
            note_mtime: None,
            due_date: None,
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
}
