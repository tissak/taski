# ADR-0025: Kanban board view — status lanes as rows

- **Status:** Accepted
- **Date:** 2026-10-07
- **Decides:** A TUI-only kanban view (`B`) that sections tasks into status lanes stacked
  as full-width rows, and the `<`/`>` gestures that move a task between lanes. Introduces
  `[!]` as the **blocked** status. **Does not amend [ADR-0003](./0003-checkbox-only-mvp.md)**
  — a lane move is a checkbox flip, the same admitted scope as `i` (ADR-0016).

## Context

The user wants a simple kanban. Classic kanban columns word-wrap and run off a terminal
once there are more than two or three. Rows are wider and read more naturally; the
left-to-right flow of columns is given up for an easier top-to-bottom review.

Kanban needs a per-task status. Obsidian already encodes one: **the checkbox char**.
`[ ]` todo, `[/]` in progress, `[x]` done, `[-]` cancelled are Tasks-plugin built-ins, and
custom chars (`[>]`, `[?]`, `[!]`, …) are a community convention (Minimal / ITS themes)
that the Tasks plugin can map to status types. Taski already parses any char
(`Status::Other`), indexes `raw_checkbox_char`, and the daemon already flips to an
arbitrary `new_char` (covered by `writeback_proptest`, ADR-0016).

## Decision

**Lanes, top to bottom** (active work first, done fades out last):

| Lane | Char |
|---|---|
| Doing | `/` |
| Blocked | `!` |
| Todo | ` ` |
| Done | `x` / `X` |

- **`B`** toggles the board. Each non-empty lane gets a `━━ Label (n) ━━` divider; inside a
  lane the tasks are grouped by the active `G` axis (default folder+note — the "swimlanes").
  Tasks in no lane (cancelled `-`, other custom chars) don't appear on the board.
- **Lane-scoped fold state.** Kanban header keys are `"<lane char>\u{1f}<group key>"`, so a
  note folds independently in each lane.
- **Lanes fold too.** `Enter` / `←` / `→` on a lane divider folds or unfolds the whole lane
  (session-only state). **Done starts folded** — just its `▸ Done (n)` divider shows, so
  finished work fades out at the bottom without clutter. `Tab` doesn't open a folded lane.
- **Filters:** `T`, `O`, `/`, `F` compose as usual. The `f` status filter is ignored on the
  board — the lanes *are* the status axis.
- **`<` / `>`** move the selected task one lane up / down: a `checkbox` action to the
  neighbouring lane's char. No new action_type, no schema bump, no daemon change. `u` undo
  is free. Because lanes are adjacent-only and Done sits next to Todo, Done is entered and
  left only via ` `↔`x`, so the ADR-0012 `✅` stamp/clear is always correct. The keys work in
  the list view too (they just step through the lane order).
- **Blocked counts as open.** `is_open_like` now includes `[!]`, so blocked tasks show under
  the default `Open` filter and in open counts. Without this, `[!]` tasks vanish from the
  normal list.
- **`m` move mode is refused on the board.** A board group holds one lane's slice of a note,
  and a reorder permutation across that slice is not what the user sees. Reorder from the
  list view.

## Consequences

- Moving a task Done → Todo → Blocked keeps the `✅` stamp cleared (via Todo). A task
  hand-edited from `[x]` to `[!]` in Obsidian keeps its `✅` — same "other chars skip the
  stamp oracles" behaviour as ADR-0016.
- **Obsidian rendering:** plain Obsidian renders any non-space checkbox char as checked.
  `[!]` needs a theme/CSS snippet (Minimal renders it as a warning icon) and, for Tasks
  queries, registering as a custom status (type `TODO` / `IN_PROGRESS`).
- Lanes are hardcoded (`LANES` in `taski-tui`). A `[kanban]` config table is deferred until
  a lane needs adding or renaming.
- After a lane move the task can land in a folded group in its new lane; the cursor falls
  back to a same-note row. Following the moved task is deferred.

## Alternatives rejected

- **Obsidian Kanban plugin format** (one note per board, `## heading` per column). Moving a
  card means moving its line under another heading — a structural cross-section move, far
  beyond the bounded gates of ADRs 0020/0021 — and it only covers one note, not the vault.
- **Status tags** (`#status/doing`). Writing them edits task text, which ADR-0003 rejects,
  and duplicates what the checkbox char already says.
- **A `G` grouping axis "status"** alone. Close, but the lanes need their own fold scope,
  fixed order, and to keep `G` free for swimlanes inside them.
