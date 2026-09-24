//! Bounded RLCD assistance for the layout repair loop.
//!
//! The model never emits coordinates, net names, or code. Rust describes the
//! failed attempt, derives the actions that are valid for that evidence, and
//! asks for one key from that closed set. The layout loop still owns iteration
//! count, stopping, and the deterministic implementation of every action.

use ooda::{Answer, Client, Question, Request, Trace};
use serde::{Deserialize, Serialize};

/// Measurements shown to the decider after one place/route/check attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepairEvidence {
    pub attempt: usize,
    pub attempts_remaining: usize,
    pub unrouted_connections: usize,
    pub route_conflicts: Vec<String>,
    pub rule_violations: Vec<RuleEvidence>,
    pub drc_errors: usize,
    pub drc_error_kinds: Vec<String>,
    pub signal_hpwl_mm: f64,
    pub critical_hpwl_mm: f64,
    pub via_count: usize,
}

/// A structured design-rule failure; `repairable` means Rust has a concrete
/// part/destination hint available, not that the model may invent one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleEvidence {
    pub tier: String,
    pub by_mm: f64,
    pub description: String,
    pub repairable: bool,
}

/// The entire action alphabet available to RLCD. Adding an action requires a
/// deterministic implementation in the host before it can become selectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairAction {
    /// Follow host-computed rule destinations; explore locally for other parts.
    FollowRuleHints,
    /// Small golden-angle displacement, preserving most of the placement.
    ExploreLocal,
    /// Larger deterministic displacement to escape congestion.
    ExploreWide,
}

impl RepairAction {
    pub const fn key(self) -> &'static str {
        match self {
            Self::FollowRuleHints => "follow_rule_hints",
            Self::ExploreLocal => "explore_local",
            Self::ExploreWide => "explore_wide",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::FollowRuleHints => {
                "Apply only Rust-computed rule repair vectors; locally explore unmentioned parts"
            }
            Self::ExploreLocal => "Apply a small deterministic golden-angle placement perturbation",
            Self::ExploreWide => "Apply a larger deterministic perturbation to escape congestion",
        }
    }

    fn parse(key: &str) -> Option<Self> {
        match key {
            "follow_rule_hints" => Some(Self::FollowRuleHints),
            "explore_local" => Some(Self::ExploreLocal),
            "explore_wide" => Some(Self::ExploreWide),
            _ => None,
        }
    }
}

/// Derive the closed action set from evidence. This is policy in Rust, so RLCD
/// cannot request a strategy whose prerequisites are absent.
pub fn valid_actions(evidence: &RepairEvidence) -> Vec<RepairAction> {
    let mut actions = Vec::with_capacity(3);
    if evidence.rule_violations.iter().any(|v| v.repairable) {
        actions.push(RepairAction::FollowRuleHints);
    }
    actions.push(RepairAction::ExploreLocal);
    if evidence.unrouted_connections > 0 || evidence.drc_errors > 0 {
        actions.push(RepairAction::ExploreWide);
    }
    actions
}

/// Ask RLCD for one valid repair action and append the exact choice to `trace`.
/// An invalid/wrong-kind answer fails loudly; it is never interpreted as text.
pub fn decide_repair(
    client: &dyn Client,
    trace: &mut Trace,
    evidence: &RepairEvidence,
) -> Result<RepairAction, ooda::Error> {
    let actions = valid_actions(evidence);
    let criteria: ooda::Criteria = actions
        .iter()
        .map(|action| (action.key(), action.description()))
        .collect();
    let request = Request::single(
        serde_json::to_value(evidence).expect("repair evidence is serializable"),
        format!("layout_repair_attempt_{}", evidence.attempt),
        Question::choice(
            "Choose one bounded placement-repair strategy. Do not propose coordinates or new actions.",
            criteria,
        ),
    );
    let outcome = client.decide(&request)?;
    let key = format!("layout_repair_attempt_{}", evidence.attempt);
    let answer = outcome.recorded_answer(&key, trace)?;
    let Answer::Choice { choice, .. } = answer else {
        return Err(ooda::Error::WrongAnswerKind {
            question: key,
            expected: "choice",
        });
    };
    RepairAction::parse(choice).ok_or_else(|| ooda::Error::UnknownChoice {
        question: key,
        chosen: choice.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> RepairEvidence {
        RepairEvidence {
            attempt: 2,
            attempts_remaining: 3,
            unrouted_connections: 2,
            route_conflicts: vec!["net 3 (SDA): could not route".into()],
            rule_violations: vec![RuleEvidence {
                tier: "electrical".into(),
                by_mm: 2.5,
                description: "C1 too far from U1".into(),
                repairable: true,
            }],
            drc_errors: 1,
            drc_error_kinds: vec!["clearance".into()],
            signal_hpwl_mm: 42.0,
            critical_hpwl_mm: 8.0,
            via_count: 4,
        }
    }

    #[test]
    fn action_set_is_derived_and_bounded() {
        assert_eq!(
            valid_actions(&evidence()),
            vec![
                RepairAction::FollowRuleHints,
                RepairAction::ExploreLocal,
                RepairAction::ExploreWide
            ]
        );
        let mut clean = evidence();
        clean.unrouted_connections = 0;
        clean.drc_errors = 0;
        clean.rule_violations.clear();
        assert_eq!(valid_actions(&clean), vec![RepairAction::ExploreLocal]);
    }

    #[test]
    fn decision_sees_structured_evidence_and_is_traced() {
        let client = ooda::ScriptedClient::new([r#"{
            "answers":{"layout_repair_attempt_2":{"type":"choice",
            "choice":"explore_wide","confidence":0.91}}}"#]);
        let mut trace = Trace::new();
        let action = decide_repair(&client, &mut trace, &evidence()).unwrap();
        assert_eq!(action, RepairAction::ExploreWide);
        assert_eq!(trace.records()[0].chosen, "explore_wide");
        let request = &client.requests()[0];
        assert_eq!(request["state"]["unrouted_connections"], 2);
        assert_eq!(request["state"]["rule_violations"][0]["repairable"], true);
        assert!(request["questions"]["layout_repair_attempt_2"]["criteria"]
            .get("invent_coordinates")
            .is_none());
    }
}
