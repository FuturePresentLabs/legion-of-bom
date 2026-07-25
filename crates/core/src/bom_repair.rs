//! Repairing a BOM that has no part numbers (b8c).
//!
//! Imported BOMs frequently leave the distributor column blank — of the five
//! SuperSynthesis packages, only one fills it. But the *comment* column is rarely
//! empty, and it is doing one of three quite different jobs:
//!
//! | it says | it means | what to do |
//! |---|---|---|
//! | `1N4148WS`, `LM13700`, `VAOL-3LAE2` | a manufacturer part number | look it up to confirm |
//! | `100nF`, `1.8kΩ,0603`, `2.2MEG` | a value | search on value + package |
//! | `CV`, `OUT`, `TRIG` | what a jack is *for* | not a part number at all |
//!
//! Telling these apart is the whole job: treating a value as an MPN searches for
//! a part that does not exist, and treating an MPN as a value throws away the
//! answer someone already wrote down.

/// What a BOM comment turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Comment {
    /// Looks like a manufacturer part number — usable directly, once confirmed.
    Mpn(String),
    /// A component value, with the package it came in when the comment said so
    /// (`1.8kΩ,0603` carries both).
    Value {
        value: String,
        package: Option<String>,
    },
    /// A function label — a jack or header named for its role on the panel.
    /// Not orderable from this text; the package is all there is to go on.
    Label(String),
    /// Nothing usable.
    Empty,
}

/// Whether a package name denotes a connector — a jack, header or terminal.
/// Their comments name a *signal*, not a part.
fn is_connector(package: &str) -> bool {
    let p = package.to_ascii_uppercase();
    [
        "JACK",
        "THONKICONN",
        "EURO_POWER",
        "EUROPWR",
        "HEADER",
        "TERMINAL",
        "USB",
        "MOLEX",
    ]
    .iter()
    .any(|k| p.contains(k))
}

/// Whether the text reads as a component value: a number, optionally a decimal,
/// then a magnitude and/or unit — `10k`, `100nF`, `2.2MEG`, `47n`, `220Ω`.
fn looks_like_value(s: &str) -> bool {
    let t = s.trim();
    // Split at the first character that is not part of the number.
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, rest) = t.split_at(split);
    if num.is_empty() || num.parse::<f64>().is_err() {
        return false;
    }
    // Whatever follows must be only a magnitude and/or unit. Anything else — more
    // digits, a hyphen — means it is a part number that merely starts numeric,
    // like `2N7002T`.
    let rest = rest.trim().to_ascii_uppercase();
    if rest.is_empty() {
        return true;
    }
    matches!(
        rest.as_str(),
        "K" | "M"
            | "MEG"
            | "R"
            | "E"
            | "U"
            | "N"
            | "P"
            | "F"
            | "H"
            | "V"
            | "Ω"
            | "µ"
            | "OHM"
            | "OHMS"
            | "KΩ"
            | "MΩ"
            | "RΩ"
            | "KOHM"
            | "MOHM"
            | "PF"
            | "NF"
            | "UF"
            | "µF"
            | "MF"
    )
}

/// Whether the text reads as a manufacturer part number: long enough to be
/// distinctive, and mixing letters with digits the way part numbers do.
fn looks_like_mpn(s: &str) -> bool {
    let t = s.trim();
    if t.len() < 4 {
        return false;
    }
    let has_digit = t.chars().any(|c| c.is_ascii_digit());
    let has_alpha = t.chars().any(|c| c.is_ascii_alphabetic());
    let plausible = t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./+".contains(c));
    has_digit && has_alpha && plausible
}

/// Whether a token is a chip-package size on its own (`0603`).
fn is_package_size(t: &str) -> bool {
    matches!(
        t.trim(),
        "0201" | "0402" | "0603" | "0805" | "1206" | "1210" | "2010" | "2512"
    )
}

/// Classify a BOM comment, using the package for what the text alone cannot
/// settle.
///
/// Order matters, and each step earns its place against the real data:
///
/// - The **first token** decides, because authors append ratings, tolerances and
///   notes freely: `10uF 50v`, `10k 0603 1%`, `RV9012NO-PA25B7.0 Tall Trimmer 10k`.
///   Judging the whole string reads `10uF 50v` as a part number.
/// - A value is tested **before** a part number, since `10uF` satisfies both
///   ("has letters and digits") and only one of them is true.
/// - A part number is tested **before** the connector rule, or a jack that
///   records a real one (`WQP-WQP518MA`) is thrown away as a function label.
pub fn classify(comment: &str, package: &str) -> Comment {
    let text = comment.trim();
    if text.is_empty() {
        return Comment::Empty;
    }
    // A "do not populate" marker is an instruction, not a part.
    if text.to_ascii_uppercase().contains("DNP") {
        return Comment::Empty;
    }

    // Commas separate as much as spaces do: `1.8kΩ,0603` and `10k 0603 1%` are
    // the same thought written two ways.
    let tokens: Vec<&str> = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let Some(first) = tokens.first().copied() else {
        return Comment::Empty;
    };

    if looks_like_value(first) {
        return Comment::Value {
            value: first.to_string(),
            // A later token may name the package — prefer it over the column,
            // since the author wrote it next to the value on purpose.
            package: tokens[1..]
                .iter()
                .find(|t| is_package_size(t))
                .map(|t| t.to_string()),
        };
    }

    if looks_like_mpn(first) {
        return Comment::Mpn(first.to_string());
    }

    // Only now: a connector whose comment names the signal it carries.
    if is_connector(package) {
        return Comment::Label(text.to_string());
    }
    Comment::Label(text.to_string())
}

/// What to do about one BOM line that has no part number.
#[derive(Debug, Clone, PartialEq)]
pub enum Repair {
    /// The comment already *is* a part number — confirm it and use it.
    UsePartNumber(String),
    /// Search a distributor with this keyword built from value + package.
    Search(String),
    /// Nothing to search: a connector or label, identifiable only by its package.
    NotSourceable(String),
}

/// Decide how to recover a part number for a line, from its comment and package.
pub fn plan(comment: &str, package: &str) -> Repair {
    match classify(comment, package) {
        Comment::Mpn(m) => Repair::UsePartNumber(m),
        Comment::Value { value, package: p } => {
            // Use both: the comment's token is the more trustworthy *size* (the
            // author wrote it beside the value), while the column carries the
            // part *class* — `10k 0603 1%` with column `0603 RES` should search
            // for a resistor, not just a 0603 something.
            let pkg = match p {
                Some(t) => format!("{t} {package}"),
                None => package.to_string(),
            };
            Repair::Search(search_keyword(&value, &pkg))
        }
        Comment::Label(l) => Repair::NotSourceable(format!("{l} ({package})")),
        Comment::Empty => Repair::NotSourceable(package.to_string()),
    }
}

/// A distributor search keyword from a value and a package.
///
/// Package strings in an imported BOM are written for people — `0603 CAP`,
/// `CAP_1206`, `RES_0603` — so the size is pulled out and the class is taken from
/// whichever word is present, giving `100nF capacitor 0603` rather than a literal
/// echo of the column.
pub fn search_keyword(value: &str, package: &str) -> String {
    let p = package.to_ascii_uppercase();
    let class = if p.contains("CAP") {
        "capacitor"
    } else if p.contains("RES") {
        "resistor"
    } else if p.contains("IND") {
        "inductor"
    } else {
        ""
    };
    let size = [
        "0201", "0402", "0603", "0805", "1206", "1210", "2010", "2512",
    ]
    .iter()
    .find(|s| p.contains(*s))
    .copied()
    .unwrap_or("");
    [value.trim(), class, size]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comment column is doing three jobs at once, and confusing them is the
    /// whole failure mode: searching a distributor for "100nF" as a part number
    /// finds nothing, and discarding "1N4148WS" as a value throws away the
    /// answer the author already wrote down.
    #[test]
    fn tells_part_numbers_from_values_from_labels() {
        // Real part numbers, straight out of the SuperSynthesis BOMs.
        for (c, p) in [
            ("1N4148WS", "SOD-323"),
            ("MMBT3906", "SOT23"),
            ("LM13700", "SOIC-16/150mil"),
            ("TL072D", "SOIC-8/150mil"),
            ("VAOL-3LAE2", "LEDT1"),
            ("2N7002T", "SOT-523"),
            ("CD4013", "SOIC-14/150mil"),
            ("PTA2043-2015DPB103", "PTA2043"),
        ] {
            assert!(
                matches!(classify(c, p), Comment::Mpn(m) if m == c),
                "{c} should read as a part number, got {:?}",
                classify(c, p)
            );
        }

        // Values, in the several ways these BOMs write them.
        for (c, p) in [
            ("100nF", "0603 CAP"),
            ("47n", "0603 CAP"),
            ("10uF", "CAP_1206"),
            ("2.2MEG", "RES_0603"),
            ("1.5k", "0603 RES"),
            ("220Ω", "RES_0603"),
        ] {
            assert!(
                matches!(classify(c, p), Comment::Value { .. }),
                "{c} should read as a value, got {:?}",
                classify(c, p)
            );
        }

        // A jack's comment names the signal, not the part.
        for c in ["CV", "OUT", "TRIG", "EOC"] {
            assert!(
                matches!(classify(c, "Thonkiconn Jack"), Comment::Label(_)),
                "{c} on a jack is a function label"
            );
        }
    }

    /// `1.8kΩ,0603` carries the value and the package in one field.
    #[test]
    fn splits_a_value_that_carries_its_own_package() {
        let got = classify("1.8kΩ,0603", "RES_0603");
        assert_eq!(
            got,
            Comment::Value {
                value: "1.8kΩ".into(),
                package: Some("0603".into())
            }
        );
    }

    /// A trailing human note must not defeat the part number in front of it.
    #[test]
    fn takes_the_part_number_from_a_commented_line() {
        assert_eq!(
            classify("RV9012NO-PA25B7.0 Tall Trimmer 10k", "EVUF"),
            Comment::Mpn("RV9012NO-PA25B7.0".into())
        );
    }

    /// Authors append ratings, tolerances and package sizes to a value. Judging
    /// the whole string reads `10uF 50v` as a part number and drops
    /// `10k 0603 1%` entirely, so the first token decides.
    #[test]
    fn a_value_survives_its_trailing_rating_and_tolerance() {
        for (text, want_value, want_pkg) in [
            ("10uF 50v", "10uF", None),
            ("1n 50v", "1n", None),
            ("10k 0603 1%", "10k", Some("0603")),
            ("100kΩ 0603", "100kΩ", Some("0603")),
            ("86.6kΩ 0603", "86.6kΩ", Some("0603")),
        ] {
            assert_eq!(
                classify(text, "0603 RES"),
                Comment::Value {
                    value: want_value.into(),
                    package: want_pkg.map(str::to_string)
                },
                "{text}"
            );
        }
    }

    /// A connector that records a real part number must keep it — the signal
    /// name is only a fallback for when there is nothing better.
    #[test]
    fn a_jack_with_a_part_number_keeps_it() {
        assert_eq!(
            classify("WQP-WQP518MA", "Thonkiconn Jack"),
            Comment::Mpn("WQP-WQP518MA".into())
        );
        assert_eq!(
            classify("CV", "Thonkiconn Jack"),
            Comment::Label("CV".into())
        );
    }

    #[test]
    fn dnp_is_an_instruction_not_a_part() {
        assert_eq!(classify("*RST* DNP", "0603 CAP"), Comment::Empty);
    }

    /// A part number that merely starts with a digit must not be mistaken for a
    /// value — `2N7002T` is a transistor, not 2 of something.
    #[test]
    fn a_numeric_leading_part_number_is_not_a_value() {
        assert!(!looks_like_value("2N7002T"));
        assert!(!looks_like_value("1N4148WS"));
        assert!(looks_like_value("2.2MEG"));
        assert!(looks_like_value("100n"));
    }

    #[test]
    fn builds_a_searchable_keyword_from_value_and_package() {
        assert_eq!(search_keyword("100nF", "0603 CAP"), "100nF capacitor 0603");
        assert_eq!(search_keyword("10k", "RES_0603"), "10k resistor 0603");
        assert_eq!(search_keyword("10uF", "CAP_1206"), "10uF capacitor 1206");
    }

    /// The comment's package token is the better size, the column is the better
    /// class; a search wants both.
    #[test]
    fn a_search_keeps_the_class_from_the_package_column() {
        assert_eq!(
            plan("10k 0603 1%", "0603 RES"),
            Repair::Search("10k resistor 0603".into())
        );
    }

    #[test]
    fn plans_the_right_action_per_line() {
        assert_eq!(
            plan("1N4148WS", "SOD-323"),
            Repair::UsePartNumber("1N4148WS".into())
        );
        assert_eq!(
            plan("100nF", "0603 CAP"),
            Repair::Search("100nF capacitor 0603".into())
        );
        assert!(matches!(
            plan("CV", "Thonkiconn Jack"),
            Repair::NotSourceable(_)
        ));
    }
}
