# ADR-0018: Color theming and per-panel density knobs

- **Status:** Accepted
- **Date:** 2026-06-22
- **Decides:** How users customize the TUI's color palette and per-panel space
  allocation. Adds two optional `[theme]` and `[ui]` sections to `config.toml`;
  the TUI resolves them into a `Theme` (ratatui colors) and `LayoutPrefs`
  (pane proportions, density, wrapping) at startup. This is a **render-path /
  read-path** feature: no schema bump, no daemon change, no `pending_actions`,
  no write-back ADR touched, and `taski-core` stays pure.

## Context

The TUI hardcodes ~33 `Color::X` call sites across `draw`, `row_to_item`,
`checkbox_style`, `draw_context_pane`, and `help_popup`. The palette works on a
dark terminal but is unfixable on a light background (Yellow keycaps wash out,
LightRed overdue is hard to read). Users have also asked for "per-panel text
sizing" — wanting the task list more prominent than the menu/footer.

The sizing ask is constrained by terminal reality: ratatui (and the ECMA-48
spec) has no per-region font-size escape; font size is a terminal-emulator
global. Every comparable TUI (lazygit, helix, gitui) uses **space allocation +
style emphasis** as the analogue. This ADR codifies that reframe and exposes
the real levers.

The hardcoded palette also makes future cosmetic changes (e.g. a high-contrast
accessibility preset, or a light terminal preset) touch 33 sites instead of one.

## Decision

Add two capabilities, both with **compiled defaults that produce byte-identical
rendering to today**, both **opt-in via `config.toml`**.

### A. Color theming

A `[theme]` section maps 12 semantic roles to user-configurable colors.

```toml
[theme]
accent         = "cyan"          # branding, headers, in-progress, ctx title
accent_bright  = "light_cyan"    # "today" emphasis
group_accent   = "magenta"       # group-axis indicator
success        = "green"         # done state + query echoes
warning        = "yellow"        # keycaps, open checkbox, due date
danger         = "red"           # failure notice
danger_bright  = "light_red"     # overdue
muted          = "dark_gray"     # counts, line numbers, "other" status
context_target = "yellow"        # context pane target-line highlight
scheduled      = "cyan"          # ⏳ suffix
path_prefix    = "dark_gray"     # dir prefix in note-group headers
background     = "default"       # window bg; "default" = terminal's own bg
```

Each value is `Option<ColorSpec>` — absent means "compiled default" (today's
exact color). Accepted spellings: ratatui named colors (case-insensitive,
snake_case: `"cyan"`, `"light_red"`, `"dark_gray"`), `"#rrggbb"` hex truecolor,
or `"default"` (the terminal's fg).

**Role granularity:** 12 roles — enough to recolor every distinct surface
without exploding the schema. Several roles share a color in the default palette
(e.g. `warning` covers both keycaps and due dates, `scheduled` shares
`accent`'s default, `path_prefix` shares `muted`'s) but remain independently
configurable.

`background` is the only **bg** role; the other 11 are foreground colors. It
defaults to `"default"` (`Color::Reset` = the terminal's own background), and
while it is `Reset` the renderer paints no background at all — so the default
theme stays pixel-for-pixel identical to a bare terminal. Setting it (named or
hex) makes Taski fill its whole surface, letting the app's theme diverge from
the terminal's. This works because every other surface sets only `.fg(...)`
(ratatui `Style` is a patch — an unset `bg` leaves the painted background
intact), so a single full-area background block at the top of `draw` shows
through everywhere; the help-overlay `Clear` region is the one spot that needs
an explicit re-paint.

### B. Per-panel density

A `[ui]` section exposes the real "text size" levers.

```toml
[ui]
list_pane_percent = 50          # 20–80; list width when ctx pane is visible
list_density     = "compact"    # compact | comfortable | spacious
context_wrap     = false        # wrap context-pane lines instead of truncating
```

- **`list_pane_percent`** replaces the hardcoded `Percentage(50)`/`Percentage(50)`
  split at `draw`. The "make the list larger" lever.
- **`list_density`** inserts blank-line separators between group headers
  (simulated line-height; the closest terminal analogue to "more readable
  spacing"). Compact = today (no separators). Comfortable = 1 blank line.
  Spacious = 2 blank lines.
- **`context_wrap`** toggles `Paragraph::wrap(Wrap { trim: false })` on the
  context pane. Today's behavior is truncation.

A pane-zoom key (lazygit-style: expand the focused pane to 100%) is **deferred**
— the plumbing lands for free with this ADR; the state design (transient vs
persistent, which pane is focused) deserves its own slice.

### C. Note-header path/filename emphasis (S3 follow-on)

Under the **Note** grouping axis, the group-header key is a note path
(`Projects/Work/standup.md`). It used to render as one bold span. To make the
filename scannable at a glance, `row_to_item` now splits the key at the last `/`
and renders the directory prefix (`Projects/Work/`) in the new `path_prefix`
role while the filename (`standup.md`) keeps bold + default fg.

- **Note-axis only.** The split is keyed on `GroupBy::Note` (threaded into
  `row_to_item`). Tag / Priority / Folder headers — whose keys aren't note paths
  — render whole, exactly as before.
- **Root-level notes** (no `/`) have no prefix and render unchanged.
- The split is a pure helper (`split_note_header`) so the path/filename boundary
  is unit-tested without a render backend.

This is the **one intentional exception** to the default-fidelity invariant
below: with default config the dir prefix now dims to `DarkGray` instead of
sharing the filename's default fg. The change is confined to Note-group headers;
every other surface stays byte-identical.

### Type boundary

- `taski-config` owns `ThemeConfig` + `UiConfig` + `ColorSpec` — pure data,
  `serde::Deserialize`, **no ratatui dep** (matches the crate's existing role
  as the sole TOML owner).
- `taski-tui` owns `Theme` + `LayoutPrefs` (ratatui-typed) and does the
  one-way conversion in `run_inner`.
- `taski-core`: **untouched**. Purity preserved.

### Failure handling (must not garble the alt screen)

All resolution happens in `run_inner` **before** `enter_terminal()`:

- Unknown color spelling (`"mauve"`) → `tracing::warn!` + that role falls
  back to the compiled default (other roles still apply).
- Malformed hex (`"#zzzzzz"`) → same per-role fallback.
- `list_pane_percent` outside `[20, 80]` → clamp to the nearest bound + warn.
- Unknown `list_density` variant → hard error at `taski_config::load_from`
  (malformed TOML) → `run_inner` returns `Err` before entering the alt screen.
- Priority coloring is explicitly out of scope for this ADR (the `group_accent`
  role for the group-axis indicator is not priority coloring). A follow-up ADR
  could add a `priority_tint` semantic role; deferred as YAGNI.

The TUI thread never `eprintln!`s (ADR-0015 landmine honored).

### Default fidelity invariant

`Theme::resolve_from(&ThemeConfig::default())` must produce a `Theme`
byte-equal to today's hardcoded palette. Codified by a unit test asserting all
12 fields, plus the existing `TestBackend` render smokes. The `background` role
defaults to `Reset` and `draw` skips the bg paint while it is `Reset`, so it
adds no departure from parity (a `TestBackend` test asserts the default leaves
every cell's `bg` at `Reset`).

**One scoped exception (section C):** the `path_prefix` role defaults to
`DarkGray`, which dims the directory prefix in Note-group headers. This is the
sole deliberate departure from pixel-for-pixel parity with the pre-theming TUI;
it is intentional (the feature's whole point) and confined to Note headers.
Every other surface — task rows, other grouping axes, the context pane, footer,
help — remains byte-identical under default config.

## Implementation Notes

1. **`crates/taski-tui/src/theme.rs`** — new module with `Theme`, `LayoutPrefs`,
   `Theme::resolve_from`, `LayoutPrefs::resolve_from`. Keeps the already-large
   `lib.rs` from bloating further.
2. **`crates/taski-config/src/lib.rs`** — add `ColorSpec`, `ThemeConfig`,
   `UiConfig`, `DensityPreset`; extend `Config` with `#[serde(default)]` fields
   `theme` and `ui`; extend `template()` with commented `[theme]` and `[ui]`
   blocks (following the existing `use_advanced_uri = false` pattern).
3. **`crates/taski-tui/src/lib.rs`**:
   - `App` gains `theme: Theme`, `layout: LayoutPrefs` (default-initialized in
     `App::new()`, overridden in `run_loop` like `inbox_path`).
   - `draw`, `row_to_item`, `checkbox_style`, `draw_context_pane`,
     `help_popup` read from `&app.theme` / `&app.layout`.
   - `row_to_item` also takes `group_by: GroupBy` and, for `GroupBy::Note`,
     splits the header key via `split_note_header` to dim the dir prefix
     (`path_prefix`) — section C.
   - `draw` paints a full-area background `Block` first when
     `theme.background != Reset` (and re-paints under the help-overlay `Clear`).
     Because every other surface is `.fg`-only, the bg shows through.
   - `run_loop` signature: `+ theme: &Theme, + layout: &LayoutPrefs`. (A
     parameter-struct `RunCtx` is deferred — same defer already noted on
     `build_view`.)
4. **Tests:** per-role fallback, default-fidelity snapshot, density blank-line
   count, percent clamp range, `TestBackend` buffer assertions on a non-default
   theme.
5. **Sequencing:**
   - S1 — Centralize palette, add `theme.rs`, replace all inline `Color::X`
     with `app.theme.<role>`. No config change. Tests green.
   - S2 — Add config-driven colors (`[theme]`). This ADR lands with this slice.
   - S3 — Add per-panel density knobs (`[ui]`). Feature doc lands with this slice.

## Rationale

- **The reframe is the value.** "Per-panel font size" is impossible; saying so
  in an ADR prevents future re-litigation and points the user at the real
  levers (allocation + emphasis + wrap + density), which is what every comparable
  TUI uses.
- **Defaults are load-bearing.** The byte-identical constraint means a user
  who never edits `[theme]`/`[ui]` sees *exactly* today's TUI. This is what
  lets the refactor land as a no-op for existing users.
- **`taski-config` owns all TOML.** Splitting color parsing between crates
  would break the precedent; keeping ratatui out of `taski-config` preserves
  its dep profile.
- **Per-role granularity (10) balances power and complexity.** A flat
  `fg`/`bg`/`accent` triple (3 roles) is too coarse — keycaps, due dates, and
  failure notices shouldn't share a color by force. A per-call-site mapping
  (33 roles) is unmanageable. 10 covers every distinct surface.

## Consequences

- ✅ A user can recolor the TUI via `config.toml` without touching code. Light-
  bg users get a working preset for the first time.
- ✅ A user can give the task list more cells (`list_pane_percent = 65`) and
  turn on context wrapping — the practical "make it readable" levers.
- ✅ Future cosmetic work (high-contrast accessibility preset, alternative
  palette) touches one struct, not 33 sites.
- ✅ Under Note grouping, the filename pops (dir prefix dimmed) so a long task
  list is faster to scan. Tunable via `path_prefix` (set it to `"default"` to
  restore the old undimmed look).
- ✅ Setting `background` lets Taski own a full-surface background independent of
  the terminal theme, completing presets like Nord (`#2e3440`) / Tokyo Night
  (`#1a1b26`) without reconfiguring the terminal.
- ⚠️ A user who sets `success == warning` loses the open/done checkbox color
  distinction. The `CROSSED_OUT + DIM` modifiers on Done still carry the
  distinction. No guardrail (a personal tool trusts the user).
- ⚠️ Hex colors approximate on non-truecolor terminals. Documented; the
  default named palette is unaffected.
- ⚠️ Density presets consume list rows (a comfortable layout shows ~1 fewer
  task per group in the same height). The user opts in explicitly.

## Alternatives considered

- **Per-call-site color mapping (33+ roles).** Rejected — unmanageable for a
  user and offers no real power over 12 well-chosen roles.
- **A flat `fg`/`bg`/`accent` triple.** Rejected — too coarse; keycaps and
  failure notices shouldn't share.
- **TUI runtime theme switching (a `:` command mode, like helix).** Deferred —
  the config-driven approach is simpler, ships sooner, and a runtime command
  can layer on later without an ADR (it would mutate the same `Theme`).
- **`tui-big-text` for the task list.** Rejected — multi-row glyphs are
  unusable for a list (each char eats 6+ rows); useful only for a splash title,
  which Taski doesn't have.
- **Auto-detect terminal background to switch light/dark preset.** Rejected —
  terminal bg queries (`OSC 11`) are unreliable across emulators; ratatui has
  no API. Let the user pick.
- **Make `MIN_SPLIT_WIDTH` configurable.** Rejected for S3 (YAGNI); it's an
  ergonomic floor, not a knob users have asked for.
- **Make `list_density` a numeric "blank lines between groups".** Rejected —
  a preset enum is clearer for the three regimes users actually want; numeric
  freedom gives no real value.
- **Put `Theme` in `taski-core`.** Rejected — would violate core's purity (no
  ratatui dep). `taski-config` is the right home for deserialization, and
  `taski-tui` for the render types.
- **Priority emoji coloring as a semantic role.** Deferred — out of scope for
  this ADR; the `group_accent` role is the group-axis indicator, not priority.
  (Note: the 11th role added here is `path_prefix`, a render-emphasis tweak —
  section C — not priority.) Adding a `priority_tint` role would be a genuine
  new feature (not a refactor) and a clean follow-up ADR if desired.

## Edge cases

| Case | Behavior |
|---|---|
| No `[theme]` / `[ui]` sections | Compiled defaults; byte-identical to today **except** Note-group headers dim the dir prefix (`path_prefix`, section C). |
| Note path with no `/` (root note) | No prefix; filename renders whole, unchanged. |
| `background` unset / `"default"` | `Color::Reset`; `draw` paints no bg — terminal background shows through (byte-identical default). |
| `background = "#2e3440"` (or any color) | Full-screen bg paint; all cells (incl. blank rows and the help overlay) get that bg. |
| Non-Note grouping (Tag/Priority/Folder) | Header key renders whole; no path split. |
| `path_prefix = "default"` | Dir prefix uses terminal default fg — restores the pre-split undimmed header. |
| `[theme]` present, some roles omitted | Omitted roles use compiled defaults; present roles apply. |
| `accent = "default"` | Ratatui `Color::Reset` (terminal default fg). |
| `accent = "mauve"` (unknown named) | Per-role fallback to default + `tracing::warn!`. |
| `accent = "#zzzzzz"` (bad hex) | Per-role fallback + warn. |
| `accent = "CYAN"` / `"Cyan"` / `"cyan"` | All accepted (case-insensitive). |
| `list_pane_percent = 5` | Clamped to 20 + warn. |
| `list_pane_percent = 95` | Clamped to 80 + warn. |
| `list_density = "Cozy"` (bad variant) | Hard error at config load (before alt screen). |
| Terminal below `MIN_SPLIT_WIDTH` (60 cols) | Pane auto-hides; `list_pane_percent` is moot. |
| Terminal width 100, `list_pane_percent = 60` | List ≈ 60 cols, ctx ≈ 38 (minus borders). |
| Non-truecolor terminal, hex color set | Ratatui approximates to 256-color; may differ. Documented. |
| `success == warning` (same color) | Open/done checkbox distinction lost; `CROSSED_OUT + DIM` still signals done. |
| Bad `[theme]` while combined mode runs | `tracing::warn!` flows to daemon.log (existing combined-mode subscriber). No garbled alt screen. |

## References

- [`docs/context.md`](../context.md) — decision list, gotcha about TUI thread
  never `eprintln!`-ing, and the `MIN_SPLIT_WIDTH`/footer-width notes.
- [`docs/tech.md`](../tech.md) — `taski-config` row; new "UI / TUI" entry for
  theming.
- [ADR-0015](./0015-open-in-obsidian-deep-link.md) — established the
  `tracing::warn!`-on-failure pattern in the TUI thread (this ADR reuses it
  for bad color values).
- [ADR-0017](./0017-frontmatter-taski-skip-opt-out.md) — most recent precedent
  for a no-schema-bump, no-write-back feature landing in `taski-config` +
  one consumer crate.
- [`docs/features/theming.md`](../features/theming.md) — the feature reference
  doc with examples and error-handling guide.
