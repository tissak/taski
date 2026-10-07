# ADR-0027: Scriptable CLI for AI agents

- **Status:** Accepted
- **Date:** 2026-10-07
- **Decides:** How an AI agent (or a shell script) reads and changes Taski tasks — as
  subcommands on the unified `taski` binary (`list`, `show`, `done`, `open`, `start`,
  `block`, `cancel`, `schedule`, `add`, `note`), a third client of the index alongside
  the TUI. No schema change, no new action type, no new dependency.

## Context

The user wants AI agents to work with Taski the way they already work with Obsidian:
by running shell commands. Agents can already edit the vault's markdown directly, but
doing so (a) gives them no cross-vault queries (today, overdue, by tag) without
re-implementing the parser, and (b) skips the write-back safety contract — the
content-hash conflict refusal (ADR-0004) that guarantees a write never clobbers a
concurrent edit.

## Decision

**A CLI is a third client of the index, built exactly like the TUI.** Reads go through
`db::all_tasks` / `db::note_content`; writes only enqueue `pending_actions` rows through
the existing `enqueue_*` functions. The CLI never opens a vault file itself — every
mutation goes through the daemon's `process_pending_actions` → `atomic_write`. Every
safety property of ADRs 0002/0004/0008 applies unchanged.

**Writes are synchronous.** An agent needs to know whether its write landed. After
enqueuing, the CLI probes the single-writer lock (ADR-0008):

- **Held** (a daemon is running): poll the action row (`db::action`) until it resolves,
  up to 10 s. On timeout, report the action id as still pending and say *do not retry*
  (it will apply later; a retry could double-apply an `add`/`note`).
- **Free** (no daemon): the CLI *takes* the lock and acts as the daemon for one drain —
  re-index the target note (nobody else is keeping it fresh), enqueue, run the same
  `process_pending_actions`, release. It goes through `acquire_daemon_lock`, so it can
  never be a second writer. A daemon starting during that window sees the lock held and
  refuses, exactly as it would against any other daemon.

Exit code is 0 on success (or a no-op), non-zero with the daemon's refusal reason on
failure.

**Status commands set a target state, not a toggle.** `done`/`open`/`start`/`block`/`cancel`
each map to a checkbox char (`x`, ` `, `/`, `!`, `-`) and are idempotent — if the task is
already there, print `unchanged` and exit 0. Toggles are a keyboard affordance; an agent
retrying a toggle would undo its own work. They're all ordinary `checkbox` actions, so
`✅`/`❌` stamping (ADRs 0012/0013) comes free.

**Every write prints the resulting task line, with its current id.** Task identity
(ADR-0005) survives line shifts, but reconciliation keys on the text hash — and the
write itself just changed the text (the `✅`/`⏳` stamp, the notes link). So `taski done
42` can leave the task as id 57. The CLI finds the task at the same note + line after the
write (for `add`: the inbox's last task) and prints it, so the agent always holds a live
id.

**Location is part of every read.** `list --json` / `show --json` carry `note_path`
(vault-relative), `path` (absolute, when a vault is configured) and `line`. `line` is a
hint for reading the file — it's where the task was at the last scan. Mutations only ever
take the id.

**Output.** Plain text by default (`id<TAB>[c] text<TAB>path:line`, the shape tools and
editors already understand). `--json` on `list`/`show`, hand-encoded (a 15-line string
escaper) to keep `serde_json` out of the tree for one use.

**Shared predicates.** The Today (ADR-0022) and Overdue (ADR-0023) predicates and the
"open-like" status set (ADR-0025) moved from TUI closures into `taski-core`
(`Task::is_today`, `Task::is_overdue`, `Status::is_open_like`) so the TUI and the CLI can't
drift.

**Agent skill.** `skills/taski/SKILL.md` documents the commands and the rules (read the
file freely, write through the CLI, re-read ids after writes, don't retry a pending
action) for agents to load.

## Scope

In: `list` (status/today/overdue/search/file/tag filters), `show` (task + note context),
the five status commands, `schedule`, `add`, `note`.

Out, until wanted: `reorder`, `archive`, bullet toggle, `undo` (an agent can issue the
inverse command), an MCP server (would be a thin wrapper over this CLI), streaming/watch
output.

## Consequences

- No new write path, so the "never corrupts" proptests already cover CLI writes.
- Reads without a running daemon can be stale; writes are not (the target note is
  re-indexed under the lock first). The skill tells agents to run the daemon for fresh
  reads.
- Ids are not stable across writes that change task text; documented in the skill, and
  each write reports the new id.
- The launcher crate now depends on `taski-db` and `rusqlite` directly (both already in
  the workspace tree).
