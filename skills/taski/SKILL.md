---
name: taski
description: Read and update the user's Obsidian tasks through the `taski` CLI — list open, today's, or overdue tasks; find where a task lives; mark tasks done, in progress, blocked, or cancelled; schedule them; add new tasks to the inbox; attach notes to a task. Use whenever the user asks about their tasks, todos, what's due or overdue, what to work on today, or wants a task created, completed, rescheduled, or annotated.
---

# Taski

Taski indexes every Obsidian checkbox task (`- [ ] …`) in the user's vault. The `taski`
CLI queries that index and writes changes back safely: if a note changed underneath, the
write is refused rather than overwriting it.

## Rules

1. **Change tasks through `taski`, never by editing the markdown.** Reading note files
   directly is fine — use `path` and `line` from the output.
2. **Mutating commands take a task id.** Get ids from `taski list`.
3. **Ids can change after a write.** A write that changes the task's text (✅/❌ stamps,
   ⏳ dates, a notes link) gives the task a new id. Every write prints the updated task
   line — use the id from that line for the next command, or re-run `taski list`.
4. **A non-zero exit means the write didn't happen.** The message says why (e.g. the note
   changed since it was scanned). Re-run `taski list`/`show` to get fresh state, then
   decide whether to try again.
5. **If a write says "still pending … Do not retry", don't retry.** It's queued and will
   apply when the daemon processes it; retrying could apply it twice.
6. Confirm with the user before bulk changes (more than a handful of tasks) or
   cancelling tasks.

## Reading

```sh
taski list                       # open, in-progress and blocked tasks
taski list --today               # scheduled ⏳ or due 📅 today
taski list --overdue             # due or scheduled before today (combine with --status)
taski list --status done         # open (default) | done | cancelled | all
taski list --search plumber      # substring of task text (case-insensitive)
taski list --file Projects/      # substring of the note path
taski list --tag home            # tag, with or without '#'
taski list --today --json        # any of the above as JSON
taski show 42                    # one task + 5 lines of note context either side
taski show 42 --context 20 --json
```

Filters combine (AND). Plain-text output is one task per line:

```
42	[ ] Call plumber 📅 2026-10-09 #home	Projects/House.md:17
```

That's id, checkbox and task text (including its metadata emojis), then
`note_path:line`. Checkbox chars: `[ ]` open, `[/]` in progress, `[!]` blocked,
`[x]` done, `[-]` cancelled.

`--json` gives one object per task:

```json
{"id":42,"status":"open","checkbox":" ","text":"Call plumber 📅 2026-10-09 #home",
 "note_path":"Projects/House.md","path":"/abs/vault/Projects/House.md","line":17,
 "due":"2026-10-09","scheduled":null,"start":null,"created":null,"done":null,
 "cancelled":null,"priority":null,"tags":["home"]}
```

`status` is one of `open`, `in_progress`, `blocked`, `done`, `cancelled`, `other`.
`priority` is the emoji: 🔺 highest, ⏫ high, 🔼 medium, 🔽 low, ⏬ lowest. `line` is
where the task was at the last scan; treat it as a hint for reading the file, not as
identity.

## Writing

```sh
taski done 42                    # [x], stamps ✅ today
taski open 42                    # [ ], clears ✅/❌ (re-open)
taski start 42                   # [/] in progress
taski block 42                   # [!] blocked
taski cancel 42                  # [-], stamps ❌ today
taski schedule 42 2026-10-12     # set ⏳ (also: today, none)
taski add "Call the plumber 📅 2026-10-09 #home"   # new task in the inbox note, stamps ➕ today
taski note 42 "Quoted 350 for the hinge"           # adds a note under the task's notes section
```

Status commands set a state; they don't toggle. Running `done` on a done task prints
`unchanged` and exits 0. `add` and `note` take a single line of text. Always quote it:
an unquoted `#tag` starts a shell comment and gets silently dropped. In `add`, use
Obsidian Tasks syntax for metadata: `📅 YYYY-MM-DD` due, `⏳ YYYY-MM-DD` scheduled,
`#tag`, a priority emoji.

On success a write prints the updated task line (same format as `list`) and exits 0.

## Workflow examples

**"What's on today?"** Run `taski list --today`. Then run `taski list --overdue` and
mention overdue items too.

**"I called the plumber"** Run `taski list --search plumber`. If exactly one task
matches, run `taski done <id>`. If several match, ask which one.

**"Push the gate task to Friday"** Find the task's id, work out Friday's date, then run
`taski schedule <id> YYYY-MM-DD`.

**"What's the context on #42?"** Run `taski show 42 --context 20`, or read `path` around
`line` directly.

## Setup notes

- The vault and index paths come from `~/.config/taski/config.toml` (override with
  `--vault` / `--db`, which go before the subcommand).
- Writes work with or without the Taski daemon running. Reads come from the index,
  which only stays current while the daemon (or the `taski` TUI) is running. If results
  look stale, tell the user.
