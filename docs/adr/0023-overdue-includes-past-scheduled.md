# ADR-0023: Overdue view includes past-scheduled (due OR scheduled < today)

- **Status:** Accepted
- **Date:** 2026-09-07
- **Decides:** The scope of the `O` Overdue view filter. **Reverses the deferred
  "overdue roll-up" non-goal of [ADR-0022](./0022-today-view-includes-due-today.md)** — a
  **read-path** change. It does **not** touch [ADR-0003](./0003-checkbox-only-mvp.md) or
  any write-back ADR: no write path, schema, daemon, or `pending_actions` change is
  involved.

## Context

ADR-0022 kept the `O` Overdue view as strict `due_date < today` and explicitly deferred
the overdue roll-up. Daily use exposed the gap: tasks triaged with the `t` mark-for-today
gesture (or any `⏳` scheduled date) carry **no** `📅` due date, so when the day passes
unfinished they vanish from the `T` Today view (strict `==`) and **never appear in `O`**.
The task silently disappears from both date-focused views — exactly the
"nothing falls through the cracks" failure the views exist to prevent. Reported
first-hand: tasks marked for a day, left unfinished, and expected in Overdue were simply
gone.

The root cause is that Overdue looked at only one of the two date axes Taski indexes.
Scheduled-date lateness is real lateness for a personal execution tool: "I planned to do
this Tuesday" being un-done on Wednesday is an overdue commitment whether or not a
deadline emoji is present.

## Decision

Widen the `O` Overdue predicate from `due_date < today` to:

> **`due_date < today` OR `scheduled_date < today`**

Implementation is the `not_overdue` closure in `taski-tui`'s `build_view`; the `T` Today
view (`scheduled == today` OR `due == today`, ADR-0022) is **unchanged** and the two
filters stay separate, orthogonal axes.

### Why this is not the roll-up ADR-0022 rejected

ADR-0022 rejected folding overdue **into Today** (`T` becomes `<= today`), which would
subsume `O` and grow Today without bound. This ADR widens **Overdue**, the "what did I
miss" view — the one place unfinished past commitments belong. Today stays strict-`==`
and focused; Overdue absorbs the residue. The ADR-0022 rationale ("keep 'what should I do
today' cleanly separable from 'what did I miss'") is preserved, arguably strengthened.

### Orthogonality invariant holds

Today matches equality on a date axis; Overdue matches strictly-less on the same axes. On
each single axis the two are mutually exclusive, so `T`+`O` can only match **cross-axis**:
past-due ∧ scheduled-today (pre-existing case), or past-scheduled ∧ due-today (new mirror
case). `overdue_only ⟂ today_only` is preserved; the fixture now pins both cross cases.

## Scope deliberately held (non-goals)

- **No status coupling.** Overdue remains purely date-based; a done task with a past
  scheduled date still shows under `O`+Done (completed-was-overdue review, now including
  the scheduled axis).
- **`🛫` start date is not added** to the Overdue predicate. A future start date is not a
  commitment; a past one is not unambiguously "missed" (the task may simply never have
  been started deliberately). YAGNI — consistent with ADR-0009/0022's start-date
  deferrals.
- **No write-path change, no schema bump, no daemon change, no new `action_type`.**
- **No rendering change.** The overdue indicator, grouping, and row rendering are
  untouched; membership is the only difference.

## Consequences

- ✅ An unfinished task marked for a past day (`⏳ <past>`, no due date) now appears under
  `O` instead of vanishing.
- ✅ `T` Today stays strict-`==` (scheduled ∪ due == today, ADR-0022 unchanged).
- ✅ `T`+`O` composition still meaningful; the orthogonality fixture pins both cross
  cases.
- ⚠️ **Reverses ADR-0022's deferred "overdue roll-up / `<= today`" non-goal** — the roll-up
  is adopted on the *Overdue* side only. Today-side roll-up remains rejected.
- ⚠️ Overdue lists grow relative to due-only behavior for users who triage via `t` without
  due dates — that is the point; clear the backlog or reschedule with `t` (pressing `t`
  on a past-scheduled task marks it today, which also lifts it out of Overdue).

## Alternatives considered

- **Leave Overdue due-only; add a third view for past-scheduled.** Rejected: two
  near-identical "past" views would split the "what did I miss" population and burn a
  keybinding for no conceptual gain.
- **Roll past-scheduled into Today instead.** Rejected — this is precisely the `<= today`
  Today growth ADR-0022 rejected with the user.
- **Include `🛫` start < today.** Deferred with the other start-date questions; a past
  start date is a weaker lateness signal than a past scheduled date.

## Edge cases

| Case | Behavior |
|---|---|
| Task has `⏳ yesterday`, no due date, open | **Appears in `O` Overdue** (new). |
| Task has `⏳ today`, no due date | Not overdue; appears in `T` (unchanged). |
| Task has `⏳ tomorrow` | Not in Today, not overdue (unchanged). |
| Task has `📅 yesterday`, `⏳ today` | Appears in `T` (scheduled == today) **and** in `O` (due < today). Cross-axis case, pinned by fixture task 1. |
| Task has `📅 today`, `⏳ yesterday` | Mirror cross case: `T` via due-today, `O` via past-scheduled (new fixture task 4). |
| Done task with `⏳ <past>` | Appears under `O`+Done (completed-was-overdue review, widened). |
| Task with no dates | Never overdue (unchanged). |

## References

- [ADR-0022](./0022-today-view-includes-due-today.md) — deferred non-goal reversed here
  (Overdue side); Today scope (scheduled ∪ due == today) unchanged.
- [ADR-0009](./0009-scheduled-date-today.md) — the `⏳` scheduled-date axis this ADR
  admits into Overdue.
- `crates/taski-tui/src/lib.rs` — `build_view`'s `not_overdue` closure; tests
  `overdue_only_keeps_past_scheduled_tasks`, `overdue_only_orthogonal_to_today_filter`.
