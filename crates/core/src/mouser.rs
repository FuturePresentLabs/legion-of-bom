//! Mouser Search API client — live unit price + stock by MPN. zya.2, DESIGN.md 9.
//!
//! Single-vendor to start (the multi-vendor JLC/LCSC/DigiKey fallback is
//! deliberately deferred). The key comes from `MOUSER_API_KEY` (loaded from
//! `.env` at CLI startup). The request/response mechanics — URL, JSON shape,
//! how a response maps onto [`PartPrice`] — live in `assets/distributors/
//! mouser.lua`, loaded via [`crate::distributor_lua`]: swapping in a
//! different pricing API means editing that script, not this file.

use serde::{Deserialize, Serialize};

use crate::distributor_lua::{load_named_script, DistributorScript, DistributorScriptError};

const SCRIPT_NAME: &str = "mouser.lua";
const REQUIRED_FNS: &[&str] = &["search_mpn", "search_keyword"];

/// Errors from Mouser lookups.
#[derive(Debug, thiserror::Error)]
pub enum MouserError {
    #[error("MOUSER_API_KEY is not set (put it in .env)")]
    MissingKey,
    #[error("Mouser: {0}")]
    Script(#[from] DistributorScriptError),
}

/// One quantity price break.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceBreak {
    pub quantity: u64,
    pub unit_price: f64,
    pub currency: String,
}

/// Live pricing/stock for a part, as returned by Mouser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Serialize)]
struct MpnRequest<'a> {
    api_key: &'a str,
    mpn: &'a str,
}

#[derive(Serialize)]
struct KeywordRequest<'a> {
    api_key: &'a str,
    keyword: &'a str,
    records: u16,
}

/// A Mouser Search API client.
pub struct MouserClient {
    api_key: String,
    script: DistributorScript,
}

impl MouserClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self, MouserError> {
        let script = load_named_script(SCRIPT_NAME, REQUIRED_FNS)?;
        Ok(MouserClient {
            api_key: api_key.into(),
            script,
        })
    }

    /// Build from `MOUSER_API_KEY` in the environment.
    pub fn from_env() -> Result<Self, MouserError> {
        match std::env::var("MOUSER_API_KEY") {
            Ok(key) if !key.trim().is_empty() => MouserClient::new(key),
            _ => Err(MouserError::MissingKey),
        }
    }

    /// Search Mouser by manufacturer part number; returns the best match.
    pub fn search_mpn(&self, mpn: &str) -> Result<Option<PartPrice>, MouserError> {
        Ok(self.script.call(
            "search_mpn",
            &MpnRequest {
                api_key: &self.api_key,
                mpn,
            },
        )?)
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
        Ok(self.script.call(
            "search_keyword",
            &KeywordRequest {
                api_key: &self.api_key,
                keyword,
                records,
            },
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The real shipped script — these tests exercise its actual parsing
    /// logic (no live API key / network needed: `parse_search_response` and
    /// `parse_keyword_response` are pure functions of a JSON fixture).
    fn script() -> DistributorScript {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/distributors/mouser.lua");
        DistributorScript::load(&path, REQUIRED_FNS).expect("load mouser.lua")
    }

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

    #[derive(Serialize)]
    struct SearchReq<'a> {
        json: &'a str,
        wanted: &'a str,
    }

    #[test]
    fn parses_response_and_prefers_exact_match() {
        let price: PartPrice = script()
            .call(
                "parse_search_response",
                &SearchReq {
                    json: FIXTURE,
                    wanted: "LM13700M/NOPB",
                },
            )
            .unwrap();
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

        // $1,234.50 (thousands separator) parses correctly too.
        let other: PartPrice = script()
            .call(
                "parse_search_response",
                &SearchReq {
                    json: FIXTURE,
                    wanted: "LM13700MX/NOPB",
                },
            )
            .unwrap();
        assert_eq!(other.unit_price_at(1), Some(1234.50));
    }

    #[test]
    fn reports_api_errors_and_empty() {
        let err_fixture = r#"{"Errors":[{"Message":"Invalid key"}]}"#;
        let result: Result<Option<PartPrice>, _> = script().call(
            "parse_search_response",
            &SearchReq {
                json: err_fixture,
                wanted: "X",
            },
        );
        assert!(result.is_err());

        let empty_fixture = r#"{"Errors":[],"SearchResults":{"Parts":[]}}"#;
        let empty: Option<PartPrice> = script()
            .call(
                "parse_search_response",
                &SearchReq {
                    json: empty_fixture,
                    wanted: "X",
                },
            )
            .unwrap();
        assert_eq!(empty, None);
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
        let hits: Vec<PartPrice> = script()
            .call("parse_keyword_response", &KEYWORD_FIXTURE)
            .unwrap();
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
        let err_fixture = r#"{"Errors":[{"Message":"Too many"}]}"#;
        let result: Result<Vec<PartPrice>, _> =
            script().call("parse_keyword_response", &err_fixture);
        assert!(result.is_err());

        let empty_fixture = r#"{"SearchResults":{"Parts":[]}}"#;
        let empty: Vec<PartPrice> = script()
            .call("parse_keyword_response", &empty_fixture)
            .unwrap();
        assert_eq!(empty.len(), 0);
    }
}
