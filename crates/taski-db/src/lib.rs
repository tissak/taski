//! SQLite storage layer for Taski: owns the canonical schema and exposes read/write
//! APIs over the shared `tasks` index.
//!
//! The DB is opened in WAL mode with `synchronous=NORMAL`, the standard one-writer +
//! many-readers configuration that lets the daemon (writer) and the TUI (reader)
//! operate against the same file across processes — see ADR-0001.

use std::collections::{HashMap, HashSet, VecDeque};

use rusqlite::Connection;

// Re-export the shared domain types so downstream crates (e.g. the TUI) can depend on
// `taski-db` alone without a direct `taski-core` dependency. This also brings `Status`
// and `Task` into scope within this module.
pub use taski_core::{Status, Task};

/// The canonical schema v2 (ADR-0005): surrogate rowid identity + content-hash
/// reconciliation. Created with `IF NOT EXISTS` so [`ensure_schema`] is idempotent.
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
";

/// Schema version tag stored in `PRAGMA user_version`. Increment when the schema
/// changes in a backward-incompatible way.
const SCHEMA_VERSION: i64 = 2;

/// Ensure the database schema exists and is at the current version. If the DB predates
/// v2, the old tables are dropped and recreated (pre-MVP: no data to preserve). A
/// one-line note is logged via `eprintln` so it's visible even before tracing is set up.
fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        if version > 0 {
            eprintln!(
                "taski-db: schema v{version} -> v{SCHEMA_VERSION}; recreating tables (any pending actions will be lost)"
            );
        }
        // Drop both tables so the fresh CREATE is clean. Order matters: pending_actions
        // has no FK but dropping tasks first is harmless.
        conn.execute_batch("DROP TABLE IF EXISTS pending_actions; DROP TABLE IF EXISTS tasks;")?;
    }
    conn.execute_batch(SCHEMA)?;
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    Ok(())
}

/// Open (or create) the database at `path`, configure WAL multi-process access, and
/// ensure the schema exists at the current version.
pub fn open(path: &str) -> rusqlite::Result<Connection> {
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
}
