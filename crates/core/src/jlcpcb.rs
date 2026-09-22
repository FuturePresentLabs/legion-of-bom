//! JLCPCB (open API) client — authoritative part data by LCSC component code.
//! okm.3 distributor-official source; DESIGN.md 3.5, LIBRARIES.md §1.
//!
//! JLCPCB's component API is keyed by LCSC code (`C…`), not MPN, and returns
//! datasheet URL, description, structured `parameters`, package, price, and
//! stock — but not pin names (those are CAD data; the KiCad-library source fills
//! pins). So this source contributes the authoritative datasheet + ratings +
//! MPN↔LCSC mapping.
//!
//! The request/response mechanics — the HMAC-SHA256 request signing, the
//! JSON shape, how a response maps onto [`JlcpcbComponent`] — live in
//! `assets/distributors/jlcpcb.lua`, loaded via [`crate::distributor_lua`]:
//! swapping in a different open-API distributor means editing that script,
//! not this file. The signature itself (real crypto) and the network call
//! stay host-side; the script only decides what to sign and where to send it.

use serde::{Deserialize, Serialize};

use crate::distributor_lua::{load_named_script, DistributorScript, DistributorScriptError};

const SCRIPT_NAME: &str = "jlcpcb.lua";
const REQUIRED_FNS: &[&str] = &["component_by_code"];

/// Errors from JLCPCB lookups.
#[derive(Debug, thiserror::Error)]
pub enum JlcpcbError {
    #[error(
        "JLCPCB_APP_ID / JLCPCB_ACCESS_KEY / JLCPCB_SECRET_KEY not all set (put them in .env)"
    )]
    MissingKeys,
    #[error("JLCPCB: {0}")]
    Script(#[from] DistributorScriptError),
}

/// A component as returned by JLCPCB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Serialize)]
struct ComponentRequest<'a> {
    app_id: &'a str,
    access_key: &'a str,
    secret_key: &'a str,
    code: &'a str,
}

/// A signed JLCPCB open-API client.
pub struct JlcpcbClient {
    app_id: String,
    access_key: String,
    secret_key: String,
    script: DistributorScript,
}

impl JlcpcbClient {
    pub fn new(
        app_id: impl Into<String>,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Result<Self, JlcpcbError> {
        let script = load_named_script(SCRIPT_NAME, REQUIRED_FNS)?;
        Ok(JlcpcbClient {
            app_id: app_id.into(),
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            script,
        })
    }

    /// Build from `JLCPCB_APP_ID` / `JLCPCB_ACCESS_KEY` / `JLCPCB_SECRET_KEY`.
    pub fn from_env() -> Result<Self, JlcpcbError> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        match (
            get("JLCPCB_APP_ID"),
            get("JLCPCB_ACCESS_KEY"),
            get("JLCPCB_SECRET_KEY"),
        ) {
            (Some(a), Some(k), Some(s)) => JlcpcbClient::new(a, k, s),
            _ => Err(JlcpcbError::MissingKeys),
        }
    }

    /// Look up a component by LCSC code (`C1002`).
    pub fn component_by_code(&self, code: &str) -> Result<Option<JlcpcbComponent>, JlcpcbError> {
        Ok(self.script.call(
            "component_by_code",
            &ComponentRequest {
                app_id: &self.app_id,
                access_key: &self.access_key,
                secret_key: &self.secret_key,
                code,
            },
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const REQUIRED_FNS_FOR_TEST: &[&str] = &[
        "component_by_code",
        "sign_for_test",
        "parse_component_response",
    ];

    /// The real shipped script — these tests exercise its actual signing and
    /// parsing logic (no live API keys / network needed: both are pure
    /// functions given fixed inputs).
    fn script() -> DistributorScript {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/distributors/jlcpcb.lua");
        DistributorScript::load(&path, REQUIRED_FNS_FOR_TEST).expect("load jlcpcb.lua")
    }

    #[derive(Serialize)]
    struct SignReq<'a> {
        secret_key: &'a str,
        method: &'a str,
        path: &'a str,
        timestamp: &'a str,
        nonce: &'a str,
        body: &'a str,
    }

    #[test]
    fn signing_is_deterministic_and_matches_spec() {
        // Known HMAC-SHA256 of the exact string, base64. Recomputed here to lock
        // the string-to-sign format (METHOD\npath\nts\nnonce\nbody\n).
        let sig: String = script()
            .call(
                "sign_for_test",
                &SignReq {
                    secret_key: "topsecret",
                    method: "POST",
                    path: "/x",
                    timestamp: "1700000000",
                    nonce: "abc",
                    body: "{}",
                },
            )
            .unwrap();
        let expected = {
            use base64::Engine;
            use hmac::{Hmac, Mac};
            use sha2::Sha256;
            let mut mac = Hmac::<Sha256>::new_from_slice(b"topsecret").unwrap();
            mac.update(b"POST\n/x\n1700000000\nabc\n{}\n");
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
        };
        assert_eq!(sig, expected);
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
        let c: JlcpcbComponent = script().call("parse_component_response", &FIXTURE).unwrap();
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

    #[test]
    fn parses_absent_component_as_none() {
        let empty = r#"{"code":200,"data":[]}"#;
        let c: Option<JlcpcbComponent> = script().call("parse_component_response", &empty).unwrap();
        assert_eq!(c, None);
    }
}
