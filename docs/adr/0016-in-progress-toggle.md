# ADR-0016: In-progress (`/`) toggle gesture

- **Status:** Accepted
- **Date:** 2026-06-21
- **Decides:** How the user sets a task to the Obsidian in-progress state (`- [/]`) from
  the TUI (`i` key). **Does not amend [ADR-0003](./0003-checkbox-only-mvp.md)** — this is
  a pure checkbox flip to a char that was always within the admitted "checkbox-state flips"
  scope; it adds no token, no stamp, no oracle, and no daemon change.

## Context

`Status::InProgress` (`/`, rendered `- [/]`) has been a first-class status since v0.1:
`taski-core` parses it, round-trips it through `from_checkbox_char`/`to_checkbox_char`,
the index stores it, and the TUI renders it (cyan checkbox). **But there was no way to
*set* it from the TUI.** The three checkbox-flip gestures were `Space` (open ↔ done) and
`d` (cancel, ADR-0013). A task could only reach `- [/]` by being hand-edited in Obsidian;
Taski would then display it but could never produce it. Pressing `Space` on an in-progress
task reset it to open (`toggle_target_char` maps `/` → ` `), and `d` cancelled it — both
*destroying* the in-progress state rather than reaching it.

The gap was purely a missing gesture, not a missing capability: every layer below the TUI
already understood `/`.

## Decision

Add an `i` key that toggles the selected task's in-progress state, implemented as a
`checkbox` action with `new_char = '/'` — the exact structural mirror of `d`/cancel
(`new_char = '-'`, ADR-0013) and `Space`/done (`new_char = 'x'`):

- open (` `) → in-progress (`/`)
- in-progress (`/`) → open (` `)
- any other state (done, cancelled, forwarded, …) → in-progress (`/`) — "press `i` to mark
  in-progress"

A pure `in_progress_target_char(raw)` helper mirrors `cancel_target_char`. `submit_in_progress`
mirrors `submit_cancel`: enqueues a `checkbox` action, records a
`LastAction::CheckboxToggle` (same variant — in-progress *is* a checkbox flip). **Undo is
free**: the existing `u` reverses checkbox flips.

### Why this needs no stamp, no oracle, no daemon change

`Space` composes a `✅` stamp (ADR-0012) and `d` composes a `❌` stamp (ADR-0013) because
those flips are *semantically coupled* to a dated token — you stamp `✅` *because* the task
is done. In-progress has **no associated dated token**: there is no `🚧 <started-date>` in
the Obsidian Tasks vocabulary that Taski reads or writes. So there is nothing to compose.

Critically, the daemon already handles this correctly with **zero new code**.
`process_action_at`'s stamp decision (ADRs 0012/0013) has an explicit "other chars" arm:

> anything else (e.g. InProgress `/`) → skip the `✅`/`❌` oracles; only the flip is written
> (`✅`/`❌` left untouched — ambiguous, do not guess).

and `process_action_at` validates `new_char` is a single Unicode scalar (`single_char`),
not an allowlist — so `/` was already an accepted target. The in-progress flip was an
unreachable code path until this ADR wired up the gesture that drives it.

### Why this does not amend ADR-0003

ADR-0003's original admitted scope is **checkbox-state flips**. In-progress is a checkbox
state (`Status::InProgress`, round-tripped since v0.1); flipping to `/` is a checkbox-state
flip. The only ADRs that amended ADR-0003 were those admitting a *new kind of mutation*
alongside the flip: `⏳` scheduled writes (ADR-0009), `✅` done stamps (ADR-0012), `❌`
cancelled stamps (ADR-0013), and bounded append-only creation (ADR-0014). ADR-0016 adds no
new mutation kind — it targets an already-admitted flip at an already-valid char. It is the
first write-gesture ADR that leaves ADR-0003 untouched.

## Implementation Notes

All changes are in **`crates/taski-tui/src/lib.rs`** — no other crate is touched:

1. `in_progress_target_char(raw)` — pure helper after `cancel_target_char` (`"/" => " "`,
   `_ => "/"`).
2. `enqueue_in_progress(conn, task)` — reuses `db::enqueue_action` with the in-progress
   target char; after `enqueue_cancel`.
3. `submit_in_progress(&mut self, conn)` on `impl App` — mirrors `submit_cancel`; records
   `LastAction::CheckboxToggle { new_char: "/", .. }` only on successful enqueue.
4. `KeyCode::Char('i')` → `app.submit_in_progress(conn)` in `run_loop`'s normal-mode
   dispatch (after the `d` arm). Suppressed during a search prompt — `i` falls through to
   `push_search_char`, exactly like `b`/`d`/`a`/`t`.
5. `render_failure_notice` arm: `_ if action.new_char == "/" => ("Mark in-progress", "i")`,
   alongside the cancel `_ if action.new_char == "-"` arm — so a refused in-progress flip
   hints the `i` retry key, not `Space`.
6. Help-overlay row: `row("i", "Mark in-progress / re-open")` in the "Task actions"
   section. The trimmed footer cheat-sheet is intentionally untouched (`d` is not in it
   either).

**No proptest is added.** The write path is the existing checkbox-flip pipeline; the
existing `writeback_proptest` (256 cases, "never corrupts") already exercises arbitrary
`new_char` flips and the `/`-skips-oracle arm. There is no new pure oracle to property-test.

## Rationale

- **It closes a pure gestural gap.** Every layer already understood `/`; only the TUI key
  was missing. This is the smallest possible change that exposes a first-class Obsidian
  status.
- **Zero daemon risk.** `process_action_at` was already in-progress-aware (ADR-0012's
  "leave `✅` untouched on `/` flips" arm). Nothing on the write path changed — this ADR
  only drives an existing, already-correct code path from the TUI.
- **Symmetry with the other checkbox-flip siblings.** `Space` (done), `d` (cancelled), and
  now `i` (in-progress) form a complete set over the three Obsidian checkbox states Taski
  models. Each reuses the `checkbox` action_type; each differs only in `new_char`.
- **Undo is free.** `LastAction::CheckboxToggle` already reverses arbitrary checkbox flips;
  in-progress needs no new `LastAction` variant and no new `submit_undo` arm (exactly as
  cancel needed none in ADR-0013).

## Consequences

- ✅ The user can now mark a task in-progress from the TUI (`i`), and toggle it back to
  open (`i` again). `u` reverses it.
- ✅ ADR-0003, ADR-0004, ADR-0005, and the daemon are all **unchanged**. No schema bump, no
  new action_type, no new `LastAction` variant, no new pure oracle, no new proptest.
- ⚠️ **Stamp interaction (deliberate, not a bug):** because the daemon skips the `✅`/`❌`
  oracles for `/` flips, flipping a **done** task to in-progress leaves its `✅` in place,
  and flipping a **cancelled** task to in-progress leaves its `❌` in place. This is the
  existing, documented ADR-0012/0013 "ambiguous — do not guess" behavior, now reachable
  from the TUI. It is accepted: an in-progress task with a historical done/cancelled date
  is coherent (the task was done/cancelled and is now being worked on again), and silently
  clearing the date would destroy information the user may want. If the user wants a clean
  state, `Space` (→ done, stamps fresh `✅`) then `i` is available, or they edit in
  Obsidian.
- ⚠️ The `i` key is consumed for in-progress. The `u` undo scope widens implicitly (undo
  already reverses checkbox flips, and in-progress is one) — context.md's keybinding table
  and "Undo scope" note are updated to mention `i`.
- ⚠️ A `/` write does **not** change `text_hash` beyond the single checkbox char, so —
  unlike `✅`/`❌`/`⏳` writes — the surrogate id is retained across the post-apply re-index
  (the body hash is unchanged). This is benign and slightly *better* than the dated-token
  writes.

## Alternatives considered

- **Compose a start-date stamp (`🛫`) on the in-progress flip.** **Rejected.** Although
  `🛫` is the Obsidian Tasks "start date" and is parsed by Taski (Tier 1, schema v6), it is
  not semantically "the date I started working on this" — it is a *planned* start
  constraint that drives the Tasks plugin's "is this task yet actionable?" logic. Stamping
  it on an in-progress flip would conflate two meanings and mutate a field the user may
  have intentionally set. In-progress has no dated-token semantics; the right move is to
  flip the char and touch nothing else. (Writing `🛫` from the TUI remains deferred under
  the ADR-0009 grammar-provability gate, like the other unwritten tokens.)
- **Make `i` a three-way cycle (open → in-progress → done).** **Rejected.** Done already
  has a dedicated, stamp-composing gesture (`Space`); overloading `i` to also reach done
  would split the done path and diverge from the `✅`-on-done invariant. A dedicated
  in-progress toggle that mirrors `d`'s "target this state / re-open from it" shape is
  clearer and keeps each gesture single-purpose.
- **Defer until a status-filter for in-progress is also added.** **Rejected.** The
  `StatusFilter` cycle (`All`/`Open`/`Done`) not surfacing in-progress is an independent,
  view-side concern; it does not block the write gesture. This ADR is the write gesture
  only.
- **New `set_in_progress` action type.** **Rejected.** In-progress is a checkbox flip, not
  a metadata write; a new action_type would needlessly widen the daemon's dispatch surface
  for no functional gain, exactly as ADR-0013 rejected `set_cancelled`.

## Edge cases

| Case | Behavior |
|---|---|
| Flip ` ` → `/` (open → in-progress) | Only the checkbox char changes. No stamp. |
| Flip `/` → ` ` (in-progress → open) | Only the checkbox char changes. |
| Flip `x` → `/` (done → in-progress) | Flip lands; existing `✅` is **left untouched** (ADR-0012 "other chars" arm). |
| Flip `-` → `/` (cancelled → in-progress) | Flip lands; existing `❌` is **left untouched** (ADR-0013 "other chars" arm). |
| `Space` on an in-progress task | Resets to open (`toggle_target_char` maps `/` → ` `) — unchanged. Stamping follows the open-target rule (clears `✅`). |
| `d` on an in-progress task | Cancels (`cancel_target_char` maps `/` → `-`) — unchanged. |
| `u` after `i` | Reverses the flip (in-progress ↔ open); the existing checkbox-undo path. |
| Concurrent Obsidian edit (any line) | `note_hash` mismatch → `ConflictNoteChanged` → refuse (ADR-0004, unchanged). |
| During a search/quick-add prompt | `i` builds the query string, exactly like `b`/`d`/`a`/`t`. |

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; the
  in-progress flip routes through the existing checkbox action.
- [ADR-0003](./0003-checkbox-only-mvp.md) — **not amended**; in-progress is a checkbox-state
  flip, within the original admitted scope.
- [ADR-0004](./0004-refuse-on-conflict.md) — refuse-on-conflict / TOCTOU; **reused
  unchanged**.
- [ADR-0005](./0005-surrogate-identity.md) — **not amended**; a `/` flip does not change
  `text_hash` beyond the checkbox char, so the surrogate id is retained.
- [ADR-0011](./0011-bullet-toggle-undo.md) — the `u` undo model this gesture reuses for
  free.
- [ADR-0012](./0012-done-date-on-toggle.md) — defines the "other chars (e.g. InProgress
  `/`) → skip the `✅` oracle" arm this gesture relies on.
- [ADR-0013](./0013-cancelled-date-on-cancel.md) — the direct structural template (`d`
  key, `checkbox` action with `new_char = '-'`); this ADR is its in-progress sibling.
- [`docs/context.md`](../context.md) — keybinding table and "Undo scope" note updated for
  the `i` gesture.
