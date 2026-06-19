# Tech — Taski

*Last updated: 2026-06-20*

Authoritative record of technology choices for Taski. Each entry has a one-line rationale and a link to the deciding ADR where applicable. Update this file whenever a choice is made or revised.

> Convention: prefer choices already listed here over new ones. To deviate, raise it first and record the outcome here plus an ADR.

## Language & Runtime
| Choice | Rationale | Decided |
|---|---|---|
| **Rust (edition 2024)** | Scanner/daemon must be small, fast, low-resource; single language across the stack. | 2026-06-20 |
| **Rust + `ratatui` for the TUI** | Single language/toolchain end-to-end; the SQLite boundary keeps a future rewrite open regardless. | 2026-06-20 |

## Storage / Handoff
| Choice | Rationale | Decided |
|---|---|---|
| **SQLite via `rusqlite` + `libsqlite3-sys`** | Battle-tested WAL multi-process access (daemon writes, any-language TUI reads). Limbo/Turso rejected — see ADR-0001. | 2026-06-20 |
| **WAL journal mode**, `synchronous=NORMAL` | Standard SQLite pattern for one-writer + many-readers across processes. | 2026-06-20 |

> **Pin note (rusqlite):** currently pinned to `0.39` (libsqlite3-sys `0.37`). `rusqlite 0.40` pulls libsqlite3-sys `0.38`, whose `build.rs` uses the unstable `cfg_select!` macro and does **not** compile on stable rustc 1.93. Bump back to `0.40` once the toolchain stabilizes `cfg_select!`. (Validated 2026-06-20.)

## Scanner / Daemon
| Choice | Rationale | Decided |
|---|---|---|
| **`notify` + `notify-debouncer-mini`** | Cross-platform FS events; debounced (300ms) to coalesce rapid saves. Primary target macOS (FSEvents). Note: `mini` does not report event kind (create/modify/remove) — action is decided by a file-existence check. | 2026-06-20 |
| **`clap`** (derive) | Daemon CLI args (`--vault`, `--db`, `--once`). | 2026-06-20 |
| **`walkdir`** | Recursive vault scan with hidden-dir pruning (`.obsidian` / `.trash` / `.git`). | 2026-06-20 |
| **`ctrlc`** | Graceful SIGINT shutdown of the watch loop. | 2026-06-20 |
| **Line-based parser** (in `taski-core`) | Current Markdown checkbox parser — fence-aware, tolerates leading blockquote markers. Chosen over `pulldown-cmark` for now (YAGNI; checkboxes are line-oriented). | 2026-06-20 |

> **Deferred (revisit when needed):** `pulldown-cmark` (adopt when real edge cases — tasks in nested lists / inline code / callouts — exceed the line parser), and Tasks-plugin due-date (`📅`) *extraction* (the line is already parsed as a task; only the metadata parse is deferred).

## UI / TUI
| Choice | Rationale | Decided |
|---|---|---|
| **`ratatui`** | De-facto Rust TUI framework; pairs with the single-language stack. | 2026-06-20 |

## Cross-cutting
| Choice | Rationale | Decided |
|---|---|---|
| **`serde` + `toml`** | Config parsing (`~/.taski/config.toml`). | 2026-06-20 |
| **`tracing` + `tracing-subscriber`** | Structured logs to stderr + rotating file (`~/.taski/logs/`); essential for post-incident write-back diagnosis. | 2026-06-20 |

## Testing
| Choice | Rationale | Decided |
|---|---|---|
| **`tempfile`** | Integration tests against throwaway fake vaults — never the real vault. (Real vault is exercised at runtime via `taski.db`, which is gitignored.) | 2026-06-20 |
| **`proptest`** | Property tests: parser never panics on arbitrary input; (Slice 3+) write-back "never corrupts" + stable identity. | 2026-06-20 |
| **`cargo-fuzz`** | *Deferred* — needs nightly; `proptest` covers the "never panic" property on stable for now. Revisit when nightly is acceptable. | 2026-06-20 |
| **`cargo-deny`** | CI advisory/license/supply-chain checks. | 2026-06-20 |

## Explicitly Rejected (for MVP)
| Choice | Why rejected | Revisit when |
|---|---|---|
| **Limbo / Turso Database** | No mixed SQLite+Turso multi-process access (hard blocker for a foreign-language TUI reader); still beta. See ADR-0001. | Turso `multiprocess_wal` goes stable **and** drops the no-mixing rule, or all processes adopt the Turso SDK. |

## Tooling / Foundations
| Choice | Rationale | Decided |
|---|---|---|
| **Cargo workspace** (`taski-core` / `taski-db` / `taski-daemon` / `taski-tui`) | Shared schema/types prevent drift; one canonical schema definition in `taski-db`. | 2026-06-20 |
| **GitHub Actions on macOS** | Primary platform is darwin; fmt + clippy (-D warnings) + test + cargo-deny. | 2026-06-20 |
| **`rust-toolchain.toml`** (pinned) | Reproducible builds across CI and local. | 2026-06-20 |
