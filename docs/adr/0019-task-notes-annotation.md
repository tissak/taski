# ADR-0019: Task notes — grouped closing notes under a `## task-notes` section with an aliased in-page link

- **Status:** Accepted
- **Date:** 2026-06-23
- **Decides:** How Taski lets the user attach free-text closing notes to an existing task from the TUI — an `n`-key single-line text-entry modal that appends the note as a bullet under a per-task `### notes-<id>` heading inside a single `## task-notes` section in **the note the task already lives in**, and (on the first note for that task) inserts one aliased in-page wikilink (`[[#notes-<id>|Notes]]`) into the task line so the task can jump to its notes. **Amends [ADR-0003](./0003-checkbox-only-mvp.md)** for a fifth time and opens a **second new gate class** — bounded task annotation — distinct from both the grammar-provability token gate (ADRs 0009/0012/0013) and the bounded append-only *creation* gate (ADR-0014).

## Context

Quick-add (ADR-0014) lets the user *capture* a new task without leaving the TUI, but every write-back feature still operates on tasks in isolation — there is no way to record a closing thought, a decision, or a reference next to a task without switching to Obsidian. The user's workflow wants exactly that: while acting on a task in the list, type a quick note ("went with the photographic hero — design approved it"), have it land in the source note, and leave a clickable link on the task so the note is one keypress away in Obsidian.

This is the first feature that both **appends free text to an arbitrary note** (the one the task lives in, not a designated inbox) and **edits an existing task line's description** (inserting the link). ADR-0014 explicitly rejected *both* of those — "arbitrary-note creation, mid-note insertion, text editing, and line deletion remain explicitly rejected." So this ADR does not extend ADR-0014's creation gate; it opens a **new, parallel gate** with its own principled boundary, and it must justify crossing the two ADR-0014 exclusions it touches.

### Why this needs a third gate, not an existing one

- **The grammar-provability gate (ADR-0009)** admits standard Tasks tokens with a single insertion grammar produced by a pure proptested rewrite. A free-text note has no single insertion grammar — it fails clause (ii), exactly as quick-add's arbitrary text did. Not applicable.
- **The bounded append-only creation gate (ADR-0014)** is scoped to appending a *well-formed checkbox-task line* to a *designated inbox note*. Task notes append *prose* to *the task's own note*, and additionally *edit the task line*. Both fall outside that gate's boundary.

So this ADR opens a **bounded task-annotation gate**:

> Taski may, at the user's explicit request on a specific existing task, (a) append a free-text note as a Markdown bullet under a **per-task heading** (`### notes-<id>`) inside a single `## task-notes` section **in the note that task already lives in**, and (b) on the *first* note for that task, insert exactly one **aliased in-page wikilink** (`[[#notes-<id>|Notes]]`) into that task line's description, before any trailing Tasks-plugin metadata — provided that: (i) note content is **append-only** (the `## task-notes` section and a `### notes-<id>` heading are created if absent; subsequent notes for the task are appended at the end of that task's existing heading block); (ii) the **only** edit to an existing line is the one-time, idempotent insertion of the aliased link into the target task's description; (iii) both spans are composed into a **single `atomic_write`** under the ADR-0004 TOCTOU discipline; and (iv) the write proceeds only when the note's on-disk content hash still equals the hash cached at index time (ADR-0006), which guarantees the target task line is byte-identical to what was indexed. Editing any other line, mid-note insertion outside the rules above, deletion, and reordering remain explicitly rejected.

### Why crossing the two ADR-0014 exclusions is acceptable here

- **"Arbitrary-note" append.** ADR-0014 rejected arbitrary-note *creation* to keep the capture surface narrow and the target unambiguous. Task notes do not create or target an arbitrary note chosen by the user — the target is **deterministically the note the selected task lives in**, the one place the note belongs. There is no ambiguity and no new file-discovery surface.
- **"Text editing" of an existing line.** ADR-0014 rejected editing existing-line text because arbitrary edits shift content unpredictably and have no provable grammar. The edit here is far narrower: a **single, bounded, idempotent insertion** of a fixed-shape aliased wikilink at one well-defined position (immediately before the first Tasks metadata emoji, or end-of-line if none), produced by a pure rewrite and re-verified against the cached file hash. It never modifies the user's existing description bytes — it only inserts.

## Decision

### The heading / link scheme

- One `## task-notes` section per **note** (file), created at EOF if absent.
- One `### notes-<id>` heading per **task** that has notes, where `<id>` is a write-time-generated token unique within the file (Unix-epoch milliseconds; if `### notes-<id>` already exists, increment until unique). The `<id>` is opaque-but-stable: once written it is literal text in the file and never recomputed.
- Each note is a Markdown bullet (`- <text>`) appended under that heading — a **running list** of notes for the task.
- The task line carries exactly one **aliased** link, `[[#notes-<id>|Notes]]`, inserted on the first note. The alias hides the opaque anchor and renders as "Notes"; the user clicks it in Obsidian to jump to the heading.

Worked example — task in `projects/website.md`, after two notes:

```
- [ ] Redesign the landing page [[#notes-1719153000123|Notes]] ⏳ 2026-06-25

## task-notes

### notes-1719153000123
- Went with the photographic hero.
- Design approved it on the 23rd.
```

### The action model

A new `add_note` action_type in `pending_actions`, enqueued by the TUI when the user commits the modal. **No schema change** — the existing NOT NULL columns carry sentinel values for unused fields, consistent with ADR-0014's established pattern:

| Column | Value for `add_note` | Rationale |
|---|---|---|
| `task_id` | `<task.id>` | The target task's surrogate id (ADR-0005). |
| `note_path` | `<task note, vault-relative>` | The note to append into / edit. |
| `line_number` | `<task line, 1-based>` | The task line to receive the link. |
| `expected_char` | `''` | Unused (verification is via cached file hash, not a single char). |
| `new_char` | `''` | Unused. |
| `action_type` | `'add_note'` | Dispatch key. |
| `payload` | `<note text>` | The user-typed note. |

### Why no per-line `expected_char` is needed

Checkbox flips re-verify a single byte because they target one character. Task notes need the whole task line to be trustworthy (to locate the link insertion point). That guarantee comes for free from ADR-0006 + ADR-0004: the daemon reads the note, computes its content hash, and compares to the hash cached in the index. **If the hashes match, the entire file — including the task line at `line_number` — is byte-identical to what was indexed**, so `line_number` is valid and the line's bytes are known. If they differ, the note changed since indexing → refuse (it will be re-indexed and the user can retry). A per-line char check would be strictly weaker than the file-level guard already required, so it is omitted.

### The daemon write path

`process_add_note(action)` — a single read, a single `atomic_write`:

1. Resolve the note: `vault_root.join(&action.note_path)`.
2. Read it; if missing → `TaskNotFound`. Compute `current_hash`; compare to the task's cached note hash (ADR-0006). On mismatch → `ConflictNoteChanged` (refuse).
3. Confirm the line at `line_number` parses as a checkbox task line (`parse_task_line`). On mismatch → refuse (defensive; the hash guard should already imply it).
4. Determine first-vs-append by inspecting that task line for an existing `[[#notes-<id>|…]]` link (the daemon owns this decision — never the TUI):
   - **First note** → generate a unique `<id>`; if `## task-notes` is absent, append it at EOF; append `### notes-<id>` and the `- <text>` bullet at EOF; insert ` [[#notes-<id>|Notes]]` into the task line via the pure rewrite (see below).
   - **Subsequent note** → read `<id>` from the existing link; locate `### notes-<id>`; append the `- <text>` bullet at the **end of that heading's block** (bounded by the next line beginning with `#`, or EOF). No line edit.
5. Compose all edits into one new buffer; `atomic_write(note, new_bytes, current_hash)` — the full ADR-0004 TOCTOU guard, unchanged. `Conflict` → `ConflictNoteChanged`.
6. Re-index the note. The `## task-notes` prose is not a task (verified: `task_captures` recognizes only `- [x]` checkbox lines, not plain bullets), so no phantom tasks appear; the task line's new link is preserved as description text.

### The pure oracle (link insertion)

A pure, proptested line rewrite — the only existing-line edit:

```rust
/// Insert ` [[#notes-<id>|Notes]]` into a task line's description, immediately
/// before the first Tasks-plugin metadata emoji (or at end-of-line if none).
/// Idempotent: if the line already contains `[[#notes-<id>|`, returns it unchanged.
pub fn insert_notes_link(line: &str, id: &str) -> String { /* ... */ }
```

The insertion point reuses the existing emoji-span scanner (`find_emoji_date_span` / the metadata-emoji set) so the link lands in the description, before `⏳`/`📅`/`✅`/`❌`/`➕`/priority emojis — matching the Tasks-plugin grammar (description first, metadata last). The note-bullet construction is a trivial `format!("- {text}")` oracle (like `inbox_line_for`), proptested for the checkbox-escape edge below.

### The TUI gesture

New `n` key opens a single-line text-entry modal (`adding_note` state, mirroring quick-add's `quick_adding`):

- `n` → enter modal (footer shows a `> ` prompt).
- Characters accumulate in `note_query` (no `rebuild()` — no filter to recompute).
- `Enter` → `submit_add_note(conn)`: enqueues the `add_note` action for the selected task; exits modal. Empty input refuses at the TUI layer (no enqueue).
- `Esc` → cancel. `Backspace` → pop char.
- All normal-mode keys suppressed during the modal (same `else if` branch pattern as search / quick-add).

### Out of scope for v1 (deliberately deferred)

- **Undo.** Quick-add's undo works because the appended line is the last line and is content-known. A task note may sit mid-file under a heading, and the append-text case cannot be reliably distinguished from user edits. Undo is **not** implemented; the user removes a note in Obsidian. (See "Known accepted risk" below.)
- **Configurable section name.** `## task-notes` is hardcoded. A config knob is a fast-follow if needed.
- **Context-pane rendering of the link.** The `[[#notes-<id>|Notes]]` link shows as raw text in the TUI body for now; pretty-rendering or hiding it is cosmetic.

## Known accepted risk — append is not replay-idempotent

If the daemon writes successfully but crashes before marking the action `done`, on restart it re-runs and appends the note bullet a second time. This is the **same risk quick-add already carries** (ADR-0014) and is not a regression. The first-note *link insertion* is idempotent (the oracle no-ops if the link is present), but the *bullet append* is not (the daemon cannot tell its own prior text from a user edit). Accepted for a personal tool; documented rather than fixed.

## Rationale

- **The target note is deterministic, not arbitrary.** Notes land in the task's own note — the one place they belong — so this does not reopen the arbitrary-note surface ADR-0014 closed.
- **The line edit is a bounded insertion, not free editing.** One fixed-shape aliased link, one well-defined position, a pure idempotent rewrite, re-verified by the cached file hash. The user's existing description bytes are never modified.
- **Two spans, one atomic write.** Because the task and its `## task-notes` section share a file, the link insertion and the note append are one `atomic_write` against one hash — no new race surface, ADR-0004 reused verbatim.
- **The daemon owns first-vs-append and `<id>` assignment.** Computed at execution time from the file (single writer, serial drain, re-read per action), so concurrent `n` presses on different tasks cannot collide on an id or mis-decide first-vs-append. The file is the single source of truth; the TUI only supplies task identity and text.
- **Hash-gated identity, no schema bump.** ADR-0006's cached note hash plus ADR-0004's TOCTOU guard make a per-line `expected_char` unnecessary; the existing `pending_actions` columns carry sentinel values, consistent with ADR-0014.
- **Grouped, aliased, clickable.** The user prioritized "click the link and see the note" over structural tidiness; one heading per task with a running bullet list and a single aliased link delivers exactly that.

### Why this does not amend ADR-0005

ADR-0005 rejected injecting a **foreign, opaque identity marker** (`%% taski:abc %%`) into notes purely for the tool's own task-identity bookkeeping. The `[[#notes-<id>|Notes]]` link is different in kind: it is **native Obsidian internal-link syntax**, **user-facing and useful** (a clickable jump the user explicitly requested by pressing `n`), and its `<id>` identifies a **note heading**, not the task. Task identity reconciliation remains surrogate-id + content-hash, untouched. The daemon does read the link to decide first-vs-append, but that is a navigational/structural signal derived from native content, not a replacement for identity reconciliation. **ADR-0005 is not amended.**

## Consequences

- ✅ The user can attach closing notes to a task from the TUI; the source note gains a grouped, clickable record.
- ✅ `atomic_write` and ADR-0004 are reused unchanged; both spans commit in one atomic write.
- ✅ No schema bump (sentinel values in existing columns, per ADR-0014's pattern).
- ✅ ADR-0005 is not crossed (native wikilink, user-requested, navigational).
- ✅ No phantom tasks: plain `- ` bullets under `## task-notes` are not parsed as tasks (only `- [x]` is).
- ⚠️ **ADR-0003 is amended a fifth time**: write-back scope widens to include **bounded task annotation** — a new gate class parallel to (not an extension of) the ADR-0014 creation gate. The grammar-provability gate is unchanged.
- ⚠️ **First existing-line *text* edit.** This is the first feature to insert text into an existing task line's description (prior line edits touched only the checkbox char, date emojis, and bullet format). Bounded to a single idempotent aliased-link insertion.
- ⚠️ **Append is not replay-idempotent** (see above) — bounded, matches quick-add, documented.
- ⚠️ The `n` key is consumed for add-note. No undo for v1.
- ⚠️ `## task-notes` is hardcoded; a new `### notes-<id>` heading appended at EOF when `## task-notes` sits mid-file may land structurally after a later section, though the link still resolves (heading links are position-independent). Accepted per the user's "click-to-see over structure" priority.

### Cross-reference note — ADR-0003 (fifth amendment)

ADR-0003's amendment block records a **fifth** amendment, opening a second new gate class:

> The write-back scope is widened to also include **bounded task annotation** (`add_note` action): at the user's explicit request on an existing task, Taski may append a free-text note as a bullet under a per-task `### notes-<id>` heading inside a single `## task-notes` section **in the note the task lives in**, and on the first such note insert one aliased in-page wikilink (`[[#notes-<id>|Notes]]`) into the task line before its Tasks metadata. The ADR-0009 grammar-provability gate and the ADR-0014 creation gate are **unchanged**. The new gate permits append-only note content plus a single bounded, idempotent link insertion, composed into one `atomic_write` under ADR-0004, gated by the ADR-0006 cached note hash. Editing other lines, mid-note insertion outside these rules, deletion, and reordering remain rejected.

## Alternatives considered

- **Pure EOF, one `### notes-<id>` block per note (no heading lookup).** Zero file parsing, but one task accumulates multiple "Notes" links and its notes scatter down the file. Rejected at user direction in favor of grouped-under-one-heading; the only added cost is a linear find of the task's heading (no level-aware nesting).
- **Heading-per-note with a page-level counter (`Notes-1`, `Notes-2`).** Rejected: the user does not need readable or sequential anchors, only unique-and-stable ones; a write-time millisecond id is simpler and collision-free.
- **Plain (non-aliased) link `[[#notes-<id>]]`.** Rejected: the opaque id would render as visible noise on the task line; the alias (`|Notes`) hides it.
- **Block reference (`[[#^blockid]]`) instead of a heading link.** Rejected: heading links read naturally, group notes visibly under a heading, and match the user's mental model; block refs add an opaque `^id` to the note body for no gain here.
- **Multi-line note entry.** Deferred: single-line is simpler and matches "quick thoughts / closing notes." Multi-line can be layered on later (the heading already accumulates a running list).
- **Undo.** Deferred (see Out of scope): mid-file, non-EOF append cannot be reversed as cleanly as quick-add's last-line removal.
- **A `pending_notes` table / schema v-next.** Deferred: the sentinel-column pattern is adequate, consistent with ADR-0014, and avoids a migration.

## Edge cases

| Case | Behavior |
|---|---|
| First note, `## task-notes` absent | Append `## task-notes`, `### notes-<id>`, and `- <text>` at EOF; insert aliased link into the task line. |
| First note, `## task-notes` present (from another task) | Append a new `### notes-<id>` + `- <text>` at EOF; insert link. (New heading may land after a later section if `## task-notes` is mid-file — link still resolves.) |
| Subsequent note (task already has a link) | Read `<id>` from the link; append `- <text>` at the end of `### notes-<id>`'s block (next `#`-heading or EOF). No line edit. |
| Note text itself forms a checkbox (`- [ ] foo`) | Escape so it cannot be indexed as a task: write `- \[ ] foo` (the parser requires a literal `[`; an escaped bracket is inert). Documented; the only path by which a note could become a phantom task. |
| Note text contains an emoji date (`✅`, `📅`, …) | Preserved verbatim in the bullet (prose, not a task line) — not parsed as metadata since the bullet is not a checkbox. |
| Task line has no Tasks metadata | Link appended at end-of-line (after trimming trailing whitespace). |
| Task line already has the link, replay/double-press | `insert_notes_link` no-ops on the line; bullet append still runs (not idempotent — see Known accepted risk). |
| Note's content hash changed since index (Obsidian edit) | `ConflictNoteChanged` → refuse; surfaced to the TUI; user retries after re-index. |
| Note file missing | `TaskNotFound`. |
| Empty input (`Enter` on empty modal) | Refuse at the TUI layer; no enqueue. |
| Embedded newline in input | Stripped (single-line only). |
| `### notes-<id>` collision on generation | Increment `<id>` until unique within the file. |
| `### notes-<id>` missing though the link is present (user deleted heading) | Treat as first-note for the section: re-create `### notes-<id>` at EOF and append; link already present, so no second link. |

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; `add_note` routes through `pending_actions` like all writes.
- [ADR-0003](./0003-checkbox-only-mvp.md) — **amended a fifth time** by this ADR (second new gate class: bounded task annotation).
- [ADR-0004](./0004-refuse-on-conflict.md) — refuse-on-conflict / TOCTOU; **reused unchanged** for the two-span single write.
- [ADR-0005](./0005-surrogate-identity.md) — **not amended**; the aliased wikilink is native, user-requested, navigational — not an opaque identity marker.
- [ADR-0006](./0006-note-content-cached-in-index.md) — the cached note hash that gates the write and makes a per-line `expected_char` unnecessary.
- [ADR-0009](./0009-scheduled-date-today.md) — the grammar-provability gate; **unchanged** (this ADR opens a separate gate).
- [ADR-0014](./0014-quick-add-inbox-creation.md) — the bounded append-only *creation* gate; this ADR is a **parallel** gate (annotation), and crosses ADR-0014's "arbitrary-note append" and "existing-line text edit" exclusions under the narrower justification above.
