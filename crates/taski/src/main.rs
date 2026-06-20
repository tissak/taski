//! taski — the unified launcher.
//!
//! One command runs the whole app: the daemon (on a background thread) and the TUI (on
//! the main thread) together by default. `taski daemon` / `taski tui` subcommands run
//! either component alone. Combined mode is **in-process** (ADR-0007) and protected by a
//! **single-writer file lock** (ADR-0008); on TUI quit the daemon drains pending actions
//! and exits. The TUI still never touches vault files; the daemon stays the sole writer.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use taski_daemon::{
    DaemonOpts, LockOutcome, ShutdownSignal, acquire_daemon_lock, daemon_lock_path, init_tracing,
    run_daemon,
};

#[derive(Parser)]
#[command(
    name = "taski",
    version,
    about = "Run the Taski daemon and TUI together, or either alone"
)]
struct Cli {
    /// Run only the daemon, or only the TUI. With no subcommand, run both (the default).
    #[command(subcommand)]
    mode: Option<Mode>,

    /// Path to the Obsidian vault (overrides `vault` in the config). Forwarded to the
    /// daemon. Place before a subcommand: `taski --vault X`, `taski --vault X daemon`.
    #[arg(long, global = true)]
    vault: Option<PathBuf>,

    /// Path to the taski SQLite index (overrides `db` in the config; default ./taski.db).
    #[arg(long, global = true)]
    db: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Mode {
    /// Run only the daemon, in the foreground. (This is the path launchd invokes.)
    Daemon,
    /// Run only the TUI. A reader — safe to run alongside any running daemon.
    Tui,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.mode {
        Some(Mode::Daemon) => run_daemon_only(cli),
        Some(Mode::Tui) => taski_tui::run_with_db(cli.db),
        None => run_combined(cli),
    }
}

/// Resolve the lock path (beside the resolved db) so combined and daemon-only modes
/// agree on a single lock file. Returns `(db_path, lock_path)`.
fn resolve_db_and_lock(db_flag: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
    let cfg = taski_config::load().context("loading taski config")?;
    let db_path = taski_config::resolve_db(db_flag.and_then(Path::to_str), &cfg);
    Ok((db_path.clone(), daemon_lock_path(&db_path)))
}

/// `taski daemon`: run the daemon alone in the foreground (Ctrl-C to stop). Refuses if
/// another daemon already holds the single-writer lock.
fn run_daemon_only(cli: Cli) -> Result<()> {
    init_tracing(); // stderr, matching the standalone `taski-daemon` binary.

    let (_db_path, lock_path) = resolve_db_and_lock(cli.db.as_deref())?;
    let guard = acquire_or_refuse(&lock_path)?;
    let (signal, handle) = ShutdownSignal::new();
    ctrlc::set_handler(move || signal.set()).context("installing Ctrl-C handler")?;

    let opts = DaemonOpts {
        vault: cli.vault,
        db: cli.db,
        once: false,
    };
    run_daemon(opts, handle, guard)
}

/// `taski` (default): run the daemon (background thread) + TUI (main thread) together.
/// The daemon's lifetime is scoped to the TUI session. If another daemon already holds
/// the lock, Phase B **refuses** with guidance; Phase C will flip this to "attach"
/// (TUI-only against the running daemon) — that flip lives in [`acquire_for_combined`].
fn run_combined(cli: Cli) -> Result<()> {
    let (_db_path, lock_path) = resolve_db_and_lock(cli.db.as_deref())?;

    // The daemon thread's tracing must go to the log FILE, never stderr: the TUI owns the
    // alternate screen, and any tracing→stderr on the daemon thread would garble it.
    let log_path = lock_path.with_file_name("daemon.log");
    init_tracing_to_file(&log_path);

    let guard = match acquire_daemon_lock(&lock_path).context("acquiring daemon lock")? {
        LockOutcome::Acquired(g) => g,
        // Phase B: refuse with guidance. Phase C: flip to attach (run TUI-only against the
        // existing daemon) here — the single spot that changes.
        LockOutcome::HeldByOther(pid) => {
            // Not in the alt-screen yet, so stderr is safe.
            eprintln!(
                "taski: a daemon is already running{}. Run `taski tui` to use it, or stop it first.",
                pid.map(|p| format!(" (PID {p})")).unwrap_or_default(),
            );
            return Ok(());
        }
    };

    let (signal, handle) = ShutdownSignal::new();
    let signal_for_panic = signal.clone();
    let opts = DaemonOpts {
        vault: cli.vault.clone(),
        db: cli.db.clone(),
        once: false,
    };

    // Spawn the daemon on a background thread. Wrapped in `catch_unwind` so a daemon
    // panic can never fire the TUI's global panic hook mid-session and yank the terminal
    // out of raw mode — instead we log, signal shutdown, and let the TUI exit cleanly.
    // A normal `run_daemon` error (bad vault, db open failure) also signals the TUI: the
    // TUI does not poll the signal itself, so it will show stale data until the user
    // quits — but at least the failure is logged and the process winds down cleanly.
    let daemon_thread = std::thread::Builder::new()
        .name("taski-daemon".into())
        .spawn(
            move || match catch_unwind(AssertUnwindSafe(|| run_daemon(opts, handle, guard))) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!(err = %e, "daemon exited with error; signaling shutdown");
                    signal_for_panic.set();
                }
                Err(_) => {
                    tracing::error!("daemon thread panicked; signaling shutdown");
                    signal_for_panic.set();
                }
            },
        )
        .context("spawning daemon thread")?;

    // Run the TUI on the main thread. On quit (`q`/`Esc`/`Ctrl-C`) it calls the hook →
    // sets the shutdown signal, then restores the terminal and returns. The daemon's
    // watch loop then sees the flag, drains pending actions, and exits.
    let tui_signal = signal.clone();
    let tui_result = taski_tui::run_combined(cli.db, move || tui_signal.set());

    // Join the daemon thread (bounded by its ≤500ms tick + final drain). If it panicked,
    // the TUI already exited via the panic-set signal; join just reaps the thread.
    if let Err(e) = daemon_thread.join() {
        tracing::error!(error = ?e, "daemon thread join error");
    }
    tui_result
}

/// Acquire the lock or refuse (used by `taski daemon`). Shared with the combined refuse
/// branch's wording; Phase C will add an attach variant alongside this.
fn acquire_or_refuse(lock_path: &Path) -> Result<taski_daemon::DaemonLockGuard> {
    match acquire_daemon_lock(lock_path).context("acquiring daemon lock")? {
        LockOutcome::Acquired(g) => Ok(g),
        LockOutcome::HeldByOther(pid) => {
            eprintln!(
                "taski: a daemon is already running{}. Refusing to start a second writer (ADR-0008).",
                pid.map(|p| format!(" (PID {p})")).unwrap_or_default(),
            );
            anyhow::bail!("daemon already running");
        }
    }
}

/// Initialize `tracing` writing to the daemon log FILE (not stderr), so the daemon
/// thread's events never corrupt the TUI's alternate screen. `info` by default
/// (`RUST_LOG` overrides). If the file can't be opened, skip the subscriber entirely —
/// with no subscriber installed, tracing events are dropped (never sent to stderr, which
/// would garble the TUI). This is the combined-mode init only; `taski daemon` uses the
/// stderr [`init_tracing`].
fn init_tracing_to_file(log_path: &Path) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("taski=info,taski_daemon=info"));
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(f) => f,
        Err(e) => {
            // Not in the alt-screen yet: one stderr notice is fine. Then skip the
            // subscriber — daemon logs are simply dropped this session.
            eprintln!("taski: could not open log file {log_path:?}: {e}; discarding daemon logs.");
            return;
        }
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(Mutex::new(file))
        .try_init();
}
