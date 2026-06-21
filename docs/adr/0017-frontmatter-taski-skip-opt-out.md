# ADR-0017: Frontmatter `taski-skip` opt-out

- **Status:** Accepted
- **Date:** 2026-06-22
- **Decides:** How a single note can opt out of task indexing without excluding a whole
  directory. A note whose YAML frontmatter carries `taski-skip: true` has **no tasks
  indexed** (and any previously-indexed tasks for it are evicted on the next scan). This is
  an **index/read-path** feature; it does **not** amend any write-back ADR, does **not**
  bump the schema, and does **not** add a dependency.

## Context

Not every checkbox in the vault is a task the user wants to triage. The motivating case:
notes that track in-game objectives, habit-style checklists, or templates contain `- [ ]`
lines that are semantically *not* actionable todos. Today every one of them lands in the
unified task list, adding noise.

The existing exclusion mechanism — `exclude_dirs` in `config.toml` — only skips whole
subdirectory trees. It cannot reach individual files that live alongside real-task notes,
and it is a global config edit rather than something co-located with the content it affects.
There was no per-file, content-local way to say "leave this note's checkboxes alone."

## Decision

Add a per-file opt-out via a namespaced YAML frontmatter flag, `taski-skip`. The detection
is a pure function in `taski-core`; the enforcement is a single guard in the daemon's
`index_note` — the one chokepoint both the initial `scan_vault` scan and the live watcher
re-index pass through.

### The flag

```markdown
---
taski-skip: true
---
```

`taski-skip: true` on a top-level frontmatter key means "do not index any task in this
note." The value is truthy only for the literal boolean `true` (case-insensitive) or its
single-quoted/double-quoted variants (`"true"`, `'true'`). Any other value — `false`,
empty, a string, `yes`, `on` — is treated as **not set** (the note indexes normally). This
deliberately rejects YAML-1.1 truthy spellings (`yes`/`on`/etc.) to keep the grammar
predictable and the opt-in explicit.

### Grammar (the single source of truth)

- The note's **first line**, with trailing whitespace/`\r` trimmed, must be exactly `---`.
  (A `---` horizontal rule mid-document is not frontmatter and is ignored.)
- Lines are scanned until the **first closing `---`** (same trim rule). Lines after the
  closing fence are never examined for this flag. If no closing `---` is found, the note is
  treated as having no frontmatter (the flag is absent).
- Within the frontmatter block, a **top-level** (column-0) line matching
  `taski-skip\s*:\s*(.+)?` is the key. Indented/nested keys with the same name are ignored
  to avoid matching nested-map values.
- First such key wins. Its value, trimmed, is truthy iff it case-insensitively equals `true`
  or equals `"true"` / `'true'`.

### Enforcement

`index_note` (`crates/taski-daemon/src/lib.rs`), immediately after the UTF-8 decode and
**before** `parse_tasks`, calls `taski_core::taski_skip_enabled(markdown)`. If true:

1. `db::reconcile_note(conn, &rel, &[], Some(&hash), mtime)` — reconcile with an **empty**
   task list. `reconcile_note`'s unmatched-row delete (`taski-db/src/lib.rs:428–433`) fires
   for every old row, so this **evicts** any tasks indexed before the flag was
   added. This is the exact mechanism that makes adding/removing the flag self-healing: the
   next scan within ~1s reflects the new state.
2. The `note_contents` cache write (`upsert_note_content`) is **skipped** — a note whose
   tasks are suppressed will never be surfaced in the context pane, so caching its body is
   dead weight. (Harmless to keep; skipped for a leaner index.)
3. Return `Ok(0)`.

### Why this is index-path only

No `pending_actions` are involved. No note is mutated. No write-back ADR is touched:
- A skipped note has **no task rows** in `tasks`, so the TUI cannot enqueue an action
  against any of its checkboxes. There is simply nothing to write back.
- If the flag is added *while* a toggle is mid-flight, the task row is evicted on the next
  re-index; the pending `process_action` then finds no row and refuses safely (not
  corruption — a normal "task vanished" refusal).
- The TUI still **never opens a vault file** (ADR-0002/0006 unchanged).

## Implementation Notes

1. **`crates/taski-core/src/lib.rs`** — add `pub fn taski_skip_enabled(markdown: &str) ->
   bool` with a doc comment restating the grammar. Pure (no FS, no I/O), so it is cheaply
   unit-testable like the rest of the parser. No new `Task` field, no schema change.
2. **`crates/taski-daemon/src/lib.rs`** — the guard in `index_note`, after the `from_utf8`
   block and the `content_hash`/`note_mtime` capture. (The hash/mtime are computed
   unconditionally — cheap — and then discarded: with zero task rows after eviction there is
   nothing to record them on, which is fine, since a note with no indexed tasks has nothing
   to conflict-check.)
3. **Tests:**
   - `taski-core` inline unit tests: flag present/absent, quoted/unquoted, `false`/`yes`/
     empty → not truthy, `---` not on line 1, no closing fence, nested indented key ignored,
     key inside a fenced code block at top of file not mistaken for frontmatter, CRLF line
     endings, case variants of `true`.
   - `taski-daemon/tests/scan.rs`: a skipped note yields 0 tasks; and the flag toggled onto
     an already-indexed note evicts its tasks on rescan.

## Rationale

- **Co-located with the content.** The flag lives in the very note it affects, so its
  meaning is obvious to anyone reading the vault (including future-you). `exclude_dirs`
  edits a distant config file and operates at directory granularity.
- **One chokepoint, two paths covered.** Putting the guard in `index_note` means the initial
  scan *and* the debounced live re-index honor it identically — no separate watcher logic,
  no startup purge (unlike `exclude_dirs`, which needs `delete_tasks_for_excluded_dirs` on
  startup because the scan prunes directories *before* `index_note` runs). Here eviction is
  a free consequence of reconciling with an empty list.
- **Self-healing on toggle.** Adding or removing the flag takes effect on the next scan
  (~1s), evicting or rehydrating rows through the normal reconciliation path. No manual
  purge command.
- **Minimal blast radius.** Read-path only, no schema bump, no new dependency, no daemon
  write-path change. The pure detector keeps `taski-core` testable in isolation.
- **Explicit opt-in.** The key is namespaced (`taski-`) so it cannot collide with other
  tools' frontmatter, and only the literal `true` is honored so a stray `taski-skip: false`
  or a commented-out line never accidentally suppresses tasks.

## Consequences

- ✅ A note with `taski-skip: true` contributes zero tasks to the index and zero rows to the
  TUI. Its checkboxes still work normally in Obsidian.
- ✅ Toggling the flag rehydrates/evicts tasks on the next scan via reconciliation. Identity
  (`text_hash`) is preserved across an evict→rehydrate cycle in the sense that a re-added
  task is re-inserted with a fresh surrogate `id` (ADR-0005 — `id` is never reused; this is
  expected and harmless for a personal tool).
- ⚠️ A skipped note's body is not cached in `note_contents`. If it is later un-skipped, the
  cache is repopulated on the next scan. No data is lost (the vault is the source of truth).
- ⚠️ Only top-level, first-block frontmatter is honored. A flag buried in a nested YAML map
  or past the closing `---` is intentionally ignored.

## Alternatives considered

- **Config-based per-file exclude list** (e.g. `exclude_files` in `config.toml`). **Rejected
  as the primary mechanism.** It is not co-located with the content, requires a daemon
  restart to take effect reliably, and duplicates the mental model of `exclude_dirs` at a
  finer grain. (A future `exclude_files` config field could complement this for power users,
  but frontmatter is the ergonomic default.)
- **A `#taski-skip` tag (or any inline tag) instead of frontmatter.** **Rejected.** A tag is
  per-task, not per-note — the user wants whole-note suppression. A note-level tag convention
  is also less discoverable and would require the parser to special-case a tag, entangling
  the tag-extraction grammar (Tier 1) with control flow. Frontmatter is the standard
  note-metadata location in Obsidian.
- **Honor full YAML 1.1 truthy values (`yes`/`on`/`True`/…).** **Rejected.** Obsidian/JS-YAML
  uses `true`/`false`; accepting `yes`/`on` invites surprises (e.g. a note about a game with
  `taski-skip: on` meaning "turn on" vs "boolean true"). Explicit `true` only.
- **Stop reading the note at the frontmatter parser level** (skip frontmatter lines in
  `parse_tasks`). **Rejected as the mechanism.** The parser already can't produce checkbox
  tasks from well-formed frontmatter (a `key: value` line is not `- [ ] …`), so there is no
  correctness gap to fix there; the requirement is *suppression of real checkbox lines below
  the frontmatter*, which is a note-level policy, not a line-level parse rule. The guard in
  `index_note` expresses that policy in exactly one place.
- **Defer until there is a "hide this note" TUI gesture.** **Rejected.** This ADR is
  read-path only and unblocks the user's immediate noise problem; a TUI-driven toggle would
  be a write-back feature (mutating frontmatter) and a separate, larger ADR.

## Edge cases

| Case | Behavior |
|---|---|
| No frontmatter | Not skipped; indexes normally. |
| Frontmatter present, no `taski-skip` key | Not skipped. |
| `taski-skip: true` | **Skipped** — 0 tasks; existing rows evicted on rescan. |
| `taski-skip: false` / `taski-skip: "false"` | Not skipped. |
| `taski-skip: yes` / `on` / `1` | Not skipped (only literal `true` honored). |
| `taski-skip:` (empty value) | Not skipped. |
| `taski-skip: true` indented (nested map) | Not skipped (top-level key only). |
| `---` not on line 1 (e.g. a blank line first) | No frontmatter recognized; not skipped. |
| `---` horizontal rule in the body | Ignored — only the line-1 block is frontmatter. |
| Flag inside a ``` ``` fenced block at the top | Not frontmatter (line 1 isn't `---`); not skipped. |
| No closing `---` | No frontmatter recognized; not skipped. |
| Flag added to an already-indexed note | Tasks evicted on next rescan (~1s). |
| Flag removed | Tasks re-indexed on next rescan; note content re-cached. |
| CRLF line endings | Handled (trailing `\r` trimmed on the fence comparison). |
| `taski-skip: true # game` (YAML inline comment) | Not skipped — strict literal match; the value is `true # game`, not `true`. Intentional: the value must be exactly `true`. |
| Leading BOM (`\u{FEFF}---`) on line 1 | Not skipped — line 1 is not exactly `---`. Rare in practice; strip the BOM in Obsidian if affected. |
| Indented closing fence (`  ---`) | Does **not** close the block (only trailing whitespace is trimmed, not leading). An indented line is ignored; only a column-0 `---` ends frontmatter. |
| Pending toggle against a just-skipped task | Row evicted → `process_action` finds no row → safe refusal (not corruption). |

## References

- [ADR-0005](./0005-surrogate-identity.md) — **not amended**; eviction re-inserts with a
  fresh surrogate `id` (never reused), which is expected.
- [ADR-0006](./0006-note-content-cached-in-index.md) — the skipped note's body is not cached;
  repopulated on un-skip. Read path only.
- `exclude_dirs` (context.md decision #11) — the directory-level sibling; this ADR is its
  per-file, content-local complement. Notably `exclude_dirs` lacks its own ADR; this one
  exists because the frontmatter grammar is a load-bearing contract future parsing must
  respect.
- [`docs/context.md`](../context.md) — decision list, testing-strategy table, and quick-ref
  updated for the flag.
- [`docs/tech.md`](../tech.md) — Scanner/Daemon note added.
