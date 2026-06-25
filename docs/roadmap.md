# Taski — Roadmap

*Status: Living document. Last updated: 2026-06-21 (Tier 1 metadata parsing + Tier 2 views shipped; `✅` done-date-on-toggle interop fix shipped [ADR-0012]; 283 tests).*
*Source: feature-gap analysis synthesizing the [Obsidian Tasks](https://publish.obsidian.md/tasks) plugin feature set, [`PRD.md`](./PRD.md) §14 non-goals, and the "Deferred / Intentionally Not Done" list in [`context.md`](./context.md). This file is the single view of "what's next"; it supersedes the scattered deferred/parking-lot notes.*

---

## How to read this

- Prioritized by **user value × fit with the architecture**, not by novelty.
- **Read-path items** (parser/index extensions) are cheap and carry **no write-back risk** — they extend `taski-core::parse_tasks` + the SQLite schema only. They're the natural place to add value.
- **Write-path items** touch the vault and must follow the [ADR-0009 pattern](./adr/0009-scheduled-date-today.md): a pure rewrite oracle in `taski-core`, a 256-case proptest, a new `pending_actions.action_type`, and a daemon dispatch branch. Each is marked **(needs ADR)**.
- Effort is relative: **S** ≈ a focused slice, **M** ≈ a multi-day slice, **L** ≈ a multi-slice feature.

## Where Taski is today (v0.4 + Tier 1/2 + interop fix)

- **Reads:** every checkbox task in the vault, `📅`/`📆`/`🗓` due date, `⏳` scheduled date, `🛫` start date, `➕`/`✅`/`❌` created/done/cancelled dates, `#tags` (multi-value), `🔺`/`⏫`/`🔼`/`🔽`/`⏬` priority.
- **Writes:** checkbox toggle (stamps `✅ <today>` on done, clears on un-done — ADR-0012), `❌`-on-cancel (`d`, ADR-0013), `i` in-progress toggle (ADR-0016), `⏳` mark-for-today, checkbox↔bullet conversion, quick-add to inbox (`a`, ADR-0014), add-note (`n`, ADR-0019), reorder/move mode (`m`, ADR-0020), archive completed → archive note (`A`, ADR-0021), undo.
- **Views:** status cycle (`f`), Today (`T`), text search (`/`), file search (`F`), overdue (`O`), group-by cycling (`G`: note/tag/priority/folder), note context pane (`p`).

Taski now parses **8 of ~15** Obsidian Tasks metadata tokens (up from 2). The metadata is now surfaced as filters and groupings; remaining view-side gaps are the "Happens" date union and urgency-score sort (Tier 2), plus the write-path items (Tier 3).

---

## Tier 1 — Foundational metadata parsing (read-only, low effort, low risk) — ✅ SHIPPED

Each item extends the parser and the `tasks` schema only. No vault writes, no ADR required. Every Tier 2 view and Tier 3 write depends on at least one of these.

**Status: complete (schema v6).** All four items shipped in one read-path slice: six new `Task` fields (`tags`, `priority`, `start_date`, `created_date`, `done_date`, `cancelled_date`), six new `tasks` columns, four date extractors wrapping the existing `extract_emoji_date` primitive, plus custom `extract_priority` (first-match, UTF-8-safe) and `extract_tags` (Obsidian-core grammar, sentinel-padded TEXT storage). 218 tests pass. See `git log` for the `feat: parse Tier 1 metadata` commit.

| Item | Tokens | Unlocks | Effort | Depends on | Status |
|---|---|---|---|---|---|
| **Tag parsing** | `#tag` in task text | tag filter, group-by-tag, project/context views — the #1 organizational axis in the Tasks ecosystem | S | — | ✅ parsed + indexed |
| **Priority parsing** | `🔺` `⏫` `🔼` `🔽` `⏬` | priority filter, urgency-score sort | S | — | ✅ parsed + indexed |
| **Start date** | `🛫` | "hide can't-start-yet" tasks — declutters daily views | S | — | ✅ parsed + indexed |
| **Created / done / cancelled dates** | `➕` `✅` `❌` | done-task review ("completed this week"), task age | S | — | ✅ parsed + indexed |

> **Note:** parsing is the read path; the Tier 2 *views* (tag filter, group-by-tag, urgency sort, overdue/happens) and the `✅`-on-toggle interop fix are now shipped. Remaining Tier 2 views (happens, urgency sort) and `❌`-on-cancel remain open.

---

## Interop correctness gap — ✅ SHIPPED (ADR-0012)

**Toggling a task done via Taski now stamps the `✅` done date.** The Tasks plugin auto-writes
`✅ <completion-date>` on completion, and Tasks queries like `done this month` depend on it —
so tasks completed in Taski are now visible to Tasks-plugin "done" queries in Obsidian.

- **Shipped:** the `✅ <today>` stamp composes into the same byte buffer as the checkbox flip
  in `process_action_at` — one write, one hash, one rename. On `[ ]`→`[x]` the stamp is
  appended (or its date replaced); on `[x]`→`[ ]` the `✅` is removed (symmetry). Flips
  to/from in-progress (`/`) leave `✅` untouched. Malformed `✅` refuses the whole action.
  The pure `rewrite_done_date` oracle shares a generalized `rewrite_emoji_date` core with
  `rewrite_scheduled` (ADR-0009); two 256-case proptests guard the safety contract. See
  [ADR-0012](./adr/0012-done-date-on-toggle.md).
- **Still open:** `❌` cancelled-date stamping (depends on a cancel gesture that doesn't
  exist yet).

---

## Tier 2 — Views the metadata unlocks (medium effort) — partially shipped

All read-path (TUI `build_view` extensions). Two of four views are shipped; the rest remain open.

| View | Gesture | What it shows | Depends on | Status |
|---|---|---|---|---|
| **Overdue** | `O` | tasks with `due_date < today` (purely date-based; composes with status/today/search) | due date (have it) | ✅ shipped |
| **Group-by cycling** | `G` | cycle group axis: note → tag → priority → folder → note (tag fans out; date axis deferred) | tags / priority | ✅ shipped |
| **"Happens"** | (toggle) | start ∪ scheduled ∪ due — broader and more useful than Today | start date | open |
| **Urgency-score sort** | sort mode | composite (due proximity + priority) — the Tasks plugin's default ordering | priority | open |

> **Shipped notes:** Overdue is a 5th orthogonal filter axis (ANDs with all others). Group-by rewrote `build_view` from contiguous-note-run walking to HashMap bucketing (handles tag fan-out and arbitrary keys); a `Date` grouping axis is deferred (date-bucketing is fuzzy). Known minor limitations: under tag grouping, refresh restores the cursor to the first group instance of the selected task; the context pane blanks on non-Note headers (headers are dividers, not notes).

---

## Tier 3 — Transformative write features (higher effort, need ADRs)

| Feature | Why | Effort | Notes |
|---|---|---|---|
| **Bulk / multi-select operations** | The **single most-requested feature** across the Obsidian Tasks ecosystem (multi-select → "set scheduled/due to [date]"). Taski has no multi-select today. | L | New TUI selection model + a write-back path that fans out over N tasks. Each per-task write still routes through the existing `atomic_write` guard. **(needs ADR)** for the batch semantics. |
| **Recurrence write-back** | Parse `🔁 every …`, and on toggle write back the *next instance* with advanced dates. Transforms Taski from a viewer into a manager. | L | The hardest write: date arithmetic + variable-length line surgery + the `when done` vs from-due distinction. Follows the ADR-0009 pattern strictly. **(needs ADR)**. |

---

## Lower priority (real but niche)

- **Task dependencies** — parse `🆔` (id) and `⛔` (depends on); compute blocking/blocked; offer a "not blocked / actionable" filter. Only direct (non-transitive) dependencies exist in the ecosystem. Niche vs. the above.
- **Custom checkbox statuses** — Taski stores the raw checkbox char but only models `open`/`done`/`in_progress`. The Tasks plugin defines ON_HOLD / CANCELLED / NON_TASK types (`[!]`, `[D]`, `[-]`, `[~]`, …). Modeling these semantically would matter only if you adopt those statuses.
- **Dataview format reading** — support `[due:: 2023-04-07]` inline-field syntax alongside emoji, for vaults that use it. Compatibility, not capability.

---

## Explicitly deferred / out of scope (don't re-propose without new info)

Confirmed out of scope for a personal single-user tool by both the PRD (§14) and ecosystem research:

- Full GUI (web/native) — TUI only.
- Multi-vault support.
- Distribution / packaging / install UX.
- Collaboration / sharing / remote sync.
- Notifications / reminders — no clean local path; `⏰` belongs to the separate `obsidian-reminder` plugin.
- Deep write-integration with Dataview/Tasks beyond reading their syntax.

**Two re-triage flags** (lifted out of the parking lot by the ecosystem signal):

- **Tag/folder saved views** — previously deferred, but tags are *the* organizational axis and parsing them is cheap. Re-classified as Tier 1 above.
- **Saved/persistent filter sets** — natural once group-by + tags land; revisit after Tier 2.

---

## Recommended sequencing

Matches the project's vertical-slice philosophy and the architecture (read path is cheap; write path stays ADR-gated):

1. **~~Tier 1 metadata parsing~~** — tags, priority, start, created/done/cancelled. Read-only, no risk, unlocks everything below. **✅ Done (schema v6).**
2. **Tier 2 views** — overdue, happens, group-by, urgency sort (each as its dependency lands).
3. **~~The `✅`-on-toggle interop fix~~** — small write-path slice, high correctness value. **✅ Done (ADR-0012).** `❌`-on-cancel **✅ Done (ADR-0013)**.
4. **~~Inbox capture + annotation + within-note structure~~** — quick-add (ADR-0014), task notes (ADR-0019), reorder (ADR-0020), and **archive completed → archive note (`A`, ADR-0021)** — the inbox-triage workflow. **✅ Done.** Archival opened the fourth write gate class (bounded move by copy-then-delete; first line deletion + first cross-note op). Its **undo** is a recorded planned fast-follow (archival is invertible).
5. **Bulk operations** — the highest-impact single feature; do after metadata is rich enough to act on.
6. **Recurrence write-back** — the viewer→manager leap; last because it's the hardest write.

Each write-path step gets its own ADR (the ADR-0009 template), pure oracle, and proptest before it ships.
