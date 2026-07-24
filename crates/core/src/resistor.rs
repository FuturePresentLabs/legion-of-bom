//! Resistor value → color-code bands, for the Visual BOM sort sheet. 5uj.1.
//!
//! A pure function: a resistor's written value (`"4.7k"`, `"470R"`, `"2M2"`)
//! becomes the ordered color bands printed on a THT axial resistor, rendered as
//! a small inline pictogram so a builder sorting a bag of resistors can match by
//! eye. No network, no data dependency — this is the cheapest, most on-target
//! Visual-BOM win (DESIGN 7.6).
//!
//! Resistor marking conventions differ from SPICE ([`crate::units`]): here `M`
//! is **mega** and `R`/`k`/`M` may sit *in place of* the decimal point (`4k7` =
//! 4.7 kΩ, `R47` = 0.47 Ω) — so this carries its own parser rather than reusing
//! `parse_eng_value` (whose `M` is milli, for the RC-cutoff math).

/// One color band: a human name (for colorblind fallback / hover) + a hex fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Band {
    pub name: &'static str,
    pub hex: &'static str,
}

impl Band {
    /// The band fill as 0.0–1.0 RGB, for the PDF drawing path (which takes float
    /// components, not hex). Parses the leading `#rrggbb`.
    pub fn rgb(&self) -> (f64, f64, f64) {
        let h = self.hex.trim_start_matches('#');
        let c = |i: usize| {
            u8::from_str_radix(h.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0) as f64 / 255.0
        };
        (c(0), c(2), c(4))
    }
}

/// Digit colors 0–9 (index == digit). Used for the significant-figure bands and,
/// for exponents 0–9, the multiplier band.
const DIGITS: [Band; 10] = [
    Band {
        name: "black",
        hex: "#1a1a1a",
    },
    Band {
        name: "brown",
        hex: "#8b4a2b",
    },
    Band {
        name: "red",
        hex: "#d42323",
    },
    Band {
        name: "orange",
        hex: "#e8770c",
    },
    Band {
        name: "yellow",
        hex: "#f4c026",
    },
    Band {
        name: "green",
        hex: "#1f9e40",
    },
    Band {
        name: "blue",
        hex: "#2f5fd0",
    },
    Band {
        name: "violet",
        hex: "#8a3ff0",
    },
    Band {
        name: "grey",
        hex: "#8a8a8a",
    },
    Band {
        name: "white",
        hex: "#fafafa",
    },
];

const GOLD: Band = Band {
    name: "gold",
    hex: "#c9a227",
};
const SILVER: Band = Band {
    name: "silver",
    hex: "#c0c0c0",
};

/// The ten digit-band colors, index == digit — for rendering a color-code
/// legend / key that stays in sync with [`color_code`].
pub fn digit_colors() -> [Band; 10] {
    DIGITS
}

/// The multiplier band for a power-of-ten `exp` (10^exp), or `None` outside the
/// standard −2..=9 range (gold = ×0.1, silver = ×0.01).
fn multiplier_band(exp: i32) -> Option<Band> {
    match exp {
        -2 => Some(SILVER),
        -1 => Some(GOLD),
        0..=9 => Some(DIGITS[exp as usize]),
        _ => None,
    }
}

/// A resistor's decoded color code: the ordered bands as printed on the body
/// (significant figures, then multiplier, then tolerance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorCode {
    /// Ordered bands, left to right (2 or 3 sig-fig bands + multiplier + tolerance).
    pub bands: Vec<Band>,
    /// `true` for a 5-band (3 sig-fig) code, `false` for 4-band (2 sig-fig).
    pub five_band: bool,
}

/// Parse a resistor value written in marking convention into ohms.
///
/// Accepts a bare number (`"470"` → 470 Ω), an SI suffix (`"4.7k"`, `"1M"`,
/// `"2.2G"`; case-insensitive, `M`/`m` = mega), and EU embedded-multiplier
/// notation where the letter replaces the decimal point (`"4k7"` = 4.7 kΩ,
/// `"R47"` = 0.47 Ω, `"2M2"` = 2.2 MΩ, `"470R"` = 470 Ω). A trailing `Ω`/`ohm`
/// and surrounding whitespace are ignored. Returns `None` if unparseable.
pub fn parse_ohms(value: &str) -> Option<f64> {
    // Strip whitespace and any ohm unit text, keeping the number + multiplier letter.
    let mut s = value.trim();
    for suffix in ["ohms", "ohm", "Ω", "Ω"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped.trim();
            break;
        }
    }
    if s.is_empty() {
        return None;
    }

    // Find the multiplier letter (R/k/M/G, case-insensitive). It may sit between
    // digits (EU notation), so search the whole string, not just the suffix.
    let mult_pos = s
        .chars()
        .position(|c| matches!(c, 'R' | 'r' | 'k' | 'K' | 'M' | 'm' | 'G' | 'g'));

    let Some(pos) = mult_pos else {
        // No multiplier letter → a plain ohm value.
        return s.parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0);
    };

    let letter = s[pos..].chars().next()?;
    let factor = match letter.to_ascii_lowercase() {
        'r' => 1.0,
        'k' => 1e3,
        'm' => 1e6, // resistor convention: mega, NOT milli
        'g' => 1e9,
        _ => return None,
    };
    let (left, right) = (&s[..pos], &s[pos + letter.len_utf8()..]);
    // The letter stands in for the decimal point: "4k7" → "4.7", "R47" → "0.47",
    // "470R"/"1k" → "470"/"1". A left part may already carry its own '.'.
    let joined = if right.is_empty() {
        left.to_string()
    } else if left.is_empty() {
        format!("0.{right}")
    } else {
        format!("{left}.{right}")
    };
    let mantissa: f64 = joined.parse().ok()?;
    let ohms = mantissa * factor;
    (ohms.is_finite() && ohms > 0.0).then_some(ohms)
}

/// Decompose `ohms` into `k` significant digits and the power-of-ten multiplier,
/// normalising any rounding carry (e.g. 9.99 → "100").
fn digits_and_exp(ohms: f64, k: usize) -> (Vec<u8>, i32) {
    let mut exp = ohms.log10().floor() as i32 - (k as i32 - 1);
    let mut mant = (ohms / 10f64.powi(exp)).round() as i64;
    let upper = 10i64.pow(k as u32);
    if mant >= upper {
        mant /= 10;
        exp += 1;
    }
    let digits = (0..k)
        .rev()
        .map(|i| ((mant / 10i64.pow(i as u32)) % 10) as u8)
        .collect();
    (digits, exp)
}

/// Reconstruct the ohm value a `(digits, exp)` decomposition encodes.
fn recompose(digits: &[u8], exp: i32) -> f64 {
    let mant: i64 = digits.iter().fold(0i64, |acc, &d| acc * 10 + d as i64);
    mant as f64 * 10f64.powi(exp)
}

/// The color code for a written resistor value, or `None` if it doesn't parse as
/// a resistance or falls outside the standard band range.
///
/// Chooses a 4-band (2 sig-fig) code when it represents the value exactly, else a
/// 5-band (3 sig-fig) code — matching how E24 vs E96 values are actually printed.
/// The tolerance band defaults per code width (4-band → gold 5%, the usual carbon
/// film; 5-band → brown 1%, the usual metal film); a part-sourced tolerance can
/// override it later.
pub fn color_code(value: &str) -> Option<ColorCode> {
    let ohms = parse_ohms(value)?;

    // Prefer 2 sig figs; fall back to 3 when 2 can't represent the value exactly.
    let (digits, exp) = {
        let (d2, e2) = digits_and_exp(ohms, 2);
        if (recompose(&d2, e2) - ohms).abs() <= ohms * 1e-6 {
            (d2, e2)
        } else {
            digits_and_exp(ohms, 3)
        }
    };
    let five_band = digits.len() == 3;

    let mult = multiplier_band(exp)?;
    let tolerance = if five_band {
        DIGITS[1] /* brown 1% */
    } else {
        GOLD /* 5% */
    };

    let mut bands: Vec<Band> = digits.iter().map(|&d| DIGITS[d as usize]).collect();
    bands.push(mult);
    bands.push(tolerance);
    Some(ColorCode { bands, five_band })
}

impl ColorCode {
    /// The band names, left to right (`"yellow violet red gold"`) — the colorblind
    /// / hover fallback shown alongside the pictogram.
    pub fn band_names(&self) -> String {
        self.bands
            .iter()
            .map(|b| b.name)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A small inline SVG pictogram of the banded axial resistor, sized to sit in
    /// a table cell. Leads + a beige body with the color bands; a `<title>` gives
    /// the band names for hover / accessibility. `width`/`height` are in px.
    pub fn to_svg(&self, width: f64, height: f64) -> String {
        let body_x = width * 0.16;
        let body_w = width * 0.68;
        let body_y = height * 0.24;
        let body_h = height * 0.52;
        // Bands sit inside the body; the last (tolerance) band is pushed right
        // with a gap, as it is on a real resistor.
        let n = self.bands.len();
        let inner_x = body_x + body_w * 0.10;
        let inner_w = body_w * 0.80;
        let band_w = (inner_w / n as f64).min(width * 0.09);
        let mut s = format!(
            "<svg viewBox=\"0 0 {width:.0} {height:.0}\" width=\"{width:.0}\" height=\"{height:.0}\" \
             role=\"img\" xmlns=\"http://www.w3.org/2000/svg\" class=\"rband\">\
             <title>{}</title>\
             <line x1=\"0\" y1=\"{cy:.1}\" x2=\"{width:.0}\" y2=\"{cy:.1}\" stroke=\"#9a9a9a\" stroke-width=\"1\"/>\
             <rect x=\"{body_x:.1}\" y=\"{body_y:.1}\" width=\"{body_w:.1}\" height=\"{body_h:.1}\" \
             rx=\"{rx:.1}\" fill=\"#e8d3a0\" stroke=\"#b79a5e\" stroke-width=\"0.75\"/>",
            self.band_names(),
            cy = height / 2.0,
            rx = body_h * 0.35,
        );
        for (i, band) in self.bands.iter().enumerate() {
            // Even placement, then shove the tolerance band toward the body's end.
            let base = inner_x + (i as f64 + 0.5) * (inner_w / n as f64) - band_w / 2.0;
            let x = if i + 1 == n {
                body_x + body_w - band_w * 1.6
            } else {
                base
            };
            s.push_str(&format!(
                "<rect x=\"{x:.1}\" y=\"{body_y:.1}\" width=\"{band_w:.1}\" height=\"{body_h:.1}\" \
                 fill=\"{}\" stroke=\"#00000022\" stroke-width=\"0.3\"/>",
                band.hex,
            ));
        }
        s.push_str("</svg>");
        s
    }

    /// A **life-size** SVG of the banded axial resistor, sized in real millimetres
    /// (so a printed Component Sorting Sheet shows it at actual size at 100%): a
    /// ~6.3×2.4 mm 1/4 W body with the color bands, plus lead stubs so a builder
    /// can lay the real resistor straight onto it. Width via CSS `mm` units.
    pub fn to_svg_lifesize(&self) -> String {
        let (body_w, body_h, lead) = (6.3_f64, 2.4_f64, 8.0_f64); // mm (1/4 W axial)
        let total_w = body_w + 2.0 * lead;
        let total_h = 5.0_f64;
        let (cy, body_x, body_y) = (total_h / 2.0, lead, total_h / 2.0 - body_h / 2.0);
        let n = self.bands.len();
        let band_w = 0.6_f64;
        let mut s = format!(
            "<svg class=\"rband-life\" width=\"{total_w:.2}mm\" height=\"{total_h:.2}mm\" \
             viewBox=\"0 0 {total_w:.2} {total_h:.2}\" role=\"img\" \
             xmlns=\"http://www.w3.org/2000/svg\"><title>{}</title>\
             <line x1=\"0\" y1=\"{cy:.2}\" x2=\"{total_w:.2}\" y2=\"{cy:.2}\" stroke=\"#9a9a9a\" \
             stroke-width=\"0.5\"/>\
             <rect x=\"{body_x:.2}\" y=\"{body_y:.2}\" width=\"{body_w:.2}\" height=\"{body_h:.2}\" \
             rx=\"1\" fill=\"#e8d3a0\" stroke=\"#b79a5e\" stroke-width=\"0.15\"/>",
            self.band_names(),
        );
        // Significant + multiplier bands clustered on the left; the tolerance band
        // sits alone near the right end, as on a real resistor.
        let (start, gap) = (body_x + 0.7, 0.85);
        for (i, band) in self.bands.iter().enumerate() {
            let x = if i + 1 == n {
                body_x + body_w - 1.2
            } else {
                start + i as f64 * gap
            };
            s.push_str(&format!(
                "<rect x=\"{x:.2}\" y=\"{body_y:.2}\" width=\"{band_w:.2}\" height=\"{body_h:.2}\" \
                 fill=\"{}\"/>",
                band.hex,
            ));
        }
        s.push_str("</svg>");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() <= b.abs() * 1e-6 + 1e-12
    }

    #[test]
    fn parses_marking_conventions() {
        assert!(approx(parse_ohms("470").unwrap(), 470.0));
        assert!(approx(parse_ohms("470R").unwrap(), 470.0));
        assert!(approx(parse_ohms("1k").unwrap(), 1_000.0));
        assert!(approx(parse_ohms("4.7k").unwrap(), 4_700.0));
        assert!(approx(parse_ohms("4k7").unwrap(), 4_700.0));
        assert!(approx(parse_ohms("R47").unwrap(), 0.47));
        assert!(approx(parse_ohms("4R7").unwrap(), 4.7));
        assert!(approx(parse_ohms("2M2").unwrap(), 2_200_000.0)); // mega, not milli
        assert!(approx(parse_ohms("1M").unwrap(), 1_000_000.0));
        assert!(approx(parse_ohms("10k ohm").unwrap(), 10_000.0));
        assert!(approx(parse_ohms("100Ω").unwrap(), 100.0));
        assert_eq!(parse_ohms(""), None);
        assert_eq!(parse_ohms("TL072"), None);
        assert_eq!(parse_ohms("100n"), None); // a capacitor value, not a resistor
    }

    fn names(v: &str) -> String {
        color_code(v).unwrap().band_names()
    }

    #[test]
    fn four_band_codes_match_reference() {
        // 1k = brown black red, 5% → gold.
        assert_eq!(names("1k"), "brown black red gold");
        // 4.7k = yellow violet red.
        assert_eq!(names("4.7k"), "yellow violet red gold");
        // 470Ω = yellow violet brown.
        assert_eq!(names("470"), "yellow violet brown gold");
        // 220Ω = red red brown.
        assert_eq!(names("220"), "red red brown gold");
        // 10Ω = brown black black.
        assert_eq!(names("10"), "brown black black gold");
        // 1MΩ = brown black green.
        assert_eq!(names("1M"), "brown black green gold");
    }

    #[test]
    fn gold_and_silver_multipliers() {
        // 4.7Ω = yellow violet gold(×0.1).
        assert_eq!(names("4R7"), "yellow violet gold gold");
        // 0.47Ω = yellow violet silver(×0.01).
        assert_eq!(names("R47"), "yellow violet silver gold");
    }

    #[test]
    fn three_sig_fig_values_use_five_bands() {
        // 4.99k (E96) needs 3 sig figs: yellow white white + brown(×10) + brown(1%).
        let cc = color_code("4k99").unwrap();
        assert!(cc.five_band);
        assert_eq!(cc.band_names(), "yellow white white brown brown");
        // 2-sig-fig values stay 4-band.
        assert!(!color_code("4.7k").unwrap().five_band);
    }

    #[test]
    fn svg_is_self_contained_and_labelled() {
        let svg = color_code("4.7k").unwrap().to_svg(72.0, 22.0);
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("<title>yellow violet red gold</title>"));
        // One rect per band + the body rect.
        assert_eq!(svg.matches("<rect").count(), 4 + 1);
    }

    #[test]
    fn rejects_non_resistor_values() {
        assert_eq!(color_code("100n"), None);
        assert_eq!(color_code("TL072"), None);
        assert_eq!(color_code(""), None);
    }
}
