//! Validation stages — surface SKiDL/KiCad ERC results and circuit-level
//! carrier-board safety checks as structured findings.
//! First-pass validation per DESIGN.md 4.1.
//!
//! ERC runs inside the SKiDL script; the runner captures its report. Here we
//! turn each `ERC WARNING:` / `ERC ERROR:` line into a [`Finding`] and fail the
//! stage only if ERC reported actual errors (warnings pass — e.g. the RC demo's
//! open-port single-pin-net notices).

use std::collections::HashMap;

use crate::source::CircuitSource;
use crate::stage::{Finding, StageOutcome};
use crate::subboard::{PinCapability, ProfilePin, SubboardProfile, SUBBOARD_LIB};

const STAGE: &str = "validate";
const CARRIER_STAGE: &str = "carrier";

/// Turn a SKiDL ERC report into a stage outcome.
pub fn validate_erc(erc_report: Option<&str>) -> StageOutcome {
    let Some(report) = erc_report else {
        return StageOutcome::passed(STAGE).with(Finding::warning("no ERC report was produced"));
    };

    let mut warnings = 0usize;
    let mut errors = 0usize;
    let mut outcome = StageOutcome::passed(STAGE);
    for line in report.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("ERC WARNING:") {
            warnings += 1;
            outcome = outcome.with(Finding::warning(rest.trim().to_string()));
        } else if let Some(rest) = line.strip_prefix("ERC ERROR:") {
            // `with` flips the outcome to failed on an error finding.
            errors += 1;
            outcome = outcome.with(Finding::error(rest.trim().to_string()));
        }
    }
    outcome.with(Finding::info(format!(
        "ERC: {warnings} warning(s), {errors} error(s)"
    )))
}

/// Check that external panel signals are not tied directly to raw SOM pins.
///
/// Patch SM is marked as already Eurorack-conditioned in its profile, so direct
/// jacks/CV/gates to its named pins are acceptable. Lower-level SOMs such as
/// Seed2 DFM expose STM32/codec pins; external panel I/O must go through an
/// analog/protection block instead of sharing the same net as the raw pin.
pub fn validate_carrier(circuit: &dyn CircuitSource) -> StageOutcome {
    let parts: HashMap<&str, _> = circuit
        .parts()
        .iter()
        .map(|p| (p.refdes.0.as_str(), p))
        .collect();
    let modules: HashMap<&str, SubboardProfile> = circuit
        .parts()
        .iter()
        .filter_map(|p| {
            let (_, name) = p.footprint.as_deref()?.split_once(':')?;
            if !p
                .footprint
                .as_deref()
                .is_some_and(|fp| fp.starts_with(&format!("{SUBBOARD_LIB}:")))
            {
                return None;
            }
            crate::subboard::profile(name).map(|profile| (p.refdes.0.as_str(), profile))
        })
        .collect();

    if modules.is_empty() {
        return StageOutcome::passed(CARRIER_STAGE)
            .with(Finding::info("no carrier sub-board profiles found"));
    }

    let mut checked = 0usize;
    let mut outcome = StageOutcome::passed(CARRIER_STAGE);
    for net in circuit.nets() {
        let external_panel_pins: Vec<_> = net
            .pins
            .iter()
            .filter_map(|pin| {
                let part = parts.get(pin.refdes.0.as_str())?;
                is_external_panel_part(part).then_some(pin)
            })
            .collect();
        if external_panel_pins.is_empty() {
            continue;
        }

        for pin in &net.pins {
            let Some(profile) = modules.get(pin.refdes.0.as_str()) else {
                continue;
            };
            checked += 1;
            let Some(profile_pin) = profile.pin_for(&pin.pin) else {
                outcome = outcome.with(Finding::error(format!(
                    "{} pin {} on net {} is not in the {} carrier profile",
                    pin.refdes, pin.pin, net.name, profile.name
                )));
                continue;
            };
            if profile.eurorack_conditioned || !raw_external_capability(profile_pin) {
                continue;
            }
            for panel_pin in &external_panel_pins {
                outcome = outcome.with(Finding::error(format!(
                    "{} pin {} ({}) on net {} connects directly to external panel part {} {}; add conditioning/protection between the jack/control and the raw SOM pin",
                    profile.name,
                    profile_pin.name,
                    capabilities(profile_pin),
                    net.name,
                    panel_pin.refdes,
                    panel_pin.pin
                )));
            }
        }
    }

    let summary = if outcome.findings.is_empty() {
        format!(
            "carrier validation: {} module(s), no external panel nets found",
            modules.len()
        )
    } else {
        format!("carrier validation: {checked} module pin(s) checked")
    };
    outcome = outcome.with(Finding::info(summary));
    outcome
}

fn raw_external_capability(pin: &ProfilePin) -> bool {
    pin.has_capability(PinCapability::AudioIn)
        || pin.has_capability(PinCapability::AudioOut)
        || pin.has_capability(PinCapability::AudioReference)
        || pin.has_capability(PinCapability::CvIn)
        || pin.has_capability(PinCapability::CvOut)
        || pin.has_capability(PinCapability::GateIn)
        || pin.has_capability(PinCapability::GateOut)
        || pin.has_capability(PinCapability::AnalogIn)
        || pin.has_capability(PinCapability::Dac)
        || pin.has_capability(PinCapability::Gpio)
}

fn is_external_panel_part(part: &crate::model::Part) -> bool {
    let fp = part.footprint.as_deref().unwrap_or_default();
    let value = part.value.as_str();
    contains_any_ci(
        fp,
        &[
            "Connector_Audio",
            "Jack_3.5",
            "PJ398",
            "Thonkiconn",
            "Connector_Banana",
            "SW_",
            "Switch",
        ],
    ) || contains_any_ci(
        value,
        &[
            "jack",
            "audio in",
            "audio out",
            "cv in",
            "gate in",
            "trigger",
        ],
    )
}

fn contains_any_ci(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| {
        haystack
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())
    })
}

fn capabilities(pin: &ProfilePin) -> String {
    pin.capabilities
        .iter()
        .map(|cap| format!("{cap:?}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carrier::{AudioChannel, CarrierBuilder, DEFAULT_JACK_FOOTPRINT};
    use crate::model::{Circuit, Net, Part, PinRef};

    #[test]
    fn warnings_pass_errors_fail() {
        let clean = "ERC WARNING: Only one pin attached to net IN.\n\
                     ERC INFO: 1 warnings found while running ERC.\n\
                     ERC INFO: 0 errors found while running ERC.";
        let outcome = validate_erc(Some(clean));
        assert!(outcome.passed);
        assert!(!outcome.has_errors());

        let bad = "ERC ERROR: Two output pins connected together on net OUT.";
        let outcome = validate_erc(Some(bad));
        assert!(!outcome.passed);
        assert!(outcome.has_errors());
    }

    #[test]
    fn missing_report_warns_but_passes() {
        let outcome = validate_erc(None);
        assert!(outcome.passed);
    }

    #[test]
    fn carrier_validation_passes_patch_sm_direct_panel_connections() {
        let mut carrier = CarrierBuilder::patch_sm("patch", "M1").unwrap();
        carrier
            .audio_input("J1", AudioChannel::Left)
            .unwrap()
            .audio_output("J2", AudioChannel::Right)
            .unwrap()
            .cv_input("J3", 1)
            .unwrap()
            .gate_input("J4", 1)
            .unwrap();

        let outcome = validate_carrier(&carrier.into_circuit());
        assert!(outcome.passed, "{:?}", outcome.findings);
    }

    #[test]
    fn carrier_validation_fails_seed2_audio_jack_direct_to_codec_pin() {
        let mut carrier = CarrierBuilder::seed2_dfm("seed2", "M1").unwrap();
        carrier
            .add_part(Part::new("J1", "audio in").with_footprint(DEFAULT_JACK_FOOTPRINT))
            .bind_to_som("IN_L", PinRef::new("J1", "T"), "AUDIO_IN_L")
            .unwrap();

        let outcome = validate_carrier(&carrier.into_circuit());
        assert!(!outcome.passed);
        assert!(outcome.findings.iter().any(|f| {
            f.message.contains("AUDIO_IN_L")
                && f.message.contains("external panel part J1 T")
                && f.message.contains("conditioning/protection")
        }));
    }

    #[test]
    fn carrier_validation_fails_seed2_cv_jack_direct_to_adc_pin() {
        let mut carrier = CarrierBuilder::seed2_dfm("seed2", "M1").unwrap();
        carrier
            .add_part(Part::new("J1", "cv in").with_footprint(DEFAULT_JACK_FOOTPRINT))
            .bind_to_som("CV_RAW", PinRef::new("J1", "T"), "A1")
            .unwrap();

        let outcome = validate_carrier(&carrier.into_circuit());
        assert!(!outcome.passed);
        assert!(outcome
            .findings
            .iter()
            .any(|f| f.message.contains("D16") && f.message.contains("AnalogIn")));
    }

    #[test]
    fn carrier_validation_passes_seed2_when_front_end_breaks_the_direct_net() {
        let circuit = Circuit {
            name: "seed2-conditioned".into(),
            parts: vec![
                Part::new("M1", "DAISY_SEED2_DFM").with_footprint("LobModule:DAISY_SEED2_DFM"),
                Part::new("J1", "cv in").with_footprint(DEFAULT_JACK_FOOTPRINT),
                Part::new("R1", "100k").with_footprint("Resistor_SMD:R_0805_2012Metric"),
            ],
            nets: vec![
                Net::new(
                    "CV_PANEL",
                    vec![PinRef::new("J1", "T"), PinRef::new("R1", "1")],
                ),
                Net::new(
                    "CV_ADC",
                    vec![PinRef::new("R1", "2"), PinRef::new("M1", "A1")],
                ),
            ],
        };

        let outcome = validate_carrier(&circuit);
        assert!(outcome.passed, "{:?}", outcome.findings);
    }
}
