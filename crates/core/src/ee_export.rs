//! Data-only electrical evidence export for independent evaluators.
//!
//! This deliberately contains no verdicts: consumers such as PCBBench own the
//! checks. Lob only serializes the parsed circuit and raw ERC observations.

use crate::{generate_bom, CircuitSource};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const EE_SOURCE_SCHEMA: &str = "lob.ee-source.v1";

#[derive(Debug, Serialize)]
pub struct EeSourceExport {
    pub schema: &'static str,
    pub source_digest: String,
    pub circuit: String,
    pub parts: Vec<EePart>,
    pub nets: Vec<EeNet>,
    pub erc: Vec<EeErcFinding>,
    pub bom: Vec<EeBomLine>,
}

#[derive(Debug, Serialize)]
pub struct EePart {
    pub id: String,
    pub value: String,
    pub footprint: Option<String>,
    pub manufacturer_part_number: Option<String>,
    pub lcsc_part_number: Option<String>,
    pub ratings: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct EeNet {
    pub id: String,
    pub pins: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EeErcFinding {
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct EeBomLine {
    pub id: String,
    pub refdes: Vec<String>,
    pub value: String,
    pub footprint: Option<String>,
    pub manufacturer_part_number: Option<String>,
    pub lcsc_part_number: Option<String>,
}

pub fn export_ee_source(circuit: &dyn CircuitSource, erc_report: Option<&str>) -> EeSourceExport {
    export_ee_source_with_ratings(circuit, erc_report, &BTreeMap::new())
}

/// Export raw circuit evidence plus caller-sourced rating facts by reference.
///
/// Values remain strings with their units/qualifiers intact. This layer does
/// not interpret them or emit a verdict; independent evaluators own that work.
pub fn export_ee_source_with_ratings(
    circuit: &dyn CircuitSource,
    erc_report: Option<&str>,
    ratings_by_refdes: &BTreeMap<String, BTreeMap<String, String>>,
) -> EeSourceExport {
    let mut parts: Vec<_> = circuit
        .parts()
        .iter()
        .map(|part| EePart {
            id: part.refdes.0.clone(),
            value: part.value.clone(),
            footprint: part.footprint.clone(),
            manufacturer_part_number: part.mpn.clone(),
            lcsc_part_number: None,
            ratings: ratings_by_refdes
                .get(&part.refdes.0)
                .cloned()
                .unwrap_or_default(),
        })
        .collect();
    parts.sort_by(|a, b| a.id.cmp(&b.id));
    let mut nets: Vec<_> = circuit
        .nets()
        .iter()
        .map(|net| {
            let mut pins: Vec<_> = net
                .pins
                .iter()
                .map(|pin| format!("{}:{}", pin.refdes.0, pin.pin))
                .collect();
            pins.sort();
            EeNet {
                id: net.name.clone(),
                pins,
            }
        })
        .collect();
    nets.sort_by(|a, b| a.id.cmp(&b.id));
    let bom = generate_bom(circuit)
        .lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| EeBomLine {
            id: format!("BOM-{:04}", index + 1),
            refdes: line.refdes,
            value: line.value,
            footprint: line.footprint,
            manufacturer_part_number: line.mpn,
            lcsc_part_number: None,
        })
        .collect();
    let erc = erc_report
        .into_iter()
        .flat_map(str::lines)
        .filter_map(|line| {
            let (severity, message) = line
                .strip_prefix("ERC ERROR:")
                .map(|m| ("error", m))
                .or_else(|| line.strip_prefix("ERC WARNING:").map(|m| ("warning", m)))?;
            Some(EeErcFinding {
                severity: severity.into(),
                message: message.trim().into(),
            })
        })
        .collect();
    let canonical =
        serde_json::to_vec(&(circuit.name(), &parts, &nets, &erc, &bom)).expect("serializable");
    let source_digest = format!("sha256:{:x}", Sha256::digest(canonical));
    EeSourceExport {
        schema: EE_SOURCE_SCHEMA,
        source_digest,
        circuit: circuit.name().into(),
        parts,
        nets,
        erc,
        bom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Circuit, Net, Part, PinRef};

    #[test]
    fn export_is_sorted_stable_and_contains_no_verdict() {
        let mut circuit = Circuit::new("x");
        circuit.parts.push(Part::new("R2", "2k"));
        circuit.parts.push(Part::new("R1", "1k").with_mpn("RC-1K"));
        circuit.nets.push(Net::new(
            "N",
            vec![PinRef::new("R2", "1"), PinRef::new("R1", "2")],
        ));
        let export = export_ee_source(&circuit, Some("ERC ERROR: broken\n"));
        assert_eq!(export.parts[0].id, "R1");
        assert_eq!(export.nets[0].pins, ["R1:2", "R2:1"]);
        assert_eq!(export.erc[0].severity, "error");
        let json = serde_json::to_string(&export).unwrap();
        assert!(!json.contains("passed"));
        assert!(!json.contains("verdict"));
    }

    #[test]
    fn sourced_ratings_are_raw_and_part_of_the_digest() {
        let mut circuit = Circuit::new("i2c");
        circuit.parts.push(Part::new("U1", "controller"));
        let plain = export_ee_source(&circuit, None);
        let rated = export_ee_source_with_ratings(
            &circuit,
            None,
            &BTreeMap::from([(
                "U1".into(),
                BTreeMap::from([
                    ("iol_at_vol_max_a".into(), "0.003".into()),
                    ("vol_max_v".into(), "0.4".into()),
                ]),
            )]),
        );
        assert_eq!(rated.parts[0].ratings["vol_max_v"], "0.4");
        assert_ne!(plain.source_digest, rated.source_digest);
    }
}
