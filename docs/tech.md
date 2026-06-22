# Tech — Taski

*Last updated: 2026-06-22*

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
| **`fs2`** | `flock`-based single-writer lock (`daemon.lock`) preventing two daemons from racing on `atomic_write` / `reconcile_note`. Auto-released on crash. See ADR-0008. | 2026-06-20 |
| **Line-based parser** (in `taski-core`) | Current Markdown checkbox parser — fence-aware (backtick + tilde), tolerates leading blockquote markers, extracts Obsidian Tasks-plugin `📅`/`📆`/`🗓` due dates and `⏳` scheduled dates (ADR-0009). Chosen over `pulldown-cmark` for now (YAGNI; checkboxes are line-oriented). | 2026-06-20 |

> **Write-back scope (ADR-0003, amended by ADR-0009 and ADR-0012):** the daemon may mutate the vault by (a) checkbox-state flips, (b) Obsidian-standard date-emoji metadata (`⏳` scheduled, ADR-0009), and (c) `✅` done-date stamping composed into the checkbox flip itself (ADR-0012 — `[ ]`→`[x]` stamps `✅ <today>`, `[x]`→`[ ]` clears it, no new action type). Task-text edits, creates/deletes, and arbitrary metadata remain rejected. Each new write token requires a pure, proptested line-rewrite ("never corrupts") and its own ADR. Both `rewrite_scheduled` and `rewrite_done_date` share a generalized `rewrite_emoji_date` core. All write paths reuse `atomic_write` unchanged — its TOCTOU guard is whole-file-hash and byte-count-agnostic.

> **Deferred (revisit when needed):** `pulldown-cmark` (adopt when real edge cases — tasks in nested lists / inline code / callouts — exceed the line parser).

> **Frontmatter `taski-skip` opt-out (ADR-0017):** a note whose first-line YAML frontmatter carries `taski-skip: true` contributes **no tasks** to the index. The pure `taski_skip_enabled(markdown)` detector lives in `taski-core` (no new dep, manual string parsing); the daemon's `index_note` guards on it — reconciling with an empty list (which **evicts** any previously-indexed rows via `reconcile_note`'s unmatched-row delete) and skipping the `note_contents` cache. Read-path only: no schema bump, no `pending_actions`, no vault mutation, no write-back ADR touched. Only the literal boolean `true` (case-insensitive) or its quoted `"true"`/`'true'` variants are honored — YAML-1.1 spellings (`yes`/`on`) are deliberately rejected. The per-file, content-local complement to `exclude_dirs`.

> **Theming & density (ADR-0018):** colors and per-panel sizing are user-configurable via optional `[theme]` and `[ui]` sections in `config.toml`. `taski-config` owns the pure-data `ThemeConfig`/`UiConfig` (no ratatui dep); `taski-tui` owns the ratatui-typed `Theme`/`LayoutPrefs` and does the one-way conversion in `run_inner`. Defaults produce byte-identical rendering to the pre-feature TUI — codified by a unit test. Per-pane font size is not possible in terminals (no ECMA-48 escape); the real levers are space allocation, style emphasis, wrapping, and density — what every comparable TUI uses.

## UI / TUI
| Choice | Rationale | Decided |
|---|---|---|
| **`ratatui`** | De-facto Rust TUI framework; pairs with the single-language stack. | 2026-06-20 |
| **`ratatui` theming via in-crate `Theme` struct** | User-configurable color palette (`[theme]` in config.toml) resolved once in `run_inner` from a `ThemeConfig` (in `taski-config`, no ratatui dep). Defaults byte-identical to the pre-feature palette. See ADR-0018. | 2026-06-22 |
| **Per-panel density via `LayoutPrefs`** | User-configurable list-pane percent, list density, context wrap (`[ui]` in config.toml). Reframes the "per-pane font size" ask (impossible in terminals) as space allocation + emphasis. See ADR-0018. | 2026-06-22 |

## Cross-cutting
| Choice | Rationale | Decided |
|---|---|---|
| **`taski-config` crate** + `serde` + `toml` | TOML config (`~/.config/taski/config.toml`, XDG-style; overridable via `TASKI_CONFIG`). Lives in its own crate so `taski-core` stays pure (no FS/TOML I/O). Precedence: CLI flag → config file → compiled default. | 2026-06-20 |
| **`tracing` + `tracing-subscriber`** | Structured logs to stderr; essential for post-incident write-back diagnosis. | 2026-06-20 |

## Testing
| Choice | Rationale | Decided |
|---|---|---|
| **`tempfile`** | Integration tests against throwaway fake vaults — never the real vault. (Real vault is exercised at runtime via `taski.db`, which is gitignored.) | 2026-06-20 |
| **`proptest`** | Property tests: parser never panics on arbitrary input; (Slice 3+) write-back "never corrupts" + stable identity. | 2026-06-20 |
| **`cargo-fuzz`** | *Deferred* — needs nightly; `proptest` covers the "never panic" property on stable for now. Revisit when nightly is acceptable. | 2026-06-20 |
| **`cargo-deny`** | Advisory/license/supply-chain checks. **Not** wired into CI (no `deny.toml`); run locally. | 2026-06-20 |

## Explicitly Rejected (for MVP)
| Choice | Why rejected | Revisit when |
|---|---|---|
| **Limbo / Turso Database** | No mixed SQLite+Turso multi-process access (hard blocker for a foreign-language TUI reader); still beta. See ADR-0001. | Turso `multiprocess_wal` goes stable **and** drops the no-mixing rule, or all processes adopt the Turso SDK. |

## Tooling / Foundations
| Choice | Rationale | Decided |
|---|---|---|
| **Cargo workspace** (`taski-core` / `taski-config` / `taski-db` / `taski-daemon` / `taski-tui`) | Shared schema/types prevent drift; one canonical schema definition in `taski-db`; config loading isolated in `taski-config`. | 2026-06-20 |
| **GitHub Actions on macOS** | Primary platform is darwin; `fmt --check` + `clippy -D warnings` + `test` (no `cargo-deny` step yet). | 2026-06-20 |
| **`rust-toolchain.toml`** (pinned) | Reproducible builds across CI and local. | 2026-06-20 |

## Packaging / Distribution
| Choice | Rationale | Decided |
|---|---|---|
| **Release build** (`cargo build --release --workspace`) | Daily-driver binaries; verified clean under `--release`. | 2026-06-20 |
| **macOS `launchd` autostart** (`scripts/install-launchd.sh`) | Daemon starts at login (`RunAtLoad`) and is restarted on crash (`KeepAlive`). The plist carries no args — the daemon reads vault/db from `~/.config/taski/config.toml`. Binaries installed to `~/.local/bin`; logs to `~/.local/share/taski/daemon.log`. | 2026-06-20 |
| **No `dirs` crate** | Config path computed manually from `$HOME` (`~/.config/taski/`) because `dirs::config_dir()` returns `~/Library/Application Support` on macOS, not the project's XDG-style `~/.config`. One fewer dependency. | 2026-06-20 |
