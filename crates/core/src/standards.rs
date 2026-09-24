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
        self.results
            .iter()
            .any(|result| result.verdict == Verdict::NeedsTest)
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
                id: "usb-type-c-2.0-sink",
                ..
            }) => Ok(verify_usb_type_c_sink(circuit)),
            Some(_) => Err(StandardsError::NotImplemented(id.clone())),
        })
        .collect()
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
}
