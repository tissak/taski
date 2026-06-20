# Unified Launcher (`taski`)

*Status: Sketch — design decisions locked, pending implementation. Branch
`experiment/unified-launcher`. See also the [ADR notes](#adrs-to-record) at the end.*

**One-liner:** A single `taski` command that runs the daemon and the TUI together by
default — the daemon's lifetime scoped to the TUI session — with `daemon`/`tui`
subcommands for running either alone. The simplest start, without weakening the
write-back safety contract.

---

## Problem & Motivation

Today Taski ships **two binaries** (`taski-daemon`, `taski-tui`) that the user must
orchestrate by hand or via launchd:

- **Ad-hoc use:** run the daemon in one terminal, the TUI in another, and remember to
  stop the daemon when done. Two commands, two processes, manual teardown. This is the
  friction the architecture exists to *avoid* — the user just wants to triage tasks.
- **Always-on use:** `scripts/install-launchd.sh` starts the daemon under launchd and the
  user runs only the TUI. Clean, but it's a separate setup path and the daemon outlives
  every session whether or not the user wants it to.

For a personal, single-user tool, **"run one command, get the whole app, and have it clean
up when I quit"** is the common case we should optimize for — while keeping the always-on
launchd path working for those who want it.

### Non-goals (what this is *not*)

- Not a relaxation of [ADR-0002](../adr/0002-write-back-through-daemon.md): the daemon
  remains the **sole writer** to the vault; the TUI still never opens a vault file.
- Not a new IPC channel between daemon and TUI: SQLite remains the only decoupling
  boundary. In combined mode the two simply live in **one process, on two threads**, each
  with its own SQLite connection (WAL's one-writer/many-readers guarantee is per-database,
  not per-process — identical to today's two processes).
- Not removing the standalone binaries: `taski-daemon`/`taski-tui` stay for back-compat
  with existing launchd plists.

---

## Solution Overview

A **single `taski` binary** using `clap` subcommands:

| Command | Behavior |
|---|---|
| `taski` *(default)* | **Combined.** Probe the single-writer lock. If free → spawn the daemon on a background thread and run the TUI on the main thread; the daemon exits when the TUI quits. If held (e.g. launchd's daemon is running) → **attach**: run the TUI only against the existing daemon. |
| `taski daemon` | Daemon only (what launchd invokes). Probe the lock; run if free, **refuse** with a clear message if held. |
| `taski tui` | TUI only. A reader; no lock check (multiple TUIs against one daemon are fine under WAL). |

All three accept the existing `--vault` / `--db` flags (forwarded into the daemon/TUI).

### The four load-bearing decisions (locked)

1. **In-process, not child-process.** Combined mode runs the daemon logic on a
   `std::thread` and the TUI on the main thread, sharing a `ShutdownSignal`
   (`Arc<AtomicBool>`). Chosen over spawning `taski-daemon` as a child process: one binary
   (no discovery/shipping pain), trivial clean shutdown (no cross-process signaling), and
   no stderr-corrupting-the-TUI problem (the daemon's `tracing` sink is a `MakeWriter`
   choice, routed to the log file in combined mode). See *Alternatives* for the full
   trade-off.
2. **Single-writer enforcement via `flock`.** Before scanning, the daemon acquires
   `flock(LOCK_EX | LOCK_NB)` on `<data-dir>/daemon.lock`. `flock` is **auto-released by
   the OS on crash/`kill -9`** (unlike a pidfile or a DB row) — no stale-lock cleanup, no
   PID-reuse hazard. This closes a real corruption vector (see *Risks*) that ADR-0002's
   machinery does not cover. Recorded as **ADR-0008**.
3. **Attach, don't refuse, in combined mode.** When the lock is held, `taski` runs the
   TUI-only against the existing daemon and prints `Attached to running daemon (PID X).`
   "Refuse" sounds robust but makes the default `taski` fail in exactly the configuration
   `install-launchd.sh` creates. Attach is both safer (never a second daemon) and better
   UX (the user's default command always works). `taski daemon`, by contrast, *does*
   refuse — the user asked for a daemon explicitly, and a second one is a config mistake.
4. **Drain on shutdown.** When the TUI quits, the daemon does **one final
   `process_pending_actions` pass** before exiting, so a toggle done right before `q`
   lands in this session rather than waiting for the next launch. (Pending actions are
   durable in SQLite regardless; the drain is about UX completeness, not correctness.)

---

## User Experience

```sh
taski                 # the common case: daemon + TUI, cleanup on quit
taski daemon          # daemon only (launchd runs this)
taski tui             # TUI only (reader; safe alongside any daemon)
taski --vault X --db Y   # flags forwarded to whichever mode
```

**Startup latency:** combined mode does *not* block on the first scan. The daemon thread
starts scanning immediately; the TUI paints at once and its 750 ms poll picks up the index
within 1–2 cycles. (The empty state only arises on first-ever run or after a schema wipe;
blocking there would penalize the common, already-populated case.)

**Quit semantics (combined):** TUI main loop returns → `ShutdownSignal` set → **terminal
restored first** (user sees their shell immediately) → daemon thread joined (bounded by its
≤500 ms tick + the final drain) → connections dropped → process exits. Ordering matters:
restore *before* join so a brief drain never freezes the alt-screen.

**Ctrl-C:** in combined mode the TUI runs in crossterm raw mode, which swallows `SIGINT` —
Ctrl-C arrives as a key event handled by the TUI (drives the same shutdown path as `q`).
The daemon's global `ctrlc::set_handler` is therefore **dormant** in combined mode and
**live** only in `taski daemon`. Only one code path ever installs it (never both).

---

## User Story Foundation

### Core Behaviors (Must Have)

**As a** single-user operator,
**I want to** type one command and get the whole app — indexing plus the TUI —
**so that** I can triage tasks without managing processes.

**Acceptance Criteria:**
- [ ] `taski` (no args) starts the daemon and the TUI together.
- [ ] Quitting the TUI (`q`) drains pending actions and exits the daemon cleanly within
      ~1 s; no toggle done before quit is left unapplied beyond normal write-back latency.
- [ ] `taski daemon` runs the daemon alone (the launchd entry point).
- [ ] `taski tui` runs the TUI alone (a reader; works alongside any running daemon).
- [ ] `--vault` / `--db` flags work on all three modes, with the existing
      CLI→config→default precedence.
- [ ] Existing `taski-daemon` / `taski-tui` binaries keep working unchanged (back-compat).

### Supporting Behaviors (Should Have)

**As a** user with the launchd daemon installed,
**I want** `taski` to just work,
**so that** I'm never told "daemon already running" by the default command.

- [ ] `taski` detects an already-running daemon (held lock) and **attaches** — runs the
      TUI-only against it, printing `Attached to running daemon (PID X).`
- [ ] `taski daemon` refuses (non-zero exit, clear message) if a daemon is already running.
- [ ] `install-launchd.sh` installs `taski` and points the plist at `taski daemon`.

### Future Considerations (Could Have)

- A live "Indexing N notes…" banner (the daemon writes a `daemon_state` row the TUI reads).
- A documented second-Ctrl-C force-quit (today's behavior) in combined mode.
- Collapsing `taski-daemon`/`taski-tui` into thin shims of `taski` once migration is
  complete (deferred — zero cost to keep them as real binaries).

### Out of Scope (Explicitly Not Doing)

- **A second writer to the vault, ever.** The lock exists to make this impossible; combined
  mode is a topology change, not a relaxation of ADR-0002.
- **Replacing the standalone binaries.** They stay (back-compat). The unified `taski` is the
  *recommended* entry point, not the only one.
- **A new daemon↔TUI IPC channel.** SQLite remains the only boundary.
- **Daemon-supervision / auto-restart inside combined mode.** If the daemon thread panics,
  we surface a notice and exit cleanly (via `catch_unwind`); we don't respawn it. Always-on
  supervision stays launchd's job.

---

## Implementation Considerations

### Architecture decision (locked — see ADRs 0007/0008)

In-process single binary + `flock` single-writer enforcement + attach-or-spawn. Rationale
and rejected alternatives are in *Reality Check* below and in the two ADR stubs.

### Technical Approach

**Library refactor (minimal):**

- `taski-daemon` (already lib+bin): add `DaemonOpts { vault, db, once }`,
  `ShutdownSignal`/`ShutdownHandle` (`Arc<AtomicBool>` pair), and
  `pub fn run_daemon(opts: DaemonOpts, shutdown: ShutdownHandle) -> Result<()>` — the
  reusable engine with no CLI parse and no `ctrlc` install. `pub fn run()` (the standalone
  binary entry) parses CLI, installs ctrlc → `signal.set()`, and calls `run_daemon`.
  Generalize `run_watch_loop`'s shutdown check to `shutdown.is_set()` and add the **final
  `process_pending_actions` drain** before breaking.
- `taski-tui` (binary → lib+bin): rename the internal `fn run(terminal, conn)` →
  `fn run_loop(...)`; add `pub fn run()` and `pub fn run_with_shutdown(shutdown:
  ShutdownSignal)` (the `q`/`Esc`/Ctrl-C arms call `shutdown.set()` before returning).
  `fn main() { taski_tui::run() }`. Existing tests target `App`/`build_view`/`context_view`
  and are unaffected.
- `taski-core` / `taski-db` / `taski-config`: **unchanged.** The lock lives in
  `taski-daemon`; config precedence resolves `vault`/`db` as today.

**New crate `crates/taski`** (binary `taski`), a leaf depending on both libs:

```rust
#[derive(Parser)]
#[command(name = "taski", version, about = "…")]
struct Cli {
    #[command(subcommand)]
    mode: Option<Mode>,        // None => combined (default)
    #[arg(long)] vault: Option<PathBuf>,
    #[arg(long)] db: Option<PathBuf>,
}
#[derive(Subcommand)]
enum Mode { Daemon, Tui }
```

`run_combined`: probe the lock → free ⇒ spawn daemon thread (which acquires the lock) + run
TUI; held ⇒ run TUI-only with an "Attached…" notice. One `init_tracing` whose daemon sink is
the log file in combined mode and stderr for standalone.

**The lock (`taski-daemon::lock`):** `acquire_daemon_lock(path) -> LockOutcome`
(`Acquired(DaemonLockGuard)` | `HeldByOther(pid)`); the guard owns the locked `File` and
releases on `Drop` (fd close → OS releases the lock). A separate non-acquiring
`probe_daemon_lock()` powers the launcher's attach decision. The lock-file path is **derived
from the resolved `db` dir** (not hardcoded) so `--db /tmp/x.db` can't bypass it.

**Dependency:** add `fs2` (a tiny, ubiquitous `flock` wrapper) to `taski-daemon` and record
it in `tech.md`. (`libc` or `nix` would also work; `fs2` is the lowest-friction.)

**SQLite in one process:** the daemon's writer `Connection` and the TUI's reader
`Connection` remain **two separate connections**, each created and owned by its thread
(`rusqlite::Connection` is `Send` but not `Sync` — create it inside the spawned closure).
Behavior is identical to today's two processes.

### Complexity Estimate

**Effort:** **M.** The machinery is conventional (threads + an atomic flag + a file lock);
the work is mostly the lib refactor and the shutdown/lock plumbing.
**Risk Level:** **Medium.** No change to *what* writes the vault, but combined mode
introduces a novel concurrency shape (two threads on one DB, the shutdown handshake, raw-mode
+ ctrlc coexistence) and sits directly adjacent to the write-back safety contract. The lock
(ADR-0008) is the safety closure.
**Dependencies:** `fs2` (new, tiny). No schema change, no `taski-core` change.

### Vertical-Slice Phasing (each phase leaves the app runnable)

- **Phase A — Library refactor (low risk):** `DaemonOpts`/`ShutdownSignal`/`run_daemon` in
  `taski-daemon`; `taski-tui` → lib+bin with `run()`/`run_with_shutdown()`. Binaries behave
  identically; all tests green. Pure refactor; unlocks B.
- **Phase B — Unified `taski` binary, combined mode, lock-then-refuse (riskiest):** new
  `crates/taski`; combined = lock-probe → spawn daemon thread + TUI (free) or **refuse**
  (held, with a clear message); shared `ShutdownSignal` + **final drain**; daemon tracing →
  log file; daemon thread wrapped in `catch_unwind`. Leaves `taski` as a working daily
  driver. *Documented limitation until C: "if launchd's daemon also runs, use `taski tui`."*
- **Phase C — Attach-or-spawn + launchd migration (safety completion):** flip combined-mode
  "refuse if locked" to **"attach if locked"**; `taski daemon` refuse-if-locked; update
  `install-launchd.sh` (install `taski`, plist → `taski daemon`); update `context.md` /
  `setup.md` / `tech.md`. The double-writer hazard is now fully closed and launchd + `taski`
  coexist correctly.

**Why B is the riskiest:** it introduces all the novel machinery at once — two threads on
one SQLite file, the shutdown handshake ordering, ctrlc/raw-mode coexistence, the tracing
sink, the panic-isolation boundary. Validate specifically: (1) two-conns-in-one-process
reads see the other's commits within the 750 ms poll; (2) Ctrl-C in combined mode is caught
by the TUI key handler and does *not* terminate via the dormant global handler; (3)
`join()` after terminal-restore never hangs; (4) no `tracing` output reaches the terminal
during a TUI session.

### Questions for Development

- Confirm the combined-mode attach behavior (vs. refuse) is the wanted default — this is
  the biggest deviation from the naive "lock + refuse" pattern. *(Recommended: attach.)*
- `fs2` vs. `libc` vs. `nix` for the flock call? *(Recommend `fs2` — smallest surface.)*
- Should the standalone `taski-daemon` binary also acquire the lock (so a launchd daemon and
  a hand-run `taski-daemon` can't clash)? *(Recommend: yes — acquire in `run_daemon`, so all
  daemon entry points are protected uniformly.)*

---

## ADRs to Record

- **[ADR-0007](../adr/0007-unified-in-process-launcher.md) — Unified `taski` binary
  (in-process daemon + TUI; session-scoped daemon).** Decides topology (in-process over
  child-process), the subcommand surface, the shared `ShutdownSignal`, and the final-drain
  protocol. Prevents: binary proliferation, lost-in-flight toggles on quit, ctrlc-handler
  conflict, stderr-corrupting-TUI.
- **[ADR-0008](../adr/0008-single-writer-file-lock.md) — Single-writer enforcement via file
  lock (extends ADR-0002).** Decides `flock(LOCK_EX|LOCK_NB)` on `<data-dir>/daemon.lock`
  with attach-or-spawn for combined mode and refuse for `taski daemon`. Prevents:
  `atomic_write` temp-file collision and the `reconcile_note` read-modify-write race (both
  unguarded by ADR-0004's TOCTOU check, which assumes a single writer).

---

## Reality Check

### Risks

**Double-writer is the #1 risk and it is a *corruption* vector, not a UX bug.** Two daemons
break two assumptions baked into the codebase:

1. **`atomic_write` writes a fixed-name temp** (`<note>.taski.tmp`). Two daemons both create
   it → they truncate each other's temp → both `rename` the same path over the target. The
   ADR-0004 TOCTOU re-hash guard does **not** save you (both see the same target hash).
   *Closed by the `flock` (Phase C; guarded by refuse in B).*
2. **`reconcile_note` is a read-modify-write** (`SELECT old rows → UPDATE/INSERT/DELETE`).
   Two concurrent reconciles interleave → duplicate/dropped surrogate ids, broken identity
   mapping (ADR-0005 assumes single-threaded reconciliation). *Closed by the `flock`.*

Note SQLite WAL does **not** prevent these: WAL serializes DB transactions, but the races are
on **vault files and the reconciliation window**, which SQLite never sees. Spurious
"checkbox line changed" failures from double-drained `pending_actions` are also possible
(the double-flip itself is guarded by ADR-0005's `raw_checkbox_char` re-check, but confusing).

Other gotchas: `ctrlc::set_handler` is process-global and single-install (only the daemon
installs it; dormant in combined mode); daemon-thread `tracing` must go to the log file, not
stderr, or it corrupts the alt-screen; a daemon-thread panic fires the TUI's global panic
hook and yanks the terminal out of raw mode mid-session — wrap the daemon thread in
`catch_unwind` and exit cleanly on `Err`.

### Alternative Approaches Considered

| Approach | Pros | Cons | Why Not Chosen |
|---|---|---|---|
| **Child-process launcher** (spawn `taski-daemon` binary) | Best panic isolation; reuses the binary verbatim | Binary discovery/shipping (2 binaries); cross-process shutdown signaling; **stderr interleaves with the TUI's alt-screen**; awkward subcommand mapping | In-process wins on every axis that matters for a personal tool; panic isolation is marginal here |
| **Third launcher binary** beside the two existing | Keeps binaries fully independent | Three binaries; the user must know which to run — defeats the "one command" goal | Explicitly rejected by the user (simplest UX) |
| **pidfile** (write PID, no `flock`) | Simple | Stale after crash → liveness check + PID-reuse hazard; racy | `flock` is auto-released on crash — strictly more robust |
| **SQLite `daemon_lock` row** | No new deps | Stale after crash; long `BEGIN EXCLUSIVE` bloats WAL; short txns don't actually prevent two daemons | Same staleness problem as pidfile, more friction |
| **Refuse (not attach) in combined mode** | Simplest lock semantics | The default `taski` fails in exactly the config `install-launchd.sh` creates | Attach is safer (never a second daemon) *and* better UX |
| **Block on first scan before painting the TUI** | No empty-state flash | Penalizes the common (already-populated) case; unbounded blank-screen startup | Don't block; the 750 ms poll catches up fast |

### Kill Switch

If combined mode proves unstable or the two-threads-on-one-DB model causes subtle issues,
the standalone binaries and the launchd path are untouched — `taski` can be withdrawn
without affecting anyone who didn't adopt it. The lock (ADR-0008) is independently valuable
and should stay regardless.

### Confidence Level

**Overall Confidence:** High. The topology is conventional, the safety closure (the lock)
is well-understood Unix plumbing, and nothing touches *what* writes the vault. The only
novel concurrency lives in Phase B, which is testable in isolation.

---

## Summary

**Key Behaviors:** `taski` runs daemon + TUI together (default), attaching to a running
daemon if one exists; `taski daemon` / `taski tui` run either alone; quit drains and exits
cleanly.

**Biggest Risk:** the double-writer corruption vector — closed by `flock` (ADR-0008), the
one piece that directly extends the write-back safety contract.

**Next Actions:**
1. Record **ADR-0007** (unified in-process launcher) and **ADR-0008** (single-writer file
   lock).
2. **Phase A** — lib refactor (`run_daemon`/`ShutdownSignal`; `taski-tui` lib+bin).
3. **Phase B** — `crates/taski` + combined mode + lock-refuse + final drain (riskiest).
4. **Phase C** — attach-or-spawn + `install-launchd.sh` migration + docs.
