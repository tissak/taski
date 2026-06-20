# ADR-0007: Unified `taski` binary (in-process daemon + TUI)

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** How the user starts Taski — a single binary that runs the daemon and the TUI
  together by default, with the daemon's lifetime scoped to the TUI session. (Feature
  `unified-launcher`.)

## Context

Taski ships two binaries — `taski-daemon` (watcher + sole vault writer) and `taski-tui`
(reader) — decoupled by SQLite ([ADR-0002](./0002-write-back-through-daemon.md)). For
ad-hoc use the operator must start the daemon in one terminal and the TUI in another, then
remember to stop the daemon when done: two commands, two processes, manual teardown. The
always-on launchd path avoids this but is a separate setup and leaves the daemon running
whether or not the user wants a session.

For a personal, single-user tool, "run one command, get the whole app, have it clean up on
quit" is the common case worth optimizing — without weakening the write-back safety
contract and without breaking the launchd path.

The design questions are: (a) **topology** — does combined mode spawn the daemon as a child
process, or run it in-process on a thread? — and (b) **lifetime coupling** — how do the two
shut down together so no in-flight toggle is lost and the terminal is always left sane?

## Decision

**A single `taski` binary with `clap` subcommands.** Combined mode (the default, `taski`
with no subcommand) runs the daemon **in-process on a background `std::thread`** and the
TUI on the main thread, sharing a `ShutdownSignal` (`Arc<AtomicBool>`). Subcommands `taski
daemon` and `taski tui` run either component alone.

1. **In-process, not child-process.** The daemon logic (already exposed as
   `taski_daemon::run`) is refactored into `run_daemon(opts, shutdown)`, which the launcher
   calls on a spawned thread. The TUI runs on the main thread. Each thread creates and owns
   its own SQLite `Connection` (WAL's one-writer/many-readers guarantee is per-database, not
   per-process, so two connections in one process behave identically to today's two
   processes).
2. **Session-scoped daemon in combined mode.** When the TUI quits, it sets the shared
   `ShutdownSignal`; the daemon performs one final `process_pending_actions` drain and
   exits. The daemon's lifetime is the TUI's lifetime — unless run standalone or under
   launchd.
3. **Subcommand surface.** `taski` (combined, default) · `taski daemon` (daemon only — what
   launchd invokes) · `taski tui` (TUI only, a reader). The existing `taski-daemon` /
   `taski-tui` binaries are **kept** for back-compat with installed launchd plists; they are
   not replaced.
4. **Shutdown ordering.** TUI loop returns → `shutdown.set()` → **terminal restored first**
   (user sees their shell at once) → daemon thread joined → connections dropped → exit. Once
   the watch loop is running, `join()` is bounded by the ≤500 ms tick plus the final drain; if
   the user quits during the pre-watch-loop setup (config load → vault resolve → `scan_vault`),
   `join` is bounded by that scan instead (typically sub-second for a personal vault; the daemon
   does not poll `shutdown` during setup, but the terminal is already restored so only a brief
   delay before process exit is visible).
5. **One Ctrl-C handler, dormant in combined mode.** The TUI runs in crossterm raw mode,
   which swallows `SIGINT`, so Ctrl-C arrives as a key event the TUI handles (driving the
   same shutdown path as `q`). The daemon's global `ctrlc::set_handler` is therefore live
   only in `taski daemon` and dormant in combined mode — the two never coexist.

## Rationale

- **One binary, no discovery.** In-process means a single `taski` binary to install and
  remember. A child-process launcher would have to locate `taski-daemon` on `PATH` / beside
  its own executable — fragile — and would still ship two binaries.
- **Clean shutdown for free.** A shared `AtomicBool` + thread `join()` is simpler and more
  reliable than cross-process signaling (SIGTERM/SIGINT to a child) and composes with the
  daemon's existing clean-shutdown path.
- **No alt-screen corruption.** A child daemon's `tracing`→stderr lands inside the TUI's
  alternate screen and garbles it; in-process, the daemon's sink is a `MakeWriter` choice,
  routed to the log file in combined mode.
- **Trivial subcommand mapping.** One `main` dispatches `daemon` / `tui` / combined; a
  child-process model forces an awkward third launcher binary or makes the launcher *be* the
  TUI spawning the daemon.
- **WAL equivalence.** Two threads on one database file under WAL is the same guarantee as
  two processes today — no new concurrency hazard is introduced at the SQLite layer.

## Consequences

- ✅ The common case is one command (`taski`) with automatic teardown; the always-on launchd
  path and the standalone binaries keep working.
- ✅ Quit drains pending actions, so a toggle done right before `q` lands in this session
  (pending rows are durable in SQLite regardless; the drain is about UX completeness).
- ⚠️ **Panic coupling.** A daemon-thread panic fires the TUI's global panic hook and could
  yank the terminal out of raw mode mid-session. Mitigation: wrap the daemon thread in
  `catch_unwind`; on `Err`, log, set the shutdown flag, and exit cleanly with a notice.
- ⚠️ **Two daemon lifetime modes** now exist (session-scoped vs. always-on). The
  single-writer lock ([ADR-0008](./0008-single-writer-file-lock.md)) makes them
  non-conflicting; combined mode *attaches* to a running daemon rather than spawning a
  second.
- ⚠️ **Tracing routing.** In combined mode the daemon's `tracing` must go to the log file,
  not stderr; standalone `taski daemon` keeps stderr (for `tail -f`-style debugging).
- ⚠️ Requires a small library refactor: `taski-daemon` gains `DaemonOpts` /
  `ShutdownSignal` / `run_daemon`; `taski-tui` becomes lib+bin with `run()` /
  `run_with_shutdown()`. `taski-core` stays pure.

## Alternatives considered

- **Child-process launcher** (spawn the `taski-daemon` binary as a child). Best panic
  isolation and reuses the binary verbatim, but loses on binary discovery/shipping,
  cross-process shutdown signaling, stderr↔alt-screen interleaving, and subcommand mapping.
  Rejected — for a personal tool the panic isolation is marginal and every other axis favors
  in-process.
- **A third launcher binary beside the two existing ones.** Keeps the binaries independent
  but means three binaries and the user must know which to run — defeating the "one command"
  goal. Explicitly rejected.
- **Block on the first scan before painting the TUI.** Avoids an empty-state flash but
  penalizes the common (already-populated) case with an unbounded blank-screen startup.
  Rejected — the TUI's 750 ms poll catches up within 1–2 cycles.

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; this ADR
  changes process *topology*, not *what* writes.
- [ADR-0008](./0008-single-writer-file-lock.md) — the lock that makes the two lifetime modes
  non-conflicting; the natural companion to this ADR.
- [`docs/features/unified-launcher.md`](../features/unified-launcher.md) — the feature plan.
- [`docs/context.md`](../context.md), [`docs/tech.md`](../tech.md) — to be updated on
  implementation.
