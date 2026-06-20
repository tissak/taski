# ADR-0008: Single-writer enforcement via file lock

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** How Taski prevents two daemon instances from writing the vault and index
  concurrently. Extends [ADR-0002](./0002-write-back-through-daemon.md) for the
  multi-startup era introduced by [ADR-0007](./0007-unified-in-process-launcher.md).

## Context

[ADR-0002](./0002-write-back-through-daemon.md) establishes that the daemon is the **sole
writer** to the vault, and the codebase's safety machinery is built on that assumption. Two
assumptions are load-bearing and *only* hold under single-writer:

1. **`atomic_write` uses a fixed-name temp file** (`<note>.taski.tmp`). Two daemons both
   create it → they truncate each other's temp → both `fsync` → both `rename` the same path
   over the target. The [ADR-0004](./0004-refuse-on-conflict.md) TOCTOU re-hash guard does
   **not** save you here: both daemons re-hash the same target bytes and see the same hash.
   This is a note-corruption vector that ADR-0004 was never designed to prevent.
2. **`reconcile_note` is a read-modify-write** (`SELECT old rows → UPDATE/INSERT/DELETE`).
   [ADR-0005](./0005-surrogate-identity.md) reconciliation assumes single-threaded
   execution; two concurrent reconciles interleave → duplicate/dropped surrogate ids and
   broken identity mapping.

SQLite WAL does **not** prevent either: WAL serializes *database transactions*, but these
races happen on **vault files and the reconciliation window**, which SQLite never sees. A
spurious "checkbox line changed" failure from double-drained `pending_actions` is also
possible (the double-flip itself is guarded by ADR-0005's `raw_checkbox_char` re-check, but
confusing to the user).

Before [ADR-0007](./0007-unified-in-process-launcher.md), two daemons required deliberate
misconfiguration. The unified launcher makes it easy: a launchd daemon plus `taski`
(combined) would spawn a second daemon unless something prevents it.

## Decision

**The daemon acquires an advisory exclusive lock — `flock(LOCK_EX | LOCK_NB)` — on
`<data-dir>/daemon.lock` before scanning.** The lock is held for the daemon's lifetime and
released by the OS when the process exits (including crash / `kill -9`).

1. **Mechanism: `flock` on a lock file.** A small `taski-daemon::lock` module exposes
   `acquire_daemon_lock(path) -> LockOutcome` (`Acquired(DaemonLockGuard)` |
   `HeldByOther(pid)`); the guard owns the locked `File` and releases on `Drop` (fd close →
   OS releases the lock). A non-acquiring `probe_daemon_lock()` powers the launcher's
   attach decision. The dependency is `fs2` (a tiny, ubiquitous `flock` wrapper), recorded
   in `tech.md`.
2. **Uniform acquisition across all daemon entry points.** The lock is taken inside
   `run_daemon`, so the standalone `taski-daemon` binary, `taski daemon`, and the combined
   launcher's spawned daemon thread are all protected identically.
3. **Attach-or-spawn for combined mode; refuse for `taski daemon`.** When the lock is held,
   `taski` (combined) runs the **TUI only** against the existing daemon and prints
   `Attached to running daemon (PID X).` `taski daemon` (and `taski-daemon`), by contrast,
   **refuse** with a non-zero exit and a clear message — the user asked for a daemon
   explicitly, and a second one is a config mistake.
4. **Lock-file path derives from the resolved `db` directory**, not a hardcoded path, so
   `--db /tmp/x.db` cannot bypass the lock by using a different directory than the data.

## Rationale

- **`flock` is auto-released on process death.** A crashed or `kill -9`'d daemon leaves
  *no stale state* — the OS releases the fd's lock the instant the process dies. No cleanup
  logic, no PID-liveness check, no PID-reuse edge case. This is the decisive property for a
  lock adjacent to the write-back safety contract.
- **Attach-not-refuse is required for a coherent default.** `install-launchd.sh` sets up an
  always-on daemon. If combined mode *refused* whenever launchd was active, the default
  `taski` command would fail in exactly the configuration most users will have. Attach turns
  that failure into the correct behavior: the user gets the always-on daemon plus their
  interactive TUI, and a second daemon can never start.
- **Atomic, non-blocking acquire.** `LOCK_NB` makes acquisition a single atomic test-and-set
  — no polling, no race between "check" and "take".
- **Doesn't touch SQLite.** The lock is orthogonal to WAL; it guards the *vault-file* and
  *reconciliation* races that WAL cannot see.

## Consequences

- ✅ The double-writer corruption vector (`atomic_write` temp collision, `reconcile_note`
  race) is closed for all startup combinations, including launchd + `taski`.
- ✅ launchd's always-on daemon and an ad-hoc `taski` session coexist correctly; the user's
  default command always works.
- ✅ Crash-safe: no stale locks to clean up, ever.
- ⚠️ Adds the `fs2` dependency to `taski-daemon` (record in `tech.md`). `fs2` is small and
  widely used; `libc` or `nix` would be equivalent choices.
- ⚠️ The lock is **advisory** — it only protects Taski from itself. An external process
  writing the vault concurrently (e.g. Obsidian) is unaffected, which is exactly the desired
  behavior: Taski never blocks the user's editor; it only prevents *itself* from running two
  daemons. ADR-0004's conflict detection still governs Obsidian-vs-Taski races.
- ⚠️ `taski tui` performs no lock check (it is a reader); multiple TUIs against one daemon
  remain supported under WAL.

## Alternatives considered

- **pidfile (write PID, no `flock`).** Simpler to write, but **stale after a crash**: it
  requires a `kill(pid, 0)` liveness check, suffers PID-reuse hazard, and is racy between
  the check and the take. Strictly worse than `flock`, which needs none of that.
- **A `daemon_lock` row in SQLite.** No new dependency, but **stale after a crash** (same
  PID-liveness problem), a long-held `BEGIN EXCLUSIVE` would bloat WAL, and short
  transactions would not actually prevent two daemons (they would just alternate). Rejected.
- **Rely on WAL's writer serialization.** Already present, but it does **not** prevent the
  vault-file / reconciliation races described in Context. Rejected as a solution.
- **Refuse (not attach) in combined mode.** Simplest lock semantics, but makes the default
  `taski` fail in the exact configuration `install-launchd.sh` creates. Rejected — attach is
  both safer and better UX.

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; this ADR
  is its completion for the multi-startup era.
- [ADR-0004](./0004-refuse-on-conflict.md) — TOCTOU/conflict detection; does **not** cover
  the two-daemon temp-collision vector this lock prevents.
- [ADR-0005](./0005-surrogate-identity.md) — `reconcile_note` assumes single-threaded
  execution; this lock preserves that assumption.
- [ADR-0007](./0007-unified-in-process-launcher.md) — the unified launcher whose safety this
  lock guarantees.
- [`docs/features/unified-launcher.md`](../features/unified-launcher.md) — the feature plan.
