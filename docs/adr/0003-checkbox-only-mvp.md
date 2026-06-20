# ADR-0003: MVP write-back is checkbox-state flips only

- **Status:** Accepted (amended 2026-06-20 by [ADR-0009](./0009-scheduled-date-today.md))
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

## Amendment — ADR-0009 (2026-06-20): write-back scope widened to Obsidian-standard date-emoji metadata

[ADR-0009](./0009-scheduled-date-today.md) ("mark for today") introduces a scheduled-date
(`⏳ YYYY-MM-DD`) write gesture that this ADR originally excluded. The amendment is scoped
intentionally and recorded here so it does not become an open door:

> The write-back scope is widened from **checkbox-state flips only** to **checkbox-state
> flips + Obsidian-standard date-emoji metadata** (`⏳` scheduled). The original ADR-0003
> rationale still applies in full to everything else: **task-text edits, creates/deletes,
> and arbitrary metadata remain explicitly rejected.**

### Principled boundary (precedent control)

Once date-emoji writes are admitted, requests will follow for priority (`⏫`), recurrence
(`🔁`), tags, and free-text edits. Future amendments are **gated by grammar-provability, not
by precedent**:

> Taski may write tokens that are (i) are **standard Obsidian Tasks syntax**, (ii) have a
> **single unambiguous insertion grammar**, and (iii) are produced by a **pure, proptested
> line-rewrite** with a "never-corrupts" contract (the generalization of the existing
> `writeback_proptest`).

Free-text edits fail (ii)/(iii). Each new token type requires its own ADR.

### Why this does not relax ADR-0004 or ADR-0005

- **ADR-0004 (refuse-on-conflict)** is reused *unchanged*: `atomic_write`'s TOCTOU guard
  re-hashes the *whole file* and is already agnostic to whether the mutation was 1 byte or N.
  The new write path inherits identical conflict semantics.
- **ADR-0005 (no injected marker)** is *not crossed*: `⏳` is native Obsidian Tasks syntax
  (human-readable, consumed by Tasks/Dataview/Obsidian), not the foreign opaque identity
  marker (`%% taski:abc %%`) that ADR-0005 rejected. The surrogate-id + content-hash
  mechanism is untouched.

See ADR-0009 for the full design, the phased delivery, and the alternatives analysis.

## References
- [`docs/tech.md`](../tech.md), [ADR-0002](./0002-write-back-through-daemon.md), [ADR-0004](./0004-refuse-on-conflict.md), [ADR-0009](./0009-scheduled-date-today.md) *(amendment)*
