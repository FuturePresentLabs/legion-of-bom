//! Textbook verification — the step that makes Phase 0 a *proof* of the loop,
//! not just a run. DESIGN.md 14.1, 1.2.
//!
//! For a single-pole RC low-pass the analytic −3 dB cutoff is `1/(2πRC)`. This
//! compares that against the cutoff the ngspice AC sweep actually produced and
//! passes only if they agree within tolerance.

use crate::source::CircuitSource;
use crate::spice::AcResult;
use crate::stage::{Finding, StageOutcome};
use crate::units::parse_eng_value;

const STAGE: &str = "verify";

/// Parts whose refdes starts with `prefix` (case-insensitive).
fn parts_with_prefix(circuit: &dyn CircuitSource, prefix: char) -> Vec<&str> {
    circuit
        .parts()
        .iter()
        .filter(|p| {
            p.refdes
                .0
                .chars()
                .next()
                .is_some_and(|c| c.eq_ignore_ascii_case(&prefix))
        })
        .map(|p| p.value.as_str())
        .collect()
}

/// Check the simulated −3 dB cutoff against `1/(2πRC)`.
///
/// Applicable only to a single-resistor, single-capacitor low-pass; other
/// topologies produce a "skipped" (still passing) outcome so the pipeline isn't
/// blocked by a check that doesn't apply. `rel_tol` is the allowed fractional
/// error (e.g. `0.02` for 2%).
pub fn check_rc_cutoff(circuit: &dyn CircuitSource, ac: &AcResult, rel_tol: f64) -> StageOutcome {
    let resistors = parts_with_prefix(circuit, 'R');
    let capacitors = parts_with_prefix(circuit, 'C');

    if resistors.len() != 1 || capacitors.len() != 1 {
        return StageOutcome::passed(STAGE).with(Finding::warning(format!(
            "analytic RC cutoff check skipped: expected exactly 1 resistor and 1 capacitor, \
             found {} R and {} C",
            resistors.len(),
            capacitors.len()
        )));
    }

    let (Some(r), Some(c)) = (
        parse_eng_value(resistors[0]),
        parse_eng_value(capacitors[0]),
    ) else {
        return StageOutcome::failed(
            STAGE,
            format!(
                "could not parse component values R='{}', C='{}'",
                resistors[0], capacitors[0]
            ),
        );
    };

    let expected = 1.0 / (2.0 * std::f64::consts::PI * r * c);
    let Some(simulated) = ac.cutoff_3db_hz() else {
        return StageOutcome::failed(
            STAGE,
            "no −3 dB crossing found in the AC sweep (is the range wide enough?)".to_string(),
        );
    };

    let rel_err = (simulated - expected).abs() / expected;
    let msg = format!(
        "−3 dB cutoff: expected {expected:.2} Hz (1/2πRC), simulated {simulated:.2} Hz, \
         error {:.3}% (tol {:.1}%)",
        rel_err * 100.0,
        rel_tol * 100.0
    );

    if rel_err <= rel_tol {
        StageOutcome::passed(STAGE).with(Finding::info(msg))
    } else {
        StageOutcome::failed(STAGE, msg)
    }
}

/// Output net + ground convention (matches [`SimConfig`](crate::spice::SimConfig)
/// defaults) used to identify the feedback vs ground resistor topologically.
const OUTPUT_NET: &str = "OUT";

fn is_ground_net(name: &str) -> bool {
    name.eq_ignore_ascii_case("GND") || name == "0"
}

/// Net names a given reference designator connects to.
fn nets_of<'a>(circuit: &'a dyn CircuitSource, refdes: &str) -> Vec<&'a str> {
    circuit
        .nets()
        .iter()
        .filter(|n| n.pins.iter().any(|p| p.refdes.0 == refdes))
        .map(|n| n.name.as_str())
        .collect()
}

/// Check the simulated passband gain against `1 + Rf/Rg` for a non-inverting amp.
///
/// The feedback and ground resistors are identified topologically (feedback
/// touches the output net, ground touches GND) so the check doesn't depend on
/// reference-designator names. Non-matching topologies are skipped.
pub fn check_noninverting_gain(
    circuit: &dyn CircuitSource,
    ac: &AcResult,
    rel_tol: f64,
) -> StageOutcome {
    let resistors: Vec<(&str, &str)> = circuit
        .parts()
        .iter()
        .filter(|p| {
            p.refdes
                .0
                .chars()
                .next()
                .is_some_and(|c| c.eq_ignore_ascii_case(&'R'))
        })
        .map(|p| (p.refdes.0.as_str(), p.value.as_str()))
        .collect();
    if resistors.len() != 2 {
        return StageOutcome::passed(STAGE).with(Finding::warning(format!(
            "non-inverting gain check skipped: expected 2 resistors, found {}",
            resistors.len()
        )));
    }

    let mut feedback = None;
    let mut ground = None;
    for (refdes, value) in &resistors {
        let nets = nets_of(circuit, refdes);
        if nets.contains(&OUTPUT_NET) {
            feedback = Some((*refdes, *value));
        }
        if nets.iter().any(|n| is_ground_net(n)) {
            ground = Some((*refdes, *value));
        }
    }
    let (Some((rf_ref, rf_val)), Some((rg_ref, rg_val))) = (feedback, ground) else {
        return StageOutcome::passed(STAGE).with(Finding::warning(
            "non-inverting gain check skipped: could not identify feedback/ground resistors"
                .to_string(),
        ));
    };
    if rf_ref == rg_ref {
        return StageOutcome::passed(STAGE).with(Finding::warning(
            "non-inverting gain check skipped: one resistor spans OUT and GND".to_string(),
        ));
    }

    let (Some(rf), Some(rg)) = (parse_eng_value(rf_val), parse_eng_value(rg_val)) else {
        return StageOutcome::failed(
            STAGE,
            format!("could not parse Rf='{rf_val}', Rg='{rg_val}'"),
        );
    };

    let expected = 1.0 + rf / rg;
    let expected_db = 20.0 * expected.log10();
    let Some(sim_db) = ac.passband_gain_db() else {
        return StageOutcome::failed(STAGE, "no simulated gain available".to_string());
    };
    let simulated = 10f64.powf(sim_db / 20.0);
    let rel_err = (simulated - expected).abs() / expected;
    let msg = format!(
        "non-inverting gain: expected {expected:.3}× ({expected_db:.2} dB, 1+Rf/Rg; Rf={rf_ref}, \
         Rg={rg_ref}), simulated {simulated:.3}× ({sim_db:.2} dB), error {:.3}% (tol {:.1}%)",
        rel_err * 100.0,
        rel_tol * 100.0
    );

    if rel_err <= rel_tol {
        StageOutcome::passed(STAGE).with(Finding::info(msg))
    } else {
        StageOutcome::failed(STAGE, msg)
    }
}

/// Run the analytic check appropriate to the circuit's topology: op-amp gain if
/// there's an ideal op-amp, otherwise the RC cutoff.
pub fn analytic_check(circuit: &dyn CircuitSource, ac: &AcResult, rel_tol: f64) -> StageOutcome {
    if circuit.parts().iter().any(|p| p.is_ideal_opamp()) {
        check_noninverting_gain(circuit, ac, rel_tol)
    } else {
        check_rc_cutoff(circuit, ac, rel_tol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef};
    use crate::spice::{AcPoint, AcResult};

    fn rc_circuit(r: &str, c: &str) -> Circuit {
        Circuit {
            name: "rc".into(),
            parts: vec![Part::new("R1", r), Part::new("C1", c)],
            nets: vec![
                Net::new("IN", vec![PinRef::new("R1", "1")]),
                Net::new("OUT", vec![PinRef::new("R1", "2"), PinRef::new("C1", "1")]),
                Net::new("GND", vec![PinRef::new("C1", "2")]),
            ],
        }
    }

    /// A response whose −3 dB point sits at ~`fc` Hz.
    fn response_with_cutoff(fc: f64) -> AcResult {
        AcResult {
            points: vec![
                AcPoint {
                    freq_hz: fc / 10.0,
                    mag_db: 0.0,
                },
                AcPoint {
                    freq_hz: fc,
                    mag_db: -3.0102999566,
                },
                AcPoint {
                    freq_hz: fc * 10.0,
                    mag_db: -20.0,
                },
            ],
        }
    }

    #[test]
    fn passes_when_simulated_matches_analytic() {
        // R=1k, C=159n → fc ≈ 1001 Hz.
        let circuit = rc_circuit("1k", "159n");
        let ac = response_with_cutoff(1000.97);
        let outcome = check_rc_cutoff(&circuit, &ac, 0.02);
        assert!(outcome.passed, "{:?}", outcome.findings);
    }

    #[test]
    fn fails_when_simulated_is_off() {
        // Analytic fc ≈ 1001 Hz, but the "simulation" says 5 kHz → must fail.
        let circuit = rc_circuit("1k", "159n");
        let ac = response_with_cutoff(5000.0);
        let outcome = check_rc_cutoff(&circuit, &ac, 0.02);
        assert!(!outcome.passed);
        assert!(outcome.has_errors());
    }

    #[test]
    fn skips_when_not_single_rc() {
        let mut circuit = rc_circuit("1k", "159n");
        circuit.parts.push(Part::new("R2", "2k"));
        let ac = response_with_cutoff(1000.0);
        let outcome = check_rc_cutoff(&circuit, &ac, 0.02);
        assert!(outcome.passed); // skipped, not failed
        assert!(!outcome.has_errors());
    }

    fn opamp_amp(rf: &str, rg: &str) -> Circuit {
        Circuit {
            name: "amp".into(),
            parts: vec![
                Part {
                    refdes: "U1".into(),
                    value: "OPAMP".into(),
                    footprint: None,
                    library_part: Some("Simulation_SPICE:OPAMP".into()),
                    mpn: None,
                },
                Part::new("R1", rf), // feedback: OUT ↔ FB
                Part::new("R2", rg), // ground:   FB ↔ GND
            ],
            nets: vec![
                Net::new("IN", vec![PinRef::new("U1", "1")]),
                Net::new(
                    "FB",
                    vec![
                        PinRef::new("U1", "2"),
                        PinRef::new("R1", "2"),
                        PinRef::new("R2", "1"),
                    ],
                ),
                Net::new("OUT", vec![PinRef::new("U1", "5"), PinRef::new("R1", "1")]),
                Net::new("GND", vec![PinRef::new("R2", "2")]),
            ],
        }
    }

    fn flat_response(gain_db: f64) -> AcResult {
        AcResult {
            points: vec![
                AcPoint {
                    freq_hz: 1.0,
                    mag_db: gain_db,
                },
                AcPoint {
                    freq_hz: 1e6,
                    mag_db: gain_db,
                },
            ],
        }
    }

    #[test]
    fn gain_passes_when_matching() {
        // Rf=9k, Rg=1k → gain 10 → 20 dB.
        let outcome = check_noninverting_gain(&opamp_amp("9k", "1k"), &flat_response(20.0), 0.02);
        assert!(outcome.passed, "{:?}", outcome.findings);
    }

    #[test]
    fn gain_fails_when_off() {
        // Analytic gain 10 (20 dB) but the sim says 6 dB (~2×) → fail.
        let outcome = check_noninverting_gain(&opamp_amp("9k", "1k"), &flat_response(6.0), 0.02);
        assert!(!outcome.passed);
        assert!(outcome.has_errors());
    }

    #[test]
    fn dispatcher_routes_by_topology() {
        // Op-amp circuit → gain check applies and passes.
        assert!(analytic_check(&opamp_amp("9k", "1k"), &flat_response(20.0), 0.02).passed);
        // RC circuit → cutoff check applies.
        let rc = rc_circuit("1k", "159n");
        assert!(analytic_check(&rc, &response_with_cutoff(1000.97), 0.02).passed);
    }
}
