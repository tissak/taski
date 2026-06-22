# Theme gallery

Ready-to-paste color presets for Taski's TUI. Each is a `[theme]` block for
`~/.config/taski/config.toml` — drop one in (replacing any existing `[theme]`
section) and restart Taski.

| Theme | Kind |
|---|---|
| [opencode](./opencode.md) | dark (+ light variant), truecolor |
| [Tokyo Night](./tokyo-night.md) | dark, truecolor |
| [Nord](./nord.md) | dark, truecolor |
| [Gruvbox Dark](./gruvbox-dark.md) | dark, truecolor |
| [Light Terminal](./light.md) | light, named colors |

## How themes work

A `[theme]` section maps **12 semantic color roles** to colors (11 foregrounds
plus a `background`). Every key is optional; an omitted role keeps Taski's
default. Color values can be:

- a named color (`"cyan"`, `"light_red"`, `"dark_gray"`),
- hex truecolor (`"#7aa2f7"`, `"#abc"`), or
- `"default"` (the terminal's own foreground).

The full role list, accepted spellings, and error handling live in the
[configuration guide](../config.md#themes).

## Adding a preset

Copy `tokyo-night.md` as a template: keep the `## Use it` paste block (all 12
roles, each commented with the source-palette color it maps to), a palette
reference table, and a short notes section. Then add a row to the table above.
