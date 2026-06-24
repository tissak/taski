# Taski

**A fast, keyboard-driven terminal UI for the tasks scattered across your Obsidian vault — read and acted on right where they live, in the notes that give them meaning.**

## Why I built this

I've kept my notes in Markdown and Obsidian for years. Every time I've tried to fold task management into that setup, I've hit the same wall: **the mindset for taking notes isn't the mindset for getting work done.**

When I'm writing — in a journal entry, a meeting note, a topic page — I want to capture a task right there, next to the thing that prompted it, in as few words as possible. But when I sit down to *do* the work, I need the opposite: not a scattered trail through dozens of notes, but a single place that gathers everything and lays it out so I can pick the right next thing.

Most tools resolve this tension by pulling the tasks *out* of the notes — into a list, a board, a calendar. That never sat right with me. A checkbox in a meeting note means something different from the same words in a project plan, and the note around it *is* the context. If I have to restate that context to make a standalone task legible, I end up writing long, wordy tasks just to remind myself what I already knew when I wrote them.

Taski is my answer. It keeps Obsidian as the source of truth and the notes intact, but gives me a fast, gathered view across all of them. The key move is the context pane on the right: when I land on a task, I see the note it lives in — the heading above it, the lines around it. That's usually all the reminder I need, so I can **keep the task itself short**. The task and its context arrive together.

A few things I cared about enough to build around:

- **Speed.** I jump in and out of this list all day so jumping into the list and moving through it had to be fast. Hence a terminal UI over a warm local index.
- **Getting back to the note fast.** When a task does need its full context — or editing — I want to be one keypress from the source note in Obsidian. So `o` opens it via a deep link.
- **Not reinventing standards.** Completion, scheduling, and cancellation are written back using the [Obsidian Tasks](https://publish.obsidian.md/tasks/) plugin's date-emoji syntax (`✅ ⏳ ❌ ➕`). What Taski writes, Obsidian and its plugins already understand — and vice versa.

It's a personal tool, narrow on purpose: no mobile, no kanban, no sync. It fits *one* workflow — mine — well, rather than many adequately.

## How it works (in one breath)

Obsidian stays the source of truth. A background daemon watches one vault, parses every checkbox task into a local SQLite index, and is the **sole writer** back to your notes. The TUI only reads the index and submits intents; the daemon performs every write atomically, re-checking the file bytes immediately before it writes and **refusing rather than clobbering** if the note changed underneath it. The only things ever written into a note are the change you asked for — a checkbox flip or a standard date stamp — under a strict, property-tested grammar. Your prose is never touched.


## Features

A keyboard-driven TUI. Press `?` in the app for the full keybinding overlay.

- **Browse** every task across the vault, grouped by folder+note / note / tag / priority / folder (cycle with `G`).
- **Filter** by status (`f`), today (`T`), overdue (`O`), text search (`/`), and file search (`F`) — all compose.
- **See context** — `p` toggles the in-note context pane; `J`/`K` scroll it. This is the whole point.
- **Act** without leaving the keyboard:
  - `Space` — toggle open ↔ done (stamps `✅`)
  - `t` — mark / unmark for today (`⏳`)
  - `d` — cancel (`❌`)
  - `b` — toggle checkbox ↔ bullet
  - `a` — quick-add to an inbox note (`➕`)
  - `n` — add a closing note to the task (grouped under a `## task-notes` section, with a clickable in-page link)
  - `m` — move mode: reorder a task within its note (`j`/`k` to bubble, `Enter` to place, `Esc` to cancel)
  - `u` — undo
  - `o` — open the task's note in Obsidian (native deep link, or exact-line jump with the [Advanced URI](https://github.com/Vinzent03/obsidian-advanced-uri) plugin)

## Quick start

Taski is a Cargo workspace (edition 2024, stable Rust ≥ 1.93). The daily-driver flow is macOS, but the TUI builds anywhere crossterm does.

```sh
# 1. Build
cargo build --release --workspace

# 2. Generate a config pointing at your vault
./target/release/taski-daemon --init-config --vault /path/to/your/vault
#   → writes ~/.config/taski/config.toml

# 3. Run it (daemon + TUI together)
./target/release/taski
```

That's it — `taski` runs the daemon and TUI in one process, drains pending actions on quit, and exits. To keep the index warm in the background, autostart the daemon at login:

```sh
scripts/install-launchd.sh        # macOS: install + load a launchd agent
```

Then `taski` will *attach* to the running daemon (TUI only) instead of spawning a second one. See [`docs/setup.md`](./docs/setup.md) for the full daily-driver guide, and `taski tui` / `taski daemon` to run either side standalone.

## Configuration

`~/.config/taski/config.toml` (override the path with `TASKI_CONFIG`). Precedence is **CLI flag → config file → compiled default**.

```toml
vault = "/path/to/your/vault"                       # required by the daemon; no default
db    = "/Users/you/.local/share/taski/taski.db"    # defaults to ./taski.db
inbox_path = "task-inbox.md"                        # quick-add (`a`) target note

# Directories to skip when scanning (relative to vault root):
exclude_dirs = ["_System/Templates"]

# Open-in-Obsidian (`o`) deep-link options:
obsidian_vault    = "My Vault"   # optional override; defaults to the vault folder name
use_advanced_uri  = false         # true → jump to the task's exact line (needs the plugin)
```

## What Taski is *not*

These are deliberate, not gaps:

- **No mobile, web, or GUI app.** Terminal only.
- **No sync, collaboration, or multi-vault.** A personal, single-user, single-vault tool.
- **No free-text editing of tasks from the TUI.** Writes are bounded to checkbox flips and Obsidian-standard date stamps, by design — safety over flexibility. When I need to edit the words, I'm one `o` keypress from the note in Obsidian.
- **No packaging / one-click install.** You build it from source.

## Under the hood

If you want the full architecture — the SQLite decoupling boundary, the conflict-checked write-back contract, the data model, and the reasoning behind each load-bearing decision — it all lives in [`docs/context.md`](./docs/context.md), with the *why* behind each choice recorded in [`docs/adr/`](./docs/adr/).

The short version: a small Rust workspace (`ratatui` · `rusqlite`/WAL · `notify` · `tracing`), edition 2024, stable toolchain. The daemon is the sole writer; the TUI only reads the index and enqueues intents. The write-back "never corrupts a note" contract is guarded by 256-case property tests.

```sh
cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --all
```

## Acknowledgments

- [Obsidian](https://obsidian.md) — the source of truth this sits on top of.
- The [Obsidian Tasks](https://publish.obsidian.md/tasks/) plugin — whose date-emoji syntax (`📅` `⏳` `🛫` `➕` `✅` `❌`) Taski reads and writes natively, so nothing here is a private standard.
- [TaskForge](https://taskforge.md/) — a polished, mobile-first task manager over a vault. It proved a dedicated task layer is worth building, and its task-*centric* model clarified, by contrast, exactly how note-*centric* I wanted mine to be.

---

*Taski is a personal tool built for one workflow, released under the [MIT License](./LICENSE).*
