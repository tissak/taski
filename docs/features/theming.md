# Theming & Per-Panel Density

## Problem

Taski's TUI hardcodes a dark-background color palette (~33 inline sites) and a
fixed 50/50 split between the task list and the context pane. Two pain points:

1. **Light-terminal users** see washed-out Yellow keycaps and an unreadable
   LightRed "overdue" indicator. There is no way to fix this without editing
   code.
2. **Readability ask:** users want the task list to dominate the screen and
   the menu/footer to recede — "large text for the list, small text for the
   menu."

## Capability reality (read this first)

**Terminals cannot render different font sizes in different panes.** Font size
is a terminal-emulator global (set in iTerm2/Terminal.app/Alacritty/WezTerm
preferences); there is no ECMA-48/VT escape sequence for per-region font size,
and [ratatui explicitly disclaims](https://ratatui.rs/faq/) control over it.
Every comparable TUI (lazygit, helix, gitui) works around this with the same
toolkit.

The real levers, which Taski now exposes:

| Lever | What it does | Taski knob |
|---|---|---|
| **Space allocation** | Give a pane more cells (rows/columns). A "larger list" really means more columns for the list. | `ui.list_pane_percent` |
| **Style emphasis** | `bold` reads as heavier; `dim` as lighter. The closest thing to font-weight. | (already applied: BOLD for headers, DIM for footer) |
| **Wrapping** | Narrow pane = wrapped dense lines; wide pane = one line per task. | `ui.context_wrap` |
| **Padding / spacing** | Blank lines between groups simulate line-height. | `ui.list_density` |
| **Color hierarchy** | Bright fg = prominent; dim fg = recessed. | `[theme]` section |

## Goals

- User can recolor every visible surface from `config.toml`.
- User can give the task list more space (answering the "large list" intent).
- User can wrap context-pane lines for readability.
- User can add breathing room between groups.
- **Defaults produce byte-identical rendering** to pre-feature Taski — with one
  intentional exception: Note-group headers dim the directory prefix so the
  filename stands out (see `path_prefix`).

## Non-goals

- Per-pane font size (impossible; see above).
- Runtime theme switching (deferred — config-driven first).
- Auto light/dark detection (unreliable across terminals).
- Pane-zoom key (deferred to a follow-up).
- Making `MIN_SPLIT_WIDTH` configurable.
- Priority emoji coloring (deferred — a clean follow-up if desired).

## Config reference

### `[theme]` — colors

All keys optional. Accepted spellings: ratatui named colors (case-insensitive,
snake_case: `"cyan"`, `"light_red"`, `"dark_gray"`), `"#rrggbb"` hex truecolor,
or `"default"` (terminal default fg).

| Role | Default | Where it's used |
|---|---|---|
| `accent` | `cyan` | Title filter, group header marker, ctx pane title, help headers, in-progress checkbox, quick-add prefix, `⏳` suffix |
| `accent_bright` | `light_cyan` | "Today" indicator (title + scheduled-today highlight) |
| `group_accent` | `magenta` | Group-axis indicator in the title bar |
| `success` | `green` | Done checkbox, search/file/quick-add query echoes, title search indicator |
| `warning` | `yellow` | Keycaps (footer + help), open checkbox, due date |
| `danger` | `red` | Write-back failure notice |
| `danger_bright` | `light_red` | Overdue indicator |
| `muted` | `dark_gray` | Group header counts, ctx-pane line numbers, "other" status checkbox |
| `context_target` | `yellow` | Context-pane target-line highlight |
| `scheduled` | `cyan` | `⏳ <date>` suffix |
| `path_prefix` | `dark_gray` | Directory prefix in **Note**-group headers (the path before the filename) |
| `background` | `default` | Window background — the only bg role; `default` = the terminal's own background |

#### Background color

`background` is the only background role (the other 11 are foregrounds). Its
default `"default"` means Taski **paints no background** — the terminal's own
background shows through, byte-identical to before. Set it to a named/hex color
and Taski fills its entire surface (blank rows, gaps, and the help overlay
included), so the app can carry a theme's background independent of the
terminal. This works because every foreground span leaves the cell's background
untouched, so one full-screen fill underneath shows through everywhere.

#### Note-header filename emphasis

Under the **Note** grouping axis, each group header is a note path like
`Projects/Work/standup.md`. Taski dims the directory prefix (`Projects/Work/`)
using `path_prefix` and keeps the filename (`standup.md`) bold + default-fg, so
the filename is easy to spot when scanning a long list. This applies only to
Note grouping — Tag / Priority / Folder headers render whole. Root-level notes
(no `/`) have no prefix. Set `path_prefix = "default"` to restore the old
undimmed look.

### `[ui]` — layout

| Key | Default | Range / values | Effect |
|---|---|---|---|
| `list_pane_percent` | `50` | `20`–`80` (clamped) | Task-list width when the context pane is visible. Below `MIN_SPLIT_WIDTH` (60 cols) the pane auto-hides and this is moot. |
| `list_density` | `"compact"` | `compact` / `comfortable` / `spacious` | Blank-line separators between groups. Compact = no separators (today). Comfortable = 1 blank. Spacious = 2 blanks. |
| `context_wrap` | `false` | bool | Wrap long context-pane lines instead of truncating. |

## Examples

### Minimal — just make the list bigger

```toml
[ui]
list_pane_percent = 65
```

### Full color presets

Complete, paste-ready `[theme]` blocks (all 12 roles, with `background`) live in
the **[theme gallery](../themes/)**:

- [opencode](../themes/opencode.md) — mirrors the opencode agent's theme (dark + light)
- [Nord](../themes/nord.md) — dark, truecolor
- [Tokyo Night](../themes/tokyo-night.md) — dark, truecolor
- [Gruvbox Dark](../themes/gruvbox-dark.md) — dark, truecolor
- [Light Terminal](../themes/light.md) — light, named colors

## Error handling

- Unknown color name, malformed hex, or out-of-range percent → the **role or
  knob** falls back to its compiled default and Taski logs a `tracing::warn!`.
  The TUI still starts.
- Unknown `list_density` variant → hard error at config load (before the
  terminal enters the alt screen). Fix the typo and re-run.
- Bad `[theme]` while in combined mode (`taski`) → the warning flows to
  `~/.local/share/taski/daemon.log`; the alt screen is never garbled.

## Future

- **Pane zoom** (`z` key, lazygit-style): expand the focused pane to 100%.
  Plumbing arrives free with this feature; the design work is the
  transient/persistent state.
- **Runtime `:theme` command** (helix-style): swap themes without restarting.
  Would mutate the same `Theme` struct; no ADR needed.
- **`ui.footer_density`**: shrink the cheat-sheet copy or hide it (today it's
  one line + DIM — already compact).
- **Priority emoji coloring**: map the `Priority` enum to a configurable color
  role. A genuine new cosmetic affordance (not a refactor), deferred as YAGNI.

## References

- [docs/config.md](../config.md) — the full configuration guide: every option,
  an annotated example, the theme reference, and a Nord preset.
- [ADR-0018](../adr/0018-theming-and-per-panel-density.md) — the architectural
  decision behind this feature.
- [ratatui FAQ on font size](https://ratatui.rs/faq/) — "ratatui doesn't
  control the terminal's font size."
