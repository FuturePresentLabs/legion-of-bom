//! Deterministic schematic connectivity and power-intent proof.
//!
//! Pin behavior comes from the source netlist's electrical types. Power limits
//! come only from explicit `Power.*` component fields. Missing declarations are
//! reported as unknown; they are never replaced with remembered device data.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::model::{is_supply_rail, Part, PinElectricalType, RefDes};
use crate::source::CircuitSource;
use crate::standards::{CheckResult, Verdict, Verifiable};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofState {
    Passed,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProofFinding {
    pub check: &'static str,
    pub state: ProofState,
    pub subject: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectivityProof {
    pub findings: Vec<ProofFinding>,
    pub typed_pins: usize,
    pub untyped_pins: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PowerRailProof {
    pub net: String,
    pub voltage_v: Option<f64>,
    pub sources: Vec<String>,
    pub loads: Vec<String>,
    pub reachable: bool,
    pub output_current_a: Option<f64>,
    pub declared_load_a: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PowerTreeProof {
    pub rails: Vec<PowerRailProof>,
    pub findings: Vec<ProofFinding>,
}

fn finding(
    check: &'static str,
    state: ProofState,
    subject: impl Into<String>,
    detail: impl Into<String>,
) -> ProofFinding {
    ProofFinding {
        check,
        state,
        subject: subject.into(),
        detail: detail.into(),
    }
}

#[must_use]
pub fn prove_connectivity(circuit: &dyn CircuitSource) -> ConnectivityProof {
    let mut findings = Vec::new();
    let mut endpoints: BTreeMap<(RefDes, String), Vec<&str>> = BTreeMap::new();
    let mut typed_pins = 0;
    let mut untyped_pins = 0;

    for net in circuit.nets() {
        for pin in &net.pins {
            endpoints
                .entry((pin.refdes.clone(), pin.pin.clone()))
                .or_default()
                .push(&net.name);
            if pin.electrical_type.is_some() {
                typed_pins += 1;
            } else {
                untyped_pins += 1;
            }
        }

        let hard_drivers: Vec<_> = net
            .pins
            .iter()
            .filter(|pin| {
                matches!(
                    pin.electrical_type,
                    Some(PinElectricalType::Output | PinElectricalType::PowerOutput)
                )
            })
            .map(|pin| format!("{}.{}", pin.refdes, pin.pin))
            .collect();
        if hard_drivers.len() > 1 {
            findings.push(finding(
                "driver_conflict",
                ProofState::Failed,
                &net.name,
                format!("multiple non-tristate drivers: {}", hard_drivers.join(", ")),
            ));
        }

        for pin in &net.pins {
            match pin.electrical_type {
                Some(PinElectricalType::NoConnect) if net.pins.len() > 1 => findings.push(finding(
                    "no_connect",
                    ProofState::Failed,
                    format!("{}.{}", pin.refdes, pin.pin),
                    format!("no-connect pin is attached to {}", net.name),
                )),
                Some(PinElectricalType::Input) if net.pins.len() == 1 => findings.push(finding(
                    "floating_input",
                    ProofState::Failed,
                    format!("{}.{}", pin.refdes, pin.pin),
                    format!("input is the only endpoint on {}", net.name),
                )),
                _ => {}
            }
        }
    }

    for ((refdes, pin), nets) in endpoints {
        let unique: BTreeSet<_> = nets.into_iter().collect();
        if unique.len() > 1 {
            findings.push(finding(
                "endpoint_uniqueness",
                ProofState::Failed,
                format!("{refdes}.{pin}"),
                format!(
                    "pin appears on multiple nets: {}",
                    unique.into_iter().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
    }

    if typed_pins == 0 {
        findings.push(finding(
            "pin_type_coverage",
            ProofState::Unknown,
            circuit.name(),
            "source supplied no electrical pin types; driver and floating-input proof is unavailable",
        ));
    } else if untyped_pins > 0 {
        findings.push(finding(
            "pin_type_coverage",
            ProofState::Unknown,
            circuit.name(),
            format!("{untyped_pins} connected pin(s) have no electrical type"),
        ));
    }
    if findings.is_empty() {
        findings.push(finding(
            "connectivity",
            ProofState::Passed,
            circuit.name(),
            format!("{typed_pins} typed pin attachment(s) are conflict-free"),
        ));
    }
    ConnectivityProof {
        findings,
        typed_pins,
        untyped_pins,
    }
}

fn field<'a>(part: &'a Part, name: &str) -> Option<&'a str> {
    part.fields
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn number(part: &Part, name: &str) -> Option<f64> {
    field(part, name)?
        .trim()
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite() && *v >= 0.0)
}

fn role(part: &Part) -> Option<&str> {
    field(part, "Power.Role")
}

fn part<'a>(circuit: &'a dyn CircuitSource, refdes: &RefDes) -> Option<&'a Part> {
    circuit.parts().iter().find(|part| &part.refdes == refdes)
}

fn rail_voltage(name: &str) -> Option<f64> {
    let mut text = name.trim().to_ascii_uppercase();
    let sign = if text.starts_with('-') {
        text.remove(0);
        -1.0
    } else {
        text = text.trim_start_matches('+').to_string();
        1.0
    };
    let head = text.split(['_', '-']).next()?;
    let (whole, fraction) = head.split_once('V')?;
    if whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !fraction.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value: f64 = if fraction.is_empty() {
        whole.parse().ok()?
    } else {
        format!("{whole}.{fraction}").parse().ok()?
    };
    Some(sign * value)
}

fn is_input_pin(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_uppercase().as_str(),
        "VIN" | "VI" | "IN" | "VBUS"
    )
}

fn is_output_pin(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_uppercase().as_str(),
        "VOUT" | "VO" | "OUT"
    )
}

#[must_use]
pub fn prove_power_tree(circuit: &dyn CircuitSource) -> PowerTreeProof {
    let rail_nets: Vec<_> = circuit
        .nets()
        .iter()
        .filter(|net| is_supply_rail(&net.name))
        .collect();
    let mut parents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut roots = BTreeSet::new();
    let mut findings = Vec::new();

    for component in circuit.parts() {
        let attached = |want: fn(&str) -> bool| {
            rail_nets
                .iter()
                .filter(|net| {
                    net.pins
                        .iter()
                        .any(|pin| pin.refdes == component.refdes && want(&pin.pin))
                })
                .map(|net| net.name.clone())
                .collect::<Vec<_>>()
        };
        let declared = |name: &str| {
            field(component, name).and_then(|wanted| {
                rail_nets
                    .iter()
                    .find(|net| {
                        net.name.eq_ignore_ascii_case(wanted)
                            && net.pins.iter().any(|pin| pin.refdes == component.refdes)
                    })
                    .map(|net| net.name.clone())
            })
        };
        let inputs = declared("Power.InputNet")
            .map(|net| vec![net])
            .unwrap_or_else(|| attached(is_input_pin));
        let outputs = declared("Power.OutputNet")
            .map(|net| vec![net])
            .unwrap_or_else(|| attached(is_output_pin));
        match role(component).map(str::to_ascii_lowercase).as_deref() {
            Some("source") => roots.extend(outputs.iter().cloned().chain(inputs.iter().cloned())),
            Some("regulator") => {
                for output in &outputs {
                    for input in &inputs {
                        parents
                            .entry(output.clone())
                            .or_default()
                            .insert(input.clone());
                    }
                }
                if inputs.is_empty() || outputs.is_empty() {
                    findings.push(finding(
                        "regulator_topology",
                        ProofState::Failed,
                        &component.refdes.0,
                        "declared regulator needs a rail on both an input and output pin",
                    ));
                }
                for input in &inputs {
                    for output in &outputs {
                        let input_v = rail_voltage(input)
                            .or_else(|| number(component, "Power.InputVoltageV"));
                        let output_v = rail_voltage(output)
                            .or_else(|| number(component, "Power.OutputVoltageV"));
                        match (input_v, output_v, number(component, "Power.DropoutV")) {
                            (Some(input_v), Some(output_v), Some(dropout_v)) => {
                                let margin = input_v.abs() - output_v.abs();
                                let state = if margin >= dropout_v {
                                    ProofState::Passed
                                } else {
                                    ProofState::Failed
                                };
                                findings.push(finding(
                                    "regulator_headroom",
                                    state,
                                    &component.refdes.0,
                                    format!(
                                        "{input} {input_v:.3} V -> {output} {output_v:.3} V leaves {margin:.3} V; declared dropout is {dropout_v:.3} V"
                                    ),
                                ));
                            }
                            _ => findings.push(finding(
                                "regulator_headroom",
                                ProofState::Unknown,
                                &component.refdes.0,
                                "rail voltage or Power.DropoutV declaration is missing",
                            )),
                        }
                    }
                }
            }
            _ => {}
        }
        for net in &rail_nets {
            if !role(component).is_some_and(|role| role.eq_ignore_ascii_case("regulator"))
                && net.pins.iter().any(|pin| {
                    pin.refdes == component.refdes
                        && pin.electrical_type == Some(PinElectricalType::PowerOutput)
                })
            {
                roots.insert(net.name.clone());
            }
        }
    }

    let mut reachable = roots.clone();
    let mut queue: VecDeque<_> = roots.iter().cloned().collect();
    while let Some(parent) = queue.pop_front() {
        for (child, candidate_parents) in &parents {
            if candidate_parents.contains(&parent) && reachable.insert(child.clone()) {
                queue.push_back(child.clone());
            }
        }
    }

    let mut rails = Vec::new();
    for net in rail_nets {
        let sources: Vec<_> = net
            .pins
            .iter()
            .filter(|pin| {
                pin.electrical_type == Some(PinElectricalType::PowerOutput)
                    || part(circuit, &pin.refdes).is_some_and(|owner| {
                        matches!(
                            role(owner).map(str::to_ascii_lowercase).as_deref(),
                            Some("source" | "regulator")
                        ) && field(owner, "Power.OutputNet")
                            .is_some_and(|declared| declared.eq_ignore_ascii_case(&net.name))
                    })
            })
            .map(|pin| format!("{}.{}", pin.refdes, pin.pin))
            .collect();
        let load_parts: Vec<_> = net
            .pins
            .iter()
            .filter(|pin| pin.electrical_type == Some(PinElectricalType::PowerInput))
            .filter_map(|pin| part(circuit, &pin.refdes))
            .map(|part| (part.refdes.clone(), part))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect();
        let loads: Vec<_> = load_parts
            .iter()
            .map(|part| part.refdes.to_string())
            .collect();
        let load_values: Vec<_> = load_parts
            .iter()
            .map(|part| number(part, "Power.LoadCurrentA"))
            .collect();
        let declared_load_a = load_values
            .iter()
            .copied()
            .collect::<Option<Vec<_>>>()
            .map(|v| v.into_iter().sum());
        let output_current_a = net
            .pins
            .iter()
            .filter_map(|pin| part(circuit, &pin.refdes))
            .filter_map(|part| number(part, "Power.OutputCurrentA"))
            .reduce(f64::max);
        let is_reachable = reachable.contains(&net.name);
        if !is_reachable && !loads.is_empty() {
            findings.push(finding(
                "power_reachability",
                ProofState::Failed,
                &net.name,
                "rail has declared power-input loads but no path from a declared source",
            ));
        } else if !is_reachable {
            findings.push(finding(
                "power_reachability",
                ProofState::Unknown,
                &net.name,
                "rail has no path from an explicitly declared source; no typed power-input load proves whether it is used",
            ));
        }
        match (output_current_a, declared_load_a) {
            (Some(limit), Some(load)) if load > limit => findings.push(finding(
                "current_budget",
                ProofState::Failed,
                &net.name,
                format!("declared load {load:.6} A exceeds source rating {limit:.6} A"),
            )),
            (Some(limit), Some(load)) => findings.push(finding(
                "current_budget",
                ProofState::Passed,
                &net.name,
                format!("declared load {load:.6} A is within {limit:.6} A"),
            )),
            _ if !loads.is_empty() => findings.push(finding(
                "current_budget",
                ProofState::Unknown,
                &net.name,
                "source rating or one or more load-current declarations are missing",
            )),
            _ => {}
        }
        rails.push(PowerRailProof {
            net: net.name.clone(),
            voltage_v: rail_voltage(&net.name),
            sources,
            loads,
            reachable: is_reachable,
            output_current_a,
            declared_load_a,
        });
    }
    if rails.is_empty() {
        findings.push(finding(
            "power_tree",
            ProofState::Unknown,
            circuit.name(),
            "no named supply rail is present",
        ));
    }
    PowerTreeProof { rails, findings }
}

fn as_check_result(aspect: &str, findings: &[ProofFinding], success: String) -> CheckResult {
    let failures: Vec<_> = findings
        .iter()
        .filter(|f| f.state == ProofState::Failed)
        .collect();
    let unknowns: Vec<_> = findings
        .iter()
        .filter(|f| f.state == ProofState::Unknown)
        .collect();
    let (verdict, detail) = if !failures.is_empty() {
        (
            Verdict::Failed,
            failures
                .iter()
                .map(|f| format!("{}: {}", f.subject, f.detail))
                .collect::<Vec<_>>()
                .join("; "),
        )
    } else if !unknowns.is_empty() {
        (
            Verdict::NeedsReview,
            unknowns
                .iter()
                .map(|f| format!("{}: {}", f.subject, f.detail))
                .collect::<Vec<_>>()
                .join("; "),
        )
    } else {
        (Verdict::Passed, success)
    };
    CheckResult {
        aspect: aspect.into(),
        verifiable: Verifiable::Artifact,
        verdict,
        detail,
    }
}

#[must_use]
pub fn connectivity_check(circuit: &dyn CircuitSource) -> CheckResult {
    let proof = prove_connectivity(circuit);
    as_check_result(
        "typed-pin connectivity and driver compatibility",
        &proof.findings,
        format!("{} typed pin attachment(s) checked", proof.typed_pins),
    )
}

#[must_use]
pub fn power_tree_check(circuit: &dyn CircuitSource) -> CheckResult {
    let proof = prove_power_tree(circuit);
    as_check_result(
        "source-to-load power reachability and declared current budgets",
        &proof.findings,
        format!(
            "{} rail(s) are reachable and within declared budgets",
            proof.rails.len()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, PinRef};

    fn typed(refdes: &str, pin: &str, kind: PinElectricalType) -> PinRef {
        PinRef::new(refdes, pin).with_electrical_type(kind)
    }

    #[test]
    fn catches_output_conflicts_and_floating_inputs() {
        let mut circuit = Circuit::new("bad");
        circuit.parts = vec![Part::new("U1", "logic"), Part::new("U2", "logic")];
        circuit.nets = vec![
            Net::new(
                "BUS",
                vec![
                    typed("U1", "OUT", PinElectricalType::Output),
                    typed("U2", "OUT", PinElectricalType::Output),
                ],
            ),
            Net::new("FLOAT", vec![typed("U2", "IN", PinElectricalType::Input)]),
        ];
        let proof = prove_connectivity(&circuit);
        assert!(proof
            .findings
            .iter()
            .any(|f| f.check == "driver_conflict" && f.state == ProofState::Failed));
        assert!(proof
            .findings
            .iter()
            .any(|f| f.check == "floating_input" && f.state == ProofState::Failed));
    }

    #[test]
    fn proves_source_regulator_load_path_and_budget() {
        let mut source = Part::new("J1", "USB");
        source.fields.insert("Power.Role".into(), "source".into());
        source.fields.insert("Power.OutputNet".into(), "5V".into());
        source
            .fields
            .insert("Power.OutputCurrentA".into(), "0.5".into());
        let mut regulator = Part::new("U1", "LDO");
        regulator
            .fields
            .insert("Power.Role".into(), "regulator".into());
        regulator
            .fields
            .insert("Power.InputNet".into(), "5V".into());
        regulator
            .fields
            .insert("Power.OutputNet".into(), "3V3".into());
        regulator
            .fields
            .insert("Power.OutputCurrentA".into(), "0.3".into());
        regulator
            .fields
            .insert("Power.DropoutV".into(), "0.2".into());
        let mut load = Part::new("U2", "MCU");
        load.fields.insert("Power.Role".into(), "load".into());
        load.fields.insert("Power.InputNet".into(), "3V3".into());
        load.fields
            .insert("Power.LoadCurrentA".into(), "0.08".into());
        let mut circuit = Circuit::new("powered");
        circuit.parts = vec![source, regulator, load];
        circuit.nets = vec![
            Net::new(
                "5V",
                vec![
                    typed("J1", "1", PinElectricalType::PowerOutput),
                    typed("U1", "1", PinElectricalType::PowerInput),
                ],
            ),
            Net::new(
                "3V3",
                vec![
                    typed("U1", "2", PinElectricalType::PowerOutput),
                    typed("U2", "7", PinElectricalType::PowerInput),
                ],
            ),
        ];
        let proof = prove_power_tree(&circuit);
        let rail = proof.rails.iter().find(|rail| rail.net == "3V3").unwrap();
        assert!(rail.reachable);
        assert_eq!(rail.voltage_v, Some(3.3));
        assert_eq!(rail.declared_load_a, Some(0.08));
        assert_eq!(rail.output_current_a, Some(0.3));
        assert!(proof
            .findings
            .iter()
            .any(|f| f.check == "current_budget" && f.state == ProofState::Passed));
        assert!(proof
            .findings
            .iter()
            .any(|f| f.check == "regulator_headroom" && f.state == ProofState::Passed));
    }

    #[test]
    fn missing_power_ratings_are_unknown_not_passed() {
        let mut source = Part::new("J1", "USB");
        source.fields.insert("Power.Role".into(), "source".into());
        let load = Part::new("U1", "MCU");
        let mut circuit = Circuit::new("unknown");
        circuit.parts = vec![source, load];
        circuit.nets = vec![Net::new(
            "5V",
            vec![
                typed("J1", "VBUS", PinElectricalType::PowerOutput),
                typed("U1", "VDD", PinElectricalType::PowerInput),
            ],
        )];
        assert_eq!(power_tree_check(&circuit).verdict, Verdict::NeedsReview);
    }
}
