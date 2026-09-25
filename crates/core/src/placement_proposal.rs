//! Bounded numeric placement proposals, shared by GPC-1 and an LLM fallback.
//!
//! Lob derives every field and bound. A predictor may choose values only inside
//! that domain; applying, snapping, and validating them remains host work. The
//! fallback prompt embeds the exact response schema used to validate its reply,
//! so the slower model sees the same contract as the native bounded path.

use std::collections::{BTreeMap, BTreeSet};

use ooda::{BoundedError, BoundedPredictor, Complete, NumericField, NumericRequest, Prompt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// One host-derived placement degree of freedom.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacementField {
    pub key: String,
    pub description: String,
    pub minimum: f64,
    pub maximum: f64,
    pub unit: String,
    /// Coordinate frame or datum the value is relative to.
    pub reference: String,
}

/// The complete domain offered to either prediction backend.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacementProposalRequest {
    pub observation: Value,
    pub instruction: String,
    pub fields: Vec<PlacementField>,
    pub correlation: Option<String>,
}

/// Backend-independent values. These are proposals, never authoritative board
/// coordinates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacementProposal {
    pub values: BTreeMap<String, f64>,
    pub evidence: ProposalEvidence,
}

/// Observable backend facts for eval and cost accounting. Cost is deliberately
/// not guessed here: the eval layer can join the resolved model and provider
/// metadata with its authoritative price table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProposalEvidence {
    pub backend: String,
    pub resolved_model: Option<String>,
    pub elapsed_ms: Option<u128>,
    pub retries: Option<u32>,
    pub provider_metadata: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProposalError {
    #[error("invalid placement proposal request: {0}")]
    InvalidRequest(String),
    #[error("placement proposal violated its supplied domain: {0}")]
    InvalidResponse(String),
    #[error("bounded predictor failed: {0}")]
    Bounded(#[from] BoundedError),
    #[error("LLM fallback failed: {0}")]
    Completion(#[from] ooda::Error),
    #[error("bounded predictor failed ({primary}); LLM fallback also failed ({fallback})")]
    Both { primary: String, fallback: String },
}

impl PlacementProposalRequest {
    pub fn validate(&self) -> Result<(), ProposalError> {
        if self.instruction.trim().is_empty() || self.fields.is_empty() {
            return Err(ProposalError::InvalidRequest(
                "an instruction and at least one field are required".into(),
            ));
        }
        let mut keys = BTreeSet::new();
        for field in &self.fields {
            if field.key.trim().is_empty()
                || field.description.trim().is_empty()
                || field.unit.trim().is_empty()
                || field.reference.trim().is_empty()
                || !keys.insert(field.key.as_str())
            {
                return Err(ProposalError::InvalidRequest(
                    "fields need unique keys and non-empty descriptions, units, and references"
                        .into(),
                ));
            }
            if !field.minimum.is_finite()
                || !field.maximum.is_finite()
                || field.minimum >= field.maximum
            {
                return Err(ProposalError::InvalidRequest(format!(
                    "field {:?} needs finite ascending bounds",
                    field.key
                )));
            }
        }
        Ok(())
    }

    /// Strict schema used both in the LLM prompt and for response validation.
    #[must_use]
    pub fn response_schema(&self) -> Value {
        let properties = self
            .fields
            .iter()
            .map(|field| {
                (
                    field.key.clone(),
                    json!({
                        "type": "number",
                        "minimum": field.minimum,
                        "maximum": field.maximum,
                        "description": format!(
                            "{} [{}; relative to {}]",
                            field.description, field.unit, field.reference
                        )
                    }),
                )
            })
            .collect::<Map<_, _>>();
        json!({
            "type": "object",
            "properties": {
                "values": {
                    "type": "object",
                    "properties": properties,
                    "required": self.fields.iter().map(|field| &field.key).collect::<Vec<_>>(),
                    "additionalProperties": false
                }
            },
            "required": ["values"],
            "additionalProperties": false
        })
    }

    fn numeric_request(&self) -> NumericRequest {
        NumericRequest {
            observation: self.observation.clone(),
            instruction: self.instruction.clone(),
            fields: self
                .fields
                .iter()
                .map(|field| NumericField {
                    key: field.key.clone(),
                    description: field.description.clone(),
                    minimum: field.minimum,
                    maximum: field.maximum,
                    unit: field.unit.clone(),
                    reference: Some(field.reference.clone()),
                    aliases: Vec::new(),
                })
                .collect(),
            correlation: self.correlation.clone(),
        }
    }

    fn validate_proposal(
        &self,
        proposal: PlacementProposal,
    ) -> Result<PlacementProposal, ProposalError> {
        let expected = self
            .fields
            .iter()
            .map(|field| field.key.as_str())
            .collect::<BTreeSet<_>>();
        let received = proposal
            .values
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if expected != received {
            return Err(ProposalError::InvalidResponse(format!(
                "expected fields {expected:?}, received {received:?}"
            )));
        }
        for field in &self.fields {
            let value = proposal.values[&field.key];
            if !value.is_finite() || !(field.minimum..=field.maximum).contains(&value) {
                return Err(ProposalError::InvalidResponse(format!(
                    "{}={} is outside [{}, {}] {}",
                    field.key, value, field.minimum, field.maximum, field.unit
                )));
            }
        }
        Ok(proposal)
    }
}

/// Native bounded path. GPC-1 returns a distribution; Lob deliberately applies
/// its MAP estimate and retains no ambiguity about which value reached layout.
pub fn propose_bounded(
    predictor: &dyn BoundedPredictor,
    request: &PlacementProposalRequest,
) -> Result<PlacementProposal, ProposalError> {
    request.validate()?;
    let outcome = predictor.estimate_numeric(&request.numeric_request())?;
    let evidence = ProposalEvidence {
        backend: "bounded_numeric".into(),
        resolved_model: outcome.resolved_model,
        elapsed_ms: outcome.elapsed_ms,
        retries: outcome.retries,
        provider_metadata: outcome.provider_metadata,
    };
    request.validate_proposal(PlacementProposal {
        values: outcome
            .estimates
            .into_iter()
            .map(|estimate| (estimate.key, estimate.map_value))
            .collect(),
        evidence,
    })
}

/// Slower compatibility path for ordinary chat models. The expected output is
/// schematized explicitly and then validated with the same bounds as GPC-1.
pub fn propose_llm(
    completer: &dyn Complete,
    request: &PlacementProposalRequest,
) -> Result<PlacementProposal, ProposalError> {
    request.validate()?;
    let prompt = Prompt::new(
        "You choose numeric PCB placement parameters from a host-supplied closed domain. Return only JSON matching the supplied schema. Never add fields, units, prose, markdown, or coordinates outside the bounds.",
        format!(
            "Instruction:\n{}\n\nObservation:\n{}\n\nRequired response JSON Schema:\n{}",
            request.instruction,
            request.observation,
            request.response_schema()
        ),
    )
    .with_temperature(0.0)
    .with_max_tokens(1024);
    let started = std::time::Instant::now();
    let raw = completer.complete(&prompt)?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LlmValues {
        values: BTreeMap<String, f64>,
    }
    let proposal: LlmValues = serde_json::from_str(raw.trim()).map_err(|error| {
        ProposalError::InvalidResponse(format!("LLM response is not exact proposal JSON: {error}"))
    })?;
    request.validate_proposal(PlacementProposal {
        values: proposal.values,
        evidence: ProposalEvidence {
            backend: "llm_fallback".into(),
            resolved_model: None,
            elapsed_ms: Some(started.elapsed().as_millis()),
            retries: None,
            provider_metadata: None,
        },
    })
}

/// Prefer the native bounded model, falling back to the schematized LLM path
/// only when the native call fails. If both fail, both causes remain visible.
pub fn propose_with_fallback(
    predictor: &dyn BoundedPredictor,
    completer: &dyn Complete,
    request: &PlacementProposalRequest,
) -> Result<PlacementProposal, ProposalError> {
    match propose_bounded(predictor, request) {
        Ok(proposal) => Ok(proposal),
        Err(primary) => propose_llm(completer, request).map_err(|fallback| ProposalError::Both {
            primary: primary.to_string(),
            fallback: fallback.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ooda::{JointOutcome, JointRequest, NumericEstimate, NumericOutcome, ScriptedComplete};

    fn request() -> PlacementProposalRequest {
        PlacementProposalRequest {
            observation: json!({"part": "U2", "keepouts": ["antenna"]}),
            instruction: "Choose a local repair delta.".into(),
            fields: vec![
                PlacementField {
                    key: "U2_dx_mm".into(),
                    description: "horizontal displacement".into(),
                    minimum: -3.0,
                    maximum: 3.0,
                    unit: "mm".into(),
                    reference: "current footprint origin".into(),
                },
                PlacementField {
                    key: "U2_dy_mm".into(),
                    description: "vertical displacement".into(),
                    minimum: -2.0,
                    maximum: 2.0,
                    unit: "mm".into(),
                    reference: "current footprint origin".into(),
                },
            ],
            correlation: Some("attempt-2".into()),
        }
    }

    struct Predictor(Result<NumericOutcome, String>);

    impl BoundedPredictor for Predictor {
        fn estimate_numeric(
            &self,
            _request: &NumericRequest,
        ) -> Result<NumericOutcome, BoundedError> {
            self.0.clone().map_err(BoundedError::InvalidResponse)
        }

        fn decide_joint(&self, _request: &JointRequest) -> Result<JointOutcome, BoundedError> {
            unreachable!()
        }
    }

    fn outcome(x: f64, y: f64) -> NumericOutcome {
        NumericOutcome {
            estimates: vec![
                NumericEstimate {
                    key: "U2_dx_mm".into(),
                    map_value: x,
                    expected_value: x,
                    probabilities: vec![1.0 / 101.0; 101],
                    normalized_grid: vec![],
                },
                NumericEstimate {
                    key: "U2_dy_mm".into(),
                    map_value: y,
                    expected_value: y,
                    probabilities: vec![1.0 / 101.0; 101],
                    normalized_grid: vec![],
                },
            ],
            resolved_model: Some("gpc-1".into()),
            elapsed_ms: Some(4),
            retries: Some(0),
            provider_metadata: None,
        }
    }

    #[test]
    fn native_path_returns_only_bounded_map_values() {
        let proposal = propose_bounded(&Predictor(Ok(outcome(1.5, -0.5))), &request()).unwrap();
        assert_eq!(proposal.values["U2_dx_mm"], 1.5);
        assert_eq!(proposal.values["U2_dy_mm"], -0.5);
    }

    #[test]
    fn llm_sees_and_obeys_the_same_strict_schema() {
        let llm = ScriptedComplete::new([r#"{"values":{"U2_dx_mm":2.0,"U2_dy_mm":-1.0}}"#]);
        let proposal = propose_llm(&llm, &request()).unwrap();
        assert_eq!(proposal.values["U2_dx_mm"], 2.0);
        assert!(
            request().response_schema()["properties"]["values"]["additionalProperties"] == false
        );
    }

    #[test]
    fn fallback_runs_when_native_model_is_unavailable() {
        let predictor = Predictor(Err("model warming".into()));
        let llm = ScriptedComplete::new([r#"{"values":{"U2_dx_mm":0.5,"U2_dy_mm":1.0}}"#]);
        let proposal = propose_with_fallback(&predictor, &llm, &request()).unwrap();
        assert_eq!(proposal.values["U2_dy_mm"], 1.0);
    }

    #[test]
    fn either_backend_is_rejected_outside_the_host_domain() {
        assert!(propose_bounded(&Predictor(Ok(outcome(99.0, 0.0))), &request()).is_err());
        let llm = ScriptedComplete::new([r#"{"values":{"U2_dx_mm":99.0,"U2_dy_mm":0.0}}"#]);
        assert!(propose_llm(&llm, &request()).is_err());
    }
}
