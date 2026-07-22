//! Parsing of engineering/SPICE component values like `"1k"`, `"159n"`, `"4.7u"`.
//!
//! Used to compute analytic circuit behaviour (e.g. the RC cutoff `1/(2πRC)`)
//! from parsed part values. Follows SPICE conventions: suffixes are
//! case-insensitive, `meg` is mega (a bare `m` is milli), and any trailing
//! characters after a recognised suffix are ignored (`"1kohm"` == 1000).

/// Parse an engineering value into a plain `f64` (base units), or `None` if the
/// numeric part is unparseable or the suffix is unrecognised.
pub fn parse_eng_value(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Split the leading numeric part from the alphabetic suffix.
    let split = s
        .find(|c: char| c.is_ascii_alphabetic() || c == 'µ')
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let value: f64 = num.parse().ok()?;

    let suffix = suffix.to_ascii_lowercase();
    let mult = if suffix.is_empty() {
        1.0
    } else if suffix.starts_with("meg") {
        1e6
    } else {
        match suffix.chars().next()? {
            'f' => 1e-15,
            'p' => 1e-12,
            'n' => 1e-9,
            'u' | 'µ' => 1e-6,
            'm' => 1e-3,
            'k' => 1e3,
            'g' => 1e9,
            't' => 1e12,
            _ => return None,
        }
    };
    Some(value * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() <= b.abs() * 1e-9 + 1e-15
    }

    #[test]
    fn parses_common_values() {
        assert!(approx(parse_eng_value("1k").unwrap(), 1e3));
        assert!(approx(parse_eng_value("159n").unwrap(), 159e-9));
        assert!(approx(parse_eng_value("4.7u").unwrap(), 4.7e-6));
        assert!(approx(parse_eng_value("2.2M").unwrap(), 2.2e-3)); // SPICE: M = milli
        assert!(approx(parse_eng_value("1meg").unwrap(), 1e6));
        assert!(approx(parse_eng_value("100").unwrap(), 100.0));
        assert!(approx(parse_eng_value("10k").unwrap(), 1e4));
    }

    #[test]
    fn ignores_trailing_unit_text() {
        assert!(approx(parse_eng_value("1kohm").unwrap(), 1e3));
        assert!(approx(parse_eng_value("100nF").unwrap(), 100e-9));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_eng_value(""), None);
        assert_eq!(parse_eng_value("abc"), None);
        assert_eq!(parse_eng_value("1x"), None);
    }
}
