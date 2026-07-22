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
}
