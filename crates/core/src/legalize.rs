//! Legalization — the missing middle stage.
//!
//! PCB placement in the literature is three stages: **global placement**, then
//! **legalization**, then fine-tuning. Global placement decides roughly where
//! things want to be; legalization moves whatever is illegal the *minimum*
//! distance to make it legal, leaving the global intent otherwise alone.
//!
//! We had the first and third and none of the second. That is the best
//! explanation for why rule-directed repair failed twice (see
//! `legion-of-bom-1xm`): it tried to do legalization's job from inside global
//! placement, by nudging a part's *target* and re-running the whole greedy pass.
//! A nudged target does not fix an illegal position — it asks the global stage
//! to please land somewhere else, and the global stage has its own opinions.
//!
//! This pass takes the placement as given and repairs it directly. It only
//! touches [`Tier::Physical`] violations: a part hanging over the board edge is
//! a board that cannot be built, and moving it is not a trade-off. Electrical
//! and preference rules are deliberately left to scoring, because moving a part
//! to satisfy those *is* a trade-off and belongs where trade-offs are priced.

use std::collections::HashMap;

use crate::board::{PartFacts, Placement};
use crate::rules::{Rule, Tier};

/// How many passes to make. Moving one part can push another out, so a couple of
/// sweeps settle more than one; but this is a repair, not a solver, and a part
/// that cannot be legalized in a few passes needs a bigger board, not more
/// iterations.
const MAX_PASSES: usize = 4;

/// Step size (mm) for the outward search when a part's legal position is
/// already occupied.
const SEARCH_STEP_MM: f64 = 0.5;

/// What a legalization pass did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    /// Parts moved, with how far each travelled (mm).
    pub moved: Vec<(String, f64)>,
    /// Parts still violating a physical rule afterwards — no legal spot exists,
    /// which usually means the board is too small.
    pub stuck: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.stuck.is_empty()
    }
}

/// Move every part breaking a physical rule to the nearest position that
/// satisfies it and clashes with nothing, in place.
///
/// Returns what moved and what could not. A part that cannot be legalized is
/// left where it was rather than dumped somewhere arbitrary: the violation stays
/// visible to [`crate::rules::assess`], which is the honest outcome — the board
/// is wrong and the report should keep saying so.
pub fn legalize(
    placements: &mut HashMap<String, Placement>,
    rules: &[Rule],
    facts: &HashMap<String, PartFacts>,
) -> Report {
    let mut report = Report::default();
    let mut travelled: HashMap<String, f64> = HashMap::new();

    for _ in 0..MAX_PASSES {
        let broken: Vec<(String, (f64, f64))> = crate::rules::assess(rules, placements)
            .into_iter()
            .filter(|a| a.tier == Tier::Physical && !a.ok())
            .filter_map(|a| a.repair.map(|r| (r.refdes, r.toward_mm)))
            .collect();
        if broken.is_empty() {
            break;
        }
        let mut any_moved = false;
        for (refdes, target) in broken {
            let Some(current) = placements.get(&refdes).copied() else {
                continue;
            };
            let Some(spot) = nearest_free(&refdes, target, current, placements, facts) else {
                continue;
            };
            let step = (spot.0 - current.x_mm).hypot(spot.1 - current.y_mm);
            if step <= 1e-6 {
                continue;
            }
            placements.insert(
                refdes.clone(),
                Placement {
                    x_mm: spot.0,
                    y_mm: spot.1,
                    ..current
                },
            );
            *travelled.entry(refdes).or_default() += step;
            any_moved = true;
        }
        if !any_moved {
            break;
        }
    }

    report.moved = travelled.into_iter().collect();
    report.moved.sort_by(|a, b| b.1.total_cmp(&a.1));
    report.stuck = crate::rules::assess(rules, placements)
        .into_iter()
        .filter(|a| a.tier == Tier::Physical && !a.ok())
        .map(|a| a.subject)
        .collect();
    report.stuck.sort();
    report.stuck.dedup();
    report
}

/// The closest position to `target` where `refdes` clashes with nothing else.
///
/// Spirals outward from the target in rings, so a part whose legal spot is taken
/// settles just beside it rather than being flung across the board — the whole
/// point of legalization is minimum disturbance.
fn nearest_free(
    refdes: &str,
    target: (f64, f64),
    current: Placement,
    placements: &HashMap<String, Placement>,
    facts: &HashMap<String, PartFacts>,
) -> Option<(f64, f64)> {
    let Some(me) = facts.get(refdes) else {
        return Some(target);
    };
    let clashes = |x: f64, y: f64| {
        placements.iter().any(|(other, p)| {
            if other == refdes {
                return false;
            }
            // A part only clashes with one on the same face; through-hole pins
            // are handled by the placer's own keep-outs, not here.
            if p.back != current.back {
                return false;
            }
            let Some(of) = facts.get(other) else {
                return false;
            };
            let (dx, dy) = ((x - p.x_mm).abs(), (y - p.y_mm).abs());
            dx < (me.extent.0 + of.extent.0) / 2.0 && dy < (me.extent.1 + of.extent.1) / 2.0
        })
    };
    if !clashes(target.0, target.1) {
        return Some(target);
    }
    // Rings outward. Eight directions is enough to find a neighbouring gap
    // without turning this into a placer.
    for ring in 1..=40 {
        let r = SEARCH_STEP_MM * ring as f64;
        for k in 0..8 {
            let a = std::f64::consts::FRAC_PI_4 * k as f64;
            let (x, y) = (target.0 + r * a.cos(), target.1 + r * a.sin());
            if !clashes(x, y) {
                return Some((x, y));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Side;
    use crate::rules::EDGE_CLEARANCE_MM;

    fn fact(w: f64, h: f64) -> PartFacts {
        PartFacts {
            extent: (w, h),
            body_extent: (w, h),
            origin_offset: (0.0, 0.0),
            side: Side::Front,
            height_mm: 1.0,
            standoff_mm: None,
            tht_pads: Vec::new(),
        }
    }

    fn at(x: f64, y: f64) -> Placement {
        Placement {
            x_mm: x,
            y_mm: y,
            rotation_deg: 0.0,
            back: false,
        }
    }

    fn edge_rule(refdes: &str, extent: (f64, f64), bounds: (f64, f64, f64, f64)) -> Rule {
        Rule::EdgeClearance {
            refdes: refdes.into(),
            extent,
            bounds,
            min_mm: EDGE_CLEARANCE_MM,
            tier: Tier::Physical,
        }
    }

    #[test]
    fn a_part_over_the_edge_is_pulled_back_in() {
        let bounds = (0.0, 0.0, 40.0, 100.0);
        let rules = vec![edge_rule("J1", (10.0, 6.0), bounds)];
        let facts: HashMap<String, PartFacts> = [("J1".to_string(), fact(10.0, 6.0))].into();
        // Centre may live in x 6.5..33.5; at 38 it hangs off.
        let mut p: HashMap<String, Placement> = [("J1".to_string(), at(38.0, 50.0))].into();
        let r = legalize(&mut p, &rules, &facts);
        assert!(r.is_clean(), "{r:?}");
        assert!((p["J1"].x_mm - 33.5).abs() < 0.01, "{:?}", p["J1"]);
        // Pulled in by the minimum: y untouched, and the move recorded.
        assert_eq!(p["J1"].y_mm, 50.0);
        assert_eq!(r.moved.len(), 1);
        assert!((r.moved[0].1 - 4.5).abs() < 0.01);
    }

    #[test]
    fn a_legal_placement_is_left_alone() {
        let bounds = (0.0, 0.0, 40.0, 100.0);
        let rules = vec![edge_rule("J1", (10.0, 6.0), bounds)];
        let facts: HashMap<String, PartFacts> = [("J1".to_string(), fact(10.0, 6.0))].into();
        let mut p: HashMap<String, Placement> = [("J1".to_string(), at(20.0, 50.0))].into();
        let r = legalize(&mut p, &rules, &facts);
        assert!(r.moved.is_empty() && r.is_clean());
        assert_eq!(p["J1"], at(20.0, 50.0));
    }

    /// Minimum disturbance: if the legal spot is occupied, settle beside it —
    /// do not fling the part across the board.
    #[test]
    fn a_part_whose_legal_spot_is_taken_settles_next_to_it() {
        let bounds = (0.0, 0.0, 40.0, 100.0);
        let rules = vec![edge_rule("J1", (10.0, 6.0), bounds)];
        let facts: HashMap<String, PartFacts> = [
            ("J1".to_string(), fact(10.0, 6.0)),
            ("U1".to_string(), fact(10.0, 6.0)),
        ]
        .into();
        let mut p: HashMap<String, Placement> = [
            ("J1".to_string(), at(38.0, 50.0)),
            // Sitting exactly where J1 wants to go.
            ("U1".to_string(), at(33.5, 50.0)),
        ]
        .into();
        legalize(&mut p, &rules, &facts);
        let j1 = p["J1"];
        // Clear of U1…
        assert!(
            (j1.x_mm - 33.5).abs() >= 10.0 || (j1.y_mm - 50.0).abs() >= 6.0,
            "{j1:?} overlaps U1"
        );
        // …but still near where it was asked to go, not across the board.
        assert!((j1.x_mm - 33.5).hypot(j1.y_mm - 50.0) < 12.0, "{j1:?}");
    }

    /// A board too small for the part has no legal position, and legalization
    /// must say so rather than dumping it somewhere and calling it fixed.
    #[test]
    fn a_part_that_cannot_fit_is_left_put_and_reported_stuck() {
        // 3 HP-ish board, 14.5mm part: no position clears 1.5mm on both sides.
        let bounds = (0.0, 0.0, 15.24, 100.0);
        let rules = vec![edge_rule("RV1", (14.5, 14.3), bounds)];
        let facts: HashMap<String, PartFacts> = [("RV1".to_string(), fact(14.5, 14.3))].into();
        let mut p: HashMap<String, Placement> = [("RV1".to_string(), at(7.6, 50.0))].into();
        let r = legalize(&mut p, &rules, &facts);
        assert_eq!(r.stuck, vec!["RV1".to_string()]);
        assert!(!r.is_clean());
        assert_eq!(p["RV1"], at(7.6, 50.0), "left where it was, not dumped");
    }
}
