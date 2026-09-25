//! Standards a brief may require and Legion can check without overclaiming.
//!
//! Mirrors Transmog's standards contract: the catalog says what exists, each
//! requirement says whether an artifact, a proxy, or a physical test settles
//! it, and reports never turn `TestOnly` into a design pass.

use serde::{Deserialize, Serialize};

use crate::model::is_ground_net;
use crate::source::CircuitSource;
use crate::units::parse_eng_value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verifiable {
    Artifact,
    Proxy,
    TestOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Status {
    Implemented { module: &'static str },
    Planned { issue: &'static str },
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Requirement {
    pub aspect: &'static str,
    pub verifiable: Verifiable,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Standard {
    pub id: &'static str,
    pub designation: &'static str,
    pub title: &'static str,
    pub brief_example: &'static str,
    pub requirements: &'static [Requirement],
    pub status: Status,
    pub public: bool,
}

use Verifiable::{Artifact, Proxy, TestOnly};

pub const CATALOG: &[Standard] = &[
    Standard {
        id: "embedded-digital-black-book",
        designation: "Puget artifact-visible embedded/audio/RF engineering profile v1",
        title: "Deterministic power, decoupling, clock, interface and RF-macro checks",
        brief_example: "must pass the embedded digital black-book topology checks",
        requirements: &[
            Requirement { aspect: "typed-pin connectivity and driver compatibility", verifiable: Artifact },
            Requirement { aspect: "source-to-load power reachability and declared current budgets", verifiable: Artifact },
            Requirement { aspect: "MCU, codec and radio supply decoupling topology", verifiable: Artifact },
            Requirement { aspect: "clock-source topology", verifiable: Artifact },
            Requirement { aspect: "digital interface pin bindings", verifiable: Artifact },
            Requirement { aspect: "cited SX1262 reference-macro integrity", verifiable: Artifact },
            Requirement { aspect: "physical electrical, SI/PI, EMC and RF performance", verifiable: TestOnly },
        ],
        status: Status::Implemented { module: "legion_of_bom_core::engineering::verify" },
        public: true,
    },
    Standard {
        id: "usb-type-c-2.0-sink",
        designation: "USB Type-C Cable and Connector Specification, Release 2.0 (2019)",
        title: "USB Type-C receptacle used as a power sink",
        brief_example: "USB-C-powered sink compliant with USB Type-C Release 2.0",
        requirements: &[
            Requirement {
                aspect: "CC1 and CC2 independently terminated to ground through Rd",
                verifiable: Artifact,
            },
            Requirement {
                aspect: "electrical behavior and interoperability",
                verifiable: TestOnly,
            },
        ],
        status: Status::Implemented {
            module: "legion_of_bom_core::standards::verify_usb_type_c_sink",
        },
        public: true,
    },
    Standard {
        id: "mil-std-3001-1a-schematic",
        designation: "MIL-STD-3001-1A, Change 2 (2021), clauses B.5.5.9 and B.5.5.13",
        title: "DoD technical-manual schematic presentation subset",
        brief_example: "schematic presentation checked against the MIL-STD-3001-1A signal-flow and callout subset",
        requirements: &[
            Requirement {
                aspect: "significant circuit features identified by reference designator and nomenclature",
                verifiable: Artifact,
            },
            Requirement {
                aspect: "major signal flow proceeds left to right where identifiable",
                verifiable: Artifact,
            },
            Requirement {
                aspect: "generated-page congestion and narrative separation",
                verifiable: Proxy,
            },
        ],
        status: Status::Implemented {
            module: "legion_of_bom_core::standards::verify_mil_std_3001_schematic",
        },
        public: true,
    },
    Standard {
        id: "ecss-q-st-70-12c-rev1-rigid-30v",
        designation: "ECSS-Q-ST-70-12C Rev.1 (2025), bounded rigid <=30 V design profile",
        title: "Artifact-checkable aerospace PCB geometry and explicit supplier evidence",
        brief_example: "ECSS aerospace PCB geometry for a rigid <=30 V board",
        requirements: &[
            Requirement {
                aspect: "rigid-board track, spacing, edge, annular-ring and through-via geometry",
                verifiable: Artifact,
            },
            Requirement {
                aspect: "as-manufactured dimensions and process capability",
                verifiable: TestOnly,
            },
            Requirement {
                aspect: "representative test coupons, inspection and traceability",
                verifiable: TestOnly,
            },
        ],
        status: Status::Implemented {
            module: "legion_of_bom_core::fab::ecss_q_st_70_12c_rev1_rigid_30v_design_rules",
        },
        public: true,
    },
    Standard {
        id: "ipc-2221c",
        designation: "IPC-2221C",
        title: "Generic Standard on Printed Board Design",
        brief_example: "IPC-2221C board design",
        requirements: &[Requirement {
            aspect: "board geometry and electrical spacing",
            verifiable: Artifact,
        }],
        status: Status::Planned {
            issue: "legion-of-bom-4oy2",
        },
        public: false,
    },
    Standard {
        id: "iec-62368-1",
        designation: "IEC 62368-1",
        title: "Audio/video and ICT equipment safety",
        brief_example: "designed for IEC 62368-1 pre-compliance",
        requirements: &[
            Requirement {
                aspect: "energy-source and safeguard design screen",
                verifiable: Proxy,
            },
            Requirement {
                aspect: "product safety certification",
                verifiable: TestOnly,
            },
        ],
        status: Status::Planned {
            issue: "legion-of-bom-4oy2",
        },
        public: false,
    },
];

#[must_use]
pub fn find(id: &str) -> Option<&'static Standard> {
    CATALOG.iter().find(|standard| standard.id == id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Passed,
    Failed,
    NeedsTest,
    NeedsReview,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub aspect: String,
    pub verifiable: Verifiable,
    pub verdict: Verdict,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandardReport {
    pub standard: String,
    pub designation: String,
    pub results: Vec<CheckResult>,
}

impl StandardReport {
    #[must_use]
    pub fn design_passes(&self) -> bool {
        self.results.iter().all(|result| {
            result.verdict != Verdict::Failed || result.verifiable == Verifiable::TestOnly
        })
    }

    #[must_use]
    pub fn needs_physical_test(&self) -> bool {
        self.results.iter().any(|result| {
            result.verdict == Verdict::NeedsTest && result.verifiable == Verifiable::TestOnly
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StandardsError {
    #[error("unknown engineering standard/profile '{0}'")]
    Unknown(String),
    #[error("standard/profile '{0}' is catalogued but its check is not implemented")]
    NotImplemented(String),
}

pub fn verify(
    circuit: &dyn CircuitSource,
    required: &[String],
) -> Result<Vec<StandardReport>, StandardsError> {
    required
        .iter()
        .map(|id| match find(id) {
            None => Err(StandardsError::Unknown(id.clone())),
            Some(Standard {
                status: Status::Planned { .. },
                ..
            }) => Err(StandardsError::NotImplemented(id.clone())),
            Some(Standard {
                id: "embedded-digital-black-book",
                ..
            }) => Ok(crate::engineering::verify(circuit)),
            Some(Standard {
                id: "usb-type-c-2.0-sink",
                ..
            }) => Ok(verify_usb_type_c_sink(circuit)),
            Some(Standard {
                id: "mil-std-3001-1a-schematic",
                ..
            }) => Ok(verify_mil_std_3001_schematic(circuit)),
            Some(Standard {
                id: "ecss-q-st-70-12c-rev1-rigid-30v",
                ..
            }) => Ok(verify_ecss_q_st_70_12_rigid_30v()),
            Some(_) => Err(StandardsError::NotImplemented(id.clone())),
        })
        .collect()
}

/// Report the claim boundary for the ECSS aerospace PCB geometry profile.
///
/// Circuit-only verification cannot prove that a `.kicad_pcb` passed the
/// generated `.kicad_dru`, so it returns `NeedsReview`, never a synthetic pass.
/// The board/fab pipeline is responsible for attaching the rule artifact and
/// running KiCad DRC. Physical manufacture and coupon inspection remain
/// `TestOnly` even after DRC is clean.
///
/// @derives-from url:https://ecss.nl/wp-content/uploads/2025/05/ECSS-Q-ST-70-12C-Rev.1%2830April2025%29.pdf §§7.3-7.5, 13.8 and 15 -- separates CAD-checkable geometry from supplier/process evidence; not whole-standard conformity
fn verify_ecss_q_st_70_12_rigid_30v() -> StandardReport {
    StandardReport {
        standard: "ecss-q-st-70-12c-rev1-rigid-30v".into(),
        designation:
            "ECSS-Q-ST-70-12C Rev.1 (2025), bounded rigid <=30 V design profile".into(),
        results: vec![
            CheckResult {
                aspect: "rigid-board track, spacing, edge, annular-ring and through-via geometry"
                    .into(),
                verifiable: Artifact,
                verdict: Verdict::NeedsReview,
                detail: "attach ecss_q_st_70_12c_rev1_rigid_30v_design_rules() beside the KiCad board and require a clean KiCad DRC; applicable only to rigid epoxy, normal-pitch outer copper <=70 µm, <=2.2 mm board thickness and <=30 V worst-case peak; this is not whole-standard conformity".into(),
            },
            CheckResult {
                aspect: "as-manufactured dimensions and process capability".into(),
                verifiable: TestOnly,
                verdict: Verdict::NeedsTest,
                detail: "ECSS-Q-ST-70-12C Rev.1 clauses 7.4.2, 7.5.3 and 13.8 require supplier tolerance/process evidence and manufactured-dimension inspection; CAD DRC cannot settle these obligations".into(),
            },
            CheckResult {
                aspect: "representative test coupons, inspection and traceability".into(),
                verifiable: TestOnly,
                verdict: Verdict::NeedsTest,
                detail: "ECSS-Q-ST-70-12C Rev.1 clauses 15.1-15.2 require supplier-reviewed representative coupons, applicable tests, inspection and coupon-to-panel traceability".into(),
            },
        ],
    }
}

/// Verify the artifact-visible subset of MIL-STD-3001-1A's schematic rules.
///
/// This profile is intentionally narrow.  MIL-STD-3001-1A governs technical
/// manuals for the covered DoD systems; passing this function is not a claim
/// that a board, drawing set, or publication conforms to the whole standard.
/// The congestion limits are named as Puget proxies in the report because the
/// source requires understandable diagrams but does not specify these numbers.
///
/// @derives-from url:https://quicksearch.dla.mil/qsDocDetails.aspx?ident_number=280652 MIL-STD-3001-1A §§B.5.5.9, B.5.5.13 -- artifact-checkable schematic subset, not whole-standard conformity
fn verify_mil_std_3001_schematic(circuit: &dyn CircuitSource) -> StandardReport {
    use std::collections::HashSet;

    let mut callout_failures = Vec::new();
    let mut seen = HashSet::new();
    for part in circuit.parts() {
        if part.refdes.0.trim().is_empty() {
            callout_failures.push("a part has no reference designator".to_string());
        } else if !seen.insert(part.refdes.0.to_ascii_uppercase()) {
            callout_failures.push(format!("duplicate reference designator {}", part.refdes));
        }
        if part.value.trim().is_empty() {
            callout_failures.push(format!("{} has no value/nomenclature", part.refdes));
        }
    }
    let callouts = CheckResult {
        aspect: "significant circuit features identified by reference designator and nomenclature"
            .into(),
        verifiable: Artifact,
        verdict: if callout_failures.is_empty() {
            Verdict::Passed
        } else {
            Verdict::Failed
        },
        detail: if callout_failures.is_empty() {
            format!(
                "all {} parts have unique reference designators and displayed nomenclature",
                circuit.parts().len()
            )
        } else {
            callout_failures.join("; ")
        },
    };

    let readability = crate::schematic::analyze_readability(circuit);
    let flow = if !readability.flow_is_identifiable {
        CheckResult {
            aspect: "major signal flow proceeds left to right where identifiable".into(),
            verifiable: Artifact,
            verdict: Verdict::NeedsReview,
            detail: "no recognized input/output net pair; a person must identify the major flow before this clause can be evaluated".into(),
        }
    } else if readability.major_signal_flow_is_left_to_right {
        CheckResult {
            aspect: "major signal flow proceeds left to right where identifiable".into(),
            verifiable: Artifact,
            verdict: Verdict::Passed,
            detail:
                "recognized input-to-output flow is ordered left to right in the generated layout"
                    .into(),
        }
    } else {
        CheckResult {
            aspect: "major signal flow proceeds left to right where identifiable".into(),
            verifiable: Artifact,
            verdict: Verdict::Failed,
            detail: "recognized output appears left of an input in the generated layout".into(),
        }
    };

    const MAX_PARTS_PER_COLUMN: usize = 8;
    const MAX_TRUNKS_PER_CHANNEL: usize = 8;
    let mut proxy_failures = Vec::new();
    if readability.max_parts_in_column > MAX_PARTS_PER_COLUMN {
        proxy_failures.push(format!(
            "{} parts share one column (Puget limit {MAX_PARTS_PER_COLUMN})",
            readability.max_parts_in_column
        ));
    }
    if readability.max_signal_trunks_in_channel > MAX_TRUNKS_PER_CHANNEL {
        proxy_failures.push(format!(
            "{} signal trunks share one channel (Puget limit {MAX_TRUNKS_PER_CHANNEL})",
            readability.max_signal_trunks_in_channel
        ));
    }
    if !readability.disconnected_parts.is_empty() {
        proxy_failures.push(format!(
            "electrically disconnected parts enter the schematic narrative: {}",
            readability.disconnected_parts.join(", ")
        ));
    }
    let congestion = CheckResult {
        aspect: "generated-page congestion and narrative separation".into(),
        verifiable: Proxy,
        verdict: if proxy_failures.is_empty() {
            Verdict::Passed
        } else {
            Verdict::Failed
        },
        detail: if proxy_failures.is_empty() {
            format!(
                "Puget proxy passes: at most {} parts/column and {} signal trunks/channel; no disconnected parts",
                readability.max_parts_in_column, readability.max_signal_trunks_in_channel
            )
        } else {
            format!(
                "Puget readability proxy, not a MIL-STD numeric limit: {}",
                proxy_failures.join("; ")
            )
        },
    };

    StandardReport {
        standard: "mil-std-3001-1a-schematic".into(),
        designation: "MIL-STD-3001-1A, Change 2 (2021), clauses B.5.5.9 and B.5.5.13".into(),
        results: vec![callouts, flow, congestion],
    }
}

/// Verify the topology-visible portion of a USB Type-C sink receptacle.
///
/// @implements url:https://www.usb.org/sites/default/files/USB%20Type-C%20Spec%20R2.0%20-%20August%202019.pdf §4.5.3.2.1, Table 4-25 -- artifact check only; compliance testing remains explicit
fn verify_usb_type_c_sink(circuit: &dyn CircuitSource) -> StandardReport {
    let mut failures = Vec::new();
    let connectors: Vec<_> = circuit
        .parts()
        .iter()
        .filter(|part| {
            part.footprint
                .as_deref()
                .is_some_and(|f| f.contains("USB_C_Receptacle"))
                || part.value.to_ascii_uppercase().contains("TYPE-C")
        })
        .collect();

    if connectors.is_empty() {
        failures.push("no USB Type-C receptacle was present".to_string());
    }
    for connector in connectors {
        let cc: Vec<_> = ["A5", "B5"]
            .iter()
            .map(|pin| {
                circuit
                    .nets()
                    .iter()
                    .find(|net| {
                        net.pins.iter().any(|p| {
                            p.refdes == connector.refdes && p.pin.eq_ignore_ascii_case(pin)
                        })
                    })
                    .map(|net| (pin, net))
            })
            .collect();
        if cc.iter().any(Option::is_none) {
            failures.push(format!(
                "{} does not expose both CC1/A5 and CC2/B5",
                connector.refdes
            ));
            continue;
        }
        let cc: Vec<_> = cc.into_iter().flatten().collect();
        if cc[0].1.name == cc[1].1.name {
            failures.push(format!(
                "{} CC1 and CC2 are not independently terminated",
                connector.refdes
            ));
        }
        for (pin, net) in cc {
            let rd = circuit.parts().iter().find(|part| {
                part.refdes.0.starts_with('R')
                    && parse_eng_value(&part.value)
                        .is_some_and(|ohms| (4_080.0..=6_120.0).contains(&ohms))
                    && net.pins.iter().any(|p| p.refdes == part.refdes)
                    && circuit.nets().iter().any(|other| {
                        is_ground_net(&other.name)
                            && other.pins.iter().any(|p| p.refdes == part.refdes)
                    })
            });
            if rd.is_none() {
                failures.push(format!(
                    "{} {pin} has no independent 5.1 kΩ ±20% pull-down to ground",
                    connector.refdes
                ));
            }
        }
    }

    let artifact = if failures.is_empty() {
        CheckResult {
            aspect: "CC1 and CC2 independently terminated to ground through Rd".into(),
            verifiable: Artifact,
            verdict: Verdict::Passed,
            detail: "each receptacle CC pin has a distinct 5.1 kΩ ±20% path to ground".into(),
        }
    } else {
        CheckResult {
            aspect: "CC1 and CC2 independently terminated to ground through Rd".into(),
            verifiable: Artifact,
            verdict: Verdict::Failed,
            detail: failures.join("; "),
        }
    };
    StandardReport {
        standard: "usb-type-c-2.0-sink".into(),
        designation: "USB Type-C Cable and Connector Specification, Release 2.0 (2019)".into(),
        results: vec![
            artifact,
            CheckResult {
                aspect: "electrical behavior and interoperability".into(),
                verifiable: TestOnly,
                verdict: Verdict::NeedsTest,
                detail: "requires the applicable USB-IF electrical/functional compliance tests; no certification is claimed".into(),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef};

    fn sink(shared_cc: bool, rd: &str) -> Circuit {
        let mut circuit = Circuit::new("usb sink");
        circuit.parts = vec![
            Part::new("J1", "TYPE-C-31-M-12")
                .with_footprint("Connector_USB:USB_C_Receptacle_HRO_TYPE-C-31-M-12"),
            Part::new("R1", rd),
            Part::new("R2", "5.1k"),
        ];
        let cc1 = Net::new("CC1", vec![PinRef::new("J1", "A5"), PinRef::new("R1", "1")]);
        let cc2 = if shared_cc {
            Net::new("CC1", vec![PinRef::new("J1", "B5"), PinRef::new("R2", "1")])
        } else {
            Net::new("CC2", vec![PinRef::new("J1", "B5"), PinRef::new("R2", "1")])
        };
        circuit.nets = vec![
            cc1,
            cc2,
            Net::new("GND", vec![PinRef::new("R1", "2"), PinRef::new("R2", "2")]),
        ];
        circuit
    }

    #[test]
    fn valid_sink_passes_artifact_check_but_still_needs_physical_testing() {
        let reports = verify(&sink(false, "5.1k"), &["usb-type-c-2.0-sink".into()]).unwrap();
        assert!(reports[0].design_passes());
        assert!(reports[0].needs_physical_test());
        assert_eq!(reports[0].results[0].verdict, Verdict::Passed);
    }

    #[test]
    fn missing_or_shared_rd_fails_loudly() {
        let reports = verify(&sink(true, "10k"), &["usb-type-c-2.0-sink".into()]).unwrap();
        assert!(!reports[0].design_passes());
        assert!(reports[0].results[0].detail.contains("independently"));
        assert!(reports[0].results[0].detail.contains("pull-down"));
    }

    #[test]
    fn a_planned_standard_is_not_reported_as_checked() {
        assert!(matches!(
            verify(&sink(false, "5.1k"), &["ipc-2221c".into()]),
            Err(StandardsError::NotImplemented(_))
        ));
    }

    #[test]
    fn aerospace_pcb_profile_never_turns_circuit_only_evidence_into_compliance() {
        let reports = verify(
            &sink(false, "5.1k"),
            &["ecss-q-st-70-12c-rev1-rigid-30v".into()],
        )
        .unwrap();
        let report = &reports[0];
        assert_eq!(report.results.len(), 3);
        assert_eq!(report.results[0].verifiable, Verifiable::Artifact);
        assert_eq!(report.results[0].verdict, Verdict::NeedsReview);
        assert_eq!(report.results[1].verifiable, Verifiable::TestOnly);
        assert_eq!(report.results[1].verdict, Verdict::NeedsTest);
        assert_eq!(report.results[2].verifiable, Verifiable::TestOnly);
        assert_eq!(report.results[2].verdict, Verdict::NeedsTest);
        assert!(report.needs_physical_test());
        assert!(report.results.iter().all(|result| {
            !result.detail.to_ascii_lowercase().contains("compliant")
                && !result.detail.to_ascii_lowercase().contains("certified")
        }));
    }

    #[test]
    fn aerospace_pcb_catalog_exposes_exact_claim_classes() {
        let profile = find("ecss-q-st-70-12c-rev1-rigid-30v").unwrap();
        assert!(matches!(profile.status, Status::Implemented { .. }));
        assert_eq!(profile.requirements.len(), 3);
        assert_eq!(profile.requirements[0].verifiable, Verifiable::Artifact);
        assert_eq!(profile.requirements[1].verifiable, Verifiable::TestOnly);
        assert_eq!(profile.requirements[2].verifiable, Verifiable::TestOnly);
    }

    fn readable_signal_chain() -> Circuit {
        let mut circuit = Circuit::new("readable chain");
        circuit.parts = vec![
            Part::new("J1", "INPUT"),
            Part::new("R1", "1k"),
            Part::new("U1", "BUFFER"),
            Part::new("J2", "OUTPUT"),
        ];
        circuit.nets = vec![
            Net::new("IN", vec![PinRef::new("J1", "1"), PinRef::new("R1", "1")]),
            Net::new("MID", vec![PinRef::new("R1", "2"), PinRef::new("U1", "1")]),
            Net::new("OUT", vec![PinRef::new("U1", "2"), PinRef::new("J2", "1")]),
        ];
        circuit
    }

    #[test]
    fn schematic_profile_checks_callouts_and_left_to_right_flow() {
        let reports = verify(
            &readable_signal_chain(),
            &["mil-std-3001-1a-schematic".into()],
        )
        .unwrap();
        assert!(reports[0].design_passes());
        assert_eq!(reports[0].results[0].verdict, Verdict::Passed);
        assert_eq!(reports[0].results[1].verdict, Verdict::Passed);
        assert_eq!(reports[0].results[2].verdict, Verdict::Passed);
        assert!(!reports[0].needs_physical_test());
    }

    #[test]
    fn schematic_profile_labels_unassessable_flow_as_review_not_pass() {
        let mut circuit = Circuit::new("unnamed flow");
        circuit.parts = vec![Part::new("R1", "1k"), Part::new("R2", "2k")];
        circuit.nets = vec![Net::new(
            "N$1",
            vec![PinRef::new("R1", "2"), PinRef::new("R2", "1")],
        )];
        let reports = verify(&circuit, &["mil-std-3001-1a-schematic".into()]).unwrap();
        assert_eq!(reports[0].results[1].verdict, Verdict::NeedsReview);
        assert!(!reports[0].results[1].detail.contains("pass"));
    }

    #[test]
    fn schematic_profile_fails_local_congestion_proxy_without_calling_it_military() {
        let mut circuit = readable_signal_chain();
        for index in 1..=9 {
            circuit.parts.push(Part::new(format!("C{index}"), "100n"));
            circuit.nets.push(Net::new(
                format!("PWR{index}"),
                vec![PinRef::new(format!("C{index}"), "1")],
            ));
        }
        let reports = verify(&circuit, &["mil-std-3001-1a-schematic".into()]).unwrap();
        let proxy = &reports[0].results[2];
        assert_eq!(proxy.verifiable, Proxy);
        assert_eq!(proxy.verdict, Verdict::Failed);
        assert!(proxy.detail.contains("Puget readability proxy"));
        assert!(!reports[0].design_passes());
    }
}
