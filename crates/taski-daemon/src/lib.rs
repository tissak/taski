//! taski-daemon — Slice 1 read path.
//!
//! On start, recursively scans an Obsidian vault for `.md` notes and extracts their
//! checkbox tasks into the shared SQLite index; then watches the vault for changes
//! and re-indexes affected notes. Read-only with respect to the vault — it never
//! writes or modifies vault files.
//!
//! The testable scan logic lives here as free functions so the integration tests in
//! `tests/` can exercise it without driving the live watcher.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer};
use rusqlite::Connection;
use taski_core::parse_tasks;
use taski_db as db;
use walkdir::WalkDir;

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

    let total = scan_vault(&conn, &vault_root)?;
    tracing::info!(count = total, ?vault_root, "initial scan complete");

    if cli.once {
        return Ok(());
    }

    run_watch_loop(&conn, &vault_root)?;
    Ok(())
}

/// Read one note, recompute its tasks, and upsert them into the index. Any previous
/// tasks for the note are deleted first so stale rows from an old line layout don't
/// linger. Returns the number of tasks indexed (0 if the note was skipped).
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

    db::delete_tasks_for_note(conn, &rel)
        .with_context(|| format!("deleting old tasks for {rel:?}"))?;

    let tasks = parse_tasks(markdown, &rel);
    for task in &tasks {
        db::upsert_task(conn, task).with_context(|| format!("upserting task from {rel:?}"))?;
    }
    tracing::debug!(?rel, count = tasks.len(), "indexed note");
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
