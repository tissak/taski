# ADR-0022: Today view includes due-today (scheduled ∪ due == today)

- **Status:** Accepted
- **Date:** 2026-06-25
- **Decides:** The scope of the `T` "Today" view filter. **Amends [ADR-0009](./0009-scheduled-date-today.md)** Phase 1 §5 ("View scope: strict scheduled-today") only — a **read-path** change. It does **not** touch [ADR-0003](./0003-checkbox-only-mvp.md) or any write-back ADR: no write path, schema, daemon, or `pending_actions` change is involved.

## Context

ADR-0009 Phase 1 shipped the `T` Today view as **strict `scheduled_date == today`**, and
§5 explicitly excluded due-today and overdue:

> The Today view shows *only* `scheduled_date == today`. It does **not** include due-today
> or overdue (the broader `happens today` union). … A separate "due/overdue" view can be
> added later if wanted.

A later Tier 2 release added the `O` Overdue view (`due_date < today`), but it used
**strict less-than**. The consequence is a gap a user hits every day: **a task whose
`due_date == today` appears in *neither* view** — not Today (scheduled-only) and not
Overdue (strictly before today). It is invisible in both date-focused views on the very day
it is due, only surfacing in Overdue the *next* day (when it is already late). For a tool
whose purpose is "nothing falls through the cracks," due-today invisibility is the wrong
default.

The user's intent: *"if any of the scheduled or due dates on a task arrive, the task
automatically shows up in the today view."* "Arrive" was resolved (with the user) to mean
**exactly today**, not today-or-earlier — so overdue tasks do **not** roll up into Today.

## Decision

Widen the `T` Today view predicate from `scheduled_date == today` to:

> **`scheduled_date == today` OR `due_date == today`**

Implementation is a single closure in `taski-tui`'s `build_view` (renamed `scheduled_today`
→ `matches_today`); the `O` Overdue view (`due_date < today`) is **unchanged** and remains
a separate, orthogonal filter axis.

### Why strict equality (not `<=`), and why the two views stay disjoint

- **Today = "happening today"** (`scheduled == today` ∪ `due == today`).
- **Overdue = "past due"** (`due < today`).
- On the `due_date` axis these are **mutually exclusive** (a date cannot be both `== today`
  and `< today`), so no task can newly satisfy both filters via the due axis — the existing
  `overdue_only ⟂ today_only` orthogonality invariant ([context.md §4](../context.md)) is
  **preserved**. The `overdue_only_orthogonal_to_today_filter` test continues to pass
  unchanged (its fixtures use due dates ≠ today).
- Rejecting the `<= today` roll-up keeps "what should I do *today*" (Today) cleanly
  separable from "what did I miss" (Overdue). Folding overdue into Today would subsume the
  `O` view and erase a distinction the user opted to keep.

### Scope deliberately held (non-goals)

- **No write-path change.** The `t` mark-for-today gesture still writes `⏳ today`
  (ADR-0009 Phase 2). There is **no** `due_date == today` write gesture and none is added.
- **No schema bump, no daemon change, no new `action_type`.**
- **No new rendering affordance.** `due_date` already renders as a yellow `· <date>` suffix;
  the scheduled-date bold-cyan "today" highlight (ADR-0009 Phase 1 §4) is unchanged. A
  due-today task surfaces in Today by *membership*; its due date is already visible in the
  row. A dedicated due-today emphasis is a separate, deferrable render decision.
- **`🛫` start date is not added** to the Today predicate. The user scoped this to
  "scheduled or due"; start-date inclusion remains YAGNI (consistent with ADR-0009's
  original `🛫` deferral).
- **The full `happens today` union (including overdue / `<= today` roll-up) remains
  deferred** — this ADR adopts only the due-today half of it.

## Rationale

- **Closes a daily-hit visibility gap.** Due-today is the single most important day to see a
  deadline task; the pre-ADR-0022 behavior hid it on that day by design.
- **Symmetric with the existing due-date parsing.** `due_date` is already a first-class
  indexed column with its own view (`O`); admitting it to Today on equality is a one-clause
  extension of the same date-axis logic, not a new concept.
- **Read-path only → lowest-risk change class.** No ADR-0004 TOCTOU surface, no
  `atomic_write`, no `rewrite_*` oracle, no proptest obligation. The change is a pure filter
  predicate guarded by ordinary unit tests.
- **Composable, not folding.** Keeping Today and Overdue as two disjoint equality/range
  predicates preserves the five-axis AND-composition model in `build_view`.

## Consequences

- ✅ A task due today (with or without a scheduled date) now appears under `T` on its due
  date. A task scheduled today continues to appear (unchanged).
- ✅ `O` Overdue and `T` Today remain orthogonal and disjoint; existing composition tests
  (status × today × overdue × text × file) hold.
- ✅ Zero write-back / schema / daemon impact. CI gates unchanged.
- ⚠️ **ADR-0009 Phase 1 §5 is amended** (Today scope: scheduled-only → scheduled-or-due).
  ADR-0009's Phase 2 write path and its ADR-0003 amendment are **untouched**.
- ⚠️ **Reverses ADR-0009's deferred "`happens today` union" alternative** — but only the
  due-today half; the overdue-roll-up half stays deferred (see Non-goals).

## Alternatives considered

- **`<= today` roll-up (Today = scheduled ≤ today ∪ due ≤ today, for open tasks).** Matches
  "arrive" most literally and subsumes Overdue. **Rejected with the user:** it erases the
  Today/Overdue distinction the user chose to keep, and an ever-growing "today" list as
  undone tasks accumulate defeats the focus intent of the view.
- **Due-today-only (leave scheduled strict, add due-today).** Rejected as asymmetric: the
  user said "scheduled **or** due," and both should contribute symmetrically.
- **Full `happens today` union (scheduled ∪ start ∪ due == today) plus overdue.** The
  maximal Obsidian-Tasks "happens today" semantic. **Deferred:** adds `🛫` start-date
  semantics the user did not ask for and re-opens the overdue-roll-up question. ADR-0022
  takes only the increment that closes the real gap.
- **A brand-new `D` "due today" view instead of widening `T`.** Rejected: two near-identical
  date-equality views would confuse keybindings and split the "what's happening today"
  population for no benefit. One widened `T` is simpler.

## Edge cases

| Case | Behavior |
|---|---|
| Task has `due_date == today`, no scheduled date | **Appears in Today** (new). Also still subject to the status/search/file axes like any task. |
| Task has `scheduled_date == today`, no due date | Appears in Today (unchanged). |
| Task has both `scheduled == today` and `due == today` | Appears in Today once (it is one row); not double-counted. |
| Task has `due_date == yesterday` (overdue), not done | **Not** in Today; appears in `O` Overdue (unchanged). |
| Task has `due_date == tomorrow` | Not in Today, not overdue (unchanged). |
| `T` + `O` both on | AND-composes; a task must be (scheduled-or-due == today) **and** (due < today). On the due axis these are disjoint, so in practice this still matches the scheduled-today ∧ past-due case (the existing orthogonality fixture). |
| Session spans midnight | `today_string()` is refreshed each refresh tick (unchanged), so "today" rolls over correctly. |

## References

- [ADR-0009](./0009-scheduled-date-today.md) — **amended** (Phase 1 §5 Today scope widened
  from scheduled-only to scheduled-or-due == today). Phase 2 write path untouched.
- [context.md §4 — TUI Filter Composition](../context.md) — the five-axis AND model;
  Today and Overdue remain orthogonal, disjoint date predicates.
- `crates/taski-tui/src/lib.rs` — `build_view`'s `matches_today` closure; tests
  `today_only_also_keeps_due_today_tasks` and `today_only_keeps_both_scheduled_today_and_due_today`.
