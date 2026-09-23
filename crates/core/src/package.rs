//! Reading a KiCad footprint id as a *package* — the short name a builder says out
//! loud ("0603", "DIP-16", "TO-92") and the body size they can measure against a
//! ruler. Shared by the two printed documents: the build guide's per-step parts
//! table and the Visual BOM's sorting cells.
//!
//! Package is the column a builder actually sorts by. Two 47k resistors with the
//! same value are *not* interchangeable if one is 0603 and the other 1206, and the
//! footprint id is the only place that fact lives — but `Resistor_SMD:R_0603_1608\
//! Metric` is not something you scan a table with. Everything here is best-effort
//! pattern reading over KiCad's naming conventions: an unknown footprint degrades
//! to a trimmed-down name and no size, never to a wrong one.

/// Imperial chip codes → body size (length × width, mm). The metric suffix in a
/// KiCad name encodes the same thing exactly (see [`body_mm`]), so this is the
/// fallback for names that carry only the imperial code.
const CHIP: &[(&str, f64, f64)] = &[
    ("01005", 0.4, 0.2),
    ("0201", 0.6, 0.3),
    ("0402", 1.0, 0.5),
    ("0603", 1.6, 0.8),
    ("0805", 2.0, 1.25),
    ("1206", 3.2, 1.6),
    ("1210", 3.2, 2.5),
    ("1806", 4.5, 1.6),
    ("1812", 4.5, 3.2),
    ("2010", 5.0, 2.5),
    ("2512", 6.3, 3.2),
];

/// Packages whose name *is* the answer — matched as a leading token, so
/// `SOIC-16_3.9x9.9mm_P1.27mm` reads as `SOIC-16`.
const NAMED: &[&str] = &[
    "SOIC", "SOP", "TSSOP", "MSOP", "SSOP", "TSOP", "QFN", "DFN", "QFP", "TQFP", "LQFP", "DIP",
    "SIP", "TO", "SOT", "DO", "SMA", "SMB", "SMC", "MELF",
];

/// The part of a footprint id after the library prefix (`Resistor_SMD:R_0603_…`
/// → `R_0603_…`).
fn tail(fp: &str) -> &str {
    fp.rsplit(':').next().unwrap_or(fp)
}

/// Parse a `1608Metric`-style code: two digits of length then two of width, each
/// in tenths of a millimetre. This is exact — KiCad's metric code *is* the body
/// size — so it wins over the imperial lookup.
fn metric_code(tail: &str) -> Option<(f64, f64)> {
    let tok = tail
        .split('_')
        .find(|t| t.ends_with("Metric") && t.len() >= 10)?;
    let digits = &tok[..tok.len() - "Metric".len()];
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let l = digits[..2].parse::<f64>().ok()? / 10.0;
    let w = digits[2..].parse::<f64>().ok()? / 10.0;
    (l > 0.0 && w > 0.0).then_some((l, w))
}

/// Pull a `3.9x9.9mm`-style explicit body dimension out of any token.
fn explicit_mm(tail: &str) -> Option<(f64, f64)> {
    let tok = tail.split('_').find(|t| {
        t.ends_with("mm") && t.contains('x') && t.bytes().next().is_some_and(|b| b.is_ascii_digit())
    })?;
    let (a, b) = tok.trim_end_matches("mm").split_once('x')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// A trailing `…mm` measurement introduced by a single-letter key (`P2.54mm` →
/// pitch, `D2.5mm` → diameter, `L6.3mm` → length).
fn keyed_mm(tail: &str, key: char) -> Option<f64> {
    tail.split('_')
        .filter(|t| t.ends_with("mm") && t.starts_with(key))
        .find_map(|t| t[1..t.len() - 2].parse::<f64>().ok())
}

/// Whether the package is round seen from above — a disc ceramic or a radial
/// electrolytic — so its silhouette is a circle rather than a rectangle.
fn is_round(fp: &str) -> bool {
    let t = tail(fp);
    t.contains("Disc") || t.contains("Radial") || t.contains("Rect_Radial")
}

/// A `2x05` grid (columns × rows) from a pin-header name.
fn header_grid(tail: &str) -> Option<(u32, u32)> {
    tail.split('_').find_map(|t| {
        let (c, r) = t.split_once('x')?;
        Some((c.parse().ok()?, r.parse().ok()?))
    })
}

/// The leading `NAME-N` token for a package whose name is standard (`DIP-16`).
fn named_token(tail: &str) -> Option<&str> {
    tail.split('_').find(|t| {
        let base = t.split('-').next().unwrap_or(t);
        NAMED.contains(&base) && t.len() > base.len()
    })
}

/// Short package name for a KiCad footprint id — what goes in a `Package` column.
/// Best-effort: an unrecognised footprint yields its trimmed tail rather than
/// nothing, so the column is never blank when a footprint exists.
pub fn short_name(fp: &str) -> String {
    let t = tail(fp);
    // A chip package's imperial code is what everyone calls it.
    if let Some((code, _, _)) = CHIP
        .iter()
        .find(|(code, _, _)| t.split('_').any(|tok| tok == *code))
    {
        return (*code).to_string();
    }
    if let Some(tok) = named_token(t) {
        let base = tok.split('_').next().unwrap_or(tok);
        return if t.contains("Socket") {
            format!("{base} socket")
        } else {
            base.to_string()
        };
    }
    if t.contains("PinHeader") || t.contains("PinSocket") {
        if let Some((c, r)) = header_grid(t) {
            let kind = if t.contains("PinSocket") {
                "socket"
            } else {
                "header"
            };
            return format!("{c}x{r:02} {kind}");
        }
    }
    if t.contains("Axial") {
        return match keyed_mm(t, 'P') {
            Some(p) => format!("Axial {p}mm"),
            None => "Axial".to_string(),
        };
    }
    if is_round(fp) {
        return match keyed_mm(t, 'D') {
            Some(d) => format!(
                "{}{d}mm",
                if t.contains("Disc") {
                    "Disc "
                } else {
                    "Radial "
                }
            ),
            None if t.contains("Disc") => "Disc".to_string(),
            None => "Radial".to_string(),
        };
    }
    // The library prefix carries the class when the footprint's own name doesn't
    // repeat it — `Potentiometer_THT:Alpha_RD901F` is still a pot.
    if fp.contains("Potentiometer") || fp.contains("Trimmer") {
        return "Pot".to_string();
    }
    if fp.contains("Jack") || fp.contains("Audio") {
        return if fp.contains("3.5mm") {
            "3.5mm jack".to_string()
        } else {
            "Jack".to_string()
        };
    }
    // Nothing recognised — give back the leading tokens, readably.
    let short: Vec<&str> = t.split('_').take(2).collect();
    short.join(" ")
}

/// Whether a footprint is surface-mount, or `None` when the name doesn't say.
///
/// Read from the library prefix first (`Resistor_SMD:` / `Resistor_THT:` — KiCad
/// is consistent about this), then from the package family, then from a metric
/// chip code, which only ever appears on a chip part. `None` for anything that
/// leaves it genuinely ambiguous, so a caller filtering SMD out of a hand-build
/// sheet keeps the part rather than silently dropping something the builder has
/// to fit.
pub fn is_surface_mount(fp: &str) -> Option<bool> {
    let lower = fp.to_ascii_lowercase();
    let (lib, t) = lower.split_once(':').unwrap_or(("", lower.as_str()));
    if lib.contains("_smd") || lib.contains("_sm:") {
        return Some(true);
    }
    if lib.contains("_tht") || lib.contains("thruhole") || lib.contains("through") {
        return Some(false);
    }
    // Package families that are only ever one or the other.
    const SMD: &[&str] = &[
        "soic", "sop", "tssop", "msop", "ssop", "qfn", "dfn", "qfp", "lqfp", "tqfp", "bga", "son",
        "melf", "chip", "sod", "smd",
    ];
    const THT: &[&str] = &[
        "dip-",
        "pinheader",
        "pinsocket",
        "to-92",
        "to-220",
        "axial",
        "radial",
    ];
    if THT.iter().any(|k| t.contains(k)) {
        return Some(false);
    }
    if SMD.iter().any(|k| t.contains(k)) {
        return Some(true);
    }
    // A metric chip code (`1608Metric`) only appears on a chip package.
    metric_code(t).map(|_| true)
}

/// Body size `(width, height)` in mm, top-down, or `None` when the footprint name
/// carries no size we can trust. Used to draw a life-size silhouette, so a wrong
/// answer is worse than none.
pub fn body_mm(fp: &str) -> Option<(f64, f64)> {
    let t = tail(fp);
    if let Some(sz) = metric_code(t) {
        return Some(sz);
    }
    if let Some((_, l, w)) = CHIP
        .iter()
        .find(|(code, _, _)| t.split('_').any(|tok| tok == *code))
    {
        return Some((*l, *w));
    }
    // DIP-N: N/2 pins a side at 0.1", body 6.35mm wide (7.62mm row spacing).
    if let Some(tok) = named_token(t).filter(|tok| tok.starts_with("DIP-")) {
        if let Ok(n) = tok["DIP-".len()..].parse::<f64>() {
            return Some(((n / 2.0) * 2.54 + 0.6, 6.35));
        }
    }
    if let Some(sz) = explicit_mm(t) {
        return Some(sz);
    }
    // Pin header: (cols-1)×pitch by (rows-1)×pitch, plus a shell wall each way.
    if t.contains("PinHeader") || t.contains("PinSocket") {
        if let (Some((c, r)), Some(p)) = (header_grid(t), keyed_mm(t, 'P')) {
            return Some((c as f64 * p, r as f64 * p));
        }
    }
    // Axial (resistor, diode): body length × diameter.
    if let (Some(l), Some(d)) = (keyed_mm(t, 'L'), keyed_mm(t, 'D')) {
        return Some((l, d));
    }
    // Disc / radial (ceramic disc, electrolytic can): a circle of diameter D,
    // which is the dimension you'd measure with calipers.
    if is_round(fp) {
        if let Some(d) = keyed_mm(t, 'D') {
            return Some((d, d));
        }
    }
    None
}

/// A **life-size** top-down silhouette of the package (CSS `mm` units, so it
/// prints true at 100% scale) with its measured size beneath. The fallback for a
/// Visual BOM cell with no photo: you can still lay the real part on the page and
/// see whether you grabbed the 0603 or the 0805.
///
/// `None` when [`body_mm`] can't read the footprint — an invented outline would
/// be worse than an honest blank.
pub fn silhouette_svg(fp: &str) -> Option<String> {
    let (w, h) = body_mm(fp)?;
    let round = is_round(fp);
    // Hairline outside the body, so the drawn edge is the body edge.
    let (pw, ph) = (w + 0.6, h + 0.6);
    let shape = if round {
        format!(
            "<circle cx=\"{cx:.3}\" cy=\"{cy:.3}\" r=\"{r:.3}\" fill=\"#fff\" stroke=\"#1c2024\" \
             stroke-width=\"0.2\"/>",
            cx = pw / 2.0,
            cy = ph / 2.0,
            r = w / 2.0,
        )
    } else {
        format!(
            "<rect x=\"0.3\" y=\"0.3\" width=\"{w:.3}\" height=\"{h:.3}\" rx=\"{rx:.3}\" \
             fill=\"#fff\" stroke=\"#1c2024\" stroke-width=\"0.2\"/>",
            rx = (w.min(h) * 0.12).min(0.5),
        )
    };
    Some(format!(
        "<svg class=\"sil\" width=\"{pw:.2}mm\" height=\"{ph:.2}mm\" \
         viewBox=\"0 0 {pw:.3} {ph:.3}\" xmlns=\"http://www.w3.org/2000/svg\">{shape}</svg>\
         <span class=\"sil-dim\">{w} × {h} mm</span>",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_packages_read_as_their_imperial_code_and_metric_size() {
        assert_eq!(short_name("Resistor_SMD:R_0603_1608Metric"), "0603");
        assert_eq!(short_name("Capacitor_SMD:C_0805_2012Metric"), "0805");
        // The metric code is exact, and it is what the size comes from.
        assert_eq!(body_mm("Resistor_SMD:R_0603_1608Metric"), Some((1.6, 0.8)));
        assert_eq!(body_mm("Capacitor_SMD:C_0805_2012Metric"), Some((2.0, 1.2)));
    }

    #[test]
    fn named_packages_keep_their_standard_name() {
        assert_eq!(
            short_name("Package_SO:SOIC-16_3.9x9.9mm_P1.27mm"),
            "SOIC-16"
        );
        assert_eq!(short_name("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm"), "SOIC-8");
        assert_eq!(
            short_name("Package_DIP:DIP-16_W7.62mm_Socket"),
            "DIP-16 socket"
        );
        assert_eq!(short_name("Package_TO_SOT_THT:TO-92_Inline"), "TO-92");
        // Explicit WxHmm in the name is the body size.
        assert_eq!(
            body_mm("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm"),
            Some((3.9, 4.9))
        );
    }

    #[test]
    fn headers_report_their_grid_and_span() {
        let fp = "Connector_PinHeader_2.54mm:PinHeader_2x05_P2.54mm_Vertical";
        assert_eq!(short_name(fp), "2x05 header");
        assert_eq!(body_mm(fp), Some((5.08, 12.7)));
    }

    #[test]
    fn axial_resistors_report_their_lead_pitch_and_body() {
        let fp = "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P7.62mm_Horizontal";
        assert_eq!(short_name(fp), "Axial 7.62mm");
        assert_eq!(body_mm(fp), Some((6.3, 2.5)));
    }

    #[test]
    fn round_packages_report_their_diameter_and_draw_as_circles() {
        let disc = "Capacitor_THT:C_Disc_D5.0mm_W2.5mm_P5.00mm";
        assert_eq!(short_name(disc), "Disc 5mm");
        assert_eq!(body_mm(disc), Some((5.0, 5.0)));
        assert!(silhouette_svg(disc).unwrap().contains("<circle"));
        // The class can live in the library prefix rather than the name itself.
        assert_eq!(short_name("Potentiometer_THT:Alpha_RD901F"), "Pot");
        assert_eq!(short_name("Connector_Audio:Jack_3.5mm"), "3.5mm jack");
    }

    #[test]
    fn unknown_footprints_degrade_to_a_readable_name_and_no_size() {
        let fp = "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical_CircularHoles";
        assert_eq!(short_name(fp), "3.5mm jack");
        // Nothing measurable in the name — better blank than invented.
        assert_eq!(body_mm("Weird_Lib:Totally_Unknown_Thing"), None);
        assert!(silhouette_svg("Weird_Lib:Totally_Unknown_Thing").is_none());
    }

    #[test]
    fn surface_mount_is_read_from_the_library_then_the_family() {
        // KiCad's library prefix is the most reliable signal.
        assert_eq!(
            is_surface_mount("Resistor_SMD:R_0603_1608Metric"),
            Some(true)
        );
        assert_eq!(
            is_surface_mount("Resistor_THT:R_Axial_DIN0207_L6.3mm"),
            Some(false)
        );
        // Then the package family.
        assert_eq!(is_surface_mount("Package_SO:SOIC-16_3.9x9.9mm"), Some(true));
        assert_eq!(is_surface_mount("Package_DIP:DIP-16_W7.62mm"), Some(false));
        assert_eq!(
            is_surface_mount("Connector_PinHeader_2.54mm:PinHeader_2x05_P2.54mm"),
            Some(false)
        );
        // Panel hardware is hand-fitted whatever else it is.
        assert_eq!(
            is_surface_mount("Potentiometer_THT:Alpha_RD901F"),
            Some(false)
        );
        // Genuinely ambiguous stays ambiguous: dropping a part the builder has
        // to fit is worse than showing one they don't.
        assert_eq!(is_surface_mount("MyLib:Mystery_Thing"), None);
    }

    #[test]
    fn silhouettes_are_drawn_life_size_in_mm() {
        let svg = silhouette_svg("Resistor_SMD:R_0603_1608Metric").unwrap();
        // 1.6 × 0.8mm body plus the 0.3mm hairline allowance each side.
        assert!(svg.contains("width=\"2.20mm\""), "{svg}");
        assert!(svg.contains("height=\"1.40mm\""), "{svg}");
        assert!(svg.contains("1.6 × 0.8 mm"), "{svg}");
    }
}
