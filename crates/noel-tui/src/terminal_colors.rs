//! Parsing of the terminal's color reports, as upstream pi's
//! `terminal-colors.ts` parses them: OSC 10/11/4 replies, the DA1 reply
//! that ends them, the mode-2031 color-scheme report, and the
//! `COLORFGBG` variable's appearance classification.

use crate::oklab::{oklab_lightness, wcag_contrast, Rgb};

/// Colors the terminal reports for its current theme.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalColors {
    /// Default foreground (OSC 10).
    pub foreground: Option<Rgb>,
    /// Default background (OSC 11).
    pub background: Option<Rgb>,
    /// ANSI colors 0-15 (OSC 4). Only set when the terminal reported all 16.
    pub palette: Option<[Rgb; 16]>,
}

/// A terminal's appearance: dark or light.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalAppearance {
    Dark,
    Light,
}

impl TerminalAppearance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

/// What an OSC color reply reports: the default foreground (10), the
/// default background (11), or a palette index (4;N).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscColorTarget {
    Foreground,
    Background,
    Palette(u8),
}

/// One reply chunk a terminal color query collected: an OSC color reply
/// (which may carry an unparseable color), the DA1 reply that ends the
/// batch, or something else, which the query ignores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalReply {
    Color(OscColorTarget, Option<Rgb>),
    DeviceAttributes,
    Other,
}

fn hex_channel(channel: &str) -> Option<u8> {
    if channel.is_empty() || !channel.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16u32.pow(channel.len() as u32) - 1;
    if max == 0 {
        return None;
    }
    let value = u32::from_str_radix(channel, 16).ok()?;
    Some((value as f64 / max as f64 * 255.0).round() as u8)
}

fn parse_osc_color_value(raw: &str) -> Option<Rgb> {
    let value = raw.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
            return Some(Rgb::new(channel(0..2)?, channel(2..4)?, channel(4..6)?));
        }
        if hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(Rgb::new(
                hex_channel(&hex[0..4])?,
                hex_channel(&hex[4..8])?,
                hex_channel(&hex[8..12])?,
            ));
        }
        return None;
    }

    let rgb_value = value
        .strip_prefix("rgb:")
        .or_else(|| value.strip_prefix("rgba:"))
        .unwrap_or(value);
    let mut parts = rgb_value.split('/');
    // Upstream destructures the first three channels and ignores any
    // fourth (rgba: alpha).
    let (r, g, b) = (parts.next()?, parts.next()?, parts.next()?);
    Some(Rgb::new(hex_channel(r)?, hex_channel(g)?, hex_channel(b)?))
}

/// Classify one escape-sequence reply from a terminal color query.
///
/// Recognizes the replies upstream's `TERMINAL_COLOR_QUERY` produces:
/// OSC 10/11/4 color replies (BEL- or ST-terminated), and the DA1 reply
/// (`CSI ? ... c`). Anything else is [`TerminalReply::Other`].
pub fn parse_terminal_reply(data: &str) -> TerminalReply {
    let device_attributes = data.starts_with("\x1b[?") && data.ends_with('c');
    if device_attributes
        && data[3..data.len() - 1]
            .chars()
            .all(|c| c.is_ascii_digit() || c == ';')
    {
        return TerminalReply::DeviceAttributes;
    }
    let Some(rest) = data
        .strip_prefix("\x1b]")
        .and_then(|rest| {
            rest.strip_suffix('\x07')
                .or_else(|| rest.strip_suffix("\x1b\\"))
        })
        .map(str::trim)
    else {
        return TerminalReply::Other;
    };
    let (target, value) = if let Some(value) = rest.strip_prefix("10;") {
        (OscColorTarget::Foreground, value)
    } else if let Some(value) = rest.strip_prefix("11;") {
        (OscColorTarget::Background, value)
    } else if let Some(value) = rest.strip_prefix("4;") {
        let mut parts = value.splitn(2, ';');
        let (index, value) = match (parts.next(), parts.next()) {
            (Some(index), Some(value)) => (index, value),
            _ => return TerminalReply::Other,
        };
        let Ok(index) = index.parse::<u8>() else {
            return TerminalReply::Other;
        };
        (OscColorTarget::Palette(index), value)
    } else {
        return TerminalReply::Other;
    };
    TerminalReply::Color(target, parse_osc_color_value(value))
}

/// Parse the mode-2031 color-scheme report (`CSI ? 997 ; 1|2 n`), one or
/// more of them; the last decides.
pub fn parse_color_scheme_report(data: &str) -> Option<TerminalAppearance> {
    let dark = "\x1b[?997;1n";
    let light = "\x1b[?997;2n";
    let reports = data.split_inclusive('n');
    if reports.clone().count() == 0 {
        return None;
    }
    let mut last = None;
    for part in reports {
        if part == dark {
            last = Some(TerminalAppearance::Dark);
        } else if part == light {
            last = Some(TerminalAppearance::Light);
        } else {
            return None;
        }
    }
    last
}

/// Dark or light from the `COLORFGBG` variable some terminals set:
/// `fg;bg` or `fg;xpm;bg`, where a field is an ANSI color index or
/// `default`. The index refers to the terminal's own palette, whose
/// colors are unknown here, so it is classified by index like Vim does:
/// 0-6 and 8 (bright black, e.g. Solarized Dark's background) are dark,
/// 7 and 9-15 are light.
pub fn detect_colorfgbg_appearance(value: &str) -> Option<TerminalAppearance> {
    let bg = value.split(';').next_back()?.trim();
    if bg.is_empty() || !bg.chars().all(|c| c.is_ascii_digit()) || bg.len() > 2 {
        return None;
    }
    let index: u8 = bg.parse().ok()?;
    if index > 15 {
        return None;
    }
    Some(if index <= 6 || index == 8 {
        TerminalAppearance::Dark
    } else {
        TerminalAppearance::Light
    })
}

/// WCAG 2 contrast that body text must reach on the surfaces it is on.
const TEXT_MINIMUM_WCAG_CONTRAST: f64 = 4.5;

/// Whether a terminal is dark or light, from its reported colors: the
/// direction of its own foreground when text can be readable that way,
/// otherwise dark when white text has more contrast on the background
/// than black text. As upstream's `terminalAppearance`.
pub fn terminal_appearance(background: Rgb, foreground: Option<Rgb>) -> TerminalAppearance {
    let white = Rgb::new(255, 255, 255);
    let black = Rgb::new(0, 0, 0);
    let white_contrast = wcag_contrast(white, background);
    let black_contrast = wcag_contrast(black, background);
    if let Some(foreground) = foreground {
        let foreground_l = oklab_lightness(foreground);
        let background_l = oklab_lightness(background);
        if (foreground_l - background_l).abs() > 0.05 {
            let appearance = if foreground_l > background_l {
                TerminalAppearance::Dark
            } else {
                TerminalAppearance::Light
            };
            let best = if appearance == TerminalAppearance::Dark {
                white_contrast
            } else {
                black_contrast
            };
            if best >= TEXT_MINIMUM_WCAG_CONTRAST {
                return appearance;
            }
        }
    }
    if white_contrast >= black_contrast {
        TerminalAppearance::Dark
    } else {
        TerminalAppearance::Light
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc_color_replies() {
        // BEL and ST terminators, #rrggbb, 16-bit-per-channel rgb:, and
        // rgba: forms; a garbage color is a reply with no color.
        for (data, target, color) in [
            (
                "\x1b]10;#c7c7c7\x07",
                OscColorTarget::Foreground,
                Some(Rgb::new(0xC7, 0xC7, 0xC7)),
            ),
            (
                "\x1b]11;#1e1e2e\x1b\\",
                OscColorTarget::Background,
                Some(Rgb::new(0x1E, 0x1E, 0x2E)),
            ),
            (
                "\x1b]4;5;rgb:cdcd/dcdc/eeee\x07",
                OscColorTarget::Palette(5),
                Some(Rgb::new(0xCD, 0xDC, 0xEE)),
            ),
            (
                "\x1b]4;0;rgba:0/0/0/ffff\x07",
                OscColorTarget::Palette(0),
                Some(Rgb::new(0, 0, 0)),
            ),
            ("\x1b]11;bogus\x07", OscColorTarget::Background, None),
        ] {
            assert_eq!(
                parse_terminal_reply(data),
                TerminalReply::Color(target, color)
            );
        }
        assert_eq!(
            parse_terminal_reply("\x1b[?1;2c"),
            TerminalReply::DeviceAttributes
        );
        assert_eq!(
            parse_terminal_reply("\x1b[?62;22c"),
            TerminalReply::DeviceAttributes
        );
        assert_eq!(parse_terminal_reply("\x1b[A"), TerminalReply::Other);
        assert_eq!(parse_terminal_reply("\x1b]12;x\x07"), TerminalReply::Other);
    }

    #[test]
    fn parses_color_scheme_reports_and_colorfgbg() {
        assert_eq!(
            parse_color_scheme_report("\x1b[?997;1n"),
            Some(TerminalAppearance::Dark)
        );
        assert_eq!(
            parse_color_scheme_report("\x1b[?997;2n"),
            Some(TerminalAppearance::Light)
        );
        assert_eq!(
            parse_color_scheme_report("\x1b[?997;1n\x1b[?997;2n"),
            Some(TerminalAppearance::Light)
        );
        assert_eq!(parse_color_scheme_report("\x1b[?997;3n"), None);
        assert_eq!(parse_color_scheme_report(""), None);

        assert_eq!(
            detect_colorfgbg_appearance("15;0"),
            Some(TerminalAppearance::Dark)
        );
        // rxvt's fg;xpm;bg form; bright black is dark (Solarized Dark).
        assert_eq!(
            detect_colorfgbg_appearance("0;15;8"),
            Some(TerminalAppearance::Dark)
        );
        assert_eq!(
            detect_colorfgbg_appearance("0;15"),
            Some(TerminalAppearance::Light)
        );
        assert_eq!(detect_colorfgbg_appearance("0;default"), None);
        assert_eq!(detect_colorfgbg_appearance("0;256"), None);
    }

    #[test]
    fn classifies_terminal_appearance_from_reported_colors() {
        let dark_bg = Rgb::new(0x1E, 0x1E, 0x2E);
        let light_fg = Rgb::new(0xC7, 0xC7, 0xC7);
        assert_eq!(
            terminal_appearance(dark_bg, Some(light_fg)),
            TerminalAppearance::Dark
        );
        let light_bg = Rgb::new(0xFF, 0xFF, 0xFF);
        let dark_fg = Rgb::new(0x33, 0x33, 0x33);
        assert_eq!(
            terminal_appearance(light_bg, Some(dark_fg)),
            TerminalAppearance::Light
        );
        // Without a foreground: whichever of white/black text contrasts more.
        assert_eq!(terminal_appearance(dark_bg, None), TerminalAppearance::Dark);
        assert_eq!(
            terminal_appearance(light_bg, None),
            TerminalAppearance::Light
        );
        // A foreground barely off the background falls back to contrast.
        assert_eq!(
            terminal_appearance(dark_bg, Some(Rgb::new(0x28, 0x28, 0x32))),
            TerminalAppearance::Dark
        );
    }
}
