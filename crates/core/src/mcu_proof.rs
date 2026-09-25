//! Deterministic minimum-system diagnostics for microcontroller schematics.
//!
//! This module reports facts from [`CircuitSource`]; it does not certify a board
//! and PCBBench must still judge the raw `lob.ee-source.v1` export independently.

use crate::model::{is_ground_net, RefDes};
use crate::source::CircuitSource;
use crate::units::parse_eng_value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinSupply {
    pub pin: String,
    pub rail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugPin {
    pub connector_pin: String,
    pub mcu_pin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McuMinimumSystemSpec {
    pub mcu: RefDes,
    pub supply_pins: Vec<PinSupply>,
    pub ground_pins: Vec<String>,
    pub reset_pin: String,
    pub boot_pin: String,
    pub oscillator_in_pin: String,
    pub oscillator_out_pin: String,
    pub debug_connector: RefDes,
    pub debug_pins: Vec<DebugPin>,
    pub debug_reset_pin: String,
    pub debug_reference_pin: String,
    pub debug_ground_pin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartIdentity {
    pub manufacturer_part_number: Option<String>,
    pub lcsc_part_number: Option<String>,
}

#[derive(Clone)]
pub struct McuProofInput<'a> {
    pub circuit: &'a dyn CircuitSource,
    pub spec: &'a McuMinimumSystemSpec,
    pub identities: &'a BTreeMap<RefDes, PartIdentity>,
    pub erc_errors: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofFinding {
    pub obligation: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McuProofReport {
    pub findings: Vec<ProofFinding>,
}

impl McuProofReport {
    pub fn passes(&self, obligation: &str) -> bool {
        !self.findings.iter().any(|f| f.obligation == obligation)
    }
}

pub fn check_mcu_minimum_system(input: McuProofInput<'_>) -> McuProofReport {
    let mut findings = Vec::new();
    let circuit = input.circuit;
    let spec = input.spec;
    if !input.erc_errors.is_empty() {
        findings.push(finding("erc_clean", input.erc_errors.join("; ")));
    }

    for supply in &spec.supply_pins {
        if pin_net(circuit, &spec.mcu, &supply.pin).map(|n| n.name.as_str())
            != Some(supply.rail.as_str())
        {
            findings.push(finding(
                "all_supply_pins",
                format!("{}:{} is not on {}", spec.mcu, supply.pin, supply.rail),
            ));
        }
    }
    for pin in &spec.ground_pins {
        if !pin_net(circuit, &spec.mcu, pin).is_some_and(|n| is_ground_net(&n.name)) {
            findings.push(finding(
                "all_supply_pins",
                format!("{}:{pin} is not grounded", spec.mcu),
            ));
        }
    }

    let ground_nets: BTreeSet<_> = circuit
        .nets()
        .iter()
        .filter(|n| is_ground_net(&n.name))
        .map(|n| n.name.as_str())
        .collect();
    let rail_names: BTreeSet<_> = spec.supply_pins.iter().map(|p| p.rail.as_str()).collect();
    let caps: Vec<_> = circuit
        .parts()
        .iter()
        .filter(|p| p.refdes.0.starts_with('C'))
        .filter_map(|p| {
            let nets = part_nets(circuit, &p.refdes);
            (nets.len() == 2).then(|| (p, nets))
        })
        .collect();
    for rail in rail_names {
        let bypass_count = caps
            .iter()
            .filter(|(p, nets)| {
                parse_eng_value(&p.value).is_some_and(|v| (v - 100e-9).abs() <= 5e-9)
                    && nets.iter().any(|n| n.as_str() == rail)
                    && nets.iter().any(|n| ground_nets.contains(n.as_str()))
            })
            .count();
        let pins_on_rail = spec.supply_pins.iter().filter(|p| p.rail == rail).count();
        if bypass_count < pins_on_rail {
            findings.push(finding(
                "decoupling",
                format!("{rail} has {bypass_count} local 100 nF capacitors for {pins_on_rail} supply pins"),
            ));
        }
        if !caps.iter().any(|(p, nets)| {
            parse_eng_value(&p.value).is_some_and(|v| v >= 1e-6)
                && nets.iter().any(|n| n.as_str() == rail)
                && nets.iter().any(|n| ground_nets.contains(n.as_str()))
        }) {
            findings.push(finding(
                "decoupling",
                format!("{rail} lacks >=1 uF bulk capacitance"),
            ));
        }
    }

    check_default_network(circuit, spec, &mut findings);
    check_clock(circuit, spec, &ground_nets, &mut findings);
    check_debug(circuit, spec, &mut findings);

    for part in circuit.parts() {
        let identity = input.identities.get(&part.refdes);
        let exact = |s: Option<&String>| {
            s.is_some_and(|s| {
                let normalized = s.trim().to_ascii_lowercase();
                !normalized.is_empty()
                    && !normalized.contains('?')
                    && !normalized.contains('|')
                    && !normalized.contains(" or ")
                    && !normalized.contains("maybe")
            })
        };
        if !exact(identity.and_then(|i| i.manufacturer_part_number.as_ref()))
            || !exact(identity.and_then(|i| i.lcsc_part_number.as_ref()))
        {
            findings.push(finding(
                "bom_identity",
                format!("{} lacks one exact MPN and LCSC identity", part.refdes),
            ));
        }
    }
    McuProofReport { findings }
}

fn check_default_network(
    circuit: &dyn CircuitSource,
    spec: &McuMinimumSystemSpec,
    findings: &mut Vec<ProofFinding>,
) {
    let Some(reset) = pin_net(circuit, &spec.mcu, &spec.reset_pin) else {
        findings.push(finding("reset_boot_defaults", "reset pin is unconnected"));
        return;
    };
    let Some(boot) = pin_net(circuit, &spec.mcu, &spec.boot_pin) else {
        findings.push(finding("reset_boot_defaults", "BOOT pin is unconnected"));
        return;
    };
    let resistor_to = |signal: &str, predicate: &dyn Fn(&str) -> bool| {
        circuit.parts().iter().any(|p| {
            p.refdes.0.starts_with('R') && {
                let nets = part_nets(circuit, &p.refdes);
                nets.iter().any(|n| n == signal) && nets.iter().any(|n| predicate(n))
            }
        })
    };
    if !resistor_to(&reset.name, &|n| n == "3V3" || n == "+3V3") {
        findings.push(finding(
            "reset_boot_defaults",
            "reset lacks a 3.3 V pull-up",
        ));
    }
    if !resistor_to(&boot.name, &is_ground_net) {
        findings.push(finding(
            "reset_boot_defaults",
            "BOOT lacks a ground pull-down",
        ));
    }
    if !circuit.parts().iter().any(|p| {
        (p.refdes.0.starts_with("SW") || p.refdes.0.starts_with('S')) && {
            let nets = part_nets(circuit, &p.refdes);
            nets.iter().any(|n| n == &reset.name) && nets.iter().any(|n| is_ground_net(n))
        }
    }) {
        findings.push(finding(
            "reset_boot_defaults",
            "reset lacks a momentary ground path",
        ));
    }
}

fn check_clock(
    circuit: &dyn CircuitSource,
    spec: &McuMinimumSystemSpec,
    grounds: &BTreeSet<&str>,
    findings: &mut Vec<ProofFinding>,
) {
    let Some(input) = pin_net(circuit, &spec.mcu, &spec.oscillator_in_pin) else {
        findings.push(finding("clock_network", "OSC_IN is unconnected"));
        return;
    };
    let Some(output) = pin_net(circuit, &spec.mcu, &spec.oscillator_out_pin) else {
        findings.push(finding("clock_network", "OSC_OUT is unconnected"));
        return;
    };
    let crystal_ok = circuit.parts().iter().any(|p| {
        (p.refdes.0.starts_with('Y') || p.refdes.0.starts_with('X'))
            && p.value.to_ascii_lowercase().contains("8mhz")
            && {
                let nets = part_nets(circuit, &p.refdes);
                nets.contains(&input.name) && nets.contains(&output.name)
            }
    });
    let load_cap = |signal: &str| {
        circuit.parts().iter().any(|p| {
            p.refdes.0.starts_with('C') && {
                let nets = part_nets(circuit, &p.refdes);
                nets.iter().any(|n| n == signal)
                    && nets.iter().any(|n| grounds.contains(n.as_str()))
            }
        })
    };
    if !crystal_ok || !load_cap(&input.name) || !load_cap(&output.name) {
        findings.push(finding(
            "clock_network",
            "8 MHz crystal or one of its grounded load capacitors is missing",
        ));
    }
}

fn check_debug(
    circuit: &dyn CircuitSource,
    spec: &McuMinimumSystemSpec,
    findings: &mut Vec<ProofFinding>,
) {
    for mapping in &spec.debug_pins {
        let mcu = pin_net(circuit, &spec.mcu, &mapping.mcu_pin).map(|n| n.name.as_str());
        let connector = pin_net(circuit, &spec.debug_connector, &mapping.connector_pin)
            .map(|n| n.name.as_str());
        if mcu.is_none() || mcu != connector {
            findings.push(finding(
                "swd_header",
                format!(
                    "debug pin {} is not bound to MCU pin {}",
                    mapping.connector_pin, mapping.mcu_pin
                ),
            ));
        }
    }
    let reset = pin_net(circuit, &spec.mcu, &spec.reset_pin).map(|n| n.name.as_str());
    if pin_net(circuit, &spec.debug_connector, &spec.debug_reset_pin).map(|n| n.name.as_str())
        != reset
        || !pin_net(circuit, &spec.debug_connector, &spec.debug_reference_pin)
            .is_some_and(|n| n.name == "3V3" || n.name == "+3V3")
        || !pin_net(circuit, &spec.debug_connector, &spec.debug_ground_pin)
            .is_some_and(|n| is_ground_net(&n.name))
    {
        findings.push(finding(
            "swd_header",
            "debug reset, reference, or ground binding is wrong",
        ));
    }
}

fn pin_net<'a>(
    circuit: &'a dyn CircuitSource,
    part: &RefDes,
    pin: &str,
) -> Option<&'a crate::model::Net> {
    circuit.nets().iter().find(|net| {
        net.pins
            .iter()
            .any(|candidate| &candidate.refdes == part && candidate.pin == pin)
    })
}

fn part_nets(circuit: &dyn CircuitSource, part: &RefDes) -> Vec<String> {
    circuit
        .nets()
        .iter()
        .filter(|n| n.pins.iter().any(|p| &p.refdes == part))
        .map(|n| n.name.clone())
        .collect()
}

fn finding(obligation: &'static str, message: impl Into<String>) -> ProofFinding {
    ProofFinding {
        obligation,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Circuit, Net, Part, PinRef};

    fn fixture() -> (
        Circuit,
        McuMinimumSystemSpec,
        BTreeMap<RefDes, PartIdentity>,
    ) {
        let names = [
            "U1", "C1", "C2", "C3", "C4", "C5", "R1", "R2", "SW1", "Y1", "J1",
        ];
        let parts = names
            .iter()
            .map(|r| {
                let value = match *r {
                    "C1" | "C2" => "100n",
                    "C3" => "1u",
                    "C4" | "C5" => "12p",
                    "R1" | "R2" => "10k",
                    "Y1" => "8MHz",
                    _ => "part",
                };
                Part::new(*r, value).with_mpn(format!("MPN-{r}"))
            })
            .collect();
        let circuit = Circuit {
            name: "mcu".into(),
            parts,
            nets: vec![
                Net::new(
                    "+3V3",
                    vec![
                        PinRef::new("U1", "VDD"),
                        PinRef::new("U1", "VDDA"),
                        PinRef::new("C1", "1"),
                        PinRef::new("C2", "1"),
                        PinRef::new("C3", "1"),
                        PinRef::new("R1", "1"),
                        PinRef::new("J1", "4"),
                    ],
                ),
                Net::new(
                    "GND",
                    vec![
                        PinRef::new("U1", "VSS"),
                        PinRef::new("U1", "VSSA"),
                        PinRef::new("C1", "2"),
                        PinRef::new("C2", "2"),
                        PinRef::new("C3", "2"),
                        PinRef::new("C4", "2"),
                        PinRef::new("C5", "2"),
                        PinRef::new("R2", "2"),
                        PinRef::new("SW1", "2"),
                        PinRef::new("J1", "5"),
                    ],
                ),
                Net::new(
                    "NRST",
                    vec![
                        PinRef::new("U1", "NRST"),
                        PinRef::new("R1", "2"),
                        PinRef::new("SW1", "1"),
                        PinRef::new("J1", "3"),
                    ],
                ),
                Net::new(
                    "BOOT0",
                    vec![PinRef::new("U1", "BOOT0"), PinRef::new("R2", "1")],
                ),
                Net::new(
                    "OSC_IN",
                    vec![
                        PinRef::new("U1", "OSC_IN"),
                        PinRef::new("Y1", "1"),
                        PinRef::new("C4", "1"),
                    ],
                ),
                Net::new(
                    "OSC_OUT",
                    vec![
                        PinRef::new("U1", "OSC_OUT"),
                        PinRef::new("Y1", "2"),
                        PinRef::new("C5", "1"),
                    ],
                ),
                Net::new(
                    "SWDIO",
                    vec![PinRef::new("U1", "SWDIO"), PinRef::new("J1", "1")],
                ),
                Net::new(
                    "SWCLK",
                    vec![PinRef::new("U1", "SWCLK"), PinRef::new("J1", "2")],
                ),
            ],
        };
        let spec = McuMinimumSystemSpec {
            mcu: "U1".into(),
            supply_pins: vec![
                PinSupply {
                    pin: "VDD".into(),
                    rail: "+3V3".into(),
                },
                PinSupply {
                    pin: "VDDA".into(),
                    rail: "+3V3".into(),
                },
            ],
            ground_pins: vec!["VSS".into(), "VSSA".into()],
            reset_pin: "NRST".into(),
            boot_pin: "BOOT0".into(),
            oscillator_in_pin: "OSC_IN".into(),
            oscillator_out_pin: "OSC_OUT".into(),
            debug_connector: "J1".into(),
            debug_pins: vec![
                DebugPin {
                    connector_pin: "1".into(),
                    mcu_pin: "SWDIO".into(),
                },
                DebugPin {
                    connector_pin: "2".into(),
                    mcu_pin: "SWCLK".into(),
                },
            ],
            debug_reset_pin: "3".into(),
            debug_reference_pin: "4".into(),
            debug_ground_pin: "5".into(),
        };
        let identities = names
            .iter()
            .enumerate()
            .map(|(n, r)| {
                (
                    (*r).into(),
                    PartIdentity {
                        manufacturer_part_number: Some(format!("MPN-{r}")),
                        lcsc_part_number: Some(format!("C{}", n + 1)),
                    },
                )
            })
            .collect();
        (circuit, spec, identities)
    }

    fn report(
        c: &Circuit,
        s: &McuMinimumSystemSpec,
        i: &BTreeMap<RefDes, PartIdentity>,
    ) -> McuProofReport {
        check_mcu_minimum_system(McuProofInput {
            circuit: c,
            spec: s,
            identities: i,
            erc_errors: &[],
        })
    }

    #[test]
    fn correct_minimum_system_passes_all_obligations() {
        let (c, s, i) = fixture();
        assert!(report(&c, &s, &i).findings.is_empty());
    }

    #[test]
    fn mutations_trip_their_owned_obligations() {
        let (mut c, s, i) = fixture();
        c.parts.retain(|p| p.refdes.0 != "C1");
        c.nets
            .iter_mut()
            .for_each(|n| n.pins.retain(|p| p.refdes.0 != "C1"));
        assert!(!report(&c, &s, &i).passes("decoupling"));

        let (mut c, s, i) = fixture();
        for pin in c.nets.iter_mut().flat_map(|n| &mut n.pins) {
            if pin.refdes.0 == "J1" {
                pin.pin = match pin.pin.as_str() {
                    "1" => "2".into(),
                    "2" => "1".into(),
                    _ => pin.pin.clone(),
                };
            }
        }
        assert!(!report(&c, &s, &i).passes("swd_header"));

        let (c, s, mut i) = fixture();
        i.get_mut(&RefDes("U1".into())).unwrap().lcsc_part_number = None;
        assert!(!report(&c, &s, &i).passes("bom_identity"));
    }
}
