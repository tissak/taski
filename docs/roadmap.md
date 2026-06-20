# Taski — Roadmap

*Status: Living document. Last updated: 2026-06-20.*
*Source: feature-gap analysis synthesizing the [Obsidian Tasks](https://publish.obsidian.md/tasks) plugin feature set, [`PRD.md`](./PRD.md) §14 non-goals, and the "Deferred / Intentionally Not Done" list in [`context.md`](./context.md). This file is the single view of "what's next"; it supersedes the scattered deferred/parking-lot notes.*

---

## How to read this

- Prioritized by **user value × fit with the architecture**, not by novelty.
- **Read-path items** (parser/index extensions) are cheap and carry **no write-back risk** — they extend `taski-core::parse_tasks` + the SQLite schema only. They're the natural place to add value.
- **Write-path items** touch the vault and must follow the [ADR-0009 pattern](./adr/0009-scheduled-date-today.md): a pure rewrite oracle in `taski-core`, a 256-case proptest, a new `pending_actions.action_type`, and a daemon dispatch branch. Each is marked **(needs ADR)**.
- Effort is relative: **S** ≈ a focused slice, **M** ≈ a multi-day slice, **L** ≈ a multi-slice feature.

## Where Taski is today (v0.4)

- **Reads:** every checkbox task in the vault, `📅`/`📆`/`🗓` due date, `⏳` scheduled date.
- **Writes:** checkbox toggle, `⏳` mark-for-today, checkbox↔bullet conversion, undo.
- **Views:** status cycle (`f`), Today (`T`), text search (`/`), file search (`F`), note context pane (`p`).

Taski parses **2 of ~15** Obsidian Tasks metadata tokens and has **no tag or priority awareness**. That is where the gaps — and most of the untapped value — concentrate.

---

## Tier 1 — Foundational metadata parsing (read-only, low effort, low risk)

Each item extends the parser and the `tasks` schema only. No vault writes, no ADR required. Every Tier 2 view and Tier 3 write depends on at least one of these.

| Item | Tokens | Unlocks | Effort | Depends on |
|---|---|---|---|---|
| **Tag parsing** | `#tag` in task text | tag filter, group-by-tag, project/context views — the #1 organizational axis in the Tasks ecosystem | S | — |
| **Priority parsing** | `⏫` `🔼` `🔽` `🔺` `⏬` | priority filter, urgency-score sort | S | — |
| **Start date** | `🛫` | "hide can't-start-yet" tasks — declutters daily views | S | — |
| **Created / done / cancelled dates** | `➕` `✅` `❌` | done-task review ("completed this week"), task age | S | — |

---

## Interop correctness gap (flag separately — bug-shaped)

**Toggling a task done via Taski does not stamp the `✅` done date** (nor `❌` on cancel). The Tasks plugin auto-writes these on completion, and Tasks queries like `done this month` depend on them — so **tasks completed in Taski are invisible to Tasks-plugin "done" queries in Obsidian.**

- **Fix:** extend the checkbox-toggle write to also stamp `✅ <today>` on `[ ]`→`[x]` (and `❌` if/when cancel is supported).
- **Effort:** M. It's a write-path change → **(needs ADR)**, but small and well-templated: a pure `rewrite_done_date` oracle + proptest, mirroring `rewrite_scheduled` (ADR-0009 Phase 2).
- **Why it ranks high:** it's correctness against the ecosystem Taski claims to interoperate with, not just a nicety.

---

## Tier 2 — Views the metadata unlocks (medium effort)

All read-path (TUI `build_view` extensions); land after their Tier 1 dependency exists.

| View | Gesture (proposed) | What it shows | Depends on |
|---|---|---|---|
| **Overdue** | `O` | tasks with `due_date < today`, sorted by how overdue | due date (have it) |
| **"Happens"** | (toggle) | start ∪ scheduled ∪ due — broader and more useful than Today | start date |
| **Group-by cycling** | `G` | cycle group axis: note → tag → priority → folder → date | tags / priority |
| **Urgency-score sort** | sort mode | composite (due proximity + priority) — the Tasks plugin's default ordering | priority |

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

1. **Tier 1 metadata parsing** — tags, priority, start, created/done/cancelled. Read-only, no risk, unlocks everything below.
2. **Tier 2 views** — overdue, happens, group-by, urgency sort (each as its dependency lands).
3. **The `✅`/`❌`-on-toggle interop fix** — small write-path slice, high correctness value.
4. **Bulk operations** — the highest-impact single feature; do after metadata is rich enough to act on.
5. **Recurrence write-back** — the viewer→manager leap; last because it's the hardest write.

Each write-path step gets its own ADR (the ADR-0009 template), pure oracle, and proptest before it ships.
