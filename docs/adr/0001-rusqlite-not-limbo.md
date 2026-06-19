# ADR-0001: Use `rusqlite` (classic SQLite), not Limbo/Turso

- **Status:** Accepted
- **Date:** 2026-06-20
- **Decides:** PRD §10.6 — SQLite engine for the Rust scanner/daemon

## Context
Taski's architecture depends on a **hard requirement**: the Rust background daemon writes extracted tasks to a local SQLite file, while a *separate process* — the TUI client — reads that same file **concurrently** (and may later be written in a non-Rust language). This requires safe multi-process concurrent access to one database file.

Two candidates were evaluated:
- **Limbo** (renamed "Turso Database" in Jan 2025) — a pure-Rust rewrite of SQLite.
- **`rusqlite` + `libsqlite3-sys`** — classic SQLite accessed via FFI.

## Decision
Use **`rusqlite` + `libsqlite3-sys`** with WAL journal mode. Do **not** use Limbo/Turso. Drop the PRD's M0 Limbo spike entirely.

## Rationale
Research (2026-06-20) established a **hard blocker** for Limbo, not a preference:

1. **Default is single-process only.** Turso locks the DB exclusively on open; a second process is rejected with a locking error.
2. **Multi-process support is experimental and non-interoperable.** Turso's `multiprocess_wal` (April 2026) requires *every* participating process to use the Turso SDK. Turso's `COMPAT.md` (Guarantee #4) states verbatim: *"We don't support mixed SQLite and Turso in multi-process scenarios."* A TUI using the standard `sqlite3` library (Python/Node/Go) **cannot read the file concurrently** with a Turso writer.
3. **Still beta.** Turso itself recommends caution for mission-critical use; the on-disk coordination format is explicitly not stable across versions.

For a personal, single-user vault tool, Limbo's only benefit (pure Rust, no C toolchain) is irrelevant, while the cost (betting vault integrity on an immature engine that can't meet the core concurrency requirement) is real and asymmetric.

Classic SQLite WAL is *the* documented, battle-tested pattern for exactly one-writer + many-readers across processes in any language.

## Consequences
- ✅ The architecture's core premise (multi-process concurrent access) is guaranteed.
- ✅ The TUI can be written in any language and read the file with a standard sqlite3 library.
- ✅ M0 spike eliminated; we start building immediately on a known-good core.
- ⚠️ Requires a C toolchain to build `libsqlite3-sys` (acceptable; standard for Rust+SQLite).
- ⚠️ Single-writer concurrency (SQLite's model) — acceptable for our access pattern.

## Alternatives considered
- **Limbo/Turso with `multiprocess_wal`** — rejected (non-interoperable, experimental; see above).
- **Limbo/Turso with all processes using the Turso SDK** — rejected; over-constrains the TUI language choice and bets on a beta engine.

## Revisit when
Turso's `multiprocess_wal` graduates to stable **and** the "no mixed SQLite and Turso in multi-process" restriction is lifted. Until both are true, this decision stands.

## References
- Research report (librarian, ses_11e6cd194ffeFgOiLLksvF4lal): Turso README, `COMPAT.md`, multi-process access docs, issues #769 / PRs #6236, #7350.
- [`docs/tech.md`](../tech.md)
