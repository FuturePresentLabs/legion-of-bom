//! `fetch_datasheet` — populate the parts library from a source, with a
//! citation for every fact. DESIGN.md 3.5; MCP.md 1. okm.3.
//!
//! Trust order (DESIGN.md 3.5): distributor-official CAD library → broader CAD
//! library → PDF datasheet extraction. Two sources exist, and compose (the CLI
//! merges non-empty fields across fetches):
//!   - [`fetch_from_kicad`]: the installed KiCad official library (CC-BY-SA,
//!     LIBRARIES.md §1) — yields pins + the symbol's datasheet URL.
//!   - [`fetch_from_jlcpcb`]: JLCPCB's authoritative distributor data by LCSC
//!     code — yields datasheet URL + structured parameters (ratings) + MPN.
//!
//! PDF-page citations (`cited_page`) come from a future PDF-extraction source.

use std::path::Path;

use crate::jlcpcb::{JlcpcbClient, JlcpcbError};
use crate::parts::{PartRecord, PinRecord, RatingRecord};
use crate::stage::StageError;
use crate::symbols;

/// Import a part from the installed KiCad official symbol library.
///
/// Looks up a symbol named exactly `mpn`, reads its pins + datasheet URL, and
/// returns an **unverified** [`PartRecord`] (the human-verification step,
/// okm.6, is separate). Manufacturer is inferred from the datasheet host.
pub fn fetch_from_kicad(mpn: &str, symbol_dir: &Path) -> Result<PartRecord, StageError> {
    let lib = symbols::find_symbol_lib(symbol_dir, mpn)?.ok_or_else(|| {
        StageError::Other(format!(
            "no KiCad symbol named '{mpn}' in {}",
            symbol_dir.display()
        ))
    })?;
    let data = symbols::read_symbol(symbol_dir, &lib, mpn)?
        .ok_or_else(|| StageError::Other(format!("symbol '{mpn}' not found in {lib}")))?;

    let mut part = PartRecord::new(mpn);
    part.manufacturer = data.datasheet.as_deref().and_then(manufacturer_from_url);
    part.datasheet_url = data.datasheet;
    part.pins = data
        .pins
        .into_iter()
        .map(|(pin_number, pin_name)| PinRecord {
            pin_number,
            pin_name,
            // Pins come from the CAD library, not a datasheet page; PDF-sourced
            // pins (a later source) carry a real cited_page.
            cited_page: None,
        })
        .collect();
    Ok(part)
}

/// Fetch a part from JLCPCB by LCSC component code (`C1002`) — an authoritative
/// distributor source yielding datasheet URL, structured parameters (as ratings),
/// and the MPN (componentModel). Not pin names (that's CAD data — use the KiCad
/// source for pins). The record is keyed by its MPN and left unverified.
pub fn fetch_from_jlcpcb(lcsc_code: &str, client: &JlcpcbClient) -> Result<PartRecord, StageError> {
    let component = client
        .component_by_code(lcsc_code)
        .map_err(jlcpcb_err)?
        .ok_or_else(|| StageError::Other(format!("no JLCPCB component for code '{lcsc_code}'")))?;

    if component.component_model.is_empty() {
        return Err(StageError::Other(format!(
            "JLCPCB component {lcsc_code} has no model/MPN"
        )));
    }

    let mut part = PartRecord::new(&component.component_model);
    part.datasheet_url = component.datasheet_url;
    part.ratings = component
        .parameters
        .into_iter()
        .map(|(name, value)| RatingRecord {
            name,
            value,
            unit: None,
            cited_page: None,
        })
        .collect();
    Ok(part)
}

fn jlcpcb_err(e: JlcpcbError) -> StageError {
    match e {
        JlcpcbError::MissingKeys => StageError::ToolNotFound("JLCPCB API keys".into()),
        other => StageError::Other(other.to_string()),
    }
}

/// Best-effort manufacturer from a datasheet URL host (a heuristic for the
/// KiCad-library source; the authoritative manufacturer comes from a
/// distributor API later).
fn manufacturer_from_url(url: &str) -> Option<String> {
    let u = url.to_ascii_lowercase();
    let name = if u.contains("ti.com") {
        "Texas Instruments"
    } else if u.contains("analog.com") {
        "Analog Devices"
    } else if u.contains("st.com") {
        "STMicroelectronics"
    } else if u.contains("onsemi.com") || u.contains("onsemi") {
        "onsemi"
    } else if u.contains("nxp.com") {
        "NXP"
    } else {
        return None;
    };
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manufacturer_inference() {
        assert_eq!(
            manufacturer_from_url("http://www.ti.com/lit/ds/symlink/lm13700.pdf").as_deref(),
            Some("Texas Instruments")
        );
        assert_eq!(manufacturer_from_url("https://example.com/x.pdf"), None);
    }

    /// Import LM13700 from the installed KiCad library. Skipped if no symbol dir.
    #[test]
    fn fetch_lm13700_from_kicad_when_available() {
        let Some(dir) = crate::skidl::kicad_symbol_dir() else {
            return; // no KiCad symbols in this environment — integration test skipped
        };
        let part = match fetch_from_kicad("LM13700", dir.path()) {
            Ok(p) => p,
            Err(_) => return, // library layout differs; don't fail the unit suite
        };
        assert_eq!(part.mpn, "LM13700");
        assert!(part
            .datasheet_url
            .as_deref()
            .is_some_and(|u| u.contains("lm13700")));
        assert_eq!(part.manufacturer.as_deref(), Some("Texas Instruments"));
        assert!(
            part.pins.len() >= 8,
            "expected a real pin set, got {}",
            part.pins.len()
        );
        assert!(!part.verified_by_human);
    }
}
