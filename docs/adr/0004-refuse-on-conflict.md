# ADR-0004: Refuse-on-conflict for write-back (never last-write-wins)

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** PRD §10.5 — conflict policy when Obsidian and Taski both edit a note

## Context
Notes are edited concurrently by Obsidian (the user) and, during write-back, by Taski. If Taski overwrites a note that the user changed since the last scan, it silently clobbers their work. In a personal vault, losing a note edit is catastrophic (lost work, eroded trust), while the cost of *not* applying a Taski action is trivial (the user just acts again).

## Decision
**Optimistic concurrency with refuse-on-conflict.** Before any write-back mutation:

1. Re-read the note and compute its current content hash + mtime.
2. Compare against the hash + mtime captured at the last scan of that task.
3. **If unchanged → apply** the write atomically (temp file → `fsync` → `rename`).
4. **If changed → REFUSE.** Re-scan the note. If the target task still exists at the expected location with the expected raw checkbox bytes, retry **once**; otherwise mark the action `failed` and surface "note changed externally — action not applied" to the TUI.

**Last-write-wins is explicitly rejected for the MVP.**

The write must also re-verify the exact bytes at the target line match the recorded `raw_checkbox_char` before flipping — the DB row is treated as a *claim*, not truth.

## Rationale
- **Asymmetric cost favors refusal.** A refused action is a minor annoyance; a clobbered Obsidian edit is data loss.
- **Detectable & recoverable.** A refusal is surfaced to the user; silent corruption is not.
- Composes cleanly with the single-writer model (ADR-0002) and checkbox-only scope (ADR-0003).

## Consequences
- ✅ Obsidian edits are never silently overwritten by Taski.
- ✅ Behavior is verifiable by a property test ("for any note + concurrent edits, Taski either writes correctly or refuses safely — never corrupts").
- ⚠️ Rare false refusals require the user to re-trigger an action — acceptable in a personal tool.
- ⚠️ The note's mtime resolution and rapid double-edits need care in implementation.

## Alternatives considered
- **Last-write-wins** — rejected; risks clobbering user edits.
- **Merge** — rejected for MVP; too complex for the value, and text-merge of Markdown checkboxes is error-prone.

## References
- [`docs/tech.md`](../tech.md), [ADR-0002](./0002-write-back-through-daemon.md), [ADR-0003](./0003-checkbox-only-mvp.md)
