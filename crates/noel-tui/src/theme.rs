use std::collections::BTreeMap;

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

pub const BUILTIN_THEMES: &[&str] = &["system", "light", "dark", "kimi"];

/// Semantic colors shared by every widget. Reset inherits the terminal palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    pub foreground: Color,
    pub background: Color,
    pub surface: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    /// The extended roles, from upstream's ~60-token set (#212):
    /// `Color::Reset` means "fall back to the base role" everywhere,
    /// so a palette that sets none of them renders exactly as before.
    /// The thinking gradient, `thinking_off` (faintest) to
    /// `thinking_max` (hottest), colors the thinking entries' label by
    /// the session's level.
    pub thinking_off: Color,
    pub thinking_minimal: Color,
    pub thinking_low: Color,
    pub thinking_medium: Color,
    pub thinking_high: Color,
    pub thinking_xhigh: Color,
    pub thinking_max: Color,
    /// The tool rows' status tints (upstream toolPendingBg/Success/Error).
    pub tool_pending_bg: Color,
    pub tool_success_bg: Color,
    pub tool_error_bg: Color,
    /// Diff lines (upstream toolDiffAdded/Removed/Context).
    pub diff_added: Color,
    pub diff_removed: Color,
    pub diff_context: Color,
    /// Markdown emphasis (upstream mdHeading/mdLink/mdCode/mdQuote).
    pub md_heading: Color,
    pub md_link: Color,
    pub md_code: Color,
    pub md_quote: Color,
}

/// A tool row's phase, for its status tint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    Pending,
    Success,
    Error,
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self {
            foreground: Color::Reset,
            background: Color::Reset,
            surface: Color::Reset,
            muted: Color::Reset,
            border: Color::Reset,
            accent: Color::Cyan,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            thinking_off: Color::Reset,
            thinking_minimal: Color::Reset,
            thinking_low: Color::Reset,
            thinking_medium: Color::Reset,
            thinking_high: Color::Reset,
            thinking_xhigh: Color::Reset,
            thinking_max: Color::Reset,
            tool_pending_bg: Color::Reset,
            tool_success_bg: Color::Reset,
            tool_error_bg: Color::Reset,
            diff_added: Color::Reset,
            diff_removed: Color::Reset,
            diff_context: Color::Reset,
            md_heading: Color::Reset,
            md_link: Color::Reset,
            md_code: Color::Reset,
            md_quote: Color::Reset,
        }
    }
}

impl ThemePalette {
    /// The mutable slot for a role name: every palette role, extended
    /// ones included, so theme JSON can override any of them.
    pub fn role_mut(&mut self, role: &str) -> Option<&mut Color> {
        Some(match role {
            "foreground" => &mut self.foreground,
            "background" => &mut self.background,
            "surface" => &mut self.surface,
            "muted" => &mut self.muted,
            "border" => &mut self.border,
            "accent" => &mut self.accent,
            "success" => &mut self.success,
            "warning" => &mut self.warning,
            "error" => &mut self.error,
            "thinkingOff" => &mut self.thinking_off,
            "thinkingMinimal" => &mut self.thinking_minimal,
            "thinkingLow" => &mut self.thinking_low,
            "thinkingMedium" => &mut self.thinking_medium,
            "thinkingHigh" => &mut self.thinking_high,
            "thinkingXhigh" => &mut self.thinking_xhigh,
            "thinkingMax" => &mut self.thinking_max,
            "toolPendingBg" => &mut self.tool_pending_bg,
            "toolSuccessBg" => &mut self.tool_success_bg,
            "toolErrorBg" => &mut self.tool_error_bg,
            "diffAdded" => &mut self.diff_added,
            "diffRemoved" => &mut self.diff_removed,
            "diffContext" => &mut self.diff_context,
            "mdHeading" => &mut self.md_heading,
            "mdLink" => &mut self.md_link,
            "mdCode" => &mut self.md_code,
            "mdQuote" => &mut self.md_quote,
            _ => return None,
        })
    }

    /// A color, or `fallback` when the role is unset.
    fn or(self, color: Color, fallback: Color) -> Color {
        if color != Color::Reset {
            color
        } else {
            fallback
        }
    }

    /// The thinking label's color at a level name, upstream's
    /// thinkingOff..Max gradient falling back to `muted`.
    pub fn thinking(self, level: &str) -> Color {
        let gradient = match level.to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => self.thinking_off,
            "minimal" | "min" => self.thinking_minimal,
            "low" => self.thinking_low,
            "medium" | "med" => self.thinking_medium,
            "high" => self.thinking_high,
            "xhigh" | "extra-high" => self.thinking_xhigh,
            "max" => self.thinking_max,
            _ => Color::Reset,
        };
        self.or(gradient, self.muted)
    }

    /// The tool status tint for a phase, falling back to transparent
    /// (the terminal background): pending is muted, success and error
    /// reuse their base roles only as text colors elsewhere.
    pub fn tool_bg(self, phase: ToolPhase) -> Color {
        match phase {
            ToolPhase::Pending => self.or(self.tool_pending_bg, Color::Reset),
            ToolPhase::Success => self.or(self.tool_success_bg, Color::Reset),
            ToolPhase::Error => self.or(self.tool_error_bg, Color::Reset),
        }
    }

    pub fn diff_added(self) -> Color {
        self.or(self.diff_added, self.success)
    }

    pub fn diff_removed(self) -> Color {
        self.or(self.diff_removed, self.error)
    }

    pub fn diff_context(self) -> Color {
        self.or(self.diff_context, self.muted)
    }

    pub fn md_heading(self) -> Color {
        self.or(self.md_heading, self.accent)
    }

    pub fn md_link(self) -> Color {
        self.or(self.md_link, self.accent)
    }

    pub fn md_code(self) -> Color {
        self.or(self.md_code, self.accent)
    }

    pub fn md_quote(self) -> Color {
        self.or(self.md_quote, self.muted)
    }

    pub fn base(self) -> Style {
        Style::default().fg(self.foreground).bg(self.background)
    }

    pub fn secondary(self) -> Style {
        let style = Style::default().fg(self.muted);
        // Native themes use intensity, not a fixed gray that assumes a dark background.
        if self.muted == Color::Reset {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    }

    pub fn selected(self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalTheme {
    pub name: String,
    pub palette: ThemePalette,
}

impl Default for TerminalTheme {
    fn default() -> Self {
        Self::builtin("system").expect("system theme exists")
    }
}

impl TerminalTheme {
    pub fn builtin(name: &str) -> Option<Self> {
        let name = if name == "default" { "system" } else { name };
        let palette = match name {
            "system" => ThemePalette::default(),
            // Upstream pi's dark.json/light.json, authored in okhsl and
            // converted to sRGB: violet accents, blue borders, muted grays
            // with a slight hue. Roles without an upstream token keep the
            // nearest one: surface is upstream's selectedBg panel tint.
            "light" | "kimi" => ThemePalette {
                foreground: Color::Rgb(0x3B, 0x3F, 0x41),
                background: Color::Reset,
                // A touch lighter than upstream's selectedBg (okhsl 235 19% 91%)
                // to keep every role at WCAG AA on the surface.
                surface: Color::Rgb(0xF4, 0xF6, 0xF8),
                muted: Color::Rgb(0x67, 0x71, 0x76),
                border: Color::Rgb(0x3D, 0x8E, 0xB3),
                accent: if name == "kimi" {
                    Color::Rgb(0x25, 0x63, 0xEB)
                } else {
                    Color::Rgb(0x74, 0x59, 0xB4)
                },
                success: Color::Rgb(0x33, 0x7E, 0x58),
                warning: Color::Rgb(0x8F, 0x68, 0x02),
                error: Color::Rgb(0xC8, 0x25, 0x3D),
                // Upstream's token values, converted from okhsl.
                thinking_off: Color::Rgb(0xC2, 0xC8, 0xCA),
                thinking_minimal: Color::Rgb(0xB5, 0xC4, 0xCB),
                thinking_low: Color::Rgb(0x9F, 0xC2, 0xD5),
                thinking_medium: Color::Rgb(0xA2, 0xB7, 0xE0),
                thinking_high: Color::Rgb(0xB5, 0xA5, 0xE8),
                thinking_xhigh: Color::Rgb(0xE5, 0x85, 0xCD),
                thinking_max: Color::Rgb(0xFE, 0x74, 0x79),
                tool_pending_bg: Color::Rgb(0xE4, 0xE5, 0xE6),
                tool_success_bg: Color::Rgb(0xDE, 0xE9, 0xE1),
                tool_error_bg: Color::Rgb(0xEE, 0xE2, 0xE1),
                diff_added: Color::Rgb(0x33, 0x7E, 0x58),
                diff_removed: Color::Rgb(0xC8, 0x25, 0x3D),
                md_heading: Color::Rgb(0x8F, 0x68, 0x02),
                md_link: Color::Rgb(0x2F, 0x78, 0x99),
                md_code: Color::Rgb(0x74, 0x59, 0xB4),
                ..ThemePalette::default()
            },
            "dark" => ThemePalette {
                foreground: Color::Rgb(0xDE, 0xE0, 0xE1),
                background: Color::Reset,
                // A touch darker than upstream's selectedBg (okhsl 233 41% 24%)
                // to keep every role at WCAG AA on the surface.
                surface: Color::Rgb(0x1E, 0x36, 0x43),
                muted: Color::Rgb(0x9D, 0xA5, 0xA9),
                border: Color::Rgb(0x5F, 0xA8, 0xCC),
                accent: Color::Rgb(0xA7, 0x98, 0xD7),
                success: Color::Rgb(0x68, 0xB7, 0x8D),
                warning: Color::Rgb(0xCD, 0x9A, 0x22),
                error: Color::Rgb(0xEA, 0x7F, 0x81),
                // Upstream's token values, converted from okhsl: the
                // thinking gradient, tool status tints, diffs, markdown.
                thinking_off: Color::Rgb(0x6C, 0x76, 0x7B),
                thinking_minimal: Color::Rgb(0x68, 0x80, 0x8D),
                thinking_low: Color::Rgb(0x54, 0x89, 0xA4),
                thinking_medium: Color::Rgb(0x61, 0x85, 0xCC),
                thinking_high: Color::Rgb(0x97, 0x76, 0xE5),
                thinking_xhigh: Color::Rgb(0xDE, 0x54, 0xC1),
                thinking_max: Color::Rgb(0xFE, 0x54, 0x62),
                tool_pending_bg: Color::Rgb(0x34, 0x38, 0x3A),
                tool_success_bg: Color::Rgb(0x25, 0x41, 0x31),
                tool_error_bg: Color::Rgb(0x5B, 0x28, 0x2A),
                diff_added: Color::Rgb(0x68, 0xB7, 0x8D),
                diff_removed: Color::Rgb(0xEA, 0x7F, 0x81),
                md_heading: Color::Rgb(0xCD, 0x9A, 0x22),
                md_link: Color::Rgb(0x69, 0xAD, 0xD0),
                md_code: Color::Rgb(0xA7, 0x98, 0xD7),
                ..ThemePalette::default()
            },
            _ => return None,
        };
        Some(Self {
            name: name.to_string(),
            palette,
        })
    }

    /// Name-only legacy resources inherit their namesake preset, or system.
    /// Custom palettes extend a built-in and override semantic color roles.
    pub fn from_json(name: &str, content: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Definition {
            base: Option<String>,
            #[serde(default)]
            colors: BTreeMap<String, String>,
        }
        let definition: Definition =
            serde_json::from_str(content).map_err(|error| format!("theme {name}: {error}"))?;
        let mut theme = if let Some(base) = definition.base {
            Self::builtin(&base).ok_or_else(|| format!("theme {name}: unknown base {base}"))?
        } else {
            Self::builtin(name).unwrap_or_default()
        };
        theme.name = name.to_string();
        for (role, value) in definition.colors {
            let color = parse_theme_color(&value)
                .map_err(|error| format!("theme {name}, {role}: {error}"))?;
            let target = theme.palette.role_mut(&role);
            let target = match target {
                Some(target) => target,
                None => return Err(format!("theme {name}: unknown color role {role}")),
            };
            *target = color;
        }
        Ok(theme)
    }

    pub fn with_accent(mut self, accent: Option<&str>) -> Result<Self, String> {
        if let Some(accent) = accent {
            self.palette.accent = parse_theme_color(accent)?;
        }
        Ok(self)
    }
}

/// ANSI color names, 0–255 palette indices, #RRGGBB, or terminal default.
pub fn parse_theme_color(value: &str) -> Result<Color, String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("default") {
        return Ok(Color::Reset);
    }
    value
        .parse()
        .map_err(|_| format!("invalid color {value:?}; use an ANSI name, 0–255, or #RRGGBB"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_inherits_terminal_colors() {
        let theme = TerminalTheme::default();
        assert_eq!(theme.name, "system");
        for color in [
            theme.palette.foreground,
            theme.palette.background,
            theme.palette.surface,
            theme.palette.muted,
            theme.palette.border,
        ] {
            assert_eq!(color, Color::Reset);
        }
        assert!(theme
            .palette
            .secondary()
            .add_modifier
            .contains(Modifier::DIM));
        assert_eq!(TerminalTheme::builtin("default"), Some(theme));
    }

    #[test]
    fn presets_have_distinct_palettes_and_readable_secondary_text() {
        for name in BUILTIN_THEMES {
            let theme = TerminalTheme::builtin(name).unwrap();
            if *name != "system" {
                assert_ne!(theme.palette.foreground, theme.palette.background);
                assert_ne!(theme.palette.muted, theme.palette.surface);
                assert!(!theme
                    .palette
                    .secondary()
                    .add_modifier
                    .contains(Modifier::DIM));
            }
        }
        assert_ne!(
            TerminalTheme::builtin("light"),
            TerminalTheme::builtin("dark")
        );
        assert_eq!(
            TerminalTheme::builtin("kimi").unwrap().palette.accent,
            Color::Rgb(0x25, 0x63, 0xEB)
        );
    }

    #[test]
    fn explicit_presets_keep_text_contrast_on_background_and_surface() {
        fn luminance(color: Color) -> f64 {
            let Color::Rgb(r, g, b) = color else {
                panic!("expected explicit preset color")
            };
            let linear = |value: u8| {
                let value = f64::from(value) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        }
        for name in ["light", "dark", "kimi"] {
            let palette = TerminalTheme::builtin(name).unwrap().palette;
            // Presets paint on the terminal's own background; only the
            // surface tint is theirs, like upstream's selectedBg panels.
            assert_eq!(palette.background, Color::Reset);
            for foreground in [
                palette.foreground,
                palette.muted,
                palette.accent,
                palette.success,
                palette.warning,
                palette.error,
            ] {
                let foreground = luminance(foreground);
                let background = luminance(palette.surface);
                let ratio =
                    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05);
                assert!(ratio >= 4.5, "{name} text contrast: {ratio}");
            }
        }
    }

    #[test]
    fn custom_roles_and_accent_override() {
        let theme = TerminalTheme::from_json("custom", r##"{"base":"dark","colors":{"accent":"#123456","surface":"default","success":"green"}}"##).unwrap();
        assert_eq!(theme.name, "custom");
        assert_eq!(theme.palette.accent, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(theme.palette.surface, Color::Reset);
        assert_eq!(theme.palette.success, Color::Green);
        assert_eq!(
            theme.with_accent(Some("magenta")).unwrap().palette.accent,
            Color::Magenta
        );
        assert_eq!(parse_theme_color("123").unwrap(), Color::Indexed(123));
    }

    #[test]
    fn legacy_resources_and_invalid_palettes() {
        assert_eq!(
            TerminalTheme::from_json("dark", r#"{"name":"dark"}"#).unwrap(),
            TerminalTheme::builtin("dark").unwrap()
        );
        assert_eq!(
            TerminalTheme::from_json("custom", "{}").unwrap().palette,
            ThemePalette::default()
        );
        for content in [
            "not JSON",
            r#"{"base":"missing"}"#,
            r#"{"colors":{"accent":"bogus"}}"#,
            r#"{"colors":{"typo":"red"}}"#,
        ] {
            assert!(TerminalTheme::from_json("bad", content).is_err());
        }
        assert!(parse_theme_color("256").is_err());
    }
}
