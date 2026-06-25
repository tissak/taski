# ADR-0021: Archive completed tasks — a copy-then-delete move of done/cancelled lines into a designated archive note

- **Status:** Accepted
- **Date:** 2026-06-24
- **Decides:** How Taski clears completed work out of a working note from the TUI — an `A`-key one-shot that moves **every flat `[x]` done or `[-]` cancelled task line** from the selected task's note into a designated archive note (default `task-archive.md`, created if missing), via a **durable-copy-then-delete** sequence committed as one `archive` action. v1 is **flat-only** and acts on a **single note**. **Amends [ADR-0003](./0003-checkbox-only-mvp.md)** for a seventh time and opens a **fourth gate class** — bounded archival (move-by-copy-then-delete) — which is Taski's **first deletion of an existing line** and **first cross-note operation**, and **revokes the "deletion remain rejected" clause** carried by the ADR-0014 and ADR-0020 gate boundaries and the **"cross-note movement remain rejected" clause** carried by ADR-0020, both bounded to this archival operation.

## Context

The motivating thread (see ADR-0020): the inbox accumulates a mix of open and completed tasks, and the user wants the inbox to stay "new work only." ADR-0020 lets the user *reorder* tasks; a follow-on idea was to *sort* completed tasks to the bottom on a keypress. Sorting, however, only **manages** the clutter in place — completed tasks live in the inbox forever and the list grows without bound, and it forces an awkward interaction between append-at-EOF quick-add (ADR-0014) and any "completed sinks to the bottom" ordering (new tasks land under the done pile).

**Archival solves the root cause instead of the symptom:** completed tasks *leave* the working note entirely. The inbox stays small and is genuinely just active work; plain append-at-EOF quick-add becomes correct again (everything in the note is active by construction); and the whole sort/append-coherence problem evaporates. This also automates the step ADR-0014 already anticipated the user doing by hand — "the inbox is a capture surface… the user reviews and moves them later in Obsidian."

But archival is the **largest expansion of Taski's write surface to date**. Every write so far is bounded to a **single note** and **never removes an existing line**:

- The grammar-provability gate (ADR-0009/0012/0013) edits an existing line **in place**.
- The append-only creation gate (ADR-0014) and the annotation gate (ADR-0019) **append** to one note.
- The structural-reordering gate (ADR-0020) **permutes** task-line contents within one note — line count invariant, nothing deleted, nothing crosses a note boundary.

Archival does the two things every prior ADR deliberately held back:

1. **Deletion** of existing lines — ADR-0014's gate boundary closes with "line deletion remain explicitly rejected"; ADR-0020's closes with "…insertion, deletion, and text editing remain rejected." Removing the completed lines from the source note **is** that deletion (and shifts every line below).
2. **Cross-note movement** — ADR-0020's boundary closes with "Cross-note movement… remain explicitly rejected," and its Alternatives record "Moving a task to a *different* note is a delete-here + create-there — a different, larger operation." Archival **is** exactly that delete-here + create-there.

So this needs its own gate and a principled boundary, and it must explicitly revoke those two clauses — bounded to the archival operation, not opened generally.

### Why this is safer than "deletion" and "cross-note move" sound

Archival decomposes into two operations that are each already proven, sequenced so the failure mode is recoverable:

- **Phase A — append to the archive.** This is the ADR-0014 append-only creation gate, applied to a second designated note (the archive), with the first-creation path reused verbatim when the archive does not yet exist. Nothing new.
- **Phase B — delete the moved lines from the source.** A pure line-deletion oracle (`remove_lines`, the structural analogue of ADR-0020's `permute_lines`) under the ADR-0004 TOCTOU guard against the ADR-0006 cached note hash. The "positional chaos" ADR-0014 feared is tamed by machinery built since: a whole-note rewrite that preserves non-removed lines and terminators (the ADR-0020 oracle pattern), plus `text_hash` reconciliation (ADR-0005) that already carries a surviving task's surrogate id to its new line number when lines above it disappear.

The principled boundary that keeps this narrow:

> **Taski deletes a source line only after it has durably copied that exact line into the archive.** Not general deletion — deletion only as Phase B of a verified archival copy, and only of flat `[x]`/`[-]` lines.

"Delete only what you have provably persisted elsewhere first" is the same discipline every other gate uses (single note; pure permutation; native syntax only). It bounds the new power tightly: a bug or a crash can at worst **duplicate** a task (copied but not deleted) — never **lose** one.

### The bounded archival gate

> Taski may, at the user's explicit request, **move every completed (`[x]` done or `[-]` cancelled) flat task line from a single source note into a designated archive note**, provided that: (i) the operation removes from the source **only** lines it has **first durably copied** (fsynced) into the archive — copy-then-delete; (ii) each write is individually guarded — the archive append by the ADR-0014 append-only creation gate (first-creation path for a missing archive), the source deletion by the ADR-0004 TOCTOU re-hash against the ADR-0006 cached note hash; (iii) the source note is **entirely flat** (no nested tasks) and resolves to a **single note**; (iv) the moved lines are appended to the archive **verbatim** (no rewrite) and every non-archived line in the source stays byte-identical, line terminators preserved. Arbitrary deletion, deletion of non-completed or nested lines, cross-note movement of *incomplete* tasks, mid-note insertion, and text editing remain explicitly rejected.

## Decision

### What "completed" means (the archival predicate)

A task line is archivable **iff** `indent == 0` **and** it is closed:

- **Done** — `Status::Done` (`[x]` / `[X]`), or
- **Cancelled** — `[-]` (modeled as `Status::Other("-")`; detected via `raw_checkbox_char == "-"`).

**Open (`[ ]`), in-progress (`[/]`), and any other custom checkbox char stay put.** This is the user's explicit decision: only fully done or cancelled work leaves, so the source note stays focused on active work. (Done lines toggled via `d` already carry `✅ <date>` (ADR-0012) and cancelled lines carry `❌ <date>` (ADR-0013), so the archive self-dates from the lines' existing stamps — no extra stamping; see "Verbatim append" below.)

### The interaction (one-shot, no mode)

A new `A` (Shift-`a`) key — `a` is already quick-add (ADR-0014):

- `A` → if **eligible**, compute the source note's archivable line numbers, enqueue one `archive` action, and flash a footer notice (`archived N task(s) → task-archive.md`). One keypress, no modal, no per-task interaction. This matches the user's ask ("a keypress will move all of those tasks to the archive").
- **Eligibility** (TUI gates entry; daemon re-validates defensively):
  - the selected task's **group resolves to a single note** (always true under the default `folder+note` grouping; refused with a notice otherwise, as in ADR-0020);
  - the note is **entirely flat** — every task has `indent == 0` (else a one-line "archive supports flat task lists only (v1)" notice; same orphaning rationale as ADR-0020);
  - **at least one** archivable (`[x]`/`[-]`, flat) task exists (else "no completed tasks to archive").
- No undo in v1 (see Out of scope). Because the move is non-destructive (data is preserved in the archive) and the keypress is a deliberate Shift-chord, v1 acts immediately; a confirmation prompt is a deferred nicety.

### The action model

A new `archive` action_type in `pending_actions`. **No schema change** — existing NOT NULL columns carry anchor/sentinel values, consistent with ADR-0014/0019/0020:

| Column | Value for `archive` | Rationale |
|---|---|---|
| `task_id` | `<anchor task.id>` (first archivable task in the source) | Fetches the cached note hash (ADR-0006) and identifies the source note. |
| `note_path` | `<source note, vault-relative>` | The single note the completed lines are removed from. |
| `line_number` | `<anchor task's line, 1-based>` | Informational anchor; the operation is defined by `payload`. |
| `expected_char` | `''` | Unused (verification is via cached file hash). |
| `new_char` | `''` | Unused. |
| `action_type` | `'archive'` | Dispatch key. |
| `payload` | `"<archive_rel_path>\t<l₁,l₂,…,lₖ>"` | Tab-separates the **archive destination** (vault-relative) from the comma-separated **1-based line numbers** of the archivable lines, top-to-bottom. |

The archive path is carried **in the payload** (not a new column and not re-derived in the daemon): the TUI resolves it from config — the single source of truth for path resolution, exactly as it already resolves the inbox path — and the daemon's `process_*` functions take only `(conn, vault_root, action)`. Tab is the separator because a comma appears only in the line list and a vault-relative path will not contain a tab.

### The pure oracles (no I/O, proptested)

Two structural functions in `taski-core`, the deletion analogues of `permute_lines`:

```rust
/// Return the content of the lines named in `line_numbers` (1-based), in the order
/// the numbers are given, each WITHOUT its terminator — the block to append to the
/// archive. Out-of-range numbers are skipped.
pub fn extract_lines(content: &str, line_numbers: &[usize]) -> Vec<String> { /* ... */ }

/// Remove the lines named in `line_numbers` (1-based) from `content`, leaving every
/// other line and every line terminator byte-identical. The structural deletion
/// analogue of `permute_lines`. Out-of-range / duplicate numbers are ignored.
pub fn remove_lines(content: &str, line_numbers: &[usize]) -> String { /* ... */ }
```

Proptest invariants (the "never-corrupts" contract, generalized from `permute_lines`):

- **No loss across the pair** — `extract_lines(s, L)` ⊎ `{task lines of remove_lines(s, L)}` equals the multiset of `s`'s task lines: every removed line reappears in the extracted block; nothing is invented.
- **Survivors pinned** — every line whose number is not in `L` is byte-identical and keeps its relative order in `remove_lines(s, L)`.
- **Count** — `remove_lines(s, L)` has exactly `lines(s) − |distinct in-range L|` lines.
- **Terminators pinned** — surviving lines keep their original terminator (`\n` / `\r\n` / none-at-EOF); CRLF notes survive (the landmine `permute_lines` already guards).
- **Identity** — `remove_lines(s, &[]) == s`.
- **Idempotent block** — `extract_lines` returns content only, never terminators, so the archive append controls its own line endings.

### The daemon write path — `process_archive` (two phases, copy first)

```
process_archive(conn, vault_root, action):
  1. Anchor row: lookup_task_for_action(task_id) → fallback lookup_task_by_location(note_path, line_number).
     None → TaskNotFound.   (Same claim+fallback as process_reorder.)
  2. Read the SOURCE note fresh. NotFound → TaskNotFound.
  3. snapshot_hash = content_hash(source_bytes); if Some(&snapshot_hash) != row.note_hash → ConflictNoteChanged.   (ADR-0006 + ADR-0004)
  4. Decode UTF-8 (else TaskLineMismatch). Parse payload → (archive_rel, lines L). Malformed → ArchiveInconsistent.
  5. Defensive validation: every lₙ in L must be a FLAT (indent == 0) CLOSED ([x]/[X] or raw "-") task line in the source
     (re-parse with parse_tasks). Empty L → Applied (no-op). Any line not flat-and-closed → ArchiveInconsistent.
  6. block = extract_lines(source_content, &L).   // the verbatim lines to archive

  ── Phase A: durable copy to the archive (ADR-0014 gate) ──────────────────────────
  7. archive_abs = vault_root.join(archive_rel).
     If archive exists:  read it; archive_hash = content_hash; append `block` (newline-joined, leading '\n'
                         if the file lacks a trailing newline) via atomic_write(archive_abs, new_archive_bytes, &archive_hash).
                         Conflict → ConflictNoteChanged (ABORT; source untouched — safe).
     If archive missing: atomic_create(archive_abs, block-as-content)  // first-creation path, no TOCTOU (ADR-0014)
     Both paths fsync before returning Written → the archived lines are now DURABLE.

  ── Phase B: delete the moved lines from the source (ADR-0004 TOCTOU) ─────────────
  8. new_source = remove_lines(source_content, &L).
     atomic_write(source_abs, new_source.as_bytes(), &snapshot_hash).
       Written  → re-index BOTH notes; mark action done.
       Conflict → ConflictNoteChanged. (Source changed in the read→write window; archive copy already landed —
                  the tasks now sit in BOTH files. No loss; see Known accepted risk.)
```

**Phase ordering is the safety property.** The archive append is fsynced *before* any source line is removed, so a crash or refusal between the phases leaves every task in **both** files (visible, recoverable) — never in **neither** (lost). This is the same "duplication is recoverable, loss is not" stance ADR-0020 took on replay.

### Re-indexing and identity

Both notes are re-indexed after a successful Phase B. `reconcile_note` (ADR-0005, `text_hash`) inserts fresh surrogate rows for the archived tasks under the **archive** note, and carries each **surviving** source task's surrogate id to its new (shifted-up) line number — exactly the case ADR-0005 already handles when content above a task changes. **ADR-0005 is not amended.**

The archive is indexed like any note; its `[x]`/`[-]` tasks are hidden under the default Open filter and visible under Done. A user who does not want the archive indexed at all can add `taski-skip` frontmatter to it (ADR-0017) — no new mechanism.

### Verbatim append (no rewrite, no re-stamp)

Phase A appends each completed line **byte-for-byte as it stood in the source** (terminator excluded; the archive append controls endings). Taski does not add or normalize a `✅`/`❌` stamp: a line toggled done/cancelled via Taski already carries one (ADR-0012/0013); a line marked `[x]` by hand in Obsidian without a date is archived as-is (the user can fix it in Obsidian, the same edge ADR-0014 accepts for user-typed text). Verbatim append keeps Phase A strictly inside the ADR-0014 append-only gate — no oracle, no rewrite, no token decision.

### Config plumbing

New `archive_path: Option<String>` on `Config`; `resolve_archive_path(&cfg) -> String` mirrors `resolve_inbox_path` (config value → default `"task-archive.md"`); `template()` gains a commented `# archive_path = "task-archive.md"` line. The TUI's `run_inner` resolves it and threads it through `run_loop` → `App::new` (mirroring `inbox_path`). No launcher changes.

## Known accepted risk — duplication over loss

The two writes are not a single cross-file atomic transaction (no filesystem provides one). The failure modes, all **no-loss**:

| When | State on disk | On daemon restart / retry |
|---|---|---|
| Crash **after Phase A, before Phase B** | Lines in **both** archive and source | Action still `pending` with the source's **old** cached hash; the source is unchanged so the hash still matches → the action **re-runs**, appending the block to the archive **again** (duplicate in the archive) then deleting from the source. End state: correctly removed from source, **duplicated in the archive**. Recoverable by hand. |
| Crash **after Phase B, before marking `done`** | Lines in archive only; removed from source | Action still `pending`, but the source hash now **mismatches** (our own Phase B changed it) → `ConflictNoteChanged` → **refuses** (no double-apply). Spurious `failed` row for an action that actually landed; correct end state. Same replay-refuses property as ADR-0020. |
| Phase A `Conflict` (archive edited concurrently) | Source untouched | Whole action refuses; nothing moved; user retries. |
| Phase B `Conflict` (source edited in read→write window) | Lines in **both** files | Archive copy landed, source delete refused; tasks in both files. Retry re-appends (archive duplicate) then deletes. No loss. |

So archival can, in rare crash/conflict windows, **duplicate a task into the archive**; it can **never lose one**. A dedup-on-append guard (skip lines already present as the archive's trailing block) is a possible hardening, deferred — duplication in an append-only archive is low-stakes and easy to clean up.

## Rationale

- **Archival solves the root cause; sorting only manages the symptom.** Removing completed tasks keeps the source note bounded and focused, automates the "move them later" step ADR-0014 anticipated, and dissolves the sort/append-coherence problem entirely.
- **Copy-then-delete makes a scary operation safe.** Phase A is the proven ADR-0014 append gate; Phase B is a proven-class whole-note rewrite under the ADR-0004 guard. Sequencing copy-before-delete makes the only failure mode duplication, never loss.
- **The gate is principled and narrow.** Single source note, flat-only, closed lines only, verbatim append, delete-only-after-durable-copy. Arbitrary deletion, cross-note movement of incomplete tasks, insertion, and text edits stay rejected — opening archival does not become an open door to free structural editing.
- **Identity is free.** `text_hash` reconciliation already moves a surviving task's id up when lines above it vanish, and assigns fresh ids to the archived tasks under the archive note. ADR-0005 untouched.
- **No schema bump.** Anchor/sentinel values in existing `pending_actions` columns, with the archive destination tab-encoded in `payload`, consistent with ADR-0014/0019/0020.
- **Flat-only is correctness, not a shortcut.** Deleting a completed parent that still has incomplete children would orphan them; restricting v1 to fully-flat notes removes the hazard and covers the motivating inbox case, exactly as ADR-0020 reasoned.

### Why this does not relax ADR-0004 or ADR-0005

- **ADR-0004** is reused *verbatim* on **both** writes: `atomic_write`'s whole-file re-hash guards the archive append and the source deletion identically, byte-count-agnostic. The archive first-creation path is the same bounded, justified ADR-0014 exception (a non-existent file has no state to conflict with).
- **ADR-0005** is *not crossed*: archival injects no marker and relies on existing content-hash reconciliation to carry identity (survivors up in the source; fresh ids in the archive). No opaque identity bookkeeping.

## Consequences

- ✅ The user clears completed work out of a note in one keypress; the inbox stays "new work only" and plain append-at-EOF quick-add is correct again.
- ✅ Phase A reuses the ADR-0014 append-only gate (and its first-creation path) unchanged; Phase B reuses the ADR-0020 whole-note-rewrite oracle pattern under ADR-0004.
- ✅ No schema bump (anchor/sentinel columns; archive path tab-encoded in `payload`).
- ✅ ADR-0005 is not crossed; survivors keep ids, archived tasks get fresh ids in the archive.
- ✅ Failure modes are **no-loss** (duplication-over-loss); Phase-B replay refuses rather than double-applies.
- ⚠️ **ADR-0003 is amended a seventh time**, opening a **fourth gate class** — bounded archival (move-by-copy-then-delete) — Taski's **first deletion of an existing line** and **first cross-note operation**. It **revokes the "deletion remain rejected" clause** in the ADR-0014/0020 boundaries and the **"cross-note movement remain rejected" clause** in ADR-0020, both **bounded to this archival operation**; arbitrary deletion and cross-note movement of incomplete tasks stay rejected.
- ⚠️ **First multi-file write.** Cross-file atomicity is impossible; the copy-then-delete ordering bounds the failure to recoverable duplication. A `failed` row may appear for an archive that actually landed (Phase-B crash); the watcher re-indexes regardless.
- ⚠️ The `A` key is consumed for archive.
- ⚠️ **Flat-only in v1**: archive is unavailable on notes containing nested tasks. **No undo in v1** (recorded as a planned feature below).
- ⚠️ The archive note is indexed like any note (its done/cancelled tasks appear under the Done filter); add `taski-skip` (ADR-0017) to exclude it.

### Cross-reference note — ADR-0003 (seventh amendment)

ADR-0003's amendment block records a **seventh** amendment, opening the fourth gate class:

> The write-back scope is widened to also include **bounded archival** (`archive` action): at the user's explicit request, Taski may move every flat `[x]` done / `[-]` cancelled task line from a **single** source note into a designated archive note, by **durably copying** the lines into the archive (the ADR-0014 append-only creation gate, first-creation path for a missing archive) and **then** deleting exactly those lines from the source (the ADR-0006-hash-gated, ADR-0004-TOCTOU-guarded `remove_lines` rewrite). This is Taski's first line **deletion** and first **cross-note** operation; it **revokes** the "deletion remain rejected" clause in the ADR-0014/0020 boundaries and the "cross-note movement remain rejected" clause in ADR-0020, **bounded to this archival operation**. The grammar-provability gate (ADR-0009) and the creation/annotation/reordering gates (ADR-0014/0019/0020) are otherwise **unchanged**. Arbitrary deletion, deletion of non-completed or nested lines, cross-note movement of incomplete tasks, mid-note insertion, and text editing remain rejected.

## Out of scope for v1 (deliberately deferred)

- **Undo (`u` after `A`).** Recorded as a planned feature, not built in v1. Archival is invertible in principle (re-append the lines to the source, remove them from the archive's trailing block), so `LastAction::Archive { source, archive, lines }` + a paired inverse action is a clean fast-follow — deferred to keep v1 minimal and because the move is non-destructive (the data is preserved in the archive). To revert manually, move the lines back in Obsidian.
- **Block-move (nested tasks).** Archiving a completed parent together with its descendants needs the same "task block" span model ADR-0020 deferred. v1 refuses on any note with nested tasks.
- **Archive grouping / dated headings.** v1 is a plain verbatim append. Grouping archived lines under a `## YYYY-MM-DD` heading (or per-source-note sections) is a future nicety.
- **Dedup-on-append.** Skipping lines already present as the archive's trailing block (hardening the crash-duplication window). Deferred — append-only archive duplication is low-stakes.
- **Re-stamping a `✅`/`❌` on hand-marked lines.** v1 appends verbatim; normalizing missing completion dates is out of scope.
- **Archiving from an arbitrary multi-note group.** v1 requires the selection to resolve to one note (the source), like ADR-0020.

## Alternatives considered

- **Sort completed to the bottom (the prior idea).** Rejected in favor of archival: sorting manages clutter in place and grows forever, and fights append-at-EOF quick-add. Archival removes the clutter and dissolves the coherence problem. (Sorting was itself a thin `reorder` permutation — cheap — but it does not solve the root problem.)
- **Delete-then-copy (or a single fused write).** Rejected: deleting first risks loss if the archive write fails or the daemon crashes between. Copy-then-delete makes the only failure mode recoverable duplication. A single fused cross-file write is impossible on a POSIX filesystem.
- **Hard-delete completed tasks (no archive).** Rejected: destroys data; the user wants the completed history retained, just out of the working note.
- **A new `pending_archives` table / schema bump.** Rejected: the anchor/sentinel-column pattern plus a tab-encoded payload is adequate and consistent with ADR-0014/0019/0020; avoids a destructive migration.
- **Move incomplete/in-progress tasks too (configurable predicate).** Rejected at user direction: only fully done/cancelled work leaves, so the source note stays focused on active work; in-progress and open always stay.
- **Per-task archive (archive only the selected task).** Deferred: the bulk "archive all completed in this note" is the motivating cleanup gesture. A selected-task variant is an easy later addition on the same machinery.
- **Cross-file atomicity via a journal / two-phase commit.** Rejected as over-engineering for a personal tool: copy-then-delete with replay-refuses (Phase B) and accepted archive-duplication (Phase A) is simpler and no-loss.

## Edge cases

| Case | Behavior |
|---|---|
| `A` on a note with any nested (indented) task | Refuse; "archive supports flat task lists only (v1)" notice. |
| `A` on a note with no `[x]`/`[-]` task | Refuse; "no completed tasks to archive" notice. |
| `A` while the group spans multiple notes (non-default grouping) | Refuse with a notice (resolve to one note required), as in ADR-0020. |
| In-progress (`[/]`) and open (`[ ]`) tasks present | Left untouched in the source; only `[x]`/`[-]` move. |
| Cancelled `[-]` line | Treated as completed (via `raw_checkbox_char == "-"`); archived. |
| `[x]` line with no `✅` date (hand-marked in Obsidian) | Archived verbatim; no stamp added. |
| Archive does not exist | Created via the first-creation path (temp → fsync → rename, no TOCTOU), per ADR-0014. |
| Archive exists, no trailing newline | Append prepends `\n` so the block starts on its own line. |
| Source edited in Obsidian since index | Phase-A may still append (its own gate), then **Phase B** hits the source hash check → `ConflictNoteChanged` → refuses the delete; tasks sit in both files until retry. No loss. |
| Archive edited concurrently (Phase-A conflict) | Whole action refuses; source untouched; retry. |
| CRLF-terminated source | `remove_lines` keeps each surviving line's terminator (proptested); archive append uses `\n`. Parser tolerates mixed endings; Obsidian normalizes. |
| Crash between Phase A and Phase B | Lines in both files; replay re-runs and may duplicate in the archive (no loss). See Known accepted risk. |
| Crash after Phase B, before `done` | Source hash mismatch → replay refuses (no double-apply). |
| `payload` names a line that isn't a flat closed task (stale/foreign) or is malformed | `ArchiveInconsistent` → refuse (defensive; the hash guard should preclude it). |
| Archive is the same path as the source | Defensive refuse (`ArchiveInconsistent`): a note cannot be its own archive (would re-append its own lines then delete them). The TUI also forbids configuring `archive_path == inbox_path`. |

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; `archive` routes through `pending_actions` like all writes.
- [ADR-0003](./0003-checkbox-only-mvp.md) — **amended a seventh time** by this ADR (fourth gate class: bounded archival; first deletion; first cross-note operation; revokes the prior "deletion" and "cross-note movement" rejection clauses, bounded to archival).
- [ADR-0004](./0004-refuse-on-conflict.md) — refuse-on-conflict / TOCTOU; **reused unchanged** on both the archive append and the source deletion.
- [ADR-0005](./0005-surrogate-identity.md) — **not amended**; reconciliation carries survivors' ids up and assigns fresh ids to archived tasks.
- [ADR-0006](./0006-note-content-cached-in-index.md) — the cached note hash that gates the source deletion.
- [ADR-0009](./0009-scheduled-date-today.md) — grammar-provability gate; **unchanged** (this ADR opens a separate gate).
- [ADR-0012](./0012-done-date-on-toggle.md) / [ADR-0013](./0013-cancelled-date-on-cancel.md) — the `✅`/`❌` stamps the archived lines already carry (the archive self-dates); define the "completed" predicate this ADR archives on.
- [ADR-0014](./0014-quick-add-inbox-creation.md) — the append-only creation gate (and first-creation path) that **Phase A reuses**; this ADR **revokes its "line deletion remain rejected" clause**, bounded to archival.
- [ADR-0017](./0017-frontmatter-taski-skip-opt-out.md) — `taski-skip` to exclude the archive note from indexing.
- [ADR-0020](./0020-task-reordering.md) — the structural-reordering gate; this ADR reuses its whole-note-rewrite oracle pattern (`permute_lines` → `remove_lines`), flat-only eligibility, and replay-refuses property, and **revokes its "deletion" and "cross-note movement remain rejected" clauses**, bounded to archival.
