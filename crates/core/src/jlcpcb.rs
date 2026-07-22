//! JLCPCB (open API) client — authoritative part data by LCSC component code.
//! okm.3 distributor-official source; DESIGN.md 3.5, LIBRARIES.md §1.
//!
//! JLCPCB's component API is keyed by LCSC code (`C…`), not MPN, and returns
//! datasheet URL, description, structured `parameters`, package, price, and
//! stock — but not pin names (those are CAD data; the KiCad-library source fills
//! pins). So this source contributes the authoritative datasheet + ratings +
//! MPN↔LCSC mapping.
//!
//! Auth (reverse-engineered, verified live): each request is signed
//! `HMAC-SHA256(secret, "METHOD\n{path}\n{timestamp}\n{nonce}\n{body}\n")`,
//! base64-encoded, sent as `Authorization: JOP appid=…,accesskey=…,timestamp=…,
//! nonce=…,signature=…`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

const BASE_URL: &str = "https://open.jlcpcb.com";
const DETAIL_PATH: &str = "/overseas/openapi/component/getComponentDetailByCode";

/// Errors from JLCPCB lookups.
#[derive(Debug, thiserror::Error)]
pub enum JlcpcbError {
    #[error(
        "JLCPCB_APP_ID / JLCPCB_ACCESS_KEY / JLCPCB_SECRET_KEY not all set (put them in .env)"
    )]
    MissingKeys,
    #[error("JLCPCB API error ({code}): {message}")]
    Api { code: i64, message: String },
    #[error("JLCPCB request failed: {0}")]
    Http(String),
}

/// A component as returned by JLCPCB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JlcpcbComponent {
    /// LCSC component code (`C1002`).
    pub component_code: String,
    /// Manufacturer part number / model.
    pub component_model: String,
    /// Package (`0603`, `SOIC-16`, …).
    pub package: Option<String>,
    pub description: Option<String>,
    pub datasheet_url: Option<String>,
    /// `library_type` — `base` or `extended`.
    pub library_type: Option<String>,
    pub stock: Option<u64>,
    /// Structured parametric data (name, value).
    pub parameters: Vec<(String, String)>,
}

/// A signed JLCPCB open-API client.
#[derive(Debug, Clone)]
pub struct JlcpcbClient {
    app_id: String,
    access_key: String,
    secret_key: String,
}

impl JlcpcbClient {
    pub fn new(
        app_id: impl Into<String>,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Self {
        JlcpcbClient {
            app_id: app_id.into(),
            access_key: access_key.into(),
            secret_key: secret_key.into(),
        }
    }

    /// Build from `JLCPCB_APP_ID` / `JLCPCB_ACCESS_KEY` / `JLCPCB_SECRET_KEY`.
    pub fn from_env() -> Result<Self, JlcpcbError> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        match (
            get("JLCPCB_APP_ID"),
            get("JLCPCB_ACCESS_KEY"),
            get("JLCPCB_SECRET_KEY"),
        ) {
            (Some(a), Some(k), Some(s)) => Ok(JlcpcbClient::new(a, k, s)),
            _ => Err(JlcpcbError::MissingKeys),
        }
    }

    /// Look up a component by LCSC code (`C1002`).
    pub fn component_by_code(&self, code: &str) -> Result<Option<JlcpcbComponent>, JlcpcbError> {
        let body = serde_json::json!({ "componentCodes": [code] }).to_string();
        let value = self.post(DETAIL_PATH, &body)?;
        Ok(parse_detail(&value))
    }

    /// Sign and POST a request, returning the parsed JSON (after checking `code`).
    fn post(&self, path: &str, body: &str) -> Result<Value, JlcpcbError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .to_string();
        let nonce = nonce();
        let signature = self.sign("POST", path, &timestamp, &nonce, body);
        let auth = format!(
            r#"JOP appid="{}",accesskey="{}",timestamp="{timestamp}",nonce="{nonce}",signature="{signature}""#,
            self.app_id, self.access_key
        );

        let response = ureq::post(&format!("{BASE_URL}{path}"))
            .set("Content-Type", "application/json")
            .set("Authorization", &auth)
            .send_string(body)
            .map_err(|e| JlcpcbError::Http(e.to_string()))?;
        let value: Value = response
            .into_json()
            .map_err(|e| JlcpcbError::Http(e.to_string()))?;

        let code = value.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 200 {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            return Err(JlcpcbError::Api { code, message });
        }
        Ok(value)
    }

    /// `base64(HMAC-SHA256(secret, "METHOD\n{path}\n{ts}\n{nonce}\n{body}\n"))`.
    fn sign(&self, method: &str, path: &str, timestamp: &str, nonce: &str, body: &str) -> String {
        let string_to_sign = format!("{method}\n{path}\n{timestamp}\n{nonce}\n{body}\n");
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret_key.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(string_to_sign.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }
}

/// A per-process-unique 32-hex-char nonce (time-nanos + counter — the server only
/// needs uniqueness within its timestamp window, not cryptographic randomness).
fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:016x}{counter:016x}")
}

fn parse_detail(value: &Value) -> Option<JlcpcbComponent> {
    let list = value.get("data").and_then(Value::as_array)?;
    let c = list.first()?;
    let string = |key: &str| {
        c.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    // Prefer the LCSC datasheet link, fall back to JLCPCB's file link.
    let datasheet_url = string("dataManualUrl").or_else(|| string("datasheetUrl"));
    let parameters = c
        .get("parameters")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    Some((
                        p.get("parameterName").and_then(Value::as_str)?.to_string(),
                        p.get("parameterValue").and_then(Value::as_str)?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    Some(JlcpcbComponent {
        component_code: string("componentCode").unwrap_or_default(),
        component_model: string("componentModel").unwrap_or_default(),
        package: string("componentSpecification"),
        description: string("description"),
        datasheet_url,
        library_type: string("libraryType"),
        stock: c.get("stockCount").and_then(Value::as_u64),
        parameters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_is_deterministic_and_matches_spec() {
        let client = JlcpcbClient::new("app", "ak", "topsecret");
        // Known HMAC-SHA256 of the exact string, base64. Recomputed here to lock
        // the string-to-sign format (METHOD\npath\nts\nnonce\nbody\n).
        let sig = client.sign("POST", "/x", "1700000000", "abc", "{}");
        let expected = {
            let mut mac = Hmac::<Sha256>::new_from_slice(b"topsecret").unwrap();
            mac.update(b"POST\n/x\n1700000000\nabc\n{}\n");
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
        };
        assert_eq!(sig, expected);
    }

    #[test]
    fn nonces_are_unique() {
        let a = nonce();
        let b = nonce();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
    }

    const FIXTURE: &str = r#"{
      "code": 200, "success": true,
      "data": [{
        "componentCode": "C1002",
        "componentModel": "GZ1608D601TF",
        "componentSpecification": "0603",
        "description": "600Ω@100MHz ±25% 0603 Ferrite Beads",
        "dataManualUrl": "https://www.lcsc.com/datasheet/x.pdf",
        "datasheetUrl": "https://jlcpcb.com/api/file/y",
        "libraryType": "base",
        "stockCount": 1072580,
        "parameters": [
          {"parameterName": "Number of Circuits", "parameterValue": "1"},
          {"parameterName": "Impedance", "parameterValue": "600Ω"}
        ]
      }]
    }"#;

    #[test]
    fn parses_detail_response() {
        let value: Value = serde_json::from_str(FIXTURE).unwrap();
        let c = parse_detail(&value).unwrap();
        assert_eq!(c.component_code, "C1002");
        assert_eq!(c.component_model, "GZ1608D601TF");
        assert_eq!(c.package.as_deref(), Some("0603"));
        assert_eq!(
            c.datasheet_url.as_deref(),
            Some("https://www.lcsc.com/datasheet/x.pdf")
        );
        assert_eq!(c.stock, Some(1072580));
        assert_eq!(c.parameters.len(), 2);
        assert_eq!(c.parameters[0], ("Number of Circuits".into(), "1".into()));
    }
}
