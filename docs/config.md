# Configuring Taski

A complete guide to `config.toml` — every option, a copy-paste example with the
defaults spelled out, and a themes section (including a ready-made Nord palette).

For the architecture behind theming see [ADR-0018](./adr/0018-theming-and-per-panel-density.md)
and the [theming feature doc](./features/theming.md). For first-time install see
[setup.md](./setup.md).

---

## Where the file lives

Taski reads, in order:

1. `$TASKI_CONFIG` — if set and non-empty, this exact path.
2. `~/.config/taski/config.toml` — the default.

A **missing** file is fine: Taski falls back to CLI flags and compiled defaults.
A **present-but-malformed** file is a hard error with the file path in the
message. The config is plain [TOML](https://toml.io).

### Precedence

For the two options that also have CLI flags, the order is:

```
--vault / --db  (CLI flag)   →   config.toml   →   compiled default
```

The CLI flag always wins for that invocation. Everything else is config-only.

### Generate a starter file

The daemon can write a fully-commented template for you (it refuses to clobber
an existing file):

```sh
taski-daemon --init-config --vault /path/to/your/vault
```

Drop `--vault` to get a template with a placeholder to fill in.

---

## Full example (every option at its default)

This is a complete, valid config with **every** key present and set to its
compiled default. Copy it and change what you like; or delete any line to let
the default apply — an omitted key behaves identically to the value shown here.

```toml
# ── Core ───────────────────────────────────────────────────────────────
# Your Obsidian vault root. REQUIRED (no default) — set here or pass --vault.
vault = "/Users/you/obsidian/MyVault"

# SQLite index database. The daemon writes it; the TUI reads it.
db = "./taski.db"

# Quick-add (`a` key) inbox note, relative to the vault root. New tasks are
# appended here as `- [ ] <text> ➕ <today>`.
inbox_path = "task-inbox.md"

# Vault name used for `obsidian://` deep links (the `o` key). Defaults to the
# basename of `vault`; set only if your Obsidian vault name differs from the
# folder name. (Shown commented because the default is derived, not literal.)
# obsidian_vault = "MyVault"

# Use the Advanced URI community plugin so `o` jumps to the task's exact line.
# Requires the plugin installed in Obsidian. false = native obsidian://open
# (opens the file but can't target a line).
use_advanced_uri = false

# Directories (relative to vault root) to skip when scanning/indexing. Hidden
# dirs (.obsidian, .trash, .git) are always excluded — no need to list them.
exclude_dirs = []

# ── Theme: colors (all optional; omit a key to keep its default) ────────
[theme]
accent         = "cyan"        # headers, ctx-pane title, in-progress, quick-add, ⏳
accent_bright  = "light_cyan"  # "today" emphasis
group_accent   = "magenta"     # group-axis indicator in the title bar
success        = "green"       # done checkbox, search/file/quick-add echoes
warning        = "yellow"      # keycaps, open checkbox, due date
danger         = "red"         # write-back failure notice
danger_bright  = "light_red"   # overdue indicator
muted          = "dark_gray"   # header counts, line numbers, "other" status
context_target = "yellow"      # context-pane target-line highlight
scheduled      = "cyan"        # ⏳ <date> suffix
path_prefix    = "dark_gray"   # dir prefix in note-group headers (filename pops)
background     = "default"      # window bg; "default" = terminal's own background

# ── UI: per-panel layout (all optional) ────────────────────────────────
[ui]
list_pane_percent = 50          # 20–80; list width when the context pane is open
list_density      = "compact"   # compact | comfortable | spacious
context_wrap      = false        # wrap context-pane lines instead of truncating
```

> Note: `[theme]` and `[ui]` are themselves optional. Omitting a whole section
> is identical to using all of its defaults.

---

## Option reference

### Core

| Key | Type | Default | Notes |
|---|---|---|---|
| `vault` | string | — (required) | Obsidian vault root. No default; set here or pass `--vault`. |
| `db` | string | `./taski.db` | SQLite index path. Override per-run with `--db`. |
| `inbox_path` | string | `task-inbox.md` | Quick-add target note, relative to vault root. |
| `obsidian_vault` | string | basename of `vault` | Only set if the Obsidian vault name ≠ folder name. |
| `use_advanced_uri` | bool | `false` | `true` uses the Advanced URI plugin so `o` targets the exact line. |
| `exclude_dirs` | array&lt;string&gt; | `[]` | Vault-relative dirs to skip. Hidden dirs always excluded. |

### `[ui]` — layout

| Key | Default | Values | Effect |
|---|---|---|---|
| `list_pane_percent` | `50` | `20`–`80` (clamped, warns if outside) | Task-list width when the context pane is visible. Below the 60-col split floor the pane auto-hides and this is moot. |
| `list_density` | `"compact"` | `compact` / `comfortable` / `spacious` | Blank-line separators between groups: 0 / 1 / 2 lines. A bad value is a **hard error** at load. |
| `context_wrap` | `false` | bool | Wrap long context-pane lines instead of truncating. |

`[theme]` is covered in its own section below.

---

## Themes

`[theme]` maps **12 semantic color roles** to colors of your choosing. Every key
is optional — an omitted role keeps Taski's classic dark default.

### Accepted color values

| Form | Example | Meaning |
|---|---|---|
| Named color | `"cyan"`, `"light_red"`, `"dark_gray"` | ratatui's 16-color palette. Case-insensitive; `light_red` / `LightRed` / `lightred` all work. |
| Hex truecolor | `"#88c0d0"`, `"#abc"` | 24-bit RGB. 3-digit shorthand expands (`#abc` → `#aabbcc`). |
| `"default"` | `"default"` | The terminal's own default foreground (`Color::Reset`). |

Named colors are: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`,
`gray`, `dark_gray`, `light_red`, `light_green`, `light_yellow`, `light_blue`,
`light_magenta`, `light_cyan`, `white`.

> **Truecolor caveat:** hex values are exact only on truecolor terminals. On a
> 256-color terminal ratatui approximates them; the named palette is unaffected.

### The 12 roles

| Role | Default | Where it shows up |
|---|---|---|
| `accent` | `cyan` | Title filter label, group-header marker, context-pane title, help headers, in-progress checkbox, quick-add prefix, `⏳` suffix |
| `accent_bright` | `light_cyan` | "Today" emphasis (title indicator + scheduled-today highlight) |
| `group_accent` | `magenta` | Group-axis indicator in the title bar |
| `success` | `green` | Done checkbox, search/file/quick-add query echoes, search indicator |
| `warning` | `yellow` | Keycaps (footer + help), open checkbox, due date |
| `danger` | `red` | Write-back failure notice |
| `danger_bright` | `light_red` | Overdue indicator |
| `muted` | `dark_gray` | Group-header counts, context-pane line numbers, "other" status checkbox |
| `context_target` | `yellow` | Context-pane target-line highlight |
| `scheduled` | `cyan` | `⏳ <date>` scheduled suffix |
| `path_prefix` | `dark_gray` | Directory prefix in **Note**-group headers |
| `background` | `default` | Window background (see below). `default` = the terminal's own bg. |

Several roles share a default color (e.g. `scheduled` matches `accent`,
`path_prefix` matches `muted`) but are independently overridable — that's the
point of separate roles.

#### Background color

`background` is the only background role; the other 11 are foreground colors. It
defaults to `"default"`, meaning **Taski paints no background** and your
terminal's background shows through (this is the byte-identical default). Set it
to a named or hex color to make Taski fill its entire surface with that color,
independent of your terminal theme — useful when you want the app to look like a
specific theme (e.g. Nord on `#2e3440`) regardless of the terminal's own colors.
The fill covers everything: blank rows, gaps, and the help overlay.

### Note-header filename emphasis

Under the **Note** grouping axis a group header is a note path, e.g.
`Projects/Work/standup.md`. Taski dims the directory prefix (`Projects/Work/`)
with `path_prefix` and keeps the filename (`standup.md`) bold and bright, so the
filename is easy to spot while scanning. This applies to Note grouping only —
Tag / Priority / Folder headers render whole, and root-level notes (no `/`) have
no prefix. To restore the old undimmed look set `path_prefix = "default"`.

### Tips for building a palette

- Start from the [full example](#full-example-every-option-at-its-default) and
  recolor a few roles at a time; omit any you don't want to change.
- For a **light terminal**, avoid `yellow` and `light_red` (they wash out) — see
  the [Light Terminal preset](./themes/light.md) in the gallery.
- Keep `success`, `warning`, and `accent` visually distinct: they encode
  done / open / in-progress checkbox states. Collapsing them loses that signal
  (Done is still struck through + dimmed, so it survives, but open vs
  in-progress would blur).

---

## Ready-made themes

Complete, paste-ready presets live in the **[theme gallery](./themes/)** — each
is a full `[theme]` block (all 12 roles, including `background`) with a palette
reference:

- [opencode](./themes/opencode.md) — mirrors the opencode agent's theme (dark + light)
- [Nord](./themes/nord.md) — dark, truecolor
- [Tokyo Night](./themes/tokyo-night.md) — dark, truecolor
- [Gruvbox Dark](./themes/gruvbox-dark.md) — dark, truecolor
- [Light Terminal](./themes/light.md) — light, named colors

Drop one into `~/.config/taski/config.toml` (replacing any existing `[theme]`
section) and restart Taski.

---

## Error handling

Resolution happens at startup, **before** the TUI takes over the screen, so a
bad config never garbles the display:

| Problem | What happens |
|---|---|
| Unknown color name (`"mauve"`) or bad hex (`"#zzzzzz"`) | That **role** falls back to its default; Taski logs a `tracing::warn!` and starts normally. |
| `list_pane_percent` outside `20–80` | Clamped to the nearest bound + a warning. |
| Unknown `list_density` (e.g. `"cozy"`) | **Hard error** at config load — fix the typo and re-run. |
| Malformed TOML anywhere | Hard error naming the file. |
| Missing config file | Not an error — CLI flags / defaults apply. |
| Bad `[theme]` in combined mode (`taski`) | The warning flows to `~/.local/share/taski/daemon.log`; the alt screen stays clean. |

The asymmetry is deliberate: color/percent mistakes are recoverable per-role, so
Taski degrades gracefully; a misspelled `list_density` *variant* can't be
guessed safely, so it's caught loudly at load time.

---

## References

- [setup.md](./setup.md) — install + first-run.
- [features/theming.md](./features/theming.md) — design notes, the
  capability-reality explainer, and more theme presets.
- [ADR-0018](./adr/0018-theming-and-per-panel-density.md) — the theming &
  density decision record.
