//! Adapt source-declared circuit evidence into `black_book` supervision checks.
//!
//! `black_book` owns the domain-neutral calculations. This module owns Lob's
//! field contract, artifact extraction, and mapping into Lob check results.

use black_book::electrical_safety::{check_fuse, FuseEvidence};
use black_book::electrical_supervision::{
    assess_off_state_isolation, assess_protected_path, assess_reset_supervision,
    assess_safe_states, assess_sense_injection, assess_watchdog, Conclusion,
    OffStateIsolationInput, ProtectedPathInput, ResetSupervisionInput, SafeStateReport,
    SenseInjectionInput, StateCase, WatchdogInput,
};

use crate::{
    model::Part,
    source::CircuitSource,
    standards::{CheckResult, Verdict, Verifiable},
};

fn number(part: &Part, key: &str) -> Option<f64> {
    part.fields.get(key)?.trim().parse().ok()
}

fn boolean(part: &Part, key: &str) -> Option<bool> {
    match part.fields.get(key)?.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn declared(part: &Part, prefix: &str) -> bool {
    part.fields.keys().any(|key| key.starts_with(prefix))
}

fn verdict(conclusions: impl IntoIterator<Item = Conclusion>) -> Verdict {
    let conclusions = conclusions.into_iter().collect::<Vec<_>>();
    if conclusions.contains(&Conclusion::Fail) {
        Verdict::Failed
    } else if conclusions.is_empty() || conclusions.contains(&Conclusion::Unresolved) {
        Verdict::NeedsReview
    } else {
        Verdict::Passed
    }
}

fn result(part: &Part, aspect: &str, verdict: Verdict, detail: String) -> CheckResult {
    CheckResult {
        aspect: format!("{}: {aspect}", part.refdes),
        verifiable: Verifiable::Artifact,
        verdict,
        detail,
    }
}

fn sensing(part: &Part) -> CheckResult {
    let report = assess_sense_injection(SenseInjectionInput {
        maximum_test_voltage_v: number(part, "Sense.MaximumTestVoltageV"),
        minimum_series_resistance_ohm: number(part, "Sense.MinimumSeriesResistanceOhm"),
        minimum_activation_current_a: number(part, "Sense.MinimumActivationCurrentA"),
        maximum_allowed_test_current_a: number(part, "Sense.MaximumAllowedTestCurrentA"),
    });
    match report {
        Ok(report) => result(
            part,
            "sensing-current non-interference",
            verdict([report.below_activation, report.within_declared_limit]),
            format!(
                "worst-case injected current={:?} A; below activation={:?}; within declared limit={:?}",
                report.worst_case_test_current_a,
                report.below_activation,
                report.within_declared_limit
            ),
        ),
        Err(error) => result(part, "sensing-current non-interference", Verdict::Failed, error.to_string()),
    }
}

fn isolation(part: &Part) -> CheckResult {
    let report = assess_off_state_isolation(OffStateIsolationInput {
        maximum_off_state_voltage_v: number(part, "Isolation.MaximumOffStateVoltageV"),
        maximum_allowed_off_state_voltage_v: number(
            part,
            "Isolation.MaximumAllowedOffStateVoltageV",
        ),
        maximum_backfeed_current_a: number(part, "Isolation.MaximumBackfeedCurrentA"),
        maximum_allowed_backfeed_current_a: number(
            part,
            "Isolation.MaximumAllowedBackfeedCurrentA",
        ),
    });
    match report {
        Ok(report) => result(
            part,
            "off-state isolation and backfeed",
            verdict([report.voltage, report.backfeed_current]),
            format!(
                "off-state voltage={:?}; backfeed current={:?}; dielectric and physical isolation remain separate obligations",
                report.voltage, report.backfeed_current
            ),
        ),
        Err(error) => result(part, "off-state isolation and backfeed", Verdict::Failed, error.to_string()),
    }
}

fn protection(part: &Part) -> CheckResult {
    let report = assess_protected_path(ProtectedPathInput {
        maximum_forward_drop_v: number(part, "Protection.MaximumForwardDropV"),
        maximum_allowed_forward_drop_v: number(part, "Protection.MaximumAllowedForwardDropV"),
        reverse_withstand_v: number(part, "Protection.ReverseWithstandV"),
        maximum_applied_reverse_voltage_v: number(part, "Protection.MaximumAppliedReverseVoltageV"),
        maximum_clamp_voltage_v: number(part, "Protection.MaximumClampVoltageV"),
        downstream_absolute_maximum_v: number(part, "Protection.DownstreamAbsoluteMaximumV"),
    });
    match report {
        Ok(report) => result(
            part,
            "protected electrical path",
            verdict([
                report.forward_drop,
                report.reverse_voltage,
                report.transient_clamp,
            ]),
            format!(
                "forward drop={:?}; reverse voltage={:?}; transient clamp={:?}; dynamic coordination and destructive verification remain external",
                report.forward_drop, report.reverse_voltage, report.transient_clamp
            ),
        ),
        Err(error) => result(
            part,
            "protected electrical path",
            Verdict::Failed,
            error.to_string(),
        ),
    }
}

fn fuse(part: &Part) -> CheckResult {
    let evidence = (
        number(part, "Fuse.LoadCurrentA"),
        number(part, "Fuse.ContinuousRatingA"),
        number(part, "Fuse.ApplicationDerating"),
        number(part, "Fuse.InterruptRatingA"),
        number(part, "Fuse.MaximumFaultCurrentA"),
        part.fields.get("Fuse.Source").cloned(),
    );
    let (Some(load), Some(rating), Some(derating), Some(interrupt), Some(fault), Some(source)) =
        evidence
    else {
        return result(
            part,
            "overcurrent protection",
            Verdict::NeedsReview,
            "load, continuous rating, application derating, interrupt rating, maximum fault current, or source is missing".into(),
        );
    };
    match check_fuse(
        load,
        &FuseEvidence {
            continuous_rating_a: rating,
            application_derating: derating,
            interrupt_rating_a: interrupt,
            maximum_fault_current_a: fault,
            source,
        },
    ) {
        Ok(report) => result(
            part,
            "overcurrent protection",
            if report.passes {
                Verdict::Passed
            } else {
                Verdict::Failed
            },
            format!(
                "static rating and interrupt screen={}; time-current coordination and physical test evidence required={}",
                report.passes, report.test_evidence_required
            ),
        ),
        Err(error) => result(
            part,
            "overcurrent protection",
            Verdict::Failed,
            error.to_string(),
        ),
    }
}

fn reset(part: &Part) -> CheckResult {
    let report = assess_reset_supervision(ResetSupervisionInput {
        maximum_reset_release_voltage_v: number(part, "Reset.MaximumReleaseVoltageV"),
        minimum_safe_operating_voltage_v: number(part, "Reset.MinimumSafeOperatingVoltageV"),
        maximum_reset_delay_s: number(part, "Reset.MaximumDelayS"),
        minimum_required_reset_delay_s: number(part, "Reset.MinimumRequiredDelayS"),
        output_safe_without_firmware: boolean(part, "Reset.OutputSafeWithoutFirmware"),
    });
    match report {
        Ok(report) => result(
            part,
            "reset supervision",
            verdict([
                report.release_voltage_safe,
                report.reset_delay_sufficient,
                report.output_safe_without_firmware,
            ]),
            format!(
                "release voltage={:?}; delay={:?}; output safe without firmware={:?}",
                report.release_voltage_safe,
                report.reset_delay_sufficient,
                report.output_safe_without_firmware
            ),
        ),
        Err(error) => result(
            part,
            "reset supervision",
            Verdict::Failed,
            error.to_string(),
        ),
    }
}

fn watchdog(part: &Part) -> CheckResult {
    let report = assess_watchdog(WatchdogInput {
        maximum_detection_time_s: number(part, "Watchdog.MaximumDetectionTimeS"),
        maximum_output_disable_time_s: number(part, "Watchdog.MaximumOutputDisableTimeS"),
        maximum_allowed_unsafe_duration_s: number(part, "Watchdog.MaximumAllowedUnsafeDurationS"),
        independent_of_supervised_software: boolean(
            part,
            "Watchdog.IndependentOfSupervisedSoftware",
        ),
    });
    match report {
        Ok(report) => result(
            part,
            "watchdog supervision",
            verdict([report.recovery_time, report.independence]),
            format!(
                "worst-case recovery={:?} s; recovery time={:?}; independence={:?}",
                report.worst_case_recovery_time_s, report.recovery_time, report.independence
            ),
        ),
        Err(error) => result(
            part,
            "watchdog supervision",
            Verdict::Failed,
            error.to_string(),
        ),
    }
}

fn safe_states(part: &Part) -> CheckResult {
    const PREFIX: &str = "SafeState.";
    const SUFFIX: &str = ".HazardousOutputActive";
    let cases = part
        .fields
        .iter()
        .filter_map(|(key, value)| {
            let id = key.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
            let active = match value.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Some(true),
                "false" | "no" | "0" => Some(false),
                _ => None,
            };
            Some(StateCase {
                id: id.into(),
                hazardous_output_active: active,
            })
        })
        .collect::<Vec<_>>();
    let SafeStateReport {
        conclusion,
        failed_cases,
        unresolved_cases,
    } = assess_safe_states(&cases);
    result(
        part,
        "declared safe states",
        verdict([conclusion]),
        format!(
            "{} case(s); hazardous={failed_cases:?}; unresolved={unresolved_cases:?}; caller owns required state enumeration",
            cases.len()
        ),
    )
}

/// Run only checks explicitly declared in component fields. Absence does not
/// imply applicability or success; domain profiles separately select required
/// obligations.
#[must_use]
pub fn checks(circuit: &dyn CircuitSource) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for part in circuit.parts() {
        if declared(part, "Sense.") {
            results.push(sensing(part));
        }
        if declared(part, "Isolation.") {
            results.push(isolation(part));
        }
        if declared(part, "Protection.") {
            results.push(protection(part));
        }
        if declared(part, "Fuse.") {
            results.push(fuse(part));
        }
        if declared(part, "Reset.") {
            results.push(reset(part));
        }
        if declared(part, "Watchdog.") {
            results.push(watchdog(part));
        }
        if declared(part, "SafeState.") {
            results.push(safe_states(part));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Circuit, Part};

    #[test]
    fn declared_checks_call_black_book_and_preserve_unknowns() {
        let mut circuit = Circuit::new("supervised");
        let mut sense = Part::new("U1", "sense");
        sense
            .fields
            .insert("Sense.MaximumTestVoltageV".into(), "5".into());
        sense
            .fields
            .insert("Sense.MinimumSeriesResistanceOhm".into(), "10000".into());
        sense
            .fields
            .insert("Sense.MinimumActivationCurrentA".into(), "0.01".into());
        sense
            .fields
            .insert("Sense.MaximumAllowedTestCurrentA".into(), "0.001".into());
        let mut watchdog = Part::new("U2", "watchdog");
        watchdog
            .fields
            .insert("Watchdog.MaximumDetectionTimeS".into(), "0.15".into());
        circuit.parts.extend([sense, watchdog]);

        let results = checks(&circuit);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].verdict, Verdict::Passed);
        assert_eq!(results[1].verdict, Verdict::NeedsReview);
    }

    #[test]
    fn safe_state_names_are_not_hardcoded_by_lob() {
        let mut circuit = Circuit::new("states");
        let mut control = Part::new("U1", "control");
        control.fields.insert(
            "SafeState.installation:true.HazardousOutputActive".into(),
            "false".into(),
        );
        control.fields.insert(
            "SafeState.user_defined_fault.HazardousOutputActive".into(),
            "unknown".into(),
        );
        circuit.parts.push(control);

        let results = checks(&circuit);
        assert_eq!(results[0].verdict, Verdict::NeedsReview);
        assert!(results[0].detail.contains("user_defined_fault"));
    }

    #[test]
    fn protection_and_fuse_checks_use_declared_bounds() {
        let mut circuit = Circuit::new("protected");
        let mut protection = Part::new("Q1", "protection");
        for (key, value) in [
            ("Protection.MaximumForwardDropV", "0.12"),
            ("Protection.MaximumAllowedForwardDropV", "0.20"),
            ("Protection.ReverseWithstandV", "20"),
            ("Protection.MaximumAppliedReverseVoltageV", "16.8"),
            ("Protection.MaximumClampVoltageV", "10"),
            ("Protection.DownstreamAbsoluteMaximumV", "12"),
            ("Fuse.LoadCurrentA", "1"),
            ("Fuse.ContinuousRatingA", "2"),
            ("Fuse.ApplicationDerating", "0.75"),
            ("Fuse.InterruptRatingA", "50"),
            ("Fuse.MaximumFaultCurrentA", "20"),
            ("Fuse.Source", "manufacturer data sheet revision A"),
        ] {
            protection.fields.insert(key.into(), value.into());
        }
        circuit.parts.push(protection);

        let results = checks(&circuit);
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|result| result.verdict == Verdict::Passed));
    }
}
