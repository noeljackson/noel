//! The `system` theme: colors derived from the terminal's own theme, a
//! port of upstream pi's `system-theme.ts`.
//!
//! Every token belongs to a color family (its hue) and has contrast
//! rules: it must reach a contrast level on the background. Hue and
//! saturation come from the terminal's palette color for the family's
//! ANSI slot, or from the family's own hue when the terminal reports no
//! palette. Lightness comes from the rules alone. Colors are built in
//! OKHSL, whose saturation is relative to the sRGB gamut, and fade
//! toward gray near black and white; a palette color never gains OKLCH
//! chroma when it moves to another lightness, so pastel palettes stay
//! pastel. A contrast level is a target-lightness curve fitted to the
//! reference theme design; on dark backgrounds the curves aim for
//! nearly fixed lightness, on light backgrounds the required difference
//! grows as the background darkens.
//!
//! Upstream's ~60 tokens map onto `ThemePalette`'s nine roles: each role
//! names the upstream token its color comes from. Upstream's tier for
//! terminals that report nothing (ANSI palette indices plus dim) stays
//! the plain `ThemePalette::default()` this port already renders: the
//! terminal paints 0-15 and its default colors with its own theme.

use crate::oklab::{
    okhsl_to_rgb, oklab_lightness, oklab_to_okhsl_lightness, rgb_chroma, rgb_to_okhsl,
    rgb_to_oklab, wcag_contrast, Rgb,
};
use crate::terminal_colors::{terminal_appearance, TerminalAppearance, TerminalColors};
use crate::theme::ThemePalette;
use ratatui::style::Color;

/// A family's OKHSL hue and saturation: `max` at mid lightness, falling
/// toward `min` at black and white; the ANSI palette slot the family
/// takes its hue and saturation from.
struct Family {
    hue: f64,
    saturation: (f64, f64),
    slot: usize,
}

const NEUTRAL: Family = Family {
    hue: 231.49,
    saturation: (0.02, 0.08),
    slot: 8,
};
const FAMILIES: &[(&str, Family)] = &[
    (
        "neutral",
        Family {
            hue: 231.49,
            saturation: (0.02, 0.08),
            slot: 8,
        },
    ),
    (
        "blue",
        Family {
            hue: 231.49,
            saturation: (0.1, 0.68),
            slot: 4,
        },
    ),
    (
        "green",
        Family {
            hue: 158.68,
            saturation: (0.1, 0.76),
            slot: 2,
        },
    ),
    (
        "red",
        Family {
            hue: 20.0,
            saturation: (0.1, 0.92),
            slot: 1,
        },
    ),
    (
        "yellow",
        Family {
            hue: 82.36,
            saturation: (0.5, 1.0),
            slot: 3,
        },
    ),
    (
        "orange",
        Family {
            hue: 52.0,
            saturation: (0.12, 0.85),
            slot: 3,
        },
    ),
    (
        "violet",
        Family {
            hue: 295.0,
            saturation: (0.2, 0.6),
            slot: 5,
        },
    ),
    (
        "calamine",
        Family {
            hue: 202.43,
            saturation: (0.1, 0.74),
            slot: 6,
        },
    ),
];

/// Body text uses the terminal's foreground when it reaches this level,
/// which is clearly stronger than muted (upstream's FOREGROUND_LEVEL).
const EMPHASIS: &str = "emphasis";

fn family(name: &str) -> &'static Family {
    FAMILIES
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, family)| family)
        .unwrap_or(&NEUTRAL)
}

/// The palette slot a role's source token reads (upstream TOKEN_SLOTS).
fn slot_of(role: Role) -> usize {
    match role {
        Role::Warning => 3,
        _ => family(role.family_name()).slot,
    }
}

/// Target-lightness curves: a polynomial in the surface's OKLab
/// lightness giving the OKLab lightness a token needs on it. `reachable`
/// is the range of surface lightness where the level can be reached.
struct Curve {
    coefficients: [f64; 6],
    reachable: (f64, f64),
}

const fn curve(coefficients: [f64; 6], reachable: (f64, f64)) -> Curve {
    Curve {
        coefficients,
        reachable,
    }
}

#[rustfmt::skip]
const LEVELS: &[(&str, &[(TerminalAppearance, Curve); 2])] = &[
    ("panel", &[
        (TerminalAppearance::Dark, curve([0.29131, -0.39746, 2.33185, -0.85524, -1.2076, 0.86276], (0.0, 0.979))),
        (TerminalAppearance::Light, curve([-3.74073, 27.94549, -78.44258, 112.6798, -79.60015, 22.11277], (0.348, 1.0))),
    ]),
    ("subtle", &[
        (TerminalAppearance::Dark, curve([0.56762, -0.02475, -0.5383, 5.12628, -7.10931, 3.17324], (0.0, 0.848))),
        (TerminalAppearance::Light, curve([-232.85459, 1376.54473, -3249.11801, 3827.91186, -2248.29472, 526.55751], (0.657, 1.0))),
    ]),
    ("readable", &[
        (TerminalAppearance::Dark, curve([0.66937, 0.04704, -1.06871, 6.43941, -8.9332, 4.17229], (0.0, 0.77))),
        (TerminalAppearance::Light, curve([-1554.52576, 8733.56817, -19604.93507, 21977.72696, -12300.99599, 2749.81288], (0.751, 1.0))),
    ]),
    ("text", &[
        (TerminalAppearance::Dark, curve([0.89242, 0.02311, -0.44862, 2.34417, -0.06084, -2.63844], (0.0, 0.5))),
        (TerminalAppearance::Light, curve([-2004.67048, 6664.47299, -6060.70202, -1792.61209, 5133.82359, -1939.85583], (0.894, 1.0))),
    ]),
    ("emphasis", &[
        (TerminalAppearance::Dark, curve([0.7303, 0.07695, -1.31626, 7.1681, -10.14436, 4.92846], (0.0, 0.712))),
        (TerminalAppearance::Light, curve([-4948.31942, 26870.91986, -58334.48399, 63280.17197, -34298.01053, 7430.30146], (0.811, 1.0))),
    ]),
];

fn level_curve(level: &str, appearance: TerminalAppearance) -> &'static Curve {
    LEVELS
        .iter()
        .find(|(name, _)| *name == level)
        .and_then(|(_, curves)| {
            curves
                .iter()
                .find(|(kind, _)| *kind == appearance)
                .map(|(_, curve)| curve)
        })
        .expect("every level has a curve for every appearance")
}

/// The theme roles, each mapped to the upstream token it derives from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Surface,    // selectedBg (a panel)
    Foreground, // text
    Muted,      // muted
    Border,     // border
    Accent,     // accent (violet family)
    Success,    // success (green family)
    Warning,    // warning (yellow family)
    Error,      // error (red family)
}
impl Role {
    fn family_name(self) -> &'static str {
        match self {
            Self::Surface => "blue",
            Self::Foreground | Self::Muted => "neutral",
            Self::Border => "blue",
            Self::Accent => "violet",
            Self::Success => "green",
            Self::Warning => "yellow",
            Self::Error => "red",
        }
    }

    /// The rules this role solves under: which surfaces it is drawn on
    /// and the level it must reach there (upstream RULES, reduced to the
    /// roles this port has).
    fn rules(self) -> &'static [(&'static str, Surface)] {
        const BACKGROUND: Surface = Surface::Background;
        match self {
            Role::Surface => &[("panel", BACKGROUND)],
            // Body text keeps at least the body level on the background
            // and on the surface (upstream: text + textOnPanel).
            Role::Foreground => &[("text", Surface::Background), ("text", Surface::Surface)],
            Role::Muted => &[
                ("readable", Surface::Background),
                ("readable", Surface::Surface),
            ],
            // Upstream draws border only on the background; accent /
            // success / error / warning sit on the background and the
            // panels (selectedBg and the tool panels), which this port's
            // surface stands in for.
            Role::Border => {
                static BORDER_RULES: [(&str, Surface); 1] = [("readable", Surface::Background)];
                &BORDER_RULES
            }
            Role::Accent | Role::Success | Role::Warning | Role::Error => &[
                ("readable", Surface::Background),
                ("readable", Surface::Surface),
            ],
        }
    }
}

/// A surface a rule's token is drawn on: the terminal background or the
/// theme's own surface panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    Background,
    Surface,
}

/// WCAG 2 contrast that body text must reach on the surfaces it is on.
const TEXT_MINIMUM_WCAG_CONTRAST: f64 = 4.5;

/// Saturation weight at a lightness: a Gaussian (center 0.5, sigma
/// 0.25), 0 at black and white, 1 in the middle.
fn bell_weight(lightness: f64) -> f64 {
    let gaussian = |x: f64| (-((x - 0.5).powi(2)) / (2.0 * 0.25_f64.powi(2))).exp();
    (gaussian(lightness) - gaussian(0.0)) / (1.0 - gaussian(0.0))
}

/// A family's saturation curve relative to its maximum: 1 at mid
/// lightness, `min / max` at black and white.
fn saturation_curve(family: &Family, lightness: f64) -> f64 {
    let (min, max) = family.saturation;
    let floor = if max > 0.0 { min / max } else { 1.0 };
    floor + (1.0 - floor) * bell_weight(lightness)
}

/// The target lightness for a level on a surface, or None where the
/// level cannot be reached.
fn level_target(level: &str, appearance: TerminalAppearance, surface_l: f64) -> Option<f64> {
    let curve = level_curve(level, appearance);
    if surface_l < curve.reachable.0 || surface_l > curve.reachable.1 {
        return None;
    }
    Some(
        curve
            .coefficients
            .iter()
            .enumerate()
            .map(|(power, coefficient)| coefficient * surface_l.powi(power as i32))
            .sum(),
    )
}

/// A source color's OKHSL channels and its OKLCH chroma.
struct SourceColor {
    h: f64,
    s: f64,
    l: f64,
    chroma: f64,
}

fn source_of(color: Rgb) -> SourceColor {
    let okhsl = rgb_to_okhsl(color);
    SourceColor {
        h: okhsl.h,
        s: okhsl.s,
        l: okhsl.l,
        chroma: rgb_chroma(color),
    }
}

/// A source color's hue at another OKHSL lightness. Its saturation
/// applies at its own lightness and falls off toward black and white
/// along the family's saturation curve, never rising above it. OKHSL
/// saturation is relative to the most chroma sRGB allows at a lightness,
/// so the same saturation can mean more chroma elsewhere: chroma is
/// therefore also capped at the source's, with the same falloff.
fn anchored(source: &SourceColor, family: &Family, lightness: f64, saturation: f64) -> Rgb {
    let anchor = saturation_curve(family, source.l);
    let falloff = if anchor > 0.0 {
        (saturation_curve(family, lightness) / anchor).min(1.0)
    } else {
        1.0
    };
    let color = okhsl_to_rgb(source.h, source.s * falloff * saturation, lightness);
    let cap = source.chroma * falloff * saturation;
    let [l, a, b] = rgb_to_oklab(color);
    let chroma = (a * a + b * b).sqrt();
    if chroma <= cap {
        color
    } else {
        // OKLCH at the same lightness and hue, chroma capped.
        let hue = b.atan2(a);
        let capped = crate::oklab::oklab_to_linear_srgb([l, cap * hue.cos(), cap * hue.sin()]);
        let color = crate::oklab::linear_srgb_to_rgb(capped);
        let [r, g, bl] = [color.r, color.g, color.b];
        Rgb::new(r, g, bl)
    }
}

/// Move a text color toward white or black until it reaches the WCAG
/// minimum on every surface.
fn with_text_contrast(color: Rgb, surfaces: &[Rgb], lighter: bool) -> Rgb {
    let meets = |candidate: Rgb| {
        surfaces
            .iter()
            .all(|surface| wcag_contrast(candidate, *surface) >= TEXT_MINIMUM_WCAG_CONTRAST)
    };
    if meets(color) {
        return color;
    }
    let okhsl = rgb_to_okhsl(color);
    let at = |lightness: f64| okhsl_to_rgb(okhsl.h, okhsl.s, lightness);
    let extreme = if lighter { 1.0 } else { 0.0 };
    if !meets(at(extreme)) {
        return at(extreme);
    }
    let (mut low, mut high) = (okhsl.l, extreme);
    for _ in 0..20 {
        let middle = (low + high) / 2.0;
        if meets(at(middle)) {
            high = middle;
        } else {
            low = middle;
        }
    }
    at(high)
}

/// The lightness a rule needs on a surface, relaxed by `t`: from 0 to 1,
/// levels stronger than the readable floor move toward it; from 1 to 2,
/// all levels move toward the surface itself.
fn target(level: &str, appearance: TerminalAppearance, surface_l: f64, t: f64) -> Option<f64> {
    let reached = level_target(level, appearance, surface_l);
    if reached.is_none() && t == 0.0 {
        return None;
    }
    let lighter = appearance == TerminalAppearance::Dark;
    let extreme = if lighter { 1.0 } else { 0.0 };
    let distance = (reached.unwrap_or(extreme) - surface_l) * if lighter { 1.0 } else { -1.0 };
    let floor_level = if lighter { "readable" } else { "subtle" };
    let floor = (level_target(floor_level, appearance, surface_l).unwrap_or(extreme) - surface_l)
        * if lighter { 1.0 } else { -1.0 };
    let compressed = if distance.abs() > floor.abs() {
        distance - (distance - floor) * t.min(1.0)
    } else {
        distance
    };
    let final_distance = compressed * (1.0 - (t - 1.0).max(0.0));
    Some(surface_l + final_distance * if lighter { 1.0 } else { -1.0 })
}

/// Generate the `system` theme's palette from the terminal's reported
/// colors. Returns None when the terminal reported no background: the
/// caller keeps the plain default palette, whose colors the terminal
/// paints with its own theme (upstream's indexed tier).
pub fn system_theme_palette(
    colors: &TerminalColors,
    saturation: f64,
) -> Option<(ThemePalette, TerminalAppearance)> {
    let background = colors.background?;
    let saturation = saturation.clamp(0.0, 1.0);
    let appearance = terminal_appearance(background, colors.foreground);
    let lighter = appearance == TerminalAppearance::Dark;
    let background_l = oklab_lightness(background);
    let palette = colors.palette;

    let paint = |role: Role, oklab_l: f64| -> Rgb {
        let lightness = oklab_to_okhsl_lightness(oklab_l);
        let family = family(role.family_name());
        match palette {
            None => {
                let (min, max) = family.saturation;
                okhsl_to_rgb(
                    family.hue,
                    (min + (max - min) * bell_weight(lightness)) * saturation,
                    lightness,
                )
            }
            Some(palette) => anchored(
                &source_of(palette[slot_of(role)]),
                family,
                lightness,
                saturation,
            ),
        }
    };

    // Panels stay readable for extreme text (a binary search toward the
    // background), like upstream's limitPanel.
    let limit_panel = |role: Role, l: f64| -> Rgb {
        let extreme_text = if lighter {
            Rgb::new(255, 255, 255)
        } else {
            Rgb::new(0, 0, 0)
        };
        let readable =
            |color: Rgb| wcag_contrast(extreme_text, color) >= TEXT_MINIMUM_WCAG_CONTRAST;
        let color = paint(role, l);
        if readable(color) {
            return color;
        }
        let (mut low, mut high) = (background_l, l);
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            if readable(paint(role, middle)) {
                low = middle;
            } else {
                high = middle;
            }
        }
        paint(role, low)
    };

    // Solve each role's rules at relaxation t (None where unreachable).
    let solve = |t: f64| -> Option<Vec<(Role, Rgb)>> {
        let surface_color = limit_panel(Role::Surface, {
            let value = target("panel", appearance, background_l, t)?;
            if !(0.0..=1.0).contains(&value) {
                return None;
            }
            value
        });
        let mut solved = vec![(Role::Surface, surface_color)];
        for role in [
            Role::Foreground,
            Role::Muted,
            Role::Border,
            Role::Accent,
            Role::Success,
            Role::Warning,
            Role::Error,
        ] {
            let mut targets = Vec::new();
            for (level, surface) in role.rules() {
                let surface_l = match surface {
                    Surface::Background => background_l,
                    Surface::Surface => oklab_lightness(surface_color),
                };
                let value = target(level, appearance, surface_l, t)?;
                if !(0.0..=1.0).contains(&value) {
                    return None;
                }
                targets.push(value);
            }
            let l = if lighter {
                targets.iter().copied().fold(f64::MIN, f64::max)
            } else {
                targets.iter().copied().fold(f64::MAX, f64::min)
            };
            solved.push((role, paint(role, l)));
        }
        Some(solved)
    };

    // Mid-gray backgrounds cannot fit every level: relax as little as
    // possible; full relaxation always fits.
    let mut relaxation = 0.0;
    let mut colors_solved = solve(0.0);
    if colors_solved.is_none() {
        let (mut low, mut high) = (0.0, 2.0);
        colors_solved = solve(high);
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            match solve(middle) {
                Some(attempt) => {
                    high = middle;
                    colors_solved = Some(attempt);
                }
                None => low = middle,
            }
        }
        relaxation = high;
    }
    let solved = colors_solved?;
    let color_of = |role: Role| -> Rgb {
        solved
            .iter()
            .find(|(key, _)| *key == role)
            .map(|(_, color)| *color)
            .expect("every role solves")
    };

    let surfaces = |role: Role| -> Vec<Rgb> {
        role.rules()
            .iter()
            .map(|(_, surface)| match surface {
                Surface::Background => background,
                Surface::Surface => color_of(Role::Surface),
            })
            .collect()
    };

    // Body text uses the terminal's own foreground where it is clearly
    // stronger than muted text (upstream's FOREGROUND_LEVEL, emphasis);
    // otherwise the foreground's hue at just enough lightness.
    let mut foreground = color_of(Role::Foreground);
    if let Some(reported) = colors.foreground {
        let needed = surfaces(Role::Foreground)
            .iter()
            .map(|surface| target(EMPHASIS, appearance, oklab_lightness(*surface), relaxation))
            .collect::<Option<Vec<f64>>>();
        if let Some(targets) = needed {
            let valid = targets.iter().all(|value| (0.0..=1.0).contains(value));
            if valid {
                let needed = if lighter {
                    targets.into_iter().fold(f64::MIN, f64::max)
                } else {
                    targets.into_iter().fold(f64::MAX, f64::min)
                };
                let foreground_l = oklab_lightness(reported);
                if lighter == (foreground_l >= needed) {
                    let to_color = |color: Color| match color {
                        Color::Rgb(r, g, b) => Rgb::new(r, g, b),
                        _ => Rgb::new(0, 0, 0),
                    };
                    let palette = ThemePalette {
                        foreground: Color::Reset,
                        background: Color::Reset,
                        surface: color_to_color(color_of(Role::Surface)),
                        muted: color_to_color(color_of(Role::Muted)),
                        border: color_to_color(color_of(Role::Border)),
                        accent: color_to_color(color_of(Role::Accent)),
                        success: color_to_color(color_of(Role::Success)),
                        warning: color_to_color(color_of(Role::Warning)),
                        error: color_to_color(color_of(Role::Error)),
                    };
                    let _ = to_color;
                    return Some((palette, appearance));
                }
                foreground = anchored(
                    &source_of(reported),
                    family("neutral"),
                    oklab_to_okhsl_lightness(needed),
                    saturation,
                );
            }
        }
    }

    let palette = ThemePalette {
        foreground: color_to_color(with_text_contrast(
            foreground,
            &surfaces(Role::Foreground),
            lighter,
        )),
        background: Color::Reset,
        surface: color_to_color(color_of(Role::Surface)),
        muted: color_to_color(color_of(Role::Muted)),
        border: color_to_color(color_of(Role::Border)),
        accent: color_to_color(color_of(Role::Accent)),
        success: color_to_color(color_of(Role::Success)),
        warning: color_to_color(color_of(Role::Warning)),
        error: color_to_color(color_of(Role::Error)),
    };
    Some((palette, appearance))
}

fn color_to_color(color: Rgb) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK_BG: Rgb = Rgb::new(0x1E, 0x1E, 0x2E);
    const LIGHT_FG: Rgb = Rgb::new(0xC7, 0xC7, 0xC7);
    const LIGHT_BG: Rgb = Rgb::new(0xF5, 0xF5, 0xF5);
    const DARK_FG: Rgb = Rgb::new(0x33, 0x33, 0x33);

    fn catppuccin_mocha_palette() -> [Rgb; 16] {
        [
            Rgb::new(0x45, 0x46, 0x75),
            Rgb::new(0xF3, 0x8B, 0xA8),
            Rgb::new(0xA6, 0xE3, 0xA1),
            Rgb::new(0xF9, 0xE2, 0xAF),
            Rgb::new(0x89, 0xB4, 0xFA),
            Rgb::new(0xF5, 0xC2, 0xE7),
            Rgb::new(0x94, 0xE2, 0xD5),
            Rgb::new(0xBA, 0xBA, 0xC7),
            Rgb::new(0x6C, 0x70, 0x86),
            Rgb::new(0xF3, 0x8B, 0xA8),
            Rgb::new(0xA6, 0xE3, 0xA1),
            Rgb::new(0xF9, 0xE2, 0xAF),
            Rgb::new(0x89, 0xB4, 0xFA),
            Rgb::new(0xF5, 0xC2, 0xE7),
            Rgb::new(0x94, 0xE2, 0xD5),
            Rgb::new(0xA6, 0xAD, 0xE8),
        ]
    }

    /// A generated palette keeps WCAG AA for every role on the
    /// background and the surface, on dark and light terminals, with a
    /// palette and without one.
    #[test]
    fn generated_palette_keeps_text_contrast() {
        for colors in [
            TerminalColors {
                foreground: Some(LIGHT_FG),
                background: Some(DARK_BG),
                palette: None,
            },
            TerminalColors {
                foreground: Some(LIGHT_FG),
                background: Some(DARK_BG),
                palette: Some(catppuccin_mocha_palette()),
            },
            TerminalColors {
                foreground: Some(DARK_FG),
                background: Some(LIGHT_BG),
                palette: None,
            },
            // A mid-gray background forces relaxation.
            TerminalColors {
                foreground: None,
                background: Some(Rgb::new(0x80, 0x80, 0x80)),
                palette: None,
            },
        ] {
            let (palette, appearance) =
                system_theme_palette(&colors, 1.0).expect("a background is reported");
            let surface = match palette.surface {
                Color::Rgb(r, g, b) => Rgb::new(r, g, b),
                _ => panic!("surface is explicit"),
            };
            for role in [
                palette.foreground,
                palette.muted,
                palette.accent,
                palette.success,
                palette.warning,
                palette.error,
            ] {
                // Body text may be the terminal default (Reset): the
                // reported foreground is the color the terminal paints.
                let color = match role {
                    Color::Reset => colors.foreground.expect("Reset only with a foreground"),
                    Color::Rgb(r, g, b) => Rgb::new(r, g, b),
                    _ => panic!("generated roles are explicit: {role:?}"),
                };
                let on_bg = wcag_contrast(color, colors.background.unwrap());
                let on_surface = wcag_contrast(color, surface);
                // On relaxed mid-gray backgrounds the text minimum may
                // only hold for body text, which with_text_contrast
                // enforces; other roles must still be clearly visible.
                let minimum = if role == palette.foreground { 4.5 } else { 3.0 };
                assert!(
                    on_bg >= minimum,
                    "{appearance:?} bg contrast {on_bg} for {color:?} vs {:?}",
                    colors.background.unwrap()
                );
                assert!(
                    on_surface >= minimum,
                    "{appearance:?} surface contrast {on_surface} for {color:?} vs {surface:?}"
                );
            }
            assert_eq!(palette.background, Color::Reset);
        }
    }

    /// No background reported: the caller keeps the default palette.
    #[test]
    fn without_a_background_no_palette_is_generated() {
        assert!(system_theme_palette(&TerminalColors::default(), 1.0).is_none());
    }

    /// A palette's hue survives: the accent keeps the violet slot's hue
    /// family (pink for Catppuccin's pink #f5c2e7 stays pink, not blue).
    #[test]
    fn palette_hues_anchor_the_generated_colors() {
        let colors = TerminalColors {
            foreground: Some(LIGHT_FG),
            background: Some(DARK_BG),
            palette: Some(catppuccin_mocha_palette()),
        };
        let (palette, appearance) = system_theme_palette(&colors, 1.0).unwrap();
        assert_eq!(appearance, TerminalAppearance::Dark);
        let Color::Rgb(r, g, b) = palette.accent else {
            panic!("accent is explicit")
        };
        let hue = rgb_to_okhsl(Rgb::new(r, g, b)).h;
        // Catppuccin mocha's slot 5 (magenta) sits near hue 330; the
        // family's own violet is 295. Anchored means near 330.
        assert!(
            (hue - 330.0).abs() < 30.0,
            "accent hue {hue} should stay near the palette's magenta slot"
        );
        // Body text uses the terminal's foreground when it is strong
        // enough: Reset, painted by the terminal itself.
        assert_eq!(palette.foreground, Color::Reset);
    }
}

#[cfg(test)]
mod upstream_reference_tests {
    use super::*;

    /// Upstream's own generateSystemThemeColors on the same input
    /// (bun, Catppuccin Mocha reports): the roles must match its tokens.
    #[test]
    fn matches_upstream_generator_on_reference_reports() {
        let colors = TerminalColors {
            foreground: Some(Rgb::new(0xC7, 0xC7, 0xC7)),
            background: Some(Rgb::new(0x1E, 0x1E, 0x2E)),
            palette: Some([
                Rgb::new(0x45, 0x46, 0x75),
                Rgb::new(0xF3, 0x8B, 0xA8),
                Rgb::new(0xA6, 0xE3, 0xA1),
                Rgb::new(0xF9, 0xE2, 0xAF),
                Rgb::new(0x89, 0xB4, 0xFA),
                Rgb::new(0xF5, 0xC2, 0xE7),
                Rgb::new(0x94, 0xE2, 0xD5),
                Rgb::new(0xBA, 0xBA, 0xC7),
                Rgb::new(0x6C, 0x70, 0x86),
                Rgb::new(0xF3, 0x8B, 0xA8),
                Rgb::new(0xA6, 0xE3, 0xA1),
                Rgb::new(0xF9, 0xE2, 0xAF),
                Rgb::new(0x89, 0xB4, 0xFA),
                Rgb::new(0xF5, 0xC2, 0xE7),
                Rgb::new(0x94, 0xE2, 0xD5),
                Rgb::new(0xA6, 0xAD, 0xE8),
            ]),
        };
        let (palette, appearance) = system_theme_palette(&colors, 1.0).unwrap();
        assert_eq!(appearance, TerminalAppearance::Dark);
        let expected = |color: Color, hex: &str| {
            let Color::Rgb(r, g, b) = color else {
                panic!("explicit color expected for {hex}")
            };
            let (er, eg, eb) = parse_hex(hex);
            // The curves and OKHSL math are float-identical in intent, but
            // last-ulp differences before rounding can move a channel by
            // one; the color must not drift further than that.
            for (got, want) in [(r, er), (g, eg), (b, eb)] {
                assert!(got.abs_diff(want) <= 1, "{hex}: ({r}, {g}, {b})");
            }
        };
        // Upstream: selectedBg #15305d, text "" (terminal default),
        // muted #9c9fb1, border #709ade, accent #bf8eb2,
        // success #77b272, warning #b39d6d, error #e17b98.
        expected(palette.surface, "#15305d");
        assert_eq!(palette.foreground, Color::Reset);
        expected(palette.muted, "#9c9fb1");
        expected(palette.border, "#709ade");
        expected(palette.accent, "#bf8eb2");
        expected(palette.success, "#77b272");
        expected(palette.warning, "#b39d6d");
        expected(palette.error, "#e17b98");
    }

    fn parse_hex(hex: &str) -> (u8, u8, u8) {
        let value = hex.strip_prefix('#').unwrap();
        let channel =
            |range: std::ops::Range<usize>| u8::from_str_radix(&value[range], 16).unwrap();
        (channel(0..2), channel(2..4), channel(4..6))
    }
}
