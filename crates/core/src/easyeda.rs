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

/// One LCSC/JLCPCB catalog candidate surfaced by an EasyEDA keyword search — the
/// keyless path from a keyword to real LCSC parts (MPN + LCSC code + stock + a
/// cheapest-break price). Contrast [`crate::jlcpcb::JlcpcbComponent`], which is an
/// *exact* lookup by a known LCSC code; this is the discovery side.
///
/// NOTE (lrr): EasyEDA's `product/list` is a text search, not a parametric one —
/// a *distinctive* keyword (an MPN or an IC name like `"TL072"`/`"LM13700"`)
/// returns clean matches, but a *generic* value (`"10k 0805 resistor"`) returns
/// loose noise. So for generic passives this is unreliable and Mouser keyword
/// search is the working path; this shines for named parts. A true LCSC
/// parametric API is bot-blocked (Akamai) and deferred rather than scraped.
#[derive(Debug, Clone, PartialEq)]
pub struct LcscCandidate {
    /// Manufacturer part number (`"RC0805FR-0710KL"`).
    pub mpn: String,
    /// LCSC component code (`"C84376"`) — the key for a later `jlcpcb` fetch.
    pub lcsc_code: Option<String>,
    pub manufacturer: Option<String>,
    /// Package/footprint as LCSC reports it (`"0805"`, `"SOIC-8"`).
    pub package: Option<String>,
    pub stock: Option<u64>,
    /// Cheapest advertised unit price across the LCSC price breaks, if any.
    pub unit_price: Option<f64>,
    /// Best small-thumbnail product photo, if the catalog entry carries one.
    pub image_url: Option<String>,
}

/// Keyword-search the LCSC/EasyEDA catalog, returning up to `limit` candidates.
/// Best-effort and keyless; any failure yields an empty list so the caller can
/// fall back to Mouser. Network call. See [`LcscCandidate`] for when this is
/// reliable (named parts) vs not (generic passives).
pub fn search_parts(keyword: &str, limit: usize) -> Vec<LcscCandidate> {
    let page_size = limit.clamp(1, 25);
    let url = format!(
        "{SEARCH_URL}?keyword={}&page=1&pageSize={page_size}",
        urlencode(keyword)
    );
    let Ok(resp) = ureq::get(&url)
        .set(
            "User-Agent",
            "Mozilla/5.0 (compatible; legion-of-bom sourcing)",
        )
        .set("Referer", "https://easyeda.com/")
        .call()
    else {
        return Vec::new();
    };
    let Ok(value): Result<Value, _> = resp.into_json() else {
        return Vec::new();
    };
    parse_candidates(&value, limit)
}

/// Extract [`LcscCandidate`]s from a `product/list` response. Only entries with a
/// non-empty `mpn` are kept (an MPN is the whole point — a bare LCSC code isn't a
/// buildable suggestion on its own).
fn parse_candidates(value: &Value, limit: usize) -> Vec<LcscCandidate> {
    let Some(products) = value
        .get("result")
        .and_then(|r| r.get("productList"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let string = |p: &Value, key: &str| {
        p.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && *s != "-")
            .map(str::to_string)
    };
    products
        .iter()
        .filter_map(|p| {
            let mpn = string(p, "mpn")?;
            Some(LcscCandidate {
                mpn,
                lcsc_code: string(p, "number"),
                manufacturer: string(p, "manufacturer"),
                package: string(p, "package"),
                stock: p.get("stock").and_then(Value::as_u64),
                unit_price: cheapest_price(p),
                image_url: product_image(p),
            })
        })
        .take(limit)
        .collect()
}

/// Cheapest unit price across an LCSC `price` array (`[[qty, unit, ext], …]`).
fn cheapest_price(p: &Value) -> Option<f64> {
    p.get("price")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|b| {
            b.get(1)
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
        })
        .min_by(|a: &f64, b: &f64| a.total_cmp(b))
}

/// The best small-thumbnail URL for a single product entry (shared with
/// [`first_product_image`]'s per-entry logic).
fn product_image(product: &Value) -> Option<String> {
    let image = product
        .get("image")
        .and_then(Value::as_array)
        .and_then(|a| a.first())?;
    ["224x224", "96x96", "900x900"]
        .iter()
        .find_map(|k| image.get(*k).and_then(Value::as_str))
        .filter(|u| !u.is_empty())
        .map(str::to_string)
}

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
    products.iter().find_map(product_image)
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

    // A real `product/list` shape for a distinctive keyword: MPN, LCSC code
    // (`number`), package, stock, and a price-break array (cheapest wins).
    const CANDIDATES_FIXTURE: &str = r#"{
      "result": {
        "total": 3,
        "productList": [
          { "mpn": "RC0805FR-0710KL", "number": "C84376", "manufacturer": "YAGEO",
            "package": "0805", "stock": 2535100,
            "price": [[1, "0.0089", "0.0089"], [100, "0.0021", "0.0021"]],
            "image": [ { "224x224": "https://assets.lcsc.com/images/lcsc/224x224/y.jpg" } ] },
          { "mpn": "TL072G", "number": "C108153", "manufacturer": "UTC",
            "package": "SOP-8", "stock": 2975,
            "price": [[5, "0.1904", "0.1904"], [5000, "0.0915", "0.0915"]] },
          { "number": "C99999", "manufacturer": "NoMPN", "package": "-", "stock": 10 }
        ]
      }
    }"#;

    #[test]
    fn parses_lcsc_candidates_and_skips_mpnless() {
        let v: Value = serde_json::from_str(CANDIDATES_FIXTURE).unwrap();
        let cands = parse_candidates(&v, 10);
        // The third entry has no `mpn` → dropped (not a buildable suggestion).
        assert_eq!(cands.len(), 2);

        let r = &cands[0];
        assert_eq!(r.mpn, "RC0805FR-0710KL");
        assert_eq!(r.lcsc_code.as_deref(), Some("C84376"));
        assert_eq!(r.package.as_deref(), Some("0805"));
        assert_eq!(r.stock, Some(2535100));
        assert_eq!(r.unit_price, Some(0.0021)); // cheapest break
        assert_eq!(
            r.image_url.as_deref(),
            Some("https://assets.lcsc.com/images/lcsc/224x224/y.jpg")
        );
        // No photo on the op-amp entry → None (Visual BOM falls back).
        assert_eq!(cands[1].image_url, None);
    }

    #[test]
    fn candidate_limit_and_empty() {
        let v: Value = serde_json::from_str(CANDIDATES_FIXTURE).unwrap();
        assert_eq!(parse_candidates(&v, 1).len(), 1);
        let empty: Value = serde_json::from_str(r#"{"result":{"productList":[]}}"#).unwrap();
        assert!(parse_candidates(&empty, 10).is_empty());
    }
}
