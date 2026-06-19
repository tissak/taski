# Idea: Taski — An Obsidian Task Partner

*Status: Refined Concept (aligned 2026-06-20)*
*Date: 2026-06-20*

## The Core

**The Problem:**
Tasks in an Obsidian vault are scattered. They live as Markdown checkboxes (`- [ ]`, `- [x]`, `- [/]`) across dozens or hundreds of notes — daily notes, project notes, meeting notes, scratchpads. Obsidian gives no first-class way to see, triage, and act on all of them in one place. The result: tasks get lost, forgotten, or duplicated, and the vault becomes a graveyard of good intentions.

**The Solution:**
A partner application with two components:
1. **Filesystem Watcher (background daemon)** — a long-running service that watches an Obsidian vault, parses Markdown, extracts task entries, and keeps a warm index of all tasks plus their basic metadata.
2. **UI (TUI first, GUI later)** — a client that talks to the daemon and surfaces all extracted tasks in one place, where they can be viewed, filtered, triaged, and acted on — **including writing changes back** into the notes.

**The "Magic":**
Tasks stay in their original notes (Obsidian remains the source of truth), but Taski gives you a single work surface over the *entire* vault at once — both *gathering* scattered tasks and *structuring* them with metadata, then reflecting your actions straight back into the Markdown. No migration, no duplication — just a powerful lens (and control surface) over the work already written down.

---

## The Scope

**In Scope (The MVP):**
- **Background daemon** that watches the vault and reacts to changes (add/edit/move/delete of notes), keeping a warm task index
- Markdown task parser recognizing common checkbox syntax
- Extraction of **basic metadata** from tasks to provide structure (specific fields to be defined in the PRD)
- A unified, filterable, **structured** task list in a TUI
- **Write-back**: completing/editing tasks from Taski updates the originating Markdown
- IPC between the daemon and the TUI client
- Live refresh as the vault changes
- **Personal use** — configured for a single user/vault; no packaging for distribution

**Out of Scope (The Parking Lot):**
*Great ideas saved for later*
- Full GUI (web or native)
- Rich/advanced metadata sync (recurring tasks, complex scheduling, full Tasks-plugin emoji semantics)
- Deep integration with Obsidian plugins (Dataview, Tasks, etc.)
- Tag/folder-based saved views
- Multi-vault support
- Distribution / packaging for other users
- Collaboration / sharing
- Notifications / reminders

---

## Architecture Direction (preliminary)
- **Scanner/watcher: Rust** — chosen to be small, fast, and high-performance. Runs as the background daemon described above.
- **Decoupling boundary: a shared local SQLite database** sits between the scanner and the UI. The daemon *writes* extracted tasks + metadata to SQLite; the UI *reads* (and, for write-back, coordinates writes through) the same DB/file. This cleanly separates the two components and lets the UI be implemented independently — even in a different language — without coupling to the scanner's internals.
- **TUI client: language TBD** — the SQLite boundary keeps this open for the PRD.
- **Candidate SQLite engine for the Rust side: Limbo** (Rust-native SQLite) — **under validation**. The architecture depends on safe *multi-process concurrent access* (daemon writes + UI reads one file), which classic SQLite provides via WAL mode. Limbo's support for this must be confirmed before locking it in.

## Why It Works
Obsidian is a superb *capture* tool, but a weak *execution* tool for tasks. Taski fills the execution gap without asking the user to leave Obsidian or change how they write notes — it both gathers the scattered work and gives it just enough structure to act on, then writes the result back where it came from.

## Risk Note (carry into PRD)
**Write-back is the highest-risk MVP feature.** Editing Markdown that Obsidian may simultaneously have open introduces concurrent-edit and conflict concerns, and a bad write could corrupt a user's notes. The PRD should define a conservative write strategy (e.g., atomic writes, checkbox-state edits first, full-text edits later) before implementation.

## Open Questions (for the PRD)
1. Which **metadata fields** belong in the MVP (e.g., due date, priority, project/tag, source note/link)? Prioritize 1–2.
2. **Write-back strategy**: MVP does checkbox-flip only, or also full task-text edits?
3. **Daemon↔TUI transport**: local socket, named pipe, or embedded library with a thin IPC shim?
4. **Conflict policy** when Obsidian and Taski edit the same note: last-write-wins, refuse-to-write, or merge?
5. **Index persistence**: in-memory only, or a local DB/cache on disk for fast cold-start?
6. Should the parser honor existing Obsidian conventions (Tasks-plugin emoji dates, Dataview inline fields) for metadata, or define Taski's own?
