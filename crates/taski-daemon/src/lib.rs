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

mod lock;
mod shutdown;

pub use lock::{DaemonLockGuard, LockOutcome, acquire_daemon_lock, daemon_lock_path};
pub use shutdown::{ShutdownHandle, ShutdownSignal};

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
use taski_core::{
    RewriteResult, inbox_line_for, parse_tasks, rewrite_cancelled_date, rewrite_done_date,
    rewrite_scheduled, toggle_bullet,
};
use taski_db as db;
use taski_db::PendingAction;
use walkdir::WalkDir;

/// Retention window for resolved `pending_actions`: rows in `done`/`failed` state
/// older than this many seconds are pruned on daemon startup (M2 housekeeping) so
/// the table does not grow without bound. Seven days balances auditability against
/// unbounded growth.
const ACTION_RETENTION_SECS: i64 = 7 * 86_400;

/// CLI configuration (PRD §12, Slice 1). `--vault` and `--db` are optional and
/// override the values in the config file (`~/.config/taski/config.toml`, overridable
/// via `TASKI_CONFIG`); see `taski_config`. If neither CLI nor config provides a
/// `vault`, the daemon errors; `db` defaults to `./taski.db`.
#[derive(Parser, Debug)]
#[command(
    name = "taski-daemon",
    version,
    about = "Watch an Obsidian vault and index checkbox tasks into SQLite"
)]
pub struct Cli {
    /// Path to the Obsidian vault root to scan and watch. Overrides `vault` in the
    /// config file; required if absent there (the daemon has no default vault).
    #[arg(long)]
    pub vault: Option<PathBuf>,
    /// Path to the taski SQLite index database. Overrides `db` in the config file;
    /// defaults to `./taski.db` if absent everywhere.
    #[arg(long)]
    pub db: Option<PathBuf>,
    /// Run a single full scan and exit (do not start the watch loop).
    #[arg(long)]
    pub once: bool,
    /// Write a ready-to-use config to the effective config path
    /// (`~/.config/taski/config.toml`, or `$TASKI_CONFIG`) and exit — do not run the
    /// daemon. If `--vault` is also given it is baked into the file; otherwise a
    /// commented placeholder is written for you to fill in. Refuses to overwrite an
    /// existing config.
    #[arg(long)]
    pub init_config: bool,
}

/// Resolved options for [`run_daemon`], decoupling the engine from the CLI parser so
/// the future unified launcher can drive it without constructing a `Cli`. Built from
/// a [`Cli`] via [`From`], or constructed directly by the launcher.
#[derive(Clone, Debug)]
pub struct DaemonOpts {
    /// Path to the Obsidian vault root to scan and watch. Required if absent from the
    /// config file (the daemon has no default vault).
    pub vault: Option<PathBuf>,
    /// Path to the taski SQLite index database. Defaults to `./taski.db` if absent
    /// everywhere.
    pub db: Option<PathBuf>,
    /// Run a single full scan and exit (do not start the watch loop).
    pub once: bool,
}

impl From<&Cli> for DaemonOpts {
    fn from(cli: &Cli) -> Self {
        DaemonOpts {
            vault: cli.vault.clone(),
            db: cli.db.clone(),
            once: cli.once,
        }
    }
}

/// Standalone entry point invoked by the `taski-daemon` binary's `main`. Parses CLI
/// args, sets up tracing, handles `--init-config`, installs the Ctrl-C handler, then
/// delegates to [`run_daemon`] with a fresh [`ShutdownSignal`]/[`ShutdownHandle`] pair.
///
/// This is the only daemon entry point that installs a Ctrl-C handler: the future
/// unified launcher drives shutdown itself (via the shared flag) and must not
/// double-install. The standalone behavior (first Ctrl-C → clean shutdown) is
/// preserved, now via the shared flag instead of an internal channel.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    // `--init-config` is a setup-only action: write a config file and exit before
    // any vault/db resolution or scanning. Handled before `load()` so it works even
    // with no (or malformed) existing config.
    if cli.init_config {
        return write_initial_config(cli.vault.as_deref());
    }

    // Ctrl-C → set the shared shutdown flag. The first signal initiates a clean
    // shutdown; a second falls back to the default (terminate). Installed here only
    // (not inside `run_daemon` / `run_watch_loop`) so the launcher can own shutdown.
    let (signal, handle) = ShutdownSignal::new();
    ctrlc::set_handler(move || signal.set()).context("installing Ctrl-C handler")?;

    // Single-writer enforcement (ADR-0008): acquire the daemon lock beside the resolved
    // db before running. The guard is passed into `run_daemon` as a capability token and
    // held for the engine's whole lifetime (released on exit). Refuse if another daemon
    // already holds it.
    let cfg = taski_config::load().context("loading taski config")?;
    let db_path = taski_config::resolve_db(cli.db.as_deref().and_then(Path::to_str), &cfg);
    let lock_path = daemon_lock_path(&db_path);
    let guard = match acquire_daemon_lock(&lock_path).context("acquiring daemon lock")? {
        LockOutcome::Acquired(g) => g,
        LockOutcome::HeldByOther(pid) => {
            eprintln!(
                "taski-daemon: another daemon is already running{}. Refusing to start a \
                 second writer (ADR-0008). Stop it first, or run a TUI against it.",
                pid.map(|p| format!(" (PID {p})")).unwrap_or_default()
            );
            anyhow::bail!("daemon already running");
        }
    };

    run_daemon(DaemonOpts::from(&cli), handle, guard)
}

/// Run the daemon engine against resolved options: load config, resolve vault/db,
/// open the DB connection, sweep stale temps, prune old actions, perform the initial
/// scan, and (unless `opts.once`) enter the watch loop. Shutdown is observed via the
/// shared `shutdown` handle.
///
/// This is the function the future unified launcher will call on a background
/// thread. It does NOT parse CLI args, call `init_tracing`, handle `--init-config`,
/// or install a Ctrl-C handler — those are the caller's responsibilities (see
/// [`run`]). `init_tracing` and `write_initial_config` stay in the standalone entry.
///
/// The `_lock` guard is the **single-writer capability** (ADR-0008): the caller MUST
/// have acquired the daemon lock and pass it here. It is held for the engine's whole
/// lifetime and dropped on return, releasing the lock exactly when the daemon stops —
/// so every daemon entry point (standalone `run`, `taski daemon`, combined mode) is
/// uniformly single-writer-protected at the type level.
pub fn run_daemon(
    opts: DaemonOpts,
    shutdown: ShutdownHandle,
    _lock: DaemonLockGuard,
) -> Result<()> {
    // Config is optional (a missing file yields defaults); a malformed file is a
    // hard error.
    let cfg = taski_config::load().context("loading taski config")?;

    // Resolve vault/db: CLI flag → config file → compiled default. The daemon
    // requires a vault (no default); db defaults to ./taski.db.
    let vault_root =
        taski_config::resolve_vault(opts.vault.as_deref().and_then(Path::to_str), &cfg)?;
    let vault_root = vault_root
        .canonicalize()
        .with_context(|| format!("canonicalizing vault path {:?}", vault_root))?;

    let db_path = taski_config::resolve_db(opts.db.as_deref().and_then(Path::to_str), &cfg);
    let conn = db::open(&db_path.to_string_lossy())
        .with_context(|| format!("opening taski database {:?}", db_path))?;

    // User-configured directory excludes (relative to vault root).
    let exclude_dirs = &cfg.exclude_dirs;

    // Purge any previously-indexed tasks that now fall under an excluded directory,
    // so the index stays consistent when the user adds or changes exclude_dirs.
    if !exclude_dirs.is_empty() {
        match db::delete_tasks_for_excluded_dirs(&conn, exclude_dirs) {
            Ok(()) => tracing::info!("purged indexed tasks inside excluded directories"),
            Err(e) => tracing::warn!(err = %e, "purging excluded-dir tasks failed; continuing"),
        }
    }

    // Sweep any temp files left behind by a crash mid-write-back (ADR-0003). Do this
    // BEFORE the scan so the freshly-written scan reflects a clean vault state.
    match sweep_tmp_files(&vault_root, exclude_dirs) {
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

    let total = scan_vault(&conn, &vault_root, exclude_dirs)?;
    tracing::info!(count = total, ?vault_root, "initial scan complete");

    if opts.once {
        return Ok(());
    }

    run_watch_loop(&conn, &vault_root, &shutdown, exclude_dirs)?;
    Ok(())
}

/// Implements `--init-config`: write a ready-to-use config to the effective config
/// path and return without running the daemon. Refuses to clobber an existing file.
fn write_initial_config(vault: Option<&Path>) -> Result<()> {
    let target = taski_config::config_path();
    let db = default_db_path();
    write_config_to(&target, vault, &db)?;
    println!("Wrote {}.", target.display());
    match vault {
        Some(_) => {
            println!("Edit if needed, then start the daemon (or run scripts/install-launchd.sh).")
        }
        None => println!(
            "Open it and set `vault` to your Obsidian vault path before starting the daemon."
        ),
    }
    Ok(())
}

/// Write a config template to `target`, creating parent dirs and refusing to overwrite
/// an existing file. Separated from [`write_initial_config`] so the write/exists logic
/// can be tested against a temp path rather than the real, environment-derived path.
fn write_config_to(target: &Path, vault: Option<&Path>, db: &str) -> Result<()> {
    if target.exists() {
        anyhow::bail!(
            "config already exists at {}; remove or edit it instead of --init-config",
            target.display()
        );
    }
    let body = taski_config::template(vault.and_then(Path::to_str), db);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    fs::write(target, body).with_context(|| format!("writing config {}", target.display()))
}

/// Conventional default DB path baked into a generated config:
/// `$HOME/.local/share/taski/taski.db`, falling back to `./taski.db` if `$HOME` is
/// unset.
fn default_db_path() -> String {
    match std::env::var_os("HOME") {
        Some(home) => Path::new(&home)
            .join(".local/share/taski/taski.db")
            .to_string_lossy()
            .into_owned(),
        None => "./taski.db".to_string(),
    }
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

    // ADR-0017: a note whose frontmatter carries `taski-skip: true` contributes no tasks.
    // Reconcile with an empty list evicts any previously-indexed rows (reconcile_note deletes
    // unmatched old rows), so toggling the flag is self-healing on the next scan. The note's
    // body is not cached (its tasks never surface in the context pane). Read-path only: no
    // pending_actions, no vault mutation, no write-back ADR touched.
    if taski_core::taski_skip_enabled(markdown) {
        tracing::debug!(?rel, "taski-skip frontmatter set; suppressing tasks");
        db::reconcile_note(conn, &rel, &[], Some(&hash), mtime)
            .with_context(|| format!("reconciling tasks for {rel:?}"))?;
        return Ok(0);
    }

    let tasks = parse_tasks(markdown, &rel);
    let summary = db::reconcile_note(conn, &rel, &tasks, Some(&hash), mtime)
        .with_context(|| format!("reconciling tasks for {rel:?}"))?;
    // ADR-0006: cache the note's full text in the same scan pass so the read-only TUI
    // can render a task's surrounding context without ever opening the vault. Content,
    // note_hash, and task line_numbers all derive from this same byte snapshot, so any
    // single poll the TUI performs sees an internally consistent view.
    db::upsert_note_content(conn, &rel, markdown, Some(&hash))
        .with_context(|| format!("caching note content for {rel:?}"))?;
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
/// descended into; user-configured exclude directories (relative to vault root) are
/// also skipped; non-`.md` files are skipped. Returns the total number of tasks
/// indexed. A single bad note logs and is skipped; it does not abort the scan.
pub fn scan_vault(conn: &Connection, vault_root: &Path, exclude_dirs: &[String]) -> Result<usize> {
    let mut total = 0usize;
    for entry in WalkDir::new(vault_root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !should_exclude_entry(e, vault_root, exclude_dirs))
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
/// the shared `shutdown` handle is set (by the standalone Ctrl-C handler or, later,
/// the unified launcher on TUI quit), then returns cleanly so the DB connection
/// drops gracefully.
///
/// Note: `notify-debouncer-mini` deliberately does not preserve event kind (it only
/// reports "something changed at this path"), so create/modify vs. remove is decided
/// by checking file existence in [`handle_debounced_event`].
fn run_watch_loop(
    conn: &Connection,
    vault_root: &Path,
    shutdown: &ShutdownHandle,
    exclude_dirs: &[String],
) -> Result<()> {
    let (event_tx, event_rx) = mpsc::channel::<DebounceEventResult>();
    let mut debouncer =
        new_debouncer(Duration::from_millis(300), event_tx).context("creating file watcher")?;
    debouncer
        .watcher()
        .watch(vault_root, RecursiveMode::Recursive)
        .with_context(|| format!("watching vault {:?}", vault_root))?;
    tracing::info!(?vault_root, "watching vault for changes (Ctrl-C to stop)");

    loop {
        match event_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(events)) => {
                for event in events {
                    handle_debounced_event(conn, &event.path, vault_root, exclude_dirs);
                }
            }
            Ok(Err(e)) => tracing::error!(err = %e, "watcher error"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!("watcher event channel closed; exiting watch loop");
                break;
            }
        }
        if shutdown.is_set() {
            // Final drain: apply any toggles the TUI queued this session before exiting
            // (ADR-0007), so "quit" means "everything I did has landed". The flag can only
            // be set after the TUI's main loop returns, so no further enqueues are possible.
            // Pending rows are durable in SQLite regardless; this is about session UX
            // completeness. Errors are logged, not fatal.
            tracing::info!("shutdown signal received; draining pending actions then exiting");
            if let Err(e) = process_pending_actions(conn, vault_root) {
                tracing::error!(err = %e, "final pending-actions drain failed");
            }
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
fn handle_debounced_event(
    conn: &Connection,
    abs_path: &Path,
    vault_root: &Path,
    exclude_dirs: &[String],
) {
    if !is_markdown(abs_path) {
        return;
    }
    if traverses_hidden_dir(abs_path, vault_root) {
        return;
    }
    if !exclude_dirs.is_empty() && path_matches_exclude(abs_path, vault_root, exclude_dirs) {
        return;
    }
    if abs_path.exists() {
        match index_note(conn, abs_path, vault_root) {
            Ok(n) => tracing::info!(path = ?abs_path, count = n, "re-indexed note"),
            Err(e) => tracing::error!(?abs_path, err = %e, "index_note failed on watch event"),
        }
    } else {
        match relative_to_vault(abs_path, vault_root) {
            Ok(rel) => {
                match db::delete_tasks_for_note(conn, &rel) {
                    Ok(()) => tracing::info!(?rel, "removed tasks for deleted note"),
                    Err(e) => tracing::error!(?rel, err = %e, "delete_tasks_for_note failed"),
                }
                // ADR-0006: drop the cached content too, so the index never carries
                // content for a deleted note.
                if let Err(e) = db::delete_note_content(conn, &rel) {
                    tracing::error!(?rel, err = %e, "delete_note_content failed");
                }
            }
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

/// True if a directory entry should be excluded from vault walking: either it is a
/// hidden directory (name starts with `.`), or it is one of the user-configured
/// exclude directories (matched as a relative path from the vault root).
///
/// For user-configured exclusions, both the exact path and any path *inside* it are
/// excluded. An `exclude_dirs` entry like `"templates"` matches the `templates/`
/// directory itself, `templates/daily/`, etc. A leading slash is not required.
fn should_exclude_entry(e: &walkdir::DirEntry, vault_root: &Path, exclude_dirs: &[String]) -> bool {
    // Always prune hidden directories (unchanged behavior).
    if e.file_type().is_dir() && e.file_name().to_string_lossy().starts_with('.') {
        return true;
    }
    // Check user-configured exclusions.
    if !exclude_dirs.is_empty()
        && let Ok(rel) = e.path().strip_prefix(vault_root)
    {
        let rel_str = rel.to_string_lossy();
        if exclude_dirs.iter().any(|excl| {
            let excl = excl.trim_end_matches('/');
            rel_str == excl || rel_str.starts_with(&format!("{}/", excl))
        }) {
            return true;
        }
    }
    false
}

/// True if `abs_path` (an absolute path, e.g. from a filesystem event) is inside (or
/// matches) one of the user-configured exclude directories. The path is relativised to
/// `vault_root` first, so exclusions like `"templates"` match `templates/daily.md` as
/// well as `templates/`.
fn path_matches_exclude(abs_path: &Path, vault_root: &Path, exclude_dirs: &[String]) -> bool {
    let Ok(rel) = abs_path.strip_prefix(vault_root) else {
        return false;
    };
    let rel_str = rel.to_string_lossy();
    exclude_dirs.iter().any(|excl| {
        let excl = excl.trim_end_matches('/');
        rel_str == excl || rel_str.starts_with(&format!("{}/", excl))
    })
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

/// Outcome of attempting one pending write-back action. Anything other than
/// [`Applied`] means the action was **refused** — the vault is left untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The write was applied atomically (or was already a no-op idempotent match).
    Applied,
    /// The note changed externally since scan (content hash mismatch) — refused.
    ConflictNoteChanged,
    /// The task is no longer in the index (deleted) or its note is gone — refused.
    TaskNotFound,
    /// The target line no longer holds the expected checkbox bytes — refused.
    TaskLineMismatch,
    /// `new_char` is not exactly one character — refused (malformed action).
    InvalidAction,
    /// ADR-0009 Phase 2: an existing `⏳` on the line is malformed (bad date,
    /// NBSP, stray variation selectors, more than one `⏳`), so
    /// `rewrite_scheduled` refused to guess — the action is not applied.
    MetadataUnparseable,
    /// ADR-0011: `taski_core::toggle_bullet` returned `Unparseable` — the line
    /// is not a valid checkbox or bullet format.
    BulletUnparseable,
    /// ADR-0012: an existing `✅` on the line is malformed (bad date, NBSP,
    /// stray variation selectors, more than one `✅`), so `rewrite_done_date`
    /// refused to guess — the whole toggle (flip + stamp) is refused, the vault
    /// is untouched. Parallel to [`ApplyOutcome::MetadataUnparseable`] on the
    /// `✅` axis.
    DoneDateUnparseable,
    /// ADR-0013: an existing `❌` on the line is malformed (bad date, NBSP,
    /// stray variation selectors, more than one `❌`), so
    /// `rewrite_cancelled_date` refused to guess — the whole cancel flip
    /// (flip + stamp) is refused, the vault is untouched. Parallel to
    /// [`ApplyOutcome::DoneDateUnparseable`] on the `❌` axis.
    CancelledDateUnparseable,
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
            // ADR-0009 Phase 2: dispatch on `action_type` at the top of the drain
            // loop. The proven checkbox path (`process_action`) and its 256-case
            // proptest are byte-for-byte unchanged; only this dispatch branch is
            // added. Unknown types are refused inline with a distinct message
            // (NOT routed through `InvalidAction`, whose `new_char` wording would be
            // wrong for an unrecognized write gesture).
            let outcome: Result<ApplyOutcome> = match action.action_type.as_str() {
                "checkbox" => process_action(conn, vault_root, action),
                "set_scheduled" => process_metadata_action(conn, vault_root, action),
                "toggle_bullet" => process_bullet_action(conn, vault_root, action),
                // ADR-0014: quick-add (append-only creation) and its undo.
                // These don't take `conn` — they only touch the vault file; the
                // drain loop re-indexes `action.note_path` (the inbox) after
                // `Applied`, same as every other action type.
                "quick_add" => process_quick_add(vault_root, action),
                "quick_add_undo" => process_quick_add_undo(vault_root, action),
                // Unknown action_type: refuse with a distinct, accurate message.
                // (NOT the checkbox `new_char` phrasing — there is no `new_char`
                // here; this is an unrecognized write gesture.)
                other => {
                    tracing::warn!(
                        id = action.id,
                        action_type = other,
                        "unknown action_type; action not applied"
                    );
                    db::resolve_action(
                        conn,
                        action.id,
                        "failed",
                        Some("unknown action_type; action not applied"),
                    )
                    .with_context(|| format!("resolving action {}", action.id))?;
                    continue;
                }
            };
            let (state, message) = match outcome {
                Ok(ApplyOutcome::Applied) => {
                    tracing::info!(
                        id = action.id,
                        task = %action.task_id,
                        kind = %action.action_type,
                        "applied action"
                    );
                    // Re-index the note so its stored `note_hash` reflects the bytes
                    // we just wrote. Without this, a *second* pending action on the
                    // same note would see a stale hash and be refused even though the
                    // only change was our own (legitimate) write. Applies to both
                    // checkbox flips and `set_scheduled` writes.
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
                        ApplyOutcome::MetadataUnparseable => {
                            "scheduled date is malformed or unparseable; action not applied"
                        }
                        ApplyOutcome::BulletUnparseable => {
                            "line could not be converted to a bullet; action not applied"
                        }
                        ApplyOutcome::DoneDateUnparseable => {
                            "existing ✅ is malformed or unparseable; toggle not applied"
                        }
                        ApplyOutcome::CancelledDateUnparseable => {
                            "existing ❌ is malformed or unparseable; cancel not applied"
                        }
                        ApplyOutcome::Applied => unreachable!(),
                    };
                    tracing::warn!(id = action.id, outcome = ?outcome, "{msg}");
                    ("failed", Some(msg.to_string()))
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::error!(id = action.id, err = %e, "action processing errored");
                    ("failed", Some(msg))
                }
            };
            db::resolve_action(conn, action.id, state, message.as_deref())
                .with_context(|| format!("resolving action {}", action.id))?;
        }
    }
    Ok(())
}

/// Execute one pending checkbox flip per ADR-0002/0004/0005, **composing the
/// ADR-0012 `✅` done-date stamp into the same byte buffer as the flip** (one
/// write, one hash, one rename). The vault is mutated **only** on a successful
/// [`ApplyOutcome::Applied`]; every other path leaves it untouched.
///
/// This is the wall-clock wrapper: it derives `<today>` via the pure
/// [`taski_core::ymd_from_unix`] and delegates to [`process_action_at`].
/// Deterministic tests call [`process_action_at`] directly with a fixed date.
///
/// See [`process_action_at`] for the full sequence.
pub fn process_action(
    conn: &Connection,
    vault_root: &Path,
    action: &PendingAction,
) -> Result<ApplyOutcome> {
    let today = taski_core::ymd_from_unix(unix_now());
    process_action_at(conn, vault_root, action, &today)
}

/// Deterministic-seam inner behind [`process_action`] (ADR-0012): same as
/// `process_action` but with `<today>` supplied by the caller so byte-exact test
/// assertions are possible. Production callers use [`process_action`]; tests use
/// this with a fixed `"2026-06-21"`.
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
/// 7. **Compose the ADR-0012 `✅` stamp into the same splice as the flip.**
///    Build the post-flip line (checkbox char already flipped) and run the pure
///    [`taski_core::rewrite_done_date`] oracle on it when the transition warrants:
///    - `new_char` is a `Status::Done` char (`x`/`X`) → stamp `Some(today)`;
///    - `new_char == ' '` (open) → clear `None` (symmetry — un-complete);
///    - anything else (e.g. InProgress `/`) → skip the oracle; only the flip is
///      written (`✅` left untouched — ambiguous, do not guess).
///
///    If the oracle returns `Unparseable`, refuse the WHOLE action with
///    [`ApplyOutcome::DoneDateUnparseable`] — no flip, no stamp, vault untouched.
///    CR-trim the line range exactly as [`process_metadata_action`] does so the
///    stamp is spliced over `[line_range.start, content_end)` and the `\r\n` lives
///    outside the spliced region (the CRLF hazard, ADR-0012 Consequences).
/// 8. Atomic write (temp file in same dir → fsync → re-verify hash → rename) with a
///    final TOCTOU check (C1). Exactly ONE `atomic_write` — never two.
pub fn process_action_at(
    conn: &Connection,
    vault_root: &Path,
    action: &PendingAction,
    today: &str,
) -> Result<ApplyOutcome> {
    // 1. Validate new_char: it must be exactly one Unicode scalar value (H1). Decode
    //    it once here so every later step can use a `char` directly.
    let new_c = match single_char(&action.new_char) {
        Some(c) => c,
        None => return Ok(ApplyOutcome::InvalidAction),
    };

    // 2. Current task row (the DB row is a *claim*, not truth — bytes are re-verified).
    let row = match lookup_task_for_action(conn, action.task_id)? {
        Some(row) => row,
        None => {
            // ADR-0012: the ✅ stamp on a done-flip changes text_hash, causing
            // reconcile_note to assign a new surrogate id. Pending actions
            // referencing the old id (e.g. undo) would fail with TaskNotFound.
            // Fall back to the recorded (note_path, line_number) location.
            tracing::debug!(
                task_id = action.task_id,
                note_path = %action.note_path,
                line_number = action.line_number,
                "task_id not found; trying location fallback"
            );
            match lookup_task_by_location(conn, &action.note_path, action.line_number)? {
                Some(row) => row,
                None => return Ok(ApplyOutcome::TaskNotFound),
            }
        }
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

    // 7. Compose the ADR-0012 `✅` stamp into the same splice as the flip.
    //    This is the first time `process_action` decodes the target line to `&str`;
    //    until now it operated purely on byte ranges via `find_checkbox_char_any`.
    //    Mirror `process_metadata_action`'s CRLF discipline exactly: `line_byte_range`
    //    delimits lines on `\n` only, so a trailing `\r` (CRLF notes) is INCLUDED in
    //    `line_range`. Compute `content_end` so the oracle operates on the
    //    CR-trimmed content and the stamp is spliced over `[line_range.start,
    //    content_end)`, leaving `bytes[content_end..]` (the `\r\n`) untouched. Without
    //    this, `✅` would be written between the CR and LF, the next `parse_tasks`
    //    would fold the CR into the task body, and `text_hash` would be polluted.
    let content_end = if line_range.end > line_range.start && bytes[line_range.end - 1] == b'\r' {
        line_range.end - 1
    } else {
        line_range.end
    };
    let line_str = match std::str::from_utf8(&bytes[line_range.start..content_end]) {
        Ok(s) => s,
        Err(_) => return Ok(ApplyOutcome::TaskLineMismatch),
    };

    // Build the post-flip line (checkbox char already swapped for `new_c`). The
    // oracle sees the FLIPPED line, not the original. `char_range` is absolute in
    // `bytes`; convert to line-relative offsets for the &str splice.
    let char_rel_start = char_range.start - line_range.start;
    let char_rel_end = char_range.end - line_range.start;
    let mut flipped = String::with_capacity(line_str.len() + 4);
    flipped.push_str(&line_str[..char_rel_start]);
    flipped.push(new_c);
    flipped.push_str(&line_str[char_rel_end..]);

    // Decide the done/cancelled date deltas from `new_c` (ADR-0013 Decision — a
    // three-state widening of ADR-0012's two-state done/open model):
    //   - Done char (x/X per `Status::Done`) → stamp `✅ <today>`, CLEAR any `❌`
    //     (a task cannot be both done and cancelled)
    //   - Cancelled char (`-`)                → stamp `❌ <today>`, CLEAR any `✅`
    //   - Open char (` `)                     → CLEAR both `✅` and `❌`
    //   - anything else (InProgress `/`, …)   → skip both oracles; only the flip
    //                                            is written (both stamps untouched)
    //
    // Regression guard (ADR-0012): the done/open branches must drive the `✅`
    // oracle EXACTLY as before, so `done_date_writeback_proptest` stays byte-for-
    // byte green. The only additions on those branches are `❌`-oracle calls,
    // which return `RewriteResult::Unchanged` (no-op) on notes without a `❌`
    // token. The `✅` oracle runs first; the `❌` oracle runs on its result.
    let final_line: String = if is_done_char(new_c) || is_cancelled_char(new_c) || new_c == ' ' {
        let desired_done: Option<&str> = if is_done_char(new_c) {
            Some(today)
        } else {
            None
        };
        let desired_cancelled: Option<&str> = if is_cancelled_char(new_c) {
            Some(today)
        } else {
            None
        };
        // Run the ✅ oracle first. Malformed existing ✅ → refuse the WHOLE
        // action (no flip, no stamp, vault untouched).
        let after_done = match rewrite_done_date(&flipped, desired_done) {
            RewriteResult::Unparseable => return Ok(ApplyOutcome::DoneDateUnparseable),
            RewriteResult::Rewritten(s) => s,
            // Idempotent on the ✅ axis (e.g. ✅ already equals today, or
            // clearing a line with no ✅): carry `flipped` forward to the ❌
            // oracle. Clone because `flipped` is also the fallback below if the
            // ❌ oracle is itself a no-op.
            RewriteResult::Unchanged => flipped.clone(),
        };
        // Run the ❌ oracle on the ✅-oracle result. Malformed existing ❌ →
        // refuse the WHOLE action (no flip, no stamp, vault untouched).
        match rewrite_cancelled_date(&after_done, desired_cancelled) {
            RewriteResult::Unparseable => return Ok(ApplyOutcome::CancelledDateUnparseable),
            RewriteResult::Rewritten(s) => s,
            // Idempotent on the ❌ axis: `after_done` (which already encodes any
            // ✅ change) is the final line.
            RewriteResult::Unchanged => after_done,
        }
    } else {
        // Non-done/non-cancelled/non-open flip (e.g. →[/]): ambiguous, leave
        // both `✅` and `❌` untouched. Only the flip is written.
        flipped
    };

    // Splice the final line into the full note buffer over `[line_range.start,
    // content_end)` (every other byte and ALL line endings — including the `\r` in
    // a CRLF note — preserved, since they live in `bytes[content_end..]`). ONE
    // `atomic_write` follows — never two.
    let mut new_bytes = Vec::with_capacity(bytes.len() + final_line.len());
    new_bytes.extend_from_slice(&bytes[..line_range.start]);
    new_bytes.extend_from_slice(final_line.as_bytes());
    new_bytes.extend_from_slice(&bytes[content_end..]);

    // 8. Atomic write with a final TOCTOU check (C1): right before the rename, re-read
    //    the target and re-hash; if it no longer matches `snapshot_hash`, refuse rather
    //    than clobber an edit that landed in our window.
    match atomic_write(&note_abs, &new_bytes, &snapshot_hash)? {
        WriteResult::Written => Ok(ApplyOutcome::Applied),
        WriteResult::Conflict => Ok(ApplyOutcome::ConflictNoteChanged),
    }
}

/// True iff `ch` is a checkbox char the codebase maps to
/// [`taski_core::Status::Done`]. ADR-0012's `✅` stamp keys off this: a flip TO
/// one of these chars stamps `✅ <today>`. Sourced from
/// `taski_core::Status::from_checkbox_char`'s `Done` arm (`"x" | "X"`); mirrored
/// here (rather than allocating a `Status`) so the hot path stays allocation-free.
/// Keep in sync with that mapping if it ever widens.
fn is_done_char(ch: char) -> bool {
    matches!(ch, 'x' | 'X')
}

/// True iff `ch` is the checkbox char the codebase treats as **cancelled**
/// (`- [-]`, the Obsidian Tasks cancelled marker). ADR-0013's `❌` stamp keys off
/// this: a flip TO `-` stamps `❌ <today>`, and a flip FROM `-` clears it.
/// Mirrors [`is_done_char`] so the hot path stays allocation-free. Keep in sync
/// with `taski_core::Status::from_checkbox_char` if its cancelled mapping widens.
fn is_cancelled_char(ch: char) -> bool {
    matches!(ch, '-')
}

/// Execute one pending `set_scheduled` write (ADR-0009 Phase 2) — structurally
/// parallel to [`process_action`] and reusing the **same** [`atomic_write`],
/// [`lookup_task_for_action`], [`line_byte_range`], [`find_checkbox_char_any`],
/// and [`content_hash`] helpers unchanged. The vault is mutated **only** on a
/// successful [`ApplyOutcome::Applied`]; every other path leaves it untouched.
///
/// The only new logic vs `process_action` is variable-length line surgery driven
/// by the pure [`taski_core::rewrite_scheduled`] oracle; the TOCTOU whole-file
/// re-hash inside `atomic_write` is agnostic to mutation size, so it is reused
/// verbatim. Sequence:
///
/// 1. Look up the current task row (surrogate rowid, ADR-0005).
/// 2. Read the note fresh; authoritative conflict check (`content_hash` ==
///    `row.note_hash`).
/// 3. Resolve the row's CURRENT `line_number` (ADR-0005 §4), not the stale action
///    value.
/// 4. Byte-verify the line still holds a `[<char>]` checkbox (`find_checkbox_char_any`).
/// 5. Decode the line as UTF-8 and call `rewrite_scheduled(line, desired)` where
///    `desired` comes from `action.payload`.
///    - `Unchanged` → `Applied` (idempotent; no write — mirrors `process_action`'s
///      already-matches short-circuit).
///    - `Unparseable` → `MetadataUnparseable` (refuse; never guess).
///    - `Rewritten(new_line)` → splice the new line bytes into the full note
///      buffer, replacing ONLY the target line (every other byte + all line
///      endings preserved), then `atomic_write` (same function, same TOCTOU).
pub fn process_metadata_action(
    conn: &Connection,
    vault_root: &Path,
    action: &PendingAction,
) -> Result<ApplyOutcome> {
    // 1. Current task row (the DB row is a *claim*, not truth — bytes are re-verified).
    let row = match lookup_task_for_action(conn, action.task_id)? {
        Some(row) => row,
        None => {
            // ADR-0009: a prior ⏳ write changed text_hash, causing reconcile_note
            // to assign a new surrogate id. Pending actions referencing the old
            // id would fail with TaskNotFound. Fall back to the recorded
            // (note_path, line_number) location. (Symmetric to the ADR-0012 ✅
            // fallback in `process_action_at`.)
            tracing::debug!(
                task_id = action.task_id,
                note_path = %action.note_path,
                line_number = action.line_number,
                "task_id not found; trying location fallback"
            );
            match lookup_task_by_location(conn, &action.note_path, action.line_number)? {
                Some(row) => row,
                None => return Ok(ApplyOutcome::TaskNotFound),
            }
        }
    };

    // 2. Read the note fresh from the vault.
    let note_abs = vault_root.join(&row.note_path);
    let bytes = match fs::read(&note_abs) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(ApplyOutcome::TaskNotFound),
        Err(e) => return Err(e).with_context(|| format!("reading note {:?}", note_abs)),
    };

    // Authoritative conflict check via content hash (mtime is informational only).
    let snapshot_hash = content_hash(&bytes);
    if Some(&snapshot_hash) != row.note_hash.as_ref() {
        return Ok(ApplyOutcome::ConflictNoteChanged);
    }

    // 3. Target the task's CURRENT line (ADR-0005 §4) — the row's `line_number`,
    //    updated by reconciliation if the task moved, NOT the stale `action.line_number`.
    let Some(line_range) = line_byte_range(&bytes, row.line_number) else {
        return Ok(ApplyOutcome::TaskLineMismatch);
    };

    // 4. Byte-verify the line is still a checkbox line (reuse the same detection
    //    `process_action` uses). If the line no longer holds a `[<char>]` pattern,
    //    refuse rather than rewrite an arbitrary line.
    if find_checkbox_char_any(&bytes, line_range.clone()).is_none() {
        return Ok(ApplyOutcome::TaskLineMismatch);
    }

    // 5. Decode the target line to a `&str` (Obsidian notes are UTF-8; if a
    //    concurrent edit produced invalid UTF-8 on this line, refuse). `line_byte_range`
    //    delimits lines on `\n` only, so a trailing `\r` (CRLF notes) is INCLUDED in
    //    `line_range`. We must treat that `\r` as part of the terminator — exactly as
    //    the read path's `str::lines()` does — so the rewritten content ends *before*
    //    any CR/LF. Otherwise `rewrite_scheduled`'s append path would insert the `⏳`
    //    *between* the CR and the LF (`"- [ ] task\r ⏳ …"`), and the next `parse_tasks`
    //    (which strips a `\r` adjacent to `\n`) would fold that CR into the task body,
    //    permanently polluting the text + `text_hash`. (This hazard is novel vs the
    //    checkbox path, which flips a char at line START, far from the `\r`.)
    let content_end = if line_range.end > line_range.start && bytes[line_range.end - 1] == b'\r' {
        line_range.end - 1
    } else {
        line_range.end
    };
    let line = match std::str::from_utf8(&bytes[line_range.start..content_end]) {
        Ok(s) => s,
        Err(_) => return Ok(ApplyOutcome::TaskLineMismatch),
    };

    // `desired` flows from `action.payload`: Some(date) → mark/re-schedule;
    // None → unmark. The pure oracle decides whether to write at all.
    let desired = action.payload.as_deref();
    let new_line = match rewrite_scheduled(line, desired) {
        RewriteResult::Unchanged => return Ok(ApplyOutcome::Applied),
        RewriteResult::Unparseable => return Ok(ApplyOutcome::MetadataUnparseable),
        RewriteResult::Rewritten(s) => s,
    };

    // Splice the rewritten line into the full note buffer, replacing ONLY the
    // content bytes `[line_range.start, content_end)` (every other byte and ALL
    // line endings — including the `\r` in a CRLF note — preserved, since they live
    // in `bytes[content_end..]`). The same discipline as `process_action`'s
    // single-char swap, generalized to N bytes.
    let mut new_bytes = Vec::with_capacity(bytes.len() + new_line.len());
    new_bytes.extend_from_slice(&bytes[..line_range.start]);
    new_bytes.extend_from_slice(new_line.as_bytes());
    new_bytes.extend_from_slice(&bytes[content_end..]);

    // Atomic write with a final TOCTOU check (C1) — the SAME function
    // `process_action` uses, unchanged. Its whole-file re-hash is agnostic to
    // whether we changed 1 byte or N.
    match atomic_write(&note_abs, &new_bytes, &snapshot_hash)? {
        WriteResult::Written => Ok(ApplyOutcome::Applied),
        WriteResult::Conflict => Ok(ApplyOutcome::ConflictNoteChanged),
    }
}

/// Execute one pending `toggle_bullet` write (ADR-0011) — converts a task line
/// between checkbox and bullet format. Structurally parallel to
/// [`process_metadata_action`], reusing the same helpers and [`atomic_write`].
///
/// Sequence is identical to `process_metadata_action` except steps 5/6 call the
/// pure [`taski_core::toggle_bullet`] oracle instead of `rewrite_scheduled`:
///
/// 1. Look up the current task row (surrogate rowid, ADR-0005).
/// 2. Read the note fresh; authoritative conflict check.
/// 3. Resolve the row's CURRENT `line_number`.
/// 4. Byte-verify the line still holds a valid checkbox or bullet format.
/// 5. Call `toggle_bullet(line)`.
///    - `Rewritten(new_line)` → splice the new line into the full note buffer
///    - `Unparseable` → `BulletUnparseable` (refuse; never guess)
/// 6. `atomic_write` (same function, same TOCTOU).
pub fn process_bullet_action(
    conn: &Connection,
    vault_root: &Path,
    action: &PendingAction,
) -> Result<ApplyOutcome> {
    // 1. Resolve the target location and note_hash.
    //    Normal path: from the task row (lookup_task_for_action).
    //    Location fallback: if the task_id churned (ADR-0012 ✅/⏳ stamp
    //      changed text_hash → reconcile_note assigned a new id).
    //    Bullet undo fallback: if the task row was deleted entirely (the
    //      forward bullet toggle converted checkbox→bullet, so reconcile_note
    //      pruned the row — no task exists at that location). Use the action's
    //      recorded (note_path, line_number) + the note_contents cache for
    //      note_hash (conflict detection still works — the cache was updated
    //      when the forward toggle's re-index ran).
    let (note_path, line_number, note_hash) = match lookup_task_for_action(conn, action.task_id)? {
        Some(row) => (row.note_path, row.line_number, row.note_hash),
        None => match lookup_task_by_location(conn, &action.note_path, action.line_number)? {
            Some(row) => (row.note_path, row.line_number, row.note_hash),
            None => {
                tracing::debug!(
                    task_id = action.task_id,
                    note_path = %action.note_path,
                    line_number = action.line_number,
                    "task_id and location not found; using note_contents fallback"
                );
                let cached_hash =
                    db::note_content(conn, &action.note_path)?.and_then(|nc| nc.note_hash);
                (action.note_path.clone(), action.line_number, cached_hash)
            }
        },
    };

    // 2. Read the note fresh from the vault.
    let note_abs = vault_root.join(&note_path);
    let bytes = match fs::read(&note_abs) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(ApplyOutcome::TaskNotFound),
        Err(e) => return Err(e).with_context(|| format!("reading note {:?}", note_abs)),
    };

    // 3. Authoritative conflict check via content hash.
    let snapshot_hash = content_hash(&bytes);
    if Some(&snapshot_hash) != note_hash.as_ref() {
        return Ok(ApplyOutcome::ConflictNoteChanged);
    }

    // 4. Target the line.
    let Some(line_range) = line_byte_range(&bytes, line_number) else {
        return Ok(ApplyOutcome::TaskLineMismatch);
    };

    // 5. Decode the line as UTF-8 and call the pure oracle.
    let line_str = std::str::from_utf8(&bytes[line_range.clone()])
        .map_err(|_| anyhow::anyhow!("target line is not valid UTF-8"))?;
    match toggle_bullet(line_str) {
        RewriteResult::Unchanged => Ok(ApplyOutcome::Applied),
        RewriteResult::Unparseable => Ok(ApplyOutcome::BulletUnparseable),
        RewriteResult::Rewritten(new_line) => {
            // Splice the new line bytes into the full note buffer, replacing ONLY
            // the target line range. Every byte outside this range is preserved.
            let mut new_bytes = Vec::with_capacity(bytes.len() - line_range.len() + new_line.len());
            new_bytes.extend_from_slice(&bytes[..line_range.start]);
            new_bytes.extend_from_slice(new_line.as_bytes());
            new_bytes.extend_from_slice(&bytes[line_range.end..]);

            // 6. Atomic write with TOCTOU guard.
            match atomic_write(&note_abs, &new_bytes, &snapshot_hash)? {
                WriteResult::Written => Ok(ApplyOutcome::Applied),
                WriteResult::Conflict => Ok(ApplyOutcome::ConflictNoteChanged),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ADR-0014: quick-add — bounded append-only task creation to a designated inbox.
// ---------------------------------------------------------------------------

/// Execute one pending `quick_add` action (ADR-0014): append a canonical
/// `- [ ] <text> ➕ <today>` task line to the designated inbox note. This is the
/// wall-clock wrapper; deterministic tests call [`process_quick_add_at`] with a
/// fixed date.
///
/// The inbox path travels in `action.note_path`; the user-typed text travels in
/// `action.payload`. If the inbox exists, the append uses the full `atomic_write`
/// TOCTOU guard (ADR-0004 reused). If the inbox does NOT exist, it is created
/// via [`atomic_create`] (temp → fsync → rename, no TOCTOU re-hash — a
/// non-existent file has no state to conflict with; bounded ADR-0004 exception).
pub fn process_quick_add(vault_root: &Path, action: &PendingAction) -> Result<ApplyOutcome> {
    let today = taski_core::ymd_from_unix(unix_now());
    process_quick_add_at(vault_root, action, &today)
}

/// Deterministic-date variant of [`process_quick_add`] for testing. The
/// daemon's drain loop calls [`process_quick_add`] (wall-clock); tests call
/// this with a fixed `"2026-06-21"`.
pub fn process_quick_add_at(
    vault_root: &Path,
    action: &PendingAction,
    today: &str,
) -> Result<ApplyOutcome> {
    let inbox_rel = &action.note_path;
    let text = action.payload.as_deref().unwrap_or("");
    let line = inbox_line_for(text, today);
    let inbox_abs = vault_root.join(inbox_rel);

    match fs::read(&inbox_abs) {
        Ok(original_bytes) => {
            // Existing inbox: append with the full TOCTOU guard.
            let original_hash = content_hash(&original_bytes);
            let mut content = String::from_utf8(original_bytes)
                .with_context(|| format!("inbox {inbox_rel:?} is not valid UTF-8"))?;
            // Prepend a newline if the file has content but no trailing newline,
            // so the appended line starts on its own line. An empty file gets the
            // line directly (no prepended newline).
            if !content.ends_with('\n') && !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&line);
            content.push('\n');
            let new_bytes = content.into_bytes();
            match atomic_write(&inbox_abs, &new_bytes, &original_hash)? {
                WriteResult::Written => {
                    tracing::info!(inbox = %inbox_rel, "quick-add appended");
                    Ok(ApplyOutcome::Applied)
                }
                WriteResult::Conflict => Ok(ApplyOutcome::ConflictNoteChanged),
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // First-creation: no TOCTOU (nothing to conflict with).
            let content = format!("{line}\n");
            atomic_create(&inbox_abs, content.as_bytes())?;
            tracing::info!(inbox = %inbox_rel, "quick-add created inbox");
            Ok(ApplyOutcome::Applied)
        }
        Err(e) => Err(e).with_context(|| format!("reading inbox {inbox_rel:?}")),
    }
}

/// Execute one pending `quick_add_undo` action (ADR-0014): remove the last line
/// of the inbox if it matches the expected `- [ ] <text> ➕ <today>` content that
/// `process_quick_add` wrote. This is the wall-clock wrapper; deterministic tests
/// call [`process_quick_add_undo_at`].
pub fn process_quick_add_undo(vault_root: &Path, action: &PendingAction) -> Result<ApplyOutcome> {
    let today = taski_core::ymd_from_unix(unix_now());
    process_quick_add_undo_at(vault_root, action, &today)
}

/// Deterministic-date variant of [`process_quick_add_undo`] for testing.
pub fn process_quick_add_undo_at(
    vault_root: &Path,
    action: &PendingAction,
    today: &str,
) -> Result<ApplyOutcome> {
    let inbox_rel = &action.note_path;
    let text = action.payload.as_deref().unwrap_or("");
    let expected_line = inbox_line_for(text, today);
    let inbox_abs = vault_root.join(inbox_rel);

    let original_bytes = match fs::read(&inbox_abs) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(ApplyOutcome::TaskNotFound),
        Err(e) => return Err(e).with_context(|| format!("reading inbox {inbox_rel:?}")),
    };
    let original_hash = content_hash(&original_bytes);
    let content = String::from_utf8(original_bytes)
        .with_context(|| format!("inbox {inbox_rel:?} is not valid UTF-8"))?;

    // The expected suffix is the appended line plus its trailing `\n`.
    let expected_suffix = format!("{expected_line}\n");
    if !content.ends_with(&expected_suffix) {
        // The last line doesn't match — either externally edited or different content.
        tracing::warn!(inbox = %inbox_rel, "quick-add undo refused: last line mismatch");
        return Ok(ApplyOutcome::ConflictNoteChanged);
    }

    // Remove the last line (including its trailing newline).
    let new_bytes = &content.as_bytes()[..content.len() - expected_suffix.len()];
    match atomic_write(&inbox_abs, new_bytes, &original_hash)? {
        WriteResult::Written => {
            tracing::info!(inbox = %inbox_rel, "quick-add undo removed last line");
            Ok(ApplyOutcome::Applied)
        }
        WriteResult::Conflict => Ok(ApplyOutcome::ConflictNoteChanged),
    }
}

/// First-creation helper for ADR-0014: write a brand-new file via temp → fsync →
/// rename, **without** the TOCTOU re-hash. A non-existent file has no state to
/// conflict with — the bounded exception to ADR-0004; see ADR-0014 §"The
/// first-creation path". The temp file is in the same directory as the target
/// (same FS, atomic rename on POSIX/APFS). Parent directories are created if
/// missing.
fn atomic_create(path: &Path, content: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("inbox path {path:?} has no parent"))?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .with_context(|| format!("inbox path {path:?} has no file name"))?;
    fs::create_dir_all(dir).with_context(|| format!("creating inbox dir {dir:?}"))?;
    let tmp = dir.join(format!("{file_name}.taski.tmp"));

    let mut file = fs::File::create(&tmp).with_context(|| format!("creating temp {tmp:?}"))?;
    file.write_all(content)
        .with_context(|| format!("writing temp {tmp:?}"))?;
    file.sync_all()
        .with_context(|| format!("fsyncing temp {tmp:?}"))?;
    drop(file);
    fs::rename(&tmp, path).with_context(|| format!("renaming {tmp:?} -> {path:?}"))?;
    Ok(())
}
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

/// Look up a task by its recorded `(note_path, line_number)` location.
/// Used as a fallback when `lookup_task_for_action(task_id)` returns `None`
/// because the task's surrogate id churned — the ✅ stamp (ADR-0012) or ⏳
/// write (ADR-0009) changes `text_hash`, causing `reconcile_note` to assign
/// a new id. The `PendingAction` carries `note_path` and `line_number` from
/// enqueue time; for same-line edits (which metadata stamps are), this
/// location is still valid after re-index.
fn lookup_task_by_location(
    conn: &Connection,
    note_path: &str,
    line_number: usize,
) -> Result<Option<TaskRow>> {
    let row = conn.query_row(
        "SELECT note_path, line_number, raw_checkbox_char, note_hash
         FROM tasks WHERE note_path = ?1 AND line_number = ?2",
        rusqlite::params![note_path, line_number as i64],
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
        Err(e) => Err(e).with_context(|| format!("looking up task at {note_path}:{line_number}")),
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
/// crash mid-write-back (ADR-0003). Walks the vault with the same directory exclusion
/// as [`scan_vault`] (hidden dirs + user-configured excludes). Returns the count of
/// files removed. A failure to remove one file is logged and skipped — it never aborts
/// the sweep.
pub fn sweep_tmp_files(vault_root: &Path, exclude_dirs: &[String]) -> Result<usize> {
    let mut removed = 0usize;
    for entry in WalkDir::new(vault_root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !should_exclude_entry(e, vault_root, exclude_dirs))
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

/// Initialise `tracing` to stderr at `info` (overridable via `RUST_LOG`). Used by the
/// standalone daemon entry points; the unified launcher reuses this for `taski daemon`
/// and installs its own file-sink subscriber for combined mode. Safe to call when a
/// subscriber is already installed (e.g. when running under a test).
pub fn init_tracing() {
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

    /// `--init-config` writes a valid config that round-trips through taski-config,
    /// baking in the `--vault` value and creating parent dirs.
    #[test]
    fn write_config_to_creates_round_trip_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Nested target to confirm parent dirs are created.
        let target = dir.path().join("nested").join("config.toml");
        let vault = Path::new("/tmp/some-vault");
        write_config_to(&target, Some(vault), "/tmp/taski.db").expect("write");

        assert!(target.exists(), "config file should exist");
        // Round-trips through taski-config.
        let cfg = taski_config::load_from(&target).expect("generated config should parse");
        assert_eq!(cfg.vault.as_deref(), Some("/tmp/some-vault"));
        assert_eq!(cfg.db.as_deref(), Some("/tmp/taski.db"));
    }

    /// `--init-config` refuses to overwrite an existing config and leaves it untouched.
    #[test]
    fn write_config_to_refuses_to_clobber() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("config.toml");
        fs::write(&target, "vault = \"/existing\"\n").unwrap();

        let err = write_config_to(&target, None, "/tmp/x.db").expect_err("should refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("already exists"), "got: {msg}");
        // The existing file is untouched.
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "vault = \"/existing\"\n"
        );
    }

    /// `should_exclude_entry` matches user-configured exclude directories by exact
    /// path and prefix. This validates the matching logic in isolation from WalkDir.
    #[test]
    fn should_exclude_entry_matches_exclude_dirs_by_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let vault = tmp.path();
        let excludes = vec!["_System/Templates".to_string()];

        // Create the excluded directory hierarchy (mimicking the user's vault layout)
        // so that WalkDir entries exist for `should_exclude_entry` to inspect.
        fs::create_dir_all(vault.join("_System/Templates")).unwrap();
        fs::write(vault.join("_System/Templates/note.md"), "- [ ] excluded\n").unwrap();
        fs::write(vault.join("_System/other.md"), "- [ ] not excluded\n").unwrap();
        fs::write(vault.join("normal.md"), "- [ ] normal\n").unwrap();

        // Walk with exclude filter — the excluded dir and any files inside it
        // must NOT appear in the yielded entries.
        let yielded: Vec<_> = WalkDir::new(vault)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !should_exclude_entry(e, vault, &excludes))
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_type().is_dir())
            .map(|e| {
                e.path()
                    .strip_prefix(vault)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        assert!(
            !yielded.iter().any(|p| p.contains("_System/Templates")),
            "_System/Templates must be excluded, got: {:?}",
            yielded
        );

        // Non-excluded paths in the same parent directory are unaffected.
        assert!(
            yielded.contains(&"_System/other.md".to_string()),
            "_System/other.md should not be excluded: {:?}",
            yielded
        );
        assert!(
            yielded.contains(&"normal.md".to_string()),
            "normal.md should not be excluded: {:?}",
            yielded
        );
    }

    /// `path_matches_exclude` correctly identifies files inside an excluded directory
    /// by their relative path from the vault root.
    #[test]
    fn path_matches_exclude_works_for_files_in_excluded_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let vault = tmp.path();
        let excludes = vec!["_System/Templates".to_string()];

        let inside = vault.join("_System/Templates/note.md");
        // Create the path structure so the file "exists" (the function only does
        // string matching, but we create it for realism).
        fs::create_dir_all(inside.parent().unwrap()).unwrap();
        fs::write(&inside, "- [ ] test\n").unwrap();

        assert!(path_matches_exclude(&inside, vault, &excludes));

        let outside = vault.join("normal.md");
        fs::write(&outside, "- [ ] test\n").unwrap();
        assert!(!path_matches_exclude(&outside, vault, &excludes));
    }

    /// `path_matches_exclude` is false when `exclude_dirs` is empty.
    #[test]
    fn path_matches_exclude_noop_when_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let vault = tmp.path();
        let f = vault.join("any.md");
        fs::write(&f, "").unwrap();
        assert!(!path_matches_exclude(&f, vault, &[]));
    }

    /// End-to-end: `scan_vault` with an `exclude_dirs` entry must NOT index any tasks
    /// from files inside that directory. This mirrors the user's scenario with
    /// `_System/Templates`.
    #[test]
    fn scan_vault_with_exclude_dirs_skips_matching_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let vault = tmp.path();
        let db_path = vault.join("test.db");
        let conn = db::open(&db_path.to_string_lossy()).expect("open db");

        let excludes = vec!["_System/Templates".to_string()];

        // Normal tasks.
        fs::write(vault.join("alpha.md"), "- [ ] normal\n").unwrap();
        fs::write(vault.join("beta.md"), "- [x] done\n").unwrap();

        // Tasks INSIDE _System/Templates — must NOT be indexed.
        fs::create_dir_all(vault.join("_System/Templates/Old - Templater")).unwrap();
        fs::write(
            vault.join("_System/Templates/Old - Templater/Old - New Task.md"),
            "- [ ] excluded template task\n",
        )
        .unwrap();
        fs::write(
            vault.join("_System/Templates/_Template - Project.md"),
            "- [ ] project template\n",
        )
        .unwrap();

        // Normal tasks inside _System but not under _System/Templates — MUST be indexed.
        fs::write(vault.join("_System/other.md"), "- [ ] other system\n").unwrap();

        let total = scan_vault(&conn, vault, &excludes).expect("scan_vault");

        let tasks = db::all_tasks(&conn).expect("all_tasks");
        let note_paths: Vec<&str> = tasks.iter().map(|t| t.note_path.as_str()).collect();

        // Only 3 tasks should be indexed: alpha.md, beta.md, _System/other.md.
        assert_eq!(total, 3, "expected 3 tasks, got {total}: {note_paths:?}");
        assert_eq!(
            tasks.len(),
            3,
            "expected 3 task rows, got {}: {note_paths:?}",
            tasks.len()
        );

        assert!(
            !note_paths.iter().any(|p| p.contains("_System/Templates")),
            "no tasks from _System/Templates: {:?}",
            note_paths
        );

        assert!(note_paths.contains(&"alpha.md"), "{:?}", note_paths);
        assert!(note_paths.contains(&"beta.md"), "{:?}", note_paths);
        assert!(note_paths.contains(&"_System/other.md"), "{:?}", note_paths);
    }
}
