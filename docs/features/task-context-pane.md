# Task Context Pane

*Generated: 2026-06-20*
*Status: Brainstorm / Ready for Stories*
*Branch: `experiment/task-context-pane`*

---

## Problem Statement

A Taski user currently browses tasks as a flat, grouped list of checkbox lines. Each row
shows the task **text** and metadata (due date, source note) — but not the **surrounding
note content**. A task body is often terse ("Send the Q3 numbers to Priya", "Fix the
onboarding edge case") and, stripped of its note, loses the context that gave it meaning:
the heading it sat under, the paragraph above it, the adjacent tasks in the same list.

The user (a single technical operator of one vault) frequently hits the moment where, to
**confidently close** a task, they need just a little more — the heading, the note title,
the line before it. Today the only way to get that is to context-switch: open Obsidian,
find the note, scroll to the line. That breaks the flow that Taski exists to preserve.

**Frequency:** High — on a meaningful fraction of triaged tasks, especially older ones or
tasks in meeting/scratch notes whose meaning depends on context.
**Severity:** Medium — not data loss, but a recurring flow-breaker that undercuts Taski's
"fast execution layer" promise. The cost is friction + abandoned/confused closes.
**Current Workaround:** Switch to Obsidian, open the note, find the line. (Or just guess
and close, risking closing the wrong thing or losing the thread.)
**Workaround Gap:** It is a full context switch out of Taski — exactly the break in flow
Taski was built to eliminate. It also assumes the user remembers *which* note the task
came from at a glance.

**Root Cause Check:** This is the real problem, not a symptom. The index carries the task
*text* but deliberately not the task *context*; displaying it requires a design decision
about how the (read-only) TUI obtains note content. That decision is the spine of this doc.

---

## Feature Fit Analysis

**Product Alignment:** Strong. PRD §2 names two pains — *discovery* and *structure* — and
the one-liner frames Taski as a fast "execution layer" over scattered tasks. Triaging
confidently is the execution step; lacking in-situ context is a gap in the execution loop.
This feature closes that loop without changing the source-of-truth model.

**Complementary Features:**
- **Group-by-note list (Slice 4):** the list already groups tasks by note; the context pane
  makes the *note* dimension visible, not just the task.
- **Status filter / selection navigation:** the pane is selection-driven, so it composes
  with the existing filter + movement model.
- **Toggle write-back (Space):** unchanged — the pane is read-only and lives alongside the
  existing toggle flow.

**Potential Conflicts:** None functional. The only tension is **architectural** (see
Implementation): the TUI needs note content it doesn't currently have. We resolve this by
*not* breaking the "TUI never touches the vault" boundary — content flows through the index
like everything else.

**Brand Consistency:** Feels on-brand — it deepens the existing fast-list-and-act model
rather than adding an unrelated surface. No new process, no new external dependency.

**Strategic Value:** Medium-High. Low cost (schema + TUI layout), recurring daily value,
and it strengthens the core "act confidently without leaving the terminal" promise.

---

## User Experience Design

### User Journey

**Entry Points:**
1. **Primary:** open `taski-tui`. The split-pane view is the default layout (tasks left,
   context right). No new command needed to discover it.
2. **Secondary:** a key to toggle the pane on/off (see Keybindings) for users who want the
   full-width list back.

**Happy Path:**
1. User moves selection up/down the task list (existing keys) → **System** updates the
   right pane to show the note content around the newly selected task, with the task's own
   line highlighted.
2. User reads the surrounding heading/paragraph → recognizes the full intent of the task.
3. User presses **Space** to toggle → checkbox flips via the existing write-back path; on
   the next scan the right pane reflects the new state.
4. User moves on; the pane follows the next task.

**Edge Cases & Error States:**
- **Selected row is a group/note header (no task):** show the note's top content or a
  neutral hint ("Select a task to see its context"), never an error.
- **Context unavailable** (pre-v3 DB, a note that failed to cache, or content pruned):
  render a graceful placeholder ("Context not available for this note"), not a crash.
- **Very large note:** cap the rendered window; the pane scrolls (see Keybindings) rather
  than reflowing the whole file.
- **Task near the top/bottom of a note:** the window clamps to the file bounds.
- **Task text wraps past one terminal line:** wrap within the pane; keep the highlight on
  the logical task line.
- **Obsidian edits the note between scans:** the index (and thus the pane) lags by ~1s.
  Because content + `line_number` + `note_hash` are written in the *same* scan, the pane is
  always internally consistent — the highlight never drifts off the task within a given
  snapshot. The user simply sees the pre-edit snapshot until the next poll.

**Success State:** For any selected task, the right pane shows enough surrounding note
content that the user can recall the task's full intent without leaving the TUI.

**Feedback & Confirmation:** The pane is live and selection-driven, so feedback is
immediate and continuous. A toggle landing is already confirmed by the existing
write-back notice + the checkbox visibly flipping on the next poll.

### UX Considerations

- **Discoverability:** Default-on split pane means zero learning cost to *find* it; the
  only thing to learn is the scroll key.
- **Learnability:** Movement stays where it is; one new concept (scroll the context pane).
- **Efficiency:** Glance-right beats context-switch-to-Obsidian. No extra keystroke to
  *see* context — it just appears.
- **Error Prevention:** Read-only pane removes any chance of accidental note edits; the
  vault is still mutated only through the audited write-back path.

### Keybindings (proposal — to confirm)

| Action | Key | Notes |
|---|---|---|
| Move task selection | existing (arrows / `j` `k`) | unchanged |
| Toggle task | `Space` | unchanged |
| Scroll context pane | `J` / `K` or `Ctrl-d`/`Ctrl-u` | must **not** collide with existing keys (`f` filter, `q` quit, etc.) |
| Toggle pane on/off | `Tab` (proposed) | reclaim full width for the list |

---

## User Story Foundation

### Core Behaviors (Must Have)

**As a** single-user operator of my vault,
**I want to** see the note content surrounding the currently selected task,
**so that** I can recall its full intent and close it confidently without leaving the TUI.

**Acceptance Criteria:**
- [ ] The TUI renders a two-pane layout: task list on the left, note context on the right.
- [ ] Selecting a task (move up/down) updates the right pane to show that task's note, with
      the task's line highlighted and centered in a window of surrounding lines.
- [ ] The right pane is **read-only**; toggling a task still flows through the existing
      `pending_actions` → daemon write-back path (ADRs 0002/0003/0004 unchanged).
- [ ] The TUI **never opens a vault file directly** — all note content is read from the
      SQLite index, as today.
- [ ] The pane degrades gracefully when context is unavailable (placeholder, no panic).
- [ ] The pane reflects a toggle within the normal ~1–2s scan/poll latency (checkbox flips
      visibly in both panes after write-back).

### Supporting Behaviors (Should Have)

**As a** user,
**I want to** scroll the context pane independently,
**so that** I can read more of the note than the default window shows.

- [ ] A dedicated key scrolls the context pane without moving task selection.

**As a** user,
**I want to** collapse the context pane to reclaim the full-width list,
**so that** I can work in either mode.

- [ ] A key toggles the pane between split and full-width-list.

### Future Considerations (Could Have)

- Configurable context window size (lines above/below) in `config.toml`.
- Jump the context window to the task's nearest preceding Markdown heading anchor.
- Render lightweight Markdown emphasis in the pane (bold/heading) rather than raw text.
- Show sibling tasks (same note) as faint context lines in the pane.

### Out of Scope (Explicitly Not Doing)

- **Editing note content from the TUI.** MVP write-back is checkbox flips only
  (ADR-0003). Editing text from the pane is a *large* change and is explicitly deferred.
- **TUI reading vault files directly.** Rejected for this feature — see Implementation.
- **Storing/rendering rendered Markdown (HTML) or images.** Plain-text window only.
- **Per-task stored context windows.** We store note content *per note*, not per task.
- **Caching rendered panes across runs.** The index is the cache; nothing extra.

---

## Implementation Considerations

### Architecture decision (locked for this feature)

**The daemon caches note content in the SQLite index; the TUI reads it like any other
index data.** This preserves the codebase's central invariant — *"SQLite is the
decoupling boundary; the TUI never touches the vault"* — and, as a bonus, **solves the
staleness problem for free**: because the note content, the task's `line_number`, and the
`note_hash` are all written during the *same* `index_note` scan pass, they are always
mutually consistent in any snapshot the TUI reads. The highlight can never drift off the
task within a single poll.

The rejected alternative (TUI reads `<vault>/<note_path>` directly) was faster to prototype
but would (a) reintroduce a direct vault dependency in the TUI, (b) require the TUI to
load the `vault` path from config (it doesn't today), and (c) risk the highlight landing
on the wrong line when the live file has changed since the last scan (index lags ~1s).

> This is a load-bearing choice and should be recorded as **ADR-0006 — "Note content cached
> in the index for TUI context"** during implementation, with the rationale above and an
> explicit note that it does *not* relax ADR-0002 (TUI still never writes; still never
> opens vault files).

### Technical Approach

**Patterns to Leverage:**
- `taski-db::SCHEMA` + `SCHEMA_VERSION` bump (existing destructive-bump migration pattern
  — fine pre-MVP; see context.md "Schema migration is destructive").
- `taski-daemon::index_note` — already reads each note end-to-end to parse tasks; writing a
  content row is a trivial addition in the same pass.
- `taski-tui` view-model tests (existing `main.rs` unit tests for grouping/filter/selection)
  — extend with context-pane window/highlight logic as pure functions.
- `ratatui` `Layout::horizontal` for the split; existing `draw()`/`render` style.

**Likely New Components:**
- **`taski-db`:** a new `note_contents` table:
  `note_path TEXT PRIMARY KEY, content TEXT, note_hash TEXT, line_count INTEGER, updated_at`.
  One row per note (deduped by note, *not* per task). New APIs: `upsert_note_content()`
  (daemon, same pass as `reconcile_note`) and `note_content(conn, note_path)` (TUI read).
  Bump `SCHEMA_VERSION` to 3.
- **`taski-tui`:** a context-pane widget + a pure `context_window(content, line, before,
  after)` helper that returns the lines to render and the index to highlight. A small
  struct in `App` for the pane's scroll offset and the currently-loaded `note_path`.
- Lazy fetching: the TUI reads `note_content` **only when the selected note changes**, not
  on every 750ms poll (keeps polling cheap). Cache by `note_path` + `note_hash` to avoid
  re-reading unchanged content.

**Data Considerations:**
- Storage: full note text per note. Personal vault notes are typically KB-scale; acceptable
  for a single-user local DB. Add a size cap / truncation guard only if real notes prove
  huge (deferred until measured).
- No new `taski-core` concern (it stays pure — it already returns parsed tasks; it does not
  and should not own note storage). Content caching lives in `taski-db` + `taski-daemon`.
- Consistency is structural (see Architecture decision): no extra coordination needed.

**Integration Points:** None external. Everything stays inside the existing daemon↔SQLite↔TUI
loop. No new crates, no new third-party deps (rendering is plain `ratatui` text).

### Complexity Estimate

**Effort:** **M.** The risky part is TUI layout/keybinding, not data safety. Backend
(schema + daemon write + db read) is small and sits on existing patterns.
**Risk Level:** **Low.** No write-back or identity changes; the safety contract (ADRs
0002–0005) is untouched. The feature is purely additive read path + UI.
**Dependencies:** None blocking. Schema bump wipes dev DBs (already the pre-MVP behavior).

### Vertical Slice Phasing (matches the repo's slice style — each leaves it runnable)

- **Phase A — Backend, no UI:** schema v3 + `note_contents` table + `upsert_note_content`/
  `note_content` APIs + daemon writes content in `index_note` + `taski-db`/`taski-daemon`
  tests. App behaves identically; index now carries content.
- **Phase B — Vertical thin slice:** TUI renders a fixed (non-scrolling) context window for
  the selected task, highlight on the task line. Proves the read path end-to-end. Ugly but
  working.
- **Phase C — Polish:** independent pane scrolling, the toggle-pane keybinding, window
  clamping/wrapping, the "context unavailable" placeholder, and view-model tests.

### Questions for Development

- Confirm the context scroll keybinding (`J`/`K` vs `Ctrl-d`/`Ctrl-u`) and the pane-toggle
  key (`Tab`) don't collide with anything planned.
- Decide the default window size (e.g., ±12 lines) and whether it's hardcoded v1 or
  config-driven from day one.
- Do we store full note content, or only a bounded window, in `note_contents`? (Recommend
  full content per note — simplest, lets the TUI choose any window; revisit if storage bites.)

---

## Success Metrics

Taski is a personal, single-user tool, so the bar is qualitative flow, not dashboards.

### Primary Signal
**Reduced "what was this task again?" friction.** Subjective: over a week of use, does the
user close tasks from the TUI without switching to Obsidian more often than before? Target:
the user stops context-switching to Obsidian for context on *most* triaged tasks.

### Supporting Signals
- **Consistency:** the highlighted line always matches the selected task across polls (no
  drift) — verified by view-model tests and the structural same-scan guarantee.
- **Liveness:** a toggle reflects in the pane within the existing ~1–2s latency (no new
  latency budget introduced).
- **Robustness:** no pane-related panics on missing/huge/empty notes.

### Qualitative Feedback
Self-reported after a few days: does the pane earn its screen real estate, or does the user
mostly keep it collapsed? (Informs whether the default should stay on.)

### Success Definition
The user can, for the typical task, recall its full intent from the right pane alone and
close it confidently — without leaving the TUI — and the index/vault safety guarantees are
unchanged.

---

## Reality Check

### Risks

**Scope Creep:** Drifting toward *editing* note content in the pane, or rendering rich
Markdown. → **Guardrail:** ADR-0003 stays in force (checkbox-only write-back); the pane is
read-only and plain-text. Editing is explicitly Out of Scope.
**User Adoption:** The pane eats horizontal space on narrow terminals, making both sides
cramped. → **Mitigation:** a pane-toggle key + a sensible min-width; fall back to
full-width list below a terminal-width threshold.
**Technical:** Schema bump wipes dev DBs (already the pre-MVP behavior — low impact), and
larger `taski.db` size. → **Mitigation:** store content per-note (deduped); add a size cap
only if measured necessary.

### Alternative Approaches Considered

| Approach | Pros | Cons | Why Not Chosen |
|----------|------|------|----------------|
| TUI reads vault files directly | Fastest prototype; no schema change | Reintroduces vault dep in TUI; TUI needs config's `vault` path; highlight can drift when file changed since last scan; muddies "TUI never touches vault" | Loses the consistency guarantee and violates the spirit of the architecture for a one-time convenience |
| Store per-task context windows in the index | Smaller rows; window pre-computed | Awkward (one window per task; recomputed on every scan); inflexible window size; more write churn | Per-note content is simpler, deduped, and lets the TUI pick the window |
| Show only the note title + nearest heading (no body) | Cheapest; tiny storage | Often insufficient context (the body above the task is the valuable part) | Doesn't reliably solve the "remember the full task" problem |

### Kill Switch
If, after using it, the pane mostly stays collapsed and the user still context-switches to
Obsidian for real context, the feature isn't earning its keep — revert and reconsider
(e.g., jump-to-Obsidian-at-line instead).

### Confidence Level
**Overall Confidence:** High.
**Reasoning:** Additive read path (no safety-contract risk), small backend sitting on
existing patterns, and the architecture choice preserves the codebase's core invariants.
The only real uncertainty is UX tuning (window size, keybindings), which is cheap to
iterate on.

---

## Summary

**One-liner:** The context pane helps the user confidently close a task by showing the
note content around the selected task in a right-hand pane — driven entirely through the
SQLite index so the vault boundary stays intact.

**Key Behaviors:** split-pane (tasks left / context right); selection-driven, highlighted,
read-only context window; optional scroll + collapse.

**Biggest Risk:** UX — the pane crowding narrow terminals / not being valuable enough to
keep default-on. Mitigated by a toggle key and real-world use feedback.

**Next Actions:**
1. Record the architecture choice as **ADR-0006** (note content cached in the index; does
   not relax ADR-0002).
2. Implement **Phase A** (schema v3 + `note_contents` + daemon write + db read + tests).
3. Implement **Phase B** (non-scrolling context window in the TUI) to prove the vertical.
4. Polish in **Phase C** (scroll, toggle, edge cases, view-model tests).
