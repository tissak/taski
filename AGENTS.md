# Taski — Agent Instructions

## Read context first (required)

Before doing **any** work in this repo — code changes, tests, docs, refactors, debugging,
research that informs a change — **read [`docs/context.md`](./docs/context.md) in full.**

It is the operating manual for Taski: architecture, the two data flows, the data model,
every load-bearing ADR, and the landmines that will bite you if you don't know they're
there. `context.md` itself says it best:

> Read this first, then the `PRD`, `tech.md`, and the ADRs.

A request almost never contains enough context on its own. `docs/context.md` is where the
context lives. Skipping it is how load-bearing decisions get casually undone and how the
documented landmines get stepped on.

If a request touches a specific area, also pull the related doc from `docs/`:

- `docs/PRD.md` — product scope, vertical slices, what's in/out of MVP.
- `docs/tech.md` — library choices + pins (e.g. `rusqlite` pinned to `0.39`, edition 2024
  env-mutation `unsafe`). Read before adding or bumping any dependency.
- `docs/adr/` — the *why* behind every load-bearing decision. Do not undo one without
  reading its ADR. Decisions go through ADRs, not just code comments or commit messages.
- `docs/setup.md` — how to run the daily-driver binaries.

## Non-negotiables (from `context.md`, restated so they're not missed)

- **The TUI never opens a vault file.** Write-back routes through the daemon via
  `pending_actions`. Only the daemon mutates notes, and only after byte re-verification
  (`atomic_write` TOCTOU guard, ADR-0004).
- **The single-writer `flock` is load-bearing** (ADR-0008). Don't add a daemon entry point
  that bypasses `acquire_daemon_lock`.
- **`taski-core` stays pure** — no FS, no I/O, no deps on other taski crates.
- **Never run tests against the real vault.** Use `tempfile` fake vaults / `:memory:` DBs.
- **Schema migration is destructive.** Bump `SCHEMA_VERSION` and existing dev DBs get wiped.

## CI gates (run before considering work done)

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

## Conventions

- Conventional Commits: `feat:`, `fix:`, `chore:`, `docs(adr):`.
- Vertical slices that leave the app runnable.
- Property-test the invariants (write-back "never corrupts" proptests encode the safety
  contract — keep them green when you touch those areas).
- New load-bearing decision → write an ADR in `docs/adr/` and update `tech.md`.
