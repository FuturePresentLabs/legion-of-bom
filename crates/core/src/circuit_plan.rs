//! Bounded, replayable circuit operations for RLCD.
//!
//! This is an interface, not a topology generator: the caller chooses a
//! sequence of explicit edits and replay validates every reference, net and
//! pin ownership before producing the generic SKiDL emitter IR.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::catalog::{Catalog, PartSymbol};
use crate::skidl_emit::{Circuit, EmitPart, PinRef, SymbolSrc};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "by", content = "value", rename_all = "snake_case")]
pub enum PinSelector {
    Name(String),
    NameAt { name: String, index: usize },
    Number(u32),
}

impl From<&PinSelector> for PinRef {
    fn from(value: &PinSelector) -> Self {
        match value {
            PinSelector::Name(name) => Self::Name(name.clone()),
            PinSelector::NameAt { name, index } => Self::NameAt(name.clone(), *index),
            PinSelector::Number(number) => Self::Num(*number),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum CircuitOp {
    AddCatalogPart {
        reference: String,
        mpn: String,
    },
    CreateNet {
        name: String,
    },
    Connect {
        net: String,
        reference: String,
        pin: PinSelector,
    },
    MarkNoConnect {
        reference: String,
        pin: PinSelector,
    },
    SetValue {
        reference: String,
        value: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CircuitPlan {
    pub schema: String,
    pub title: String,
    /// Fingerprint of the exact catalog whose facts the operations reference.
    pub catalog: String,
    pub operations: Vec<CircuitOp>,
}

impl CircuitPlan {
    pub const SCHEMA: &'static str = "lob.circuit-operations.v1";

    pub fn new(title: impl Into<String>, catalog: &Catalog, operations: Vec<CircuitOp>) -> Self {
        Self {
            schema: Self::SCHEMA.into(),
            title: title.into(),
            catalog: catalog.fingerprint(),
            operations,
        }
    }

    pub fn replay(&self, catalog: &Catalog, symbol_dir: &Path) -> Result<Circuit, PlanError> {
        if self.schema != Self::SCHEMA {
            return Err(PlanError::Schema(self.schema.clone()));
        }
        let actual_catalog = catalog.fingerprint();
        if self.catalog != actual_catalog {
            return Err(PlanError::StaleCatalog {
                expected: self.catalog.clone(),
                actual: actual_catalog,
            });
        }
        nonempty("title", &self.title)?;
        let mut circuit = Circuit {
            title: self.title.clone(),
            ..Circuit::default()
        };
        let mut part_indexes = BTreeMap::<String, usize>::new();
        let mut part_pins = BTreeMap::<String, Vec<(String, String)>>::new();
        let mut claimed_pins = BTreeSet::<(String, PinSelector)>::new();

        for (index, operation) in self.operations.iter().enumerate() {
            let fail = |message| PlanError::Operation { index, message };
            match operation {
                CircuitOp::AddCatalogPart { reference, mpn } => {
                    nonempty("reference", reference).map_err(|e| fail(e.to_string()))?;
                    if part_indexes.contains_key(reference) {
                        return Err(fail(format!("duplicate part {reference}")));
                    }
                    let part = catalog
                        .part(mpn)
                        .ok_or_else(|| fail(format!("unknown catalog MPN {mpn}")))?;
                    let (mut pins, alternates) = part
                        .pins(symbol_dir)
                        .map_err(|error| fail(format!("cannot resolve pins for {mpn}: {error}")))?;
                    pins.extend(alternates);
                    let symbol = match &part.symbol {
                        PartSymbol::Kicad { kicad } => {
                            let (library, name) = kicad
                                .split_once(':')
                                .ok_or_else(|| fail(format!("invalid KiCad symbol {kicad}")))?;
                            SymbolSrc::Kicad(library.into(), name.into())
                        }
                        PartSymbol::Inline { pins } => SymbolSrc::Inline(
                            pins.iter()
                                .map(|pin| (pin.number.clone(), pin.name.clone(), pin.io.clone()))
                                .collect(),
                        ),
                    };
                    let mut fields = BTreeMap::from([("MPN".into(), part.mpn.clone())]);
                    if let Some(lcsc) = &part.lcsc {
                        fields.insert("LCSC".into(), lcsc.clone());
                    }
                    if part.sim_excluded {
                        fields.insert("Sim.Enable".into(), "0".into());
                    }
                    part_indexes.insert(reference.clone(), circuit.parts.len());
                    part_pins.insert(reference.clone(), pins);
                    circuit.parts.push(EmitPart {
                        reference: reference.clone(),
                        symbol,
                        value: part.mpn.clone(),
                        footprint: part.footprint.clone(),
                        fields,
                    });
                }
                CircuitOp::CreateNet { name } => {
                    nonempty("net", name).map_err(|e| fail(e.to_string()))?;
                    if circuit.nets.insert(name.clone(), Vec::new()).is_some() {
                        return Err(fail(format!("duplicate net {name}")));
                    }
                }
                CircuitOp::Connect {
                    net,
                    reference,
                    pin,
                } => {
                    require_part(&part_indexes, reference).map_err(&fail)?;
                    validate_pin(&part_pins, reference, pin).map_err(&fail)?;
                    if !circuit.nets.contains_key(net) {
                        return Err(fail(format!("unknown net {net}")));
                    }
                    let key = (reference.clone(), pin.clone());
                    if !claimed_pins.insert(key) {
                        return Err(fail(format!("pin {reference}:{pin:?} already assigned")));
                    }
                    circuit.connect(net, reference, pin.into());
                }
                CircuitOp::MarkNoConnect { reference, pin } => {
                    require_part(&part_indexes, reference).map_err(&fail)?;
                    validate_pin(&part_pins, reference, pin).map_err(&fail)?;
                    let key = (reference.clone(), pin.clone());
                    if !claimed_pins.insert(key.clone()) {
                        return Err(fail(format!("pin {reference}:{pin:?} already assigned")));
                    }
                    circuit.no_connects.insert((reference.clone(), pin.into()));
                }
                CircuitOp::SetValue { reference, value } => {
                    let part = part_mut(&mut circuit, &part_indexes, reference).map_err(&fail)?;
                    nonempty("value", value).map_err(|e| fail(e.to_string()))?;
                    part.value = value.clone();
                }
            }
        }
        for (reference, pins) in &part_pins {
            let mut physical_pins = BTreeSet::new();
            for (number, name) in pins {
                if !physical_pins.insert(number) {
                    continue;
                }
                let covered = claimed_pins.iter().any(|(claimed_ref, selector)| {
                    claimed_ref == reference && selector_covers(selector, number, name, pins)
                });
                if !covered {
                    return Err(PlanError::UnassignedPin {
                        reference: reference.clone(),
                        number: number.clone(),
                        name: name.clone(),
                    });
                }
            }
        }
        if circuit.parts.is_empty() {
            return Err(PlanError::Empty("parts"));
        }
        Ok(circuit)
    }
}

fn selector_covers(
    selector: &PinSelector,
    number: &str,
    name: &str,
    pins: &[(String, String)],
) -> bool {
    match selector {
        PinSelector::Number(selected) => number == selected.to_string(),
        PinSelector::Name(selected) => {
            name == selected
                || pins.iter().any(|(candidate_number, candidate)| {
                    candidate_number == number && candidate == selected
                })
        }
        PinSelector::NameAt {
            name: selected,
            index,
        } => {
            name == selected
                && pins
                    .iter()
                    .filter(|(_, candidate)| candidate == selected)
                    .nth(*index)
                    .is_some_and(|(selected_number, _)| selected_number == number)
        }
    }
}

fn nonempty(kind: &'static str, value: &str) -> Result<(), PlanError> {
    if value.trim().is_empty() {
        Err(PlanError::Empty(kind))
    } else {
        Ok(())
    }
}

fn require_part(parts: &BTreeMap<String, usize>, reference: &str) -> Result<usize, String> {
    parts
        .get(reference)
        .copied()
        .ok_or_else(|| format!("unknown part {reference}"))
}

fn validate_pin(
    parts: &BTreeMap<String, Vec<(String, String)>>,
    reference: &str,
    selector: &PinSelector,
) -> Result<(), String> {
    let pins = parts
        .get(reference)
        .ok_or_else(|| format!("unknown part {reference}"))?;
    let valid = match selector {
        PinSelector::Number(number) => pins.iter().any(|(pin, _)| pin == &number.to_string()),
        PinSelector::Name(name) => pins.iter().any(|(_, pin_name)| pin_name == name),
        PinSelector::NameAt { name, index } => {
            pins.iter().filter(|(_, pin_name)| pin_name == name).count() > *index
        }
    };
    if valid {
        Ok(())
    } else {
        Err(format!("part {reference} has no pin matching {selector:?}"))
    }
}

fn part_mut<'a>(
    circuit: &'a mut Circuit,
    parts: &BTreeMap<String, usize>,
    reference: &str,
) -> Result<&'a mut EmitPart, String> {
    let index = require_part(parts, reference)?;
    Ok(&mut circuit.parts[index])
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("unsupported circuit-operation schema {0:?}")]
    Schema(String),
    #[error("circuit plan catalog changed (plan {expected}, current {actual})")]
    StaleCatalog { expected: String, actual: String },
    #[error("{0} must not be empty")]
    Empty(&'static str),
    #[error("part {reference} pin {number} ({name}) is neither connected nor marked no-connect")]
    UnassignedPin {
        reference: String,
        number: String,
        name: String,
    },
    #[error("operation {index}: {message}")]
    Operation { index: usize, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CatalogPart, Cite, InlinePin};

    fn catalog() -> Catalog {
        Catalog {
            parts: vec![CatalogPart {
                mpn: "RC0603FR-0710KL".into(),
                manufacturer: "Yageo".into(),
                lcsc: Some("C25804".into()),
                summary: "10k resistor".into(),
                symbol: PartSymbol::Inline {
                    pins: vec![
                        InlinePin {
                            number: "1".into(),
                            name: "1".into(),
                            io: "passive".into(),
                            cite: Cite::Reading {
                                reading: "package terminal".into(),
                                page: None,
                                confirmed_by: Some("fixture".into()),
                            },
                        },
                        InlinePin {
                            number: "2".into(),
                            name: "2".into(),
                            io: "passive".into(),
                            cite: Cite::Reading {
                                reading: "package terminal".into(),
                                page: None,
                                confirmed_by: Some("fixture".into()),
                            },
                        },
                    ],
                },
                footprint: "Resistor_SMD:R_0603_1608Metric".into(),
                datasheet: None,
                provides: vec![],
                params: BTreeMap::new(),
                interfaces: vec![],
                support: vec![],
                sim_excluded: false,
            }],
            ..Catalog::default()
        }
    }

    fn plan() -> CircuitPlan {
        let catalog = catalog();
        CircuitPlan::new(
            "divider",
            &catalog,
            vec![
                CircuitOp::AddCatalogPart {
                    reference: "R1".into(),
                    mpn: "RC0603FR-0710KL".into(),
                },
                CircuitOp::SetValue {
                    reference: "R1".into(),
                    value: "10k".into(),
                },
                CircuitOp::CreateNet { name: "VIN".into() },
                CircuitOp::Connect {
                    net: "VIN".into(),
                    reference: "R1".into(),
                    pin: PinSelector::Number(1),
                },
                CircuitOp::MarkNoConnect {
                    reference: "R1".into(),
                    pin: PinSelector::Number(2),
                },
            ],
        )
    }

    #[test]
    fn serde_replay_is_deterministic_and_emits_explicit_no_connect() {
        let encoded = serde_json::to_string(&plan()).unwrap();
        let decoded: CircuitPlan = serde_json::from_str(&encoded).unwrap();
        let first = decoded
            .replay(&catalog(), Path::new("."))
            .unwrap()
            .to_skidl();
        let second = decoded
            .replay(&catalog(), Path::new("."))
            .unwrap()
            .to_skidl();
        assert_eq!(first, second);
        assert!(first.contains("r1.p[2] += builtins.NC"));
        assert!(first.contains(r#"r1.fields["LCSC"] = "C25804""#));
    }

    #[test]
    fn mutations_fail_loudly() {
        let cases = [
            CircuitOp::Connect {
                net: "MISSING".into(),
                reference: "R1".into(),
                pin: PinSelector::Number(2),
            },
            CircuitOp::Connect {
                net: "VIN".into(),
                reference: "MISSING".into(),
                pin: PinSelector::Number(2),
            },
            CircuitOp::Connect {
                net: "VIN".into(),
                reference: "R1".into(),
                pin: PinSelector::Number(1),
            },
        ];
        for mutation in cases {
            let mut candidate = plan();
            candidate.operations.push(mutation);
            assert!(matches!(
                candidate.replay(&catalog(), Path::new(".")),
                Err(PlanError::Operation { .. })
            ));
        }
    }

    #[test]
    fn nonexistent_pin_is_rejected_before_emission() {
        let mut candidate = plan();
        candidate.operations.push(CircuitOp::Connect {
            net: "VIN".into(),
            reference: "R1".into(),
            pin: PinSelector::Number(99),
        });
        assert!(matches!(
            candidate.replay(&catalog(), Path::new(".")),
            Err(PlanError::Operation { index: 5, .. })
        ));
    }

    #[test]
    fn replay_refuses_catalog_drift() {
        let mut changed = catalog();
        changed.parts[0].footprint = "different".into();
        assert!(matches!(
            plan().replay(&changed, Path::new(".")),
            Err(PlanError::StaleCatalog { .. })
        ));
    }

    #[test]
    fn every_catalog_pin_requires_an_explicit_disposition() {
        let mut candidate = plan();
        candidate.operations.pop();
        assert_eq!(
            candidate.replay(&catalog(), Path::new(".")),
            Err(PlanError::UnassignedPin {
                reference: "R1".into(),
                number: "2".into(),
                name: "2".into(),
            })
        );
    }
}
