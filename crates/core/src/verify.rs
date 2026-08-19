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
/// Self-selecting: returns `None` unless the circuit is a single-resistor,
/// single-capacitor low-pass (so it can be run alongside other checks without a
/// dispatcher choosing it). `rel_tol` is the allowed fractional error (e.g.
/// `0.02` for 2%). A `Some` result means the check ran (pass or fail).
pub fn check_rc_cutoff(
    circuit: &dyn CircuitSource,
    ac: &AcResult,
    rel_tol: f64,
) -> Option<StageOutcome> {
    let resistors = parts_with_prefix(circuit, 'R');
    let capacitors = parts_with_prefix(circuit, 'C');

    // Not an RC low-pass → this check doesn't apply.
    if resistors.len() != 1 || capacitors.len() != 1 {
        return None;
    }

    let (Some(r), Some(c)) = (
        parse_eng_value(resistors[0]),
        parse_eng_value(capacitors[0]),
    ) else {
        return Some(StageOutcome::failed(
            STAGE,
            format!(
                "could not parse component values R='{}', C='{}'",
                resistors[0], capacitors[0]
            ),
        ));
    };

    let expected = 1.0 / (2.0 * std::f64::consts::PI * r * c);
    let Some(simulated) = ac.cutoff_3db_hz() else {
        return Some(StageOutcome::failed(
            STAGE,
            "no −3 dB crossing found in the AC sweep (is the range wide enough?)".to_string(),
        ));
    };

    let rel_err = (simulated - expected).abs() / expected;
    let msg = format!(
        "−3 dB cutoff: expected {expected:.2} Hz (1/2πRC), simulated {simulated:.2} Hz, \
         error {:.3}% (tol {:.1}%)",
        rel_err * 100.0,
        rel_tol * 100.0
    );

    Some(if rel_err <= rel_tol {
        StageOutcome::passed(STAGE).with(Finding::info(msg))
    } else {
        StageOutcome::failed(STAGE, msg)
    })
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
/// Self-selecting: returns `None` unless the circuit presents the non-inverting
/// topology — two resistors, one touching the output net (feedback) and one
/// touching ground (Rg). That structure *is* the recogniser, so the check never
/// asks "is there an op-amp?"; it works for the ideal symbol and a real device
/// alike. A `Some` result means the check ran (pass or fail).
pub fn check_noninverting_gain(
    circuit: &dyn CircuitSource,
    ac: &AcResult,
    rel_tol: f64,
) -> Option<StageOutcome> {
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
        return None;
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
    // Not the feedback/ground topology (or one resistor spans both) → doesn't apply.
    let (Some((rf_ref, rf_val)), Some((rg_ref, rg_val))) = (feedback, ground) else {
        return None;
    };
    if rf_ref == rg_ref {
        return None;
    }

    let (Some(rf), Some(rg)) = (parse_eng_value(rf_val), parse_eng_value(rg_val)) else {
        return Some(StageOutcome::failed(
            STAGE,
            format!("could not parse Rf='{rf_val}', Rg='{rg_val}'"),
        ));
    };

    let expected = 1.0 + rf / rg;
    let expected_db = 20.0 * expected.log10();
    let Some(sim_db) = ac.passband_gain_db() else {
        return Some(StageOutcome::failed(
            STAGE,
            "no simulated gain available".to_string(),
        ));
    };
    let simulated = 10f64.powf(sim_db / 20.0);
    let rel_err = (simulated - expected).abs() / expected;
    let msg = format!(
        "non-inverting gain: expected {expected:.3}× ({expected_db:.2} dB, 1+Rf/Rg; Rf={rf_ref}, \
         Rg={rg_ref}), simulated {simulated:.3}× ({sim_db:.2} dB), error {:.3}% (tol {:.1}%)",
        rel_err * 100.0,
        rel_tol * 100.0
    );

    Some(if rel_err <= rel_tol {
        StageOutcome::passed(STAGE).with(Finding::info(msg))
    } else {
        StageOutcome::failed(STAGE, msg)
    })
}

/// Check that one channel of a multi-channel circuit does not disturb another.
///
/// Self-selecting like the analytic checks: returns `None` unless the circuit
/// presents at least two signal channels (`SIG_IN1`/`SIG_OUT1`, `SIG_IN2`/…),
/// so it costs a single-channel board nothing.
///
/// `aggressor` is the driven channel's output waveform and `victim` the *un*driven
/// channel's, from the same stimulus. The check fails if the victim moved more
/// than `max_ratio` of the aggressor's swing — and also if the aggressor barely
/// moved, because a simulation that drove nothing would otherwise "pass" while
/// measuring nothing.
///
/// WHAT THIS DOES AND DOES NOT PROVE. A netlist has ideal supplies and a
/// zero-impedance ground, so the only coupling it can express is a *shared node*
/// — two channels accidentally sharing a net, a resistor, or an op-amp section.
/// That is exactly the failure mode of a dual built by instantiating one channel
/// twice (a net name that didn't get its per-channel suffix), which is why this
/// is worth gating on. Coupling through shared rail/return impedance, and
/// through a shared die, are physical and cannot appear here — see the product
/// repo's `circuits/dual_slew_limiter/dual_slew_limiter_crosstalk.cir` for the
/// first (it sweeps the shared ground return) and the bench for the second.
pub fn check_channel_crosstalk(
    circuit: &dyn CircuitSource,
    aggressor: &crate::spice::TranResult,
    victim: &crate::spice::TranResult,
    max_ratio: f64,
) -> Option<StageOutcome> {
    let channels = crate::spice::signal_channels(circuit);
    if channels.len() < 2 {
        return None;
    }
    let (Some(driven), Some(leaked)) = (aggressor.peak_to_peak_v(), victim.peak_to_peak_v()) else {
        return Some(StageOutcome::failed(
            STAGE,
            "channel crosstalk: no waveform data to compare".to_string(),
        ));
    };
    // Guard against a vacuous pass: if the aggressor didn't swing, the ratio is
    // meaningless however small the victim's excursion is.
    const MIN_DRIVEN_V: f64 = 0.1;
    if driven < MIN_DRIVEN_V {
        return Some(StageOutcome::failed(
            STAGE,
            format!(
                "channel crosstalk: the driven channel ({}) only moved {driven:.4} V — \
                 the stimulus never reached it, so isolation was not measured",
                channels[0].1
            ),
        ));
    }

    let ratio = leaked / driven;
    let db = if ratio > 0.0 {
        format!("{:.1} dB", 20.0 * ratio.log10())
    } else {
        "-inf dB".to_string()
    };
    let msg = format!(
        "channel crosstalk: drove {} → {} swung {driven:.3} Vpp; undriven {} moved \
         {leaked:.6} Vpp — {ratio:.3e} ({db}, limit {max_ratio:.1e}); \
         {} channels, ideal supplies",
        channels[0].0,
        channels[0].1,
        channels[1].1,
        channels.len()
    );
    Some(if ratio <= max_ratio {
        StageOutcome::passed(STAGE).with(Finding::info(msg))
    } else {
        StageOutcome::failed(STAGE, msg)
    })
}

/// The analytic checks, in registry order. Each is *self-selecting* — it returns
/// `None` when its topology doesn't match — so verification never sniffs for a
/// device type to decide what to run. Adding a check means adding it here.
type AnalyticCheck = fn(&dyn CircuitSource, &AcResult, f64) -> Option<StageOutcome>;
const ANALYTIC_CHECKS: [AnalyticCheck; 2] = [check_rc_cutoff, check_noninverting_gain];

/// Run every analytic check the circuit's topology matches and merge the results.
/// No topology matched → a passing note (nothing to verify against), so the
/// pipeline isn't blocked.
pub fn analytic_check(circuit: &dyn CircuitSource, ac: &AcResult, rel_tol: f64) -> StageOutcome {
    let applied: Vec<StageOutcome> = ANALYTIC_CHECKS
        .iter()
        .filter_map(|check| check(circuit, ac, rel_tol))
        .collect();

    if applied.is_empty() {
        return StageOutcome::passed(STAGE).with(Finding::info(
            "no analytic check matched this circuit's topology — nothing to verify against",
        ));
    }

    StageOutcome {
        stage: STAGE.to_string(),
        passed: applied.iter().all(|o| o.passed),
        findings: applied.into_iter().flat_map(|o| o.findings).collect(),
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
        let outcome = check_rc_cutoff(&circuit, &ac, 0.02).expect("RC check applies");
        assert!(outcome.passed, "{:?}", outcome.findings);
    }

    #[test]
    fn fails_when_simulated_is_off() {
        // Analytic fc ≈ 1001 Hz, but the "simulation" says 5 kHz → must fail.
        let circuit = rc_circuit("1k", "159n");
        let ac = response_with_cutoff(5000.0);
        let outcome = check_rc_cutoff(&circuit, &ac, 0.02).expect("RC check applies");
        assert!(!outcome.passed);
        assert!(outcome.has_errors());
    }

    #[test]
    fn rc_check_does_not_apply_to_other_topologies() {
        // Two resistors + a cap is not a single RC low-pass → the check bows out.
        let mut circuit = rc_circuit("1k", "159n");
        circuit.parts.push(Part::new("R2", "2k"));
        let ac = response_with_cutoff(1000.0);
        assert!(check_rc_cutoff(&circuit, &ac, 0.02).is_none());
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
                    sim: None,
                    side: None,
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
        let outcome = check_noninverting_gain(&opamp_amp("9k", "1k"), &flat_response(20.0), 0.02)
            .expect("gain check applies");
        assert!(outcome.passed, "{:?}", outcome.findings);
    }

    #[test]
    fn gain_fails_when_off() {
        // Analytic gain 10 (20 dB) but the sim says 6 dB (~2×) → fail.
        let outcome = check_noninverting_gain(&opamp_amp("9k", "1k"), &flat_response(6.0), 0.02)
            .expect("gain check applies");
        assert!(!outcome.passed);
        assert!(outcome.has_errors());
    }

    #[test]
    fn checks_self_select_by_topology_no_device_sniffing() {
        // Each check recognises its own topology and bows out of the other's — no
        // "is there an op-amp?" branch anywhere.
        let rc = rc_circuit("1k", "159n");
        let amp = opamp_amp("9k", "1k");
        assert!(check_noninverting_gain(&rc, &flat_response(0.0), 0.02).is_none());
        assert!(check_rc_cutoff(&amp, &response_with_cutoff(1000.0), 0.02).is_none());
    }

    #[test]
    fn analytic_check_runs_the_matching_check() {
        // Op-amp circuit → gain check applies and passes.
        let gain = analytic_check(&opamp_amp("9k", "1k"), &flat_response(20.0), 0.02);
        assert!(gain.passed, "{:?}", gain.findings);
        assert!(
            gain.findings
                .iter()
                .any(|f| f.message.contains("non-inverting gain")),
            "gain check did not contribute its finding: {:?}",
            gain.findings
        );
        // RC circuit → cutoff check applies and passes.
        let rc = rc_circuit("1k", "159n");
        let cutoff = analytic_check(&rc, &response_with_cutoff(1000.97), 0.02);
        assert!(cutoff.passed, "{:?}", cutoff.findings);
        assert!(
            cutoff.findings.iter().any(|f| f.message.contains("cutoff")),
            "RC cutoff check did not contribute its finding: {:?}",
            cutoff.findings
        );
    }

    fn dual_circuit() -> Circuit {
        Circuit {
            name: "dual".into(),
            parts: vec![Part::new("U1", "LM13700")],
            nets: vec![
                Net::new("SIG_IN1", vec![PinRef::new("U1", "3")]),
                Net::new("SIG_OUT1", vec![PinRef::new("U1", "5")]),
                Net::new("SIG_IN2", vec![PinRef::new("U1", "14")]),
                Net::new("SIG_OUT2", vec![PinRef::new("U1", "12")]),
            ],
        }
    }

    /// A waveform swinging `pp` volts peak-to-peak.
    fn swing(pp: f64) -> crate::spice::TranResult {
        use crate::spice::TranPoint;
        crate::spice::TranResult {
            points: vec![
                TranPoint { t_s: 0.0, v: 0.0 },
                TranPoint { t_s: 1.0, v: pp },
                TranPoint { t_s: 2.0, v: 0.0 },
            ],
        }
    }

    #[test]
    fn crosstalk_passes_when_channels_are_independent() {
        let outcome = check_channel_crosstalk(&dual_circuit(), &swing(4.0), &swing(0.0), 1e-3)
            .expect("crosstalk check applies to a dual");
        assert!(outcome.passed, "{:?}", outcome.findings);
    }

    #[test]
    fn crosstalk_fails_when_a_node_is_shared() {
        // The failure this exists for: a dual built by instantiating one channel
        // twice, with a net that didn't get its per-channel suffix. The undriven
        // output then follows the driven one.
        let outcome = check_channel_crosstalk(&dual_circuit(), &swing(2.0), &swing(2.0), 1e-3)
            .expect("crosstalk check applies to a dual");
        assert!(!outcome.passed);
        assert!(outcome.has_errors());
    }

    #[test]
    fn crosstalk_does_not_vacuously_pass_on_a_dead_stimulus() {
        // Victim at 0 and aggressor at 0 is perfect isolation by arithmetic and
        // no measurement at all — it must fail, not pass.
        let outcome = check_channel_crosstalk(&dual_circuit(), &swing(0.0), &swing(0.0), 1e-3)
            .expect("crosstalk check applies to a dual");
        assert!(!outcome.passed);
    }

    #[test]
    fn crosstalk_bows_out_of_a_single_channel_circuit() {
        assert!(
            check_channel_crosstalk(&rc_circuit("1k", "159n"), &swing(4.0), &swing(0.0), 1e-3)
                .is_none()
        );
    }

    #[test]
    fn analytic_check_passes_benignly_when_nothing_matches() {
        // A lone resistor matches no analytic check → passing note, not a failure.
        let circuit = Circuit {
            name: "x".into(),
            parts: vec![Part::new("R1", "1k")],
            nets: vec![Net::new("IN", vec![PinRef::new("R1", "1")])],
        };
        let outcome = analytic_check(&circuit, &flat_response(0.0), 0.02);
        assert!(outcome.passed);
        assert!(!outcome.has_errors());
    }
}
