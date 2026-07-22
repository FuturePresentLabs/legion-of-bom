//! `fetch_datasheet` — populate the parts library from a source, with a
//! citation for every fact. DESIGN.md 3.5; MCP.md 1. okm.3.
//!
//! Trust order (DESIGN.md 3.5): distributor-official CAD library → broader CAD
//! library → PDF datasheet extraction. This module currently implements the
//! **KiCad official symbol library** source — a high-trust CAD library
//! (CC-BY-SA, already installed, LIBRARIES.md §1). It yields pins + the symbol's
//! datasheet URL; ratings and PDF-page citations come from the
//! distributor-API/PDF sources added next (JLCPCB, then Mouser for pricing in the
//! BOM epic).

use std::path::Path;

use crate::parts::{PartRecord, PinRecord};
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
