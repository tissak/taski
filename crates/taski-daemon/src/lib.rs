//! taski-daemon — vault scanner, watcher, and the sole writer to the vault.
//!
//! On start, recursively scans an Obsidian vault for `.md` notes and extracts their
//! checkbox tasks into the shared SQLite index; then watches the vault for changes
//! and re-indexes affected notes. It is also the **single writer** to the vault
//! (ADR-0002): it drains the `pending_actions` queue written by the TUI and applies
//! checkbox flips via a conflict-checked, atomic write-back path (ADR-0003/0004).
//!
//! The testable scan + write-back logic lives here as free functions so the
//! integration tests in `tests/` can exercise it without driving the live watcher.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hasher;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer};
use rusqlite::Connection;
use taski_core::parse_tasks;
use taski_db as db;
use taski_db::PendingAction;
use walkdir::WalkDir;

/// Retention window for resolved `pending_actions`: rows in `done`/`failed` state
/// older than this many seconds are pruned on daemon startup (M2 housekeeping) so
/// the table does not grow without bound. Seven days balances auditability against
/// unbounded growth.
const ACTION_RETENTION_SECS: i64 = 7 * 86_400;

/// CLI configuration (PRD §12, Slice 1).
#[derive(Parser, Debug)]
#[command(
    name = "taski-daemon",
    version,
    about = "Watch an Obsidian vault and index checkbox tasks into SQLite"
)]
pub struct Cli {
    /// Path to the Obsidian vault root to scan and watch.
    #[arg(long)]
    pub vault: PathBuf,
    /// Path to the taski SQLite index database.
    #[arg(long, default_value = "./taski.db")]
    pub db: PathBuf,
    /// Run a single full scan and exit (do not start the watch loop).
    #[arg(long)]
    pub once: bool,
}

/// Entry point invoked by the binary's `main`. Parses CLI args, sets up tracing,
/// opens the DB, scans the vault, and (unless `--once`) enters the watch loop.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    let vault_root = cli
        .vault
        .canonicalize()
        .with_context(|| format!("canonicalizing vault path {:?}", cli.vault))?;

    let conn = db::open(&cli.db.to_string_lossy())
        .with_context(|| format!("opening taski database {:?}", cli.db))?;

    // Sweep any temp files left behind by a crash mid-write-back (ADR-0003). Do this
    // BEFORE the scan so the freshly-written scan reflects a clean vault state.
    match sweep_tmp_files(&vault_root) {
        Ok(n) if n > 0 => tracing::warn!(count = n, "swept stale *.taski.tmp files"),
        Ok(_) => {}
        Err(e) => tracing::warn!(err = %e, "temp-file sweep failed; continuing"),
    }

    // Prune resolved pending_actions older than the retention window so the table
    // does not grow without bound (M2 housekeeping). Done once at startup; pending
    // actions are never pruned.
    let cutoff = unix_now() - ACTION_RETENTION_SECS;
    match db::prune_old_actions(&conn, cutoff) {
        Ok(n) if n > 0 => tracing::warn!(count = n, "pruned old resolved actions"),
        Ok(_) => tracing::debug!("no old resolved actions to prune"),
        Err(e) => tracing::warn!(err = %e, "action pruning failed; continuing"),
    }

    let total = scan_vault(&conn, &vault_root)?;
    tracing::info!(count = total, ?vault_root, "initial scan complete");

    if cli.once {
        return Ok(());
    }

    run_watch_loop(&conn, &vault_root)?;
    Ok(())
}

/// Read one note, recompute its tasks, and reconcile them into the index via
/// content-hash matching (ADR-0005 §3). Tasks whose `text_hash` matches an existing
/// row for this note keep their surrogate rowid (UPDATEd in place); unmatched old
/// rows are deleted; new tasks are inserted. Returns the number of tasks indexed
/// (0 if the note was skipped).
///
/// Tolerates non-UTF8 and just-vanished files: both are skipped with a log line
/// rather than aborting the caller.
pub fn index_note(conn: &Connection, abs_path: &Path, vault_root: &Path) -> Result<usize> {
    let rel = relative_to_vault(abs_path, vault_root)?;

    let bytes = match fs::read(abs_path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            tracing::warn!(?rel, "note vanished before read; skipping");
            return Ok(0);
        }
        Err(e) => return Err(e).with_context(|| format!("reading note {:?}", abs_path)),
    };

    let markdown = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(?rel, err = %e, "note is not valid UTF-8; skipping");
            return Ok(0);
        }
    };

    // Capture the note's content hash + mtime at scan time. These anchor the
    // write-back conflict check (ADR-0004): before flipping a checkbox the daemon
    // verifies the note is still byte-identical to this snapshot.
    let hash = content_hash(&bytes);
    let mtime = note_mtime(abs_path);

    let tasks = parse_tasks(markdown, &rel);
    let summary = db::reconcile_note(conn, &rel, &tasks, Some(&hash), mtime)
        .with_context(|| format!("reconciling tasks for {rel:?}"))?;
    tracing::debug!(
        ?rel,
        total = tasks.len(),
        kept = summary.kept,
        inserted = summary.inserted,
        deleted = summary.deleted,
        "reconciled note"
    );
    Ok(tasks.len())
}

/// Walk `vault_root` recursively and index every `.md` note. Directories whose name
/// starts with `.` (e.g. `.obsidian`, `.trash`, `.git`) are pruned and never
/// descended into; non-`.md` files are skipped. Returns the total number of tasks
/// indexed. A single bad note logs and is skipped; it does not abort the scan.
pub fn scan_vault(conn: &Connection, vault_root: &Path) -> Result<usize> {
    let mut total = 0usize;
    for entry in WalkDir::new(vault_root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_hidden_dir_entry(e))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(err = %e, "walkdir error; skipping entry");
                continue;
            }
        };
        if entry.file_type().is_dir() {
            continue;
        }
        let path = entry.path();
        if !is_markdown(path) {
            continue;
        }
        match index_note(conn, path, vault_root) {
            Ok(n) => total += n,
            Err(e) => tracing::error!(?path, err = %e, "index_note failed; continuing scan"),
        }
    }
    Ok(total)
}

/// Event-driven watch loop. Sets up a recursive debounced watcher on `vault_root`
/// and re-indexes `.md` notes on change, deleting their rows on removal. Runs until
/// Ctrl-C / SIGINT, then returns cleanly so the DB connection drops gracefully.
///
/// Note: `notify-debouncer-mini` deliberately does not preserve event kind (it only
/// reports "something changed at this path"), so create/modify vs. remove is decided
/// by checking file existence in [`handle_debounced_event`].
fn run_watch_loop(conn: &Connection, vault_root: &Path) -> Result<()> {
    let (event_tx, event_rx) = mpsc::channel::<DebounceEventResult>();
    let mut debouncer =
        new_debouncer(Duration::from_millis(300), event_tx).context("creating file watcher")?;
    debouncer
        .watcher()
        .watch(vault_root, RecursiveMode::Recursive)
        .with_context(|| format!("watching vault {:?}", vault_root))?;
    tracing::info!(?vault_root, "watching vault for changes (Ctrl-C to stop)");

    // Ctrl-C → shutdown channel. The first signal initiates a clean shutdown; a
    // second falls back to the default (terminate).
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        let _ = shutdown_tx.send(());
    })
    .context("installing Ctrl-C handler")?;

    loop {
        match event_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) => {
                for event in events {
                    handle_debounced_event(conn, &event.path, vault_root);
                }
            }
            Ok(Err(e)) => tracing::error!(err = %e, "watcher error"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!("watcher event channel closed; exiting watch loop");
                break;
            }
        }
        if shutdown_rx.try_recv().is_ok() {
            tracing::info!("shutdown signal received; exiting watch loop");
            break;
        }

        // Drain any TUI-requested checkbox flips each tick (ADR-0002). Errors here
        // are logged, not fatal — a bad action must not kill the watcher.
        if let Err(e) = process_pending_actions(conn, vault_root) {
            tracing::error!(err = %e, "processing pending actions failed");
        }
    }

    // Dropping `debouncer` stops its background watcher thread before the DB
    // connection is dropped on return.
    drop(debouncer);
    Ok(())
}

/// Dispatch one debounced vault event. `.md` files only; paths whose relative path
/// traverses a hidden directory are ignored. Existence decides the action:
/// present → re-index (covers Create + Modify); absent → delete its rows (Remove).
fn handle_debounced_event(conn: &Connection, abs_path: &Path, vault_root: &Path) {
    if !is_markdown(abs_path) {
        return;
    }
    if traverses_hidden_dir(abs_path, vault_root) {
        return;
    }
    if abs_path.exists() {
        match index_note(conn, abs_path, vault_root) {
            Ok(n) => tracing::info!(path = ?abs_path, count = n, "re-indexed note"),
            Err(e) => tracing::error!(?abs_path, err = %e, "index_note failed on watch event"),
        }
    } else {
        match relative_to_vault(abs_path, vault_root) {
            Ok(rel) => match db::delete_tasks_for_note(conn, &rel) {
                Ok(()) => tracing::info!(?rel, "removed tasks for deleted note"),
                Err(e) => tracing::error!(?rel, err = %e, "delete_tasks_for_note failed"),
            },
            Err(e) => {
                tracing::warn!(?abs_path, err = %e, "could not compute relative path on remove")
            }
        }
    }
}

/// Compute a note's path relative to `vault_root` using forward-slash separators, so
/// `note_path` is stable and portable across the daemon, DB, and TUI.
fn relative_to_vault(abs_path: &Path, vault_root: &Path) -> Result<String> {
    let rel = abs_path
        .strip_prefix(vault_root)
        .with_context(|| format!("stripping vault root from {:?}", abs_path))?;
    Ok(rel
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/"))
}

/// True for a directory whose name starts with `.` (hidden / VCS / Obsidian-internal).
fn is_hidden_dir_entry(e: &walkdir::DirEntry) -> bool {
    e.file_type().is_dir() && e.file_name().to_string_lossy().starts_with('.')
}

/// True if `path` ends with `.md` (case-insensitive).
fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

/// True if any *ancestor directory* component of `abs_path` (relative to the vault)
/// starts with `.`. The filename itself is not checked — a top-level hidden file is
/// not "traversing" a hidden dir, matching the scan's directory-only pruning.
fn traverses_hidden_dir(abs_path: &Path, vault_root: &Path) -> bool {
    let Ok(rel) = abs_path.strip_prefix(vault_root) else {
        return false;
    };
    // Compare every component except the last (the filename).
    let mut comps = rel.components();
    let _filename = comps.next_back();
    comps.any(|c| {
        matches!(c, std::path::Component::Normal(name)
            if name.to_string_lossy().starts_with('.'))
    })
}

// ---------------------------------------------------------------------------
// Write-back: the daemon is the sole vault writer (ADR-0002/0003/0004).
// ---------------------------------------------------------------------------

/// Outcome of attempting one pending checkbox flip. Anything other than [`Applied`]
/// means the action was **refused** — the vault is left untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The flip was written atomically.
    Applied,
    /// The note changed externally since scan (content hash mismatch) — refused.
    ConflictNoteChanged,
    /// The task is no longer in the index (deleted) or its note is gone — refused.
    TaskNotFound,
    /// The target line no longer holds the expected checkbox bytes — refused.
    TaskLineMismatch,
    /// `new_char` is not exactly one character — refused (malformed action).
    InvalidAction,
}

/// Process every currently-pending action to completion, resolving each (`done` on
/// [`ApplyOutcome::Applied`], `failed`+message otherwise). Re-fetches until the queue
/// is empty so actions enqueued concurrently are also served. An unexpected error in
/// one action marks just that action `failed`; it never aborts the drain.
pub fn process_pending_actions(conn: &Connection, vault_root: &Path) -> Result<()> {
    loop {
        let pending = db::pending_actions(conn).context("fetching pending actions")?;
        if pending.is_empty() {
            break;
        }
        for action in &pending {
            let (state, message) = match process_action(conn, vault_root, action) {
                Ok(ApplyOutcome::Applied) => {
                    tracing::info!(id = action.id, task = %action.task_id, "applied checkbox flip");
                    // Re-index the note so its stored `note_hash` reflects the bytes
                    // we just wrote. Without this, a *second* pending action on the
                    // same note would see a stale hash and be refused even though the
                    // only change was our own (legitimate) flip.
                    if let Err(e) =
                        index_note(conn, &vault_root.join(&action.note_path), vault_root)
                    {
                        tracing::warn!(
                            id = action.id, err = %e,
                            "post-apply re-index failed; action still marked done"
                        );
                    }
                    ("done", None::<String>)
                }
                Ok(outcome) => {
                    let msg = match outcome {
                        ApplyOutcome::ConflictNoteChanged => {
                            "note changed externally since scan; action not applied"
                        }
                        ApplyOutcome::TaskNotFound => {
                            "task no longer in index (or note gone); action not applied"
                        }
                        ApplyOutcome::TaskLineMismatch => {
                            "checkbox line no longer matches expected bytes; action not applied"
                        }
                        ApplyOutcome::InvalidAction => "invalid new_char; action not applied",
                        ApplyOutcome::Applied => unreachable!(),
                    };
                    tracing::warn!(id = action.id, outcome = ?outcome, "{msg}");
                    ("failed", Some(msg.to_string()))
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::error!(id = action.id, err = %e, "process_action errored");
                    ("failed", Some(msg))
                }
            };
            db::resolve_action(conn, action.id, state, message.as_deref())
                .with_context(|| format!("resolving action {}", action.id))?;
        }
    }
    Ok(())
}

/// Execute one pending checkbox flip per ADR-0002/0004/0005. The vault is mutated
/// **only** on a successful [`ApplyOutcome::Applied`]; every other path leaves it
/// untouched.
///
/// Sequence:
/// 1. Validate `new_char` is exactly one character (H1) and decode it to a `char`.
/// 2. Look up the current task row by `action.task_id` (surrogate rowid, ADR-0005).
/// 3. Read the note fresh.
/// 4. Authoritative conflict check: current content hash == stored `note_hash`.
/// 5. **Target the row's CURRENT `line_number`** (ADR-0005 §4), not the stale
///    `action.line_number` (which is now audit-only).
/// 6. Three-way byte verification on the on-disk checkbox char (M2 idempotency):
///    - equals `new_char` → already done (e.g. retry after a crash) → `Applied`;
///    - equals `expected_char` → flip it;
///    - anything else → `TaskLineMismatch`.
///
///    Plus a guard (ADR-0005 §4): if `action.expected_char != row.raw_checkbox_char`
///    (the checkbox was changed in Obsidian between enqueue and execute, and the
///    re-scan updated the row), refuse with `TaskLineMismatch`.
/// 7. Flip exactly that one char (byte-level surgery — preserves every other byte,
///    including line endings).
/// 8. Atomic write (temp file in same dir → fsync → re-verify hash → rename) with a
///    final TOCTOU check (C1).
pub fn process_action(
    conn: &Connection,
    vault_root: &Path,
    action: &PendingAction,
) -> Result<ApplyOutcome> {
    // 1. Validate new_char: it must be exactly one Unicode scalar value (H1). Decode
    //    it once here so every later step can use a `char` directly.
    let new_c = match single_char(&action.new_char) {
        Some(c) => c,
        None => return Ok(ApplyOutcome::InvalidAction),
    };

    // 2. Current task row (the DB row is a *claim*, not truth — bytes are re-verified).
    let Some(row) = lookup_task_for_action(conn, action.task_id)? else {
        return Ok(ApplyOutcome::TaskNotFound);
    };

    // 3. Read the note fresh from the vault.
    let note_abs = vault_root.join(&row.note_path);
    let bytes = match fs::read(&note_abs) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(ApplyOutcome::TaskNotFound),
        Err(e) => return Err(e).with_context(|| format!("reading note {:?}", note_abs)),
    };

    // 4. Authoritative conflict check via content hash (mtime is informational only).
    let snapshot_hash = content_hash(&bytes);
    if Some(&snapshot_hash) != row.note_hash.as_ref() {
        return Ok(ApplyOutcome::ConflictNoteChanged);
    }

    // 5. Target the task's CURRENT line (ADR-0005 §4) — the row's `line_number`,
    //    updated by reconciliation if the task moved, NOT the stale `action.line_number`.
    let Some(line_range) = line_byte_range(&bytes, row.line_number) else {
        return Ok(ApplyOutcome::TaskLineMismatch);
    };

    // 6. Three-way byte verification (M2) + row-level guard.
    let Some((on_disk_c, char_range)) = find_checkbox_char_any(&bytes, line_range.clone()) else {
        return Ok(ApplyOutcome::TaskLineMismatch);
    };
    if on_disk_c == new_c {
        // The flip is already present on disk — e.g. we applied it, crashed before
        // resolving, and on restart the re-scan updated the hash. Treat as done and
        // do not touch the file (idempotent).
        return Ok(ApplyOutcome::Applied);
    }
    // Guard (ADR-0005 §4): the row's checkbox char was updated by a re-scan since
    // enqueue, meaning the user changed the checkbox in Obsidian. Refuse rather than
    // act on a stale expected_char. (This fires AFTER the M2 idempotency check so
    // crash-recovery idempotent applies still succeed.)
    if action.expected_char != row.raw_checkbox_char {
        return Ok(ApplyOutcome::TaskLineMismatch);
    }
    // Decode expected_char for the on-disk comparison. If malformed, the on-disk char
    // can't equal the intended "before" state, so refuse.
    let Some(expected_c) = single_char(&action.expected_char) else {
        return Ok(ApplyOutcome::TaskLineMismatch);
    };
    if on_disk_c != expected_c {
        return Ok(ApplyOutcome::TaskLineMismatch);
    }

    // 7. Flip exactly the single checkbox char. Byte-level surgery means nothing else
    //    (whitespace, line endings, other lines) can change.
    let mut new_bytes = Vec::with_capacity(bytes.len() + 4);
    new_bytes.extend_from_slice(&bytes[..char_range.start]);
    new_bytes.extend_from_slice(new_c.encode_utf8(&mut [0u8; 4]).as_bytes());
    new_bytes.extend_from_slice(&bytes[char_range.end..]);

    // 8. Atomic write with a final TOCTOU check (C1): right before the rename, re-read
    //    the target and re-hash; if it no longer matches `snapshot_hash`, refuse rather
    //    than clobber an edit that landed in our window.
    match atomic_write(&note_abs, &new_bytes, &snapshot_hash)? {
        WriteResult::Written => Ok(ApplyOutcome::Applied),
        WriteResult::Conflict => Ok(ApplyOutcome::ConflictNoteChanged),
    }
}

/// If `s` holds exactly one Unicode scalar value, return it; otherwise `None`.
fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    (chars.next().is_none()).then_some(c)
}

/// The slice of a task row needed to execute an action.
struct TaskRow {
    note_path: String,
    /// The task's CURRENT line number (updated by reconciliation if the task moved).
    /// process_action targets this line, not the stale `action.line_number`.
    line_number: usize,
    /// The task's current checkbox char — compared to `action.expected_char` as a
    /// guard against the checkbox having been flipped between enqueue and execute.
    raw_checkbox_char: String,
    note_hash: Option<String>,
}

/// Fetch [`TaskRow`] for a task id (surrogate rowid), or `None` if the task is no
/// longer indexed.
fn lookup_task_for_action(conn: &Connection, task_id: i64) -> Result<Option<TaskRow>> {
    let row = conn.query_row(
        "SELECT note_path, line_number, raw_checkbox_char, note_hash
         FROM tasks WHERE id = ?1",
        rusqlite::params![task_id],
        |row| {
            Ok(TaskRow {
                note_path: row.get(0)?,
                line_number: row.get::<_, i64>(1)? as usize,
                raw_checkbox_char: row.get(2)?,
                note_hash: row.get(3)?,
            })
        },
    );
    match row {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("looking up task {task_id}")),
    }
}

/// Byte range `[start, end)` of line `line_number` (1-based) in `bytes`, where lines
/// are delimited by `'\n'`. Returns `None` if the line number is out of range. A
/// trailing `'\n'` is a terminator, not an extra line — this matches how
/// `taski_core::parse_tasks` numbers lines via `str::lines`.
fn line_byte_range(bytes: &[u8], line_number: usize) -> Option<std::ops::Range<usize>> {
    if line_number == 0 {
        return None;
    }
    let mut start = 0usize;
    for _ in 1..line_number {
        match bytes[start..].iter().position(|&b| b == b'\n') {
            Some(pos) => start += pos + 1,
            None => return None, // fewer lines than requested
        }
    }
    let end = bytes[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |p| start + p);
    Some(start..end)
}

/// Within `bytes[line_range]`, find the first `[`, verify it opens a `[\u{1}]`
/// checkbox, and return the decoded char plus its **absolute** byte range. `None` if
/// the pattern isn't a well-formed single-char checkbox. The caller decides what the
/// char should be (M2 three-way check) — this function only reports what is there.
fn find_checkbox_char_any(
    bytes: &[u8],
    line_range: std::ops::Range<usize>,
) -> Option<(char, std::ops::Range<usize>)> {
    let line = &bytes[line_range.clone()];
    let bracket_rel = line.iter().position(|&b| b == b'[')?;
    let char_start_rel = bracket_rel + 1;
    if char_start_rel >= line.len() {
        return None;
    }
    // Decode exactly one UTF-8 char after `[`.
    let rest = std::str::from_utf8(&line[char_start_rel..]).ok()?;
    let ch = rest.chars().next()?;
    let close_rel = char_start_rel + ch.len_utf8();
    if close_rel >= line.len() || line[close_rel] != b']' {
        return None;
    }
    Some((
        ch,
        (line_range.start + char_start_rel)..(line_range.start + close_rel),
    ))
}

/// Result of [`atomic_write`]: either the bytes were committed, or a concurrent edit
/// was detected at the last moment and the write was aborted (C1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteResult {
    Written,
    Conflict,
}

/// Atomically replace `path`'s contents with `content`, but **only** if the file still
/// hashes to `expected_hash` at the instant of the rename (C1 TOCTOU guard):
///
/// 1. write `<name>.taski.tmp` in the same directory → `fsync`;
/// 2. capture the original file's permissions and apply them to the temp (M3);
/// 3. **re-read the target, re-hash, and compare to `expected_hash`** — if it differs,
///    remove the temp and return [`WriteResult::Conflict`] (the vault is untouched);
/// 4. otherwise `rename` the temp over the target (atomic on POSIX/APFS — same FS).
///
/// There is deliberately **no I/O between the step-3 re-read and the rename**. Any
/// error cleans up the temp file so no partial write is left behind.
fn atomic_write(path: &Path, content: &[u8], expected_hash: &str) -> Result<WriteResult> {
    let dir = path
        .parent()
        .with_context(|| format!("path {path:?} has no parent"))?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .with_context(|| format!("path {path:?} has no file name"))?;
    let tmp = dir.join(format!("{file_name}.taski.tmp"));

    let attempt = (|| -> Result<WriteResult> {
        let mut file = fs::File::create(&tmp).with_context(|| format!("creating temp {tmp:?}"))?;

        // M3: preserve the original file's permission bits on the temp (best-effort —
        // a failure here is logged and ignored; the write still proceeds).
        if let Ok(orig_meta) = fs::metadata(path) {
            let mode = orig_meta.permissions().mode();
            if let Err(e) = file.set_permissions(fs::Permissions::from_mode(mode)) {
                tracing::warn!(?tmp, err = %e, "could not clone file mode onto temp");
            }
        }

        file.write_all(content)
            .with_context(|| format!("writing temp {tmp:?}"))?;
        file.sync_all()
            .with_context(|| format!("fsyncing temp {tmp:?}"))?;
        drop(file);

        // C1: final TOCTOU guard. Re-read the target's current bytes and re-hash; if
        // they no longer match what we verified in `process_action`, refuse instead of
        // clobbering an edit that landed in our window. The rename immediately below is
        // the very next I/O, minimizing the race window.
        let current = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // The note vanished between our read and now — treat as a conflict.
                return Ok(WriteResult::Conflict);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("re-reading {path:?} for TOCTOU check"));
            }
        };
        if content_hash(&current) != expected_hash {
            return Ok(WriteResult::Conflict);
        }

        fs::rename(&tmp, path).with_context(|| format!("renaming {tmp:?} -> {path:?}"))?;
        Ok(WriteResult::Written)
    })();

    if !matches!(attempt, Ok(WriteResult::Written)) {
        // Best-effort cleanup of the temp on any non-Written path (Conflict or error);
        // a missing temp file is not an error here.
        let _ = fs::remove_file(&tmp);
    }
    attempt
}

/// Stable content hash of the note's raw bytes (change-detection, not crypto). Same
/// inputs always hash identically within a daemon run, which is all the conflict
/// check needs (hashes are only ever compared against ones this process produced).
fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    format!("{:016x}", hasher.finish())
}

/// The note's mtime in unix seconds, if obtainable. Informational (the hash is the
/// authoritative conflict signal); `None` if the syscall fails.
fn note_mtime(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// Current unix time in seconds, or 0 if the clock is before the epoch. Used to
/// compute the `pending_actions` prune cutoff at startup.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Remove any leftover `<name>.taski.tmp` files under `vault_root` left behind by a
/// crash mid-write-back (ADR-0003). Walks the vault with the same hidden-directory
/// pruning as [`scan_vault`]. Returns the count of files removed. A failure to remove
/// one file is logged and skipped — it never aborts the sweep.
pub fn sweep_tmp_files(vault_root: &Path) -> Result<usize> {
    let mut removed = 0usize;
    for entry in WalkDir::new(vault_root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_hidden_dir_entry(e))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(err = %e, "walkdir error during tmp sweep; skipping entry");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy();
        if !name.ends_with(".taski.tmp") {
            continue;
        }
        match fs::remove_file(path) {
            Ok(()) => {
                tracing::warn!(?path, "swept stale temp file");
                removed += 1;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(?path, err = %e, "could not remove stale temp file"),
        }
    }
    Ok(removed)
}

/// Initialise `tracing` stderr output. Honors `RUST_LOG`; defaults to `info`. Safe to
/// call when a subscriber is already installed (e.g. when running under a test).
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C1: when `expected_hash` matches the on-disk content, `atomic_write` commits the
    /// new bytes and reports [`WriteResult::Written`].
    #[test]
    fn atomic_write_commits_when_hash_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note.md");
        let original = b"- [ ] a\n";
        fs::write(&path, original).unwrap();
        let hash = content_hash(original);

        let new_bytes = b"- [x] a\n";
        let res = atomic_write(&path, new_bytes, &hash).expect("write");
        assert_eq!(res, WriteResult::Written);
        assert_eq!(fs::read(&path).unwrap(), new_bytes);
        // No temp left behind.
        assert!(!dir.path().join("note.md.taski.tmp").exists());
    }

    /// C1: when `expected_hash` does NOT match the on-disk content (a concurrent edit
    /// landed in the window), `atomic_write` refuses, leaves the target untouched, and
    /// cleans up the temp file.
    #[test]
    fn atomic_write_refuses_when_hash_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note.md");
        fs::write(&path, b"- [ ] a\n").unwrap();

        // Pass a deliberately wrong expected_hash to force the Conflict path
        // deterministically.
        let wrong_hash = content_hash(b"something completely different");
        let new_bytes = b"- [x] a\n";
        let res = atomic_write(&path, new_bytes, &wrong_hash).expect("write");
        assert_eq!(res, WriteResult::Conflict);
        // The target was not touched.
        assert_eq!(fs::read(&path).unwrap(), b"- [ ] a\n");
        // The temp was cleaned up.
        assert!(!dir.path().join("note.md.taski.tmp").exists());
    }
}
