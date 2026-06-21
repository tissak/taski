# ADR-0013: Cancelled-date (`❌`) stamp on cancel (Tasks-plugin interop)

- **Status:** Accepted
- **Date:** 2026-06-21
- **Decides:** How Taski closes the interop-correctness gap where a task cancelled via Taski is invisible to Obsidian Tasks-plugin "cancelled" queries because the `❌ YYYY-MM-DD` cancelled date is never stamped, and how the user invokes the cancel gesture from the TUI (`d` key). **Amends [ADR-0003](./0003-checkbox-only-mvp.md)** for a third time — the first two amendments were [ADR-0009](./0009-scheduled-date-today.md) (`⏳` scheduled) and [ADR-0012](./0012-done-date-on-toggle.md) (`✅` done).

## Context

Taski already **reads** the `❌` cancelled date (Tier 1, schema v6: `extract_cancelled_date` → `tasks.cancelled_date`), and the Tasks plugin auto-writes `❌ <cancellation-date>` when a task is cancelled. Tasks queries like `cancelled`, `cancelled this month`, and Dataview's `status.name === "Cancelled"` all key off `❌` and the `- [-]` checkbox. **But Taski had no cancel gesture at all** — the user could toggle done (`Space`) but could not cancel. The roadmap's deferred list states: *"`❌` cancelled-date is the next candidate but depends on a cancel gesture that doesn't exist yet."* This ADR is that gesture.

### Why this is an amendment to ADR-0003, exactly parallel to ADR-0012

ADR-0012 admitted `✅` under the ADR-0009 grammar-provability gate by composing the `✅ <today>` stamp into the same byte-buffer splice as the `[ ]`→`[x]` flip. `❌` meets the same three gates identically:

- (i) `❌ YYYY-MM-DD` is canonical Tasks-plugin cancelled-date syntax, human-readable and consumed by Tasks/Dataview/Obsidian.
- (ii) The insertion grammar is identical to `✅`/`⏳` — same emoji + optional VS16 + whitespace + strict `YYYY-MM-DD`, scanned by the shared `extract_emoji_date` reader.
- (iii) The pure oracle `rewrite_cancelled_date` (this ADR) is a one-line wrapper over the already-generalized `rewrite_emoji_date` core (ADR-0012), guarded by its own 256-case proptest — the direct sibling of `rewrite_done_date`.

So `❌` is admissible under the *existing, unchanged* boundary; this ADR does not widen it. Precedent remains gated by grammar-provability.

### Why the stamp composes into `process_action`, not a new action type

For the same reason ADR-0012 composed `✅` into the checkbox flip: the cancelled date is **semantically coupled to the cancel flip itself**. You stamp `❌` *because* you cancelled the task; the stamp without the flip is meaningless, and the flip without the stamp is the same interop bug ADR-0012 closed (this time for "cancelled" queries). Splitting would mean two queue rows, two writes, two TOCTOU windows, and a partial-failure state (flip landed, stamp refused — vault lies). Composing keeps it one write, one hash, one rename.

The cancel gesture reuses the existing `checkbox` action_type — `d` enqueues a checkbox flip with `new_char = '-'` (the Obsidian cancelled char). No new action_type, no schema change, no new TUI undo machinery (the existing `u` reverses checkbox flips, so undo-of-cancel is free).

## Decision

On a checkbox flip executed by `process_action`, additionally rewrite the target line via the pure `rewrite_cancelled_date` oracle **in the same byte buffer** that performs the flip, before the single `atomic_write`. The stamp decision widens from ADR-0012's two-state (done/open) model to a three-state (done/cancelled/open) model:

- **→ Done char (`x`/`X`):** stamp `✅ <today>` (ADR-0012, unchanged); **clear** any existing `❌` (you cannot be both done and cancelled).
- **→ Cancelled char (`-`):** **clear** any existing `✅`; stamp `❌ <today>`. If a parseable `❌` already exists, replace its date with `<today>` (canonical re-cancel); if it already equals `<today>`, the oracle returns `Unchanged` for that dimension.
- **→ Open char (` `):** clear `✅` (ADR-0012) **and** clear `❌` (an open task has neither).
- **→ Other chars (`/`, `>`, `!`, …):** leave both `✅` and `❌` untouched (ambiguous; do not guess). Only the flip is written.

A malformed `❌` on a cancel transition refuses the whole action with `CancelledDateUnparseable` (parallel to ADR-0012's `DoneDateUnparseable`) — no flip, no stamp, vault untouched. A malformed `✅` on a cancel transition (which would clear it) refuses with `DoneDateUnparseable` (ADR-0012, unchanged).

`<today>` is computed by the pure `taski_core::ymd_from_unix(unix_now())`, identical to ADR-0012.

### The pure oracle

A one-line wrapper over the `rewrite_emoji_date` core ADR-0012 generalized from `rewrite_scheduled`:

```rust
const CANCELLED_EMOJI: char = '❌';

pub fn rewrite_cancelled_date(line: &str, desired: Option<&str>) -> RewriteResult {
    rewrite_emoji_date(line, desired, CANCELLED_EMOJI)
}
```

`rewrite_done_date`'s signature and behavior are preserved verbatim — ADR-0012's 256-case proptest and `done_date_writeback_proptest` stay green, unchanged. The new `rewrite_cancelled_date` gets its own 256-case proptest (a near-clone with `'✅'` → `'❌'` and `extract_done_date` → `extract_cancelled_date`).

### The TUI gesture

New `d` key — toggle the selected task's cancelled state:

- open (` `) → cancelled (`-`)
- cancelled (`-`) → open (` `)
- any other state (done, in-progress, …) → cancelled (`-`) — "press d to mark cancelled"

A `cancel_target_char(raw)` helper mirrors `toggle_target_char`. `submit_cancel` mirrors `submit_toggle`: enqueues a `checkbox` action with `new_char = cancel_target_char(...)`, records a `LastAction::CheckboxToggle` (same variant — cancel *is* a checkbox flip). **Undo is free**: the existing `u` reverses checkbox flips, so `u` after `d` flips back and the composed stamp logic restores `✅`/clears `❌` as appropriate on the reverse flip. No new `LastAction` variant, no new `submit_undo` arm.

## Implementation Notes

1. **`taski-core/src/lib.rs`** — add the `CANCELLED_EMOJI` const and the `rewrite_cancelled_date` wrapper. No other change to taski-core's public API. (The read-path `extract_cancelled_date` already exists from Tier 1.) Add a 256-case proptest for `rewrite_cancelled_date`, near-cloned from `rewrite_done_date`'s proptest (`'✅'` → `'❌'`, `extract_done_date` → `extract_cancelled_date`).

2. **`taski-daemon/src/lib.rs :: process_action_at`** — widen the ADR-0012 stamp decision (currently a two-branch `if is_done_char(new_c) || new_c == ' '`) to a three-state decision over `new_c`:
   - Compute `(desired_done, desired_cancelled)` per the Decision table:
     - done (`x`/`X`) → `(Some(today), None)`
     - cancelled (`-`) → `(None, Some(today))`
     - open (` `) → `(None, None)`
     - other → skip both oracles; `final_line = flipped` (preserves ADR-0012's "leave ✅ untouched" and extends it to ❌)
   - For done/cancelled/open: run `rewrite_done_date` then `rewrite_cancelled_date` on the flipped line; refuse with `DoneDateUnparseable` / `CancelledDateUnparseable` if either returns `Unparseable`; use each oracle's `Rewritten`/`Unchanged` result as the running line.
   - The existing single `atomic_write` is unchanged. Preserve the CRLF `content_end` discipline (ADR-0012) — both oracles operate on the CR-trimmed line.
   - **Regression guard:** the done/open branches must drive the `✅` oracle *exactly* as before so `done_date_writeback_proptest` stays byte-for-byte green. The only additions on those branches are `❌`-oracle calls, which are no-ops (`RewriteResult::Unchanged`) on notes without a `❌` token.

3. **`taski-daemon/src/lib.rs`** — add `is_cancelled_char(ch)` parallel to `is_done_char` (`matches!(ch, '-')`). Add `CancelledDateUnparseable` to `ApplyOutcome` parallel to `DoneDateUnparseable`; wire its failure phrase in the outcome→message match (the TUI's `friendly_failure_reason` generic fallback covers it, but add an explicit match for clarity).

4. **`taski-tui/src/lib.rs`** — add `cancel_target_char(raw)` parallel to `toggle_target_char` (`"-" => " "`, `_ => "-"`); add `submit_cancel(&mut self, conn)` mirroring `submit_toggle` (uses `cancel_target_char`, builds `LastAction::CheckboxToggle`, calls `enqueue_toggle` — which already derives `new_char` from `toggle_target_char`; for cancel, either generalize `enqueue_toggle` to take a target-char source or add a small `enqueue_cancel` helper). Add `KeyCode::Char('d')` → `app.submit_cancel(conn)` to the normal-mode dispatch in `run_loop` (mirror how `b` is handled — `d` is suppressed during a search prompt, same as `b`). Extend `render_failure_notice`'s verb/retry-key map: cancel reuses the `checkbox` action_type, but a refused cancel's retry key is `d` not `Space` — add a way to surface that (e.g. track the originating gesture on `LastAction::CheckboxToggle`, or accept the generic "Toggle/Space" message as a known minor UX loose end).

5. **Proptest** — add `crates/taski-daemon/tests/cancelled_date_writeback_proptest.rs`: 256 cases, near-clone of `done_date_writeback_proptest.rs` with `✅`→`❌`, `rewrite_done_date`→`rewrite_cancelled_date`, `is_done_char`→`is_cancelled_char`, and the flip targets widened to include `-`. Assert: either the on-disk note equals the oracle output with only the target line changed and line count preserved, or it equals the concurrent edit byte-for-byte — never corruption.

## Rationale

- **It's the roadmap's documented next step.** context.md: *"`❌` cancelled-date is the next candidate but depends on a cancel gesture that doesn't exist yet."* This ADR is that gesture, closing the interop-correctness gap for Tasks-plugin "cancelled" queries (the same class of bug ADR-0012 closed for "done" queries).
- **Composing, not splitting, is correct (ADR-0012's argument, verbatim).** A cancelled date that can exist independently of a `- [-]` checkbox is incoherent; coupling the stamp to the flip in a single atomic write makes the invariant structural.
- **The boundary is already open.** ADR-0009 admitted `⏳` and ADR-0012 admitted `✅` under the grammar-provability gate. `❌` is the third (and likely final) dated token admissible under it — the gate itself is unchanged.
- **Reusing the proven machinery.** `atomic_write`'s whole-file TOCTOU re-hash is byte-count-agnostic (ADR-0004 reused unchanged); the pure-oracle + proptest pattern is copied from ADR-0012; the CRLF discipline is copied from `process_metadata_action`/ADR-0012. The genuinely new surface is one enum variant, one const, one oracle wrapper, one TUI key, and the three-state decision in `process_action_at`.
- **No structural mutation.** Unlike a hard-delete feature (considered in depth and rejected — see Alternatives), cancel is a single-line in-place rewrite: the note's line count never changes, no new mutation class is opened, no new safety boundary is needed, and the task remains in the vault (reversible by hand if the TUI restarts and clears `last_action`).

### Why this does not violate ADR-0005

For the same reasons ADR-0009/0012 did not: `❌ YYYY-MM-DD` is native Obsidian Tasks syntax (human-readable, consumed by Tasks/Dataview/Obsidian), not the foreign opaque identity marker ADR-0005 rejected. The surrogate-id + content-hash reconciliation mechanism is untouched. When `❌` is written, `text_hash` changes and `reconcile_note` treats it as delete-old + insert-new — exactly as for `✅`/`⏳` writes. **ADR-0005 is not amended.**

## Consequences

- ✅ Tasks cancelled in Taski (`d`) now appear in Tasks-plugin `cancelled …` queries and Dataview's `status.name === "Cancelled"` — closes the interop-correctness gap symmetric to ADR-0012.
- ✅ `atomic_write`, ADR-0004, and ADR-0005 are all reused unchanged.
- ✅ ADR-0012's `rewrite_done_date`, its proptest, and `done_date_writeback_proptest` stay byte-for-byte unchanged (the three-state restructure preserves the done/open behavior; the new `❌` oracle calls are no-ops on notes without `❌`).
- ✅ Undo-of-cancel is free (`u` already reverses checkbox flips; the composed stamp logic restores `✅`/clears `❌` as appropriate on the reverse flip).
- ✅ Cancel survives TUI restart gracefully — the `- [-]` checkbox and `❌` stamp persist in the vault; if `last_action` is cleared on restart, the user can press `Space`/`d` by hand.
- ⚠️ **ADR-0003 is amended a third time**: write-back scope widens from "checkbox flips + `⏳` scheduled + `✅` done" to "checkbox flips + `⏳` scheduled + `✅` done **+ `❌` cancelled (stamped on cancel flip)**." The amendment records that the grammar-provability gate itself is unchanged — `❌` was already admissible. See the cross-reference note below.
- ⚠️ **`process_action_at`'s stamp decision widens from two-state to three-state.** ADR-0012's two-branch `if` becomes a `(desired_done, desired_cancelled)` decision table over `new_c`. Contained by: (a) both oracles pure and proptested, (b) the existing `done_date_writeback_proptest` staying green (regression guard for ADR-0012), (c) the new `cancelled_date_writeback_proptest`, (d) the unchanged CRLF discipline.
- ⚠️ The `d` key is consumed for cancel. The `u` undo scope widens implicitly (undo already reverses checkbox flips, and cancel is one) — update context.md's keybinding table and "Undo scope" gotcha to mention cancel.
- ⚠️ A `❌` write changes `text_hash`, so the surrogate id churns on the post-apply re-index — fine, expected, identical to `✅`/`⏳` behavior.

### Cross-reference note — ADR-0003 (third amendment)

ADR-0003's amendment block must record a **third** amendment. The grammar-provability gate (ADRs 0009/0012) is reaffirmed unchanged; only the enumeration of admitted tokens widens:

> The write-back scope is widened from **checkbox-state flips + `⏳` scheduled + `✅` done (stamped on flip)** to **checkbox-state flips + `⏳` scheduled + `✅` done + `❌` cancelled (stamped on cancel flip)**. The ADR-0009 principled boundary is **unchanged**: Taski may write tokens that (i) are standard Obsidian Tasks syntax, (ii) have a single unambiguous insertion grammar, and (iii) are produced by a pure, proptested line-rewrite with a "never-corrupts" contract. Free-text edits, creates/deletes, and arbitrary metadata remain explicitly rejected. Each subsequent token type still requires its own ADR.

`❌` is the third (and likely final) dated token admitted under the gate; the gate itself is not widened.

## Alternatives considered

- **Hard delete (remove the line from the vault).** Considered in depth — a full ADR-0013 was drafted for it and then withdrawn. Delete is the first **structural mutation**: it changes the note's line count, requiring a new principled boundary (a "removal-only" gate parallel to the grammar-provability gate), a paired `restore_task` action type, a schema bump for `expected_note_hash`, a positional re-insertion oracle, and a documented restart-data-loss edge (an accidental `d` then `q` is unrecoverable — the task text is gone from the vault). Cancel delivers the same UX intent ("press a key, task is dealt with, press `u` to reverse") at a fraction of the code, with strictly better safety properties (no structural mutation, no schema change, no restart data loss), and closes the documented roadmap gap rather than opening a new mutation class. If true physical deletion is later required, the cancel machinery (`❌` stamp + `d` gesture) is a prerequisite, not wasted work.
- **New `set_cancelled` action type (mirror ADR-0009 slavishly).** **Rejected** for the same reason ADR-0012 rejected `set_done_date`: decouples the cancelled date from the flip it belongs to, doubles writes, opens a partial-failure window, provides no value. Composition is strictly better.
- **Stamp `❌` only on `[-]`, never clear on un-cancel.** **Rejected** (parallel to ADR-0012's symmetry argument): breaks Tasks-plugin semantics (an open task with a stale `❌` is incoherent) and the symmetry with `✅`/`⏳` togglability.
- **Add a `Status::Cancelled` variant.** Tempting for type-safety, but the stamping logic keys off `new_c` (a `char`) via `is_done_char`/`is_cancelled_char` helpers to keep the hot path allocation-free (per the existing `is_done_char` comment). Adding a Status variant would touch the enum, `from_checkbox_char`, `to_checkbox_char`, and every exhaustive match — scope creep for no functional gain. The helper approach mirrors ADR-0012 exactly.
- **Defer until a multi-token metadata-ADR.** **Rejected:** this is a correctness gap against the ecosystem Taski interoperates with (the roadmap frames `❌` as the documented next candidate), not a feature; deferring delays the interop fix for no design benefit.

## Edge cases

| Case | Behavior |
|---|---|
| Flip ` ` → `-`, no existing `❌` | Append ` ❌ <today>` at logical line end. |
| Flip ` ` → `-`, existing `❌ <other>` | **Replace** the date with `<today>` (canonical re-cancel). |
| Flip ` ` → `-`, existing `❌ <today>` | `rewrite_cancelled_date` returns `Unchanged`; only the flip is written. Idempotent. |
| Flip `-` → ` `, existing `❌` | Remove the `❌` token and its single preceding space. |
| Flip `-` → ` `, no `❌` | `rewrite_cancelled_date` returns `Unchanged`; only the flip is written. |
| Flip `x` → `-` (done → cancelled) | Clear `✅`, stamp `❌ <today>`. |
| Flip `-` → `x` (cancelled → done) | Stamp `✅ <today>`, clear `❌`. |
| Flip to/from `Status::InProgress` (`/`) | Leave both `✅` and `❌` untouched; only the flip is written. Ambiguous. |
| Existing `❌` is malformed | Refuse the whole action with `CancelledDateUnparseable`; **no flip, no stamp**, vault untouched. |
| Existing `✅` is malformed (on a cancel transition that would clear it) | Refuse with `DoneDateUnparseable` (ADR-0012, unchanged). |
| Concurrent Obsidian edit (any line) | `note_hash` mismatch → `ConflictNoteChanged` → refuse (ADR-0004, unchanged). |
| Recurring task (`🔁`) | Oracle treats `🔁` as ordinary trailing content, preserved byte-for-byte. |
| CRLF-terminated note | Both oracles operate on the CR-trimmed line; `\r\n` preserved outside the spliced range (ADR-0012 discipline). |
| Trailing tags / `📅` / `⏳` / `✅` on the line | Append `❌` after the last non-newline content; preserve every other byte. |

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; the stamp composes into the existing checkbox action, no new routing.
- [ADR-0003](./0003-checkbox-only-mvp.md) — **amended a third time** by this ADR.
- [ADR-0004](./0004-refuse-on-conflict.md) — refuse-on-conflict / TOCTOU; **reused unchanged**.
- [ADR-0005](./0005-surrogate-identity.md) — **not amended**; `❌` is native Obsidian syntax, not an identity marker.
- [ADR-0009](./0009-scheduled-date-today.md) — the grammar-provability gate this ADR admits `❌` under.
- [ADR-0011](./0011-bullet-toggle-undo.md) — the `u` undo model this ADR's cancel gesture reuses for free.
- [ADR-0012](./0012-done-date-on-toggle.md) — the direct template: same compose pattern, same `rewrite_emoji_date` core, same CRLF discipline; this ADR is its `❌` sibling.
- [`docs/context.md`](../context.md) "Deferred" — *"`❌` cancelled-date is the next candidate but depends on a cancel gesture that doesn't exist yet."* This ADR is that gesture.
