//! EasyEDA / LCSC product-photo lookup by keyword. 5uj.4.
//!
//! The one free, keyless route to **real** part photos that works server-side.
//! The obvious sources don't: Mouser bot-blocks its image host (HTML "access
//! denied"), JLCPCB's component API carries no image, and LCSC's `szlcsc` bucket
//! is private. But EasyEDA's product search returns LCSC catalog entries whose
//! photos live on the hotlinkable `assets.lcsc.com/images/lcsc/` CDN (verified
//! 2026-07-24) — a real photo of the actual part, keyed by a keyword (an MPN or a
//! distinctive value like `"LM13700"`).
//!
//! Kept thin over `ureq`; every failure returns `None` so the Visual BOM falls
//! back to a color swatch / blank and never breaks on a missing photo.

use serde_json::Value;

const SEARCH_URL: &str = "https://easyeda.com/api/eda/product/list";

/// Look up a product-photo URL for `keyword` (an MPN, or a distinctive value like
/// `"LM13700"`). Returns the best small-thumbnail URL of the first catalog match,
/// or `None` on any failure / no match. Network call.
pub fn product_image_url(keyword: &str) -> Option<String> {
    // Scan several matches, not just the first: the top hit for a keyword often
    // lacks a photo while a sibling variant (same MPN, different maker) has one.
    let url = format!(
        "{SEARCH_URL}?keyword={}&page=1&pageSize=8",
        urlencode(keyword)
    );
    let resp = ureq::get(&url)
        .set(
            "User-Agent",
            "Mozilla/5.0 (compatible; legion-of-bom Visual BOM)",
        )
        .set("Referer", "https://easyeda.com/")
        .call()
        .ok()?;
    let value: Value = resp.into_json().ok()?;
    first_product_image(&value)
}

/// The best thumbnail URL of the first product that *has* a photo in an EasyEDA
/// `product/list` response, preferring a mid-size render.
fn first_product_image(value: &Value) -> Option<String> {
    let products = value.get("result")?.get("productList")?.as_array()?;
    for product in products {
        let Some(image) = product
            .get("image")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            continue;
        };
        // A part image object carries several sizes; prefer a legible-but-small one.
        if let Some(url) = ["224x224", "96x96", "900x900"]
            .iter()
            .find_map(|k| image.get(*k).and_then(Value::as_str))
            .filter(|u| !u.is_empty())
        {
            return Some(url.to_string());
        }
    }
    None
}

/// Percent-encode a query keyword (RFC 3986 unreserved set kept verbatim).
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The real shape of an EasyEDA product/list response (verified live): the
    // FIRST match here has no photo, a later one does — the common case.
    const FIXTURE: &str = r#"{
      "result": {
        "total": 3,
        "productList": [
          { "mpn": "TL072A", "manufacturer": "TI" },
          { "mpn": "TL072G", "manufacturer": "UTC",
            "image": [
              { "sort": 1, "type": "front",
                "900x900": "https://assets.lcsc.com/images/lcsc/900x900/x_front.jpg",
                "224x224": "https://assets.lcsc.com/images/lcsc/224x224/x_front.jpg",
                "96x96": "https://assets.lcsc.com/images/lcsc/96x96/x_front.jpg" } ] }
        ]
      }
    }"#;

    #[test]
    fn parses_first_photo_skipping_imageless_matches_preferring_midsize() {
        let v: Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            first_product_image(&v).as_deref(),
            Some("https://assets.lcsc.com/images/lcsc/224x224/x_front.jpg")
        );
    }

    #[test]
    fn missing_or_empty_image_yields_none() {
        let empty: Value = serde_json::from_str(r#"{"result":{"productList":[]}}"#).unwrap();
        assert_eq!(first_product_image(&empty), None);
        let no_result: Value = serde_json::from_str(r#"{"foo":1}"#).unwrap();
        assert_eq!(first_product_image(&no_result), None);
    }

    #[test]
    fn urlencodes_keywords() {
        assert_eq!(urlencode("LM13700"), "LM13700");
        assert_eq!(urlencode("TL072CDR/NOPB"), "TL072CDR%2FNOPB");
        assert_eq!(urlencode("2N3904 BJT"), "2N3904%20BJT");
    }
}
