//! Bounded, replayable circuit operations for RLCD.
//!
//! This is an interface, not a topology generator: the caller chooses a
//! sequence of explicit edits and replay validates every reference, net and
//! pin ownership before producing the generic SKiDL emitter IR.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ooda::{Answer, Client, Criteria, Question, Request, Trace};
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
                    let fields = part.netlist_fields();
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

const MAX_PLAN_STEPS: usize = 512;
const NET_NAMES: &[&str] = &[
    "GND", "+3V3", "+5V", "+9V", "+12V", "-12V", "VREF", "IN", "OUT", "TRIGGER", "ACCENT", "CLOCK",
    "RESET", "MIX", "NOISE", "ENV", "OSC", "N001", "N002", "N003", "N004", "N005", "N006", "N007",
    "N008", "N009", "N010", "N011", "N012", "N013", "N014", "N015", "N016", "N017", "N018", "N019",
    "N020", "N021", "N022", "N023", "N024", "N025", "N026", "N027", "N028", "N029", "N030", "N031",
    "N032",
];

#[derive(Debug, thiserror::Error)]
pub enum PlanGenerationError {
    #[error(transparent)]
    Decision(#[from] ooda::Error),
    #[error("planner returned {answer:?} for bounded choice {key}")]
    WrongAnswer { key: String, answer: Answer },
    #[error("planner selected unknown option {choice:?} for {key}")]
    UnknownChoice { key: String, choice: String },
    #[error("catalog part {mpn} has no usable physical pins")]
    NoPins { mpn: String },
    #[error("circuit planner exhausted {MAX_PLAN_STEPS} bounded operations without finishing")]
    StepLimit,
    #[error(transparent)]
    Replay(#[from] PlanError),
}

#[derive(Clone)]
struct PlannedPart {
    mpn: String,
    pins: Vec<(PinSelector, String)>,
}

/// Let RLCD author an arbitrary graph through bounded, currently-valid
/// operations. Rust supplies the legal alphabet and validates replay; it never
/// chooses a component, connection, topology, or completion point.
pub fn generate_plan(
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
    catalog: &Catalog,
    symbol_dir: &Path,
) -> Result<CircuitPlan, PlanGenerationError> {
    let mut plan = CircuitPlan::new(brief, catalog, Vec::new());
    let mut parts = BTreeMap::<String, PlannedPart>::new();
    let mut nets = BTreeSet::<String>::new();
    let mut claimed = BTreeSet::<(String, PinSelector)>::new();

    for step in 0..MAX_PLAN_STEPS {
        let finishable = plan.replay(catalog, symbol_dir).is_ok();
        let has_free_pins = parts.iter().any(|(reference, part)| {
            part.pins
                .iter()
                .any(|(pin, _)| !claimed.contains(&(reference.clone(), pin.clone())))
        });
        let mut actions = vec![("add_part", "Add one exact catalog part")];
        if NET_NAMES.iter().any(|name| !nets.contains(*name)) {
            actions.push(("create_net", "Create one named electrical net"));
        }
        if has_free_pins && !nets.is_empty() {
            actions.push(("connect", "Connect one unassigned physical pin to a net"));
        }
        if has_free_pins {
            actions.push(("no_connect", "Explicitly mark one physical pin unconnected"));
        }
        if finishable {
            actions.push(("finish", "Finish and submit this replay-valid circuit"));
        }
        let action = decide(
            client,
            trace,
            brief,
            &plan,
            step,
            "action",
            "Choose the next circuit construction operation. Finish only when the complete circuit satisfies the brief.",
            actions.into_iter().map(|(key, description)| (key.to_owned(), description.to_owned())).collect(),
        )?;
        match action.as_str() {
            "finish" if finishable => {
                plan.replay(catalog, symbol_dir)?;
                return Ok(plan);
            }
            "add_part" => {
                let options: Criteria = catalog
                    .parts
                    .iter()
                    .map(|part| {
                        (
                            part.mpn.clone(),
                            format!(
                                "{}; manufacturer {}; LCSC {}",
                                part.summary,
                                part.manufacturer,
                                part.lcsc.as_deref().unwrap_or("not recorded")
                            ),
                        )
                    })
                    .collect();
                let mpn = decide(
                    client,
                    trace,
                    brief,
                    &plan,
                    step,
                    "part",
                    "Choose the exact catalog part to add.",
                    options,
                )?;
                let catalog_part =
                    catalog
                        .part(&mpn)
                        .ok_or_else(|| PlanGenerationError::UnknownChoice {
                            key: format!("plan_{step}_part"),
                            choice: mpn.clone(),
                        })?;
                let pins = physical_pins(catalog_part, symbol_dir)?;
                let reference = next_reference(&parts, reference_prefix(catalog_part));
                parts.insert(
                    reference.clone(),
                    PlannedPart {
                        mpn: mpn.clone(),
                        pins,
                    },
                );
                plan.operations
                    .push(CircuitOp::AddCatalogPart { reference, mpn });
            }
            "create_net" => {
                let options: Criteria = NET_NAMES
                    .iter()
                    .filter(|name| !nets.contains(**name))
                    .map(|name| ((*name).to_owned(), format!("Create the {name} net")))
                    .collect();
                let name = decide(
                    client,
                    trace,
                    brief,
                    &plan,
                    step,
                    "net",
                    "Choose the next net to create.",
                    options,
                )?;
                nets.insert(name.clone());
                plan.operations.push(CircuitOp::CreateNet { name });
            }
            "connect" | "no_connect" => {
                let reference_options: Criteria = parts
                    .iter()
                    .filter(|(reference, part)| {
                        part.pins
                            .iter()
                            .any(|(pin, _)| !claimed.contains(&((*reference).clone(), pin.clone())))
                    })
                    .map(|(reference, part)| {
                        (reference.clone(), format!("{reference}: {}", part.mpn))
                    })
                    .collect();
                let reference = decide(
                    client,
                    trace,
                    brief,
                    &plan,
                    step,
                    "reference",
                    "Choose the component whose physical pin will be assigned.",
                    reference_options,
                )?;
                let part =
                    parts
                        .get(&reference)
                        .ok_or_else(|| PlanGenerationError::UnknownChoice {
                            key: format!("plan_{step}_reference"),
                            choice: reference.clone(),
                        })?;
                let pin_options: Criteria = part
                    .pins
                    .iter()
                    .filter(|(pin, _)| !claimed.contains(&(reference.clone(), pin.clone())))
                    .map(|(pin, label)| {
                        (pin_key(&reference, pin), format!("{reference} pin {label}"))
                    })
                    .collect();
                let selected = decide(
                    client,
                    trace,
                    brief,
                    &plan,
                    step,
                    "pin",
                    "Choose the physical pin for this operation.",
                    pin_options,
                )?;
                let pin = part
                    .pins
                    .iter()
                    .map(|(pin, _)| pin)
                    .find(|pin| pin_key(&reference, pin) == selected)
                    .cloned()
                    .ok_or_else(|| PlanGenerationError::UnknownChoice {
                        key: format!("plan_{step}_pin"),
                        choice: selected,
                    })?;
                claimed.insert((reference.clone(), pin.clone()));
                if action == "connect" {
                    let options: Criteria = nets
                        .iter()
                        .map(|net| (net.clone(), format!("Connect to {net}")))
                        .collect();
                    let net = decide(
                        client,
                        trace,
                        brief,
                        &plan,
                        step,
                        "target_net",
                        "Choose the destination net for this pin.",
                        options,
                    )?;
                    plan.operations.push(CircuitOp::Connect {
                        net,
                        reference,
                        pin,
                    });
                } else {
                    plan.operations
                        .push(CircuitOp::MarkNoConnect { reference, pin });
                }
            }
            choice => {
                return Err(PlanGenerationError::UnknownChoice {
                    key: format!("plan_{step}_action"),
                    choice: choice.to_owned(),
                })
            }
        }
    }
    Err(PlanGenerationError::StepLimit)
}

#[allow(clippy::too_many_arguments)]
fn decide(
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
    plan: &CircuitPlan,
    step: usize,
    suffix: &str,
    instructions: &str,
    options: Criteria,
) -> Result<String, PlanGenerationError> {
    let key = format!("plan_{step}_{suffix}");
    let request = Request::single(
        serde_json::json!({"brief": brief, "current_plan": plan, "step": step}),
        &key,
        Question::choice(instructions, options.clone()),
    );
    let outcome = client.decide(&request)?;
    let answer = outcome.recorded_answer(&key, trace)?.clone();
    let Answer::Choice { choice, .. } = answer else {
        return Err(PlanGenerationError::WrongAnswer { key, answer });
    };
    if !options.contains_key(&choice) {
        return Err(PlanGenerationError::UnknownChoice { key, choice });
    }
    Ok(choice)
}

fn physical_pins(
    part: &crate::catalog::CatalogPart,
    symbol_dir: &Path,
) -> Result<Vec<(PinSelector, String)>, PlanGenerationError> {
    let (pins, alternates) = part
        .pins(symbol_dir)
        .map_err(|_| PlanGenerationError::NoPins {
            mpn: part.mpn.clone(),
        })?;
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for (number, name) in pins.into_iter().chain(alternates) {
        if !seen.insert(number.clone()) {
            continue;
        }
        let selector = number
            .parse::<u32>()
            .map(PinSelector::Number)
            .unwrap_or_else(|_| PinSelector::Name(name.clone()));
        result.push((selector, format!("{number}/{name}")));
    }
    if result.is_empty() {
        Err(PlanGenerationError::NoPins {
            mpn: part.mpn.clone(),
        })
    } else {
        Ok(result)
    }
}

fn reference_prefix(part: &crate::catalog::CatalogPart) -> &'static str {
    let symbol = match &part.symbol {
        PartSymbol::Kicad { kicad } => kicad.as_str(),
        PartSymbol::Inline { .. } => "",
    };
    let summary = part.summary.to_ascii_lowercase();
    if symbol.contains("Device:R") || summary.contains("resistor") {
        "R"
    } else if symbol.contains("Device:C") || summary.contains("capacitor") {
        "C"
    } else if symbol.contains("Device:L") || summary.contains("inductor") {
        "L"
    } else if part.provides.iter().any(|role| role == "connector") {
        "J"
    } else if part.provides.iter().any(|role| role == "crystal") {
        "Y"
    } else {
        "U"
    }
}

fn next_reference(parts: &BTreeMap<String, PlannedPart>, prefix: &str) -> String {
    (1..)
        .map(|index| format!("{prefix}{index}"))
        .find(|candidate| !parts.contains_key(candidate))
        .unwrap()
}

fn pin_key(reference: &str, pin: &PinSelector) -> String {
    match pin {
        PinSelector::Number(number) => format!("{reference}:n{number}"),
        PinSelector::Name(name) => format!("{reference}:s{name}"),
        PinSelector::NameAt { name, index } => format!("{reference}:s{name}:{index}"),
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
    use crate::catalog::{CatalogPart, Cite, InlinePin, Param, PowerIntent, PowerRole};

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
                power: Some(PowerIntent {
                    role: PowerRole::Load,
                    input_net: Some("+5V".into()),
                    output_net: None,
                    cite: Cite::Reading {
                        reading: "fixture load".into(),
                        page: None,
                        confirmed_by: Some("test".into()),
                    },
                    input_voltage_v: None,
                    output_voltage_v: None,
                    output_current_a: None,
                    load_current_a: Some(Param {
                        value: 0.001,
                        cite: Cite::Reading {
                            reading: "fixture current".into(),
                            page: None,
                            confirmed_by: Some("test".into()),
                        },
                    }),
                    dropout_v: None,
                }),
                conduction: vec![],
                control_outputs: vec![],
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
                CircuitOp::CreateNet { name: "+5V".into() },
                CircuitOp::Connect {
                    net: "+5V".into(),
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
        assert!(first.contains(r#"r1.fields["Power.Role"] = "load""#));
        assert!(first.contains(r#"r1.fields["Power.InputNet"] = "+5V""#));
        assert!(first.contains(r#"r1.fields["Power.LoadCurrentA"] = "0.001""#));
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
                net: "+5V".into(),
                reference: "MISSING".into(),
                pin: PinSelector::Number(2),
            },
            CircuitOp::Connect {
                net: "+5V".into(),
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
            net: "+5V".into(),
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
    fn rlcd_authors_the_operation_sequence_and_topology() {
        let client = ooda::ScriptedClient::new([
            r#"{"answers":{"plan_0_action":{"type":"choice","choice":"add_part","confidence":0.99}}}"#,
            r#"{"answers":{"plan_0_part":{"type":"choice","choice":"RC0603FR-0710KL","confidence":0.99}}}"#,
            r#"{"answers":{"plan_1_action":{"type":"choice","choice":"create_net","confidence":0.99}}}"#,
            r#"{"answers":{"plan_1_net":{"type":"choice","choice":"+5V","confidence":0.99}}}"#,
            r#"{"answers":{"plan_2_action":{"type":"choice","choice":"connect","confidence":0.99}}}"#,
            r#"{"answers":{"plan_2_reference":{"type":"choice","choice":"R1","confidence":0.99}}}"#,
            r#"{"answers":{"plan_2_pin":{"type":"choice","choice":"R1:n1","confidence":0.99}}}"#,
            r#"{"answers":{"plan_2_target_net":{"type":"choice","choice":"+5V","confidence":0.99}}}"#,
            r#"{"answers":{"plan_3_action":{"type":"choice","choice":"no_connect","confidence":0.99}}}"#,
            r#"{"answers":{"plan_3_reference":{"type":"choice","choice":"R1","confidence":0.99}}}"#,
            r#"{"answers":{"plan_3_pin":{"type":"choice","choice":"R1:n2","confidence":0.99}}}"#,
            r#"{"answers":{"plan_4_action":{"type":"choice","choice":"finish","confidence":0.99}}}"#,
        ]);
        let mut trace = Trace::new();
        let generated = generate_plan(
            &client,
            &mut trace,
            "a divider whose topology RLCD chooses",
            &catalog(),
            Path::new("."),
        )
        .unwrap();
        assert_eq!(generated.operations.len(), 4);
        assert!(matches!(
            generated.operations[0],
            CircuitOp::AddCatalogPart { .. }
        ));
        assert!(matches!(
            generated.operations[1],
            CircuitOp::CreateNet { .. }
        ));
        assert!(matches!(generated.operations[2], CircuitOp::Connect { .. }));
        assert!(matches!(
            generated.operations[3],
            CircuitOp::MarkNoConnect { .. }
        ));
        assert_eq!(trace.records().len(), 12);
        generated.replay(&catalog(), Path::new(".")).unwrap();
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
