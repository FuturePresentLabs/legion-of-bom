//! Design rules as **data**, and their cost in the layout score.
//!
//! The layout loop tries several placements and keeps the best by
//! [`score`](crate::layout::score). Until this module existed, that score was
//! wirelength, vias and unrouted nets — everything a *router* cares about and
//! nothing about whether the circuit works. Rules that mattered were expressed
//! as weight bonuses inside the greedy placer, which meant they could be, and
//! were, thrown away after the fact.
//!
//! The measured case: on the shipped `slew_limiter`, C2 sat **96 mm** from U1.
//! The placer had it right — a traced attempt put C3 at (5.8, 125.5), on its IC
//! — and the loop then chose a different attempt that shaved wirelength and
//! ruined the decoupling, because nothing in the score objected. Deferring caps
//! in the greedy loop was tried and produced byte-identical output: correct
//! ordering, same discarded winner. A placement heuristic cannot fix a scoring
//! problem.
//!
//! So a rule is a record, not a code path. It is *derived* from the circuit,
//! *evaluated* against a placement, and *priced* by [`Tier`] — with the tiers
//! separated far enough that no amount of wirelength can buy back a violated
//! electrical rule.
//!
//! Adding a rule means adding a variant and a deriver, not editing the placer.

use std::collections::HashMap;

use crate::board::Placement;
use crate::source::CircuitSource;

/// How much a rule matters.
///
/// The tiers are priced decades apart so the optimiser cannot trade across them
/// at any realistic board scale: a whole board's wirelength is on the order of
/// hundreds of millimetres, so one millimetre of [`Tier::Electrical`] violation
/// already outweighs every preference term combined. It is a soft lexicographic
/// order — cheap to fold into one `f64` score, and honest about being an
/// approximation of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Preference: shorter, tidier, fewer vias. Trade freely.
    Preference,
    /// Electrical intent: decoupling proximity, critical-net length, pour
    /// integrity. Break only when nothing else will fit, and say so.
    Electrical,
    /// Physical possibility: overlap, off-board, the panel not mating. A board
    /// violating one of these cannot be built.
    Physical,
}

impl Tier {
    /// Cost per millimetre of violation.
    pub fn weight(self) -> f64 {
        match self {
            Tier::Preference => 1.0,
            Tier::Electrical => 1_000.0,
            Tier::Physical => 1_000_000.0,
        }
    }
}

/// What the derivers need to know about the board being laid out.
///
/// Rules are records that carry everything their evaluation needs, so
/// [`evaluate`] takes only a placement. That means the board facts a rule
/// depends on — part sizes, the outline — are resolved once, here, at derive
/// time. A deriver given nothing still produces rules; it produces weaker ones.
#[derive(Default, Clone, Copy)]
pub struct Context<'a> {
    /// Measured part keep-outs, for rules whose limit depends on part size.
    pub facts: Option<&'a HashMap<String, crate::board::PartFacts>>,
    /// The board outline, when it is fixed up front (a panel-sized board).
    ///
    /// `None` for a board whose outline is the pad bounding box, where an
    /// edge-clearance rule would be circular: the outline is derived from the
    /// very placement the rule would constrain, so nothing can ever overhang.
    pub outline: Option<(f64, f64, f64, f64)>,
}

/// House inset from the board edge, in millimetres.
///
/// Matches the placer's own edge margin and sits comfortably above KiCad's
/// 0.5 mm `copper_edge_clearance`, so a board that satisfies this rule does not
/// then fail DRC on the thing the rule was about.
pub const EDGE_CLEARANCE_MM: f64 = 1.5;

/// One declarative design rule.
///
/// Deliberately a small closed set: every variant here is enforced, and a rule
/// nobody evaluates is worse than no rule, because it reads as a guarantee.
#[derive(Debug, Clone, PartialEq)]
pub enum Rule {
    /// `a` must sit within `max_mm` of `b`, centre to centre.
    Proximity {
        a: String,
        b: String,
        max_mm: f64,
        tier: Tier,
        /// Why, for the report — a violation should explain itself.
        why: &'static str,
    },
    /// `refdes`'s keep-out (`extent`, w×h) must sit at least `min_mm` inside
    /// `bounds`. A part hanging over the edge is not a board.
    EdgeClearance {
        refdes: String,
        extent: (f64, f64),
        bounds: (f64, f64, f64, f64),
        min_mm: f64,
        tier: Tier,
    },
}

impl Rule {
    pub fn tier(&self) -> Tier {
        match self {
            Rule::Proximity { tier, .. } | Rule::EdgeClearance { tier, .. } => *tier,
        }
    }
}

/// A rule a placement broke, and by how much.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub tier: Tier,
    /// How far past the rule, in millimetres.
    pub by_mm: f64,
    /// Human-readable, for the layout report.
    pub what: String,
    /// How to fix it: which part to move, and where it should head.
    ///
    /// A violation that cannot say this is one the loop can only price, not
    /// repair — it will be reported and scored against, and the layout will
    /// have to be relaxed or fixed by hand.
    pub repair: Option<Repair>,
}

/// Where a violating part wants to be, so the loop can aim rather than shake.
#[derive(Debug, Clone, PartialEq)]
pub struct Repair {
    /// The part to move — the one that is out of place, not its reference.
    pub refdes: String,
    /// Board-mm the part should head toward.
    pub toward_mm: (f64, f64),
}

/// How much clear board may sit between a decoupling capacitor's keep-out and
/// its IC's before it stops being a decoupling capacitor. The intent is
/// "touching, or as near as clearance allows".
pub const DECOUPLE_GAP_MM: f64 = 2.0;

/// Fallback centre-to-centre limit, used only when part sizes are unavailable.
///
/// Kept small on purpose but **known to be wrong for large packages** — it was
/// the original rule, and it is why the loop spent six iterations chasing a
/// target it could not reach. Measured on slew_limiter: a 0603 cannot get closer
/// than 6.7 mm to a SOIC-16's centre, because at 5 mm it would be inside the
/// chip. Prefer [`derive_with_sizes`].
pub const DECOUPLE_MAX_MM: f64 = 5.0;

/// Derive the rule set a circuit implies.
///
/// This is where "what matters about this circuit" is decided, once, from the
/// netlist — rather than in whichever code path happens to touch a part.
pub fn derive(circuit: &dyn CircuitSource) -> Vec<Rule> {
    derive_in(circuit, &Context::default())
}

/// [`derive`], with each proximity limit sized to the two parts involved.
///
/// A centre-to-centre limit is the wrong shape for "put the cap against the
/// chip", because how close two centres *can* get depends entirely on how big
/// the parts are. A flat 5 mm is satisfiable for an SOIC-8 and physically
/// impossible for an SOIC-16 — and an unsatisfiable rule is worse than none: it
/// reports a violation on every attempt, so the loop burns its whole budget
/// chasing it and the report cries wolf.
///
/// With `facts`, the limit becomes "the two keep-outs touching, plus
/// [`DECOUPLE_GAP_MM`]" — always reachable, and violated only when the cap
/// really has been pushed away from its chip.
pub fn derive_in(circuit: &dyn CircuitSource, ctx: &Context<'_>) -> Vec<Rule> {
    let facts = ctx.facts;
    // Closest two keep-outs can approach, centre to centre, along whichever axis
    // needs least room.
    let floor = |a: &str, b: &str| -> Option<f64> {
        let f = facts?;
        let (ea, eb) = (f.get(a)?.extent, f.get(b)?.extent);
        Some(((ea.0 + eb.0) / 2.0).min((ea.1 + eb.1) / 2.0))
    };
    let mut rules: Vec<Rule> = crate::board::decoupling_pairs(circuit)
        .into_iter()
        .map(|(cap, ic)| {
            let max_mm = floor(&cap, &ic)
                .map(|f| f + DECOUPLE_GAP_MM)
                .unwrap_or(DECOUPLE_MAX_MM);
            Rule::Proximity {
                a: cap,
                b: ic,
                max_mm,
                tier: Tier::Electrical,
                why: "a decoupling capacitor must sit at its IC's power pins",
            }
        })
        .collect();

    // Nothing may hang over the edge. Only derivable when the outline is fixed
    // up front; on a board whose outline is the pad bounding box the rule would
    // be circular and could never fire.
    if let (Some(bounds), Some(f)) = (ctx.outline, facts) {
        let mut refs: Vec<&str> = circuit
            .parts()
            .iter()
            .map(|p| p.refdes.0.as_str())
            .collect();
        refs.sort_unstable();
        for r in refs {
            let Some(fact) = f.get(r) else { continue };
            rules.push(Rule::EdgeClearance {
                refdes: r.to_string(),
                extent: fact.extent,
                bounds,
                min_mm: EDGE_CLEARANCE_MM,
                tier: Tier::Physical,
            });
        }
    }
    rules
}

/// One rule's standing against a placement — whether it passes, and by how much.
///
/// This is the uniform interface the whole system reads through: the score wants
/// the failures, the report wants the failures with their tier, and a human
/// debugging a layout wants *every* rule and how close it came. Deriving all
/// three from one evaluation means the panel cannot disagree with the score.
#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub tier: Tier,
    /// The part this rule is about — what to look at on the board.
    pub subject: String,
    /// The requirement and the measurement, in one sentence.
    pub detail: String,
    /// Room left before the rule breaks, in millimetres. Negative means broken
    /// by that much.
    pub margin_mm: f64,
    /// How to fix it, when a fix exists.
    pub repair: Option<Repair>,
}

impl Assessment {
    pub fn ok(&self) -> bool {
        self.margin_mm >= 0.0
    }
}

/// Assess every rule against a placement — passes included.
///
/// A rule naming a part that was never placed is skipped rather than reported:
/// it is a fact about a circuit we did not lay out, and calling it a failure
/// would be noise.
pub fn assess(rules: &[Rule], placements: &HashMap<String, Placement>) -> Vec<Assessment> {
    let mut out = Vec::new();
    for rule in rules {
        match rule {
            Rule::Proximity {
                a,
                b,
                max_mm,
                tier,
                why,
            } => {
                let (Some(pa), Some(pb)) = (placements.get(a), placements.get(b)) else {
                    continue;
                };
                let d = (pa.x_mm - pb.x_mm).hypot(pa.y_mm - pb.y_mm);
                out.push(Assessment {
                    tier: *tier,
                    subject: a.clone(),
                    detail: format!("{a} is {d:.1}mm from {b} (max {max_mm:.1}mm) — {why}"),
                    margin_mm: max_mm - d,
                    // Move the cap to its IC, not the other way round: the IC is
                    // the anchor the rest of the circuit hangs off.
                    repair: Some(Repair {
                        refdes: a.clone(),
                        toward_mm: (pb.x_mm, pb.y_mm),
                    }),
                });
            }
            Rule::EdgeClearance {
                refdes,
                extent,
                bounds,
                min_mm,
                tier,
            } => {
                let Some(p) = placements.get(refdes) else {
                    continue;
                };
                let (x0, y0, x1, y1) = *bounds;
                // A part the placer stood on end occupies its extent swapped.
                // Measuring the unrotated box against the board reports a
                // 90°-rotated header as hanging off when it fits perfectly.
                let quarter_turns = (p.rotation_deg / 90.0).round() as i64;
                let (ew, eh) = if quarter_turns % 2 == 0 {
                    (extent.0, extent.1)
                } else {
                    (extent.1, extent.0)
                };
                let (hw, hh) = (ew / 2.0, eh / 2.0);
                // The band the part's *centre* may occupy.
                let (ax0, ay0) = (x0 + min_mm + hw, y0 + min_mm + hh);
                let (ax1, ay1) = (x1 - min_mm - hw, y1 - min_mm - hh);
                // A part wider than the board can never satisfy this, and saying
                // "move it 4mm" would be a lie — the board is too small. Report
                // the shortfall as what it is.
                if ax0 > ax1 || ay0 > ay1 {
                    let short = (ax0 - ax1).max(ay0 - ay1).max(0.0);
                    out.push(Assessment {
                        tier: *tier,
                        subject: refdes.clone(),
                        detail: format!(
                            "{refdes} ({ew:.1}×{eh:.1}mm) does not fit inside the board with \
                             {min_mm:.1}mm edge clearance — the outline is {short:.1}mm too small"
                        ),
                        margin_mm: -short,
                        repair: None,
                    });
                    continue;
                }
                // Positive: room left on the tightest side. Negative: how far past.
                let slack = (p.x_mm - ax0)
                    .min(ax1 - p.x_mm)
                    .min(p.y_mm - ay0)
                    .min(ay1 - p.y_mm);
                let detail = if slack < 0.0 {
                    format!(
                        "{refdes} hangs {:.1}mm past the board's {min_mm:.1}mm edge clearance",
                        -slack
                    )
                } else {
                    format!("{refdes} clears the board edge by {slack:.1}mm (min {min_mm:.1}mm)")
                };
                out.push(Assessment {
                    tier: *tier,
                    subject: refdes.clone(),
                    detail,
                    margin_mm: slack,
                    repair: Some(Repair {
                        refdes: refdes.clone(),
                        toward_mm: (p.x_mm.clamp(ax0, ax1), p.y_mm.clamp(ay0, ay1)),
                    }),
                });
            }
        }
    }
    // Worst first: a report should lead with what actually stops the board.
    out.sort_by(|x, y| {
        y.tier
            .cmp(&x.tier)
            .then(x.margin_mm.total_cmp(&y.margin_mm))
    });
    out
}

/// Evaluate `rules` against a placement, returning only what was broken.
pub fn evaluate(rules: &[Rule], placements: &HashMap<String, Placement>) -> Vec<Violation> {
    assess(rules, placements)
        .into_iter()
        .filter(|a| !a.ok())
        .map(|a| Violation {
            tier: a.tier,
            by_mm: -a.margin_mm,
            what: a.detail,
            repair: a.repair,
        })
        .collect()
}

/// The total cost of a violation set, for [`score`](crate::layout::score).
pub fn penalty(violations: &[Violation]) -> f64 {
    violations.iter().map(|v| v.tier.weight() * v.by_mm).sum()
}

/// Total millimetres broken in each tier, worst tier first:
/// `[physical, electrical, preference]`.
///
/// The ordering key for **relaxation**. Comparing attempts on this
/// lexicographically means a lower tier is only ever traded once every higher
/// tier ties — you break the cheapest thing that lets the board fit, and never
/// buy a millimetre of tidiness with a millimetre of electrical intent. The
/// single-`f64` [`penalty`] approximates the same order with weights; this is
/// the exact version, for choosing between attempts.
pub fn by_tier(violations: &[Violation]) -> [f64; 3] {
    let mut out = [0.0; 3];
    for v in violations {
        let i = match v.tier {
            Tier::Physical => 0,
            Tier::Electrical => 1,
            Tier::Preference => 2,
        };
        out[i] += v.by_mm;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef, RefDes};

    fn at(x: f64, y: f64) -> Placement {
        Placement {
            x_mm: x,
            y_mm: y,
            rotation_deg: 0.0,
            back: false,
        }
    }

    fn pin(r: &str, p: &str) -> PinRef {
        PinRef {
            refdes: RefDes(r.into()),
            pin: p.into(),
        }
    }

    /// A bypass cap across +12V/GND with an IC on the same rail.
    fn circuit() -> Circuit {
        Circuit {
            name: "t".into(),
            parts: vec![
                Part::new("U1", "TL072").with_footprint("Package_SO:SOIC-8_3.9x4.9mm"),
                Part::new("C2", "100n").with_footprint("Capacitor_SMD:C_0603_1608Metric"),
            ],
            nets: vec![
                Net {
                    name: "+12V".into(),
                    pins: vec![pin("U1", "8"), pin("C2", "1")],
                    net_class: None,
                },
                Net {
                    name: "GND".into(),
                    pins: vec![pin("U1", "4"), pin("C2", "2")],
                    net_class: None,
                },
            ],
        }
    }

    #[test]
    fn a_bypass_cap_derives_a_proximity_rule_to_its_ic() {
        let rules = derive(&circuit());
        assert_eq!(rules.len(), 1);
        let Rule::Proximity { a, b, tier, .. } = &rules[0] else {
            panic!("expected a proximity rule, got {:?}", rules[0])
        };
        assert_eq!((a.as_str(), b.as_str()), ("C2", "U1"));
        assert_eq!(*tier, Tier::Electrical);
    }

    #[test]
    fn a_cap_on_its_ic_is_no_violation_and_one_across_the_board_is() {
        let rules = derive(&circuit());
        let close: HashMap<String, Placement> = [
            ("U1".into(), at(100.0, 100.0)),
            ("C2".into(), at(102.0, 100.0)),
        ]
        .into();
        assert!(evaluate(&rules, &close).is_empty());

        // The shipped failure: 96mm away.
        let far: HashMap<String, Placement> = [
            ("U1".into(), at(100.0, 100.0)),
            ("C2".into(), at(100.0, 196.0)),
        ]
        .into();
        let v = evaluate(&rules, &far);
        assert_eq!(v.len(), 1);
        assert!((v[0].by_mm - 91.0).abs() < 0.01, "{:?}", v[0]);
        assert!(v[0].what.contains("C2") && v[0].what.contains("96.0mm"));
    }

    /// The whole point of the tiers: no amount of wirelength buys back a
    /// violated electrical rule. A whole board's signal HPWL is a few hundred
    /// millimetres at weight 1.0, so it cannot reach even 1mm of Electrical.
    #[test]
    fn wirelength_can_never_outbid_an_electrical_violation() {
        let one_mm_electrical = penalty(&[Violation {
            tier: Tier::Electrical,
            by_mm: 1.0,
            what: String::new(),
            repair: None,
        }]);
        let whole_board_of_wirelength = 1.0 * 500.0; // weights.wirelength * HPWL
        assert!(one_mm_electrical > whole_board_of_wirelength);
        // …and physical outranks electrical by the same margin.
        let one_mm_physical = penalty(&[Violation {
            tier: Tier::Physical,
            by_mm: 1.0,
            what: String::new(),
            repair: None,
        }]);
        assert!(one_mm_physical > 100.0 * one_mm_electrical);
    }

    /// A flat centre-to-centre limit is the wrong shape: how close two centres
    /// can get depends on how big the parts are. Measured on slew_limiter, a
    /// 0603 cannot get within 6.7mm of a SOIC-16's centre, so the old 5mm rule
    /// reported a violation on every attempt and the loop burned its budget
    /// chasing a target that did not exist.
    #[test]
    fn the_limit_is_sized_to_the_parts_so_it_can_actually_be_met() {
        use crate::board::PartFacts;
        use crate::model::Side;
        let fact = |w: f64, h: f64| PartFacts {
            extent: (w, h),
            body_extent: (w, h),
            origin_offset: (0.0, 0.0),
            side: Side::Front,
            height_mm: 1.0,
            standoff_mm: None,
            tht_pads: Vec::new(),
        };
        // A big chip and a small cap. Closest approach is beside the chip's long
        // edge, not off its end: (7.4 + 3.0)/2 = 5.2mm, not (10.4 + 1.5)/2.
        let facts: HashMap<String, PartFacts> = [
            ("U1".into(), fact(7.4, 10.4)),
            ("C2".into(), fact(3.0, 1.5)),
        ]
        .into();
        let sized = derive_in(
            &circuit(),
            &Context {
                facts: Some(&facts),
                outline: None,
            },
        );
        let Rule::Proximity { max_mm, .. } = &sized[0] else {
            panic!("expected a proximity rule, got {:?}", sized[0])
        };
        assert!(
            (*max_mm - (5.2 + DECOUPLE_GAP_MM)).abs() < 0.01,
            "limit is the touching distance plus the gap, got {max_mm}"
        );
        assert!(
            *max_mm > DECOUPLE_MAX_MM,
            "and it is larger than the flat rule that could not be met"
        );

        // A cap sitting right against the chip is now compliant…
        let touching: HashMap<String, Placement> = [
            ("U1".into(), at(100.0, 100.0)),
            ("C2".into(), at(106.0, 100.0)),
        ]
        .into();
        assert!(evaluate(&sized, &touching).is_empty());
        // …and the flat rule would have called it a violation.
        assert!(!evaluate(&derive(&circuit()), &touching).is_empty());
    }

    /// Nothing may hang over the edge — the Physical-tier rule behind
    /// legion-of-bom-t5t, where a 3 HP board reported buildable and came back
    /// with copper-edge-clearance errors and parts off the outline.
    #[test]
    fn a_part_over_the_edge_is_a_physical_violation_with_a_way_back() {
        let bounds = (0.0, 0.0, 20.0, 100.0);
        let rule = Rule::EdgeClearance {
            refdes: "J1".into(),
            extent: (10.0, 6.0),
            bounds,
            min_mm: 1.5,
            tier: Tier::Physical,
        };
        // Centre may live in x 6.5..13.5. At 16.0 it is 2.5mm past.
        let over: HashMap<String, Placement> = [("J1".into(), at(16.0, 50.0))].into();
        let v = evaluate(std::slice::from_ref(&rule), &over);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].tier, Tier::Physical);
        assert!((v[0].by_mm - 2.5).abs() < 0.01, "{:?}", v[0]);
        // …and the repair says exactly where it has to go.
        let r = v[0].repair.as_ref().unwrap();
        assert!((r.toward_mm.0 - 13.5).abs() < 0.01, "{:?}", r);
        // Comfortably inside is no violation.
        let ok: HashMap<String, Placement> = [("J1".into(), at(10.0, 50.0))].into();
        assert!(evaluate(std::slice::from_ref(&rule), &ok).is_empty());
    }

    /// A part the placer stood on end occupies its extent swapped. Measuring
    /// the unrotated box reported a 90° power header as hanging 3.8mm off a
    /// board it fits perfectly.
    #[test]
    fn a_rotated_part_is_measured_on_the_side_it_actually_occupies() {
        let rule = Rule::EdgeClearance {
            refdes: "J3".into(),
            extent: (7.0, 14.7),
            bounds: (0.0, 0.0, 25.4, 128.5),
            min_mm: 1.5,
            tier: Tier::Physical,
        };
        // Laid horizontal: 14.7 across, 7 tall, so a centre 5mm from the top
        // edge fits. Unrotated it would look 3.85mm over.
        let laid = Placement {
            x_mm: 12.0,
            y_mm: 5.0,
            rotation_deg: 90.0,
            back: false,
        };
        let p: HashMap<String, Placement> = [("J3".into(), laid)].into();
        assert!(evaluate(std::slice::from_ref(&rule), &p).is_empty());
        // Upright at the same spot genuinely does hang off.
        let upright = Placement {
            rotation_deg: 0.0,
            ..laid
        };
        let p: HashMap<String, Placement> = [("J3".into(), upright)].into();
        assert!(!evaluate(std::slice::from_ref(&rule), &p).is_empty());
    }

    /// A part wider than the board can never be moved into compliance, and
    /// "shift it 2.3mm" would be a lie. Say the outline is too small.
    #[test]
    fn a_part_that_cannot_fit_reports_the_board_not_the_placement() {
        let rule = Rule::EdgeClearance {
            refdes: "RV1".into(),
            extent: (14.5, 14.3),
            bounds: (0.0, 0.0, 15.24, 128.5), // 3 HP
            min_mm: 1.5,
            tier: Tier::Physical,
        };
        let p: HashMap<String, Placement> = [("RV1".into(), at(7.6, 60.0))].into();
        let v = evaluate(std::slice::from_ref(&rule), &p);
        assert_eq!(v.len(), 1);
        assert!(v[0].what.contains("too small"), "{}", v[0].what);
        assert!(
            v[0].repair.is_none(),
            "no placement fixes a board this narrow"
        );
    }

    #[test]
    fn a_rule_about_an_unplaced_part_is_skipped_not_counted() {
        let rules = derive(&circuit());
        let only_ic: HashMap<String, Placement> = [("U1".into(), at(0.0, 0.0))].into();
        assert!(evaluate(&rules, &only_ic).is_empty());
    }

    #[test]
    fn violations_are_reported_worst_first() {
        let rules = vec![
            Rule::Proximity {
                a: "C1".into(),
                b: "U1".into(),
                max_mm: 1.0,
                tier: Tier::Electrical,
                why: "x",
            },
            Rule::Proximity {
                a: "C2".into(),
                b: "U1".into(),
                max_mm: 1.0,
                tier: Tier::Physical,
                why: "y",
            },
        ];
        let p: HashMap<String, Placement> = [
            ("U1".into(), at(0.0, 0.0)),
            ("C1".into(), at(50.0, 0.0)),
            ("C2".into(), at(10.0, 0.0)),
        ]
        .into();
        let v = evaluate(&rules, &p);
        // Physical first even though its overshoot is smaller.
        assert_eq!(v[0].tier, Tier::Physical);
        assert_eq!(v[1].tier, Tier::Electrical);
    }
}
