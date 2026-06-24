# ADR-0014: Quick-add — bounded append-only task creation to a designated inbox

- **Status:** Accepted (the "reorder … remain rejected" clause in this ADR's gate boundary was **revoked 2026-06-24 by [ADR-0020](./0020-task-reordering.md)**, which opens a separate bounded structural-reordering gate; this ADR's append-only creation gate is otherwise unchanged)
- **Date:** 2026-06-21
- **Decides:** How Taski enables the user to create a new task from the TUI without leaving it — a single-line text-entry modal (`a` key) that appends `- [ ] <text> ➕ <today>` to a configurable inbox note (default `task-inbox.md`, created if missing). **Amends [ADR-0003](./0003-checkbox-only-mvp.md)** for a fourth time and opens a **new gate class** — bounded append-only creation — distinct from the grammar-provability token gate of ADRs 0009/0012/0013.

## Context

All prior write-back features (ADRs 0003/0009/0011/0012/0013) operate on **existing** task lines — flipping checkboxes, stamping date emojis, toggling bullet format. The user could act on tasks already in the vault but could not **capture** a new task without switching to Obsidian. This ADR introduces quick-add: the first content-creation feature.

### Why this needs a new gate, not the grammar-provability gate

ADR-0009 established the grammar-provability gate: Taski may write tokens that (i) are standard Obsidian Tasks syntax, (ii) have a single insertion grammar, and (iii) are produced by a pure proptested line-rewrite. `⏳`/`✅`/`❌` were admitted under it. But quick-add writes **arbitrary user text** (`- [ ] <user-typed-text> ➕ <today>`), and free text fails gate (ii) — there is no single insertion grammar for arbitrary content. So the grammar-provability gate does not apply.

Instead, this ADR opens a **separate, narrower gate** — bounded append-only creation — with its own principled boundary:

> Taski may append a new well-formed checkbox-task line to a **designated inbox note**, provided (i) the operation is **append-only** (never modifies, reorders, or deletes existing lines), (ii) the appended line is canonical Tasks-plugin checkbox syntax with a `➕ <today>` created-date stamp, and (iii) the write uses the same `atomic_write` TOCTOU discipline (or a first-creation path with no conflict surface). Arbitrary-note creation, mid-note insertion, text editing, and line deletion remain explicitly rejected.

The key structural distinction from hard-delete (considered in depth and rejected in ADR-0013's Alternatives): append adds to the end of a file and **shifts no existing lines** — there is no positional chaos, no line-number reconciliation, no `expected_note_hash` column needed. Delete removes a line and shifts every subsequent line's number, requiring positional re-insertion oracles and a schema bump. Append is structurally simpler and strictly safer.

### Why the `➕ <today>` stamp

Same argument as ADR-0012's `✅` and ADR-0013's `❌`, applied to creation:

- (i) `➕ YYYY-MM-DD` is canonical Tasks-plugin created-date syntax, human-readable and consumed by Tasks/Dataview for sort/filter.
- (ii) Composing the stamp into the appended line (not a separate write) keeps it one write, one hash, one rename.
- (iii) The created-date has triage value: the user sees when each inbox task was captured.

`➕` was previously a **read-only** token (Tier 1 parsing, schema v6: `extract_created_date`). This ADR makes it the fourth written token (`⏳`/`✅`/`❌`/`➕`), but the first written in a **creation** context rather than an edit of an existing line.

## Decision

### The action model

A new `quick_add` action_type in `pending_actions`, enqueued by the TUI when the user commits the modal. **No schema change (v6 unchanged)** — the existing NOT NULL columns carry well-documented sentinel values for unused fields, consistent with the established pattern (`expected_char`/`new_char` are already "unused for non-checkbox action types"):

| Column | Value for `quick_add` | Rationale |
|---|---|---|
| `task_id` | `0` | No existing task (sentinel; no FK constraint in schema). |
| `note_path` | `<inbox_path>` | The note being appended to (vault-relative). |
| `line_number` | `0` | No existing line — append, not edit. |
| `expected_char` | `''` | Unused (non-checkbox action). |
| `new_char` | `''` | Unused. |
| `action_type` | `'quick_add'` | Dispatch key. |
| `payload` | `<task text>` | The user-typed text. |

### The pure oracle

A construction oracle (simpler than the rewrite oracles — it builds a line, not edits one):

```rust
const CREATED_EMOJI: char = '➕';

pub fn inbox_line_for(text: &str, today: &str) -> String {
    format!("- [ ] {text} {CREATED_EMOJI} {today}")
}
```

No `RewriteResult` — there is no existing line to fail on. The oracle is pure and proptested: never produces malformed syntax, handles empty text, strips embedded newlines (single-line only), and preserves text containing emoji dates verbatim (the user can fix in Obsidian).

### The daemon write path

`process_quick_add(action)`:

1. Resolve the inbox: `vault_root.join(&action.note_path)` (the `note_path` column carries the inbox path).
2. Construct the line: `taski_core::inbox_line_for(payload, ymd_from_unix(unix_now()))`.
3. **If the inbox exists:** read it, place the line, `atomic_write` with the pre-append hash as `expected_hash` (standard TOCTOU — ADR-0004 reused unchanged). Placement is **append-at-EOF** unless the inbox carries a `## task-notes` section (ADR-0019), in which case the line is inserted on the last line of the task list — immediately **above** that section's blank-line separator — so a new task joins the task list and never lands under a note (see ["Interaction with ADR-0019"](#interaction-with-adr-0019-task-notes) below). Prepend `\n` if the insertion point lacks a trailing newline.
4. **If the inbox does NOT exist:** write it directly (temp → fsync → rename) with the single line as content. No TOCTOU re-hash — a non-existent file has no state to conflict with. This is a deliberate, bounded exception to ADR-0004, justified below.
5. On success: re-index the inbox note. The scanner parses the new line and `reconcile_note` inserts the task row. The task appears in the TUI on next poll (~750 ms).

### The first-creation path (bounded ADR-0004 exception)

`atomic_write` returns `WriteResult::Conflict` on `ErrorKind::NotFound` (the re-read at the TOCTOU guard treats a missing file as a conflict). Quick-add must handle first-creation explicitly: if the file does not exist, create it without the TOCTOU re-hash. This is safe because:

- A non-existent file has no prior state — there is nothing to conflict with.
- The temp → fsync → rename sequence is still atomic (the rename is atomic on POSIX).
- The created file is immediately eligible for normal `atomic_write` on subsequent quick-adds.

The exception is bounded to the first-creation path only; every subsequent append uses the full TOCTOU guard unchanged.

### Interaction with ADR-0019 (task notes)

ADR-0019 lets the user attach closing notes to a task; those notes accumulate under a single `## task-notes` section appended at the **end** of the note. When the inbox is also a task's note-bearing file, that section sits below the task list. A naïve append-at-EOF would then place each new quick-add task *under* the notes section — visually grouped with a note rather than in the task list — which is wrong: the `## task-notes` section is annotation, not task content.

So quick-add's placement is **task-list-aware**: if the inbox contains a `## task-notes` section, the new line is inserted at the end of the task list — just above the blank line that precedes the section heading — instead of at EOF. The notes section and its blank-line separator are preserved byte-for-byte. When there is no such section (the common case), placement is plain append-at-EOF as before.

This stays inside the bounded append-only creation gate: the write is still append-only with respect to *existing* lines (no existing line is modified, reordered, or deleted — the new line is inserted at a line boundary), still one `atomic_write` under the ADR-0004 TOCTOU guard, and the only structural awareness added is locating the `## task-notes` heading to choose the insertion point. The decision is owned by the daemon at write time, read from the file (single writer, re-read per action), consistent with ADR-0019's "the file is the single source of truth" principle. Placement is a pure offset computation over the read content; no new oracle and no schema change.

### Undo

Undo of quick-add (`u` after `a`) removes the appended line. This is the first **content-removing** undo (all prior undos flip a checkbox or bullet back). It is safe because:

- The appended line is **positionally known** — the last line of the inbox, or, when a `## task-notes` section exists, the last line of the task list (just above the section). Undo recomputes the same placement offset and removes exactly that line, leaving the notes section intact (symmetric with the task-list-aware add path above).
- The appended line is **content-known** (Taski wrote it; the daemon can verify it matches `inbox_line_for(text, today)` before removing).
- If the inbox was edited externally between append and undo, the TOCTOU hash check catches the mismatch and refuses.

The TUI records `LastAction::QuickAdd { inbox_path, text }`. On undo, it enqueues a `quick_add_undo` action (separate action_type, dispatched separately in the drain loop — keeps the existing `undo` handler clean). The daemon reads the inbox, verifies the last line matches the expected content, removes it via `atomic_write`, and re-indexes. The restart limitation is unchanged: if `last_action` is cleared on restart, the appended line persists and the user removes it manually in Obsidian (same documented edge as checkbox undo).

### The TUI gesture

New `a` key opens a single-line text-entry modal (`quick_adding` state, mirroring `searching`/`file_searching`):

- `a` → enter modal (footer shows `> ` prompt with blinking cursor).
- Characters accumulate in `quick_add_query` (no `rebuild()` call — no filter to recompute, unlike search).
- `Enter` → `submit_quick_add(conn)`: enqueues the `quick_add` action, records `LastAction::QuickAdd`, exits modal.
- `Esc` → cancel (clears query, exits modal).
- `Backspace` → pop last char.
- All normal-mode keys (`d`, `b`, `Space`, `u`, etc.) suppressed during modal (same `else if app.quick_adding` branch as search).

### Config plumbing

New `inbox_path: Option<String>` on `Config`. `resolve_inbox_path(&cfg) -> String` mirrors `resolve_db`: config value → default `"task-inbox.md"`. The TUI's `run_inner` (which already loads `cfg` at line 105) resolves it and threads it through `run_loop` → `App::new`. No launcher changes (~4 lines). `template()` gains a commented `# inbox_path = "task-inbox.md"` line.

## Implementation Notes

1. **`taski-core/src/lib.rs`** — add `CREATED_EMOJI` const (`'➕'`, alongside `CANCELLED_EMOJI`); add `inbox_line_for(text, today) -> String` pure oracle; optionally update `extract_created_date` to reference the new const. Add a proptest for construction correctness and edge cases (empty text, embedded newlines stripped, text containing `✅`/`❌`/`📅` preserved verbatim).

2. **`taski-config/src/lib.rs`** — add `pub inbox_path: Option<String>` to `Config` (derive Default → `None`, serde auto-deserializes); add `resolve_inbox_path(&cfg) -> String` mirroring `resolve_db`; extend `template()` with an inbox_path line.

3. **`taski-db/src/lib.rs`** — add `enqueue_quick_add(note_path, text)` (INSERT with sentinel values per the table above); add `enqueue_quick_add_undo(note_path, text)`; extend `PendingAction` struct + `pending_actions()` SELECT to surface the new action types.

4. **`taski-daemon/src/lib.rs`** — add `process_quick_add` (existence branch → append via `atomic_write`, or first-creation via temp→fsync→rename); add `process_quick_add_undo` (verify-last-line-matches → remove via `atomic_write` → re-index); wire both into the drain loop's `action_type` dispatch. Add the first-creation helper.

5. **`taski-tui/src/lib.rs`** — add `quick_adding: bool`, `quick_add_query: String`, `inbox_path: String` to `App`; add `start_quick_add`/`push_quick_add_char`/`pop_quick_add_char`/`finish_quick_add`/`clear_quick_add` (mirroring search, minus `rebuild()`); add `submit_quick_add(conn)` (enqueue + record `LastAction::QuickAdd`); extend `run_loop` with `else if app.quick_adding` branch + `KeyCode::Char('a')` in normal mode; extend footer render with the modal prompt; add `LastAction::QuickAdd` variant + `submit_undo` arm; thread `inbox_path` through `run_inner` → `run_loop` → `App::new`.

6. **Proptest** — `crates/taski-daemon/tests/quick_add_writeback_proptest.rs`: 256 cases covering append to existing inbox (various trailing-newline states, concurrent edits) and first-creation (non-existent inbox). Assert: either the line lands correctly with existing content preserved, or a concurrent edit is refused — never corruption. Mirror the structure of `cancelled_date_writeback_proptest.rs`.

## Rationale

- **It's the first creation feature, and append is the safest possible creation.** Appending to the end of a designated file shifts no existing lines, requires no positional reconciliation, and has an unambiguous target position. General creation (mid-note insertion) and deletion are structurally riskier and remain rejected.
- **The inbox is a capture surface, not a curated note.** The user's stated workflow: quick-add captures tasks into an inbox; the user reviews and moves them later in Obsidian. This matches GTD-style inbox processing and keeps Taski's write surface narrow.
- **No schema bump.** The existing `pending_actions` columns carry sentinel values for unused fields, consistent with the established "unused for non-checkbox action types" pattern. A schema v7 redesign (nullable columns or a separate `pending_creates` table) was considered and deferred — the sentinel approach is adequate for a personal tool and avoids a destructive migration.
- **`➕ <today>` is composed into the appended line, not a separate write.** Same argument as ADR-0012/0013: one write, one hash, one rename.
- **First-creation without TOCTOU is safe.** A non-existent file has no state to conflict with; the temp → fsync → rename is still atomic. The exception is bounded to first-creation only; subsequent appends use the full TOCTOU guard.
- **Undo is safe because the line is known.** The appended line's position (last) and content (Taski wrote it) are both known; the daemon verifies before removing.

### Why this does not violate ADR-0005

`➕ YYYY-MM-DD` is native Obsidian Tasks created-date syntax (human-readable, consumed by Tasks/Dataview), not the foreign opaque identity marker ADR-0005 rejected. The user-typed text is the user's own content, not an injected marker. The surrogate-id + content-hash reconciliation mechanism is untouched — when the inbox is re-indexed after append, `reconcile_note` inserts the new task with a fresh surrogate `id`. **ADR-0005 is not amended.**

## Consequences

- ✅ The user can capture tasks from the TUI without switching to Obsidian.
- ✅ `atomic_write` and ADR-0004 are reused for existing-file appends; the first-creation path is a bounded, justified exception.
- ✅ ADR-0005 is not crossed (`➕` is native syntax, text is user content).
- ✅ No schema bump (sentinel values in existing columns).
- ✅ Undo works (`u` removes the appended line; first content-removing undo, safe because the line is positionally and contentually known).
- ⚠️ **ADR-0003 is amended a fourth time**: write-back scope widens to include bounded append-only creation to a designated inbox. The grammar-provability gate (ADRs 0009/0012/0013) is **unchanged** — this amendment opens a **new gate class** (bounded append-only creation), not an extension of the token gate.
- ⚠️ `pending_actions` carries sentinel values (`0`, `''`, `0`) for `quick_add` rows — documented but semantically weak. If a future action type also lacks these fields, a schema v7 redesign should be considered.
- ⚠️ The `a` key is consumed for quick-add. The `u` undo scope widens (now covers quick-add removal in addition to checkbox flips, cancels, and bullet toggles).
- ⚠️ User text containing emoji dates (`✅`, `❌`, `📅`, `⏳`) is preserved verbatim — the scanner will parse them as metadata. This is a known edge case; the user can fix in Obsidian.
- ⚠️ **Placement became task-list-aware** (post-ADR-0019): when the inbox carries a `## task-notes` section, quick-add inserts above it rather than at EOF, and undo recomputes the same offset. This stays inside the append-only creation gate (no existing line modified/reordered/deleted; one `atomic_write`), but the position is no longer unconditionally "last line of file." See ["Interaction with ADR-0019"](#interaction-with-adr-0019-task-notes).

### Cross-reference note — ADR-0003 (fourth amendment)

ADR-0003's amendment block must record a **fourth** amendment. Unlike the prior three (which widened the token set under the grammar-provability gate), this amendment opens a **new gate class**:

> The write-back scope is widened from **checkbox-state flips + `⏳`/`✅`/`❌` date-emoji stamps** to also include **bounded append-only task creation** to a designated inbox note (`quick_add` action). The ADR-0009 grammar-provability gate is **unchanged** and still governs token writes. A new, separate gate governs creation: Taski may append a well-formed checkbox-task line (with `➕ <today>` created-date stamp) to a designated inbox note, provided the operation is append-only (no modification / reorder / deletion of existing lines), uses the standard `atomic_write` TOCTOU discipline (or a first-creation path with no conflict surface), and re-indexes after write. Arbitrary-note creation, mid-note insertion, text editing, and line deletion remain explicitly rejected.

## Alternatives considered

- **General creation (arbitrary note, arbitrary position).** Rejected: mid-note insertion shifts subsequent line numbers, requiring positional reconciliation and a far more complex oracle. Append-only to a designated inbox is the narrowest opening that delivers value.
- **Bare `- [ ] text` (no `➕` stamp).** Rejected at user direction: the `➕` stamp provides created-date triage value and Tasks-plugin interop, consistent with the `✅`/`❌` convention established by ADRs 0012/0013.
- **Schema v7 (nullable `task_id` or a separate `pending_creates` table).** Considered and deferred: the sentinel-value approach is adequate for a personal tool, consistent with the existing "unused for non-checkbox action types" pattern, and avoids a destructive migration. If a second creation-class action type is added, revisit.
- **Optimistic TUI display (show the task before the daemon confirms).** Rejected (consistent with existing policy — see context.md "Deferred"): the TUI waits for daemon confirmation; simpler and never lies.
- **Multi-line entry.** Rejected: single-line modal is simpler and sufficient for inbox capture. Multi-line would complicate the oracle and the undo semantics.
- **Recurring-task stamp (`🔁`) on creation.** Rejected: out of scope; `🔁` is not yet a written token and would need its own ADR under the grammar-provability gate.

## Edge cases

| Case | Behavior |
|---|---|
| Inbox exists, ends with `\n` | Append `- [ ] text ➕ today\n` directly. |
| Inbox exists, no trailing `\n` | Append `\n- [ ] text ➕ today\n` (prepend newline). |
| Inbox has a `## task-notes` section (ADR-0019) | Insert the line at the end of the task list, above the section's blank-line separator — not at EOF. The section and separator are preserved byte-for-byte. Undo recomputes the same offset and removes that line. |
| Inbox does not exist | Create with `- [ ] text ➕ today\n` as sole content (first-creation path, no TOCTOU). |
| Inbox exists, concurrent Obsidian edit | Hash mismatch → `ConflictNoteChanged` → refuse (ADR-0004, unchanged for existing-file path). |
| Empty text (Enter on empty query) | Refuse at TUI layer (do not enqueue); modal exits with no action. |
| Text contains embedded newline | Strip at TUI or oracle layer (single-line only). |
| Text contains `✅`/`❌`/`📅`/`⏳` | Preserved verbatim; scanner parses as metadata. Known edge case; user can fix in Obsidian. |
| Undo after quick-add, inbox unchanged | Verify last line matches expected content; remove via `atomic_write`; re-index. |
| Undo after quick-add, inbox edited externally | Hash mismatch → refuse; user removes the line manually in Obsidian. |
| Undo after quick-add, then `q` (restart) | `last_action` cleared on restart; line persists. User removes manually. (Same limitation as checkbox undo — documented, not persisted.) |
| CRLF-terminated inbox | Appended line uses LF (`\n`), not CRLF — mixed endings. Parser handles it (`str::lines()` strips `\r`); Obsidian normalizes on open. Not corruption. Detecting and matching the file's dominant line ending is deferred. |
| Undo after first-creation quick-add | Line removed; inbox file remains (empty, not deleted). Harmless — next quick-add reuses it. Deleting the file would add complexity for no safety gain. |

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; `quick_add` routes through `pending_actions` like all writes.
- [ADR-0003](./0003-checkbox-only-mvp.md) — **amended a fourth time** by this ADR (new gate class: bounded append-only creation).
- [ADR-0004](./0004-refuse-on-conflict.md) — refuse-on-conflict / TOCTOU; **reused for existing-file appends**; bounded exception for first-creation.
- [ADR-0005](./0005-surrogate-identity.md) — **not amended**; `➕` is native syntax, text is user content.
- [ADR-0009](./0009-scheduled-date-today.md) — the grammar-provability gate; **unchanged** by this ADR (which opens a separate gate).
- [ADR-0011](./0011-bullet-toggle-undo.md) — the undo model this ADR extends with `LastAction::QuickAdd`.
- [ADR-0012](./0012-done-date-on-toggle.md) / [ADR-0013](./0013-cancelled-date-on-cancel.md) — the compose-stamp-into-write pattern; `➕` extends it to creation.
- [ADR-0019](./0019-task-notes-annotation.md) — introduces the `## task-notes` section; quick-add placement (and undo) became task-list-aware to keep new tasks above it. See ["Interaction with ADR-0019"](#interaction-with-adr-0019-task-notes).
