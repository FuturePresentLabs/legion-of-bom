//! Which photograph belongs to a BOM line, and where it comes from.
//!
//! Splitting *choosing* the source from *embedding* it matters because two
//! callers need different halves. The Visual BOM wants the picture; the
//! dashboard's crop editor wants the source URL, so it can show the uncropped
//! original and record a rectangle against it ([`crate::images::Crop`]). Both
//! must agree on which image a line uses, or you would crop one photo and see
//! another.

use std::path::Path;

use crate::bom::{BomLine, LineKind};
use crate::images::source_bytes;

/// The image source for a line — a `file://` path or an http(s) URL — or `None`
/// when no photo is wanted or none could be found.
///
/// Tries in order and returns the first that actually yields an image, rather
/// than the first that merely *names* one: a curated URL that 404s must fall
/// through to Thonk, not leave the cell blank.
///
/// Order matters. LCSC has no picture of a bag of jack nuts and a poor one of an
/// Alpha pot; Thonk sells both and photographs them on a bench. For a jellybean
/// op-amp it is the other way round, which is why the Thonk attempt is gated on
/// [`thonk_keyword`] rather than tried for everything.
pub fn photo_source(line: &BomLine, cache: &Path) -> Option<String> {
    // A curated/library image (may be a `file://` local photo) wins.
    if let Some(src) = &line.image_url {
        if source_bytes(src, cache).is_some() {
            return Some(src.clone());
        }
    }
    if let Some(url) = thonk_keyword(line).and_then(|k| crate::thonk::product_image_url(&k)) {
        if source_bytes(&url, cache).is_some() {
            return Some(url);
        }
    }
    let url = photo_keyword(line).and_then(|k| crate::easyeda::product_image_url(&k))?;
    source_bytes(&url, cache).is_some().then_some(url)
}

/// A Thonk-shaped search term for a line, or `None` when Thonk is the wrong shop
/// to ask.
///
/// Thonk is searched by what a thing *is* ("Alpha 9mm pot"), not by MPN — they
/// stock `WQP-PJ398SM` jacks but that string returns nothing, while "Thonkiconn
/// 3.5mm jack" returns the product. So this maps our footprint vocabulary onto
/// theirs, and stays silent for anything that is really a catalog part.
pub fn thonk_keyword(line: &BomLine) -> Option<String> {
    if line.kind == LineKind::Hardware {
        // "M6 jack nut" / "Pot washer" → the bag Thonk actually sells.
        let v = line.value.to_ascii_lowercase();
        if v.contains("jack") {
            return Some("jack nuts and washers".into());
        }
        if v.contains("pot") {
            return Some("potentiometer nuts washers".into());
        }
        return None;
    }
    let fp = line.footprint.as_deref()?.to_ascii_lowercase();
    let term = if fp.contains("pj398sm") || fp.contains("thonkiconn") {
        "thonkiconn 3.5mm jack sockets"
    } else if fp.contains("pj301") {
        "pj301bm 3.5mm jack sockets"
    } else if fp.contains("jack_3.5mm") || fp.contains("audiojack") {
        "3.5mm jack sockets"
    } else if fp.contains("rd901f") || fp.contains("potentiometer") {
        "alpha 9mm pots vertical"
    // Panel LEDs, by the size that decides which bag they came from.
    } else if fp.contains("led_d3") || fp.contains("led_d3.0") {
        "3mm led"
    } else if fp.contains("led_d5") || fp.contains("led_d5.0") {
        "5mm led"
    } else if fp.contains("pinheader_2x05") {
        "eurorack power header shrouded"
    } else {
        return None;
    };
    Some(term.to_string())
}

/// The photo-search keyword for a line, or `None` when a photo isn't wanted:
/// passives (R/C) get a swatch/blank, and a generic value with no MPN (e.g.
/// `"100k"`) would only return noise — so require a part-number-like token.
pub fn photo_keyword(line: &BomLine) -> Option<String> {
    let prefix: String = line
        .refdes
        .first()?
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    if matches!(prefix.as_str(), "R" | "C") {
        return None;
    }
    let keyword = line
        .mpn
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(line.value.as_str());
    if keyword.is_empty() || (line.mpn.is_none() && !has_letter_run(keyword, 2)) {
        return None;
    }
    Some(keyword.to_string())
}

/// Whether `s` contains a run of at least `n` consecutive ASCII letters — the
/// "looks like a part number, not a resistance" test.
fn has_letter_run(s: &str, n: usize) -> bool {
    let mut run = 0usize;
    for c in s.chars() {
        run = if c.is_ascii_alphabetic() { run + 1 } else { 0 };
        if run >= n {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(refdes: &str, value: &str, footprint: Option<&str>) -> BomLine {
        BomLine {
            kind: LineKind::Component,
            mpn: None,
            value: value.to_string(),
            footprint: footprint.map(str::to_string),
            refdes: vec![refdes.to_string()],
            unit_price: None,
            ext_price: None,
            image_url: None,
        }
    }

    #[test]
    fn passives_get_no_photo_keyword_and_generic_values_are_not_searched() {
        assert_eq!(photo_keyword(&line("R1", "10k", None)), None);
        assert_eq!(photo_keyword(&line("C1", "100nF", None)), None);
        // A bare value with no letter run is noise as a search term.
        assert_eq!(photo_keyword(&line("U1", "100", None)), None);
        assert_eq!(
            photo_keyword(&line("U1", "TL072", None)).as_deref(),
            Some("TL072")
        );
    }

    #[test]
    fn thonk_is_asked_by_what_a_thing_is_not_by_mpn() {
        assert_eq!(
            thonk_keyword(&line("J1", "jack", Some("Connector:AudioJack2_SwitchT"))).as_deref(),
            Some("3.5mm jack sockets")
        );
        // A catalog part is not Thonk's department.
        assert_eq!(
            thonk_keyword(&line("U1", "TL072", Some("Package_SO:SOIC-8"))),
            None
        );
    }
}
