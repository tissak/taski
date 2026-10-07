# ADR-0026: Compact, clickable links in task rows

- **Status:** Accepted
- **Date:** 2026-10-07
- **Decides:** How links inside task text render in the TUI list, and how they open.
  Display-only and TUI-local — no vault write, no daemon round-trip, no schema change
  (same class as ADR-0015's `o` gesture).

## Context

Task lines often carry links: `[docs](https://…)`, `[[Some Note|alias]]`, and Taski's own
`[[#notes-<id>|Notes]]` task-note link (ADR-0019). Rendered raw, the URL or target eats
the row and buries the task text. The user wants `[link](www.google.com)` to show as just
**link**, and to open the URL on click.

## Decision

**Compact rendering.** The pure `split_links` splits task text into plain runs and links:

| Source | Shown |
|---|---|
| `[label](url)` | `label` |
| `[[target\|label]]` | `label` |
| `[[target]]` | `target` |

Labels are styled underlined + accent (+ the global `bold` toggle, ADR-0018). Embeds
(`![…](…)`, `![[…]]`), bare `[x]`, URLs containing whitespace, and unclosed brackets stay
plain. Search (`/`) still matches the raw text.

**Click to open: OSC 8 terminal hyperlinks.** After the list renders, `apply_hyperlinks`
wraps each label cell in `ESC ]8;;<url> ESC \ <char> ESC ]8;; ESC \`. The terminal itself
handles the click — Taski does **not** enable mouse capture, so normal text selection
keeps working. ratatui 0.30's `CellDiffOption::ForcedWidth(1)` stops the buffer diff from
counting the escape bytes as visible columns; wide glyphs are left unwrapped rather than
forced to width 1.

Link targets (`link_url`):

- A URL with a scheme (`https://…`, `mailto:`) opens as-is; `www.…` gets `https://`.
  Other relative targets (`other.md`) render compact but aren't hyperlinked.
- Wiki links open `obsidian://open?vault=…&file=<target>` (the `#heading` part is dropped
  — native URIs can't target headings). An in-page link (`[[#…]]`) opens the task's own
  note. No configured vault name → compact but not hyperlinked.
- **Any control character in the target → no hyperlink.** Note text is untrusted; an
  `ESC` or `BEL` could terminate the OSC 8 sequence early and inject terminal commands.

## Consequences

- Works in terminals with OSC 8 support: Alacritty (0.11+), Ghostty, iTerm2, WezTerm,
  Kitty. Elsewhere the escapes are ignored and the label still renders compact. Inside
  tmux, OSC 8 needs `terminal-features` `hyperlinks` set.
- How a click is triggered is the terminal's choice (plain click vs Cmd/Shift-click).
- Only the task list is linkified; the context pane still shows raw note text.
- A keyboard way to open links (for terminals without OSC 8) is deferred until wanted.

## Alternatives rejected

- **Mouse capture + `open`.** Works in any terminal, but capturing the mouse disables
  normal click-drag text selection in a keyboard-first tool.
- **Writing escape sequences inside a `Span`.** ratatui measures the escape bytes as text
  width and garbles the row; the per-cell `ForcedWidth` overlay is the supported route.
