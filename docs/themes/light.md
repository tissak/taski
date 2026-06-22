# Light Terminal

A dark-on-light preset for Taski's TUI, for users on a **light terminal
background**. Uses only named colors, so it works on any terminal (no truecolor
required) — it deliberately avoids yellow and light-red, which wash out on white.

## Use it

Paste this `[theme]` block into `~/.config/taski/config.toml` (replace an
existing `[theme]` section, or add one), then restart Taski:

```toml
[theme]
# Dark-on-light friendly: avoids yellow / light_red which wash out on white.
accent         = "blue"
accent_bright  = "cyan"
group_accent   = "magenta"
success        = "green"
warning        = "blue"
danger         = "red"
danger_bright  = "red"
muted          = "dark_gray"
context_target = "blue"
scheduled      = "blue"
path_prefix    = "gray"
background     = "white"      # paint a light canvas (or "default" to keep yours)
```

Everything else (core options, `[ui]` layout) is independent — this only sets
colors. See the [configuration guide](../config.md) for the full picture.

## Palette reference

This preset uses named colors (the standard 16-color ANSI set):

| Color | Role used for |
|---|---|
| `white`     | `background` |
| `blue`      | `accent`, `warning`, `context_target`, `scheduled` |
| `cyan`      | `accent_bright` |
| `magenta`   | `group_accent` |
| `green`     | `success` |
| `red`       | `danger`, `danger_bright` |
| `dark_gray` | `muted` |
| `gray`      | `path_prefix` |

## Notes

- `background = "white"` paints a light canvas inside Taski. If your terminal is
  already light, set it to `"default"` to keep your own background instead.
- Named colors render from your terminal's palette, so the exact shades follow
  your terminal theme — handy on a non-truecolor terminal.
- `warning` and `accent` share `blue` here because yellow (the default warning
  color) is hard to read on white; adjust to taste if your light theme has a
  readable amber.
