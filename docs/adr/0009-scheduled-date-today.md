# ADR-0009: Scheduled-date metadata + "mark for today"

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** How Taski surfaces and sets the Obsidian Tasks-plugin "scheduled" date (`⏳`), and the "mark a task for today" triage gesture. **Amends [ADR-0003](./0003-checkbox-only-mvp.md)** (write-back scope) for the Phase 2 write path.

## Context

Taski already reads the Obsidian Tasks-plugin **due** date (`📅`/`📆`/`🗓`) into `due_date`,
but cannot answer "what am I doing today?" and has no way to *triage* a task into today.

The user wants two distinct capabilities:

1. **A view** — "show me the list of tasks I want to address today."
2. **A mark gesture** — "flick through tasks, promote some to today."

### Why the mark gesture requires a vault write (and a local flag cannot work)

"Today-ness" must persist across the TUI's 750ms re-reads *and* across daemon re-scans. It
can live in exactly one of two places:

- **In the vault** as task metadata. Persistent by definition; visible to Obsidian; but a
  Taski-initiated write of anything other than a checkbox flip crosses
  [ADR-0003](./0003-checkbox-only-mvp.md).
- **In the SQLite index only** (a flag column). No vault write — but the index is *rebuilt
  from vault content on every scan* via `reconcile_note`, which matches rows by `text_hash`
  ([ADR-0005](./0005-surrogate-identity.md)). Therefore an index-only flag keyed to
  `tasks.id` **orphans the moment the user edits the task text in Obsidian** (text_hash
  changes → id not preserved → delete-old + insert-new), is **lost on any schema bump**
  (`ensure_schema` drop-and-recreates), and is invisible to Obsidian / Dataview / Tasks
  queries. A local flag is a false feature that silently desyncs from reality.

**Conclusion:** the durable version of the mark gesture requires a vault write. The only
question is *what* to write.

### The idiomatic Obsidian signal is the scheduled date `⏳`

The Obsidian Tasks plugin defines three dated emojis with distinct semantics:

| Emoji | Meaning | Fit for "do today"? |
|---|---|---|
| `📅` due | **Hard deadline** | No — overloading it corrupts deadline information. |
| `⏳` scheduled | **"Plan to work on this"** | **Yes — this is the canonical triage gesture.** |
| `🛫` start | Earliest visible | No — a visibility gate, not a triage signal. |

`scheduled today` and `happens today` (earliest of start/scheduled/due = today) are
first-class Tasks-plugin queries; Dataview reads `⏳` as the `scheduled` field; community
consensus names `⏳ = today` as *the* "promote this to today" gesture. The canonical
insertion is ` ⏳ YYYY-MM-DD` appended at the **end** of the task line (the Tasks parser
scans right-to-left and stops at unrecognized text). If a `⏳` already exists, its date is
**replaced**, not duplicated.

## Decision

Adopt the Tasks-plugin **scheduled date `⏳ YYYY-MM-DD`** as Taski's "for today" signal, and
deliver it in **two phases** — the read path first (crosses no ADR), the write path second
(amends ADR-0003).

### Phase 1 — read path (no ADR crossing)

1. **Parser.** Generalize `extract_due_date` in `taski-core` into a single helper applied to
   both emoji families: `📅`/`📆`/`🗓` → `due_date` (existing), `⏳` → **new `scheduled_date`**.
   The grammar (emoji + optional VS16 + whitespace + strict `YYYY-MM-DD`) is identical; only
   the leading emoji set differs. **`taski-core` stays pure** (no FS/I/O) — this is more
   byte-scanning. Do **not** add `🛫` start dates now (YAGNI); the helper generalizes
   trivially later.

2. **Schema.** Add a `scheduled_date TEXT` column to `tasks`; bump `SCHEMA_VERSION`
   3 → 4. Per the pre-MVP policy, `ensure_schema` drop-and-recreates on a version change (no
   data to preserve). Update `reconcile_note`'s UPDATE/INSERT and `all_tasks`/`upsert_task`
   to carry the column. Reconciliation semantics are otherwise unchanged: a `⏳` write shows
   up as delete-old + insert-new (text_hash changes), which is correct and intended.

3. **Today view.** Add an independent `today_only: bool` to the TUI `App`, toggled by **`T`**.
   When set, `build_view` filters to tasks whose `scheduled_date == today`. It is
   **independent of the `f` status-cycle** (`All`/`Open`/`Done`): today-ness and open/done
   are orthogonal axes, so `today_only + Open` = "today's open work." We do **not** fold
   `Today` into the `f` cycle — that would conflate date and status semantics.

4. **Row indicator.** In `row_to_item`, render `⏳ <date>` in cyan, parallel to the existing
   yellow `· <due>`; render it **bold/bright cyan** when `scheduled_date == today` (the
   "this is a today task" affordance). No new glyph — `⏳` is self-explanatory.

5. **View scope: strict scheduled-today.** The Today view shows *only* `scheduled_date ==
   today`. It does **not** include due-today or overdue (the broader `happens today` union).
   This keeps the view symmetric with the mark gesture and avoids mixing two date semantics
   in one list. A separate "due/overdue" view can be added later if wanted.

Phase 1 delivers value on its own to anyone already using `⏳` in Obsidian, crosses no ADR,
and is a strict subset of Phase 2 (no rework).

### Phase 2 — write path (amends ADR-0003)

6. **Action queue.** Extend `pending_actions` with an `action_type TEXT NOT NULL DEFAULT
   'checkbox'` and a nullable `payload TEXT` — **do not** create a second table. The queue's
   lifecycle (pending→done/failed, pruning, `recent_actions` → TUI notice) is identical for
   both action kinds and is the bulk of the queue code; splitting would duplicate it and
   force a UNION for notice-surfacing. Existing checkbox rows backfill to
   `action_type='checkbox'`, `payload=NULL`. The proven checkbox drain path is perturbed only
   by a dispatch on `action_type` at the top of the loop; the 256-case checkbox proptest
   stays scoped to checkbox actions verbatim.

7. **New action + gesture.** `enqueue_set_scheduled(conn, task_id, note_path, line_number,
   desired: Option<&str>)`. The TUI **`t`** key enqueues it with `desired = Some(today)`.
   **`t` is a toggle**: on a task already scheduled-today, it enqueues `desired = None`
   (removes the `⏳`), mirroring how `Space` toggles done. "Today" is the wall-clock date at
   enqueue time; the persisted *value* is that date, so a task marked Monday stays `⏳
   Monday` on Tuesday (it is no longer "today" — correct; the view simply stops matching).

8. **Daemon mutation — the riskiest new code.** A new `process_metadata_action`, structurally
   parallel to `process_action` and **reusing `atomic_write` unchanged**:
   - Look up the current task row by `task_id`; re-read the note; conflict-check via
     `content_hash` vs `row.note_hash` (identical to `process_action`).
   - Resolve the task's **current** `line_number` from the row (not a stale action value —
     [ADR-0005](./0005-surrogate-identity.md)).
   - Byte-verify the line is still a checkbox line; refuse otherwise.
   - Call a **pure** line-rewrite in `taski-core`:
     `fn rewrite_scheduled(line: &str, desired: Option<&str>) -> RewriteResult`
     where `RewriteResult = Unchanged | Rewritten(String) | Unparseable`.
     `Some(date)` → replace an existing `⏳` date, else append ` ⏳ YYYY-MM-DD` at logical
     line-end (preserving the line terminator and any trailing tags). `None` → remove an
     existing `⏳` token and its preceding space. Malformed input (NBSP, stray variation
     selectors, unparseable date) → `Unparseable` → refuse.
   - Splice the rewritten line into the full note buffer (preserve every other byte and all
     line endings — the same discipline as `process_action`'s single-char swap).
   - `atomic_write(note_abs, &new_bytes, &snapshot_hash)` — **the same function**, whose
     TOCTOU guard re-reads and re-hashes the *whole file*. It is already agnostic to whether
     we changed 1 byte or N. This is the key reuse: the most-reviewed code in the project
     needs zero changes.
   - On success, re-index the note (refreshes `note_hash`, parses the new `scheduled_date`).

9. **New outcome variant.** Add `MetadataUnparseable` to `ApplyOutcome` for the "existing
    `⏳` is malformed — refuse rather than guess" case. The happy/conflict paths reuse the
    existing `Applied`/`ConflictNoteChanged`/`TaskNotFound`/`TaskLineMismatch` variants, and
    the TUI surfaces outcomes through the existing `recent_actions` → `friendly_failure_reason`
    notice path.

## Rationale

- **`⏳` is dated, semantic, and participating.** Unlike a `#today` tag (not dated —
  yesterday's `#today` is still `#today` today, and it doesn't join `scheduled`/`happens`
  queries) or overloading `📅` (wrong semantics — hard deadline), `⏳` carries a real date,
  means precisely "scheduled," and is consumed by Tasks, Dataview, and Obsidian's own UI.
- **Adopting, not inventing.** We are implementing the gesture the Obsidian ecosystem has
  already standardized on, so we inherit its semantics and queryability rather than
  defining a parallel one.
- **Phased delivery de-risks.** Phase 1 is pure read-path work identical to the existing
  `due_date` handling; it ships value with zero ADR cost and validates the parser/schema
  change before the write path is touched. Phase 2 is reviewed on the write-path merits
  alone.

### Why this does not violate ADR-0005

[ADR-0005](./0005-surrogate-identity.md) rejects specifically the **opaque identity marker**
`%% taski:abc %%` — a token that is (i) *foreign* (no Obsidian vocabulary), (ii) *opaque*
(human-unreadable), (iii) *machine-intros-only* (no tool but Taski consumes it), and (iv)
whose purpose is to give **Taski** a durable identity handle, bootstrapped by a one-way mass
vault mutation.

`⏳ YYYY-MM-DD` is categorically different on every axis: it is **native Obsidian Tasks
syntax**, **human-readable**, **consumed by Tasks/Dataview/Obsidian itself**, and carries
**independent task meaning** (not an identity handle). It is written **one line at a time on
explicit user gesture**, never as a mass bootstrap.

Crucially, **the surrogate-identity mechanism is untouched.** When `⏳` is written,
`text_hash` changes and `reconcile_note` treats it as delete-old + insert-new — exactly as
it already does for any Obsidian text edit. No opaque handle is injected; identity is still
reconciled from content. **ADR-0005's *mechanism* (surrogate id + content-hash match) is
unchanged; only the *content* gains a standard metadata token.** The write-scope question is
therefore an [ADR-0003](./0003-checkbox-only-mvp.md) concern (what writes are permitted), not
an ADR-0005 concern (how identity works). **ADR-0005 is not amended.**

## Consequences

- ✅ A durable, Obsidian-native "for today" workflow: triage with `t`, view with `T`, fully
  interoperable with Tasks/Dataview queries a user may already run.
- ✅ The read path (Phase 1) is risk-free and immediately useful to existing `⏳` users.
- ✅ `atomic_write` and ADR-0004's refuse-on-conflict contract are **reused unchanged** — the
  new write path inherits identical conflict semantics.
- ⚠️ **ADR-0003 is amended** (Phase 2): write-back scope widens from checkbox-flips-only to
  checkbox-flips **+ Obsidian-standard date-emoji metadata**. The amendment records a
  *principled boundary* so it does not become an open door (see below).
- ⚠️ **Schema bump to v4** drops+recreates existing dev DBs (pre-MVP policy; no data to
  preserve).
- ⚠️ **New risk surface: variable-length line insertion.** Today's write is a fixed
  single-codepoint swap at a parser-validated structural position; the new write appends or
  replaces a variable-length token at line-end. Contained by (a) a **pure**
  `rewrite_scheduled` in `taski-core`, exhaustively proptested in isolation, and (b) an
  **analogous proptest** to `writeback_proptest.rs`: for any generated note + any existing
  `⏳` state + any concurrent edit, `process_metadata_action` either produces a note whose
  target line equals the pure `rewrite_scheduled` output and whose line count is unchanged,
  **or** the file equals the concurrent edit byte-for-byte (refused). Never corruption,
  never a dropped/added line. This is the direct generalization of the existing
  "never-corrupts" contract.
- ⚠️ A `⏳` write changes `text_hash`, so the surrogate id churns on the post-apply re-index.
  This is fine — no pending FK depends on the old id by the time re-index runs (the action is
  resolved) — and is documented as expected behavior.

### The ADR-0003 amendment boundary (precedent control)

The single biggest long-term risk is **precedent creep**: once ADR-0003 admits date-emoji
writes, requests will follow for priority (`⏫`), recurrence (`🔁`), tags, and eventually
free-text edits. The amendment therefore records a **principled boundary**, not a blanket
"metadata is fine":

> Taski may write tokens that are (i) **standard Obsidian Tasks syntax**, (ii) have a
> **single unambiguous insertion grammar**, and (iii) are produced by a **pure, proptested
> line-rewrite** with a "never-corrupts" contract.

Free-text edits fail (ii)/(iii) and remain explicitly rejected. Creates/deletes remain
rejected. **Each new token type gets its own ADR.** This makes the precedent *gated by
grammar-provability*, not by "we already write *something*."

## Alternatives considered

- **Taski-local flag in SQLite (separate table keyed by `task_id`).** No vault write, no ADR
  cost. **Rejected:** ephemeral by design — orphans on any task-text edit (text_hash churns
  the surrogate id), lost on schema-bump DB wipe, invisible to Obsidian, and silent data loss
  the user cannot see or reason about. A "today" flag that vanishes mid-session is not a real
  feature.
- **`#today` tag write.** **Rejected:** tags are not dated (yesterday's `#today` is still
  today's `#today`, which is wrong), do not participate in the date-based query ecosystem
  (`happens`/`scheduled` ignore them), collide with whatever the user already uses `#today`
  for, and require manual cleanup. `⏳` is strictly more correct because it is *dated*.
- **Piggyback on the existing `📅` due date.** **Rejected on semantic grounds:** `📅` is a
  *hard deadline*, not a triage signal. Overloading it misrepresents user intent to every
  tool that reads due dates and clobbers any existing deadline.
- **Daily-note relocation (move the task into today's `YYYY-MM-DD.md`).** The most
  "Obsidian-native" triage pattern for some workflows, but it is a **cross-note move** —
  deleting+inserting whole lines across files, a far larger write-back surface — and is
  deferred by ADR-0005's "no mass mutation" ethos. Out of scope; may be revisited separately.
- **Fold `Today` into the `f` status-cycle.** Cheaper on keys. **Rejected:** conflates the
  date axis with the status axis; `today_only` as an independent boolean composes cleanly
  with `Open`/`Done`/`All` and is worth one extra key.
- **`happens today` view scope** (scheduled-today ∪ due-today ∪ overdue). Matches "tasks to
  address today" more literally. **Deferred:** mixes two date semantics in one list and
  breaks symmetry with the mark gesture. Ship strict scheduled-today now; a separate
  due/overdue view can be added later if wanted.

## Edge cases

| Case | Behavior |
|---|---|
| Task already has `⏳ <other-date>` | **Replace** the date with today (canonical Tasks behavior). No warning — this is the expected "re-schedule to today." |
| Task already scheduled-today, user hits `t` | **Unmark** — enqueue `desired = None`, removing the `⏳`. |
| Task has `📅 <today>` already | Still allow the `⏳` mark; the two are independent semantics. (Today view is strict-scheduled, so it won't double-list.) |
| Existing `⏳` is malformed (bad date / NBSP / stray VS) | Refuse with `MetadataUnparseable`; surface a notice. Never guess. |
| Obsidian edits the line (or any line of the note) concurrently | `note_hash` mismatch → `ConflictNoteChanged` → refuse (ADR-0004, unchanged). |
| Recurring task (`🔁`) | Taski's write is a one-shot on the current line; recurrence advances `⏳` per its own rules on completion. Non-interacting; add one fixture test. |
| Trailing tags/whitespace on the line | Append after the last non-newline content (canonical per the Tasks right-to-left parser); preserve the line terminator exactly. |

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; the new
  action routes through `pending_actions` like any other.
- [ADR-0003](./0003-checkbox-only-mvp.md) — **amended** by Phase 2 of this ADR (write-back
  scope widened to Obsidian-standard date-emoji metadata, with a principled boundary).
- [ADR-0004](./0004-refuse-on-conflict.md) — refuse-on-conflict / TOCTOU; **reused
  unchanged** (whole-file hash is byte-count-agnostic).
- [ADR-0005](./0005-surrogate-identity.md) — **not amended**; `⏳` is native Obsidian syntax,
  not an identity marker. The surrogate-id + content-hash mechanism is untouched.
- [Obsidian Tasks — Dates](https://github.com/obsidian-tasks-group/obsidian-tasks/blob/main/docs/Getting%20Started/Dates.md)
  and [Tasks Emoji Format](https://github.com/obsidian-tasks-group/obsidian-tasks/blob/main/docs/Reference/Task%20Formats/Tasks%20Emoji%20Format.md)
  — the authoritative `⏳`/`📅`/`🛫` semantics and right-to-left parsing rules.
- [`docs/tech.md`](../tech.md) — updated for `⏳` parsing and the write-back scope amendment.
