//! Thonk product lookup — real photos of the parts we actually buy.
//!
//! LCSC ([`crate::easyeda`]) pictures catalog electronics well and DIY panel
//! hardware badly. A Thonkiconn jack, an Alpha 9 mm pot, a bag of jack nuts:
//! these are Eurorack-shop goods, and the shop that stocks them is our supplier.
//! Thonk runs WooCommerce, whose WordPress REST API is public and keyless:
//! `GET /wp-json/wp/v2/product?search=…&_embed=wp:featuredmedia` returns the
//! product title, its page, and a featured image in several sizes (verified
//! 2026-07-26).
//!
//! The catch is that it is a *fuzzy* WordPress search, not a parametric one — a
//! query for `LM13700` cheerfully returns an unrelated VCA kit, and pasting that
//! photo next to an op-amp on a sorting sheet would be worse than leaving the
//! cell blank. So every hit is checked against the query by [`title_matches`]
//! before it is offered, and anything unconvincing is dropped.
//!
//! Kept thin over `ureq`; every failure yields `None`/empty so a build never
//! depends on a shop being up.

use serde_json::Value;

const API: &str = "https://www.thonk.co.uk/wp-json/wp/v2/product";
const UA: &str = "Mozilla/5.0 (compatible; legion-of-bom build-doc image fetch)";

/// One product in Thonk's catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct ThonkProduct {
    /// Product title, entity-decoded (`"Thonkiconn – 3.5mm Jack Sockets"`).
    pub title: String,
    /// The product page — worth printing next to a part so a builder can reorder.
    pub url: String,
    /// Best product photo for a sorting-sheet cell, if the entry has one.
    pub image_url: Option<String>,
}

/// Search Thonk for `keyword`, returning up to `limit` products whose titles
/// actually correspond to it ([`title_matches`]). Network call; any failure
/// yields an empty list.
pub fn search(keyword: &str, limit: usize) -> Vec<ThonkProduct> {
    let url = format!(
        "{API}?per_page={}&_embed=wp:featuredmedia&search={}",
        limit.clamp(1, 20),
        urlencode(keyword),
    );
    let Ok(resp) = ureq::get(&url).set("User-Agent", UA).call() else {
        return Vec::new();
    };
    let Ok(value): Result<Value, _> = resp.into_json() else {
        return Vec::new();
    };
    parse_products(&value, keyword, limit)
}

/// The best product photo for `keyword`, or `None`.
pub fn product_image_url(keyword: &str) -> Option<String> {
    search(keyword, 5).into_iter().find_map(|p| p.image_url)
}

/// Extract matching products from a `wp/v2/product` response.
fn parse_products(value: &Value, keyword: &str, limit: usize) -> Vec<ThonkProduct> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|p| {
            let title = decode_entities(
                p.get("title")
                    .and_then(|t| t.get("rendered"))
                    .and_then(Value::as_str)?,
            );
            if !title_matches(keyword, &title) {
                return None;
            }
            Some(ThonkProduct {
                image_url: featured_image(p),
                url: p
                    .get("link")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                title,
            })
        })
        .take(limit)
        .collect()
}

/// Whether a search hit's title plausibly *is* what was searched for.
///
/// WordPress search is generous — it will return a whole module kit because the
/// build notes mention an op-amp, or a bag of power *cables* for a query about a
/// power *header*. The guard: at least two thirds of the query's meaningful words
/// must appear in the title, and at least one of them must be a substantial word
/// rather than a stray "3.5" or "mm".
///
/// Two thirds rather than half, because half let "Eurorack Power Cables" through
/// for "eurorack power header shrouded" — it shares the two generic words and
/// none of the specific ones. A missing photo costs the builder a blank cell; a
/// wrong photo costs them the wrong part.
fn title_matches(keyword: &str, title: &str) -> bool {
    let title = title.to_ascii_lowercase();
    let words: Vec<String> = keyword
        .to_ascii_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '.')
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(w))
        .map(str::to_string)
        .collect();
    if words.is_empty() {
        return false;
    }
    let hits = words.iter().filter(|w| title.contains(w.as_str())).count();
    let strong = words
        .iter()
        .any(|w| w.len() >= 4 && title.contains(w.as_str()));
    strong && hits * 3 >= words.len() * 2
}

/// Words too common in part descriptions to count as evidence of a match.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "pcb", "kit", "diy", "new", "set", "pack", "type", "vertical",
];

/// The featured image URL from an `_embed`ed response, preferring a size big
/// enough to print at ~35 mm but not the full-resolution original.
fn featured_image(product: &Value) -> Option<String> {
    let media = product
        .get("_embedded")
        .and_then(|e| e.get("wp:featuredmedia"))
        .and_then(Value::as_array)
        .and_then(|a| a.first())?;
    let sizes = media.get("media_details").and_then(|d| d.get("sizes"));
    let sized = sizes.and_then(|s| {
        ["woocommerce_thumbnail", "medium_large", "medium", "large"]
            .iter()
            .find_map(|k| {
                s.get(*k)
                    .and_then(|v| v.get("source_url"))
                    .and_then(Value::as_str)
            })
    });
    sized
        .or_else(|| media.get("source_url").and_then(Value::as_str))
        .filter(|u| !u.is_empty())
        .map(str::to_string)
}

/// Decode the handful of HTML entities WordPress puts in rendered titles.
fn decode_entities(s: &str) -> String {
    s.replace("&#8211;", "–")
        .replace("&#8212;", "—")
        .replace("&#8217;", "’")
        .replace("&#038;", "&")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}

/// Percent-encode a query string component.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hit_must_actually_be_the_thing_we_searched_for() {
        // Real responses observed 2026-07-26.
        assert!(title_matches(
            "TL072",
            "TL072 – Through-hole (DIP-8) IC Chip (x1)"
        ));
        assert!(title_matches("jack nut", "Jack Nuts and Washers"));
        assert!(title_matches(
            "Alpha 9mm potentiometer",
            "Alpha 9mm Pots – Vertical T18 Shaft"
        ));
        // The one that matters: searching an op-amp returned a whole VCA kit.
        assert!(!title_matches(
            "LM13700",
            "Zlob Modular – VnIcursal VCA Full DIY Kit"
        ));
        // A single short token is not evidence.
        assert!(!title_matches("1k", "Turing Machine Mkii"));
        assert!(!title_matches("", "Anything"));
        // Sharing only the generic words is not a match: this one shipped a
        // photo of power *cables* for a query about a power *header*.
        assert!(!title_matches(
            "eurorack power header shrouded",
            "Eurorack Power Cables"
        ));
        assert!(title_matches(
            "eurorack power header shrouded",
            "Eurorack 10pin Power Headers – Shrouded (x10)"
        ));
    }

    #[test]
    fn stopwords_do_not_carry_a_match_on_their_own() {
        // "kit"/"diy" appear in half the catalog, so they must not be what makes
        // a hit look convincing.
        assert!(!title_matches("diy kit", "Eurorack DIY Essentials"));
    }

    #[test]
    fn parses_a_product_with_its_embedded_photo() {
        let body = serde_json::json!([{
            "title": {"rendered": "Thonkiconn &#8211; 3.5mm Jack Sockets"},
            "link": "https://www.thonk.co.uk/shop/thonkiconn/",
            "_embedded": {"wp:featuredmedia": [{
                "source_url": "https://x/full.jpg",
                "media_details": {"sizes": {
                    "woocommerce_thumbnail": {"source_url": "https://x/700.jpg"},
                    "medium": {"source_url": "https://x/300.jpg"}
                }}
            }]}
        }]);
        let got = parse_products(&body, "thonkiconn jack", 5);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "Thonkiconn – 3.5mm Jack Sockets");
        assert_eq!(got[0].url, "https://www.thonk.co.uk/shop/thonkiconn/");
        // Prefers a print-sized image over the full-resolution original.
        assert_eq!(got[0].image_url.as_deref(), Some("https://x/700.jpg"));
    }

    #[test]
    fn a_product_with_no_photo_is_still_a_result() {
        let body = serde_json::json!([{
            "title": {"rendered": "PJ301BM – 3.5mm Jack Sockets"},
            "link": "https://www.thonk.co.uk/shop/pj301bm/"
        }]);
        let got = parse_products(&body, "PJ301BM jack", 5);
        assert_eq!(got.len(), 1);
        assert!(got[0].image_url.is_none());
    }

    #[test]
    fn a_malformed_response_yields_nothing_rather_than_panicking() {
        assert!(parse_products(&serde_json::json!({}), "x", 5).is_empty());
        assert!(parse_products(&serde_json::json!([{"title": 3}]), "x", 5).is_empty());
    }
}
