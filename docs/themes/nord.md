# Nord

A faithful [Nord](https://www.nordtheme.com/) preset for Taski's TUI — a
low-contrast arctic palette. Uses hex truecolor, so it needs a truecolor
terminal for exact colors.

## Use it

Paste this `[theme]` block into `~/.config/taski/config.toml` (replace an
existing `[theme]` section, or add one), then restart Taski:

```toml
[theme]
# Nord — https://www.nordtheme.com/docs/colors-and-palettes
accent         = "#88c0d0"   # nord8  — frost, signature Nord cyan
accent_bright  = "#8fbcbb"   # nord7  — frost, brighter accent for "today"
group_accent   = "#b48ead"   # nord15 — aurora purple
success        = "#a3be8c"   # nord14 — aurora green
warning        = "#ebcb8b"   # nord13 — aurora yellow
danger         = "#bf616a"   # nord11 — aurora red
danger_bright  = "#d08770"   # nord12 — aurora orange (urgent/overdue)
muted          = "#4c566a"   # nord3  — polar night, the classic Nord "comment"
context_target = "#ebcb8b"   # nord13 — aurora yellow, matches the highlight role
scheduled      = "#81a1c1"   # nord9  — frost blue, distinct from accent
path_prefix    = "#4c566a"   # nord3  — dim the dir prefix so the filename pops
background     = "#2e3440"   # nord0  — polar night, the Nord canvas
```

Everything else (core options, `[ui]` layout) is independent — this only sets
colors. See the [configuration guide](../config.md) for the full picture.

## Palette reference

The Nord colors this preset draws from:

| Swatch | Hex | Role used for |
|---|---|---|
| nord0  | `#2e3440` | `background` |
| nord3  | `#4c566a` | `muted`, `path_prefix` |
| nord7  | `#8fbcbb` | `accent_bright` |
| nord8  | `#88c0d0` | `accent` |
| nord9  | `#81a1c1` | `scheduled` |
| nord11 | `#bf616a` | `danger` |
| nord12 | `#d08770` | `danger_bright` |
| nord13 | `#ebcb8b` | `warning`, `context_target` |
| nord14 | `#a3be8c` | `success` |
| nord15 | `#b48ead` | `group_accent` |

## Notes

- Nord is a **dark** theme built on the nord0 `#2e3440` canvas. The `background`
  line above makes Taski paint that itself, so it looks right on any terminal.
  Remove it (or set `"default"`) to fall back to your terminal's background.
- On a 256-color (non-truecolor) terminal these hex values are approximated; the
  result still reads as Nord but won't be pixel-exact.
- `path_prefix` shares the `muted` color so directory prefixes in Note-group
  headers recede and the filename stands out. Set `path_prefix = "default"` to
  turn that off.
