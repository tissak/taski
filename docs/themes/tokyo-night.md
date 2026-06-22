# Tokyo Night

A [Tokyo Night](https://github.com/enkia/tokyo-night-vscode-theme) preset for
Taski's TUI — the popular low-contrast dark palette (night variant). Uses hex
truecolor, so it needs a truecolor terminal for exact colors.

## Use it

Paste this `[theme]` block into `~/.config/taski/config.toml` (replace an
existing `[theme]` section, or add one), then restart Taski:

```toml
[theme]
# Tokyo Night (night) — https://github.com/enkia/tokyo-night-vscode-theme
accent         = "#7aa2f7"   # blue   — headers, ctx title, in-progress, quick-add, ⏳ base
accent_bright  = "#7dcfff"   # cyan   — "today" emphasis (pops against accent)
group_accent   = "#bb9af7"   # purple — group-axis indicator
success        = "#9ece6a"   # green  — done checkbox, query echoes
warning        = "#e0af68"   # yellow — keycaps, open checkbox, due date
danger         = "#f7768e"   # red    — write-back failure notice
danger_bright  = "#ff9e64"   # orange — overdue (brighter/urgent)
muted          = "#565f89"   # comment — counts, line numbers, "other" status
context_target = "#e0af68"   # yellow — context-pane target-line highlight
scheduled      = "#73daca"   # teal   — ⏳ <date> suffix, distinct from accent
path_prefix    = "#565f89"   # comment — dim the dir prefix so the filename pops
background     = "#1a1b26"   # bg     — the Tokyo Night canvas
```

Everything else (core options, `[ui]` layout) is independent — this only sets
colors. See the [configuration guide](../config.md) for the full picture.

## Palette reference

The Tokyo Night colors this preset draws from:

| Swatch | Hex | Role used for |
|---|---|---|
| blue    | `#7aa2f7` | `accent` |
| cyan    | `#7dcfff` | `accent_bright` |
| teal    | `#73daca` | `scheduled` |
| green   | `#9ece6a` | `success` |
| yellow  | `#e0af68` | `warning`, `context_target` |
| orange  | `#ff9e64` | `danger_bright` |
| red     | `#f7768e` | `danger` |
| purple  | `#bb9af7` | `group_accent` |
| comment | `#565f89` | `muted`, `path_prefix` |
| bg      | `#1a1b26` | `background` |

## Notes

- Tokyo Night is a **dark** theme built on the `#1a1b26` canvas. The
  `background` line above makes Taski paint that itself, so it looks right on any
  terminal. Remove it (or set `"default"`) to fall back to your terminal's
  background instead.
- On a 256-color (non-truecolor) terminal these hex values are approximated; the
  result still reads as Tokyo Night but won't be pixel-exact.
- `path_prefix` shares the `muted` comment color so directory prefixes in
  Note-group headers recede and the filename stands out. Set
  `path_prefix = "default"` to turn that off.
