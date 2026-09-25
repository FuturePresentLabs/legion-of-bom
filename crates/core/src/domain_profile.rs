//! Sandboxed domain-policy profiles.
//!
//! Rust owns the stable interface, validation, units, calculations and control
//! flow. Bundled Lua owns only bounded policy mapping: selections in; required
//! fact IDs, check IDs and unresolved evidence obligations out. Profiles get no
//! filesystem, package, network, process, clock or host callbacks.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

pub use black_book::assurance::{Applicability, EvidenceKind, EvidenceObligation, EvidenceScope};
use mlua::LuaSerdeExt;
use serde::{Deserialize, Serialize};

const AEROSPACE_LUA: &str = include_str!("../../../assets/domain_profiles/aerospace.lua");
const MEDICAL_LUA: &str = include_str!("../../../assets/domain_profiles/medical.lua");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainProfileRequest {
    #[serde(default)]
    pub selections: BTreeMap<String, String>,
    #[serde(default)]
    pub flags: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredFact {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckObligation {
    pub id: String,
    pub designation: String,
    pub applicability: Applicability,
    pub reason: String,
    pub source_locator: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainProfile {
    pub id: String,
    pub facts: Vec<RequiredFact>,
    pub checks: Vec<CheckObligation>,
    pub evidence: Vec<EvidenceObligation>,
    /// Human-readable claim boundary. The host rejects certification or
    /// compliance claims; a profile may only describe selected obligations.
    pub claim: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DomainProfileError {
    #[error("unknown bundled domain profile {0:?} (expected aerospace or medical)")]
    UnknownProfile(String),
    #[error("domain profile {profile:?} failed to load: {source}")]
    Load {
        profile: String,
        #[source]
        source: mlua::Error,
    },
    #[error("domain profile {profile:?} must define profile(request)")]
    MissingFunction { profile: String },
    #[error(
        "domain profile {profile:?} rejected the request or returned malformed output: {source}"
    )]
    Malformed {
        profile: String,
        #[source]
        source: mlua::Error,
    },
    #[error("domain profile {profile:?} produced invalid output: {reason}")]
    InvalidOutput { profile: String, reason: String },
}

fn bundled_source(name: &str) -> Result<&'static str, DomainProfileError> {
    match name {
        "aerospace" => Ok(AEROSPACE_LUA),
        "medical" => Ok(MEDICAL_LUA),
        other => Err(DomainProfileError::UnknownProfile(other.to_string())),
    }
}

/// Evaluate one named bundled profile in a deterministic Lua sandbox.
///
/// There is intentionally no `load(path)` API. Code is selected from the two
/// embedded, reviewed assets above and the Lua state exposes only table,
/// string and math operations. No Rust function is installed into Lua.
pub fn evaluate_domain_profile(
    name: &str,
    request: &DomainProfileRequest,
) -> Result<DomainProfile, DomainProfileError> {
    let source = bundled_source(name)?;
    let lua = sandboxed_lua().map_err(|source| DomainProfileError::Load {
        profile: name.to_string(),
        source,
    })?;
    lua.load(source)
        .set_name(name)
        .exec()
        .map_err(|source| DomainProfileError::Load {
            profile: name.to_string(),
            source,
        })?;
    let function = lua
        .globals()
        .get::<mlua::Value>("profile")
        .map_err(|source| DomainProfileError::Load {
            profile: name.to_string(),
            source,
        })?;
    let mlua::Value::Function(function) = function else {
        return Err(DomainProfileError::MissingFunction {
            profile: name.to_string(),
        });
    };
    let input = lua
        .to_value(request)
        .map_err(|source| DomainProfileError::Malformed {
            profile: name.to_string(),
            source,
        })?;
    let output: mlua::Value =
        function
            .call(input)
            .map_err(|source| DomainProfileError::Malformed {
                profile: name.to_string(),
                source,
            })?;
    let profile: DomainProfile =
        lua.from_value(output)
            .map_err(|source| DomainProfileError::Malformed {
                profile: name.to_string(),
                source,
            })?;
    validate_profile(name, &profile)?;
    Ok(profile)
}

fn sandboxed_lua() -> mlua::Result<mlua::Lua> {
    let lua = mlua::Lua::new_with(
        mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH,
        mlua::LuaOptions::default(),
    )?;
    // A minimal set of base functions exists independently of the requested
    // stdlib mask. Remove dynamic-code, file, process and module access
    // explicitly so a future mlua/Lua change cannot widen this boundary.
    for name in [
        "io", "os", "package", "require", "dofile", "loadfile", "load", "debug",
    ] {
        lua.globals().set(name, mlua::Value::Nil)?;
    }
    lua.set_memory_limit(4 * 1024 * 1024)?;
    let ticks = Arc::new(AtomicUsize::new(0));
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(1_000),
        move |_, _| {
            if ticks.fetch_add(1, Ordering::Relaxed) >= 100 {
                Err(mlua::Error::RuntimeError(
                    "domain profile exceeded 100000 Lua instructions".into(),
                ))
            } else {
                Ok(mlua::VmState::Continue)
            }
        },
    );
    Ok(lua)
}

fn validate_profile(name: &str, profile: &DomainProfile) -> Result<(), DomainProfileError> {
    if profile.id != name {
        return invalid(
            name,
            format!("id {:?} does not match requested profile", profile.id),
        );
    }
    let claim = profile.claim.to_ascii_lowercase();
    if claim.trim().is_empty()
        || ["compliant", "certified", "qualified", "conforms"]
            .iter()
            .any(|word| claim.contains(word))
    {
        return invalid(
            name,
            "claim is empty or overclaims compliance/certification",
        );
    }
    unique_nonempty(
        name,
        "fact",
        profile.facts.iter().map(|item| (&item.id, &item.reason)),
    )?;
    unique_nonempty(
        name,
        "check",
        profile.checks.iter().map(|item| (&item.id, &item.reason)),
    )?;
    unique_nonempty(
        name,
        "evidence",
        profile.evidence.iter().map(|item| (&item.id, &item.reason)),
    )?;
    for item in &profile.checks {
        if item.designation.trim().is_empty() || item.source_locator.trim().is_empty() {
            return invalid(
                name,
                format!("check {} lacks designation/source locator", item.id),
            );
        }
    }
    for item in &profile.evidence {
        if item.source_locator.trim().is_empty() {
            return invalid(name, format!("evidence {} lacks source locator", item.id));
        }
    }
    Ok(())
}

fn unique_nonempty<'a>(
    profile: &str,
    kind: &str,
    values: impl Iterator<Item = (&'a String, &'a String)>,
) -> Result<(), DomainProfileError> {
    let mut seen = BTreeSet::new();
    for (id, reason) in values {
        if id.trim().is_empty() || reason.trim().is_empty() {
            return invalid(profile, format!("{kind} has an empty id or reason"));
        }
        if !seen.insert(id) {
            return invalid(profile, format!("duplicate {kind} id {id:?}"));
        }
    }
    Ok(())
}

fn invalid<T>(profile: &str, reason: impl Into<String>) -> Result<T, DomainProfileError> {
    Err(DomainProfileError::InvalidOutput {
        profile: profile.to_string(),
        reason: reason.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(selections: &[(&str, &str)], flags: &[(&str, bool)]) -> DomainProfileRequest {
        DomainProfileRequest {
            selections: selections
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
            flags: flags
                .iter()
                .map(|(key, value)| ((*key).into(), *value))
                .collect(),
        }
    }

    #[test]
    fn aerospace_radiation_policy_is_mission_bounded_and_deterministic() {
        let suborbital = request(&[("mission_class", "educational_suborbital")], &[]);
        let orbit = request(&[("mission_class", "orbital_experimental")], &[]);
        let a = evaluate_domain_profile("aerospace", &suborbital).unwrap();
        let b = evaluate_domain_profile("aerospace", &suborbital).unwrap();
        assert_eq!(a, b);
        assert_eq!(
            a.checks
                .iter()
                .find(|check| check.id == "radiation_hardness_assurance")
                .unwrap()
                .applicability,
            Applicability::NotApplicable
        );
        assert_eq!(
            evaluate_domain_profile("aerospace", &orbit)
                .unwrap()
                .checks
                .iter()
                .find(|check| check.id == "radiation_hardness_assurance")
                .unwrap()
                .applicability,
            Applicability::Required
        );
    }

    #[test]
    fn medical_patient_contact_and_essential_performance_select_unresolved_evidence() {
        let profile = evaluate_domain_profile(
            "medical",
            &request(
                &[
                    ("intended_use", "monitoring"),
                    ("contact", "patient"),
                    ("applied_part", "cf"),
                    ("use_environment", "home"),
                ],
                &[("essential_performance", true)],
            ),
        )
        .unwrap();
        assert!(profile.checks.iter().any(|c| c.id == "applied_part_class"));
        assert!(profile
            .checks
            .iter()
            .any(|c| c.id == "essential_performance"));
        assert!(profile.evidence.iter().any(|e| {
            e.id == "emc_immunity_and_emissions_test" && e.kind == EvidenceKind::Test
        }));
        assert!(profile
            .evidence
            .iter()
            .any(|e| { e.id == "risk_management_file" && e.kind == EvidenceKind::Review }));
        assert!(!profile.claim.to_ascii_lowercase().contains("compli"));
    }

    #[test]
    fn aerospace_part_evidence_names_fields_without_teaching_rust_nasa_policy() {
        let profile = evaluate_domain_profile(
            "aerospace",
            &request(&[("mission_class", "sounding_rocket")], &[]),
        )
        .unwrap();
        let part = profile
            .evidence
            .iter()
            .find(|item| item.id == "eee_part_traceability")
            .unwrap();
        assert_eq!(part.scope, EvidenceScope::Part);
        assert_eq!(
            part.required_fields,
            [
                "manufacturer",
                "grade",
                "authorized_source",
                "traceability_record",
                "lot_code",
                "date_code",
            ]
        );
        assert_eq!(part.required_risks, ["pure_tin", "pem"]);
    }

    #[test]
    fn checked_in_fixtures_select_the_expected_external_evidence() {
        let aerospace: DomainProfileRequest = serde_json::from_str(include_str!(
            "../examples/fixtures/domain_aerospace_orbital.json"
        ))
        .unwrap();
        let medical: DomainProfileRequest = serde_json::from_str(include_str!(
            "../examples/fixtures/domain_medical_home_monitor.json"
        ))
        .unwrap();
        let aerospace = evaluate_domain_profile("aerospace", &aerospace).unwrap();
        let medical = evaluate_domain_profile("medical", &medical).unwrap();
        assert!(aerospace.evidence.iter().any(|item| {
            item.id == "radiation_analysis_and_test"
                && item.applicability == Applicability::Required
        }));
        assert!(medical.evidence.iter().any(|item| {
            item.id == "essential_performance_verification" && item.kind == EvidenceKind::Test
        }));
        assert!(medical
            .checks
            .iter()
            .any(|item| item.id == "medical_risk_management"));
    }

    #[test]
    fn malformed_or_unknown_requests_fail_loud() {
        assert!(matches!(
            evaluate_domain_profile("other", &request(&[], &[])),
            Err(DomainProfileError::UnknownProfile(_))
        ));
        assert!(matches!(
            evaluate_domain_profile("aerospace", &request(&[("mission_class", "invented")], &[])),
            Err(DomainProfileError::Malformed { .. })
        ));
        assert!(matches!(
            evaluate_domain_profile(
                "medical",
                &request(
                    &[
                        ("intended_use", "monitoring"),
                        ("contact", "patient"),
                        ("applied_part", "none"),
                        ("use_environment", "home")
                    ],
                    &[]
                )
            ),
            Err(DomainProfileError::Malformed { .. })
        ));
    }

    #[test]
    fn sandbox_has_no_file_network_process_or_dynamic_code_facilities() {
        let lua = sandboxed_lua().unwrap();
        for name in [
            "io", "os", "package", "require", "dofile", "loadfile", "load", "debug",
        ] {
            assert!(
                lua.globals().get::<mlua::Value>(name).unwrap().is_nil(),
                "{name}"
            );
        }
    }
}
