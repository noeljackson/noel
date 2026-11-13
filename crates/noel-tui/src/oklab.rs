//! Oklab and OKHSL <-> sRGB conversion.
//!
//! A port of Björn Ottosson's reference implementation
//! (https://bottosson.github.io/posts/colorpicker/), Copyright (c) 2021
//! Björn Ottosson, used under the MIT license, as upstream pi ports it
//! (`packages/tui/src/oklab.ts`). OKHSL's saturation is relative to the
//! sRGB gamut at each hue and lightness.

/// An sRGB color, channels 0-255.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

type Vector = [f64; 3];
type Matrix = [Vector; 3];

fn multiply(matrix: Matrix, vector: Vector) -> Vector {
    [
        matrix[0][0] * vector[0] + matrix[0][1] * vector[1] + matrix[0][2] * vector[2],
        matrix[1][0] * vector[0] + matrix[1][1] * vector[1] + matrix[1][2] * vector[2],
        matrix[2][0] * vector[0] + matrix[2][1] * vector[1] + matrix[2][2] * vector[2],
    ]
}

const LINEAR_SRGB_TO_LMS: Matrix = [
    [0.412_221_469_470_762_9, 0.536_332_537_261_734_9, 0.051_445_992_673_502_2],
    [0.211_903_495_817_825_1, 0.680_699_550_645_234_4, 0.107_396_953_536_940_5],
    [0.088_302_459_190_056_4, 0.281_718_839_136_121_5, 0.629_978_701_673_822_2],
];
const LMS_TO_LAB: Matrix = [
    [0.210_454_268_309_314, 0.793_617_774_702_305, -0.004_072_043_011_619_3],
    [1.977_998_532_431_168_4, -2.428_592_242_048_58, 0.450_593_709_617_411],
    [0.025_904_042_465_547_8, 0.782_771_712_457_529_6, -0.808_675_754_923_077_4],
];
const LAB_TO_LMS: Matrix = [
    [1.0, 0.396_337_777_376_174_9, 0.215_803_757_309_913_6],
    [1.0, -0.105_561_345_815_658_6, -0.063_854_172_825_813_3],
    [1.0, -0.089_484_177_529_811_9, -1.291_485_548_019_409_2],
];
const LMS_TO_LINEAR_SRGB: Matrix = [
    [4.076_741_636_075_958_3, -3.307_711_539_258_062_9, 0.230_969_903_182_104_3],
    [-1.268_437_973_285_031_5, 2.609_757_349_287_688_2, -0.341_319_376_002_657],
    [-0.004_196_076_138_675_6, -0.703_418_617_935_936_2, 1.707_614_694_074_611_7],
];

/// Per sRGB channel (red, green, blue): the (a, b) half-plane where that
/// channel clips first, and the polynomial approximating the maximum
/// saturation there.
const SATURATION_FIT: [([f64; 2], [f64; 5]); 3] = [
    (
        [-1.881_703_1, -0.809_365_01],
        [1.190_862_77, 1.765_767_28, 0.596_626_41, 0.755_151_97, 0.567_712_45],
    ),
    (
        [1.814_440_8, -1.194_452_67],
        [0.739_565_15, -0.459_544_04, 0.082_854_27, 0.125_410_73, -0.145_032_04],
    ),
    (
        [0.131_107_58, 1.813_339_71],
        [1.357_336_52, -0.009_157_99, -1.151_302_1, -0.505_596_06, 0.006_921_67],
    ),
];

const K1: f64 = 0.206;
const K2: f64 = 0.03;
const K3: f64 = (1.0 + K1) / (1.0 + K2);

/// Oklab lightness to OKHSL lightness.
pub fn oklab_to_okhsl_lightness(x: f64) -> f64 {
    0.5 * (K3 * x - K1 + ((K3 * x - K1).powi(2) + 4.0 * K2 * K3 * x).sqrt())
}

/// OKHSL lightness to Oklab lightness.
fn okhsl_to_oklab_lightness(x: f64) -> f64 {
    (x * x + K1 * x) / (K3 * (x + K2))
}

/// sRGB transfer function: linear to encoded channel, both 0-1.
fn linear_to_srgb(value: f64) -> f64 {
    if value > 0.003_130_8 {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    } else {
        12.92 * value
    }
}

/// Inverse sRGB transfer function: encoded to linear channel, both 0-1.
fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Oklab [L, a, b] to linear sRGB [r, g, b] (0-1, may leave the gamut).
pub fn oklab_to_linear_srgb(lab: Vector) -> Vector {
    let lms = multiply(LAB_TO_LMS, lab).map(|value| value.powi(3));
    multiply(LMS_TO_LINEAR_SRGB, lms)
}

/// Linear sRGB [r, g, b] (0-1) to Oklab [L, a, b].
fn linear_srgb_to_oklab(rgb: Vector) -> Vector {
    let lms = multiply(LINEAR_SRGB_TO_LMS, rgb).map(|value| value.cbrt());
    multiply(LMS_TO_LAB, lms)
}

/// sRGB channels (0-255) to Oklab [L, a, b].
pub fn rgb_to_oklab(color: Rgb) -> Vector {
    linear_srgb_to_oklab([
        srgb_to_linear(f64::from(color.r) / 255.0),
        srgb_to_linear(f64::from(color.g) / 255.0),
        srgb_to_linear(f64::from(color.b) / 255.0),
    ])
}

/// Linear sRGB [r, g, b] (0-1) to sRGB channels (0-255, rounded), clipping
/// out-of-gamut channels.
fn linear_srgb_to_rgb(linear: Vector) -> Rgb {
    let clamp_channel = |value: f64| (linear_to_srgb(value.clamp(0.0, 1.0)) * 255.0).round() as u8;
    Rgb::new(
        clamp_channel(linear[0]),
        clamp_channel(linear[1]),
        clamp_channel(linear[2]),
    )
}

/// Rate of change of each cube-root LMS component along a chroma direction (a, b).
fn lms_slopes(a: f64, b: f64) -> Vector {
    [
        LAB_TO_LMS[0][1] * a + LAB_TO_LMS[0][2] * b,
        LAB_TO_LMS[1][1] * a + LAB_TO_LMS[1][2] * b,
        LAB_TO_LMS[2][1] * a + LAB_TO_LMS[2][2] * b,
    ]
}

/// Largest saturation (C/L) inside sRGB for hue (a, b): polynomial fit plus one Halley step.
fn max_saturation(a: f64, b: f64) -> f64 {
    let channel = SATURATION_FIT
        .iter()
        .enumerate()
        .find_map(|(index, ([x, y], _))| (index == 2 || x * a + y * b >= 1.0).then_some(index))
        .expect("the third channel always matches");
    let [k0, k1, k2, k3, k4] = SATURATION_FIT[channel].1;
    let weights = LMS_TO_LINEAR_SRGB[channel];
    let saturation = k0 + k1 * a + k2 * b + k3 * a * a + k4 * a * b;

    let slopes = lms_slopes(a, b);
    let base = slopes.map(|k| 1.0 + saturation * k);
    let dot = |values: Vector| {
        weights[0] * values[0] + weights[1] * values[1] + weights[2] * values[2]
    };
    let f = dot(base.map(|value| value.powi(3)));
    let f1 = dot([
        3.0 * slopes[0] * base[0].powi(2),
        3.0 * slopes[1] * base[1].powi(2),
        3.0 * slopes[2] * base[2].powi(2),
    ]);
    let f2 = dot([
        6.0 * slopes[0].powi(2) * base[0],
        6.0 * slopes[1].powi(2) * base[1],
        6.0 * slopes[2].powi(2) * base[2],
    ]);
    saturation - (f * f1) / (f1 * f1 - 0.5 * f * f2)
}

/// Oklab lightness and chroma of the most saturated sRGB color of hue (a, b).
fn cusp(a: f64, b: f64) -> (f64, f64) {
    let saturation = max_saturation(a, b);
    let linear = oklab_to_linear_srgb([1.0, saturation * a, saturation * b]);
    let lightness = (1.0 / linear.iter().copied().fold(f64::MIN, f64::max)).cbrt();
    (lightness, lightness * saturation)
}

/// Chroma where the constant-lightness line at `lightness` leaves the sRGB gamut.
fn max_chroma(a: f64, b: f64, lightness: f64, (cusp_l, cusp_c): (f64, f64)) -> f64 {
    if lightness <= cusp_l {
        return (cusp_c * lightness) / cusp_l;
    }
    // Upper half: triangle edge, then one Halley step against each channel reaching 1.
    let t = (cusp_c * (lightness - 1.0)) / (cusp_l - 1.0);
    let slopes = lms_slopes(a, b);
    let lms = slopes.map(|k| lightness + t * k);
    let cubes = lms.map(|value| value.powi(3));
    let first = [
        3.0 * slopes[0] * lms[0].powi(2),
        3.0 * slopes[1] * lms[1].powi(2),
        3.0 * slopes[2] * lms[2].powi(2),
    ];
    let second = [
        6.0 * slopes[0].powi(2) * lms[0],
        6.0 * slopes[1].powi(2) * lms[1],
        6.0 * slopes[2].powi(2) * lms[2],
    ];
    let steps = LMS_TO_LINEAR_SRGB.map(|row| {
        let dot = |values: Vector| row[0] * values[0] + row[1] * values[1] + row[2] * values[2];
        let f = dot(cubes) - 1.0;
        let f1 = dot(first);
        let f2 = dot(second);
        let u = f1 / (f1 * f1 - 0.5 * f * f2);
        if u >= 0.0 { -f * u } else { f64::MAX }
    });
    t + steps.iter().copied().fold(f64::MAX, f64::min)
}

/// OKHSL's chroma reference points at lightness L and hue (a, b): [c0, cMid, cMax].
fn chroma_stops(l: f64, a: f64, b: f64) -> (f64, f64, f64) {
    let peak = cusp(a, b);
    let c_max = max_chroma(a, b, l, peak);
    let k = c_max
        / (l * (peak.1 / peak.0))
        .min((1.0 - l) * (peak.1 / (1.0 - peak.0)))
        .max(f64::MIN_POSITIVE);
    let mid_s = 0.115_169_93
        + 1.0 / (7.447_789_7
            + 4.159_012_4 * b
            + a * (-2.195_573_47
                + 1.751_984_01 * b
                + a * (-2.137_049_48
                    - 10.023_010_43 * b
                    + a * (-4.248_945_61 + 5.387_708_19 * b + 4.698_910_13 * a))));
    let mid_t = 0.112_396_42
        + 1.0 / (1.613_203_2
            - 0.681_243_79 * b
            + a * (0.403_706_12
                + 0.901_481_23 * b
                + a * (-0.270_879_43
                    + 0.612_239_9 * b
                    + a * (0.002_992_15 - 0.453_995_68 * b - 0.146_618_72 * a))));
    let c_mid =
        0.9 * k * (1.0 / ((1.0 / (l * mid_s)).powi(4) + (1.0 / ((1.0 - l) * mid_t)).powi(4))).sqrt().sqrt();
    let c0 = (1.0 / (1.0 / (l * 0.4).powi(2) + 1.0 / ((1.0 - l) * 0.8).powi(2))).sqrt();
    (c0, c_mid, c_max)
}

/// Convert OKHSL to sRGB channels (0-255, rounded), clipping out-of-gamut channels.
pub fn okhsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> Rgb {
    let l = okhsl_to_oklab_lightness(lightness);
    let mut lab = [l, 0.0, 0.0];
    if l > 0.0 && l < 1.0 && saturation > 0.0 {
        let angle = (2.0 * std::f64::consts::PI * (((hue % 360.0) + 360.0) % 360.0)) / 360.0;
        let a = angle.cos();
        let b = angle.sin();
        let (c0, c_mid, c_max) = chroma_stops(l, a, b);
        // Chroma rises from 0 through cMid at s = 0.8 to cMax at s = 1.
        let chroma = if saturation < 0.8 {
            let t = 1.25 * saturation;
            let k1 = 0.8 * c0;
            (t * k1) / (1.0 - (1.0 - k1 / c_mid) * t)
        } else {
            let t = 5.0 * (saturation - 0.8);
            let k1 = (0.2 * c_mid.powi(2) * 1.25_f64.powi(2)) / c0;
            c_mid + (t * k1) / (1.0 - (1.0 - k1 / (c_max - c_mid)) * t)
        };
        lab = [l, chroma * a, chroma * b];
    }
    linear_srgb_to_rgb(oklab_to_linear_srgb(lab))
}

/// OKHSL channels of an sRGB color: hue in degrees (0 for grays), saturation and lightness 0-1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Okhsl {
    pub h: f64,
    pub s: f64,
    pub l: f64,
}

/// Convert sRGB channels (0-255) to OKHSL.
pub fn rgb_to_okhsl(color: Rgb) -> Okhsl {
    let [l, lab_a, lab_b] = rgb_to_oklab(color);
    let chroma = (lab_a * lab_a + lab_b * lab_b).sqrt();
    let lightness = oklab_to_okhsl_lightness(l);
    if chroma < 1e-9 || lightness <= 0.0 || lightness >= 1.0 {
        return Okhsl {
            h: 0.0,
            s: 0.0,
            l: lightness,
        };
    }

    let hue = ((lab_b.atan2(lab_a).to_degrees()) + 360.0) % 360.0;
    let (c0, c_mid, c_max) = chroma_stops(l, lab_a / chroma, lab_b / chroma);
    let saturation = if chroma < c_mid {
        let k1 = 0.8 * c0;
        0.8 * (chroma / (k1 + (1.0 - k1 / c_mid) * chroma))
    } else {
        let k1 = (0.2 * c_mid.powi(2) * 1.25_f64.powi(2)) / c0;
        let offset = chroma - c_mid;
        0.8 + 0.2 * (offset / (k1 + (1.0 - k1 / (c_max - c_mid)) * offset))
    };
    Okhsl {
        h: hue,
        s: saturation.clamp(0.0, 1.0),
        l: lightness,
    }
}

/// OKLCH chroma of an sRGB color.
pub(crate) fn rgb_chroma(color: Rgb) -> f64 {
    let [_, a, b] = rgb_to_oklab(color);
    (a * a + b * b).sqrt()
}

/// OKLab lightness of an sRGB color, 0-1.
pub fn oklab_lightness(color: Rgb) -> f64 {
    rgb_to_oklab(color)[0]
}

/// WCAG 2 relative luminance of an sRGB color.
pub fn relative_luminance(color: Rgb) -> f64 {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

/// WCAG 2 contrast ratio of two sRGB colors, 1-21.
pub fn wcag_contrast(first: Rgb, second: Rgb) -> f64 {
    let a = relative_luminance(first);
    let b = relative_luminance(second);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values computed with upstream's own oklab.ts through bun.
    #[test]
    fn okhsl_conversions_match_upstream() {
        for (h, s, l, expected) in [
            (234.0, 0.03, 0.89, Rgb::new(0xDE, 0xE0, 0xE1)),
            (229.0, 0.06, 0.67, Rgb::new(0x9D, 0xA5, 0xA9)),
            (295.0, 0.50, 0.67, Rgb::new(0xA7, 0x98, 0xD7)),
            (232.0, 0.54, 0.67, Rgb::new(0x69, 0xAD, 0xD0)),
            (159.0, 0.59, 0.67, Rgb::new(0x68, 0xB7, 0x8D)),
            (20.0, 0.72, 0.67, Rgb::new(0xEA, 0x7F, 0x81)),
            (83.0, 0.88, 0.67, Rgb::new(0xCD, 0x9A, 0x22)),
            (233.0, 0.41, 0.24, Rgb::new(0x21, 0x3B, 0x49)),
            (225.0, 0.05, 0.27, Rgb::new(0x3B, 0x3F, 0x41)),
            (295.0, 0.60, 0.46, Rgb::new(0x74, 0x59, 0xB4)),
            (231.0, 0.68, 0.47, Rgb::new(0x2F, 0x78, 0x99)),
            (159.0, 0.75, 0.46, Rgb::new(0x33, 0x7E, 0x58)),
            (20.0, 0.91, 0.47, Rgb::new(0xC8, 0x25, 0x3D)),
            (83.0, 0.99, 0.47, Rgb::new(0x8F, 0x68, 0x02)),
            (0.0, 0.0, 0.5, Rgb::new(0x77, 0x77, 0x77)),
        ] {
            let rgb = okhsl_to_rgb(h, s, l);
            assert_eq!(rgb, expected, "okhsl({h}, {s}, {l})");
            // Round trips within OKHSL precision: near-grays carry almost no
            // hue information, so their recovered hue may drift far while the
            // color stays visually identical (upstream drifts the same way:
            // okhsl(234, 3%, 89%) recovers h = 228.8).
            let back = rgb_to_okhsl(rgb);
            if s >= 0.4 {
                assert!((back.h - h).abs() < 1.0, "hue {h} -> {} -> {}", rgb.r, back.h);
            }
            assert!((back.s - s).abs() < 2e-2, "sat {s} -> {}", back.s);
            assert!((back.l - l).abs() < 5e-3, "light {l} -> {}", back.l);
        }
    }
}
