# ADR-0002: Write-back routes through the daemon (single writer to the vault)

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** PRD §8 — write-back control flow

## Context
Write-back (UI actions editing the originating Markdown) is the highest-risk MVP feature. A naive design where both the TUI and the daemon can write to vault files creates races: the daemon's re-indexer could read/rewrite a note while the TUI is editing it, or two write paths could clobber each other. The original PRD §8 phrase "TUI coordinates write-back" was ambiguous and invited exactly this class of bug.

## Decision
**The TUI never touches vault files directly.** Write-back is routed through the daemon:

1. The TUI inserts a row into a **`pending_actions`** table in the shared SQLite database describing the requested change (target task, desired new state).
2. The daemon polls `pending_actions`, performs the actual file write (atomic, conflict-checked per ADR-0004), and marks the row `done` / `failed`.
3. The daemon's own re-index then observes the new file state and updates the task index.

**The daemon is the single writer to both the database and the vault.**

## Rationale
- **Eliminates an entire class of races** by making the vault single-writer.
- **Reuses the existing SQLite boundary** as the IPC mechanism — no new socket/pipe channel is required for the MVP.
- **Centralizes the safety logic** (atomic writes, conflict checks, identity re-verification) in one place rather than duplicating it in the TUI.
- A few-hundred-ms poll latency is imperceptible for checkbox flips.

## Consequences
- ✅ No TUI↔vault file coupling; the TUI stays a pure reader + action-requester.
- ✅ All write-back safety code lives in the daemon and is testable there.
- ⚠️ Write-back latency is bounded by the daemon's `pending_actions` poll interval (acceptable for MVP).
- A Unix-domain-socket command channel for snappier feedback is a fast-follow if latency demands it.

## Alternatives considered
- **TUI writes files directly** — rejected; races with daemon re-index, duplicates safety logic, couples TUI to vault layout.
- **Separate IPC channel (socket/pipe) for commands** — deferred; SQLite `pending_actions` is simpler and sufficient for MVP.

## References
- [`docs/tech.md`](../tech.md), [ADR-0003](./0003-checkbox-only-mvp.md), [ADR-0004](./0004-refuse-on-conflict.md)
