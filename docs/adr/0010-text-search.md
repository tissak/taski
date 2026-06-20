# ADR-0010: Task text and file search in the TUI

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** How the user can find tasks by text content or by note path in the TUI — a `/`-key text-search prompt and an `F`-key file/path-search prompt, both filtering the task list by case-insensitive substring match.

## Context

Taski's TUI groups and filters tasks by status (`f`), Today view (`T`), and note-group collapse state. These axes are enough for triage but not for *finding* a specific task when you know what it's about (e.g. "the deployment checklist item for the database migration") or which file it lives in (e.g. "all tasks in `deployment-notes.md`").

The user needs a quick way to narrow the list:
- **Text search:** tasks whose body contains a given word or phrase — a "find in list" gesture analogous to `/` in Vim, less, or any paginated TUI.
- **File search:** tasks whose source note path contains a given string — useful when you know which note the work lives in but not the exact task text.

### Constraints

1. **Must compose with existing filters** — both searches should narrow within whatever status/today filter is active, not replace it.
2. **Independent filter axes** — text search and file search are separate filters that AND together when both are active. The user can search by `/` for text, `F` for file, or both.
3. **Only one prompt active at a time** — whichever key (`/` or `F`) was pressed last owns the footer prompt, but both filters remain applied.
4. **No DB changes** — the existing `db::all_tasks()` already returns the full task set into memory; filtering is a cheap linear scan over a few hundred rows.
5. **No DB write-back** — search is a purely local TUI filter. It implies no data-modelling decision and crosses no earlier ADR.
6. **No new dependencies** — TUI-only, existing crate surface (`crossterm` key handling, `ratatui` rendering).

### Considered approaches

| Approach | Pros | Cons |
|---|---|---|
| **Modal `/` prompt for text** (selected) | Vim/less muscle memory; clear affordance; doesn't consume a permanent keybinding; query visible at prompt | Slightly more code (modal state) |
| **Modal `F` prompt for file** (selected) | Mirrors the `/` pattern; distinct key makes intent explicit; doesn't conflate two gestures | Additional keybinding memorization |
| **Single unified search matching both text and path** (rejected — initial approach) | Simpler implementation; single gesture | Can't distinguish "show me tasks in this file" from "show me tasks with this word"; a path match accidentally broadens a text search the user intended to be narrow |
| **Dedicated filter key** (e.g. `s` toggles a search bar) | Always-visible query | Wastes screen space; `s` is easy to fat-finger |
| **SQL `LIKE` filter in `all_tasks()`** | Could reduce data transferred from DB | All tasks already read for title counts; SQL injection/escaping footgun for a non-networked tool; adds a query param for marginal gain |
| **FTS5 full-text search** | Fast on huge vaults | Over-engineered; new schema; dependency on FTS5 being compiled into libsqlite3-sys; personal vaults rarely exceed 500 tasks |

## Decision

Add two modal search prompts to the TUI:

1. **`/` key** — text search, filters by case-insensitive substring match on `task.text`.
2. **`F` key** — file/path search, filters by case-insensitive substring match on `task.note_path`.

Both follow the same interaction pattern as `/` in Vim/less:

1. Press `/` or `F` → a prompt appears in the footer area (`/query_` for text, `File: /query_` for file).
2. Type characters → the task list re-filters live on each keystroke.
3. `Enter` → dismiss the prompt, keep the query as an active filter.
4. `Esc` → dismiss the prompt and clear that filter (return to the full list).
5. Press the same key again while that query is active → re-enter the prompt with the query still populated, ready to edit.
6. Press the *other* key while a prompt is active → switches to the other prompt (previous query stays applied).

### Filter semantics

- **Text search (`/`):** case-insensitive substring of `task.text`.
- **File search (`F`):** case-insensitive substring of `task.note_path`.
- **Composition:** both filters AND with each other and with the status filter and Today view. So `/deploy` + `F alpha` + `f` Open + `T` Today shows today's open deployment tasks whose note path contains "alpha".
- **Substring match** (not word-boundary or prefix): finds inside compound words; the simplest correct default.

### Why two separate gestures instead of one unified search?

The initial implementation searched both `task.text` AND `task.note_path` with a single `/` query. This was changed to two independent gestures because:

- A query like `/deploy` matching a filename like `deployment-notes.md` is often *not* what the user wants — it broadens rather than narrows the list.
- File search is a different intent: "show me work in this note" vs "show me tasks mentioning this word."
- Having both filters independently addressable enables precise compound queries: `/migration` + `F deploy` finds migration-related tasks within deployment notes, ignoring deployment mentions in other files.

### Why substring, not word-boundary or prefix?

Substring is the simplest correct default: it finds `"deploy"` inside `"pre-deploy-check"`, `"redeploy"`, `"deployment-script"` — all of which a user searching for "deploy" likely wants. Word-boundary or prefix matching would miss those and require the user to retry. Users who need exact-word matching can type the whole word.

## Consequences

- Both search features are entirely contained in `crates/taski-tui/src/lib.rs`.
- No schema change, no config change, no new crate dependency, no daemon change.
- The `Esc` key now has a context-dependent meaning: in a search prompt it clears/cancels rather than quitting. Quitting still works via `q` or `Ctrl-C`.
- Two modal states (`searching`, `file_searching`) require `run_loop` to branch before the existing key-match block. This is a small complexity increase in the event loop but follows the same pattern as any modal TUI interaction.
- `F` is consumed for file search; it was previously unused (lowercase `f` is the status cycle).
- The file search prompt is visually distinguished in the footer: `File: /query_` (green) vs `/query_` (green) for text search.
- Active filter indicators appear in the title bar: `search: query` and/or `file: query`.
- Future: could be extended to due dates or scheduled dates; could be case-sensitive via a config toggle. Neither is needed for MVP.

## References

- The existing `StatusFilter` + `today_only` filter composition in `build_view()` (ADR-0009) — both searches follow the same pattern.
- `crates/taski-tui/src/lib.rs` — sole implementation file.
