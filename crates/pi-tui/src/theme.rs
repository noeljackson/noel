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
        }
    }
}

impl ThemePalette {
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
            "light" | "kimi" => ThemePalette {
                foreground: Color::Rgb(0x24, 0x29, 0x32),
                background: Color::Rgb(0xFA, 0xFA, 0xF9),
                surface: Color::Rgb(0xF0, 0xF1, 0xF3),
                muted: Color::Rgb(0x60, 0x67, 0x73),
                border: Color::Rgb(0x80, 0x87, 0x92),
                accent: if name == "kimi" {
                    Color::Rgb(0x25, 0x63, 0xEB)
                } else {
                    Color::Rgb(0x00, 0x69, 0x80)
                },
                success: Color::Rgb(0x16, 0x70, 0x3A),
                warning: Color::Rgb(0x91, 0x5A, 0x00),
                error: Color::Rgb(0xBE, 0x25, 0x35),
            },
            "dark" => ThemePalette {
                foreground: Color::Rgb(0xE4, 0xE7, 0xEC),
                background: Color::Rgb(0x18, 0x1B, 0x22),
                surface: Color::Rgb(0x25, 0x29, 0x33),
                muted: Color::Rgb(0xA2, 0xAA, 0xB8),
                border: Color::Rgb(0x78, 0x83, 0x96),
                accent: Color::Rgb(0x79, 0xBC, 0xFF),
                success: Color::Rgb(0x7A, 0xD6, 0x9B),
                warning: Color::Rgb(0xED, 0xC3, 0x72),
                error: Color::Rgb(0xFF, 0x8A, 0x94),
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
            let target = match role.as_str() {
                "foreground" => &mut theme.palette.foreground,
                "background" => &mut theme.palette.background,
                "surface" => &mut theme.palette.surface,
                "muted" => &mut theme.palette.muted,
                "border" => &mut theme.palette.border,
                "accent" => &mut theme.palette.accent,
                "success" => &mut theme.palette.success,
                "warning" => &mut theme.palette.warning,
                "error" => &mut theme.palette.error,
                _ => return Err(format!("theme {name}: unknown color role {role}")),
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
            for foreground in [
                palette.foreground,
                palette.muted,
                palette.accent,
                palette.success,
                palette.warning,
                palette.error,
            ] {
                for background in [palette.background, palette.surface] {
                    let foreground = luminance(foreground);
                    let background = luminance(background);
                    let ratio =
                        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05);
                    assert!(ratio >= 4.5, "{name} text contrast: {ratio}");
                }
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
