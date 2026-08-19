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
    legalize_pinning(placements, rules, facts, &std::collections::HashSet::new())
}

/// [`legalize`], leaving `pinned` parts exactly where they are.
///
/// A part anchored to a panel cutout is not the placer's to move — its position
/// is where the hole is. Repairing such a violation by sliding the part would
/// hand back a board that cannot mate its own panel, so it is left broken and
/// reported in [`Report::stuck`], where it reads as "the panel needs changing".
pub fn legalize_pinning(
    placements: &mut HashMap<String, Placement>,
    rules: &[Rule],
    facts: &HashMap<String, PartFacts>,
    pinned: &std::collections::HashSet<String>,
) -> Report {
    let mut report = Report::default();
    let mut travelled: HashMap<String, f64> = HashMap::new();

    for _ in 0..MAX_PASSES {
        let broken: Vec<(String, (f64, f64))> = crate::rules::assess(rules, placements)
            .into_iter()
            .filter(|a| a.tier == Tier::Physical && !a.ok())
            .filter_map(|a| a.repair.map(|r| (r.refdes, r.toward_mm)))
            .filter(|(refdes, _)| !pinned.contains(refdes))
            .collect();
        if broken.is_empty() {
            break;
        }
        let mut any_moved = false;
        for (refdes, target) in broken {
            let Some(current) = placements.get(&refdes).copied() else {
                continue;
            };
            let Some(spot) = nearest_free(&refdes, target, current, placements, facts, rules)
            else {
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
    rules: &[Rule],
) -> Option<(f64, f64)> {
    // A part with no measured facts used to short-circuit to `Some(target)` —
    // moved with NO collision check at all. We cannot check its own body without
    // an extent, but we can still refuse to drop it inside a part we DO know,
    // so treat it as a point rather than skipping the test. Callers legitimately
    // pass an empty `facts` and rely on the rule's own repair target (see
    // `panel_hardware_that_overlaps_is_a_physical_violation_and_gets_repaired`),
    // and that keeps working: with nothing to collide against, nothing clashes.
    let (my_extent, my_offset) = facts
        .get(refdes)
        .map(|f| (f.extent, f.origin_offset))
        .unwrap_or(((0.0, 0.0), (0.0, 0.0)));
    let clashes = |x: f64, y: f64| {
        // Ask the geometry question the way the RULE asks it. This used to
        // compare placement origin to placement origin with unrotated extents,
        // while `rules::placed_box` applies the back-flip, `rotate_local`, the
        // quarter-turn extent swap and `origin_offset`. An Alpha pot's origin is
        // pin 1, ~5.35mm off its body centre, so the two disagreed by that much
        // on every Eurorack board — legalization would declare a spot free, move
        // the part there, and hand back a clean report on interpenetrating
        // bodies (measured: RV1 moved 22.1mm to escape an edge and landed 5.065mm
        // inside SW1, `legion-of-bom-<legalize>`).
        //
        // `-TOLERANCE_MM` rather than a bare `<` for the same reason `rules`
        // needs it: `escape_target` returns precisely the touching distance, so
        // an exact tie is the common case, and the two sides reach that number by
        // different summation orders.
        let mine = crate::rules::placed_box(
            my_extent,
            my_offset,
            &Placement {
                x_mm: x,
                y_mm: y,
                ..current
            },
        );
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
            let theirs = crate::rules::placed_box(of.extent, of.origin_offset, p);
            crate::rules::gap_between(mine, theirs) < -crate::rules::TOLERANCE_MM
        })
    };
    let satisfies_rules = |x: f64, y: f64| {
        let mut trial = placements.clone();
        trial.insert(
            refdes.to_string(),
            Placement {
                x_mm: x,
                y_mm: y,
                ..current
            },
        );
        !crate::rules::assess(rules, &trial)
            .into_iter()
            .any(|a| a.tier == Tier::Physical && a.subject == refdes && !a.ok())
    };
    let accepts = |x: f64, y: f64| satisfies_rules(x, y) && !clashes(x, y);
    if accepts(target.0, target.1) {
        return Some(target);
    }
    // Rings outward. Eight directions is enough to find a neighbouring gap
    // without turning this into a placer.
    for ring in 1..=40 {
        let r = SEARCH_STEP_MM * ring as f64;
        for k in 0..8 {
            let a = std::f64::consts::FRAC_PI_4 * k as f64;
            let (x, y) = (target.0 + r * a.cos(), target.1 + r * a.sin());
            if accepts(x, y) {
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
            pin_offsets: HashMap::new(),
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
            origin_offset: (0.0, 0.0),
            bounds,
            min_mm: EDGE_CLEARANCE_MM,
            tier: Tier::Physical,
        }
    }

    /// **Repairing one violation must not create another.**
    ///
    /// `nearest_free`'s clash test measured placement ORIGIN to ORIGIN with
    /// UNROTATED extents, while the checker it has to satisfy — `rules`'
    /// `placed_box` — applies the back-flip, `rotate_local`, the quarter-turn
    /// extent swap AND `origin_offset`. An Alpha pot's origin is pin 1, ~5.35mm
    /// from its body centre, so the two disagree by that offset on every
    /// Eurorack board.
    ///
    /// The fixture is MEASURED, not reasoned about
    /// (`examples/legalize_probe.rs`). Two hand-built guesses failed to
    /// reproduce it: at 90° the offset rotates out of the way, and at the switch
    /// positions I first picked the gap happened to be clear either way. The
    /// probe swept rotation × both x positions and found the real case — at
    /// rotation ZERO, legalize moves RV1 22.1mm to escape the edge and parks it
    /// 5.065mm inside SW1. Origin-to-origin it looks fine (12.1mm apart against
    /// a 11.815mm sum of half-extents); with the offset applied, RV1's body
    /// spans 14.00..28.50 and SW1's 23.435..32.565.
    ///
    /// An earlier version of this test asserted `report.is_clean()` against
    /// `rules::assess` over the SAME rule list `legalize` computes `stuck` from —
    /// a tautology that could not fail. The rules here are deliberately split:
    /// only the edge rule goes in, and the overlap is checked afterwards.
    #[test]
    fn legalize_does_not_create_an_overlap_while_repairing_an_edge() {
        // A pot: rotated a quarter turn, origin well away from its body centre.
        let pot = PartFacts {
            extent: (14.5, 14.32),
            body_extent: (14.5, 14.32),
            origin_offset: (5.35, 0.0),
            ..fact(14.5, 14.32)
        };
        let sw = fact(9.13, 10.14);
        let facts: HashMap<String, PartFacts> =
            [("RV1".into(), pot.clone()), ("SW1".into(), sw.clone())].into();

        let bounds = (0.0, 0.0, 40.0, 100.0);
        let mut placements: HashMap<String, Placement> = [
            (
                "RV1".to_string(),
                Placement {
                    x_mm: 38.0,
                    y_mm: 50.0,
                    rotation_deg: 0.0,
                    back: false,
                },
            ),
            ("SW1".to_string(), at(28.0, 50.0)),
        ]
        .into();

        // ONLY the edge rule. `report.stuck` is computed by running `assess` over
        // exactly these rules, so asserting against the same list is a tautology
        // — it cannot fail however wrong the placement is. The thing under test
        // is `nearest_free`'s own `clashes` guard, whose entire job is to avoid
        // parking a part on top of another WHILE repairing something else.
        let rules = vec![Rule::EdgeClearance {
            refdes: "RV1".into(),
            extent: pot.extent,
            origin_offset: pot.origin_offset,
            bounds,
            min_mm: EDGE_CLEARANCE_MM,
            tier: Tier::Physical,
        }];

        let report = legalize(&mut placements, &rules, &facts);
        assert!(
            !report.moved.is_empty(),
            "precondition: RV1 must actually be repaired, else this proves nothing"
        );

        // Now ask the checker the fab gate uses whether the repair left the two
        // bodies interpenetrating.
        let overlap = vec![Rule::Overlap {
            a: "RV1".into(),
            a_extent: pot.extent,
            a_offset: pot.origin_offset,
            a_tht: Vec::new(),
            a_back: false,
            b: "SW1".into(),
            b_extent: sw.extent,
            b_offset: sw.origin_offset,
            b_tht: Vec::new(),
            b_back: false,
            tier: Tier::Physical,
        }];
        let bad: Vec<String> = crate::rules::assess(&overlap, &placements)
            .into_iter()
            .filter(|a| !a.ok())
            .map(|a| format!("{} overlaps by {:.4}mm", a.subject, -a.margin_mm))
            .collect();
        assert!(
            bad.is_empty(),
            "legalize reported clean={} after moving {:?}, but {bad:?}",
            report.is_clean(),
            report.moved
        );

        // Minimum disturbance is the module's stated contract. A ring search
        // escaping a phantom keep-out can travel further than the board is wide.
        let travelled: f64 = report.moved.iter().map(|(_, d)| d).sum();
        assert!(
            travelled < 40.0,
            "travelled {travelled:.3}mm on a 40mm board: {:?}",
            report.moved
        );
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

    #[test]
    fn occupied_edge_target_does_not_make_ring_search_pick_an_off_board_spot() {
        let bounds = (0.0, 0.0, 20.0, 20.0);
        let rules = vec![edge_rule("J1", (4.0, 4.0), bounds)];
        let facts: HashMap<String, PartFacts> = [
            ("J1".to_string(), fact(4.0, 4.0)),
            ("U1".to_string(), fact(4.0, 4.0)),
        ]
        .into();
        let mut p: HashMap<String, Placement> = [
            ("J1".to_string(), at(22.0, 10.0)),
            // J1's edge repair target is x=17.0. With no rule check in
            // nearest_free, the first non-clashing ring point was east at
            // x=21.0: clear of U1, but still off the board.
            ("U1".to_string(), at(17.0, 10.0)),
        ]
        .into();

        let r = legalize(&mut p, &rules, &facts);
        assert!(r.is_clean(), "{r:?} left {:?}", p["J1"]);
        assert!(
            p["J1"].x_mm <= 17.0 + 1e-6,
            "ring search accepted an off-board candidate: {:?}",
            p["J1"]
        );
    }

    /// A part anchored to a panel cutout is never moved, however illegal it is.
    /// Sliding a jack off its hole to satisfy a rule hands back a board that
    /// cannot mate its own panel — the panel is what needs changing, so the
    /// violation is reported instead.
    #[test]
    fn a_pinned_part_is_reported_not_moved() {
        let bounds = (0.0, 0.0, 40.0, 100.0);
        let rules = vec![edge_rule("J1", (10.0, 6.0), bounds)];
        let facts: HashMap<String, PartFacts> = [("J1".to_string(), fact(10.0, 6.0))].into();
        let mut p: HashMap<String, Placement> = [("J1".to_string(), at(38.0, 50.0))].into();
        let pinned: std::collections::HashSet<String> = ["J1".to_string()].into();

        let r = legalize_pinning(&mut p, &rules, &facts, &pinned);
        assert_eq!(p["J1"], at(38.0, 50.0), "left on its cutout");
        assert!(r.moved.is_empty());
        assert_eq!(r.stuck, vec!["J1".to_string()], "and reported as stuck");

        // Unpinned, the same part is repaired — so the pin is what changed it.
        let mut q: HashMap<String, Placement> = [("J1".to_string(), at(38.0, 50.0))].into();
        legalize(&mut q, &rules, &facts);
        assert!((q["J1"].x_mm - 33.5).abs() < 0.01);
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
