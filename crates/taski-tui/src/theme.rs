//! Theme and layout-preference types for the TUI.
//!
//! `Theme` holds the 12 semantic color roles the render pipeline uses. Every role has a
//! static default that reproduces today's hardcoded palette, so adding the type is a
//! pure refactor — no user-visible change.
//!
//! `LayoutPrefs` holds per-panel density knobs (list split, spacing, wrapping). All
//! defaults match the current behaviour.
//!
//! Both types are `Default`-constructible and live in this crate (not `taski-config`)
//! because they depend on `ratatui::style::Color`. Config deserialisation (S2) will
//! happen through a separate config-layer type in `taski-config` and be mapped to this
//! type at the `run_inner` boundary.

use ratatui::style::{Color, Modifier};
use taski_config::{ThemeConfig, UiConfig};

/// 12 semantic colour roles that cover every render call site in the TUI.
///
/// A theme is a flat bundle of `Color` values. The `Default` impl reproduces the
/// hardcoded palette used before theming was introduced; a `Theme` that is all defaults
/// produces byte-identical rendering.
///
/// When adding a new color call site, prefer reusing one of these roles over adding
/// a new one. If a genuinely new semantic role is needed, add it here + update the
/// feature doc + write a brief ADR note.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Primary accent — used for filter labels, header markers, in-progress checkboxes,
    /// context-pane note paths, help-popup group headers, quick-add inbox hint.
    pub accent: Color,
    /// Bright/emphasis accent — "today" labels, today-scheduled dates.
    pub accent_bright: Color,
    /// Group-by axis indicator — the axis name in the title bar (Note / Tag / etc.).
    pub group_accent: Color,
    /// Positive / success states — done checkboxes, search queries, prompt text.
    pub success: Color,
    /// Caution / attention — keycaps in footer and help, due dates, open checkbox.
    pub warning: Color,
    /// Errors — write-back failure notices.
    pub danger: Color,
    /// Urgent / overdue — the "overdue" filter indicator.
    pub danger_bright: Color,
    /// De-emphasised secondary text — group counts, context-pane line numbers,
    /// "other" status checkboxes.
    pub muted: Color,
    /// Context-pane target-line highlight — the line the selected task sits on.
    /// Same default hex as `warning` but semantically distinct (pane target vs
    /// attention/caution).
    pub context_target: Color,
    /// Non-today scheduled-date suffix. Same default hex as `accent` but semantically
    /// distinct (scheduled-information vs interactive accent).
    pub scheduled: Color,
    /// Directory-prefix in Note-group headers — the leading path before the
    /// filename (e.g. `Projects/Work/` in `Projects/Work/standup.md`). Dimmed by
    /// default so the filename (bold/default fg) pops at a glance. Same default
    /// hex as `muted` but semantically distinct (path chrome vs secondary text).
    pub path_prefix: Color,
    /// Window background. Defaults to `Color::Reset` (the terminal's own
    /// background) — when it equals `Reset`, `draw` paints no background at all,
    /// so the default theme stays byte-identical. Set it to any named/hex color
    /// to make Taski fill its whole surface, independent of the terminal theme.
    pub background: Color,
    /// Global bold toggle for every emphasised surface in the TUI. Bold glyphs
    /// render fuzzy on some terminals/fonts, so this defaults to `false` (off)
    /// — the only field whose default diverges from the pre-theming rendering,
    /// by deliberate choice: colour contrast already carries the emphasis. Set
    /// `bold = true` in `[theme]` to restore bold everywhere. Read it through
    /// [`Theme::bold_modifier`] at render sites, never the field directly.
    pub bold: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            accent_bright: Color::LightCyan,
            group_accent: Color::Magenta,
            success: Color::Green,
            warning: Color::Yellow,
            danger: Color::Red,
            danger_bright: Color::LightRed,
            muted: Color::DarkGray,
            context_target: Color::Yellow,
            scheduled: Color::Cyan,
            path_prefix: Color::DarkGray,
            // Reset = the terminal's own background. `draw` skips the bg paint
            // entirely while this is Reset, so default rendering is unchanged.
            background: Color::Reset,
            // Off by default: bold renders fuzzy on some fonts and colour
            // contrast already carries emphasis. Opt back in with bold = true.
            bold: false,
        }
    }
}

impl Theme {
    /// The bold modifier to apply at emphasis sites: `Modifier::BOLD` when the
    /// global `bold` toggle is on, otherwise `Modifier::empty()` (a no-op when
    /// passed to `Style::add_modifier`). Every render site routes its bold
    /// through this so the toggle is honoured in exactly one place.
    pub fn bold_modifier(&self) -> Modifier {
        if self.bold {
            Modifier::BOLD
        } else {
            Modifier::empty()
        }
    }

    /// Resolve a `Theme` from an optional user config section.
    ///
    /// Every field in `cfg` that is `Some(valid_color)` overrides the compiled
    /// default. A bad or unrecognised colour value logs a `tracing::warn!` and
    /// falls back to the compiled default for that role — other roles are still
    /// applied. `None` (the `[theme]` section absent) produces the full default.
    pub fn resolve_from(cfg: Option<&ThemeConfig>) -> Self {
        let defaults = Self::default();
        let Some(cfg) = cfg else {
            return defaults;
        };
        /// Resolve one optional colour string against the field's fallback.
        fn role(src: &Option<String>, fallback: Color) -> Color {
            match src.as_ref().and_then(|s| parse_color(s)) {
                Some(c) => c,
                None => {
                    if let Some(bad) = src {
                        tracing::warn!(
                            "unrecognised colour {bad:?} in [theme]; \
                             using compiled default for this role"
                        );
                    }
                    fallback
                }
            }
        }
        Self {
            accent: role(&cfg.accent, defaults.accent),
            accent_bright: role(&cfg.accent_bright, defaults.accent_bright),
            group_accent: role(&cfg.group_accent, defaults.group_accent),
            success: role(&cfg.success, defaults.success),
            warning: role(&cfg.warning, defaults.warning),
            danger: role(&cfg.danger, defaults.danger),
            danger_bright: role(&cfg.danger_bright, defaults.danger_bright),
            muted: role(&cfg.muted, defaults.muted),
            context_target: role(&cfg.context_target, defaults.context_target),
            scheduled: role(&cfg.scheduled, defaults.scheduled),
            path_prefix: role(&cfg.path_prefix, defaults.path_prefix),
            background: role(&cfg.background, defaults.background),
            bold: cfg.bold.unwrap_or(defaults.bold),
        }
    }
}

/// Per-panel layout preferences.
///
/// All fields have defaults that match the current behaviour. These are "density"
/// knobs — they don't change the palette, only the space allocation and wrapping.
///
/// `LayoutPrefs` is stored on `App` alongside `Theme` but is NOT yet wired into
/// the layout logic (S3 of the theming feature). It is introduced here so the
/// struct shape is stable before the wiring.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutPrefs {
    /// Context pane width as a percentage of the list-area width.
    /// `50` = equal split (current hardcoded behaviour). Clamped to [20, 80].
    pub list_pane_percent: u16,
    /// Blank-line density between groups.
    /// `0` = compact (no blank line), `1` = comfortable, `2` = spacious.
    /// Resolved from `DensityPreset` enum for hard-error-on-bad-variant safety.
    pub list_density: u8,
    /// Whether text in the context pane wraps at the pane boundary.
    /// `false` = current behaviour (no wrap).
    pub context_wrap: bool,
}

impl Default for LayoutPrefs {
    fn default() -> Self {
        Self {
            list_pane_percent: 50,
            list_density: 0, // compact = no separators (matches pre-S3 behaviour)
            context_wrap: false,
        }
    }
}

impl LayoutPrefs {
    /// Resolve layout prefs from an optional user config section.
    /// Bad `list_pane_percent` values are clamped to [20, 80] with a warning;
    /// `list_density` maps from the `DensityPreset` enum (hard error on bad
    /// variant at config load, before alt screen); `context_wrap` passes through.
    /// `None` (section absent) produces the full default.
    pub fn resolve_from(cfg: Option<&UiConfig>) -> Self {
        let defaults = Self::default();
        let Some(cfg) = cfg else {
            return defaults;
        };
        Self {
            list_pane_percent: cfg
                .list_pane_percent
                .map(|p| {
                    if !(20..=80).contains(&p) {
                        tracing::warn!(
                            "list_pane_percent={p} out of range [20, 80]; clamping to nearest bound"
                        );
                        p.clamp(20, 80)
                    } else {
                        p
                    }
                })
                .unwrap_or(defaults.list_pane_percent),
            list_density: cfg
                .list_density
                .map(|d| d.blank_lines())
                .unwrap_or(defaults.list_density),
            context_wrap: cfg.context_wrap.unwrap_or(defaults.context_wrap),
        }
    }
}

/// Parse a colour string from user config into a ratatui `Color`.
///
/// Accepted forms:
/// - Named colours: any ratatui spelling, case-insensitive (e.g. `"cyan"`,
///   `"light_red"`, `"DarkGray"`).
/// - Hex truecolor: `"#rrggbb"` or `"#RGB"`.
/// - `"default"`: terminal default foreground (`Color::Reset`).
///
/// Returns `None` for unrecognised spellings or malformed hex values (caller
/// falls back with a warning).
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("default") {
        return Some(Color::Reset);
    }
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex);
    }
    named_color(s)
}

/// Try to parse `s` (without leading `#`) as a hex truecolour.
fn parse_hex(s: &str) -> Option<Color> {
    let expanded = match s.len() {
        3 => s.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => s.to_string(),
        _ => return None,
    };
    let r = u8::from_str_radix(&expanded[0..2], 16).ok()?;
    let g = u8::from_str_radix(&expanded[2..4], 16).ok()?;
    let b = u8::from_str_radix(&expanded[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

/// Case-insensitive named colour lookup matching ratatui's standard palette.
fn named_color(s: &str) -> Option<Color> {
    // Map normalised name → Color via a static slice so we don't heap-allocate
    // a lowercased String per call.
    const NAMES: &[(&str, Color)] = &[
        ("black", Color::Black),
        ("red", Color::Red),
        ("green", Color::Green),
        ("yellow", Color::Yellow),
        ("blue", Color::Blue),
        ("magenta", Color::Magenta),
        ("cyan", Color::Cyan),
        ("gray", Color::Gray),
        ("darkgray", Color::DarkGray),
        ("lightred", Color::LightRed),
        ("lightgreen", Color::LightGreen),
        ("lightyellow", Color::LightYellow),
        ("lightblue", Color::LightBlue),
        ("lightmagenta", Color::LightMagenta),
        ("lightcyan", Color::LightCyan),
        ("white", Color::White),
    ];
    // Need to check both snake_case and no-underscore variants.
    let normalised: String = s
        .chars()
        .filter(|&c| c != '_')
        .flat_map(|c| c.to_lowercase())
        .collect();
    NAMES
        .iter()
        .find(|(name, _)| *name == normalised)
        .map(|(_, color)| *color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use taski_config::DensityPreset;

    /// The default theme must produce the exact palette the TUI shipped with
    /// before theming existed — byte-identical rendering.
    #[test]
    fn default_matches_today_palette() {
        let t = Theme::default();
        assert_eq!(t.accent, Color::Cyan);
        assert_eq!(t.accent_bright, Color::LightCyan);
        assert_eq!(t.group_accent, Color::Magenta);
        assert_eq!(t.success, Color::Green);
        assert_eq!(t.warning, Color::Yellow);
        assert_eq!(t.danger, Color::Red);
        assert_eq!(t.danger_bright, Color::LightRed);
        assert_eq!(t.muted, Color::DarkGray);
        assert_eq!(t.context_target, Color::Yellow);
        assert_eq!(t.scheduled, Color::Cyan);
        // path_prefix is a deliberate addition (ADR-0018 follow-on): it dims the
        // note-header dir prefix by default, defaulting to the same DarkGray as muted.
        assert_eq!(t.path_prefix, Color::DarkGray);
        // background defaults to Reset = "terminal's own background"; `draw`
        // paints nothing while it's Reset, keeping default rendering unchanged.
        assert_eq!(t.background, Color::Reset);
        // bold is off by default (deliberate divergence from pre-theming): bold
        // glyphs render fuzzy on some fonts, so colour contrast carries emphasis.
        assert!(!t.bold);
        assert_eq!(t.bold_modifier(), Modifier::empty());
    }

    /// The global `bold` toggle resolves from config and drives `bold_modifier`.
    #[test]
    fn resolve_from_bold_toggle() {
        // Default / absent → off.
        assert!(!Theme::resolve_from(None).bold);
        // Explicit true → on, and bold_modifier reflects it.
        let on = Theme::resolve_from(Some(&ThemeConfig {
            bold: Some(true),
            ..ThemeConfig::default()
        }));
        assert!(on.bold);
        assert_eq!(on.bold_modifier(), Modifier::BOLD);
        // Explicit false → off.
        let off = Theme::resolve_from(Some(&ThemeConfig {
            bold: Some(false),
            ..ThemeConfig::default()
        }));
        assert!(!off.bold);
        assert_eq!(off.bold_modifier(), Modifier::empty());
    }

    /// The default layout prefs must match the current hardcoded behaviour.
    #[test]
    fn default_layout_matches_current() {
        let lp = LayoutPrefs::default();
        assert_eq!(lp.list_pane_percent, 50);
        assert_eq!(lp.list_density, 0);
        assert!(!lp.context_wrap);
    }

    /// Layout prefs clamps: percent in [20, 80].
    #[test]
    fn layout_percent_in_range() {
        let lp = LayoutPrefs::default();
        assert!(lp.list_pane_percent >= 20);
        assert!(lp.list_pane_percent <= 80);
    }

    // -----------------------------------------------------------------------
    // parse_color
    // -----------------------------------------------------------------------

    #[test]
    fn parse_color_named() {
        assert_eq!(parse_color("cyan"), Some(Color::Cyan));
        assert_eq!(parse_color("CYAN"), Some(Color::Cyan));
        assert_eq!(parse_color("Cyan"), Some(Color::Cyan));
    }

    #[test]
    fn parse_color_snake_case() {
        assert_eq!(parse_color("light_red"), Some(Color::LightRed));
        assert_eq!(parse_color("Light_Red"), Some(Color::LightRed));
        assert_eq!(parse_color("dark_gray"), Some(Color::DarkGray));
    }

    #[test]
    fn parse_color_hex() {
        assert_eq!(parse_color("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("#00FF00"), Some(Color::Rgb(0, 255, 0)));
        assert_eq!(parse_color("#0000ff"), Some(Color::Rgb(0, 0, 255)));
    }

    #[test]
    fn parse_color_hex_short() {
        assert_eq!(parse_color("#f00"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("#0f0"), Some(Color::Rgb(0, 255, 0)));
    }

    #[test]
    fn parse_color_default_reset() {
        assert_eq!(parse_color("default"), Some(Color::Reset));
        assert_eq!(parse_color("DEFAULT"), Some(Color::Reset));
    }

    #[test]
    fn parse_color_unknown_returns_none() {
        assert_eq!(parse_color("mauve"), None);
        assert_eq!(parse_color("#zzzzzz"), None);
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("notacolor"), None);
    }

    // -----------------------------------------------------------------------
    // Theme::resolve_from
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_from_none_uses_all_defaults() {
        let t = Theme::resolve_from(None);
        assert_eq!(t, Theme::default());
    }

    #[test]
    fn resolve_from_partial_override() {
        let cfg = ThemeConfig {
            accent: Some("red".into()),
            ..ThemeConfig::default()
        };
        let t = Theme::resolve_from(Some(&cfg));
        // Overridden role.
        assert_eq!(t.accent, Color::Red);
        // All other roles stay at default.
        assert_eq!(t.accent_bright, Color::LightCyan);
        assert_eq!(t.success, Color::Green);
        assert_eq!(t.warning, Color::Yellow);
        assert_eq!(t.danger, Color::Red);
        assert_eq!(t.danger_bright, Color::LightRed);
        assert_eq!(t.muted, Color::DarkGray);
        assert_eq!(t.context_target, Color::Yellow);
        assert_eq!(t.scheduled, Color::Cyan);
        assert_eq!(t.path_prefix, Color::DarkGray);
        assert_eq!(t.background, Color::Reset);
    }

    /// The new `path_prefix` role is independently overridable.
    #[test]
    fn resolve_from_path_prefix_override() {
        let cfg = ThemeConfig {
            path_prefix: Some("blue".into()),
            ..ThemeConfig::default()
        };
        let t = Theme::resolve_from(Some(&cfg));
        assert_eq!(t.path_prefix, Color::Blue);
        // Other roles stay at default.
        assert_eq!(t.muted, Color::DarkGray);
    }

    /// The `background` role resolves from config; a hex value yields truecolor,
    /// and `"default"` yields `Reset` (the "paint nothing" sentinel).
    #[test]
    fn resolve_from_background_override() {
        let cfg = ThemeConfig {
            background: Some("#1a1b26".into()),
            ..ThemeConfig::default()
        };
        assert_eq!(
            Theme::resolve_from(Some(&cfg)).background,
            Color::Rgb(0x1a, 0x1b, 0x26)
        );

        let cfg_default = ThemeConfig {
            background: Some("default".into()),
            ..ThemeConfig::default()
        };
        assert_eq!(
            Theme::resolve_from(Some(&cfg_default)).background,
            Color::Reset
        );
    }

    /// A bad color string falls back to the compiled default for that role
    /// rather than crashing or refusing the whole config.
    #[test]
    fn resolve_from_bad_value_falls_back() {
        let cfg = ThemeConfig {
            accent: Some("mauve".into()),
            ..ThemeConfig::default()
        };
        let t = Theme::resolve_from(Some(&cfg));
        // accent falls back because "mauve" is not recognised.
        assert_eq!(t.accent, Color::Cyan);
        // Other roles still OK.
        assert_eq!(t.warning, Color::Yellow);
    }

    /// Hex colour resolves correctly through `resolve_from`.
    #[test]
    fn resolve_from_hex_override() {
        let cfg = ThemeConfig {
            accent: Some("#ff6600".into()),
            ..ThemeConfig::default()
        };
        let t = Theme::resolve_from(Some(&cfg));
        assert_eq!(t.accent, Color::Rgb(255, 102, 0));
    }

    /// `"default"` maps to `Color::Reset`.
    #[test]
    fn resolve_from_terminal_default() {
        let cfg = ThemeConfig {
            accent: Some("default".into()),
            ..ThemeConfig::default()
        };
        let t = Theme::resolve_from(Some(&cfg));
        assert_eq!(t.accent, Color::Reset);
    }

    // -----------------------------------------------------------------------
    // DensityPreset
    // -----------------------------------------------------------------------

    #[test]
    fn density_compact_is_zero_blank_lines() {
        assert_eq!(DensityPreset::Compact.blank_lines(), 0);
    }

    #[test]
    fn density_comfortable_is_one_blank_line() {
        assert_eq!(DensityPreset::Comfortable.blank_lines(), 1);
    }

    #[test]
    fn density_spacious_is_two_blank_lines() {
        assert_eq!(DensityPreset::Spacious.blank_lines(), 2);
    }

    // -----------------------------------------------------------------------
    // LayoutPrefs::resolve_from — density presets
    // -----------------------------------------------------------------------

    #[test]
    fn layout_resolve_from_none_uses_all_defaults() {
        let lp = LayoutPrefs::resolve_from(None);
        assert_eq!(lp, LayoutPrefs::default());
    }

    #[test]
    fn resolve_from_compact_density() {
        let cfg = UiConfig {
            list_pane_percent: None,
            list_density: Some(DensityPreset::Compact),
            context_wrap: None,
        };
        let lp = LayoutPrefs::resolve_from(Some(&cfg));
        assert_eq!(lp.list_density, 0);
        // Unset fields stay default.
        assert_eq!(lp.list_pane_percent, 50);
        assert!(!lp.context_wrap);
    }

    #[test]
    fn resolve_from_comfortable_density() {
        let cfg = UiConfig {
            list_pane_percent: None,
            list_density: Some(DensityPreset::Comfortable),
            context_wrap: None,
        };
        let lp = LayoutPrefs::resolve_from(Some(&cfg));
        assert_eq!(lp.list_density, 1);
    }

    #[test]
    fn resolve_from_spacious_density() {
        let cfg = UiConfig {
            list_pane_percent: None,
            list_density: Some(DensityPreset::Spacious),
            context_wrap: None,
        };
        let lp = LayoutPrefs::resolve_from(Some(&cfg));
        assert_eq!(lp.list_density, 2);
    }

    // -----------------------------------------------------------------------
    // LayoutPrefs::resolve_from — pane percent
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_from_pane_percent_accepted() {
        let cfg = UiConfig {
            list_pane_percent: Some(65),
            list_density: None,
            context_wrap: None,
        };
        let lp = LayoutPrefs::resolve_from(Some(&cfg));
        assert_eq!(lp.list_pane_percent, 65);
    }

    #[test]
    fn resolve_from_pane_percent_clamps_low_to_20() {
        let cfg = UiConfig {
            list_pane_percent: Some(5),
            list_density: None,
            context_wrap: None,
        };
        let lp = LayoutPrefs::resolve_from(Some(&cfg));
        assert_eq!(lp.list_pane_percent, 20);
    }

    #[test]
    fn resolve_from_pane_percent_clamps_high_to_80() {
        let cfg = UiConfig {
            list_pane_percent: Some(95),
            list_density: None,
            context_wrap: None,
        };
        let lp = LayoutPrefs::resolve_from(Some(&cfg));
        assert_eq!(lp.list_pane_percent, 80);
    }

    // -----------------------------------------------------------------------
    // LayoutPrefs::resolve_from — context_wrap
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_from_context_wrap_false() {
        let cfg = UiConfig {
            list_pane_percent: None,
            list_density: None,
            context_wrap: Some(false),
        };
        let lp = LayoutPrefs::resolve_from(Some(&cfg));
        assert!(!lp.context_wrap);
    }

    #[test]
    fn resolve_from_context_wrap_true() {
        let cfg = UiConfig {
            list_pane_percent: None,
            list_density: None,
            context_wrap: Some(true),
        };
        let lp = LayoutPrefs::resolve_from(Some(&cfg));
        assert!(lp.context_wrap);
    }
}
