# ADR-0005: Stable task identity via surrogate rowid + content-hash reconciliation

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** PRD §9 (data model — task identity) and PRD §11 risk "Task identity drift after note edits/moves"

## Context

Through Slice 3, tasks have no stable identity across re-scans. The `tasks.id` is derived as `hash(note_path|line_number|text)` (`taski-core/src/lib.rs:116`), and `index_note` reconciles by deleting all rows for a note and re-inserting (`taski-daemon/src/lib.rs:100-114`). This was adequate for the read-only slices but is now a structural blocker:

- **Slice 3 write-back** records a `task_id` in `pending_actions` and locates the target by `note_path + line_number`. If the task's line shifts (a line added/removed above it) between enqueue and execution, the flip targets the wrong line or refuses. Identity does not survive line shifts.
- **Slice 4 move/delete handling** must distinguish "task moved within a note" from "task deleted" from "task is new" — impossible when every re-scan discards all row continuity.
- **Future** completion history, "completed today" views, and stable TUI↔vault references all depend on a task retaining its identity across re-scans.

The identity scheme must survive four events: (a) the user editing task text in Obsidian, (b) line shifts when lines are added/removed above the task, (c) the note being renamed/moved on disk, and (d) the task being deleted. It must coexist with refuse-on-conflict write-back (ADR-0004) and must not weaken the byte-level re-verification that gates every vault mutation.

## Decision

**Separate identity from content. Use a surrogate primary key plus per-note content-hash reconciliation. No marker injection into the vault.**

1. **`tasks.id` becomes `INTEGER PRIMARY KEY AUTOINCREMENT`** — a surrogate assigned once by SQLite at INSERT, never changed, never reused after delete (`AUTOINCREMENT` guarantees this). This is the stable identity. `pending_actions.task_id` references this rowid.

2. **Re-scan reconciliation by `text_hash`** replaces the current delete-all + re-insert. On re-scan of a note, old rows are matched to freshly-parsed tasks by `text_hash` (greedy, in line order within the note). Matched rows are UPDATEd in place (preserving their rowid); their `line_number`, `status`, `raw_checkbox_char`, `note_hash`, and `updated_at` are refreshed. Unmatched old rows are deleted; unmatched new parses are inserted.

3. **The "same task" predicate** is: same `note_path` AND same `text_hash`. Line number, checkbox state, and note hash are mutable attributes updated on match — they are not part of identity.

4. **`process_action` targets the task row's *current* `line_number`** (updated by reconciliation if the task moved), not the stale `line_number` captured at enqueue time. The byte-level re-verification (ADR-0004) is unchanged — the on-disk checkbox char is still verified before any flip.

5. **Cross-note moves (note rename / cut-paste between notes) are deferred.** A note rename is treated as delete-all-old + insert-all-new. This loses history across renames but is safe (no corruption) and well-bounded for future enhancement.

6. **No marker is injected into the vault.** The scheme derives continuity from content that already exists (task text), preserving Obsidian's position as the pristine source of truth.

## Rationale

- **Identity and content are independent concerns.** Deriving identity from content (the current `hash(path|line|text)`) couples them: any line shift or text edit destroys identity. A surrogate rowid decouples them completely — identity is stable by construction; content is used only as a heuristic for *matching* during reconciliation, where a false match is caught by the byte re-verify and a false non-match is mere history loss.
- **Sufficient for the MVP's actual needs.** Line shifts (the common case: adding a task above existing ones) are handled perfectly by text-hash matching — the task's text is unchanged, so it matches and its rowid survives, and its `line_number` is updated. Checkbox flips (Taski's only write) don't change text, so identity is stable across them. Text edits produce a "delete + new," which is safe (pending flips refuse on `TaskNotFound`) and acceptable under ADR-0003.
- **Does not weaken ADR-0004.** The byte-level re-verification in `process_action` is completely unchanged. Identity only changes *which line* is targeted (the current one, not the stale one) — making the write-back more correct, not less safe. The `note_hash` conflict check still catches any note change and refuses.
- **Simpler than the alternatives.** No fuzzy positional matching (heuristic, fragile), no vault injection (one-way door, mass mutation risk, conflicts with the "pristine vault" ethos in `idea.md`), no cross-note global matching (aliasing risk). Each of those can be added later if a concrete feature demands it.
- **`AUTOINCREMENT` prevents rowid reuse**, so a `pending_actions` row referencing a deleted task can never accidentally point at a different task that reused the id.

## Prior art

Validated against external prior art (research notes, 2026-06-20):

- **Obsidian Tasks plugin** (3.8k★, production for years) uses **content matching** — not injected IDs — as its core identity: a 3-tier `findLineNumberOfTaskToToggle()` (exact-line → unique-content-match → section fallback). It has no persistent task id; its `TaskLocation` goes stale on edits and it retries up to 10× on mismatch. Its known pain points (identical task lines, stale line numbers) are *also* handled by our per-note greedy matching + durable surrogate rowid. Its opt-in `🆔` emoji field is for *dependency* tracking only, not identity, and is stripped from recurring tasks on completion.
- **Logseq** auto-injects `id:: <uuid>` properties — and suffers documented corruption from it: UUIDs regenerated on sync (#10814), duplicate-UUID file overwrites (#5393), and strong community pushback ("makes my Markdown files unreadable outside Logseq"). Validates *rejecting* injection.
- **Dataview** has no identity layer (re-parses every query; checkbox toggle fails silently on text edit). Community accepts human-readable inline metadata (`[key:: value]`, dates, tags) but treats **opaque machine-only IDs as pollution**.
- **Net:** the two viable approaches in the file-based-Markdown space are content-matching (validated by Obsidian Tasks) or injection (validated-as-painful by Logseq). No third technique exists. Our surrogate rowid + explicit re-scan reconciliation is strictly stronger than Obsidian Tasks because it adds a durable handle and deterministic re-scan.

## Consequences

- ✅ A task retains its identity across line shifts and checkbox flips — the two most common re-scan scenarios. Pending write-back actions survive these changes and target the correct (current) line.
- ✅ Slice 4 can classify tasks as kept / moved / deleted / new by inspecting which rows matched during reconciliation — no new machinery needed.
- ✅ The byte-level write-back safety (ADR-0004) is strengthened, not weakened: the flip targets the task's current location, and the existing byte + note_hash re-verification is intact.
- ✅ The vault remains pristine — no injected markers, no mass mutations, no migration. Obsidian stays the sole editor of task text.
- ⚠️ **Text edits break continuity.** Editing a task's text in Obsidian causes the old row to be deleted and a new one inserted. Any pending flip on the old rowid is refused (`TaskNotFound`). This is safe but means the user must re-toggle after editing text. Acceptable under ADR-0003 (Taski never edits text; text edits are user-initiated).
- ⚠️ **Cross-note moves lose history.** Renaming a note or cut-pasting tasks between notes produces delete-old + insert-new. A future slice can add note-level Jaccard-similarity rename detection.
- ⚠️ **Schema migration required.** `tasks.id` changes from `TEXT` to `INTEGER`; `pending_actions.task_id` likewise. Pre-MVP, this is a drop-and-recreate (gated by `PRAGMA user_version = 2`).
- ⚠️ Duplicate tasks (identical text, same note) are disambiguated by greedy in-order matching. Correct unless duplicates are reordered between scans (rare); history-only impact, no safety risk.

## Alternatives considered

- **Injected stable marker (`%% taski:abc %%` HTML comment).** The most robust option in principle — survives text edits, line shifts, renames, and deletions with zero false positives. Rejected for MVP because: (1) it conflicts with `idea.md`'s "no migration, no duplication — just a powerful lens" ethos; (2) the bootstrap write (injecting into all existing tasks) is a mass vault mutation — the scariest operation for a data-integrity tool and a one-way door (Logseq's experience confirms injection causes real-world corruption and user hostility); (3) it complicates the checkbox-only write-back (parser must find/extract/preserve the marker; new Obsidian-created tasks are marker-less until scan); (4) the MVP doesn't need it — content-hash matching handles line shifts perfectly. **Can be added later as an opt-in enhancement** if completion history or cross-note tracking demands it, by which point write-back will be proven enough for a safe bootstrap.

- **Pure content hash as primary key** (`id = hash(text)`). Survives line shifts and checkbox flips; breaks on any text edit (the most common user action after the one Taski performs). Also collides on duplicate tasks. Insufficient alone.

- **Composite key (note_path + content-hash + line-proximity) with fuzzy relocation.** Heuristic positional matching for edited lines. Fragile — produces false "same" (aliasing) when a task is deleted and a different task slides into its line slot, and false "different" (history loss) when a task is edited and shifts position. The aliasing risk is particularly dangerous for write-back correctness, though the byte re-verify would catch it. The complexity is not justified for the MVP's needs.

- **Global content-hash matching across all notes (for cross-note moves).** Aliases identical tasks in different notes ("buy milk" in the groceries note vs. the daily note). Correct cross-note move detection requires note-level set comparison (Jaccard similarity), not per-task global matching. Deferred to a future slice.

## References

- [`docs/tech.md`](../tech.md) — `DefaultHasher` (SipHash) used for `text_hash`; stable within a process run.
- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; identity is a daemon-internal concern.
- [ADR-0003](./0003-checkbox-only-mvp.md) — Taski never edits task text; text edits are user-initiated, justifying the "text edit = new task" trade-off.
- [ADR-0004](./0004-refuse-on-conflict.md) — byte-level re-verification is unchanged; identity adds correct line targeting on top.
- Prior art: Obsidian Tasks plugin `File.ts` (`findLineNumberOfTaskToToggle`); Logseq issues #10814 / #5393; Obsidian Dataview issue #523.
