//! Mouser Search API client — live unit price + stock by MPN. zya.2, DESIGN.md 9.
//!
//! Single-vendor to start (the multi-vendor JLC/LCSC/DigiKey fallback is
//! deliberately deferred). The key comes from `MOUSER_API_KEY` (loaded from
//! `.env` at CLI startup). Kept thin over `ureq` so it stays snappy.

use serde_json::Value;

const SEARCH_URL: &str = "https://api.mouser.com/api/v1/search/partnumber";
const KEYWORD_URL: &str = "https://api.mouser.com/api/v1/search/keyword";

/// Errors from Mouser lookups.
#[derive(Debug, thiserror::Error)]
pub enum MouserError {
    #[error("MOUSER_API_KEY is not set (put it in .env)")]
    MissingKey,
    #[error("Mouser API error: {0}")]
    Api(String),
    #[error("Mouser request failed: {0}")]
    Http(String),
}

/// One quantity price break.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceBreak {
    pub quantity: u64,
    pub unit_price: f64,
    pub currency: String,
}

/// Live pricing/stock for a part, as returned by Mouser.
#[derive(Debug, Clone, PartialEq)]
pub struct PartPrice {
    /// The manufacturer part number Mouser actually matched (may be a variant).
    pub mpn: String,
    pub manufacturer: Option<String>,
    pub in_stock: Option<u64>,
    pub datasheet_url: Option<String>,
    pub product_url: Option<String>,
    /// Product photo URL (Mouser `ImagePath`) — the Visual BOM thumbnail source.
    pub image_url: Option<String>,
    pub price_breaks: Vec<PriceBreak>,
}

impl PartPrice {
    /// Unit price for ordering `qty`: the highest price-break quantity that is
    /// still ≤ `qty` (falling back to the smallest break if `qty` is below all).
    pub fn unit_price_at(&self, qty: u64) -> Option<f64> {
        self.price_breaks
            .iter()
            .filter(|b| b.quantity <= qty)
            .max_by_key(|b| b.quantity)
            .or_else(|| self.price_breaks.iter().min_by_key(|b| b.quantity))
            .map(|b| b.unit_price)
    }
}

/// A Mouser Search API client.
#[derive(Debug, Clone)]
pub struct MouserClient {
    api_key: String,
}

impl MouserClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        MouserClient {
            api_key: api_key.into(),
        }
    }

    /// Build from `MOUSER_API_KEY` in the environment.
    pub fn from_env() -> Result<Self, MouserError> {
        match std::env::var("MOUSER_API_KEY") {
            Ok(key) if !key.trim().is_empty() => Ok(MouserClient::new(key)),
            _ => Err(MouserError::MissingKey),
        }
    }

    /// Search Mouser by manufacturer part number; returns the best match.
    pub fn search_mpn(&self, mpn: &str) -> Result<Option<PartPrice>, MouserError> {
        let url = format!("{SEARCH_URL}?apiKey={}", self.api_key);
        let body = serde_json::json!({
            "SearchByPartRequest": { "mouserPartNumber": mpn, "partSearchOptions": "" }
        });
        let response = ureq::post(&url)
            .send_json(body)
            .map_err(|e| MouserError::Http(e.to_string()))?;
        let value: Value = response
            .into_json()
            .map_err(|e| MouserError::Http(e.to_string()))?;
        parse_search(&value, mpn)
    }

    /// Search Mouser by free-text keyword — the path from a *generic* value
    /// (`"10k resistor 0805"`) to real MPNs, which `search_mpn` (exact-lookup)
    /// can't do. Returns up to `records` in-stock candidates, ranked by Mouser's
    /// own relevance (we re-rank in [`crate::sourcing`]). `records` is clamped to
    /// Mouser's 1..=50 window; 0 means "use the default (10)".
    pub fn search_keyword(
        &self,
        keyword: &str,
        records: u16,
    ) -> Result<Vec<PartPrice>, MouserError> {
        let records = if records == 0 { 10 } else { records.min(50) };
        let url = format!("{KEYWORD_URL}?apiKey={}", self.api_key);
        let body = serde_json::json!({
            "SearchByKeywordRequest": {
                "keyword": keyword,
                "records": records,
                "startingRecord": 0,
                "searchOptions": "InStock",
            }
        });
        let response = ureq::post(&url)
            .send_json(body)
            .map_err(|e| MouserError::Http(e.to_string()))?;
        let value: Value = response
            .into_json()
            .map_err(|e| MouserError::Http(e.to_string()))?;
        parse_keyword(&value)
    }
}

/// Parse a Mouser keyword-search response into every candidate part. Shares the
/// `SearchResults.Parts[]` shape with the part-number search, so it reuses
/// [`part_from_json`]; unlike [`parse_search`] it keeps *all* matches (the caller
/// ranks and picks), not just the best MPN hit.
fn parse_keyword(value: &Value) -> Result<Vec<PartPrice>, MouserError> {
    check_errors(value)?;
    let parts = value
        .get("SearchResults")
        .and_then(|r| r.get("Parts"))
        .and_then(Value::as_array);
    Ok(parts
        .map(|arr| arr.iter().map(part_from_json).collect())
        .unwrap_or_default())
}

/// Return an `Api` error if the response carries a non-empty `Errors[]`.
fn check_errors(value: &Value) -> Result<(), MouserError> {
    if let Some(errors) = value.get("Errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            let msg = errors
                .iter()
                .filter_map(|e| e.get("Message").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(MouserError::Api(if msg.is_empty() {
                "unknown error".into()
            } else {
                msg
            }));
        }
    }
    Ok(())
}

/// Parse a Mouser search response, preferring an exact MPN match.
fn parse_search(value: &Value, wanted: &str) -> Result<Option<PartPrice>, MouserError> {
    check_errors(value)?;

    let Some(parts) = value
        .get("SearchResults")
        .and_then(|r| r.get("Parts"))
        .and_then(Value::as_array)
        .filter(|p| !p.is_empty())
    else {
        return Ok(None);
    };

    let part = parts
        .iter()
        .find(|p| p.get("ManufacturerPartNumber").and_then(Value::as_str) == Some(wanted))
        .unwrap_or(&parts[0]);
    Ok(Some(part_from_json(part)))
}

fn part_from_json(p: &Value) -> PartPrice {
    let string = |key: &str| {
        p.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let price_breaks = p
        .get("PriceBreaks")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    Some(PriceBreak {
                        quantity: b.get("Quantity").and_then(Value::as_u64)?,
                        unit_price: b
                            .get("Price")
                            .and_then(Value::as_str)
                            .and_then(parse_price)?,
                        currency: b
                            .get("Currency")
                            .and_then(Value::as_str)
                            .unwrap_or("USD")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    PartPrice {
        mpn: string("ManufacturerPartNumber").unwrap_or_default(),
        manufacturer: string("Manufacturer"),
        in_stock: p
            .get("AvailabilityInStock")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok()),
        datasheet_url: string("DataSheetUrl"),
        product_url: string("ProductDetailUrl"),
        image_url: string("ImagePath"),
        price_breaks,
    }
}

/// Parse a Mouser price string like `"$1.48"` or `"$1,234.50"` into a number.
/// Assumes a `.`-decimal currency (USD); strips the currency symbol and thousands
/// separators.
fn parse_price(s: &str) -> Option<f64> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    cleaned.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "Errors": [],
      "SearchResults": {
        "NumberOfResult": 2,
        "Parts": [
          { "ManufacturerPartNumber": "LM13700M/NOPB", "Manufacturer": "Texas Instruments",
            "AvailabilityInStock": "8156", "ProductDetailUrl": "https://mouser.com/x",
            "ImagePath": "https://www.mouser.com/images/ti/ITP_SOIC-16.jpg",
            "PriceBreaks": [
              {"Quantity": 1, "Price": "$1.48", "Currency": "USD"},
              {"Quantity": 10, "Price": "$1.07", "Currency": "USD"},
              {"Quantity": 100, "Price": "$0.858", "Currency": "USD"}
            ] },
          { "ManufacturerPartNumber": "LM13700MX/NOPB", "Manufacturer": "Texas Instruments",
            "PriceBreaks": [{"Quantity": 1, "Price": "$1,234.50", "Currency": "USD"}] }
        ]
      }
    }"#;

    #[test]
    fn parses_price_strings() {
        assert_eq!(parse_price("$1.48"), Some(1.48));
        assert_eq!(parse_price("$0.858"), Some(0.858));
        assert_eq!(parse_price("$1,234.50"), Some(1234.50));
        assert_eq!(parse_price(""), None);
    }

    #[test]
    fn parses_response_and_prefers_exact_match() {
        let value: Value = serde_json::from_str(FIXTURE).unwrap();
        let price = parse_search(&value, "LM13700M/NOPB").unwrap().unwrap();
        assert_eq!(price.mpn, "LM13700M/NOPB");
        assert_eq!(price.in_stock, Some(8156));
        assert_eq!(
            price.image_url.as_deref(),
            Some("https://www.mouser.com/images/ti/ITP_SOIC-16.jpg")
        );
        assert_eq!(price.price_breaks.len(), 3);
        // qty 1 → $1.48; qty 50 → the 10-break ($1.07); qty 1000 → the 100-break.
        assert_eq!(price.unit_price_at(1), Some(1.48));
        assert_eq!(price.unit_price_at(50), Some(1.07));
        assert_eq!(price.unit_price_at(1000), Some(0.858));
    }

    #[test]
    fn reports_api_errors_and_empty() {
        let err: Value = serde_json::from_str(r#"{"Errors":[{"Message":"Invalid key"}]}"#).unwrap();
        assert!(matches!(parse_search(&err, "X"), Err(MouserError::Api(_))));

        let empty: Value =
            serde_json::from_str(r#"{"Errors":[],"SearchResults":{"Parts":[]}}"#).unwrap();
        assert_eq!(parse_search(&empty, "X").unwrap(), None);
    }

    // A keyword search ("10k resistor 0805") returns several unrelated candidates —
    // the whole point is turning a generic value into real MPNs. Same
    // `SearchResults.Parts[]` shape as the partnumber search.
    const KEYWORD_FIXTURE: &str = r#"{
      "Errors": [],
      "SearchResults": {
        "NumberOfResult": 3,
        "Parts": [
          { "ManufacturerPartNumber": "RC0805FR-0710KL", "Manufacturer": "YAGEO",
            "AvailabilityInStock": "425000", "DataSheetUrl": "https://y/ds.pdf",
            "ProductDetailUrl": "https://mouser.com/rc0805",
            "PriceBreaks": [
              {"Quantity": 1, "Price": "$0.10", "Currency": "USD"},
              {"Quantity": 100, "Price": "$0.012", "Currency": "USD"}
            ] },
          { "ManufacturerPartNumber": "CRCW080510K0FKEA", "Manufacturer": "Vishay",
            "AvailabilityInStock": "200000",
            "PriceBreaks": [{"Quantity": 1, "Price": "$0.11", "Currency": "USD"}] },
          { "ManufacturerPartNumber": "ERJ-6ENF1002V", "Manufacturer": "Panasonic",
            "AvailabilityInStock": "0",
            "PriceBreaks": [{"Quantity": 1, "Price": "$0.09", "Currency": "USD"}] }
        ]
      }
    }"#;

    #[test]
    fn keyword_search_returns_all_candidates() {
        let value: Value = serde_json::from_str(KEYWORD_FIXTURE).unwrap();
        let hits = parse_keyword(&value).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].mpn, "RC0805FR-0710KL");
        assert_eq!(hits[0].manufacturer.as_deref(), Some("YAGEO"));
        assert_eq!(hits[0].in_stock, Some(425000));
        assert_eq!(hits[0].unit_price_at(1), Some(0.10));
        // Out-of-stock candidates are still parsed (ranking, not filtering, is the
        // sourcing layer's job).
        assert_eq!(hits[2].in_stock, Some(0));
    }

    #[test]
    fn keyword_search_errors_and_empty() {
        let err: Value = serde_json::from_str(r#"{"Errors":[{"Message":"Too many"}]}"#).unwrap();
        assert!(matches!(parse_keyword(&err), Err(MouserError::Api(_))));
        let empty: Value = serde_json::from_str(r#"{"SearchResults":{"Parts":[]}}"#).unwrap();
        assert_eq!(parse_keyword(&empty).unwrap().len(), 0);
    }
}
