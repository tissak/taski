# opencode

A preset that mirrors the default [opencode](https://opencode.ai) coding-agent
theme — warm peach primary, purple accent, green/orange/red status colors, on a
near-black canvas. Colors are taken from opencode's own theme definition
([`opencode.json`](https://github.com/anomalyco/opencode/blob/dev/packages/tui/src/theme/assets/opencode.json)).
Uses hex truecolor, so it needs a truecolor terminal for exact colors.

## Use it

Paste this `[theme]` block into `~/.config/taski/config.toml` (replace an
existing `[theme]` section, or add one), then restart Taski:

```toml
[theme]
# opencode (dark) — https://opencode.ai
accent         = "#fab283"   # primary  — peach; headers, ctx title, in-progress, ⏳ base
accent_bright  = "#ffc09f"   # step10   — bright peach for "today"
group_accent   = "#9d7cd8"   # accent   — purple, group-axis indicator
success        = "#7fd88f"   # success  — green, done checkbox + query echoes
warning        = "#f5a742"   # warning  — orange, keycaps + open checkbox + due date
danger         = "#e06c75"   # error    — red, write-back failure notice
danger_bright  = "#e06c75"   # error    — red, overdue (opencode uses a single red)
muted          = "#808080"   # textMuted — counts, line numbers, "other" status
context_target = "#e5c07b"   # yellow   — context-pane target-line highlight
scheduled      = "#56b6c2"   # info     — cyan, ⏳ <date> suffix, distinct from accent
path_prefix    = "#606060"   # step8    — dim the dir prefix so the filename pops
background     = "#0a0a0a"   # step1    — opencode's near-black canvas
```

Everything else (core options, `[ui]` layout) is independent — this only sets
colors. See the [configuration guide](../config.md) for the full picture.

## Palette reference

opencode's dark palette, and the Taski roles it maps to:

| opencode token | Hex | Taski role |
|---|---|---|
| primary       | `#fab283` | `accent` |
| step10        | `#ffc09f` | `accent_bright` |
| accent        | `#9d7cd8` | `group_accent` |
| success       | `#7fd88f` | `success` |
| warning       | `#f5a742` | `warning` |
| error         | `#e06c75` | `danger`, `danger_bright` |
| yellow        | `#e5c07b` | `context_target` |
| info (cyan)   | `#56b6c2` | `scheduled` |
| textMuted     | `#808080` | `muted` |
| step8         | `#606060` | `path_prefix` |
| background    | `#0a0a0a` | `background` |

opencode also defines a `secondary` blue (`#5c9cf5`) which has no direct Taski
role; the peach `accent` and purple `group_accent` carry the brand instead.

## Light variant

opencode is an adaptive theme — its light mode flips the primary to blue and the
accent to orange. For a light terminal, use this instead:

```toml
[theme]
# opencode (light)
accent         = "#3b7dd8"   # primary (light) — blue
accent_bright  = "#2968c3"   # step10 (light)
group_accent   = "#d68c27"   # accent (light)  — orange
success        = "#3d9a57"
warning        = "#d68c27"
danger         = "#d1383d"
danger_bright  = "#d1383d"
muted          = "#8a8a8a"
context_target = "#b0851f"
scheduled      = "#318795"
path_prefix    = "#a0a0a0"
background     = "#ffffff"
```

## Notes

- The dark variant is built on opencode's `#0a0a0a` canvas; the `background` line
  paints it directly. Remove it (or set `"default"`) to keep your terminal's
  background.
- Taski has no dedicated body-text role — task text uses your terminal's default
  foreground. On a dark terminal that's already a light color, so it reads well
  against `#0a0a0a`; if you set a dark `background` on a terminal with a dark
  default foreground, body text would be hard to read.
- On a 256-color (non-truecolor) terminal these hex values are approximated.
- opencode uses one red for all error states, so `danger` and `danger_bright`
  share `#e06c75`. Give `danger_bright` a brighter red if you want overdue tasks
  to stand out more.
