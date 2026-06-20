# ADR-0011: Bullet toggle and undo in the TUI

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** How the user can convert a checkbox task to a plain bullet (and back), and undo the last checkbox or bullet action.

## Context

The TUI's write-back model currently supports two action types: `checkbox` flips (`Space` toggles `[ ]↔[x]`) and `set_scheduled` (`t` writes/removes `⏳ <date>`). Both mutate the task's state or metadata but keep it as a checkbox task.

Sometimes a task becomes irrelevant — the work is no longer needed, the task was a mistake, or the user wants to preserve the note text without it being a tracked todo. In Obsidian the user would delete the checkbox by hand (`- [ ] ` → `- `), but the TUI had no way to make this change.

Additionally, users need a way to reverse an accidental action — an undo gesture that reverses the last `Space` or `b` toggle.

### Constraints

1. **Must go through the daemon as sole writer** — same as every other write gesture (ADR-0002).
2. **No schema change** — `pending_actions.action_type` is already a TEXT column; new action types are free.
3. **Must compose with existing write-back safety** — the same `lookup_task_for_action` + content-hash + `atomic_write` pipeline applies unchanged.
4. **Undo reverses immediately** — queues the reverse action without waiting for the original to resolve. If the original fails, the undo will also fail naturally (daemon re-verifies current state).
5. **Undo scope** — reverses the last `Space` (checkbox flip) or `b` (bullet toggle), not `t` (mark-for-today). The `t` gesture is idempotent (pressing `t` again removes the mark), so undo is less useful.

## Decision

### Feature 1: Bullet toggle (`b` key)

Add a `b` keybinding that toggles the selected task between checkbox and bullet format:

- `- [ ] task text` → `- task text` (checkbox to bullet)
- `- [x] task text` → `- task text` (same, regardless of status)
- `- task text` → `- [ ] task text` (bullet back to open checkbox)

The daemon processes this as a new action type `toggle_bullet`, reusing the same conflict-checked atomic write path.

A pure oracle `taski_core::toggle_bullet(line) -> RewriteResult` drives the line-level transformation, mirroring `rewrite_scheduled` for the `⏳` write path. The oracle never guesses on malformed input.

### Feature 2: Undo (`u` key)

Add a `u` keybinding that reverses the last write action:

- **After `Space` (checkbox flip):** queues the opposite checkbox flip (swapped `expected_char`/`new_char`).
- **After `b` (bullet toggle):** queues another bullet toggle (it's a toggle, so the inverse is the same operation).
- **After anything else or nothing:** no-op.

The TUI tracks the last action in-memory. When undo is pressed, it immediately enqueues the reverse action. The daemon processes it in its next drain cycle — verifying current state against current bytes, so if the original action already failed or was overtaken by an Obsidian edit, the undo also fails naturally.

## Consequences

- No schema change (new action type values are free-form TEXT).
- The `b` key is consumed for bullet toggle.
- The `u` key is consumed for undo.
- The existing `ApplyOutcome` enum is extended with a `BulletUnparseable` variant for when `toggle_bullet` returns `Unparseable`.
- The failure-notice renderer in the TUI learns the `toggle_bullet` action type for user-facing messages.
- Undo is tracked in TUI memory only (not persisted). A TUI restart clears the undo history — acceptable for a personal tool.
- The `toggle_bullet` action stores no `expected_char`/`new_char` (unused; the daemon dispatches on `action_type`).

## References

- `process_metadata_action` in `taski-daemon` — the structural template for `process_bullet_action`.
- `rewrite_scheduled` in `taski-core` — the structural template for `toggle_bullet`.
- ADR-0009 — established the pattern for adding new action types to the pending_actions pipeline.
