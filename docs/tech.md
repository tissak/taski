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

## Scanner / Daemon
| Choice | Rationale | Decided |
|---|---|---|
| **`notify`** (crate) | Cross-platform FS events; primary target is macOS (FSEvents). Debounce + periodic reconcile needed on darwin. | 2026-06-20 |
| **`pulldown-cmark`** | Robust, spec-compliant Markdown parser; tolerant of real-world vault markup. | 2026-06-20 |

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
| **`tempfile` + `assert_fs`** | Integration tests against throwaway fake vaults — never the real vault. | 2026-06-20 |
| **`proptest`** | Property tests for write-back safety ("never corrupts") and stable identity. | 2026-06-20 |
| **`cargo-fuzz`** | Fuzz the parser against arbitrary Markdown-ish bytes (never panic). | 2026-06-20 |
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
