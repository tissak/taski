# Taski — Engineering Context & Onboarding

*Onboarding guide for new engineers. Last updated: 2026-06-20 (v0.1).*

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

Two Rust binaries + a shared SQLite file:

```
 Obsidian vault ──watch──▶ taski-daemon ──write──▶ SQLite (taski.db) ◀──read─── taski-tui
   (source of       (sole writer to vault            tasks + pending_actions        (polls)
    truth)           + to the index)
                         ▲                                  │ inserts action rows
                         └──────────────────────────────────┘
```

The whole point of the architecture: **SQLite is the decoupling boundary.** The daemon
writes; the TUI reads; write-back commands flow back through a `pending_actions` table
that only the daemon executes. Neither binary talks to the other directly.

---

## Repository Layout

Cargo workspace, edition 2024, five crates. Dependencies point downward only (no cycles):

| Crate | Responsibility | Key file(s) |
|---|---|---|
| `taski-core` | **Pure** domain: `Task`/`Status` types, the Markdown parser (`parse_tasks`, fence-aware), due-date extraction (`extract_due_date`). No FS, no I/O, no deps on other taski crates. | `crates/taski-core/src/lib.rs` |
| `taski-config` | TOML config loading (`~/.config/taski/config.toml`) + CLI→config→default precedence + the `template()` renderer for `--init-config`. Keeps FS/TOML out of `taski-core`. | `crates/taski-config/src/lib.rs` |
| `taski-db` | The canonical SQLite schema, `open()` (WAL + schema + dir creation), and all read/write APIs (`all_tasks`, `reconcile_note`, `enqueue_action`, `pending_actions`, `prune_old_actions`, …). Owns `tasks` + `pending_actions`. | `crates/taski-db/src/lib.rs` |
| `taski-daemon` | The watcher/scanner + **sole writer to the vault**: `run()`, `scan_vault`, `index_note`, `process_action`, `atomic_write` (TOCTOU-hardened), watch loop. Two binaries: `taski-daemon` (service) + tests. | `crates/taski-daemon/src/{lib,main}.rs`, `tests/` |
| `taski-tui` | The `ratatui` client: polls the index, groups by note, filters, renders, submits toggle actions. Never touches vault files. | `crates/taski-tui/src/main.rs` |

Supporting: `docs/` (PRD, tech, ADRs, setup, this file), `scripts/install-launchd.sh`
+ `uninstall-launchd.sh`, `.github/workflows/ci.yml`, `rust-toolchain.toml`.

---

## Build, Run, Test

```sh
cargo build --workspace                       # dev build
cargo build --release --workspace             # optimized daily-driver binaries
cargo test --workspace                        # all tests (92 as of v0.1)
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
./target/release/taski-daemon --init-config --vault /path/to/your/vault   # one-time config
./target/release/taski-daemon --once --vault /path/to/vault               # single scan + exit (no watcher)
./target/release/taski-tui                                                # the UI
# or autostart the daemon at login:
scripts/install-launchd.sh
```

Config precedence is **CLI flag → config file → compiled default**. `vault` has no
default (daemon requires it); `db` defaults to `./taski.db`. Config location is
`~/.config/taski/config.toml`, overridable with the `TASKI_CONFIG` env var.

### Debugging

- **Logs:** the daemon logs to **stderr** via `tracing` at `info` by default. Set
  `RUST_LOG=debug` (or `taski_daemon=trace`) to see reconciliation summaries, action
  outcomes, and conflict reasons. Under launchd, logs stream to
  `~/.local/share/taski/daemon.log`.
- **Inspect the index/queue directly:**
  `sqlite3 ~/.local/share/taski/taski.db "SELECT id,note_path,state,error FROM pending_actions ORDER BY id DESC LIMIT 10"`
  — the fastest way to answer "why didn't my toggle land?" (`state` is `pending`/`done`/`failed`).
- **Shutdown:** the daemon installs a ctrlc handler — the **first** Ctrl-C initiates a
  clean shutdown (up to ~500ms, the event-loop tick); a **second** Ctrl-C force-terminates.
  A brief pause after the first is normal, not a hang.
- **Latency expectations:** FS events are debounced **300ms**; the daemon event loop ticks
  every **500ms**; the TUI re-reads the index every **750ms**. So a toggle or an Obsidian
  edit typically reflects in 1–2s.

---

## The Mental Model: Two Data Flows

Understanding these two flows is 90% of understanding the codebase.

### 1. Indexing (vault → index), daemon-owned

```
FS event (debounced 300ms) ─▶ scan_vault / index_note(note)
                              └─▶ taski_core::parse_tasks(note_text)   // fence-aware, extracts due dates
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

The TUI **never** opens a vault file. It only inserts `pending_actions` rows. Only the
daemon mutates notes, and only after byte-re-verification. This is the core safety
guarantee (ADRs 0002/0003/0004).

---

## Data Model (schema v3)

Defined in `taski-db::SCHEMA`. `PRAGMA user_version` tracks the version; older DBs are
dropped and recreated (pre-MVP, no data to preserve). v3 added the `note_contents` cache
that backs the read-only TUI context pane ([ADR-0006](./adr/0006-note-content-cached-in-index.md)).

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
| `updated_at` | Last-seen timestamp. |

**`pending_actions`** — the TUI→daemon command queue. Lifecycle `pending → done | failed`.
Holds `task_id`, the `expected_char`/`new_char` for the flip, and on failure an `error`.
Resolved rows older than 7 days are pruned on daemon startup (`ACTION_RETENTION_SECS`).

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

3. **Checkbox-state flips only** ([ADR-0003](./adr/0003-checkbox-only-mvp.md)) — MVP
   write-back flips `[ ]↔[x]`, nothing more. Text/metadata edits are explicitly deferred.
   Adding "edit task text from the TUI" is a *big* change, not a small one.

4. **Refuse-on-conflict, never last-write-wins** ([ADR-0004](./adr/0004-refuse-on-conflict.md))
   — before renaming, re-read the note and re-hash; if it changed since scan, *refuse*
   (mark the action `failed`), do not overwrite. The addendum hardens the temp→rename
   step against TOCTOU. If you ever feel tempted to "just write it," don't.

5. **Surrogate rowid identity + content-hash reconciliation** ([ADR-0005](./adr/0005-surrogate-identity.md))
   — `id` is an autoincrement integer (stable, never reused), decoupled from location.
   `(note_path, line_number)` is a write-time location claim, re-verified against bytes.
   Crucially: **Taski injects nothing into the vault** (unlike Logseq-style inline IDs);
   identity is reconciled from content each scan. This was validated against
   Obsidian-Tasks prior art.

6. **Note content cached in the index for the TUI context pane** ([ADR-0006](./adr/0006-note-content-cached-in-index.md))
   — the daemon caches each note's full text in `note_contents`; the TUI reads it like any
   other index data. The TUI **still never opens a vault file** — this is a read path, not
   a relaxation of ADR-0002. Chosen over "TUI reads the vault directly" so content and task
   locations stay consistent (same scan) and the SQLite decoupling boundary stays intact.

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

- **The TUI does surface refused toggles** as a one-line notice (via `recent_actions` →
  `friendly_failure_reason`), cleared on the next action. But the TUI↔daemon coupling is
  loose *by choice*: `friendly_failure_reason` string-matches the daemon's `ApplyOutcome`
  phrases, with a generic fallback. A structured reason-code was considered and deferred
  (low value for a personal tool). If you change daemon error wording, sanity-check the
  TUI messages.

- **`thiserror` is a stale unused workspace dependency** (left in `Cargo.toml`
  `[workspace.dependencies]` after it was dropped from `taski-db`). Safe to remove in a
  future cleanup; don't assume it's load-bearing.

- **Tags are local-only.** `v0.1` and all commits exist only in the local repo until
  pushed. There is currently no remote set up in this working tree — confirm before
  assuming `git push` will work.

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

---

## Testing Strategy

| Location | What it guards |
|---|---|
| `taski-core` unit tests + `proptest` | Parser correctness on a synthetic corpus; never-panics on arbitrary input; due-date extraction. |
| `taski-config` unit tests | TOML parsing, precedence (CLI→config→default), env override, `template()` round-trips. |
| `taski-db` unit tests | Schema, `reconcile_note` identity retention, upsert/read round-trips, action pruning, `open()` creates missing dirs. |
| `taski-daemon/tests/scan.rs` | End-to-end scan of a fake vault → correct task rows. |
| `taski-daemon/tests/reconcile.rs` | Content-hash reconciliation: identity survives edits, deletes, reorders. |
| `taski-daemon/tests/writeback.rs` + `writeback_proptest.rs` | The safety contract: atomic_write commits on match, refuses on conflict, never corrupts. |
| `taski-tui` unit tests (in `main.rs`) | View model: grouping, collapse, filter, display-index↔Task mapping, selection reconciliation, failure-notice surfacing. |

Tests use `tempfile` fake vaults and `:memory:` or temp-file DBs. The real vault is
exercised only at runtime (its `taski.db` is gitignored).

---

## Quick Reference — "I want to…"

| Task | Look at |
|---|---|
| Change how tasks are parsed / add metadata extraction | `taski-core/src/lib.rs` (`parse_tasks`, `extract_due_date`) |
| Change the DB schema | `taski-db::SCHEMA` + bump `SCHEMA_VERSION`; update `reconcile_note`/`upsert_task` |
| Cache/read note content for the TUI context pane | `taski-db`: `note_contents` table + `upsert_note_content`/`note_content`/`delete_note_content`; daemon writes it in `index_note` ([ADR-0006](./adr/0006-note-content-cached-in-index.md)) |
| Change write-back behavior | `taski-daemon`: `process_action`, `atomic_write` (mind ADR-0004 TOCTOU) |
| Change how the TUI looks/behaves | `taski-tui/src/main.rs`: `App`, `build_view`, key handling |
| Add a CLI flag | `Cli` struct in the relevant binary's `lib.rs`/`main.rs` |
| Change config format/precedence | `taski-config/src/lib.rs` |
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
- **Distribution / packaging / GUI / multi-vault / collaboration** — out of MVP scope (PRD §14).

If you pick one up, record the decision and update this list.

---

## Glossary

- **Write-back** — reflecting a TUI toggle into the originating Markdown note, via the
  daemon. MVP = checkbox flips only.
- **Reconciliation** — re-matching a note's freshly-parsed tasks to existing index rows by
  `text_hash`, preserving surrogate `id`s (ADR-0005).
- **TOCTOU** — time-of-check-to-time-of-use; the race between reading a file and writing
  it. Guarded in `atomic_write` by re-hashing immediately before rename.
- **Surrogate identity** — a `tasks.id` that is an arbitrary autoincrement integer, not
  derived from the task's content or location, so it survives edits.
- **`pending_actions`** — the SQLite table that is the TUI→daemon command channel.
- **WAL** — SQLite's Write-Ahead Logging mode; enables one writer + many readers across
  processes (daemon writes, TUI reads).
