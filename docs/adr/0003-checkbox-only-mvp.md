# ADR-0003: MVP write-back is checkbox-state flips only

- **Status:** Accepted (amended 2026-06-20 by [ADR-0009](./0009-scheduled-date-today.md), 2026-06-21 by [ADR-0012](./0012-done-date-on-toggle.md), 2026-06-21 by [ADR-0013](./0013-cancelled-date-on-cancel.md), 2026-06-21 by [ADR-0014](./0014-quick-add-inbox-creation.md), 2026-06-23 by [ADR-0019](./0019-task-notes-annotation.md))
- **Date:** 2026-06-20
- **Decides:** PRD §10.2 — MVP write-back scope

## Context
Write-back is the project's top data-integrity risk. The full vision includes editing task text, creating/deleting tasks, and writing metadata back into notes — but each adds write-complexity and corruption surface. Shipping the riskiest, broadest write-back first would put the vault at maximal risk before the safety machinery is proven.

## Decision
The **MVP supports only checkbox-state flips**: changing the checkbox character (e.g., `- [ ]` ↔ `- [x]` ↔ `- [/]`). No task-text edits, no creates/deletes, no metadata writes from the TUI in the MVP.

## Rationale
- **Lowest-risk path to a working sync.** A byte-level flip of a single known character is the smallest possible mutation and the easiest to make conflict-safe.
- **Highest day-to-day value.** Toggling task completion is by far the most frequent action; it's the core of "act on tasks from one place."
- **Proves the write-back pipeline** (ADR-0002 routing + ADR-0004 conflict safety) on the simplest case before expanding surface area.

## Consequences
- ✅ Vault integrity risk is minimized while the safety layer is validated.
- ✅ The atomic-write + conflict-refusal machinery is exercised on real data early.
- ⚠️ Text edits, creates/deletes, and metadata write-back are deferred to fast-follow slices — out of MVP scope.
- This decision is intentionally easy to *expand* later without redesign.

## Alternatives considered
- **Full text + metadata write-back in MVP** — rejected; maximal corruption surface before safety is proven; not justified by value/risk trade-off.

## Amendment — ADR-0009 (2026-06-20): write-back scope widened to Obsidian-standard date-emoji metadata

[ADR-0009](./0009-scheduled-date-today.md) ("mark for today") introduces a scheduled-date
(`⏳ YYYY-MM-DD`) write gesture that this ADR originally excluded. The amendment is scoped
intentionally and recorded here so it does not become an open door:

> The write-back scope is widened from **checkbox-state flips only** to **checkbox-state
> flips + Obsidian-standard date-emoji metadata** (`⏳` scheduled). The original ADR-0003
> rationale still applies in full to everything else: **task-text edits, creates/deletes,
> and arbitrary metadata remain explicitly rejected.**

### Principled boundary (precedent control)

Once date-emoji writes are admitted, requests will follow for priority (`⏫`), recurrence
(`🔁`), tags, and free-text edits. Future amendments are **gated by grammar-provability, not
by precedent**:

> Taski may write tokens that are (i) are **standard Obsidian Tasks syntax**, (ii) have a
> **single unambiguous insertion grammar**, and (iii) are produced by a **pure, proptested
> line-rewrite** with a "never-corrupts" contract (the generalization of the existing
> `writeback_proptest`).

Free-text edits fail (ii)/(iii). Each new token type requires its own ADR.

### Why this does not relax ADR-0004 or ADR-0005

- **ADR-0004 (refuse-on-conflict)** is reused *unchanged*: `atomic_write`'s TOCTOU guard
  re-hashes the *whole file* and is already agnostic to whether the mutation was 1 byte or N.
  The new write path inherits identical conflict semantics.
- **ADR-0005 (no injected marker)** is *not crossed*: `⏳` is native Obsidian Tasks syntax
  (human-readable, consumed by Tasks/Dataview/Obsidian), not the foreign opaque identity
  marker (`%% taski:abc %%`) that ADR-0005 rejected. The surrogate-id + content-hash
  mechanism is untouched.

See ADR-0009 for the full design, the phased delivery, and the alternatives analysis.

## Amendment — ADR-0012 (2026-06-21): write-back scope widened to `✅` done-on-toggle

[ADR-0012](./0012-done-date-on-toggle.md) stamps `✅ <today>` on the same byte-buffer splice
that flips the checkbox `[ ]→[x]` — the done-date stamp is **composed into `process_action`**,
not a new action type. It also clears `✅` on `[x]→[ ]` (symmetry). The ADR-0009 principled
boundary is **unchanged**; `✅` is the second token admitted under it:

> The write-back scope is widened from **checkbox-state flips + `⏳` scheduled** to
> **checkbox-state flips + `⏳` scheduled + `✅` done (stamped on flip)**. The ADR-0009
> principled boundary is **unchanged**: Taski may write tokens that are (i) standard
> Obsidian Tasks syntax, (ii) have a single unambiguous insertion grammar, and (iii) are
> produced by a pure, proptested line-rewrite with a "never-corrupts" contract. Free-text
> edits, creates/deletes, and arbitrary metadata remain explicitly rejected. Each
> subsequent token type still requires its own ADR.

Unlike the `⏳` amendment (which added a new gesture + action type), this one does **not**
add a new action type, schema column, or TUI key — the stamp rides inside the existing
checkbox `pending_action`. `rewrite_done_date` is a one-line wrapper over a generalized
`rewrite_emoji_date` that also backs `rewrite_scheduled` (ADR-0009), guarded by its own
256-case proptest.

See ADR-0012 for the full design, the compose-vs-split rationale, the CRLF-hazard analysis,
and the alternatives.

## Amendment — ADR-0013 (2026-06-21): write-back scope widened to `❌` cancelled-on-cancel

[ADR-0013](./0013-cancelled-date-on-cancel.md) stamps `❌ <today>` on the same byte-buffer
splice that flips the checkbox `[ ]`→`[-]` (the new `d` "cancel" gesture) — the cancelled-date
stamp is **composed into `process_action`**, not a new action type, exactly parallel to how
ADR-0012 composes `✅` on `[ ]`→`[x]`. It also clears `✅`/`❌` symmetrically on cross-state
flips (done→cancelled clears `✅`; cancelled→done clears `❌`; either→open clears both). The
ADR-0009 principled boundary is **unchanged**; `❌` is the third token admitted under it:

> The write-back scope is widened from **checkbox-state flips + `⏳` scheduled + `✅` done
> (stamped on flip)** to **checkbox-state flips + `⏳` scheduled + `✅` done + `❌` cancelled
> (stamped on cancel flip)**. The ADR-0009 principled boundary is **unchanged**: Taski may
> write tokens that (i) are standard Obsidian Tasks syntax, (ii) have a single unambiguous
> insertion grammar, and (iii) are produced by a pure, proptested line-rewrite with a
> "never-corrupts" contract. Free-text edits, creates/deletes, and arbitrary metadata
> remain explicitly rejected. Each subsequent token type still requires its own ADR.

Like ADR-0012, this amendment does **not** add a new action type or schema change — the
stamp rides inside the existing checkbox `pending_action`, dispatched by the existing `d`
→ checkbox-flip-with-`new_char='-'` enqueue. `rewrite_cancelled_date` is a one-line wrapper
over the same generalized `rewrite_emoji_date` core that backs `rewrite_scheduled` (ADR-0009)
and `rewrite_done_date` (ADR-0012), guarded by its own 256-case proptest. Undo is free:
`u` already reverses checkbox flips (ADR-0011), and cancel *is* a checkbox flip.

`❌` is the third (and likely final) dated token admitted under the gate; the gate itself is
not widened. This closes the documented roadmap gap (*"`❌` cancelled-date is the next
candidate but depends on a cancel gesture that doesn't exist yet"*) — ADR-0013 is that gesture.

See ADR-0013 for the full design, the three-state stamp decision table, the hard-delete
alternative that was considered and rejected, and the edge cases.

## Amendment — ADR-0014 (2026-06-21): write-back scope widened to bounded append-only creation

[ADR-0014](./0014-quick-add-inbox-creation.md) ("quick-add") introduces the first **content
creation** feature — the `a` key opens a text-entry modal that appends
`- [ ] <text> ➕ <today>` to a designated inbox note (`task-inbox.md` by default, created if
missing). Unlike the prior three amendments (which admitted new *tokens* under the
grammar-provability gate), this amendment opens a **new gate class** — bounded append-only
creation — because quick-add writes arbitrary user text, which fails the grammar-provability
gate's "single insertion grammar" requirement. The new gate is narrower and structurally
distinct:

> The write-back scope is widened from **checkbox-state flips + `⏳`/`✅`/`❌` date-emoji
> stamps** to also include **bounded append-only task creation** to a designated inbox note
> (`quick_add` action). The ADR-0009 grammar-provability gate is **unchanged** and still
> governs token writes. A new, separate gate governs creation: Taski may append a
> well-formed checkbox-task line (with `➕ <today>` created-date stamp) to a designated inbox
> note, provided the operation is append-only (no modification / reorder / deletion of
> existing lines), uses the standard `atomic_write` TOCTOU discipline (or a first-creation
> path with no conflict surface), and re-indexes after write. Arbitrary-note creation,
> mid-note insertion, text editing, and line deletion remain explicitly rejected.

This amendment does **not** add a schema change (the existing `pending_actions` columns carry
sentinel values for unused fields) or amend ADR-0004/0005. The first-creation path (inbox file
does not exist yet) is a bounded, justified exception to ADR-0004's TOCTOU re-hash: a
non-existent file has no state to conflict with. Undo is extended: `u` after `a` removes the
appended line (the first content-removing undo, safe because the line is positionally and
contentually known).

See ADR-0014 for the full design, the new gate's boundary, the first-creation path rationale,
the undo semantics, and the alternatives.

## Amendment — ADR-0019 (2026-06-23): write-back scope widened to bounded task annotation

[ADR-0019](./0019-task-notes-annotation.md) ("task notes") introduces the first **annotation**
feature — the `n` key opens a single-line text-entry modal that appends a free-text note as a
bullet under a per-task `### notes-<id>` heading inside a single `## task-notes` section **in the
note the task already lives in**, and on the first note for that task inserts one aliased in-page
wikilink (`[[#notes-<id>|Notes]]`) into the task line. Unlike ADR-0014 (which opened the
append-only *creation* gate, scoped to a designated inbox and rejecting both arbitrary-note append
and existing-line text edits), this amendment opens a **second, parallel gate class** — bounded
task annotation — and deliberately crosses those two ADR-0014 exclusions under a narrower
justification (the target note is deterministic, not arbitrary; the line edit is a single bounded
idempotent link insertion, not free editing):

> The write-back scope is widened to also include **bounded task annotation** (`add_note`
> action): at the user's explicit request on an existing task, Taski may append a free-text note
> as a bullet under a per-task `### notes-<id>` heading inside a single `## task-notes` section in
> the note the task lives in, and on the first such note insert one aliased in-page wikilink
> (`[[#notes-<id>|Notes]]`) into the task line before its Tasks metadata. The ADR-0009
> grammar-provability gate and the ADR-0014 creation gate are **unchanged**. The new gate permits
> append-only note content plus a single bounded, idempotent link insertion, composed into one
> `atomic_write` under ADR-0004, gated by the ADR-0006 cached note hash. Editing other lines,
> mid-note insertion outside these rules, deletion, and reordering remain rejected.

This amendment does **not** add a schema change (the existing `pending_actions` columns carry
sentinel values for unused fields, per ADR-0014) and does **not** amend ADR-0004/0005/0006. No undo
in v1 (the user removes a note in Obsidian). See ADR-0019 for the full design, the new gate's
boundary, the heading/link scheme, the hash-gated identity argument, and the alternatives.

## References
- [`docs/tech.md`](../tech.md), [ADR-0002](./0002-write-back-through-daemon.md), [ADR-0004](./0004-refuse-on-conflict.md), [ADR-0009](./0009-scheduled-date-today.md) *(amendment)*, [ADR-0012](./0012-done-date-on-toggle.md) *(amendment)*, [ADR-0013](./0013-cancelled-date-on-cancel.md) *(amendment)*, [ADR-0014](./0014-quick-add-inbox-creation.md) *(amendment)*, [ADR-0019](./0019-task-notes-annotation.md) *(amendment)*
