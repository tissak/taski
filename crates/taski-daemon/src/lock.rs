//! Single-writer enforcement for the daemon (ADR-0008).
//!
//! The daemon is the sole writer to the vault (ADR-0002), and several of its safety
//! invariants assume a *single* daemon instance at a time: `atomic_write` uses a
//! fixed-name temp file, and `reconcile_note` is a read-modify-write. Two daemons would
//! race on those and corrupt notes / the index — and SQLite WAL does **not** save us,
//! because the races happen on vault files and the reconciliation window, not in DB
//! transactions.
//!
//! We prevent a second daemon with an advisory exclusive lock (`flock`) on a lock file
//! beside the database. `flock` is **auto-released by the OS when the process dies**
//! (crash, `kill -9`), so there is no stale-lock cleanup and no PID-reuse hazard — the
//! decisive property for a lock adjacent to the write-back safety contract. The lock is
//! *advisory*: it only protects Taski from itself, never blocks Obsidian or other editors.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;

/// The outcome of attempting to acquire the daemon lock.
pub enum LockOutcome {
    /// We hold the lock for as long as the guard is alive; dropping it releases it.
    Acquired(DaemonLockGuard),
    /// Another daemon holds the lock. The PID is best-effort (may be `None` if the holder
    /// hasn't written it yet or the value is unparseable).
    HeldByOther(Option<u32>),
}

/// Owns the locked file descriptor. Held for the daemon's whole lifetime; on `Drop` the
/// lock is explicitly released (and the `File` closing re-releases it at the OS level as
/// a backstop). `Send` so it can move into the daemon thread in combined mode.
pub struct DaemonLockGuard(fs::File);

impl Drop for DaemonLockGuard {
    fn drop(&mut self) {
        // Explicit release for clarity; the OS would release the flock when the fd closes
        // anyway, but doing it here documents the release as deliberate.
        let _ = self.0.unlock();
    }
}

/// Where the lock file lives: beside the resolved database path (`<db_dir>/daemon.lock`).
/// Derived from the db path (not hardcoded) so `--db /tmp/x.db` cannot bypass the lock by
/// using a different directory than the data.
pub fn daemon_lock_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new(""))
        .join("daemon.lock")
}

/// Try to acquire the daemon lock without blocking.
///
/// Creates the lock file (and its parent directory) if missing. On success the returned
/// [`DaemonLockGuard`] holds the lock; on failure because another holder exists, returns
/// [`LockOutcome::HeldByOther`] with the holder's PID if it could be read. Other
/// unexpected I/O errors propagate.
pub fn acquire_daemon_lock(lock_path: &Path) -> io::Result<LockOutcome> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    // read+write so the same handle can write our PID (acquired) or read the holder's
    // (held), without reopening the file. Deliberately NOT truncated here: on the held
    // path we must read the existing holder's PID, which a `.truncate(true)` open would
    // destroy before `try_lock_exclusive` even runs. (clippy::suspicious_open_options is
    // a false positive for this intentional create-without-truncate.)
    #[allow(clippy::suspicious_open_options)]
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            // Record our PID for diagnostics (best-effort; non-fatal if it fails).
            let _ = file.seek(SeekFrom::Start(0));
            let _ = file.set_len(0);
            let _ = write!(file, "{}", std::process::id());
            let _ = file.flush();
            // Keep the lock; the File owns the locked fd.
            Ok(LockOutcome::Acquired(DaemonLockGuard(file)))
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            Ok(LockOutcome::HeldByOther(read_pid_best_effort(&mut file)))
        }
        Err(e) => Err(e),
    }
}

/// Best-effort read of the holder's PID from the lock file. Returns `None` if the file is
/// empty, missing the value, or unparseable — the caller treats `None` as "held, PID
/// unknown".
fn read_pid_best_effort(file: &mut fs::File) -> Option<u32> {
    let mut buf = String::new();
    if file.seek(SeekFrom::Start(0)).is_err() {
        return None;
    }
    if file.read_to_string(&mut buf).is_err() {
        return None;
    }
    buf.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// Acquiring the lock, then acquiring again on a separately-opened handle (here, on
    /// another thread to guarantee a distinct open file description), reports `HeldByOther`
    /// while the first holder is alive.
    #[test]
    fn acquire_then_acquire_is_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.lock");

        let _guard = match acquire_daemon_lock(&path).expect("first acquire") {
            LockOutcome::Acquired(g) => g,
            LockOutcome::HeldByOther(_) => panic!("first acquire must succeed"),
        };

        // A second attempt from a different thread (distinct open file description) must
        // observe the lock as held, not acquire it.
        let path2 = path.clone();
        let held = thread::spawn(move || {
            matches!(
                acquire_daemon_lock(&path2).expect("second acquire"),
                LockOutcome::HeldByOther(_)
            )
        })
        .join()
        .expect("thread");
        assert!(held, "a second daemon must see the lock as held");
    }

    /// Dropping the guard releases the lock, so a fresh acquire succeeds afterward. (This
    /// mirrors the OS auto-releasing flock on process death — here we just drop it.)
    #[test]
    fn drop_releases_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.lock");

        {
            let _guard = match acquire_daemon_lock(&path).expect("first acquire") {
                LockOutcome::Acquired(g) => g,
                LockOutcome::HeldByOther(_) => panic!("first acquire must succeed"),
            };
            // guard dropped at end of this block
        }

        match acquire_daemon_lock(&path).expect("second acquire") {
            LockOutcome::Acquired(_) => {}
            LockOutcome::HeldByOther(_) => panic!("lock must be released after drop"),
        }
    }

    /// The lock file sits beside the database, named `daemon.lock`.
    #[test]
    fn daemon_lock_path_beside_db() {
        assert_eq!(
            daemon_lock_path(Path::new("/a/b/taski.db")),
            Path::new("/a/b/daemon.lock")
        );
        // A bare filename (no parent) degrades gracefully.
        assert_eq!(
            daemon_lock_path(Path::new("taski.db")),
            Path::new("daemon.lock")
        );
    }
}
