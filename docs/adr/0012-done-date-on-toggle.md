# ADR-0012: Done-date (`✅`) stamp on toggle (Tasks-plugin interop)

- **Status:** Accepted
- **Date:** 2026-06-21
- **Decides:** How Taski closes the interop-correctness gap where a task toggled done via Taski is invisible to Obsidian Tasks-plugin "done" queries (`done this month`, `not done`, etc.) because the `✅ YYYY-MM-DD` done date is never stamped. **Amends [ADR-0003](./0003-checkbox-only-mvp.md)** (write-back scope) for a second time — the first amendment was [ADR-0009](./0009-scheduled-date-today.md) (`⏳` scheduled).

## Context

Taski already **reads** the `✅` done date (Tier 1, schema v6: `extract_done_date` →
`tasks.done_date`) and the Tasks plugin auto-writes `✅ <completion-date>` on every
completion. Tasks queries like `done this month`, `done after ...`, and Dataview's
`status.name`/`completed` all key off `✅`. **But Taski's toggle gesture (`Space`) flips the
checkbox char only** — it does not stamp `✅`. Result: every task completed via Taski is
silently missing from Tasks-plugin "done" queries in Obsidian. The roadmap classifies this
as a **bug-shaped interop gap**, not a nicety ("correctness against the ecosystem Taski
claims to interoperate with").

### Why this is an amendment to ADR-0003, not a new write gesture

ADR-0003's MVP write scope is "checkbox-state flips only." ADR-0009 widened it once, to
`⏳` scheduled-date metadata, under a **principled boundary** (i): standard Obsidian Tasks
syntax, (ii): single unambiguous insertion grammar, (iii): produced by a pure, proptested
line-rewrite with a "never-corrupts" contract. `✅` meets all three gates identically:

- (i) `✅ YYYY-MM-DD` is canonical Tasks-plugin done-date syntax, human-readable and
  consumed by Tasks/Dataview/Obsidian's own UI.
- (ii) The insertion grammar is identical to `⏳` — same emoji + optional VS16 + whitespace
  + strict `YYYY-MM-DD`, scanned right-to-left by the Tasks parser. The read-path extractor
  `extract_done_date` already shares the grammar via `extract_emoji_date`.
- (iii) The pure oracle `rewrite_done_date` (this ADR) is a thin wrapper over a generalized
  `rewrite_emoji_date`, guarded by its own 256-case proptest — the direct sibling of
  `rewrite_scheduled`.

So `✅` is admissible under the *existing* boundary; this ADR does not widen the boundary,
it admits a second token under it. Precedent remains gated by grammar-provability, not by
"we already write metadata."

### Why the stamp composes into `process_action`, not a new action type

The `⏳` mark-for-today (ADR-0009) is an *independent* user gesture (`t`) — orthogonal to
checkbox state, so it deserved its own `action_type` (`set_scheduled`) and its own daemon
dispatch branch (`process_metadata_action`). The `✅` done date is categorically different:
it is **semantically coupled to the checkbox flip itself**. You stamp `✅` *because* you
completed the task; the stamp without the flip is meaningless, and the flip without the
stamp is the bug.

Splitting the stamp into a second `pending_actions` row would mean: two queue rows for one
`Space` press, two writes, two TOCTOU windows, two conflict checks, and an awkward
partial-failure state (flip landed, stamp refused — now the vault lies). Composing the
stamp **into the same byte splice as the flip** keeps it one write, one hash, one rename,
one atomic outcome. This is the key implementation difference from ADR-0009: no new
`action_type`, no schema change (the existing `payload` column is unused for `checkbox`
rows and stays unused), no new TUI key.

## Decision

On a checkbox flip executed by `process_action`, additionally rewrite the target line via
the pure `rewrite_done_date` oracle **in the same byte buffer** that performs the flip,
before the single `atomic_write`:

- **`[ ]` → `[x]` (or any `Status::Done` char):** also stamp `✅ <today>`. If a parseable
  `✅` token already exists, replace its date with `<today>` (canonical re-done behavior);
  if the existing `✅` already equals `<today>`, the oracle returns `Unchanged` for that
  dimension and only the flip is written. If a malformed `✅` is present, the whole action
  refuses with `DoneDateUnparseable` rather than guessing — the user sees a notice and the
  vault is untouched (no flip, no stamp).
- **`[x]` → `[ ]` (un-complete):** also remove an existing `✅` token (symmetry — you
  cannot be open and have a done date). If no `✅` is present, only the flip is written.
- **Flips involving `Status::InProgress` (`/`) or other non-done/non-open chars:** leave
  the `✅` untouched (ambiguous; do not guess). Only the flip is written.

`<today>` is computed by the pure `taski_core::ymd_from_unix(unix_now())` (no date crate),
identical to the read-path's Today view. The persisted value is that date string.

### The pure oracle

Generalize `taski_core::rewrite_scheduled` into a shared core:

```rust
fn rewrite_emoji_date(line: &str, desired: Option<&str>, emoji: char) -> RewriteResult
```

…identical to today's `rewrite_scheduled` body with the hardcoded `SCHEDULED_EMOJI` const
and the `'⏳'` filter replaced by the `emoji` parameter. The grammar helper
`find_emoji_date_span(line, &[emoji])` and the date validator `parse_date_at` are reused
unchanged (they are already emoji-agnostic). Then:

```rust
const DONE_EMOJI: char = '✅';

pub fn rewrite_scheduled(line: &str, desired: Option<&str>) -> RewriteResult {
    rewrite_emoji_date(line, desired, '⏳')   // ADR-0009 — unchanged behavior
}

pub fn rewrite_done_date(line: &str, desired: Option<&str>) -> RewriteResult {
    rewrite_emoji_date(line, desired, DONE_EMOJI)  // ADR-0012
}
```

`rewrite_scheduled`'s signature and behavior are preserved verbatim — ADR-0009's 256-case
`rewrite_scheduled_proptest` and `metadata_writeback_proptest` stay green, unchanged. The
new `rewrite_done_date` gets its own 256-case proptest (a near-clone of the scheduled one
with `'⏳'` → `'✅'` and `extract_scheduled_date` → `extract_done_date`).

## Implementation Notes

1. **`taski-core/src/lib.rs`** — extract `rewrite_emoji_date` (private) from the existing
   `rewrite_scheduled` body; keep `rewrite_scheduled` as a one-line wrapper; add the
   `DONE_EMOJI` const and the `rewrite_done_date` wrapper. No other change to `taski-core`'s
   public API.

2. **`taski-daemon/src/lib.rs :: process_action`** — the compose point. Between the current
   step 6 (three-way byte verification) and step 7 (the single-char flip splice), insert
   line-decoding + the `rewrite_done_date` call **when the transition warrants it** (see
   Decision). The flip bytes and the rewritten line bytes are spliced into the *same*
   `new_bytes` buffer; a single `atomic_write` follows. Do **not** add a second
   `atomic_write`. If `rewrite_done_date` returns `Unparseable`, refuse the whole action
   with `DoneDateUnparseable` (no flip, no stamp, vault untouched).

   This is the first time `process_action` decodes the target line to `&str`; until now it
   operated purely on byte ranges via `find_checkbox_char_any`. See Consequences for the
   CRLF hazard this introduces.

3. **`taski-daemon/src/lib.rs :: ApplyOutcome`** — add `DoneDateUnparseable`, parallel to
   `MetadataUnparseable` (ADR-0009) and `BulletUnparseable` (ADR-0011). Wire its failure
   message in `process_pending_actions`' outcome → message `match` (the `friendly_failure_reason`
   path in the TUI already has a generic fallback, so a structured reason-code is not
   needed — consistent with the project's deferred reason-codes decision).

4. **Deterministic-date seam.** `<today>` is wall-clock, which would make byte-exact test
   assertions impossible. Factor the date out as a parameter, mirroring the codebase's
   established pattern for env/time-dependent logic (cf. `taski_config::config_path_from`):

   ```rust
   fn process_action_at(
       conn: &Connection,
       vault_root: &Path,
       action: &PendingAction,
       today: &str,
   ) -> Result<ApplyOutcome>;

   pub fn process_action(
       conn: &Connection,
       vault_root: &Path,
       action: &PendingAction,
   ) -> Result<ApplyOutcome> {
       let today = ymd_from_unix(unix_now());
       process_action_at(conn, vault_root, action, &today)
   }
   ```

   Production callers use `process_action`; tests use `process_action_at` with a fixed
   `"2026-06-20"`. The existing checkbox-flip tests (which don't care about the stamp) are
   unaffected — pass any valid date; the flip assertion still holds.

5. **No TUI change.** `Space` already enqueues a `checkbox` action. No new key, no new
   `action_type`, no schema bump, no `tech.md` dependency change.

## Rationale

- **It's a bug, not a feature.** Taski claims Obsidian-Tasks interoperability; tasks
  toggled in Taski vanishing from Tasks "done" queries is silent data loss the user cannot
  see or reason about — exactly the failure mode ADR-0009 cited when rejecting an
  index-only today-flag.
- **Composing, not splitting, is correct.** A done date that can exist independently of a
  done checkbox is incoherent; coupling the stamp to the flip in a single atomic write
  makes the invariant structural rather than aspirational.
- **The boundary is already open.** ADR-0009 admitted `⏳` under a grammar-provability gate
  specifically so a second dated token (`✅`) could follow without re-litigating the
  write-scope question. This ADR exercises that gate; it does not widen it.
- **Reusing the proven machinery.** `atomic_write`'s whole-file TOCTOU re-hash is
  byte-count-agnostic (ADR-0004 reused unchanged); the pure-oracle + proptest pattern is
  copied from ADR-0009; the CRLF discipline is copied from `process_metadata_action`. The
  genuinely new surface is one enum variant and the compose logic in `process_action`.

### Why this does not violate ADR-0005

For the same reasons ADR-0009 did not: `✅ YYYY-MM-DD` is native Obsidian Tasks syntax
(human-readable, consumed by Tasks/Dataview/Obsidian), not the foreign opaque identity
marker (`%% taski:abc %%`) ADR-0005 rejected. The surrogate-id + content-hash reconciliation
mechanism is untouched. When `✅` is written, `text_hash` changes and `reconcile_note`
treats it as delete-old + insert-new — exactly as it already does for any Obsidian edit and
for `⏳` writes. **ADR-0005's mechanism is unchanged; only the content gains a second
standard metadata token.** ADR-0005 is not amended.

## Consequences

- ✅ Tasks completed in Taski now appear in Tasks-plugin `done …` queries and Dataview's
  `completed` field — closes the interop-correctness gap.
- ✅ `atomic_write` and ADR-0004's refuse-on-conflict contract are reused unchanged (the
  composed flip+stamp is one write, one re-hash, one rename).
- ✅ ADR-0009's `rewrite_scheduled`, its proptest, and the metadata write-back proptest are
  all byte-for-byte unchanged — the `rewrite_emoji_date` refactor is behavior-preserving.
- ⚠️ **ADR-0003 is amended a second time**: write-back scope widens from "checkbox flips +
  `⏳` scheduled" to "checkbox flips **+ `⏳` scheduled + `✅` done (stamped on flip)**." The
  amendment records that the boundary itself is unchanged — `✅` was already admissible under
  ADR-0009's gate; this ADR admits it. See the cross-reference note below.
- ⚠️ **New risk surface: `process_action` now does variable-length line surgery.** Until now
  the checkbox path was a fixed single-codepoint swap at a parser-validated structural
  position — the lowest-risk write imaginable, and the reason ADR-0003 scoped MVP to it.
  Composing the `✅` stamp means `process_action` now decodes the line, runs the oracle, and
  splices a variable-length result. Contained by: (a) the pure `rewrite_done_date` oracle,
  exhaustively proptested in isolation (256 cases), (b) an **analogous proptest** to
  `metadata_writeback_proptest.rs` for the composed `process_action` (256 cases: arbitrary
  note + arbitrary existing `✅`/`⏳`/tags state + any concurrent edit → either the on-disk
  note equals the oracle output with ONLY the target line changed and line count preserved,
  or it equals the concurrent edit byte-for-byte — never corruption, never a dropped/added
  line), and (c) the CRLF discipline below.
- ⚠️ **CRLF hazard, newly imported into `process_action`.** `line_byte_range` delimits lines
  on `\n` only, so a trailing `\r` (CRLF notes) is INCLUDED in the line range. `process_action`
  currently doesn't care — it flips a char at line START, far from the `\r`. The composed
  stamp appends/replaces at line END, where the `\r` lives. The implementation MUST compute
  `content_end = if bytes[line_range.end - 1] == b'\r' { line_range.end - 1 } else { line_range.end }`
  and run the oracle on `[line_range.start, content_end)`, exactly as
  `process_metadata_action` does at `crates/taski-daemon/src/lib.rs:842–846`. Splice back into
  `[line_range.start, content_end)`, leaving `bytes[content_end..]` (the `\r\n`) untouched.
  Without this, `✅` is written *between* the CR and LF (`"- [x] task\r ✅ …"`), the next
  `parse_tasks` (which strips a `\r` adjacent to `\n`) folds the CR into the task body, and
  the `text_hash` is permanently polluted. The integration proptest's independent
  `str::lines()`-based assertion (copied from `metadata_writeback_proptest.rs:295–304`) fails
  if this is wrong.
- ⚠️ A `✅` write changes `text_hash`, so the surrogate id churns on the post-apply re-index
  — fine, expected, and identical to the documented `⏳` behavior.

### Cross-reference note — ADR-0003 (second amendment)

ADR-0003's amendment block must record a **second** amendment (the first was ADR-0009). The
boundary text is reaffirmed verbatim; only the enumeration of admitted tokens widens:

> The write-back scope is widened from **checkbox-state flips + `⏳` scheduled** to
> **checkbox-state flips + `⏳` scheduled + `✅` done (stamped on flip)**. The ADR-0009
> principled boundary is **unchanged**: Taski may write tokens that are (i) standard
> Obsidian Tasks syntax, (ii) have a single unambiguous insertion grammar, and (iii) are
> produced by a pure, proptested line-rewrite with a "never-corrupts" contract. Free-text
> edits, creates/deletes, and arbitrary metadata remain explicitly rejected. Each
> subsequent token type still requires its own ADR.

`✅` is the second token admitted under the gate; the gate itself is not widened. Future
candidates (`🛫` start, `➕` created, `🔁` recurrence, priority emojis) remain out of scope
until individually ADR'd, and recurrence in particular remains Tier 3 per the roadmap (date
arithmetic + the "when done vs from-due" distinction).

## Alternatives considered

- **New `set_done_date` action type (`action_type='set_done_date'`), separate `Space`-stamp
  gesture.** Mirrors ADR-0009 slavishly. **Rejected:** decouples the done date from the
  checkbox flip it semantics-belong-to, doubles the writes for one user gesture, opens a
  partial-failure window (flip applied, stamp refused), and provides no value — the stamp
  is only ever wanted *with* a flip. Composition is strictly better.
- **Stamp `✅` only on `[x]`, never clear on `[ ]`.** Simpler; lets the user keep a
  completion history after un-checking. **Rejected:** breaks Tasks-plugin semantics (an
  open task with a `✅` is incoherent and confuses Dataview) and the symmetry with how `⏳`
  is togglable. If a "completion log" is ever wanted, it belongs in a separate gesture
  (e.g. a `➕`-style created-date stamp), not in `✅`.
- **Index-only `done_at` timestamp (no vault write).** **Rejected** for the same reason
  ADR-0009 rejected an index-only today-flag: ephemeral — orphans on any task-text edit
  (text_hash churns the surrogate id), lost on schema-bump DB wipe, invisible to Obsidian /
  Tasks / Dataview. Silent data loss the user cannot see.
- **Defer until Tier 3 / a multi-token metadata-ADR.** Tempting (one ADR covering `✅`/`❌`/
  `🛫`/`➕` together). **Rejected:** this is classified as a correctness bug against the
  ecosystem Taski interoperates with, not a feature; bundling it with lower-value tokens
  delays the fix for no design benefit. `❌` cancelled-date is left for a future ADR (cancel
  is not yet a Taski gesture).

## Edge cases

| Case | Behavior |
|---|---|
| Flip `[ ]` → `[x]`, no existing `✅` | Append ` ✅ <today>` at logical line end (after any trailing tags, before the line terminator). |
| Flip `[ ]` → `[x]`, existing `✅ <other>` | **Replace** the date with `<today>` (canonical re-done). No warning. |
| Flip `[ ]` → `[x]`, existing `✅ <today>` | `rewrite_done_date` returns `Unchanged` for the stamp dimension; only the flip is written. Idempotent. |
| Flip `[x]` → `[ ]`, existing `✅` | Remove the `✅` token and its single preceding space (mirror of `⏳` unmark). |
| Flip `[x]` → `[ ]`, no `✅` | `rewrite_done_date` returns `Unchanged`; only the flip is written. |
| Flip to/from `Status::InProgress` (`/`) | Leave `✅` untouched; only the flip is written. Ambiguous, do not guess. |
| Existing `✅` is malformed (bad date, NBSP, stray VS, two `✅`) | Refuse the whole action with `DoneDateUnparseable`; **no flip, no stamp**, vault untouched. Surface a notice. |
| Concurrent Obsidian edit (any line) | `note_hash` mismatch → `ConflictNoteChanged` → refuse (ADR-0004, unchanged). |
| Recurring task (`🔁`) | `rewrite_done_date` treats `🔁` as ordinary trailing content, preserved byte-for-byte (parallel to `rewrite_scheduled_preserves_recurring_token`). Add one fixture test. |
| CRLF-terminated note | Oracle operates on the CR-trimmed line; `\r\n` preserved outside the spliced range. See Consequences / Implementation Notes. |
| Trailing tags / `📅` / `⏳` on the line | Append `✅` after the last non-newline content; preserve every other byte. (Tasks' right-to-left parser handles arbitrary emoji ordering.) |

## References

- [ADR-0002](./0002-write-back-through-daemon.md) — daemon is sole vault writer; the stamp
  composes into the existing checkbox action, no new routing.
- [ADR-0003](./0003-checkbox-only-mvp.md) — **amended a second time** by this ADR
  (write-back scope widened to `✅` done-on-toggle, under the unchanged ADR-0009 gate).
- [ADR-0004](./0004-refuse-on-conflict.md) — refuse-on-conflict / TOCTOU; **reused
  unchanged** (whole-file hash is byte-count-agnostic; the composed flip+stamp is one write).
- [ADR-0005](./0005-surrogate-identity.md) — **not amended**; `✅` is native Obsidian
  syntax, not an identity marker. Mechanism untouched.
- [ADR-0009](./0009-scheduled-date-today.md) — the template: same grammar gate, same
  pure-oracle + 256-case-proptest pattern, same `extract_emoji_date` / `find_emoji_date_span`
  helpers. The `rewrite_emoji_date` refactor generalizes its `rewrite_scheduled`.
- [`docs/roadmap.md`](../roadmap.md) § "Interop correctness gap" — classifies this as a bug.
- [`docs/context.md`](../context.md) "Gotchas" — the CRLF hazard and the
  `process_metadata_action` CR-trim reference at `lib.rs:842–846`.
- [Obsidian Tasks — Dates](https://github.com/obsidian-tasks-group/obsidian-tasks/blob/main/docs/Getting%20Started/Dates.md)
  — authoritative `✅`/`❌` done/cancelled semantics.
