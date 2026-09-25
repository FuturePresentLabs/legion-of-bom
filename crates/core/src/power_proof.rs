//! Product-facing adapters for deterministic power evidence.
//!
//! Inputs are raw extracted facts; outputs are diagnostics, not certifications.

use black_book::electronics_power::{
    ldo_envelope, usb_c_power_sink_envelope, LdoEnvelope, LdoEnvelopeInput, UsbCPowerSinkEnvelope,
    UsbCPowerSinkInput,
};

#[derive(Debug, Clone, PartialEq)]
pub struct PowerFinding {
    pub obligation: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverseBackfeedEvidence {
    Prohibited,
    ProtectedBySourcedCircuit,
    Missing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LdoRawEvidence {
    pub erc_errors: Vec<String>,
    pub power_tree_connected: bool,
    pub enable_tied_to_input: bool,
    pub input_effective_capacitance_f: Option<f64>,
    pub output_effective_capacitance_f: Option<f64>,
    pub minimum_input_capacitance_f: Option<f64>,
    pub minimum_output_capacitance_f: Option<f64>,
    pub envelope: LdoEnvelopeInput,
    pub pcb_applicable_thermal_evidence: bool,
    pub reverse_backfeed: ReverseBackfeedEvidence,
    pub exact_bom_identity: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LdoProofReport {
    pub envelope: LdoEnvelope,
    pub findings: Vec<PowerFinding>,
}

pub fn evaluate_ldo(evidence: &LdoRawEvidence) -> LdoProofReport {
    let envelope = ldo_envelope(evidence.envelope);
    let mut findings = Vec::new();
    if !evidence.erc_errors.is_empty() {
        push(&mut findings, "erc_clean", evidence.erc_errors.join("; "));
    }
    if !evidence.power_tree_connected {
        push(
            &mut findings,
            "power_tree_connectivity",
            "input, output, and return topology is not proven",
        );
    }
    if !evidence.enable_tied_to_input {
        push(
            &mut findings,
            "enable_state",
            "enable is not tied to the input rail",
        );
    }
    let capacitance_ok = |actual: Option<f64>, required: Option<f64>| matches!((actual, required), (Some(a), Some(r)) if a >= r);
    if !capacitance_ok(
        evidence.input_effective_capacitance_f,
        evidence.minimum_input_capacitance_f,
    ) || !capacitance_ok(
        evidence.output_effective_capacitance_f,
        evidence.minimum_output_capacitance_f,
    ) {
        push(
            &mut findings,
            "local_capacitance",
            "effective capacitance or sourced minimum is missing/insufficient",
        );
    }
    if !envelope.input_within_absolute_max || !envelope.load_within_rating {
        push(
            &mut findings,
            "voltage_current_envelope",
            "input voltage or load exceeds sourced rating",
        );
    }
    if !envelope.dropout_proven {
        push(
            &mut findings,
            "dropout_headroom",
            "maximum dropout rating is missing or exceeds headroom",
        );
    }
    if !evidence.pcb_applicable_thermal_evidence {
        push(&mut findings, "dissipation_and_thermal", format!(
            "{:.6} W worst-case dissipation calculated; junction temperature unresolved without PCB-applicable evidence",
            envelope.worst_case_dissipation_w
        ));
    }
    if evidence.reverse_backfeed == ReverseBackfeedEvidence::Missing {
        push(
            &mut findings,
            "reverse_backfeed",
            "reverse-powered output is neither prohibited nor protected with sourced evidence",
        );
    }
    if !evidence.exact_bom_identity {
        push(
            &mut findings,
            "bom_identity",
            "one or more fitted parts lack exact MPN/LCSC identity",
        );
    }
    LdoProofReport { envelope, findings }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsbCRawEvidence {
    pub erc_errors: Vec<String>,
    pub all_power_contacts_connected: bool,
    pub data_contacts_explicitly_unused: bool,
    pub shield_strategy_documented: bool,
    pub series_fuse_topology: bool,
    pub unidirectional_tvs_to_ground: bool,
    pub envelope: UsbCPowerSinkInput,
    pub tvs_clamp_below_downstream_abs_max: Option<bool>,
    pub exact_bom_identity: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsbCProofReport {
    pub envelope: UsbCPowerSinkEnvelope,
    pub findings: Vec<PowerFinding>,
}

pub fn evaluate_usb_c_sink(evidence: &UsbCRawEvidence) -> UsbCProofReport {
    let envelope = usb_c_power_sink_envelope(evidence.envelope);
    let mut findings = Vec::new();
    if !evidence.erc_errors.is_empty() {
        push(&mut findings, "erc_clean", evidence.erc_errors.join("; "));
    }
    if !evidence.all_power_contacts_connected || !evidence.series_fuse_topology {
        push(
            &mut findings,
            "power_contact_coverage",
            "VBUS/GND contact coverage or series-fuse path is incomplete",
        );
    }
    if !envelope.independent_rd_in_range {
        push(
            &mut findings,
            "cc_terminations",
            "both independent CC Rd values are not proven in range",
        );
    }
    if !evidence.data_contacts_explicitly_unused {
        push(
            &mut findings,
            "power_only_data_state",
            "USB data contacts are not explicitly unused",
        );
    }
    if !envelope.current_budget_ok {
        push(
            &mut findings,
            "default_current_budget",
            "continuous load exceeds evidenced available current",
        );
    }
    if !envelope.fuse_hold_proven || !evidence.series_fuse_topology {
        push(
            &mut findings,
            "fuse_envelope",
            "fuse topology or hold current at maximum ambient is unproven",
        );
    }
    if !envelope.tvs_standoff_proven
        || !evidence.unidirectional_tvs_to_ground
        || evidence.tvs_clamp_below_downstream_abs_max != Some(true)
    {
        push(
            &mut findings,
            "tvs_envelope",
            "TVS topology, standoff, or clamp bound is unproven",
        );
    }
    if !evidence.shield_strategy_documented {
        push(
            &mut findings,
            "shield_strategy",
            "connector shield strategy is missing",
        );
    }
    if !evidence.exact_bom_identity {
        push(
            &mut findings,
            "bom_identity",
            "one or more fitted parts lack exact MPN/LCSC identity",
        );
    }
    UsbCProofReport { envelope, findings }
}

fn push(findings: &mut Vec<PowerFinding>, obligation: &'static str, message: impl Into<String>) {
    findings.push(PowerFinding {
        obligation,
        message: message.into(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ldo() -> LdoRawEvidence {
        LdoRawEvidence {
            erc_errors: vec![],
            power_tree_connected: true,
            enable_tied_to_input: true,
            input_effective_capacitance_f: Some(1e-6),
            output_effective_capacitance_f: Some(1e-6),
            minimum_input_capacitance_f: Some(1e-6),
            minimum_output_capacitance_f: Some(1e-6),
            envelope: LdoEnvelopeInput {
                input_min_v: 4.75,
                input_max_v: 5.25,
                output_v: 3.3,
                load_a: 0.3,
                quiescent_max_a: 25e-6,
                input_abs_max_v: 5.5,
                output_current_rating_a: 0.5,
                dropout_max_v: Some(0.238),
            },
            pcb_applicable_thermal_evidence: true,
            reverse_backfeed: ReverseBackfeedEvidence::Prohibited,
            exact_bom_identity: true,
        }
    }

    #[test]
    fn ldo_mutations_are_owned_and_missing_ratings_do_not_pass() {
        assert!(evaluate_ldo(&ldo()).findings.is_empty());
        let mut bad = ldo();
        bad.output_effective_capacitance_f = None;
        assert!(evaluate_ldo(&bad)
            .findings
            .iter()
            .any(|f| f.obligation == "local_capacitance"));
        let mut bad = ldo();
        bad.envelope.dropout_max_v = None;
        assert!(evaluate_ldo(&bad)
            .findings
            .iter()
            .any(|f| f.obligation == "dropout_headroom"));
        let mut bad = ldo();
        bad.pcb_applicable_thermal_evidence = false;
        assert!(evaluate_ldo(&bad)
            .findings
            .iter()
            .any(|f| f.obligation == "dissipation_and_thermal"));
    }

    fn usb() -> UsbCRawEvidence {
        UsbCRawEvidence {
            erc_errors: vec![],
            all_power_contacts_connected: true,
            data_contacts_explicitly_unused: true,
            shield_strategy_documented: true,
            series_fuse_topology: true,
            unidirectional_tvs_to_ground: true,
            envelope: UsbCPowerSinkInput {
                cc1_rd_ohm: Some(5100.0),
                cc2_rd_ohm: Some(5100.0),
                continuous_load_a: 0.4,
                advertised_current_a: None,
                default_current_budget_a: 0.5,
                fuse_hold_at_max_ambient_a: Some(0.5),
                tvs_standoff_v: Some(5.5),
                maximum_vbus_v: 5.25,
            },
            tvs_clamp_below_downstream_abs_max: Some(true),
            exact_bom_identity: true,
        }
    }

    #[test]
    fn usb_mutations_trip_independent_obligations() {
        assert!(evaluate_usb_c_sink(&usb()).findings.is_empty());
        let mut bad = usb();
        bad.envelope.cc2_rd_ohm = None;
        assert!(evaluate_usb_c_sink(&bad)
            .findings
            .iter()
            .any(|f| f.obligation == "cc_terminations"));
        let mut bad = usb();
        bad.envelope.continuous_load_a = 0.9;
        assert!(evaluate_usb_c_sink(&bad)
            .findings
            .iter()
            .any(|f| f.obligation == "default_current_budget"));
        let mut bad = usb();
        bad.tvs_clamp_below_downstream_abs_max = None;
        assert!(evaluate_usb_c_sink(&bad)
            .findings
            .iter()
            .any(|f| f.obligation == "tvs_envelope"));
    }
}
