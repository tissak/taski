# ADR-0003: MVP write-back is checkbox-state flips only

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** PRD §10.2 — MVP write-back scope

## Context
Write-back is the project's top data-integrity risk. The full vision includes editing task text, creating/deleting tasks, and writing metadata back into notes — but each adds write-complexity and corruption surface. Shipping the riskiest, broadest write-back first would put the vault at maximal risk before the safety machinery is proven.

## Decision
The **MVP supports only checkbox-state flips**: changing the checkbox character (e.g., `- [ ]` ↔ `- [x]` ↔ `- [/]`). No task-text edits, no creates/deletes, no metadata writes from the TUI in the MVP.

## Rationale
- **Lowest-risk path to a working sync.** A byte-level flip of a single known character is the smallest possible mutation and the easiest to make conflict-safe.
- **Highest day-to-day value.** Toggling task completion is by far the most frequent action; it's the core of "act on tasks from one place."
- **Proves the write-back pipeline** (ADR-0002 routing + ADR-0004 conflict safety) on the simplest case before expanding surface area.

## Consequences
- ✅ Vault integrity risk is minimized while the safety layer is validated.
- ✅ The atomic-write + conflict-refusal machinery is exercised on real data early.
- ⚠️ Text edits, creates/deletes, and metadata write-back are deferred to fast-follow slices — out of MVP scope.
- This decision is intentionally easy to *expand* later without redesign.

## Alternatives considered
- **Full text + metadata write-back in MVP** — rejected; maximal corruption surface before safety is proven; not justified by value/risk trade-off.

## References
- [`docs/tech.md`](../tech.md), [ADR-0002](./0002-write-back-through-daemon.md), [ADR-0004](./0004-refuse-on-conflict.md)
