# ADR-0020: Task reordering — a TUI-local move mode that commits a single in-note line-content permutation

- **Status:** Accepted (the "deletion" and "cross-note movement remain rejected" clauses in this ADR's gate boundary were **revoked 2026-06-24 by [ADR-0021](./0021-archive-completed-tasks.md)** — bounded to its copy-then-delete archival operation, which reuses this ADR's whole-note-rewrite oracle pattern, flat-only eligibility, and replay-refuses property; this ADR's structural-reordering gate is otherwise unchanged)
- **Date:** 2026-06-24
- **Decides:** How Taski lets the user reorder tasks within a note from the TUI — an `m`-key "move mode" in which `j`/`k` (or `↑`/`↓`) bubble the selected task up/down among the other tasks in its note, `Enter` commits the new order as a single `reorder` write, and `Esc` cancels (restoring the original order). v1 is **flat-only**: move mode is offered only when the selected task's note has no nested (indented) tasks. **Amends [ADR-0003](./0003-checkbox-only-mvp.md)** for a sixth time and opens a **third gate class** — bounded structural reordering — and **revokes the "reordering remain rejected" clause** carried in the ADR-0014 and ADR-0019 gate boundaries.

## Context

Every write-back gesture to date either **edits an existing line in place** (checkbox flips `Space`/`d`/`i`; the `⏳`/`✅`/`❌` date-emoji stamps; the bullet toggle `b`; the ADR-0019 link insertion) or **appends** (quick-add ADR-0014; task-notes ADR-0019). **None of them change the *position* of an existing line.** That is deliberate: ADR-0014's and ADR-0019's gate boundaries both close with "…deletion, and **reordering** remain explicitly rejected."

The user's workflow exposes the gap. Tasks captured via quick-add (`a`) stack at the end of the inbox note in arrival order. The user frequently wants to nudge a just-added task up or down a line or two — pure triage, no text change. Today that requires switching to Obsidian and dragging lines. The request: a TUI gesture to move the selected task up/down within its note and commit the new order.

This is Taski's **first write that changes which line a task occupies** — a structural mutation. It therefore cannot ride the grammar-provability gate (ADR-0009) or either creation/annotation gate (ADR-0014/0019); it needs its own principled boundary, and it must explicitly revoke the blanket "reordering rejected" clause those two ADRs carry.

### Why this needs a third gate, not an existing one

- **The grammar-provability gate (ADR-0009)** admits standard Tasks tokens with a single insertion grammar. Reordering writes no token at all — it permutes existing line contents. Not applicable.
- **The bounded append-only creation gate (ADR-0014)** is append-only and explicitly forbids "modification / reorder / deletion of existing lines." Reordering *is* that forbidden modification. Outside the gate.
- **The bounded task-annotation gate (ADR-0019)** appends note content + one idempotent link insertion, and also forbids reordering. Outside the gate.

So this ADR opens a **bounded structural-reordering gate**:

> Taski may, at the user's explicit request, **permute the contents of the checkbox-task lines within a single note among those same lines' existing positions**, provided that: (i) the operation is confined to **one note** (no cross-note movement); (ii) it changes **no line's count and no non-task line** — line terminators and every non-task line stay byte-identical and in place, and the multiset of task-line contents is preserved (the operation is a pure permutation); (iii) it is committed as a **single `atomic_write`** under the ADR-0004 TOCTOU discipline; and (iv) the write proceeds only when the note's on-disk content hash still equals the hash cached at index time (ADR-0006), which guarantees every listed line is byte-identical to what was indexed. Cross-note movement, moving or reordering non-task lines, insertion, deletion, and text editing remain explicitly rejected.

### Why this is a smaller mutation than "structural" suggests

The reorder is modeled as a **permutation of task-line contents among their existing line positions**, *not* as deleting a line and re-inserting it elsewhere (which would shift every line below). Concretely: a note's checkbox-task lines sit at positions `P = [p₁ < p₂ < … < pₖ]` with contents `C = [c₁, …, cₖ]`. A reorder produces a permutation π and writes content `c_π(i)` at position `pᵢ`. **Line count is invariant; non-task lines never move; only *which task text sits at which task-line position* changes.** This reduces "reorder" to *k in-place line-content replacements* — the same mutation class already proven safe (the `⏳`/`✅`/`❌`/bullet rewrites replace a line's content in place), applied to several lines at once.

Identity follows content for free. `reconcile_note` matches freshly-parsed tasks to existing rows by `text_hash` and **keeps the surrogate `id` (UPDATE in place)** even when a task's `line_number` changed (ADR-0005, context.md "Indexing"). After a reorder the task that moved up keeps its `id` at its new line — exactly the mechanism ADR-0005 already relies on when text *above* a task shifts it. **ADR-0005 is not amended.**

## Decision

### The interaction (TUI-local move mode)

A new `m` key enters **move mode** on the selected task (a new `moving` App state, mirroring the modal pattern of search / quick-add / add-note):

- `m` → enter move mode **iff eligible** (see "Eligibility"). The footer shows a `MOVE` indicator and the selected row is visually marked.
- `j` / `↓` → swap the selected task with the **next** task in its note; `k` / `↑` → swap with the **previous**. Selection follows the task as it bubbles. Movement is **clamped to the note's task block** — you cannot bubble past the first/last task of the note (and never into another note or group).
- `Enter` → **commit**: enqueue one `reorder` action carrying the final order; exit move mode. If the net order is unchanged from entry, **no action is enqueued** (idempotent no-op).
- `Esc` → **cancel**: discard the local reorder, restore the entry order, exit move mode.
- All other normal-mode keys are suppressed while moving (same `else if` modal branch as search/quick-add), except `Ctrl-C` (always-available quit).

**Nothing is written to the vault until `Enter`.** Move mode reorders only an in-memory buffer of the displayed rows; `Esc`'s "snap back" is therefore free — no `pending_actions` row was ever created. This is the central design choice and it falls directly out of the async write model (see Rationale).

### Why per-keypress writes are not used (latency)

The daemon event loop ticks every **500ms** and the TUI re-reads the index every **750ms** (context.md "latency expectations"). A daemon round-trip per `j`/`k` press would impose ~1–2s of lag per nudge and a flood of `pending_actions` rows. Move mode instead reorders locally and commits **once**, so the only write is the final order.

**The 750ms index refresh is suspended while in move mode.** Otherwise a periodic re-read would clobber the local reorder buffer with the on-disk (pre-reorder) order. On commit or cancel, normal refresh resumes.

### Eligibility — flat-only (v1)

Move mode is offered only when the selected task's note is **entirely flat**: every task in that note has `indent == 0` (no nested subtasks). If any task in the note is indented, `m` shows a one-line notice ("reorder supports flat task lists only (v1)") and does nothing.

The reason is correctness, not laziness: a task with indented children owns the lines beneath it. The permutation model swaps a single task line's content; applied to a parent it would **orphan its children** (leave them stranded under whatever content moved into the parent's line). Restricting v1 to fully-flat notes makes every task line independent, so a content permutation is unambiguous and cannot orphan anything. The inbox — the motivating case — is a flat `- [ ]` list, so this covers the real workflow. **Block-move (moving a task together with its indented descendants) is the deferred follow-up** (see Out of scope).

Move mode also requires the selected **group to be a single note** — the permutation maps to one note's line positions. Under the default `folder+note` grouping every group is exactly one note, so this always holds; under `note`/`folder`/`tag`/`priority` grouping (where a group can span notes) `m` is refused with a notice unless the group happens to be one note. The TUI gates entry on both conditions before setting `moving`; the daemon re-validates defensively (every listed line must be a flat checkbox task in the resolved note).

### The action model

A new `reorder` action_type in `pending_actions`, enqueued by the TUI on commit. **No schema change** — the existing NOT NULL columns carry sentinel/anchor values, consistent with the ADR-0014/0019 pattern:

| Column | Value for `reorder` | Rationale |
|---|---|---|
| `task_id` | `<anchor task.id>` (the task that was moved) | Used to fetch the cached note hash (ADR-0006) and to identify the note. |
| `note_path` | `<note, vault-relative>` | The single note whose task lines are permuted. |
| `line_number` | `<anchor task's entry line, 1-based>` | Informational anchor; the operation is defined by `payload`, not this line. |
| `expected_char` | `''` | Unused (verification is via cached file hash, not a single char). |
| `new_char` | `''` | Unused. |
| `action_type` | `'reorder'` | Dispatch key. |
| `payload` | `"<l₁>,<l₂>,…,<lₖ>"` | The **involved (visible) task lines' original 1-based line numbers, listed in the desired top-to-bottom order**. The *set* `{lᵢ}` is a subset of the note's flat-task-line positions (the tasks the active filter leaves visible); the *order* encodes the permutation. |

#### Why no per-line `expected_char` is needed

Identical to ADR-0019's argument: the whole-note content hash gates the write. The daemon reads the note, hashes it, and compares to the anchor task's cached `note_hash` (ADR-0006). If they match, **every line — including all `k` task lines named in `payload` — is byte-identical to what was indexed**, so all the `lᵢ` are valid task-line positions with known bytes. If they differ → refuse. A per-line char check would be strictly weaker than this file-level guard.

### The daemon write path

`process_reorder(action)` — a single read, a single `atomic_write`:

1. Resolve the note: `vault_root.join(&action.note_path)`. Missing → `TaskNotFound`.
2. Read it; compute `current_hash`; compare to the anchor task's cached note hash (ADR-0006). Mismatch → `ConflictNoteChanged` (refuse).
3. Parse `payload` into the desired-order list `L = [l₁, …, lₖ]`. Validate (defensive — the hash guard should already imply all of this):
   - the payload is well-formed (non-empty, comma-separated positive integers);
   - every `lᵢ` is the line of a **flat** (`indent == 0`) checkbox task in the note (`parse_tasks`), i.e. `{lᵢ}` is a **subset** of the note's flat-task-line positions.
   On any failure → refuse (`ReorderInconsistent`). Note this is a *subset*, not equality: when a filter (status / Today / search) hides some of the note's tasks, the TUI lists only the **visible** task lines, and the permutation moves those among their own positions while every hidden task line (and every non-task line) stays put.
4. Target positions are the sorted set `P = sort({lᵢ})`. Place the content of line `lᵢ` into the i-th smallest position: `new_content[P[i]] = old_content[L[i]]`. **Line terminators stay with positions, not content** (a `\r\n`-terminated position stays `\r\n`); non-task lines are copied verbatim. This is the CRLF-safe analogue of the existing single-line splice (see "CRLF" below).
5. `atomic_write(note, new_bytes, current_hash)` — the full ADR-0004 TOCTOU guard, unchanged. `Conflict` → `ConflictNoteChanged`.
6. Re-index the note. `reconcile_note` matches by `text_hash`, so each moved task keeps its surrogate `id` at its new line. Mark the action `done`.

### The pure oracle

A pure, proptested permutation applier in `taski-core` (no I/O), the structural analogue of `rewrite_emoji_date`:

```rust
/// Permute the contents of the lines named in `desired_order` (1-based line
/// numbers) among those same lines' positions, leaving every other line and
/// every line terminator byte-identical. `desired_order` must be a permutation
/// of a set of line numbers; the i-th smallest of those positions receives the
/// content of `desired_order[i]`.
pub fn permute_lines(content: &str, desired_order: &[usize]) -> String { /* ... */ }
```

Proptest invariants (the "never-corrupts" contract, generalized from `writeback_proptest`):

- **Line count preserved** — output has exactly as many lines as input.
- **Permutation** — the multiset of task-line contents is unchanged (no content invented or lost).
- **Non-listed lines pinned** — every line whose number is not in `desired_order` is byte-identical and in its original position.
- **Terminators pinned** — each position keeps its original terminator (`\n` / `\r\n` / none-at-EOF), so CRLF notes survive (the landmine the `metadata_writeback_proptest` guards for single lines).
- **Identity** — `permute_lines(s, ascending) == s` (idempotent no-op when order is unchanged).
- **Invertible** — applying the inverse permutation restores the original (this is what makes undo a clean fast-follow; see below).

### CRLF

The existing single-line rewrites compute the splice span to *exclude* a trailing `\r` so `\r\n` is preserved (context.md landmine). `permute_lines` applies the same discipline per position: a position's terminator is fixed structure; only the content *before* the terminator is permuted. In a uniformly-terminated file this is moot, but the rule is defined explicitly and proptested.

### Out of scope for v1 (deliberately deferred)

- **Block-move (nested tasks).** Moving a parent task together with its indented descendants is the natural v2. v1 refuses move mode on any note containing nested tasks (see Eligibility). Block-move needs a "task block" span model (parent + contiguous deeper-indented lines) and a position-shifting permutation rather than a pure content swap.
- **Undo.** Not wired in v1 — to revert, re-enter move mode and move the task back (or `Esc` before committing). Unlike `add_note`'s append (which ADR-0019 could not cleanly undo), **reorder is cleanly invertible** (the oracle's inverse-permutation property), so `u` support is a low-risk fast-follow: store the inverse order as the `LastAction` and enqueue a second `reorder`. Deferred only to keep v1 minimal.
- **Cross-note / cross-group movement.** Rejected by the gate. Moving a task to a *different* note is a delete-here + create-there — a different, larger operation.
- **Drag with the mouse.** Keyboard-only, consistent with the rest of the TUI.

## Known accepted risk — replay refuses rather than double-applies

If the daemon writes successfully but crashes before marking the action `done` (and before re-indexing), on restart the action is still `pending` with the anchor task's **old** cached note hash, while the file now holds the **new** order. The replay therefore hits step 2's hash check and **refuses** (`ConflictNoteChanged`) — it does **not** re-apply. So reorder is effectively replay-safe (no double-application), at the cost of a possible spurious `failed` row for an action that actually landed. This is strictly better than the append features' replay behavior (ADR-0014/0019, which can duplicate). The note will be re-indexed by the watcher regardless.

## Rationale

- **The mutation is a content permutation, not a line shuffle.** Line count and non-task lines are invariant; only which task text sits at which task-line position changes. This is `k` in-place line-content replacements — the already-proven-safe mutation class — committed atomically.
- **Identity is free.** `text_hash` reconciliation (ADR-0005) already moves a task's surrogate `id` to its new line when content shifts. Reorder is exactly that case, intentionally triggered. ADR-0005 is untouched.
- **TUI-local buffer + single commit fits the async model.** The 500ms/750ms latencies make per-keypress writes unusable; buffering locally and committing once on `Enter` is both responsive and frugal, and makes `Esc` cancel cost-free (nothing was enqueued).
- **Hash-gated identity, no schema bump.** ADR-0006's cached hash + ADR-0004's TOCTOU guard make a per-line `expected_char` unnecessary; existing `pending_actions` columns carry sentinel/anchor values, consistent with ADR-0014/0019.
- **Flat-only is correctness, not a shortcut.** A content permutation cannot safely move a parent without its children; restricting v1 to flat notes removes the orphaning hazard entirely while covering the inbox workflow that motivated the feature.
- **The gate is principled and narrow.** Single note, pure permutation of task-line contents, atomic, hash-gated. Cross-note movement, non-task-line movement, insertion, deletion, and text edits stay rejected — so opening reordering does not become an open door to free structural editing.

### Why this does not relax ADR-0004 or ADR-0005

- **ADR-0004 (refuse-on-conflict)** is reused *verbatim*: `atomic_write`'s whole-file re-hash is byte-count-agnostic and equally guards a `k`-line permutation as a one-byte flip.
- **ADR-0005 (surrogate identity / no injected marker)** is *not crossed*: reordering injects nothing and relies on the existing content-hash reconciliation to carry identity to the new line. No opaque marker, no identity bookkeeping change.

## Consequences

- ✅ The user can reorder tasks within a note from the TUI; the inbox triage workflow no longer requires Obsidian.
- ✅ `atomic_write` and ADR-0004 are reused unchanged; the whole permutation commits in one atomic write.
- ✅ No schema bump (sentinel/anchor values in existing columns, per ADR-0014/0019).
- ✅ ADR-0005 is not crossed (identity follows content via existing `text_hash` reconciliation).
- ✅ Replay refuses rather than double-applies (better than the append features).
- ⚠️ **ADR-0003 is amended a sixth time**, opening a **third gate class** — bounded structural reordering — and **revoking the "reordering remain rejected" clause** previously carried by the ADR-0014 and ADR-0019 gate boundaries. The grammar-provability and creation/annotation gates are otherwise unchanged.
- ⚠️ **First write that changes a line's position.** Bounded to a within-note permutation of task-line contents with line count and non-task lines invariant.
- ⚠️ The `m` key is consumed for move mode; the 750ms index refresh is suspended while moving.
- ⚠️ **Flat-only in v1**: move mode is unavailable on notes containing nested tasks. Block-move deferred.
- ⚠️ **No undo in v1** (re-enter move mode to revert) — though reorder is cleanly invertible, so undo is a low-risk fast-follow.

### Cross-reference note — ADR-0003 (sixth amendment)

ADR-0003's amendment block records a **sixth** amendment, opening the third gate class:

> The write-back scope is widened to also include **bounded structural reordering** (`reorder` action): at the user's explicit request, Taski may permute the contents of the checkbox-task lines within a **single** note among those same lines' existing positions, preserving line count and every non-task line, committed as one `atomic_write` under ADR-0004 and gated by the ADR-0006 cached note hash. This **revokes the "reordering remain rejected" clause** in the ADR-0014 and ADR-0019 gate boundaries. The grammar-provability gate (ADR-0009) and the creation/annotation gates (ADR-0014/0019) are otherwise unchanged. Cross-note movement, moving non-task lines, insertion, deletion, and text editing remain rejected.

## Alternatives considered

- **Per-keypress write (enqueue a swap on every `j`/`k`).** Rejected: ~1–2s lag per nudge under the 500ms/750ms latencies, a flood of `pending_actions` rows, and a fragile `Esc`-cancel (would have to enqueue reverse swaps). The local-buffer + single-commit model is both faster and makes cancel free.
- **Model the reorder as delete-line + insert-line (line shift).** Rejected: shifts every line below the move and is a strictly larger, more corruption-prone mutation. The content-permutation model keeps line count and non-task lines invariant and reduces to the proven in-place-replace class.
- **A `tasks.sort_order` / manual-order column in the index.** Rejected: it would make the *index* the source of order, diverging from the note (Obsidian is the source of truth, TL;DR). Order must live in the note's line order, where Obsidian and Dataview see it.
- **Block-move (parent + descendants) in v1.** Deferred: needs a task-block span model and a position-shifting permutation; flat-only covers the motivating inbox case at a fraction of the complexity and with no orphaning hazard.
- **Undo in v1.** Deferred: clean to add later (reorder is invertible), but omitted to keep v1 minimal; `Esc`-before-commit and re-entering move mode cover the immediate need.
- **A `pending_reorders` table / schema bump.** Rejected: the sentinel/anchor-column pattern is adequate and consistent with ADR-0014/0019; avoids a destructive migration.

## Edge cases

| Case | Behavior |
|---|---|
| `m` on a note with any nested (indented) task | Refuse to enter; one-line "flat task lists only (v1)" notice. |
| `m` on a note with a single task | Enter is allowed but `j`/`k` are no-ops (clamped); `Enter`/`Esc` exit with no change. |
| Bubble past the first / last task of the note | Clamped — no movement, stays in move mode. |
| `Enter` with order unchanged from entry | No `pending_actions` row enqueued (idempotent no-op). |
| `Esc` after several swaps | Local buffer restored to entry order; nothing was written. |
| Note's content hash changed since index (Obsidian edit mid-move) | On commit, `process_reorder` hits the hash check → `ConflictNoteChanged` → refuse; surfaced to the TUI; the move buffer is discarded on the next refresh and the user retries. |
| Note file missing at commit | `TaskNotFound`. |
| Index refresh fires during move mode | Suspended — the local buffer is authoritative until commit/cancel. |
| Daemon crash between successful write and `done` | Replay refuses on hash mismatch (no double-apply); see Known accepted risk. |
| CRLF-terminated note | Each position keeps its terminator; only content is permuted (proptested). |
| `payload` names a line that isn't a flat checkbox task (stale/foreign), or is malformed | `ReorderInconsistent` → refuse (defensive; the hash guard should preclude it). |
| A filter hides some of the note's tasks | The TUI lists only the visible task lines; they permute among their own positions while hidden task lines (and non-task lines) stay put. |

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; `reorder` routes through `pending_actions` like all writes.
- [ADR-0003](./0003-checkbox-only-mvp.md) — **amended a sixth time** by this ADR (third new gate class: bounded structural reordering; revokes the prior "reordering rejected" clause).
- [ADR-0004](./0004-refuse-on-conflict.md) — refuse-on-conflict / TOCTOU; **reused unchanged** for the multi-line permutation write.
- [ADR-0005](./0005-surrogate-identity.md) — **not amended**; content-hash reconciliation already carries a task's surrogate id to its new line.
- [ADR-0006](./0006-note-content-cached-in-index.md) — the cached note hash that gates the write and makes a per-line `expected_char` unnecessary.
- [ADR-0009](./0009-scheduled-date-today.md) — the grammar-provability gate; **unchanged** (this ADR opens a separate gate).
- [ADR-0014](./0014-quick-add-inbox-creation.md) — the bounded append-only *creation* gate; this ADR **revokes its "reorder … remain rejected" clause** and opens a parallel structural gate.
- [ADR-0019](./0019-task-notes-annotation.md) — the bounded *annotation* gate; this ADR **revokes its "reordering remain rejected" clause**.
