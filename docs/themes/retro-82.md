# Retro 82

The [Omarchy](https://omarchy.org) `retro-82` preset for Taski's TUI — a
retro-futurist navy canvas with warm amber and cyan/teal accents. Uses hex
truecolor, so it needs a truecolor terminal for exact colors.

## Use it

Paste this `[theme]` block into `~/.config/taski/config.toml` (replace an
existing `[theme]` section, or add one), then restart Taski:

```toml
[theme]
# Omarchy Retro 82 — https://github.com/basecamp/omarchy (themes/retro-82)
accent         = "#faa968"   # accent — headers, ctx title, in-progress, quick-add, ⏳ base
accent_bright  = "#f6dcac"   # foreground — "today" emphasis (pops against the amber accent)
group_accent   = "#8cbfb8"   # cyan   — group-axis indicator, distinct from accent
success        = "#028391"   # green  — done checkbox, query echoes
warning        = "#e97b3c"   # yellow — keycaps, open checkbox, due date
danger         = "#f85525"   # red    — write-back failure notice
danger_bright  = "#f85525"   # red    — overdue (shares the one red in this palette)
muted          = "#2a6b78"   # muted  — counts, line numbers, "other" status
context_target = "#e97b3c"   # yellow — context-pane target-line highlight
scheduled      = "#3f8f8a"   # blue   — ⏳ <date> suffix, distinct from accent
path_prefix    = "#2a6b78"   # muted  — dim the dir prefix so the filename pops
background     = "#05182e"   # bg     — the Retro 82 navy canvas
```

Everything else (core options, `[ui]` layout) is independent — this only sets
colors. See the [configuration guide](../config.md) for the full picture.

## Palette reference

The Retro 82 colors this preset draws from:

| Swatch | Hex | Role used for |
|---|---|---|
| bg         | `#05182e` | `background` |
| foreground | `#f6dcac` | `accent_bright` |
| accent     | `#faa968` | `accent` |
| muted      | `#2a6b78` | `muted`, `path_prefix` |
| blue       | `#3f8f8a` | `scheduled` |
| cyan       | `#8cbfb8` | `group_accent` |
| green      | `#028391` | `success` |
| yellow     | `#e97b3c` | `warning`, `context_target` |
| red        | `#f85525` | `danger`, `danger_bright` |

## Notes

- Retro 82 is a **dark** theme built on the `#05182e` navy canvas. The
  `background` line above makes Taski paint that itself, so it looks right on
  any terminal. Remove it (or set `"default"`) to fall back to your terminal's
  background instead.
- The source palette has only one red and one true accent hue, so `danger`/
  `danger_bright` share a color (like Gruvbox Dark does) and `accent_bright`
  borrows the palette's cream foreground rather than a second warm tone, to
  keep "today" visually distinct from the standard accent.
- On a 256-color (non-truecolor) terminal these hex values are approximated;
  the result still reads as Retro 82 but won't be pixel-exact.
