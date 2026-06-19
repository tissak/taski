# PRD: Taski — An Obsidian Task Partner

*Status: Draft v0.2 — key decisions locked (see §10 + `docs/adr/`)*
*Date: 2026-06-20*
*Source: derived from [`idea.md`](./idea.md); planning input in [`tech.md`](./tech.md) and [`docs/adr/`](./adr/)*

---

## 1. Overview

**Taski** is a personal, single-user partner application for Obsidian. It continuously scans an Obsidian vault, extracts Markdown task entries and their basic metadata into a structured index, and presents them in a terminal UI (TUI) where they can be viewed, filtered, and acted on — with changes written back into the originating notes.

**One-liner:** A fast, local "execution layer" for the tasks scattered across your Obsidian vault — Obsidian stays the source of truth; Taski gathers, structures, and acts on them.

## 2. Background & Problem

Tasks in an Obsidian vault are scattered. They live as Markdown checkboxes (`- [ ]`, `- [x]`, `- [/]`) across daily notes, project notes, meeting notes, and scratchpads. Obsidian has no first-class way to see, triage, and act on *all* of them in one place. The result: tasks get lost, forgotten, or duplicated.

There are two intertwined pains:
1. **Discovery** — tasks are scattered/lost across many notes.
2. **Structure** — even when found, tasks lack actionable organization (no clear due dates, grouping, or prioritization in one view).

Taski addresses both.

## 3. Goals & Non-Goals

### Goals (MVP)
- Reliably extract every task from a vault and keep that index warm in real time.
- Present tasks in a single, structured, filterable TUI.
- Capture enough **metadata** to make the list actionable (grouping + timing).
- Let the user act on tasks from the TUI and have those actions reflected back in the Markdown (write-back).
- Be small, fast, and low-resource (Rust core).

### Non-Goals (MVP)
- Distribution to other users / packaging / install UX.
- Full GUI (web or native) — TUI only for MVP.
- Rich scheduling (recurring tasks, complex reminder engines).
- Deep integration with Obsidian plugins (Dataview, Tasks) beyond reading their syntax.
- Multi-vault support.
- Collaboration / sharing / sync to remote.

## 4. Target User

A single, technical user (the author) managing one Obsidian vault. Assumes comfort with the terminal, Markdown, and local services. Configuration is manual (no onboarding UX required for MVP).

## 5. User Stories

- **As a user**, I want to see *all* incomplete tasks across my vault in one list, so nothing falls through the cracks.
- **As a user**, I want to filter/sort tasks by due date and group them by source note, so I can plan my day.
- **As a user**, when I complete a task in the TUI, I want the corresponding checkbox in the note to flip to `- [x]`, so Obsidian and Taski stay in sync.
- **As a user**, when I create or edit a task in Obsidian, I want it to appear/update in the TUI within seconds, so the view is always current.
- **As a user**, I want the watcher running quietly in the background, so the TUI opens instantly with a warm index.

## 6. Functional Requirements

### 6.1 Scanner / Watcher Daemon (Rust)
- **FR-1** Watch a configured vault root recursively for filesystem events (create/modify/move/delete) on `.md` files.
- **FR-2** Parse Markdown and extract task entries, recognizing checkbox states: `- [ ]` (open), `- [x]` (done), `- [/]` (in-progress), and other common Obsidian states.
- **FR-3** Extract task text and provenance (source note path + line number).
- **FR-4** Extract basic **metadata** for structuring (MVP subset locked in §10: due date + source-note grouping).
- **FR-5** Write/update extracted tasks into the shared SQLite index.
- **FR-6** Keep the index consistent on rename/move/delete of notes (update or remove affected tasks).
- **FR-7** Run as a long-running background process (daemon) and keep the index warm.

### 6.2 Index / Handoff (SQLite)
- **FR-8** Provide a local SQLite database as the single shared state between scanner and UI.
- **FR-9** Support concurrent access: scanner process writes, UI process reads, against the same file.
- **FR-10** Persist across restarts for fast cold-start of the TUI.

### 6.3 TUI Client
- **FR-11** Render a unified, structured list of all tasks from the index.
- **FR-12** Filter and sort (by status, due date, source note, etc. — per MVP metadata subset).
- **FR-13** Reflect live changes from the scanner (TUI polls SQLite — §10.4).
- **FR-14** Support write-back actions from the UI (checkbox-state flips only — [ADR-0003](./adr/0003-checkbox-only-mvp.md)).

### 6.4 Write-back
- **FR-15** Persist UI-initiated task changes back to the originating Markdown note(s).
- **FR-16** Do so safely without corrupting the vault (sole-writer routing per [ADR-0002](./adr/0002-write-back-through-daemon.md); refuse-on-conflict per [ADR-0004](./adr/0004-refuse-on-conflict.md)).

## 7. Non-Functional Requirements

- **Performance:** Vault scan and incremental re-index must be fast; TUI must feel instant. The Rust core should be small in binary size, memory, and CPU. (Specific targets to be set during design.)
- **Reliability / Data Integrity:** The vault must never be corrupted by Taski. Write-back safety is the top reliability concern (§11; [ADR-0002](./adr/0002-write-back-through-daemon.md), [ADR-0004](./adr/0004-refuse-on-conflict.md)).
- **Resilience:** The daemon must tolerate malformed notes, rapid save loops, and moves without crashing or thrashing the index.
- **Operability:** Simple to start/stop the daemon locally; clear logs. No exotic dependencies for MVP.

## 8. Architecture Overview

Two decoupled components joined by a shared local SQLite store.

```
 ┌───────────────────────┐         ┌──────────────────┐         ┌────────────────────┐
 │  Obsidian Vault (MD)  │  watch  │  Scanner Daemon  │  write  │                    │
 │  (source of truth)    │ ──────▶ │   (Rust/rusqlite)│ ──────▶ │   SQLite Index     │
 └───────────────────────┘         └──────────────────┘         │  (tasks + metadata)│
          ▲                              ▲   ▲                   └─────────┬──────────┘
          │ atomic, conflict-safe write  │   │ polls                        │ read (poll)
          │ (sole writer to vault)       │   │ pending_actions              ▼
          │                              │   │                     ┌────────────────────┐
          └──────────────────────────────┘   └─────────────────────│   TUI Client       │
                                            inserts action rows   │  (Rust/ratatui)    │
                                                                  └────────────────────┘
```

- **Scanner Daemon (Rust):** owns vault watching, parsing, and writing to SQLite. It is also the **sole writer to the vault** — it drains a `pending_actions` table and performs conflict-safe writes (ADR-0002, ADR-0004).
- **SQLite Index:** the decoupling boundary. The daemon writes; the TUI reads. Write-back is requested by the TUI via `pending_actions` rows and executed by the daemon, then re-indexed (the vault remains source of truth).
- **TUI Client (Rust/ratatui):** reads the index; submits action rows; never touches vault files directly.

> **Resolved:** SQLite is both the *data* handoff and the *command* channel (via `pending_actions`). The TUI polls SQLite for refresh; a dedicated socket push channel is a fast-follow only if latency demands it.

## 9. Preliminary Data Model

Core entity: **Task**. **Identity and location are separate concerns** (see ADR-0005 spike): a stable `id` must survive edits *above* the task line (which shift `line_number`), so identity is *not* derived from path+line. `(note_path, line_number)` is treated as a **write-time location claim**, re-verified against file bytes before any mutation.

| Field | Description | MVP? |
|---|---|---|
| `id` | **Stable identity** — content-hash of normalized text + nearest-heading anchor (NOT path+line; survives edits above the task). Final scheme pending identity spike. | yes |
| `note_path` | Source note (relative to vault root) — *location*, write-time only | yes |
| `line_number` | Line in the note — *location*; re-verified at write time, not trusted as identity | yes |
| `text` | Task body | yes |
| `text_hash` | Hash of task text, for identity re-verification at write time | yes |
| `status` | open / done / in-progress / etc. | yes |
| `raw_checkbox_char` | Exact checkbox char — re-verified against file bytes before flipping | yes |
| `note_hash` / `note_mtime` | Note state captured at last scan — used for conflict detection (ADR-0004) | yes |
| `due_date` | Parsed due date (Tasks-plugin `📅`) — **locked MVP metadata** | yes |
| `updated_at` | Last-seen timestamp | yes |
| `priority`, `tags`/`project` | Structuring fields | **fast-follow** |

## 10. Decisions (locked 2026-06-20)

These were validated against the PRD risks and are now locked; rationales live in [`docs/adr/`](./adr/) and [`docs/tech.md`](./tech.md).

| # | Decision | Status |
|---|---|---|
| 1 | **MVP metadata = due date + source-note grouping.** Priority/tags are fast-follow. | Locked |
| 2 | **MVP write-back = checkbox-state flips only.** Text/metadata writes deferred. | Locked — [ADR-0003](./adr/0003-checkbox-only-mvp.md) |
| 3 | **Parser honors Obsidian Tasks-plugin syntax** (checkbox states + `📅` due dates). Dataview inline fields fast-follow. | Locked |
| 4 | **Daemon↔TUI refresh = TUI polls SQLite** (no socket channel for MVP). | Locked |
| 5 | **Conflict policy = refuse-on-conflict** (re-check hash+mtime; never last-write-wins). | Locked — [ADR-0004](./adr/0004-refuse-on-conflict.md) |
| 6 | **SQLite engine = `rusqlite` + WAL** (Limbo/Turso rejected — hard multi-process blocker). M0 spike eliminated. | Locked — [ADR-0001](./adr/0001-rusqlite-not-limbo.md) |
| 7 | **TUI = Rust/ratatui** (single-language stack; SQLite boundary keeps a future rewrite open). | Locked |
| — | **Write-back routes through the daemon** (TUI → `pending_actions` table → daemon sole writer). | Locked — [ADR-0002](./adr/0002-write-back-through-daemon.md) |

### Still open (spike before building on it)
- **Stable task identity** — the scheme in §9 (`hash(text + heading anchor)`) must be validated to survive edits/moves; outcome recorded as ADR-0005. *(Blocks reliable move/delete handling, not the walking skeleton.)*

## 11. Risks & Mitigations

| Risk | Severity | Mitigation |
|---|---|---|
| Write-back corrupts a note / clobbers Obsidian edits | **Critical** | Atomic writes (temp file + `fsync` + `rename`); refuse-on-conflict ([ADR-0004](./adr/0004-refuse-on-conflict.md)); checkbox-only first ([ADR-0003](./adr/0003-checkbox-only-mvp.md)); sole-writer routing ([ADR-0002](./adr/0002-write-back-through-daemon.md)); property-tested ("never corrupts") |
| Unstable task identity after edits/moves | High | Separate stable `id` from write-time location (§9); validate scheme in identity spike → ADR-0005; re-verify bytes before any write |
| macOS FSEvents reliability (coalescing/latency/rename quirks) | Medium | Debounce + coalesce events; periodic full-reconcile sweep; tolerate during normal use |
| Rapid Obsidian saves cause index thrash | Medium | Debounce/coalesce FS events; idempotent re-index |
| Malformed/unusual Markdown breaks parser | Medium | Tolerant parser (`pulldown-cmark`); golden-file corpus + fuzz; skip-and-log rather than crash |
| ~~Limbo lacks multi-process WAL~~ | ~~Resolved~~ | Resolved 2026-06-20 — using `rusqlite`+WAL; see [ADR-0001](./adr/0001-rusqlite-not-limbo.md) |

## 12. Milestones / Phasing (vertical slices, toward MVP)

Sequencing favors thin end-to-end slices over horizontal layers, and **proves the riskiest feature (write-back) early** rather than last. (Supersedes the earlier M0–M5 layer plan.)

- **Slice 0 — Walking skeleton:** hardcoded note → parse 1 task → write SQLite → minimal TUI prints it. *Proves the whole vertical stack compiles and runs. (~½–1 day)*
- **Slice 1 — Real read path:** watch a *temp/test* dir, parse all tasks, persist, TUI lists them. Parser golden-file corpus + `cargo-fuzz` harness wired in from day one. *(2–3 days)*
- **Slice 2 — Live updates:** TUI polls SQLite and refreshes on daemon writes. *(½ day)*
- **Slice 3 — Write-back minimal (retire the top risk):** flip one checkbox end-to-end via the daemon, conflict-safe, with the "never corrupts" proptest passing. *Done before deepening the scanner.* *(2–3 days)*
- **Slice 4 — Deepen:** move/delete handling, stable-identity hardening (ADR-0005), due-date parsing, filters, FS-event debouncing.
- **Slice 5 — Harden:** edge cases, performance, resilience, logging polish, local-use packaging.

**Identity spike** (informs §9 / ADR-0005) runs alongside Slice 1–3 and must conclude before Slice 4's move/delete work.

## 13. Definition of Done (MVP)

- All FRs in §6 met.
- A task completed in the TUI appears as `- [x]` in Obsidian; a task created/edited in Obsidian appears in the TUI within seconds.
- No vault corruption across normal usage (including concurrent Obsidian + Taski edits → safe refusal, not data loss).
- Daemon runs unobtrusively; TUI opens to a warm index.

## 14. Out of Scope / Parking Lot

Full GUI; advanced/recurring metadata and full Tasks-plugin semantics; deep plugin integration (Dataview/Tasks write support); tag/folder saved views; multi-vault; distribution/packaging; collaboration/sharing; notifications/reminders.

## 15. References
- [`idea.md`](./idea.md) — refined concept and early thinking.
- [`tech.md`](./tech.md) — locked technology choices.
- [`adr/`](./adr/) — decision records:
  - [0001 — rusqlite, not Limbo](./adr/0001-rusqlite-not-limbo.md)
  - [0002 — write-back routes through the daemon](./adr/0002-write-back-through-daemon.md)
  - [0003 — checkbox-only MVP write-back](./adr/0003-checkbox-only-mvp.md)
  - [0004 — refuse-on-conflict](./adr/0004-refuse-on-conflict.md)
  - 0005 — stable task identity *(pending spike)*
