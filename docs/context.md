# Taski — Engineering Context & Onboarding

*Onboarding guide for new engineers. Last updated: 2026-06-20 (v0.4 — adds `exclude_dirs` config + ADR-0011 bullet toggle and undo; 165 tests across 6 crates).*

This document is the "operating manual" for working on Taski: what it is, how it's
built, the decisions that are load-bearing (and must not be casually undone), and the
landmines that will bite you if you don't know they're there. Read this first, then the
[`PRD`](./PRD.md), [`tech.md`](./tech.md), and the [ADRs](./adr/).

---

## TL;DR

**Taski** is a personal, single-user partner app for [Obsidian](https://obsidian.md). It
continuously scans one Markdown vault, extracts every checkbox task (`- [ ]`, `- [x]`,
`- [/]`) into a structured SQLite index, and shows them in a terminal UI where you can
browse, filter, and toggle them — with toggles written safely back into the notes.
Obsidian stays the source of truth; Taski is the fast "execution layer" over scattered
tasks.

A unified Rust binary (`taski` — daemon + TUI in one process) backed by a shared SQLite file (the standalone `taski-daemon` / `taski-tui` binaries are kept for backcompat):

```
 Obsidian vault ──watch──▶ taski-daemon ──write──▶ SQLite (taski.db) ◀──read─── taski-tui
   (source of       (sole writer to vault            tasks + pending_actions        (polls)
    truth)           + to the index)
                          ▲                                  │ inserts action rows
                          └──────────────────────────────────┘
```

The whole point of the architecture: **SQLite is the decoupling boundary.** The daemon
writes; the TUI reads; write-back commands flow back through a `pending_actions` table
that only the daemon executes. By default `taski` runs both roles together in one process
(the diagram's two boxes are the standalone binaries, still kept for backcompat); neither
side talks to the other directly.

---

## Repository Layout

Cargo workspace, edition 2024, six crates. Dependencies point downward only (no cycles):

| Crate | Responsibility | Key file(s) |
|---|---|---|
| `taski-core` | **Pure** domain: `Task`/`Status` types, the Markdown parser (`parse_tasks`, fence-aware), emoji-date extraction (`extract_due_date` for 📅/📆/🗓, `extract_scheduled_date` for ⏳ — both via shared `extract_emoji_date`; ADR-0009), and pure `ymd_from_unix` (today's date, no date crate). No FS, no I/O, no deps on other taski crates. | `crates/taski-core/src/lib.rs` |
| `taski-config` | TOML config loading (`~/.config/taski/config.toml`) + CLI→config→default precedence + the `template()` renderer for `--init-config`. Fields include `exclude_dirs` for skipping vault subdirectory trees. Keeps FS/TOML out of `taski-core`. | `crates/taski-config/src/lib.rs` |
| `taski-db` | The canonical SQLite schema, `open()` (WAL + schema + dir creation), and all read/write APIs (`all_tasks`, `reconcile_note`, `enqueue_action` / `enqueue_set_scheduled` / `enqueue_bullet_toggle`, `pending_actions`, `prune_old_actions`, `delete_tasks_for_excluded_dirs`, …). Owns `tasks` + `pending_actions` + `note_contents`. | `crates/taski-db/src/lib.rs` |
| `taski-daemon` | The watcher/scanner + **sole writer to the vault**: the reusable engine `run_daemon(opts, shutdown, lock)`, plus `scan_vault`, `index_note`, `process_action` (checkbox flips) / `process_metadata_action` (`⏳` writes) / `process_bullet_action` (checkbox↔bullet toggle) — all three reuse `atomic_write` (ADR-0009/0011), the watch loop; the `ShutdownSignal`/`ShutdownHandle` pair; and the `flock` single-writer lock (`DaemonLockGuard`/`acquire_daemon_lock`/`LockOutcome`). The drain loop dispatches on `pending_actions.action_type`. Also handles `exclude_dirs` purge + filtered scanning. **lib + bin** — a `taski-daemon` binary *and* the library the unified launcher depends on. | `crates/taski-daemon/src/{lib,main,shutdown,lock}.rs`, `tests/` |
| `taski-tui` | The `ratatui` client: polls the index, groups by note, filters (status-cycle `f`, Today view `T`, text search `/`, file search `F`), renders, submits toggle (`Space`) / mark-for-today (`t`) / bullet toggle (`b`) / undo (`u`) actions, and shows the context pane via the cached `note_contents` table. Never touches vault files. **lib + bin** — public entry points `run()` / `run_with_db(db)` / `run_combined(db, quit_hook)`; `main.rs` is a thin shim. Key internal modules: `App` (state machine), `build_view` (filter pipeline), `draw` (render), `run_loop` (input). | `crates/taski-tui/src/{lib,main}.rs` |
| `taski` | The **unified launcher** binary: runs the daemon (background thread) + TUI (main thread) together by default (`taski`), or either alone via `taski daemon` / `taski tui` subcommands. Attach-or-spawn + single-writer lock (ADRs 0007/0008). | `crates/taski/src/main.rs` |

Supporting: `docs/` (PRD, tech, ADRs, setup, this file), `scripts/install-launchd.sh`
+ `uninstall-launchd.sh`, `.github/workflows/ci.yml`, `rust-toolchain.toml`.

---

## Build, Run, Test

```sh
cargo build --workspace                       # dev build
cargo build --release --workspace             # optimized daily-driver binaries
cargo test --workspace                        # all tests (~165 as of v0.4)
cargo test -p taski-daemon writeback          # run one suite / filter by name
```

The CI gates (`.github/workflows/ci.yml`, macOS) are exactly three steps:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

(`--all` is cargo's alias for `--workspace`.) There is **no** `cargo-deny`/supply-chain
step in CI — install and run it locally if you want it. `rust-toolchain.toml` pins
**stable**, which auto-installs on first `cargo` invocation.

**Run it** (daily driver): see [`setup.md`](./setup.md). Short version:

```sh
./target/release/taski                                                    # combined: daemon + TUI, drains on quit
./target/release/taski daemon                                             # daemon only (what launchd runs)
./target/release/taski tui                                                # TUI only (reader)
./target/release/taski-daemon --init-config --vault /path/to/your/vault   # one-time config
./target/release/taski-daemon --once --vault /path/to/vault               # single scan + exit (no watcher)
# or autostart the daemon at login:
scripts/install-launchd.sh
```

Config precedence is **CLI flag → config file → compiled default**. `vault` has no
default (daemon requires it); `db` defaults to `./taski.db`. Config location is
`~/.config/taski/config.toml`, overridable with the `TASKI_CONFIG` env var.

### Debugging

- **Logs:** the daemon logs via `tracing` at `info` by default. Where they go depends on
  the entry point: `taski daemon` and standalone `taski-daemon` log to **stderr** (so
  under launchd they stream to `~/.local/share/taski/daemon.log`); `taski` in **combined
  mode** routes the daemon thread's tracing to that log **file** directly (never stderr —
  stderr would garble the TUI's alternate screen). Set `RUST_LOG=debug` (or
  `taski_daemon=trace`) to see reconciliation summaries, action outcomes, and conflict
  reasons.
- **Inspect the index/queue directly:**
  `sqlite3 ~/.local/share/taski/taski.db "SELECT id,note_path,state,error FROM pending_actions ORDER BY id DESC LIMIT 10"`
  — the fastest way to answer "why didn't my toggle land?" (`state` is `pending`/`done`/`failed`).
- **Shutdown:** `taski daemon` / standalone `taski-daemon` install a ctrlc handler — the
  **first** Ctrl-C initiates a clean shutdown (up to ~500ms, the event-loop tick); a
  **second** Ctrl-C force-terminates. In **combined mode** there is no ctrlc handler — the
  TUI runs in raw mode (which swallows `SIGINT`) and `q`/`Esc`/Ctrl-C all drive the same
  shutdown path; the daemon then drains and the process exits. A brief pause after quit is
  the drain, not a hang.
- **Latency expectations:** FS events are debounced **300ms**; the daemon event loop ticks
  every **500ms**; the TUI re-reads the index every **750ms**. So a toggle or an Obsidian
  edit typically reflects in 1–2s.

---

## The Mental Model: Two Data Flows + TUI Filtering

Understanding these two flows is 90% of understanding the codebase.

### 1. Indexing (vault → index), daemon-owned

```
FS event (debounced 300ms) ─▶ scan_vault / index_note(note)
                              └─▶ taski_core::parse_tasks(note_text)   // fence-aware, extracts 📅 due + ⏳ scheduled dates
                                  └─▶ taski_db::reconcile_note(...)    // ADR-0005 (see below)
```

`reconcile_note` matches the freshly-parsed tasks against existing rows for that note by
`text_hash` (content-hash). Matches **keep their surrogate `id`** (UPDATE in place); the
rest are deleted/inserted. This is how a task keeps its identity when text *above* it
shifts its `line_number`.

### 2. Write-back (TUI → vault), via the daemon as sole writer

```
TUI: user hits Space on a task
 └─▶ taski_db::enqueue_action(task_id, expected_char, new_char, ...)   // inserts a 'pending' row
       (daemon, next tick)
       └─▶ process_action: SELECT the task row for its CURRENT line_number (not the stale action line)
           ├─▶ check: current note content-hash == stored note_hash?  else → ConflictNoteChanged (refuse)
           └─▶ atomic_write(note, expected_hash, new_bytes)
                 ├─ re-read file bytes, re-hash, compare to expected_hash   // TOCTOU guard (2nd check)
                 ├─ if mismatch → WriteResult::Conflict (refuse; mark action 'failed')   // never clobber
                 └─ else → write temp (.taski.tmp) → fsync → rename over the note
            └─▶ on success: flip the checkbox, re-index the note, mark action 'done'
```

**The same pipeline carries the second action type (`set_scheduled` / `⏳` mark-for-today, ADR-0009 Phase 2).** The TUI enqueues a `pending_actions` row with `action_type='set_scheduled'` and `payload=<date>` (or `NULL` to unmark); the daemon dispatches on `action_type` and runs a structurally parallel `process_metadata_action` instead of `process_action`:

```
TUI: user hits t on a task
 └─▶ taski_db::enqueue_set_scheduled(task_id, line_number, desired_date_or_none)
        (daemon, next tick)
        └─▶ process_metadata_action: same CURRENT line_number + same content-hash check
            ├─▶ call pure taski_core::rewrite_scheduled(line, desired)
            │      → Unchanged (idempotent) → Applied, no write
            │      → Unparseable → MetadataUnparseable, refuse
            │      → Rewritten(new_line) → splice ONLY the target line bytes
            └─▶ same atomic_write(note, expected_hash, new_bytes)   // UNCHANGED: whole-file TOCTOU
                 └─▶ on success: re-index the note, mark action 'done'
```

`atomic_write` and its TOCTOU guard are reused **verbatim** — the whole-file re-hash is
byte-count-agnostic, so whether the mutation was a single-char flip or a variable-length
`⏳` insertion, the same conflict check protects the vault. The pure `rewrite_scheduled`
oracle is testable without any I/O and is guarded by its own 256-case proptest.

The TUI **never** opens a vault file. It only inserts `pending_actions` rows. Only the
daemon mutates notes, and only after byte-re-verification. This is the core safety
guarantee (ADRs 0002/0003/0004/0009).

### 3. Process topology — the unified `taski` binary (ADRs 0007/0008)

By default `taski` runs **both roles in one process**: the daemon on a background thread,
the TUI on the main thread, each with its own SQLite `Connection` (WAL's
one-writer/many-readers is per-*database*, not per-process — so this is identical to the
two-process case). They share a `ShutdownSignal` (`Arc<AtomicBool>`): on TUI quit
(`q`/`Esc`/`Ctrl-C`) the TUI sets it, restores the terminal, and the launcher `join`s the
daemon thread — which first **drains `pending_actions`**, then exits (so a toggle done
right before `q` lands in this session). A `flock` on `<db_dir>/daemon.lock` guarantees a
**sole writer**: if the lock is already held (e.g. launchd's daemon is running), `taski`
**attaches** — runs the TUI-only against that daemon (a reader) instead of spawning a
second daemon. `taski daemon` and `taski-tui` run either side standalone. The daemon
thread is wrapped in `catch_unwind` so a panic/error sets the shutdown signal and logs
rather than corrupting the TUI mid-session.

### 4. TUI Filter Composition (the four filter axes)

The TUI's `build_view()` ANDs four independent filter axes in a single pass over the task
list. This means filters narrow each other — adding a filter can only reduce the visible set:

| Axis | Gesture | Scope |
|---|---|---|
| Status cycle | `f` | `All` → `Open` → `Done` → `All` |
| Today view | `T` | Tasks whose `scheduled_date == today` |
| Text search | `/` | Case-insensitive substring of `task.text` |
| File search | `F` | Case-insensitive substring of `task.note_path` |

Both search prompts are modal (Vim `/` style): keystrokes build the query until
dismissed. `Enter` keeps the filter applied; `Esc` clears it. Only one search prompt is
active at a time (whichever key was pressed last), but both filters remain applied
independently — so you can `/deploy` for text, dismiss with `Enter`, then `F alpha` for
file, and the view shows only tasks whose body contains "deploy" **and** whose note path
contains "alpha".

The `build_view` function signature captures all four axes:
```rust
fn build_view(
    tasks: &[Task],
    filter: StatusFilter,        // All / Open / Done
    expanded: &HashSet<String>,  // group collapse state
    today_only: bool,
    today: &str,                 // today's date string
    search_query: &str,          // text search
    file_query: &str,            // file/path search
) -> Vec<DisplayRow>
```

---

## TUI Keybinding Reference

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Move selection up/down |
| `Space` | Toggle selected task open ↔ done |
| `Enter` | Toggle group expand/collapse on header; fold sub-tasks on task |
| `←` / `→` | Collapse / expand group at cursor |
| `Tab` / `⇧Tab` | Expand all / collapse all groups |
| `f` | Cycle status filter: All → Open → Done → All |
| `T` | Toggle Today view (tasks scheduled for today) |
| `t` | Mark/unmark selected task for today (writes `⏳ <today>`) |
| `b` | Toggle selected task between checkbox (`- [ ]`) and bullet (`-`) format |
| `u` | Undo the last checkbox flip or bullet toggle action |
| `/` | Open text search prompt (matches `task.text`, case-insensitive) |
| `F` | Open file/path search prompt (matches `task.note_path`) |
| `p` | Toggle the context pane (right-half note preview) |
| `J` / `K` | Scroll context pane up/down |
| `q` / `Esc` / `Ctrl-C` | Quit |

While a search prompt is active: `Esc` cancels (clears filter), `Enter` dismisses (keeps
filter), `Backspace` edits query, characters build query.

---

## Data Model (schema v5)

Defined in `taski-db::SCHEMA`. `PRAGMA user_version` tracks the version; older DBs are
dropped and recreated (pre-MVP, no data to preserve). v3 added the `note_contents` cache
that backs the read-only TUI context pane ([ADR-0006](./adr/0006-note-content-cached-in-index.md));
v4 added `tasks.scheduled_date` (`⏳`) backing the "Today" view
([ADR-0009](./adr/0009-scheduled-date-today.md), Phase 1); v5 added
`pending_actions.action_type` + `payload` for the `⏳` write gesture (ADR-0009, Phase 2).

**`tasks`** — one row per checkbox task found in the vault:

| Column | Notes |
|---|---|
| `id` | `INTEGER PRIMARY KEY AUTOINCREMENT` — **surrogate identity**, never reused (ADR-0005). NOT path+line. |
| `note_path`, `line_number` | **Write-time location only**, re-verified against file bytes before any mutation. Not trusted as identity. |
| `text`, `text_hash` | Body + its hash; `text_hash` drives reconciliation and the write-back TOCTOU check. |
| `status` | `open` / `done` / `in_progress` (+ other Obsidian chars). Reconstructed from `raw_checkbox_char` on read (`all_tasks`); the stored column is never consulted, so the two can't drift. |
| `raw_checkbox_char` | The exact checkbox char seen at scan; re-verified before flipping. |
| `note_hash` | Content hash of the note at last scan — **the** conflict-detection input (re-checked before write-back). |
| `note_mtime` | Note mtime at last scan — **informational only**, not used by conflict detection. |
| `due_date` | Parsed 📅/📆/🗓 date (Obsidian Tasks-plugin syntax). |
| `scheduled_date` *(v4)* | Parsed `⏳` scheduled date (Obsidian Tasks-plugin syntax — "plan to work on this"). Backs the Today view (`T`) and is *written* by the `t` "mark for today" gesture ([ADR-0009](./adr/0009-scheduled-date-today.md)). |
| `updated_at` | Last-seen timestamp. |

**`pending_actions`** — the TUI→daemon command queue. Lifecycle `pending → done | failed`.
Each row carries `task_id`, an `action_type` (`checkbox`, `set_scheduled`, `toggle_bullet`,
or `undo`), and a `payload` (NULL for checkbox flips; the desired date / NULL-to-unmark for
`set_scheduled`; the prior checkbox char for undo). Checkbox rows also hold
`expected_char`/`new_char`; date-action rows leave them empty and the daemon dispatches on
`action_type`. On failure an `error` is recorded. Resolved rows older than 7 days are pruned
on daemon startup (`ACTION_RETENTION_SECS`).

**`note_contents`** *(v3)* — per-note full-text cache backing the read-only TUI context
pane ([ADR-0006](./adr/0006-note-content-cached-in-index.md)). One row per indexed note:
`note_path` (PK), `content` (full UTF-8 text), `note_hash` (mirrors the note's
`tasks.note_hash`), `updated_at` (informational). The daemon writes it in the same
`index_note` pass that parses tasks, so content, hash, and task `line_number` all derive
from one byte snapshot. The TUI reads it via `db::note_content()` — it still never opens a
vault file. Window sizing is a render concern and lives in the TUI, not the index.

---

## Key Design Decisions (read the ADRs — these are load-bearing)

Each of these exists for a concrete reason. **Do not undo one without reading its ADR and
understanding the failure mode it prevents.**

1. **rusqlite + WAL, not Limbo/Turso** ([ADR-0001](./adr/0001-rusqlite-not-limbo.md)) —
   Limbo had no multi-process WAL access (hard blocker for a separate TUI process). Don't
   "modernize" to Limbo without confirming `multiprocess_wal` is stable *and* drops the
   no-mixing rule.

2. **Write-back routes through the daemon** ([ADR-0002](./adr/0002-write-back-through-daemon.md))
   — the daemon is the *sole* writer to the vault, draining `pending_actions`. The TUI
   must never write a note directly. This is what makes write-back auditable and safe.

3. **Checkbox-state flips only** ([ADR-0003](./adr/0003-checkbox-only-mvp.md), **amended by [ADR-0009](./adr/0009-scheduled-date-today.md)**)
   — MVP write-back flips `[ ]↔[x]`, nothing more. Text/metadata edits are explicitly deferred.
   Adding "edit task text from the TUI" is a *big* change, not a small one. ADR-0009 widens
   the scope to **also** permit Obsidian-standard date-emoji metadata (`⏳` scheduled) for the
   "mark for today" gesture — but only tokens that are standard syntax + grammar-provably
   safe; free-text edits and creates/deletes remain rejected.

4. **Refuse-on-conflict, never last-write-wins** ([ADR-0004](./adr/0004-refuse-on-conflict.md))
   — before renaming, re-read the note and re-hash; if it changed since scan, *refuse*
   (mark the action `failed`), do not overwrite. The addendum hardens the temp→rename
   step against TOCTOU. If you ever feel tempted to "just write it," don't.

5. **Surrogate rowid identity + content-hash reconciliation** ([ADR-0005](./adr/0005-surrogate-identity.md))
   — `id` is an autoincrement integer (stable, never reused), decoupled from location.
   `(note_path, line_number)` is a write-time location claim, re-verified against bytes.
   Crucially: **Taski injects nothing into the vault** (unlike Logseq-style inline IDs);
   identity is reconciled from content each scan. This was validated against
   Obsidian-Tasks prior art. (Note: ADR-0009's `⏳` write is *native Obsidian Tasks syntax* —
   human-readable and consumed by Tasks/Dataview — not the foreign opaque identity marker
   this ADR rejected, so ADR-0005 is **not** amended by it.)

6. **Note content cached in the index for the TUI context pane** ([ADR-0006](./adr/0006-note-content-cached-in-index.md))
   — the daemon caches each note's full text in `note_contents`; the TUI reads it like any
   other index data. The TUI **still never opens a vault file** — this is a read path, not
   a relaxation of ADR-0002. Chosen over "TUI reads the vault directly" so content and task
   locations stay consistent (same scan) and the SQLite decoupling boundary stays intact.

7. **Unified launcher + single-writer lock** ([ADR-0007](./adr/0007-unified-in-process-launcher.md), [ADR-0008](./adr/0008-single-writer-file-lock.md)) — one `taski` binary runs daemon + TUI in-process; a `flock` on `<db_dir>/daemon.lock` guarantees a sole writer across all startup combinations (launchd + `taski`, etc.), closing the two-daemon corruption vector ADR-0004 doesn't cover.

8. **Scheduled date `⏳` + Today view** ([ADR-0009](./adr/0009-scheduled-date-today.md)) —
   Taski parses Obsidian's `⏳` scheduled date and writes `⏳ today` as the "mark for today"
   triage gesture. Phase 1 (parser + schema v4 + read-only `T` Today view) and Phase 2 (the
   `t` toggle write gesture, schema v5 `pending_actions.action_type`/`payload`, the pure
   `taski_core::rewrite_scheduled` line-rewrite + daemon `process_metadata_action` reusing
   `atomic_write` unchanged) are both shipped. This is Taski's **first non-checkbox vault
   write**; it amends ADR-0003 (write-back scope) but not ADR-0005. The view is strict
   `scheduled_date == today`, orthogonal to the `f` status-cycle. "Today" is computed by the
   pure `taski_core::ymd_from_unix` (no date crate). Two 256-case proptests guard the write.

9. **Text and file search in the TUI** ([ADR-0010](./adr/0010-text-search.md)) —
   Two independent modal search gestures: `/` for case-insensitive substring of
   `task.text`, `F` for case-insensitive substring of `task.note_path`. Both AND with
   each other and with the status/Today filters. Only one search prompt active at a time
   (whichever key was pressed last), but both filters stay applied independently.
   Initially implemented as a single unified search (text + path together), then split
   into two gestures because a path match can accidentally broaden a text search.

10. **Bullet toggle and undo** ([ADR-0011](./adr/0011-bullet-toggle-undo.md)) —
   Two new action types: `toggle_bullet` (`b` key, toggles `- [ ] task` ↔ `- task`) and
   `undo` (`u` key, reverses the last checkbox or bullet action). Both route through the
   daemon's existing `process_action` pipeline (same `lookup_task_for_action` +
   content-hash + `atomic_write`). No schema change — `pending_actions.action_type` is
   already a TEXT column. Undo queues the reverse action immediately without waiting for
   the original to resolve; if the original failed, the undo fails naturally because the
   daemon re-verifies current state. `t` (mark-for-today) is not undo-able — it's
   already idempotent (pressing `t` again removes the mark).

11. **`exclude_dirs` config for vault directory exclusion** — The `exclude_dirs` field
    in `config.toml` skips whole subdirectory trees (e.g. `_System/Templates`) from vault
    scanning and indexing. Works alongside the always-excluded hidden dirs (`.obsidian`,
    `.trash`, `.git`). On daemon startup, stale entries matching excluded prefixes are
    purged from both `tasks` and `note_contents`. Watcher events inside excluded dirs are
    also dropped. Exclude paths are relative to the vault root.

---

## Gotchas & Landmines (read this before you change anything)

These are the things that aren't obvious from reading the code and will cost you time.

- **Never run tests against the real vault.** The real vault
  (`/Users/.../Personal-PARA`) is the user's data. All tests use `tempfile` fake vaults
  or `:memory:` DBs. If you point a test or a `cargo run` at the real vault, you risk
  mutating real notes. The daemon's `atomic_write` is safe, but **don't rely on that as
  license to test against real data.**

- **`process_action` targets the row's *current* `line_number`, not the action's.** A
  `pending_action` captures a stale `line_number` at enqueue time. When executing, the
  daemon re-reads the task row to get the *current* line, then re-verifies bytes. If you
  "simplify" this to use `action.line_number` directly, you'll re-introduce a
  wrong-line-corruption bug.

- **`atomic_write` re-verifies bytes before rename (TOCTOU).** The expected-hash check
  happens *after* writing the temp file, immediately before `rename`. Don't move or
  reorder the re-read — it's the guard against concurrent Obsidian edits.

- **The metadata write path (`process_metadata_action`) must handle CRLF lines.** A line
  ending in `\r\n` has `rewrite_scheduled(line, ...)` called with the `\r`*included* in
  `line` (it's a terminal byte, not part of the splice). The returned `new_line` must be
  spliced in at a `content_end` that *excludes* the `\r`, so `\r\n` is preserved outside
  the changed range. If you compute the splice span naively from `line.len()`, the `\r`
  gets removed on CRLF-terminated notes. The `metadata_writeback_proptest` catches this —
  its `check_oracle` assertion uses `str::lines()` (which strips `\r`) as the independent
  reference to verify the written result still parses to the same `⏳` date.

- **`db::open()` creates parent directories.** SQLite returns `SQLITE_CANTOPEN` if the
  db's directory doesn't exist, so `open()` `create_dir_all`s the parent first. This is
  why `~/.local/share/taski/taski.db` works on first run. (A bare filename or `:memory:`
  has no parent and is left alone.) `open()` returns `anyhow::Result`.

- **Schema migration is destructive.** `ensure_schema` drops+recreates tables on a
  version bump (fine pre-MVP, no data to keep). If you change the schema, bump
  `SCHEMA_VERSION` and know that existing dev DBs get wiped. A real migration path is
  deferred.

- **`rusqlite` is pinned to `0.39`** — `0.40` pulls a `libsqlite3-sys` whose build script
  uses unstable `cfg_select!` and won't compile on stable 1.93. Don't bump it blindly;
  see the pin note in [`tech.md`](./tech.md).

- **Edition 2024 made env mutation `unsafe`.** `std::env::set_var`/`remove_var` are
  `unsafe` now (and racy under parallel tests). When you need env-dependent behavior,
  factor the logic into a pure helper that *takes* the env value as an argument (see
  `taski_config::config_path_from`). Don't sprinkle `unsafe` in tests to work around it.

- **`notify-debouncer-mini` doesn't report event *kind*.** You get "something changed,"
  not create/modify/delete. The daemon decides what to do by checking file existence.
  Don't assume you can branch on event type.

- **The single-writer lock is load-bearing (ADR-0008).** The daemon acquires
  `flock(LOCK_EX | LOCK_NB)` on `<db_dir>/daemon.lock` before scanning. Two daemons would
  corrupt the vault — `atomic_write`'s fixed-name temp collision + `reconcile_note`'s
  read-modify-write race, *neither* guarded by ADR-0004's TOCTOU check and *both* invisible
  to WAL. `run_daemon` takes a `DaemonLockGuard` as a **capability token**: you cannot call
  it without acquiring the lock, and the guard can't be forged (its constructor is private).
  Don't add a daemon entry point that bypasses `acquire_daemon_lock`.

- **Combined mode routes daemon tracing to the log file, never stderr.** In `taski`
  (combined) the TUI owns the alternate screen; any `eprintln!` or `tracing`→stderr on the
  daemon thread garbles it. Daemon-thread events go to `<db_dir>/daemon.log` via
  `init_tracing_to_file`. If you add code that runs on the daemon thread, use `tracing`
  (not `eprintln!`). (`taski daemon` / standalone `taski-daemon` still use stderr — fine,
  no TUI.)

- **`ctrlc::set_handler` is process-global and single-install.** Only `taski daemon` and
  standalone `taski-daemon` install it. Combined mode must **not** — the TUI runs in
  crossterm raw mode, which swallows `SIGINT` and delivers it as a key event the TUI
  handles (driving the same shutdown path as `q`). Installing it from both paths errors.

- **`--once` and `--init-config` live only on `taski-daemon`, not on `taski`.** The
  unified `taski` binary exposes only the global `--vault`/`--db` flags plus the
  `daemon`/`tui` subcommands. For a one-shot scan or config generation, use the standalone
  `taski-daemon` binary.

- **The TUI does surface refused toggles** as a one-line notice (via `recent_actions` →
  `friendly_failure_reason`), cleared on the next action. But the TUI↔daemon coupling is
  loose *by choice*: `friendly_failure_reason` string-matches the daemon's `ApplyOutcome`
  phrases, with a generic fallback. A structured reason-code was considered and deferred
  (low value for a personal tool). If you change daemon error wording, sanity-check the
  TUI messages.

- **`exclude_dirs` SQL LIKE patterns need a trailing `%`.** When purging indexed tasks
  for an excluded directory, the SQL is `DELETE … WHERE note_path LIKE ?` with the bind
  value `_System/Templates/%`. The `%` is required — without it, LIKE matches only the
  literal directory path (and SQL's single-char `_` wildcard makes it hairier). If purge
  silently does nothing, check that the bind value ends with `/%`.

- **Undo scope is limited to `Space` and `b` only.** `u` undoes the last checkbox flip
  or bullet toggle, not `t` (mark-for-today). The `t` gesture is already idempotent
  (pressing `t` again removes the mark), so undo adds little value. This is intentional,
  not a bug.

- **Tags are local-only.** `v0.1` and all commits exist only in the local repo until
  pushed. There is currently no remote set up in this working tree — confirm before
  assuming `git push` will work.

- **The `run_loop` branches on search state before normal key dispatch.** When
  `app.searching` or `app.file_searching` is true, most keystrokes build the search query
  instead of performing their normal action. This means adding a new keybinding requires
  checking whether it should also be available during a search prompt. So far only `Esc`
  and `Enter` are handled during both, and `Enter` just dismisses the prompt.

---

## Development Workflow & Conventions

- **Vertical slices.** Work is organized as end-to-end slices (see PRD §12), each leaving
  the app runnable. We prove the riskiest thing (write-back) early, not last. Commit
  messages follow Conventional Commits: `feat:`, `fix:`, `chore:`, `docs(adr):`.

- **The gates are non-negotiable.** CI (macOS, `.github/workflows/ci.yml`) runs exactly
  `fmt --check`, `clippy -D warnings`, and `test` — **no** `cargo-deny`. Run those three
  locally before considering work done:
  `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all`.

- **Test the hard paths; property-test the invariants.** Write-back correctness is
  guarded by a 256-case proptest ("never corrupts": arbitrary task + arbitrary concurrent
  byte change → either the flip lands or it's refused, never corruption). The parser has
  a proptest for "never panics on arbitrary input." When you touch these areas, keep the
  proptests green — they encode the safety contract.

- **Decisions go through ADRs.** When you make (or revise) a load-bearing choice, record
  it in `docs/adr/` and update `tech.md`. Don't let important decisions live only in code
  comments or commit messages.

- **Keep `taski-core` pure.** No filesystem, no I/O, no deps on other taski crates. FS/
  config concerns go in `taski-config` or the binaries. This purity is what makes the
  parser cheaply testable.

- **Test both search modes.** `/` (text) and `F` (file) are independent TUI-side filters
  implemented in `build_view`. Tests cover each in isolation, case-insensitivity, compose
  with each other (text+file+status triple), and compose with status/Today filters. Since
  the search logic is pure (no DB/IO), tests live as inline unit tests in
  `crates/taski-tui/src/lib.rs`.

---

## Testing Strategy

| Location | What it guards |
|---|---|
| `taski-core` unit tests + `proptest` + `rewrite_scheduled_proptest` | Parser correctness on a synthetic corpus; never-panics on arbitrary input; due-date + scheduled-date extraction; pure `rewrite_scheduled` oracle (256-case ADR-0009 Phase 2). |
| `taski-config` unit tests | TOML parsing, precedence (CLI→config→default), env override, `template()` round-trips. |
| `taski-db` unit tests | Schema, `reconcile_note` identity retention, upsert/read round-trips, action pruning, `open()` creates missing dirs. |
| `taski-daemon/tests/scan.rs` | End-to-end scan of a fake vault → correct task rows. |
| `taski-daemon/tests/reconcile.rs` | Content-hash reconciliation: identity survives edits, deletes, reorders. |
| `taski-daemon/tests/writeback.rs` + `writeback_proptest.rs` + `metadata_writeback_proptest.rs` | The safety contract: atomic_write commits on match, refuses on conflict, never corrupts; `⏳` metadata write-back "never corrupts" (256-case ADR-0009 Phase 2, oracle = `rewrite_scheduled`, CRLF assertion, VS16 guards). Also covers `toggle_bullet` and `undo` action types (ADR-0011). |
| `taski-daemon/src/lock.rs` unit tests | The `flock` single-writer lock: acquire/refuse outcome, lock-path derivation. |
| `taski-daemon` unit tests in `lib.rs` | `should_exclude_entry`, `path_matches_exclude`, `scan_vault_with_exclude_dirs_skips_matching_directory` — exclude-dir filtering in WalkDir and watcher events. |
| `taski-tui` unit tests (in `lib.rs`) | View model: grouping, collapse, four-axis filter composition (status + today + text search + file search), display-index↔Task mapping, selection reconciliation, failure-notice surfacing, context-pane render/scroll/toggle + `context_view` centering (headless `TestBackend` smoke). |
| `taski-db` unit tests | `delete_tasks_for_excluded_dirs` — verifies exact-match and prefix-match SQL purges the right rows. |
| `taski` (unified launcher) | No unit tests by design — it's thin dispatch over the two libraries. Correctness is runtime-verified (combined spawn, attach-when-held, refuse-when-held, quit-drain); see the smokes described in ADRs 0007/0008. |

Tests use `tempfile` fake vaults and `:memory:` or temp-file DBs. The real vault is
exercised only at runtime (its `taski.db` is gitignored).

---

## Quick Reference — "I want to…"

| Task | Look at |
|---|---|
| Change how tasks are parsed / add metadata extraction | `taski-core/src/lib.rs` (`parse_tasks`, `extract_due_date`/`extract_scheduled_date` via shared `extract_emoji_date`, `ymd_from_unix`) |
| Change the DB schema | `taski-db::SCHEMA` + bump `SCHEMA_VERSION`; update `reconcile_note`/`upsert_task` |
| Cache/read note content for the TUI context pane | `taski-db`: `note_contents` table + `upsert_note_content`/`note_content`/`delete_note_content`; daemon writes it in `index_note` ([ADR-0006](./adr/0006-note-content-cached-in-index.md)) |
| Change write-back behavior | `taski-daemon`: `process_action` (checkbox flips) / `process_metadata_action` (`⏳` writes, ADR-0009), `atomic_write` (mind ADR-0004 TOCTOU); the drain loop dispatches on `pending_actions.action_type` |
| Change how the TUI looks/behaves | `taski-tui/src/lib.rs`: `App`, `build_view` (filter pipeline), `context_view`/`draw_context_pane`, key handling in `run_loop` |
| Change the TUI filter composition | `crates/taski-tui/src/lib.rs:build_view()` — ANDs status + today + text search + file search in one pass |
| Change keybindings (add/remove a key) | `crates/taski-tui/src/lib.rs:run_loop()` — handles three branches: `searching`, `file_searching`, and normal mode. `b` / `u` added in ADR-0011 |
| Change context-pane keybindings/behavior | `taski-tui/src/lib.rs` key match in `run_loop` (`J`/`K` scroll, `p` toggle) + `MIN_SPLIT_WIDTH` auto-hide; `sync_context` for the read path |
| Add/change vault directory exclusions | Add `exclude_dirs` to `~/.config/taski/config.toml`; restart daemon. Purge happens on startup — see `delete_tasks_for_excluded_dirs` in `taski-db`, `should_exclude_entry`/`path_matches_exclude` in `taski-daemon` |
| Change undo behavior | `taski-tui/src/lib.rs` `submit_undo` (enqueues the reverse via `db::enqueue_action` for checkbox undo or `db::enqueue_bullet_toggle` for bullet undo); daemon dispatches to `process_action` / `process_bullet_action` like other action types |
| Change launcher behavior (combined/daemon/tui dispatch, attach-or-spawn, shutdown handshake) | `crates/taski/src/main.rs` (`run_combined`/`run_combined_spawn`/`run_daemon_only`); ADR-0007 |
| Change the single-writer lock | `crates/taski-daemon/src/lock.rs` (ADR-0008) |
| Add a CLI flag | `Cli` struct in the relevant binary's `lib.rs`/`main.rs` |
| Change config format/precedence | `taski-config/src/lib.rs` |
| Run the app (daemon + TUI combined) | `taski` (unified binary); see [setup.md](./setup.md) |
| Run the daemon only | `taski daemon` (or the standalone `taski-daemon`) |
| Run the TUI only | `taski tui` (or `taski-tui`); a reader, safe alongside any running daemon |
| Run a one-shot scan (no watcher) | `taski-daemon --once --vault …` |
| Generate a config file | `taski-daemon --init-config --vault …` |
| Inspect the index / pending actions | `sqlite3 <db> "SELECT …"` (see Debugging) |
| Add a new dependency | add to `[workspace.dependencies]` + the crate; record in `tech.md` |
| Understand *why* something is the way it is | check `docs/adr/` first, then git history |

---

## Deferred / Intentionally Not Done (so you don't "fix" non-bugs)

A holistic review triaged these as low-value for a personal single-user tool. They are
**deliberately absent**, not oversights:

- **Retry-once on write-back conflict** — the daemon refuses and surfaces failure; manual
  retry is fine for one user.
- **`fsync` of the parent directory** after rename (M5 durability) — acceptable risk for
  personal notes.
- **Unique temp-file names** (L2) — the single-writer model makes collisions near-impossible.
- **Optimistic TUI updates** (flip the checkbox locally before the daemon confirms) —
  current behavior waits for confirmation; simpler and never lies.
- **`pulldown-cmark`** for parsing — the line-based parser handles checkboxes fine; adopt
  only when real edge cases (nested lists, callouts, inline code) actually bite.
- **Structured write-back reason-codes** between daemon and TUI — string matching + fallback.
- **Real DB migration path** — schema bumps drop+recreate (pre-MVP).
- **Additional write-back token types** — only `⏳` (scheduled, ADR-0009 Phase 2) has been
  admitted alongside checkbox flips per the ADR-0003 principled boundary. Other Obsidian
  Tasks date-emoji tokens (priority `🔼`/`🔼`/`🔽`/`⏫`/`⏬`, recurrence `🔁`, due date `📅`)
  remain unresearched. Each would need its own ADR, pure rewrite oracle, proptest, and
  action-type dispatch branch — the ADR-0009 pattern is the template.
- **Case-sensitive search toggle** — search is case-insensitive; a future config toggle
  could make it case-sensitive. Not needed for MVP (ADR-0010).
- **Search by due-date / scheduled-date** — the `/` and `F` gestures cover text and file;
  searching by date fields is a natural extension but deferred.
- **Undo of `t` (mark-for-today)** — explicitly excluded from undo scope; `t` is already
  idempotent (ADR-0011).
- **External change detection for undo** — undo only reverses the last TUI action, not
  external vault edits. Detecting external edits to offer "revert" is a separate problem.
- **Distribution / packaging / GUI / multi-vault / collaboration** — out of MVP scope (PRD §14).

If you pick one up, record the decision and update this list.

---

## Glossary

- **Write-back** — reflecting a TUI action into the originating Markdown note, via the
  daemon. Two action types: **checkbox flips** (`[ ]↔[x]`, ADR-0003) and **`⏳` scheduled-date
  writes** (`set_scheduled`, ADR-0009 Phase 2). Both reuse the same `atomic_write` TOCTOU
  guard.
- **Scheduled date (`⏳`)** — Obsidian Tasks-plugin syntax for "plan to work on this." Taski
  parses it, indexes it in `tasks.scheduled_date`, offers a **Today view** (`T`)
  of tasks whose scheduled date == today, and provides a **mark-for-today** toggle (`t`)
  that writes `⏳ <today>` into the note line (ADR-0009).
- **Today view** — the `T`-key toggled filter that shows only tasks whose
  `scheduled_date == today` (computed by `taski_core::ymd_from_unix`). Orthogonal to the
  `f` status-cycle.
- **Mark-for-today** — the `t` toggle gesture on a selected task. Idempotent: if the task
  already has `⏳ today`, pressing `t` removes it (writes `NULL`). The TUI enqueues a
  `set_scheduled` `pending_actions` row; the daemon dispatches to
  `process_metadata_action` → `rewrite_scheduled` → `atomic_write`.
- **Text search** — `/` key modal search that filters the task list by case-insensitive
  substring of `task.text`. Enter keeps the filter; Esc clears it. Independently composes
  with file search, status, and Today filters (ADR-0010).
- **File search** — `F` key modal search that filters the task list by case-insensitive
  substring of `task.note_path`. Same interaction pattern as text search. Both searches
  AND together when both are active (ADR-0010).
- **`rewrite_scheduled`** — the pure oracle in `taski-core` that takes a task line and a
  desired scheduled date, and returns a `RewriteResult` (Unchanged, Rewritten(String), or
  Unparseable). Called by `process_metadata_action`; guarded by its own 256-case proptest.
- **Bullet toggle** — the `b` keybinding that converts a checkbox task to a plain bullet
  (`- [ ] task` → `- task`) or back. Implemented as `toggle_bullet` action type, routed
  through the same daemon pipeline (ADR-0011).
- **Undo** — the `u` keybinding that reverses the last checkbox flip (`Space`) or bullet
  toggle (`b`). Queues the reverse action immediately; the daemon re-verifies current
  state, so a failed original naturally fails the undo too (ADR-0011).
- **`action_type`** — the column on `pending_actions` (schema v5) that distinguishes
  `checkbox` flips, `set_scheduled` writes, `toggle_bullet` toggles, and `undo` actions.
  The daemon drain loop dispatches on it.
- **Reconciliation** — re-matching a note's freshly-parsed tasks to existing index rows by
  `text_hash`, preserving surrogate `id`s (ADR-0005).
- **TOCTOU** — time-of-check-to-time-of-use; the race between reading a file and writing
  it. Guarded in `atomic_write` by re-hashing immediately before rename.
- **Surrogate identity** — a `tasks.id` that is an arbitrary autoincrement integer, not
  derived from the task's content or location, so it survives edits.
- **`pending_actions`** — the SQLite table that is the TUI→daemon command channel.
- **WAL** — SQLite's Write-Ahead Logging mode; enables one writer + many readers across
  processes (daemon writes, TUI reads).
- **Combined mode** — running the daemon (background thread) + TUI (main thread) in one
  `taski` process, the daemon's lifetime scoped to the TUI session (ADR-0007).
- **Attach-or-spawn** — `taski`'s lock-probe on startup: spawn the daemon if the
  single-writer lock is free, else attach (run the TUI-only against the running daemon).
  `taski daemon` refuses instead of attaching (ADR-0008).
- **Single-writer lock** — `flock(LOCK_EX | LOCK_NB)` on `<db_dir>/daemon.lock`, held for
  the daemon's lifetime and auto-released by the OS on crash; guarantees one daemon writes
  the vault/index (ADR-0008).
- **ShutdownSignal** — a shared `Arc<AtomicBool>` (`ShutdownSignal` to set, `ShutdownHandle`
  to check) used to cooperatively stop the daemon; in combined mode the TUI's quit hook
  sets it.
