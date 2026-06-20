# CR1 — Taski v0.4 Code Review

- **Date:** 2026-06-20
- **Reviewer:** Oracle (zai-coding-plan/glm-5.1)
- **Scope:** Full workspace review — correctness, maintainability, API surface, test gaps, idiomatic Rust
- **Baseline:** `docs/context.md` (v0.4), all 11 ADRs, all 6 crate sources, CI config, integration tests
- **Outcome:** 1 must-fix (undo bug), 6 should-fix, 5 nice-to-have

---

## Summary

The codebase is in strong shape for a v0.4 personal tool. The write-back path is layered with defense-in-depth (content-hash + three-way byte verify + `atomic_write` TOCTOU re-hash + post-apply re-index + single-writer flock). The `taski-core` purity discipline pays off in testability. The proptests encode real safety contracts.

The biggest concern is a **real bug in checkbox undo** (`u` after `Space`) where the action is enqueued without swapping `expected_char`/`new_char`, causing the undo to silently no-op — or, if the original toggle failed, to *apply* the original flip instead of reversing it. Secondary concerns are minor: an `eprintln!` from inside the TUI's alt-screen path, missing tests for `process_bullet_action` and `submit_undo`, and some documentation drift.

---

## Must-fix (correctness/safety)

### M1. Checkbox undo (`u`) does not reverse the toggle — it silently no-ops or applies the original flip

**Where:** `crates/taski-tui/src/lib.rs:814–828`

```rust
LastAction::CheckboxToggle {
    task_id, note_path, line_number, expected_char, new_char,
} => enqueue_undo_checkbox(
    conn, task_id, &note_path, line_number,
    &expected_char,   // BUG: passed as-is, not swapped
    &new_char,        // BUG: passed as-is, not swapped
),
```

**What's wrong.** `LastAction::CheckboxToggle` is populated in `submit_toggle` (`lib.rs:771–777`) with `expected_char = task.raw_checkbox_char` and `new_char = toggle_target_char(...)` — i.e. the **original** flip's semantics (e.g. `expected=" ", new="x"` for an open→done toggle). The `enqueue_undo_checkbox` docstring (`lib.rs:1042–1043`) explicitly says "expected_char and new_char are explicitly provided (swapped from the original)", but the call site never swaps them. The undo therefore enqueues the *same* action as the original.

**Why it matters.** Trace of the two cases (assuming daemon processed the original before `u`):

- **Original toggle succeeded** (task went `[ ]` → `[x]`; post-apply re-index updated `row.raw_checkbox_char` to `"x"`):
  The undo action carries `expected=" ", new="x"`. In `process_action` (`taski-daemon/src/lib.rs:729–737`) the **idempotency check fires first**: `on_disk_c ("x") == new_c ("x")` → returns `ApplyOutcome::Applied` *without writing*. The undo "succeeds" but the task stays done. The user pressed `u` and nothing visibly happened.

- **Original toggle failed** (task still `[ ]`; `row.raw_checkbox_char` still `" "`):
  The undo action carries `expected=" ", new="x"`. `on_disk_c (" ") != new_c ("x")` → not idempotent → `action.expected_char (" ") == row.raw_checkbox_char (" ")` → guard passes → `on_disk_c (" ") == expected_c (" ")` → flip proceeds → **the undo actually applies the original flip**. The user pressed `u` to undo a failed toggle and instead it gets applied.

The docs claim (`docs/context.md:358–359`, ADR-0011 summary) that "if the original failed, the undo fails naturally because the daemon re-verifies current state" — but that is not what happens with this bug.

**Fix.** Swap the arguments at the call site:
```rust
} => enqueue_undo_checkbox(
    conn, task_id, &note_path, line_number,
    &new_char,       // the undo's expected = original's target state
    &expected_char,  // the undo's target   = original's prior state
),
```
Then add a regression test (see S3 below — there is currently *no* test exercising `submit_undo`, which is why this slipped). The bullet-undo path on line 833 is correct (it relies on `toggle_bullet`'s self-inverse property, verified by `toggle_bullet_is_self_inverse_for_open_checkboxes`), so this bug is specific to checkbox undo.

---

## Should-fix (maintainability/clean-up)

### S1. `eprintln!` from inside the TUI garbles the alternate screen

**Where:** `crates/taski-tui/src/lib.rs:745` (`sync_context`) and `:856` (`track_enqueued`).

**What's wrong.** The TUI owns the alternate screen + raw mode for the whole session. Any write to stderr from the TUI thread interleaves with ratatui's diff-based rendering and corrupts the display (cursor jumps, ghost bytes). The docs ("Gotchas → Combined mode routes daemon tracing to the log file, never stderr") correctly forbid this on the *daemon* thread but the same hazard applies to the *TUI* thread — and these two call sites fire on real DB errors (note-content read failure, enqueue failure).

**Fix.** Either silently drop the error (the surrounding pattern is already "log and never propagate") or convert to `tracing::warn!`. Since the TUI doesn't initialize a subscriber in standalone mode, the cleanest minimal fix is to swallow the error inline (`let _ = e;`) — the user already gets UX feedback for enqueue failures via `app.notice` on the next refresh.

### S2. Documentation drift: action type is `toggle_bullet`, not `convert_bullet`; `enqueue_undo_action`/`enqueue_convert_bullet` don't exist

**Where:** `docs/context.md` lines 47, 274, 354, 548, 633 vs. the actual code.

**What's wrong.** The code consistently uses action type `"toggle_bullet"` and the public DB function is `enqueue_bullet_toggle` (`crates/taski-db/src/lib.rs:489`, dispatched at `taski-daemon/src/lib.rs:589`, matched in the TUI at `lib.rs:1099`). The docs call it `convert_bullet` and reference `enqueue_convert_bullet` / `enqueue_undo_action`, neither of which exists. Undo in the code is implemented inline in the TUI via `enqueue_action` + `enqueue_bullet_toggle`, not via a dedicated `enqueue_undo_action`.

**Why it matters.** The onboarding manual is the load-bearing doc; naming drift here will mislead anyone touching the action-dispatch path (and made reviewing this area slower than it should have been). Worth a single docs-only commit aligning names to the code.

### S3. Missing integration tests for `process_bullet_action` and the `convert_bullet`/`undo` paths (ADR-0011)

**Where:** `crates/taski-daemon/tests/writeback.rs` (entire file).

**What's wrong.** `docs/context.md:523` claims `writeback.rs` "Also covers `convert_bullet` and `undo` action types (ADR-0011)." It does not — the file exercises `process_action` and `process_pending_actions` only; there is no test calling `process_bullet_action`, and no test for `submit_undo` in the TUI. ADR-0011 added a *new vault write path* (`process_bullet_action` → `toggle_bullet` → `atomic_write`), and bullet toggling does variable-length line surgery structurally similar to the metadata path that *did* get a 256-case proptest.

**Fix (priority order):**
1. One TUI unit test that enqueues a checkbox toggle, resolves it `done`, then calls `submit_undo` and asserts the resulting pending action has swapped `expected_char`/`new_char` — this would have caught M1.
2. One integration test in `writeback.rs` that runs `process_bullet_action` to completion (applied), and one that confirms refusal on `ConflictNoteChanged` — mirrors `flip_open_to_done_applied_unchanged_elsewhere` and `flip_refused_on_concurrent_edit_leaves_file_unchanged`.
3. (Optional) A 256-case `bullet_writeback_proptest.rs` cloned from `metadata_writeback_proptest.rs`, with the oracle = `toggle_bullet`. Cheap to write given the existing template; would lock in the byte-preserve invariants for the bullet path the same way the metadata path is locked.

### S4. `last_action` is set even when the enqueue failed

**Where:** `crates/taski-tui/src/lib.rs:780–781` (`submit_toggle`) and `:801–802` (`submit_bullet_toggle`).

**What's wrong.** `self.last_action = last_action;` runs before `track_enqueued(result, ...)`. If `enqueue_toggle`/`enqueue_bullet_toggle` returns `Err`, `last_action` is still updated. A subsequent `u` will try to undo a write that was never enqueued. The daemon's re-verification makes this *safe* (the undo will fail with `TaskLineMismatch` or be idempotent), but it's surprising and a small footgun for future changes.

**Fix.** Move `self.last_action = last_action;` inside the `Ok` arm of `track_enqueued`, or have `submit_toggle` return early on enqueue error.

### S5. Confirmed: `thiserror = "2.0"` is a stale unused workspace dependency

**Where:** `Cargo.toml:36`.

**What's wrong.** No crate's `Cargo.toml` references `thiserror` (verified by reading all six per-crate manifests). The `Cargo.lock` entries are all transitive (via `proptest`/`rusqlite`/etc.). `docs/context.md:454–456` already flags this. Safe to delete the line.

### S6. Duplicate (stale) doc-comment paragraphs on `init_tracing`

**Where:** `crates/taski-daemon/src/lib.rs:1183–1186`.

```rust
/// Initialise `tracing` stderr output. Honors `RUST_LOG`; defaults to `info`. Safe to
/// call when a subscriber is already installed (e.g. when running under a test).
/// Initialize `tracing` to stderr at `info` (overridable via `RUST_LOG`). Used by the
/// standalone daemon entry points; the unified launcher reuses this for `taski daemon`
/// and installs its own file-sink subscriber for combined mode.
pub fn init_tracing() {
```

Two near-identical doc paragraphs concatenated. Looks like an edit left the old one behind. Collapse to a single paragraph.

---

## Nice-to-have (idiomatic/polish)

- **`process_pending_actions` re-indexes the whole note after every `Applied`** (`taski-daemon/src/lib.rs:622–629`). For a burst of N pending actions on the same note this is O(N²) in file reads/parses (each one triggers `index_note` → `parse_tasks` → `reconcile_note` → `upsert_note_content`). Correctness is fine; just noting that batched toggles on the same note are more expensive than they look. Not worth changing for a single-user tool.

- **`toggle_bullet` is "never-panics"-tested only as a sampling unit test** (`taski-core/src/lib.rs:1119–1137`), not a proptest. By contrast `rewrite_scheduled` has a 256-case property test. Given `toggle_bullet` is also a vault write oracle, promoting the sampling test to a small proptest (just feed arbitrary `String`s and assert no panic) would match the project's own testing discipline.

- **`process_bullet_action` does not trim a trailing `\r`** before calling `toggle_bullet` (`taski-daemon/src/lib.rs:925`), unlike `process_metadata_action` (`:842–846`). This is *correct* — `toggle_bullet` does prefix surgery and preserves the body (including `\r`) byte-for-byte, so no CR-trim is needed. Worth a one-line code comment noting *why* it's intentionally different from the metadata path, so a future maintainer doesn't "fix" it.

- **The `process_pending_actions` outer `loop` is theoretically unbounded** if the TUI enqueues faster than the daemon drains. Not realistic for a human-speed single user; mentioning only for completeness.

- **`track_enqueued` docstring references `submit_toggle` specifically** (`lib.rs:841–843`) but actually applies to all write gestures (toggle, set_scheduled, bullet, undo). The list is now stale — reword to "shared so all write gestures stay consistent".

---

## Explicitly NOT recommending

Considered and rejected — covered by an ADR, the Deferred list, or the single-user scope:

- **Undoing ADR-0001 (move off rusqlite)** — ADR-0001 names the concrete blocker (Limbo's multi-process WAL).
- **Undoing ADR-0004 (last-write-wins instead of refuse-on-conflict)** — would re-introduce the exact data-loss vector ADR-0004 prevents.
- **Undoing ADR-0008 (drop the flock single-writer lock)** — `atomic_write`'s fixed-name `.taski.tmp` + `reconcile_note`'s read-modify-write race (neither guarded by ADR-0004's TOCTOU check) make the lock load-bearing.
- **Adding `fsync(dirfd)` after rename** — explicitly deferred.
- **Unique temp-file names** — deferred; single-writer model removes the collision vector.
- **Retry-once on conflict** — deferred.
- **Real DB migration path** — deferred (schema bumps drop+recreate).
- **Structured reason-codes daemon→TUI** — deferred (string-matching + fallback is fine for one user).
- **`pulldown-cmark` parser** — deferred until real edge cases bite.
- **Optimistic TUI updates** — deferred (current "wait for confirmation, never lie" behavior is correct).
- **Undo of `t` (mark-for-today)** — explicitly out of undo scope; `t` is already idempotent.
- **Case-sensitive search toggle / date search** — deferred.
- **`cargo-deny` / supply-chain CI step** — out of MVP scope per the docs; the three CI gates (`fmt`/`clippy -D warnings`/`test`) are appropriate.
- **Switching `anyhow` to typed errors via `thiserror`** — the codebase consistently uses `anyhow::Result` for I/O-bound call sites and the daemon's typed `ApplyOutcome` enum at the boundary that actually needs discrimination. The two-tier split is the right one; `thiserror` would pull its weight only if more layers needed structured errors.
- **Multi-process / auth / multi-tenancy concerns** — explicitly out of scope for a personal tool.

---

## Top 3

If you only do three things:

1. **Fix M1 (checkbox undo arg swap)** — one-line change at `crates/taski-tui/src/lib.rs:821–828`, immediately followed by a regression test asserting that `submit_undo` after a successful toggle enqueues the *reverse* flip. This is the only issue that affects user-visible correctness on a documented feature.

2. **Add the missing `process_bullet_action` + `submit_undo` tests (S3)** — the doc claim that ADR-0011 paths are covered is currently false. ADR-0011 introduced a new vault write type and a new TUI gesture; both should have at least the level of coverage the checkbox path has. This is also what surfaces M1.

3. **Clean up the docs/code naming drift (S2) and remove the stale `thiserror` workspace dep (S5)** — both are pure-deletion cleanups that pay off the next time anyone touches the action-dispatch path or the workspace manifest. Cheap, no risk, and they remove real onboarding friction (the `convert_bullet` vs `toggle_bullet` divergence made this review slower than it needed to be).
