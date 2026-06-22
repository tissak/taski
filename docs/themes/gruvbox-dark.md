# Gruvbox Dark

A [Gruvbox](https://github.com/morhetz/gruvbox) dark preset for Taski's TUI —
warm, retro, high-contrast. Uses hex truecolor, so it needs a truecolor terminal
for exact colors.

## Use it

Paste this `[theme]` block into `~/.config/taski/config.toml` (replace an
existing `[theme]` section, or add one), then restart Taski:

```toml
[theme]
# Gruvbox dark — https://github.com/morhetz/gruvbox
accent         = "#fabd2f"   # bright yellow — headers, ctx title, in-progress, ⏳ base
accent_bright  = "#fabd2f"   # bright yellow — "today" emphasis
group_accent   = "#d3869b"   # bright purple — group-axis indicator
success        = "#b8bb26"   # bright green  — done checkbox, query echoes
warning        = "#fe8019"   # bright orange — keycaps, open checkbox, due date
danger         = "#fb4934"   # bright red    — write-back failure notice
danger_bright  = "#fb4934"   # bright red    — overdue
muted          = "#928374"   # gray          — counts, line numbers, "other" status
context_target = "#fe8019"   # bright orange — context-pane target-line highlight
scheduled      = "#83a598"   # bright aqua   — ⏳ <date> suffix, distinct from accent
path_prefix    = "#928374"   # gray          — dim the dir prefix so the filename pops
background     = "#282828"   # bg0           — the Gruvbox dark canvas
```

Everything else (core options, `[ui]` layout) is independent — this only sets
colors. See the [configuration guide](../config.md) for the full picture.

## Palette reference

The Gruvbox dark colors this preset draws from:

| Swatch | Hex | Role used for |
|---|---|---|
| bg0           | `#282828` | `background` |
| gray          | `#928374` | `muted`, `path_prefix` |
| bright red    | `#fb4934` | `danger`, `danger_bright` |
| bright green  | `#b8bb26` | `success` |
| bright yellow | `#fabd2f` | `accent`, `accent_bright` |
| bright orange | `#fe8019` | `warning`, `context_target` |
| bright aqua   | `#83a598` | `scheduled` |
| bright purple | `#d3869b` | `group_accent` |

## Notes

- Gruvbox dark is built on the `#282828` (bg0) canvas. The `background` line
  above makes Taski paint that itself; remove it (or set `"default"`) to fall
  back to your terminal's background.
- On a 256-color (non-truecolor) terminal these hex values are approximated.
- `accent` and `accent_bright` share the same yellow here (Gruvbox's "today"
  emphasis comes from the bold weight rather than a brighter hue); set
  `accent_bright` to a lighter color if you want more contrast on today's tasks.
