# Catppuccin

A [Catppuccin](https://catppuccin.com) preset for Taski's TUI — the popular
pastel palette. This page ships the **Mocha** (dark) flavor as the default, with
a **Latte** (light) variant below. Mauve carries the brand as the primary
accent. Uses hex truecolor, so it needs a truecolor terminal for exact colors.

## Use it

Paste this `[theme]` block into `~/.config/taski/config.toml` (replace an
existing `[theme]` section, or add one), then restart Taski:

```toml
[theme]
# Catppuccin Mocha — https://catppuccin.com
accent         = "#cba6f7"   # mauve    — headers, ctx title, in-progress, quick-add, ⏳ base
accent_bright  = "#b4befe"   # lavender — "today" emphasis (pops against mauve)
group_accent   = "#89b4fa"   # blue     — group-axis indicator (distinct from accent)
success        = "#a6e3a1"   # green    — done checkbox, query echoes
warning        = "#f9e2af"   # yellow   — keycaps, open checkbox, due date
danger         = "#f38ba8"   # red      — write-back failure notice
danger_bright  = "#eba0ac"   # maroon   — overdue (a second red, distinct from the notice)
muted          = "#6c7086"   # overlay0 — counts, line numbers, "other" status
context_target = "#fab387"   # peach    — context-pane target-line highlight
scheduled      = "#89dceb"   # sky      — ⏳ <date> suffix, distinct from accent
path_prefix    = "#585b70"   # surface2 — dim the dir prefix so the filename pops
background     = "#1e1e2e"   # base     — Catppuccin Mocha canvas
```

Everything else (core options, `[ui]` layout) is independent — this only sets
colors. See the [configuration guide](../config.md) for the full picture.

## Palette reference

The Catppuccin Mocha colors this preset draws from:

| Swatch | Hex | Role used for |
|---|---|---|
| mauve    | `#cba6f7` | `accent` |
| lavender | `#b4befe` | `accent_bright` |
| blue     | `#89b4fa` | `group_accent` |
| green    | `#a6e3a1` | `success` |
| yellow   | `#f9e2af` | `warning` |
| red      | `#f38ba8` | `danger` |
| maroon   | `#eba0ac` | `danger_bright` |
| peach    | `#fab387` | `context_target` |
| sky      | `#89dceb` | `scheduled` |
| overlay0 | `#6c7086` | `muted` |
| surface2 | `#585b70` | `path_prefix` |
| base     | `#1e1e2e` | `background` |

## Latte variant

Catppuccin Latte is the light flavor. For a light terminal, use this instead:

```toml
[theme]
# Catppuccin Latte (light) — https://catppuccin.com
accent         = "#8839ef"   # mauve
accent_bright  = "#7287fd"   # lavender
group_accent   = "#1e66f5"   # blue
success        = "#40a02b"   # green
warning        = "#df8e1d"   # yellow
danger         = "#d20f39"   # red
danger_bright  = "#e64553"   # maroon
muted          = "#8c8fa1"   # overlay1 — readable dim text on the light canvas
context_target = "#fe640b"   # peach
scheduled      = "#04a5e5"   # sky
path_prefix    = "#acb0be"   # surface2 — lighter than muted, so the filename pops
background     = "#eff1f5"   # base
```

## Notes

- Mocha is a **dark** theme built on the `#1e1e2e` canvas; the `background` line
  makes Taski paint that itself, so it looks right on any terminal. Remove it (or
  set `"default"`) to fall back to your terminal's background instead.
- Taski has no dedicated body-text role — task text uses your terminal's default
  foreground. On a dark terminal that's already light (Catppuccin's `text` is
  `#cdd6f4`), so it reads well on `#1e1e2e`. If you set the Mocha `background` on
  a terminal whose default foreground is dark, body text would be hard to read.
- Catppuccin has no "brighter red", so `danger` uses **red** (`#f38ba8`) for the
  failure notice and `danger_bright` uses **maroon** (`#eba0ac`) for overdue, to
  keep the two distinguishable. Swap them if you'd rather overdue be the punchier
  red.
- On a 256-color (non-truecolor) terminal these hex values are approximated; the
  result still reads as Catppuccin but won't be pixel-exact.
- `muted` and `path_prefix` are deliberately two different grays so Note-group
  directory prefixes recede behind the filename. Set `path_prefix = "default"` to
  turn that off.
