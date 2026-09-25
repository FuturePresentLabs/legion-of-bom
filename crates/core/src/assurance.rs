//! Product adapter for domain-neutral evidence inventory checks.
//!
//! The reusable evaluator and evidence types live in `black_book`; Lob only
//! selects a bounded Lua profile and adapts its BOM into the stable interface.

use serde::{Deserialize, Serialize};

use black_book::assurance::{assess_evidence, SpecifiedPart};
pub use black_book::assurance::{
    AsBuiltPart, EvidenceFinding as AssuranceFinding, EvidenceInput as AssuranceEvidenceInput,
    FindingLevel, PartEvidence, RiskDisposition, RiskEvidence, SubstitutionEvidence,
};

use crate::{bom::Bom, domain_profile::DomainProfile};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssuranceRequest {
    pub profile: String,
    pub profile_request: crate::domain_profile::DomainProfileRequest,
    #[serde(flatten)]
    pub evidence: AssuranceEvidenceInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssuranceManifest {
    pub schema: String,
    pub profile_id: String,
    pub claim: String,
    pub as_built_parts: Vec<AsBuiltPart>,
    pub substitutions: Vec<SubstitutionEvidence>,
    pub package_records: std::collections::BTreeMap<String, String>,
    pub findings: Vec<AssuranceFinding>,
    pub evidence_complete: bool,
    /// Always false: qualification is established by the responsible authority.
    pub process_qualified_by_lob: bool,
}

/// Adapt Lob's BOM and selected profile into black_book's generic evaluator.
#[must_use]
pub fn assess_assurance_evidence(
    bom: &Bom,
    profile: &DomainProfile,
    input: &AssuranceEvidenceInput,
) -> AssuranceManifest {
    let specified_parts: Vec<_> = bom
        .components()
        .flat_map(|line| {
            line.refdes.iter().map(|refdes| SpecifiedPart {
                refdes: refdes.clone(),
                specified_mpn: line.mpn.clone(),
            })
        })
        .collect();
    let report = assess_evidence(&specified_parts, &profile.evidence, input);
    AssuranceManifest {
        schema: "legion-of-bom.assurance.v1".into(),
        profile_id: profile.id.clone(),
        claim: profile.claim.clone(),
        as_built_parts: report.as_built_parts,
        substitutions: report.substitutions,
        package_records: report.package_records,
        findings: report.findings,
        evidence_complete: report.evidence_complete,
        process_qualified_by_lob: false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        bom::{BomLine, LineKind},
        domain_profile::{Applicability, EvidenceKind, EvidenceObligation, EvidenceScope},
    };

    fn bom() -> Bom {
        Bom {
            lines: vec![BomLine {
                kind: LineKind::Component,
                mpn: Some("ABC123".into()),
                value: "IC".into(),
                footprint: Some("QFN".into()),
                refdes: vec!["U1".into()],
                unit_price: None,
                ext_price: None,
                image_url: None,
            }],
        }
    }

    fn obligation(id: &str, scope: EvidenceScope, fields: &[&str]) -> EvidenceObligation {
        EvidenceObligation {
            id: id.into(),
            kind: EvidenceKind::Supplier,
            scope,
            applicability: Applicability::Required,
            reason: "test requirement".into(),
            source_locator: "test source".into(),
            required_fields: fields.iter().map(|field| (*field).into()).collect(),
            required_risks: vec![],
        }
    }

    fn profile(id: &str) -> DomainProfile {
        DomainProfile {
            id: id.into(),
            facts: vec![],
            checks: vec![],
            evidence: vec![
                obligation("part_trace", EvidenceScope::Part, &["lot_code"]),
                obligation("inspection", EvidenceScope::Package, &[]),
            ],
            claim: "Selected evidence obligations only.".into(),
        }
    }

    fn complete_input() -> AssuranceEvidenceInput {
        AssuranceEvidenceInput {
            parts: vec![PartEvidence {
                refdes: vec!["U1".into()],
                mpn: "ABC123".into(),
                fields: BTreeMap::from([("lot_code".into(), "L7".into())]),
                records: BTreeMap::from([("part_trace".into(), "CoC-8".into())]),
                risks: BTreeMap::new(),
            }],
            records: BTreeMap::from([("inspection".into(), "INSP-1".into())]),
            ..Default::default()
        }
    }

    #[test]
    fn adapter_preserves_claim_boundary() {
        let report = assess_assurance_evidence(&bom(), &profile("aerospace"), &complete_input());
        assert!(report.evidence_complete, "{:#?}", report.findings);
        assert!(!report.process_qualified_by_lob);
        assert_eq!(report.schema, "legion-of-bom.assurance.v1");
    }

    #[test]
    fn same_adapter_accepts_medical_profile() {
        let report = assess_assurance_evidence(&bom(), &profile("medical"), &complete_input());
        assert!(report.evidence_complete);
        assert_eq!(report.profile_id, "medical");
    }

    #[test]
    fn sidecar_selects_profile_and_carries_generic_evidence() {
        let request: AssuranceRequest = toml::from_str(
            r#"
profile = "aerospace"

[profile_request.selections]
mission_class = "educational_suborbital"

[[parts]]
refdes = ["U1"]
mpn = "ABC123"

[parts.fields]
lot_code = "L7"

[parts.records]
part_trace = "CoC-8"

[records]
inspection = "INSP-1"
"#,
        )
        .unwrap();
        assert_eq!(request.profile, "aerospace");
        assert_eq!(request.evidence.parts[0].fields["lot_code"], "L7");
        assert_eq!(request.evidence.records["inspection"], "INSP-1");
    }
}
