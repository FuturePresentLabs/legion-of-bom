//! Deterministic, artifact-visible engineering checks for synthesized digital boards.
//!
//! These checks are intentionally topology screens.  They prove facts present in
//! the netlist; they do not claim signal-integrity, EMC, RF, USB, or product-safety
//! certification.  Physical obligations remain explicit in the report.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{is_ground_net, is_supply_rail, Net, Part, RefDes};
use crate::source::CircuitSource;
use crate::standards::{CheckResult, StandardReport, Verdict, Verifiable};

const PROFILE: &str = "embedded-digital-black-book";

#[must_use]
pub fn verify(circuit: &dyn CircuitSource) -> StandardReport {
    StandardReport {
        standard: PROFILE.into(),
        designation: "Puget artifact-visible embedded/audio/RF engineering profile v1".into(),
        results: vec![
            crate::electrical_proof::connectivity_check(circuit),
            crate::electrical_proof::power_tree_check(circuit),
            decoupling(circuit),
            clock_topology(circuit),
            interface_bindings(circuit),
            rf_macro(circuit),
            CheckResult {
                aspect: "physical electrical, SI/PI, EMC and RF performance".into(),
                verifiable: Verifiable::TestOnly,
                verdict: Verdict::NeedsTest,
                detail: "requires bench measurements and the applicable compliance program; this topology profile does not certify the product".into(),
            },
        ],
    }
}

fn result(aspect: &str, failures: Vec<String>, success: String) -> CheckResult {
    CheckResult {
        aspect: aspect.into(),
        verifiable: Verifiable::Artifact,
        verdict: if failures.is_empty() {
            Verdict::Passed
        } else {
            Verdict::Failed
        },
        detail: if failures.is_empty() {
            success
        } else {
            failures.join("; ")
        },
    }
}

fn part<'a>(circuit: &'a dyn CircuitSource, refdes: &RefDes) -> Option<&'a Part> {
    circuit.parts().iter().find(|part| &part.refdes == refdes)
}

fn is_cap(part: &Part) -> bool {
    part.refdes.0.starts_with('C')
}

fn is_digital_ic(part: &Part) -> bool {
    let value = part.value.to_ascii_uppercase();
    value.starts_with("STM32")
        || value.contains("ES8388")
        || value.contains("CS4270")
        || value.contains("PCM")
        || value.contains("SX1262")
}

fn net_has(net: &Net, refdes: &RefDes, pin: &str) -> bool {
    net.pins
        .iter()
        .any(|p| &p.refdes == refdes && p.pin.eq_ignore_ascii_case(pin))
}

fn ground_refs(circuit: &dyn CircuitSource) -> BTreeSet<RefDes> {
    circuit
        .nets()
        .iter()
        .filter(|net| is_ground_net(&net.name))
        .flat_map(|net| net.pins.iter().map(|pin| pin.refdes.clone()))
        .collect()
}

/// Screen each digital IC rail for a capacitor that bridges that rail to ground.
/// This proves topology only; placement rules separately prove physical proximity.
///
/// @derives-from url:https://www.st.com/resource/en/datasheet/stm32h743vi.pdf §6.3.2 -- generic topology screen; device-specific values remain catalog-owned
fn decoupling(circuit: &dyn CircuitSource) -> CheckResult {
    let grounded = ground_refs(circuit);
    let mut failures = Vec::new();
    let mut checked = 0usize;
    for ic in circuit.parts().iter().filter(|part| is_digital_ic(part)) {
        let rails: Vec<_> = circuit
            .nets()
            .iter()
            .filter(|net| {
                is_supply_rail(&net.name) && net.pins.iter().any(|pin| pin.refdes == ic.refdes)
            })
            .collect();
        if rails.is_empty() {
            failures.push(format!("{} has no named supply rail", ic.refdes));
            continue;
        }
        for rail in rails {
            checked += 1;
            let bypass = rail.pins.iter().any(|pin| {
                part(circuit, &pin.refdes).is_some_and(is_cap) && grounded.contains(&pin.refdes)
            });
            if !bypass {
                failures.push(format!(
                    "{} on {} has no capacitor from that rail to ground",
                    ic.refdes, rail.name
                ));
            }
        }
    }
    if checked == 0 {
        failures.push("no supported MCU, codec, or radio IC was present".into());
    }
    result(
        "MCU, codec and radio supply decoupling topology",
        failures,
        format!(
            "{checked} IC supply attachment(s) have a netlist-visible ground-referenced bypass"
        ),
    )
}

fn crystal_bridges(circuit: &dyn CircuitSource, owner: &Part, a: &str, b: &str) -> Option<RefDes> {
    let a_net = circuit.nets().iter().find(|net| {
        net_has(net, &owner.refdes, a)
            || (net.pins.iter().any(|pin| pin.refdes == owner.refdes)
                && net.name.to_ascii_uppercase().ends_with("HSE_IN"))
    })?;
    let b_net = circuit.nets().iter().find(|net| {
        net_has(net, &owner.refdes, b)
            || (net.pins.iter().any(|pin| pin.refdes == owner.refdes)
                && net.name.to_ascii_uppercase().ends_with("HSE_OUT"))
    })?;
    a_net.pins.iter().find_map(|pin| {
        let candidate = part(circuit, &pin.refdes)?;
        (candidate.refdes.0.starts_with('Y')
            && b_net
                .pins
                .iter()
                .any(|other| other.refdes == candidate.refdes))
        .then(|| candidate.refdes.clone())
    })
}

fn clock_topology(circuit: &dyn CircuitSource) -> CheckResult {
    let mut failures = Vec::new();
    let mut checked = Vec::new();
    for owner in circuit.parts().iter().filter(|part| {
        let value = part.value.to_ascii_uppercase();
        value.starts_with("STM32") || value.contains("SX1262")
    }) {
        let is_radio = owner.value.to_ascii_uppercase().contains("SX1262");
        let (a, b) = if is_radio {
            ("XTA", "XTB")
        } else {
            ("PH0", "PH1")
        };
        match crystal_bridges(circuit, owner, a, b) {
            Some(crystal) => checked.push(format!("{} {}-{} via {}", owner.refdes, a, b, crystal)),
            None => failures.push(format!(
                "{} has no single crystal bridging {} and {}",
                owner.refdes, a, b
            )),
        }
    }
    if checked.is_empty() && failures.is_empty() {
        failures.push("no supported MCU or radio clock owner was present".into());
    }
    result(
        "clock-source topology",
        failures,
        format!("clock endpoints are paired: {}", checked.join("; ")),
    )
}

fn interface_bindings(circuit: &dyn CircuitSource) -> CheckResult {
    let mcu = circuit
        .parts()
        .iter()
        .find(|p| p.value.to_ascii_uppercase().starts_with("STM32"));
    let codec = circuit.parts().iter().find(|p| {
        let v = p.value.to_ascii_uppercase();
        v.contains("ES8388") || v.contains("CS4270") || v.contains("PCM")
    });
    let radio = circuit
        .parts()
        .iter()
        .find(|p| p.value.to_ascii_uppercase().contains("SX1262"));
    let mut failures = Vec::new();
    let mut checked = Vec::new();
    if let (Some(mcu), Some(codec)) = (mcu, codec) {
        let audio: [(&str, &[&str]); 5] = [
            ("BCK", &["SCLK", "BCK"]),
            ("LRCK", &["LRCK", "WS"]),
            ("MCLK", &["MCLK"]),
            ("DAC data", &["DSDIN", "DIN"]),
            ("ADC data", &["ASDOUT", "DOUT"]),
        ];
        for (label, codec_pins) in audio {
            // MCU pin names are package GPIOs after synthesis, so prove the
            // binding by the semantic net name plus the codec endpoint.
            let semantic = if label == "DAC data" {
                "DOUT"
            } else if label == "ADC data" {
                "DIN"
            } else {
                label
            };
            let ok = circuit.nets().iter().any(|net| {
                net.name.to_ascii_uppercase().contains(semantic)
                    && net.pins.iter().any(|p| p.refdes == mcu.refdes)
                    && net.pins.iter().any(|p| p.refdes == codec.refdes)
                    && (codec_pins
                        .iter()
                        .any(|name| net_has(net, &codec.refdes, name))
                        || net.name.to_ascii_uppercase().starts_with("I2S_"))
            });
            if !ok {
                failures.push(format!("audio {label} is not bound MCU-to-codec"));
            }
        }
        for (label, pin) in [("I2C SCL", "CCLK"), ("I2C SDA", "CDATA")] {
            if !circuit.nets().iter().any(|net| {
                net.name
                    .to_ascii_uppercase()
                    .contains(label.split_whitespace().last().unwrap_or(""))
                    && net.pins.iter().any(|p| p.refdes == mcu.refdes)
                    && net.pins.iter().any(|p| p.refdes == codec.refdes)
                    && (net_has(net, &codec.refdes, pin)
                        || net.name.to_ascii_uppercase().starts_with("I2C_"))
            }) {
                failures.push(format!("{label} is not bound MCU-to-codec"));
            }
        }
        checked.push("I2S + I2C audio control".to_string());
    }
    if let (Some(mcu), Some(radio)) = (mcu, radio) {
        for (label, pin) in [
            ("SCK", "SCK"),
            ("MOSI", "MOSI"),
            ("MISO", "MISO"),
            ("CS", "NSS"),
            ("BUSY", "BUSY"),
            ("IRQ", "DIO1"),
            ("RESET", "~{RESET}"),
        ] {
            if !circuit.nets().iter().any(|net| {
                let upper = net.name.to_ascii_uppercase();
                let semantic = match label {
                    "CS" => upper.contains("SPI_CS"),
                    "IRQ" => upper.contains("IRQ"),
                    "RESET" => upper.contains("RESET"),
                    _ => upper.contains(label),
                };
                semantic
                    && net.pins.iter().any(|p| p.refdes == mcu.refdes)
                    && net.pins.iter().any(|p| p.refdes == radio.refdes)
                    && (net_has(net, &radio.refdes, pin)
                        || upper.starts_with("SPI_")
                        || upper.starts_with("U2_"))
            }) {
                failures.push(format!("radio {label} is not bound MCU-to-SX1262"));
            }
        }
        checked.push("SPI + BUSY/IRQ/RESET radio control".to_string());
    }
    if checked.is_empty() {
        failures.push("no supported audio-codec or SX1262 interface was present".into());
    }
    result(
        "digital interface pin bindings",
        failures,
        format!("checked {}", checked.join(" and ")),
    )
}

/// Preserve the component/value signature of the cited 915 MHz reference macro.
/// This is not an RF-performance claim: stackup, geometry, antenna, enclosure,
/// conducted power and emissions still require calculation and measurement.
///
/// @derives-from url:https://cdn-reichelt.de/documents/datenblatt/A200/SX1262REFERENCE.pdf p.1 -- topology/value signature transcribed in catalog/subcircuits/sx1262-frontend-915.json
fn rf_macro(circuit: &dyn CircuitSource) -> CheckResult {
    let Some(radio) = circuit
        .parts()
        .iter()
        .find(|p| p.value.to_ascii_uppercase().contains("SX1262"))
    else {
        return CheckResult {
            aspect: "cited SX1262 915 MHz reference-macro integrity".into(),
            verifiable: Verifiable::Artifact,
            verdict: Verdict::NeedsReview,
            detail: "not applicable: no SX1262 is present".into(),
        };
    };
    let switch = circuit
        .parts()
        .iter()
        .find(|p| p.value.to_ascii_uppercase().contains("PE4259"));
    let antenna = circuit
        .parts()
        .iter()
        .find(|p| p.footprint.as_deref().is_some_and(|f| f.contains("U.FL")));
    let mut failures = Vec::new();
    if switch.is_none() {
        failures.push("PE4259 RF switch is missing".into());
    }
    if antenna.is_none() {
        failures.push("U.FL antenna port is missing".into());
    }
    for pin in ["RFO", "RFI_P", "RFI_N", "VR_PA"] {
        if !circuit.nets().iter().any(|net| {
            net.pins.iter().any(|p| p.refdes == radio.refdes)
                && (net_has(net, &radio.refdes, pin)
                    || net.name.to_ascii_uppercase().ends_with(pin))
                && net.pins.len() > 1
        }) {
            failures.push(format!(
                "{}.{pin} is not connected into the front end",
                radio.refdes
            ));
        }
    }
    let mut have = BTreeMap::<String, usize>::new();
    for part in circuit.parts() {
        *have
            .entry(part.value.trim().to_ascii_uppercase())
            .or_default() += 1;
    }
    // Exact values from the catalog's cited macro. Multiplicity matters for
    // repeated shunts; extra passives elsewhere do not invalidate the macro.
    let expected = [
        ("47NF", 1),
        ("47PF", 1),
        ("47NH", 1),
        ("0R", 1),
        ("2.5NH", 1),
        ("3.0PF", 1),
        ("5.6PF", 1),
        ("39PF", 1),
        ("4.7NH", 1),
        ("1.8PF", 2),
        ("2.4PF", 1),
        ("15NH", 1),
        ("3.3PF", 2),
        ("9.1NH", 1),
    ];
    for (value, count) in expected {
        let actual = have.get(value).copied().unwrap_or(0);
        if actual < count {
            failures.push(format!(
                "reference macro needs {count} x {value}, found {actual}"
            ));
        }
    }
    result(
        "cited SX1262 915 MHz reference-macro integrity",
        failures,
        "radio, RF switch, antenna endpoint and cited passive-value signature are present; RF performance remains a physical-test obligation".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef};

    fn pins(items: &[(&str, &str)]) -> Vec<PinRef> {
        items.iter().map(|(r, p)| PinRef::new(*r, *p)).collect()
    }

    fn audio() -> Circuit {
        let mut c = Circuit::new("audio");
        for (r, v) in [
            ("U1", "STM32H743VIT6"),
            ("U2", "ES8388"),
            ("U3", "REG"),
            ("Y1", "24MHz"),
            ("J1", "POWER"),
            ("C1", "100nF"),
            ("C2", "33pF"),
            ("C3", "33pF"),
        ] {
            c.parts.push(Part::new(r, v));
        }
        c.nets = vec![
            Net::new(
                "+3V3",
                pins(&[("U3", "VOUT"), ("U1", "VDD"), ("U2", "DVDD"), ("C1", "1")]),
            ),
            Net::new(
                "GND",
                pins(&[
                    ("J1", "2"),
                    ("U1", "VSS"),
                    ("U2", "DGND"),
                    ("C1", "2"),
                    ("C2", "2"),
                    ("C3", "2"),
                ]),
            ),
            Net::new("HSE_IN", pins(&[("U1", "PH0"), ("Y1", "1"), ("C2", "1")])),
            Net::new("HSE_OUT", pins(&[("U1", "PH1"), ("Y1", "3"), ("C3", "1")])),
        ];
        for (index, (name, codec_pin)) in [
            ("I2S_BCK", "SCLK"),
            ("I2S_LRCK", "LRCK"),
            ("I2S_MCLK", "MCLK"),
            ("I2S_DOUT", "DSDIN"),
            ("I2S_DIN", "ASDOUT"),
            ("I2C_SCL", "CCLK"),
            ("I2C_SDA", "CDATA"),
        ]
        .into_iter()
        .enumerate()
        {
            let gpio = format!("GPIO{index}");
            c.nets.push(Net::new(
                name,
                vec![PinRef::new("U1", gpio), PinRef::new("U2", codec_pin)],
            ));
        }
        c
    }

    #[test]
    fn good_audio_fixture_passes_applicable_checks_without_certification_claim() {
        let report = verify(&audio());
        let verdict = |aspect: &str| {
            report
                .results
                .iter()
                .find(|result| result.aspect == aspect)
                .unwrap()
                .verdict
        };
        assert_eq!(
            verdict("typed-pin connectivity and driver compatibility"),
            Verdict::NeedsReview
        );
        assert_eq!(
            verdict("source-to-load power reachability and declared current budgets"),
            Verdict::NeedsReview
        );
        assert_eq!(
            verdict("MCU, codec and radio supply decoupling topology"),
            Verdict::Passed
        );
        assert_eq!(verdict("clock-source topology"), Verdict::Passed);
        assert_eq!(verdict("digital interface pin bindings"), Verdict::Passed);
        assert_eq!(
            verdict("physical electrical, SI/PI, EMC and RF performance"),
            Verdict::NeedsTest
        );
        assert!(report.needs_physical_test());
    }

    #[test]
    fn broken_audio_fixture_identifies_missing_rail_bypass_clock_and_binding() {
        let mut c = audio();
        c.nets
            .retain(|net| net.name != "GND" && net.name != "HSE_OUT" && net.name != "I2S_MCLK");
        let report = verify(&c);
        for aspect in [
            "MCU, codec and radio supply decoupling topology",
            "clock-source topology",
            "digital interface pin bindings",
        ] {
            assert_eq!(
                report
                    .results
                    .iter()
                    .find(|result| result.aspect == aspect)
                    .unwrap()
                    .verdict,
                Verdict::Failed
            );
        }
    }

    #[test]
    fn incomplete_rf_fixture_fails_control_and_reference_macro_integrity() {
        let mut c = Circuit::new("rf");
        c.parts = vec![
            Part::new("U1", "STM32F411CEU6"),
            Part::new("U2", "SX1262IMLTRT"),
        ];
        c.nets
            .push(Net::new("SPI_SCK", pins(&[("U1", "PA5"), ("U2", "SCK")])));
        let report = verify(&c);
        let interfaces = report
            .results
            .iter()
            .find(|result| result.aspect == "digital interface pin bindings")
            .unwrap();
        let rf = report
            .results
            .iter()
            .find(|result| result.aspect == "cited SX1262 915 MHz reference-macro integrity")
            .unwrap();
        assert_eq!(interfaces.verdict, Verdict::Failed);
        assert_eq!(rf.verdict, Verdict::Failed);
        assert!(rf.detail.contains("PE4259"));
    }
}
