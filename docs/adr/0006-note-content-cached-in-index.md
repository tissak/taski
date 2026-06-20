# ADR-0006: Note content cached in the index for TUI context

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** How the read-only TUI obtains note content for the task-context pane (feature `task-context-pane`), and the schema that supports it.

## Context

Taski's central invariant (PRD §8; ADR-0002) is that **the TUI never touches the vault** —
it reads only from the SQLite index, and every vault mutation flows through the daemon. So
far the index has carried each task's *text* and metadata, but never the *surrounding note
content*. The task-context pane feature needs to render the note content around a selected
task "in situ" so the user can recall a terse task's full intent without context-switching
to Obsidian.

That raises one question: how does the (read-only, vault-agnostic) TUI get the note
content? Two approaches were considered:

1. **The TUI reads the vault file directly** — load `vault` from the config, open
   `<vault>/<note_path>`, render a window around the task's `line_number`.
2. **The daemon caches the note content in the SQLite index** — the TUI reads it like any
   other index data.

The first is faster to prototype but reintroduces a direct vault dependency in the TUI
(which today has none), requires the TUI to load the `vault` path from config (it does not
today), and — critically — lets the displayed content drift from the index: the on-disk file
can change in the ~1s between a scan and the TUI's poll, so the highlight could land on the
wrong line relative to the *task rows* the TUI is showing.

## Decision

**The daemon caches each indexed note's full content in the SQLite index (new
`note_contents` table). The TUI reads it as it reads everything else — from the index. The
TUI still never opens a vault file.**

1. **New table `note_contents`** (schema v3), one row per indexed note, keyed by
   `note_path`:
   `note_contents(note_path TEXT PRIMARY KEY, content TEXT NOT NULL, note_hash TEXT,
   updated_at INTEGER NOT NULL)`. `content` is the note's full UTF-8 text; `note_hash` is
   the same content hash stored on the note's task rows; `updated_at` is informational.

2. **The daemon writes `note_contents` in the same `index_note` pass** that parses tasks and
   runs `reconcile_note`. Because the daemon already reads every note end-to-end to parse
   it, caching the content is a trivial, I/O-free addition.

3. **On note removal, the daemon deletes the `note_contents` row** alongside
   `delete_tasks_for_note`, so the index never carries content for a deleted note.

4. **This does not relax ADR-0002.** The TUI gains *read* access to note content, still
   exclusively through the index. It still never writes, and still never opens a vault
   file. The sole-writer / write-back safety contract (ADRs 0002–0004) is completely
   unchanged — this feature is a purely additive read path.

5. **Content is stored per note, not per task.** One row per note is deduped (a note with
   many tasks contributes one content row) and lets the TUI choose any context window. The
   window is a *rendering* concern and lives entirely in the TUI.

## Rationale

- **Preserves the architecture's core invariant.** "SQLite is the decoupling boundary" is
  the load-bearing idea of the whole system. Routing note content through the index keeps
  that boundary intact; reading the vault from the TUI would punch a hole in it for a
  one-time convenience.
- **Solves content/index drift.** Because content, `line_number`, and `note_hash` are all
  captured in the same scan of the same bytes, any single poll the TUI performs sees a
  snapshot where the content window and the task's location agree. (See Consequences for
  the precise, sub-poll caveat.)
- **Free at the source.** The daemon already reads and hashes every note; storing the text
  it already has in memory costs one INSERT per note per scan, no extra I/O.
- **TUI stays simple.** No config loading, no path resolution, no file reads, no "what if
  the file moved" handling in the TUI — it issues one index query, like every other read.

## Consequences

- ✅ The TUI can render a task's surrounding note context without any new vault access,
  keeping the read path uniform and the safety contract untouched.
- ✅ A schema bump to **v3** is required; per the pre-MVP policy, `ensure_schema`
  drop-and-recreates on a version change (no data to preserve). Existing dev DBs are wiped,
  which is the documented pre-MVP behavior.
- ⚠️ **`taski.db` grows with the vault** (one content row per indexed note). For a personal
  single-user vault this is a few MB at most and acceptable. A size cap / pruning of
  taskless notes can be added later if measured necessary; deliberately deferred now.
- ⚠️ **Consistency is eventual within one poll, not instantaneous.** `reconcile_note` and
  the content upsert are separate transactions, and the TUI's reads of `tasks` and
  `note_contents` are themselves non-atomic. So for a sub-poll window (~µs at write time,
  bounded by the 750ms poll at read time) the TUI could show a content snapshot that
  slightly disagrees with a task's `line_number` — e.g. a highlight one line off after an
  edit that shifted lines. This is **purely cosmetic and read-only**: it self-corrects
  within one poll, can never corrupt the vault, and the common case (a checkbox flip, which
  does not shift lines) is not affected. Making the write truly atomic would not eliminate
  the non-atomic read, so the extra coupling is not worth it.
- ⚠️ The nearest-heading/Markdown-rendering niceties are **not** provided by this ADR — the
  cache stores raw text; richer rendering is a future TUI-only enhancement.

## Alternatives considered

- **TUI reads vault files directly (`<vault>/<note_path>`).** Fastest prototype; no schema
  change. Rejected because it (a) reintroduces a vault dependency in the TUI, (b) forces the
  TUI to load `vault` from config, and (c) lets displayed content drift from the indexed
  task rows (file changed since last scan) — exactly the inconsistency this ADR avoids.

- **Store a pre-computed context window per task.** Smaller rows; window pre-decided.
  Rejected: awkward (one window per task, recomputed every scan), inflexible (window size
  fixed at write time), and more write churn. Per-note content is simpler, deduped, and
  leaves window sizing to the renderer.

- **Store only the note title + nearest heading (no body).** Cheapest; smallest storage.
  Rejected: the body *above/around* the task is usually the valuable context for recalling
  a terse task's intent; a heading alone does not reliably solve the problem.

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; this ADR
  adds a read path and does **not** relax it.
- [ADR-0003](./0003-checkbox-only-mvp.md) — write-back is checkbox flips only; the context
  pane is read-only and changes none of this.
- [ADR-0005](./0005-surrogate-identity.md) — `note_path`/`line_number` are write-time
  location claims; the cached content is captured in the same scan that sets them.
- [`docs/features/task-context-pane.md`](../features/task-context-pane.md) — the feature
  plan this ADR underpins.
- [`docs/tech.md`](../tech.md), [`docs/context.md`](../context.md) — updated for schema v3.
