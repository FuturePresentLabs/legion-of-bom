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
//!
//! A caution that applies to every rule here: a rule measures what a *placement*
//! can see, which is rarely the quantity that matters. Decoupling is the clearest
//! case — the objective is loop inductance and we score centre-to-centre
//! millimetres — but the same gap will open up for any electrical rule. Name the
//! proxy in the rule's own docs so nobody mistakes a passing check for a
//! guarantee about the circuit.

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
    /// `a` and `b` must sit at least `min_mm` apart, centre to centre.
    ///
    /// The panel-hardware rule. Two panel-mounted controls collide by their
    /// *panel* envelope — the knob, the nut, the finger room — not by their PCB
    /// courtyard, and the panel envelope is much the larger. A board can be
    /// perfectly legal in copper and still carry a pot and a jack 6.9mm apart,
    /// which is a panel nobody can assemble (`legion-of-bom-za4`).
    Separation {
        a: String,
        /// Where `a`'s panel hardware sits relative to its placement origin.
        ///
        /// Not optional detail: an Alpha pot's origin is pin 1 and its shaft is
        /// ~5.3mm away, so comparing placement origins misjudges the gap by half
        /// a knob. What collides is the hardware, so that is what is measured.
        a_offset: (f64, f64),
        b: String,
        b_offset: (f64, f64),
        min_mm: f64,
        tier: Tier,
        why: &'static str,
    },
    /// `refdes`'s keep-out (`extent`, w×h) must sit at least `min_mm` inside
    /// `bounds`. A part hanging over the edge is not a board.
    /// `a` and `b` must not occupy the same board area.
    ///
    /// Two parts clash **body to body only on the same side** — opposite faces
    /// of the board cannot touch. But a **through-hole pad is copper on BOTH
    /// layers**, so a pin clashes with any body on either side. That asymmetry is
    /// the whole rule: it is exactly what a side-aware check misses, and missing
    /// it put a back-side 0603 on the back annulus of a front jack's pin and made
    /// an entire power net unroutable (legion-of-bom-ude).
    ///
    /// Pairs that can never clash — opposite sides, neither carrying through-hole
    /// pads — are not derived at all, so this stays well short of N-squared on a
    /// real mixed-kit board.
    Overlap {
        a: String,
        a_extent: (f64, f64),
        a_offset: (f64, f64),
        /// Footprint-local through-hole pad rects.
        a_tht: Vec<(f64, f64, f64, f64)>,
        a_back: bool,
        b: String,
        b_extent: (f64, f64),
        b_offset: (f64, f64),
        b_tht: Vec<(f64, f64, f64, f64)>,
        b_back: bool,
        tier: Tier,
    },
    EdgeClearance {
        refdes: String,
        extent: (f64, f64),
        /// Keep-out centre relative to the placement origin.
        ///
        /// Not every footprint is centred on its origin — a pot's origin is its
        /// shaft and a DIP's is pin 1, so the body sits several millimetres off.
        /// Measuring the extent box around the placement origin puts it in the
        /// wrong place: RV1 on slew_limiter is offset +5.3mm, which is enough to
        /// call a part clear while KiCad finds its pad on the board edge.
        origin_offset: (f64, f64),
        bounds: (f64, f64, f64, f64),
        min_mm: f64,
        tier: Tier,
    },
}

impl Rule {
    /// The board-space box this rule measures for a part placed at `p`, if it
    /// measures one.
    ///
    /// Exposed, and used by [`assess`] itself, so there is exactly one place
    /// that knows how a footprint's keep-out lands once rotated. Duplicating
    /// that transform is how the codebase ended up with two rotation senses
    /// disagreeing with each other for months.
    pub fn measured_box(&self, p: &Placement) -> Option<(f64, f64, f64, f64)> {
        match self {
            Rule::Proximity { .. } | Rule::Separation { .. } | Rule::Overlap { .. } => None,
            Rule::EdgeClearance {
                extent,
                origin_offset,
                ..
            } => {
                // Flip in the footprint's own frame first, then rotate — the
                // order the placer uses, and the only order that agrees with
                // where KiCad actually puts a rotated back-side part.
                let local = if p.back {
                    (origin_offset.0, -origin_offset.1)
                } else {
                    *origin_offset
                };
                let (ox, oy) = crate::board::rotate_local(local, p.rotation_deg);
                let quarter_turns = (p.rotation_deg / 90.0).round() as i64;
                let (ew, eh) = if quarter_turns % 2 == 0 {
                    (extent.0, extent.1)
                } else {
                    (extent.1, extent.0)
                };
                let (cx, cy) = (p.x_mm + ox, p.y_mm + oy);
                Some((cx - ew / 2.0, cy - eh / 2.0, cx + ew / 2.0, cy + eh / 2.0))
            }
        }
    }

    pub fn tier(&self) -> Tier {
        match self {
            Rule::Proximity { tier, .. }
            | Rule::Separation { tier, .. }
            | Rule::Overlap { tier, .. }
            | Rule::EdgeClearance { tier, .. } => *tier,
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
///
/// **This is a proxy.** What actually decides whether a bypass capacitor works
/// is the *inductance of the current loop* from the capacitor to the IC's power
/// pin — which depends on via geometry and the return path, not on where two
/// part centres sit. A capacitor 2 mm away through poor vias can perform worse
/// than one 5 mm away through good ones. Distance is cheap, needs only the
/// placement, and correlates well enough to be worth enforcing; it is not the
/// thing we care about. Do not tighten this number expecting better decoupling.
/// See `legion-of-bom-REFS` for the power-integrity references.
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
                why: "short current loop from the bypass cap to the IC's power \
                      pins (distance stands in for loop inductance)",
            }
        })
        .collect();

    // Panel hardware must not collide with panel hardware. Judged on the *panel*
    // envelope — knob, nut, finger room — because that is what a builder's hands
    // meet, and it is far bigger than the PCB courtyard the placer otherwise
    // reserves. Physical tier: two knobs in the same hole is not a trade-off.
    //
    // Only pairs where BOTH parts are panel-mounted. A board part happily lives
    // under a knob; it is on the other side of the panel.
    // A part's hardware centre relative to its placement origin: the courtyard
    // centre, which for a pot is the shaft rather than pin 1.
    let off = |r: &str| -> (f64, f64) {
        facts
            .and_then(|f| f.get(r))
            .map(|f| f.origin_offset)
            .unwrap_or((0.0, 0.0))
    };
    let panel_parts: Vec<(&str, (f64, f64))> = {
        use crate::panel::{BuiltinCutouts, CutoutSource};
        let mut v: Vec<(&str, (f64, f64))> = circuit
            .parts()
            .iter()
            .filter_map(|p| {
                let spec = BuiltinCutouts
                    .cutout(p.mpn.as_deref(), p.footprint.as_deref().unwrap_or(""))?;
                Some((p.refdes.0.as_str(), spec.envelope_mm))
            })
            .collect();
        v.sort_by_key(|(r, _)| *r);
        v
    };
    for (i, (a, ea)) in panel_parts.iter().enumerate() {
        for (b, eb) in &panel_parts[i + 1..] {
            // Centre spacing that clears both envelopes whichever way they sit.
            let min_mm = ((ea.0 + eb.0) / 2.0).max((ea.1 + eb.1) / 2.0);
            rules.push(Rule::Separation {
                a: (*a).to_string(),
                a_offset: off(a),
                b: (*b).to_string(),
                b_offset: off(b),
                min_mm,
                tier: Tier::Physical,
                why: "panel hardware overlaps — knobs and nuts are bigger than \
                      the footprints under them",
            });
        }
    }

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
                origin_offset: fact.origin_offset,
                bounds,
                min_mm: EDGE_CLEARANCE_MM,
                tier: Tier::Physical,
            });
        }
    }

    // Nothing may sit on top of anything else. Needs only `facts` — unlike the
    // edge rule this is not circular on a pad-bbox outline, because it compares
    // parts to each other rather than to a boundary derived from them.
    //
    // Pairs that can never clash are skipped: opposite sides of the board with
    // no through-hole pads between them cannot touch, and on a mixed kit (SMD one
    // face, panel hardware the other) that is most pairs.
    if let Some(f) = facts {
        let mut refs: Vec<&str> = circuit
            .parts()
            .iter()
            .map(|p| p.refdes.0.as_str())
            .collect();
        refs.sort_unstable();
        refs.dedup();
        for (i, a) in refs.iter().enumerate() {
            let Some(fa) = f.get(*a) else { continue };
            for b in &refs[i + 1..] {
                let Some(fb) = f.get(*b) else { continue };
                let a_back = fa.side == crate::model::Side::Back;
                let b_back = fb.side == crate::model::Side::Back;
                let pins = !fa.tht_pads.is_empty() || !fb.tht_pads.is_empty();
                if a_back != b_back && !pins {
                    continue;
                }
                rules.push(Rule::Overlap {
                    a: (*a).to_string(),
                    a_extent: fa.extent,
                    a_offset: fa.origin_offset,
                    a_tht: fa.tht_pads.clone(),
                    a_back,
                    b: (*b).to_string(),
                    b_extent: fb.extent,
                    b_offset: fb.origin_offset,
                    b_tht: fb.tht_pads.clone(),
                    b_back,
                    tier: Tier::Physical,
                });
            }
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

/// How far past a rule counts as actually past it (mm).
///
/// A part placed *exactly* on a limit — the power header laid against the board
/// edge margin, which is the same 1.5mm as the edge-clearance rule — computes its
/// margin by summing the sheet origin in a different order than the rule does, so
/// it comes out at ±1e-14 rather than 0. Half the time that reads as a broken
/// `Tier::Physical` rule and refuses the fab package with "hangs 0.0mm past the
/// board's 1.5mm edge clearance", which is not a board defect, it is arithmetic.
/// KiCad's own board unit is 1nm; nothing below that is a different board.
const TOLERANCE_MM: f64 = 1e-6;

impl Assessment {
    pub fn ok(&self) -> bool {
        self.margin_mm >= -TOLERANCE_MM
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
            Rule::Separation {
                a,
                a_offset,
                b,
                b_offset,
                min_mm,
                tier,
                why,
            } => {
                let (Some(pa), Some(pb)) = (placements.get(a), placements.get(b)) else {
                    continue;
                };
                // Hardware centres, not placement origins.
                let ca = crate::board::place_point(*pa, a_offset.0, a_offset.1);
                let cb = crate::board::place_point(*pb, b_offset.0, b_offset.1);
                let (dx, dy) = (ca.0 - cb.0, ca.1 - cb.1);
                let d = dx.hypot(dy);
                // Push `a` straight away from `b` to exactly the minimum. Two
                // controls on the same centreline give dx = 0, so this becomes a
                // pure vertical move — which is the shape of a Eurorack column.
                let (ux, uy) = if d > 1e-6 {
                    (dx / d, dy / d)
                } else {
                    (0.0, 1.0)
                };
                out.push(Assessment {
                    tier: *tier,
                    subject: a.clone(),
                    detail: format!("{a} is {d:.1}mm from {b} (min {min_mm:.1}mm) — {why}"),
                    margin_mm: d - min_mm,
                    // Target the *origin* that puts a's hardware at the right
                    // distance, since that is what a placement stores.
                    repair: Some(Repair {
                        refdes: a.clone(),
                        toward_mm: (
                            cb.0 + ux * min_mm - (ca.0 - pa.x_mm),
                            cb.1 + uy * min_mm - (ca.1 - pa.y_mm),
                        ),
                    }),
                });
            }
            Rule::Overlap {
                a,
                a_extent,
                a_offset,
                a_tht,
                a_back,
                b,
                b_extent,
                b_offset,
                b_tht,
                b_back,
                tier,
            } => {
                let (Some(pa), Some(pb)) = (placements.get(a), placements.get(b)) else {
                    continue;
                };
                let box_a = placed_box(*a_extent, *a_offset, pa);
                let box_b = placed_box(*b_extent, *b_offset, pb);
                // Bodies only meet if they are on the same side of the board.
                let mut worst = if *a_back == *b_back {
                    gap_between(box_a, box_b)
                } else {
                    f64::INFINITY
                };
                // A pin goes through the board, so it meets a body on either side.
                for r in a_tht {
                    worst = worst.min(gap_between(placed_rect(*r, pa), box_b));
                }
                for r in b_tht {
                    worst = worst.min(gap_between(placed_rect(*r, pb), box_a));
                }
                if !worst.is_finite() {
                    continue;
                }
                let how = if *a_back == *b_back {
                    "overlap"
                } else {
                    "collide through the board — a through-hole pad is copper on both layers"
                };
                out.push(Assessment {
                    tier: *tier,
                    subject: a.clone(),
                    detail: if worst < 0.0 {
                        format!("{a} and {b} {how} by {:.1}mm", -worst)
                    } else {
                        format!("{a} clears {b} by {worst:.1}mm")
                    },
                    margin_mm: worst,
                    // Push `a` straight out along whichever axis is cheaper to
                    // escape on — the minimum move that makes it legal, which is
                    // what legalization is for.
                    repair: Some(Repair {
                        refdes: a.clone(),
                        toward_mm: escape_target(box_a, box_b, pa),
                    }),
                });
            }
            Rule::EdgeClearance {
                refdes,
                bounds,
                min_mm,
                tier,
                ..
            } => {
                let Some(p) = placements.get(refdes) else {
                    continue;
                };
                let (x0, y0, x1, y1) = *bounds;
                let Some((bx0, by0, bx1, by1)) = rule.measured_box(p) else {
                    continue;
                };
                let (ew, eh) = (bx1 - bx0, by1 - by0);
                let (px, py) = ((bx0 + bx1) / 2.0, (by0 + by1) / 2.0);
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
                let slack = (px - ax0).min(ax1 - px).min(py - ay0).min(ay1 - py);
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
                    // The repair target is a *placement origin*, so undo the
                    // offset between the origin and the measured box: clamp
                    // where the box must end up, then convert back to where the
                    // origin has to be for that to happen.
                    repair: Some(Repair {
                        refdes: refdes.clone(),
                        toward_mm: (
                            p.x_mm + (px.clamp(ax0, ax1) - px),
                            p.y_mm + (py.clamp(ay0, ay1) - py),
                        ),
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

/// A footprint's keep-out in board space, given where it is placed.
fn placed_box(extent: (f64, f64), offset: (f64, f64), p: &Placement) -> (f64, f64, f64, f64) {
    let local = if p.back {
        (offset.0, -offset.1)
    } else {
        offset
    };
    let (ox, oy) = crate::board::rotate_local(local, p.rotation_deg);
    let quarter = (p.rotation_deg / 90.0).round() as i64;
    let (w, h) = if quarter % 2 == 0 {
        extent
    } else {
        (extent.1, extent.0)
    };
    let (cx, cy) = (p.x_mm + ox, p.y_mm + oy);
    (cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0)
}

/// A footprint-local rect (a through-hole pad) in board space.
fn placed_rect(r: (f64, f64, f64, f64), p: &Placement) -> (f64, f64, f64, f64) {
    let (cx, cy) = ((r.0 + r.2) / 2.0, (r.1 + r.3) / 2.0);
    placed_box((r.2 - r.0, r.3 - r.1), (cx, cy), p)
}

/// Signed gap between two axis-aligned boxes: positive is clear air, negative is
/// how far they interpenetrate. Boxes are apart if EITHER axis separates them,
/// so the gap is the larger of the two — which is also the cheaper escape.
fn gap_between(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> f64 {
    let gx = (b.0 - a.2).max(a.0 - b.2);
    let gy = (b.1 - a.3).max(a.1 - b.3);
    gx.max(gy)
}

/// Where to move `a`'s placement origin so it just clears `b`, along whichever
/// axis needs the smaller move.
fn escape_target(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64), pa: &Placement) -> (f64, f64) {
    let gx = (b.0 - a.2).max(a.0 - b.2);
    let gy = (b.1 - a.3).max(a.1 - b.3);
    if gx >= gy {
        // Cheaper to separate horizontally.
        let push = if (a.0 + a.2) / 2.0 < (b.0 + b.2) / 2.0 {
            b.0 - a.2
        } else {
            b.2 - a.0
        };
        (pa.x_mm + push, pa.y_mm)
    } else {
        let push = if (a.1 + a.3) / 2.0 < (b.1 + b.3) / 2.0 {
            b.1 - a.3
        } else {
            b.3 - a.1
        };
        (pa.x_mm, pa.y_mm + push)
    }
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

    /// The za4 case, in miniature: a 9mm pot and a jack 6.9mm apart on the board.
    /// Legal in copper, impossible on a panel — the knob and the nut occupy far
    /// more room than the footprints under them.
    #[test]
    fn panel_hardware_that_overlaps_is_a_physical_violation_and_gets_repaired() {
        let jack = "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical";
        let pot = "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical";
        let circuit = Circuit {
            name: "za4".into(),
            parts: vec![
                Part::new("J1", "in").with_footprint(jack),
                Part::new("RV2", "100k").with_footprint(pot),
            ],
            nets: Vec::new(),
        };
        let rules = derive(&circuit);
        let sep = rules
            .iter()
            .find(|r| matches!(r, Rule::Separation { .. }))
            .expect("a separation rule between two panel controls");
        let Rule::Separation { min_mm, tier, .. } = sep else {
            unreachable!()
        };
        // Jack envelope 14.4 tall, pot 14.0 → 14.2mm centre to centre.
        assert!((min_mm - 14.2).abs() < 0.01, "{min_mm}");
        assert_eq!(*tier, Tier::Physical, "a panel that cannot be built");

        // As placed on the shipped board: 6.87mm apart, same x.
        let mut p: HashMap<String, Placement> = [
            ("RV2".to_string(), at(146.0, 95.35)),
            ("J1".to_string(), at(146.0, 102.22)),
        ]
        .into();
        let broken = evaluate(&rules, &p);
        assert!(
            broken.iter().any(|v| v.tier == Tier::Physical),
            "the overlap is reported: {broken:?}"
        );

        // Legalization is what fixes it, since it repairs physical rules.
        let facts = HashMap::new();
        crate::legalize::legalize(&mut p, &rules, &facts);
        let d = (p["J1"].x_mm - p["RV2"].x_mm).hypot(p["J1"].y_mm - p["RV2"].y_mm);
        assert!(d >= 14.2 - 0.01, "pushed apart to {d:.2}mm");
        assert!(evaluate(&rules, &p).is_empty(), "and nothing left broken");
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
            pin_offsets: HashMap::new(),
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
            origin_offset: (0.0, 0.0),
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
            origin_offset: (0.0, 0.0),
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

    /// A pot's origin is its shaft, not its body centre. Measuring the extent
    /// box around the placement origin put it 5mm from where the part actually
    /// is, which called RV1 clear on a 4 HP board while KiCad found its pad on
    /// the edge.
    #[test]
    fn the_keep_out_is_measured_where_it_sits_not_at_the_placement_origin() {
        let bounds = (0.0, 0.0, 20.32, 100.0); // 4 HP
        let rule = Rule::EdgeClearance {
            refdes: "RV1".into(),
            extent: (14.5, 14.3),
            origin_offset: (5.3, 2.5),
            bounds,
            min_mm: EDGE_CLEARANCE_MM,
            tier: Tier::Physical,
        };
        // Origin at x=5, so the keep-out is centred at 10.3 and spans 3.05..17.55
        // — 2.77mm clear of the right edge but only 3.05 of the left, both fine.
        // Move the origin to 1.0 and the keep-out lands at 6.3, spanning
        // -0.95..13.55: over the left edge, which the old code could not see.
        let p: HashMap<String, Placement> = [("RV1".into(), at(1.0, 50.0))].into();
        let v = evaluate(std::slice::from_ref(&rule), &p);
        assert_eq!(v.len(), 1, "keep-out is over the edge: {v:?}");
        // …and the repair target is a placement ORIGIN, so it undoes the offset.
        let r = v[0].repair.as_ref().unwrap();
        let keepout_x = r.toward_mm.0 + 5.3;
        assert!(
            keepout_x >= EDGE_CLEARANCE_MM + 14.5 / 2.0 - 0.01,
            "moving the origin to {:.2} puts the keep-out at {keepout_x:.2}",
            r.toward_mm.0
        );
    }

    /// A part sitting *exactly* on a limit is not over it.
    ///
    /// `board::EDGE_MARGIN_MM` and [`EDGE_CLEARANCE_MM`] are both 1.5mm, so the
    /// power header — which the placer lays against the margin by construction —
    /// lands precisely on the line. The placer reaches its coordinate as
    /// `sheet_origin + (lane_x − keepout_offset)` and the rule reaches the limit
    /// as `sheet_origin + min + half_extent`; the same number by different
    /// summation orders, which in binary floating point differ by ~1e-14. Half
    /// the widths land on the wrong side of it, and the 8 HP slew limiter's fab
    /// package was refused outright: "J3 hangs 0.0mm past the board's 1.5mm edge
    /// clearance". A Tier::Physical rejection produced by arithmetic.
    ///
    /// The placement here comes from the placer itself, not from hand-written
    /// coordinates — a fixture that guesses where the header lands cannot
    /// reproduce the arithmetic that is the whole bug.
    #[test]
    fn a_part_exactly_on_the_edge_clearance_line_is_not_a_violation() {
        use crate::board::{PartFacts, Placer, SeededPlacer};
        use crate::model::{Circuit, Part, RefDes, Side};
        let circuit = Circuit {
            name: "pwr".into(),
            parts: vec![Part {
                refdes: RefDes("J3".into()),
                value: String::new(),
                footprint: Some("Connector_PinHeader_2.54mm:PinHeader_2x05_P2.54mm".into()),
                library_part: None,
                mpn: None,
                sim: None,
                side: None,
            }],
            nets: vec![],
        };
        // The real 2×5 Eurorack header: origin at pin 1, body centred 5.08mm
        // along +X.
        let facts: HashMap<String, PartFacts> = [(
            "J3".to_string(),
            PartFacts {
                extent: (6.24, 13.86),
                body_extent: (6.24, 13.86),
                origin_offset: (1.27, 5.08),
                side: Side::Front,
                height_mm: 8.5,
                standoff_mm: None,
                tht_pads: vec![(-1.25, -1.25, 1.25, 1.25)],
                pin_offsets: HashMap::new(),
            },
        )]
        .into();

        // Every conventional width, at the sheet origin the CLI centres on — the
        // term whose summation order produces the noise. 8 HP is the one that
        // actually refused; the others prove it is not width-specific.
        for hp in [4u16, 6, 8, 10, 12] {
            let (w, h) = (hp as f64 * 5.08, 128.5f64);
            let origin = (((297.0 - w) / 2.0).max(10.0), ((210.0 - h) / 2.0).max(10.0));
            let placements =
                SeededPlacer::new(w, h, origin, HashMap::new()).place(&circuit, &facts);
            let rules = derive_in(
                &circuit,
                &Context {
                    facts: Some(&facts),
                    outline: Some((origin.0, origin.1, origin.0 + w, origin.1 + h)),
                },
            );
            let broken: Vec<_> = evaluate(&rules, &placements)
                .into_iter()
                .filter(|v| v.tier == Tier::Physical)
                .collect();
            assert!(
                broken.is_empty(),
                "{hp} HP: header on the edge line is not past it — {broken:?}"
            );
        }

        // …and a part genuinely a hair over still fails, so this is a tolerance,
        // not a hole: 10µm is four orders of magnitude above the noise.
        let bounds = (0.0, 0.0, 40.64, 128.5);
        let rule = Rule::EdgeClearance {
            refdes: "J3".into(),
            extent: (13.86, 6.24),
            origin_offset: (0.0, 0.0),
            bounds,
            min_mm: EDGE_CLEARANCE_MM,
            tier: Tier::Physical,
        };
        let over: HashMap<String, Placement> = [(
            "J3".into(),
            at(EDGE_CLEARANCE_MM + 13.86 / 2.0 - 0.01, 60.0),
        )]
        .into();
        assert_eq!(evaluate(std::slice::from_ref(&rule), &over).len(), 1);
    }

    /// A part wider than the board can never be moved into compliance, and
    /// "shift it 2.3mm" would be a lie. Say the outline is too small.
    #[test]
    fn a_part_that_cannot_fit_reports_the_board_not_the_placement() {
        let rule = Rule::EdgeClearance {
            refdes: "RV1".into(),
            extent: (14.5, 14.3),
            origin_offset: (0.0, 0.0),
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

    /// **The defect that shipped.** A back-side 0603 whose body sits on the back
    /// annulus of a FRONT-side jack's through-hole pin. A side-aware check calls
    /// this clear — opposite faces — and that is exactly how an entire power net
    /// became unroutable (legion-of-bom-ude).
    #[test]
    fn a_through_hole_pin_collides_with_a_part_on_the_other_side() {
        let rule = Rule::Overlap {
            a: "C3".into(),
            a_extent: (4.45, 2.95),
            a_offset: (0.0, 0.0),
            a_tht: Vec::new(),
            a_back: true, // back-side 0603
            b: "J1".into(),
            b_extent: (10.0, 15.38),
            b_offset: (0.0, 5.775),
            // One through-hole pin at the jack's origin.
            b_tht: vec![(-0.97, -0.92, 0.97, 0.92)],
            b_back: false, // front-side jack
            tier: Tier::Physical,
        };
        let at = |x: f64, y: f64, back: bool| Placement {
            x_mm: x,
            y_mm: y,
            rotation_deg: 0.0,
            back,
        };
        // C3 sitting essentially on the pin, as on the real board.
        let on_top: HashMap<String, Placement> = [
            ("C3".to_string(), at(50.0, 50.0, true)),
            ("J1".to_string(), at(50.0, 50.0, false)),
        ]
        .into();
        let hit = evaluate(std::slice::from_ref(&rule), &on_top);
        assert_eq!(hit.len(), 1, "a pin through a body must be a violation");
        assert_eq!(hit[0].tier, Tier::Physical);
        assert!(
            hit[0].what.contains("through the board"),
            "must say WHY opposite sides still collide: {}",
            hit[0].what
        );

        // Move it well clear on the same sides and the rule goes quiet.
        let clear: HashMap<String, Placement> = [
            ("C3".to_string(), at(70.0, 50.0, true)),
            ("J1".to_string(), at(50.0, 50.0, false)),
        ]
        .into();
        assert!(
            evaluate(&[rule], &clear).is_empty(),
            "clear parts must not violate"
        );
    }

    /// Two surface parts on OPPOSITE faces with no pins between them share board
    /// area legitimately — that is the whole point of a two-sided board, and a
    /// rule that forbids it would make every mixed kit unbuildable.
    #[test]
    fn opposite_side_surface_parts_may_share_board_area() {
        let smd = |back: bool, name: &str| (name.to_string(), back);
        let (a, a_back) = smd(false, "R1");
        let (b, b_back) = smd(true, "R2");
        let rule = Rule::Overlap {
            a,
            a_extent: (2.0, 1.5),
            a_offset: (0.0, 0.0),
            a_tht: Vec::new(),
            a_back,
            b,
            b_extent: (2.0, 1.5),
            b_offset: (0.0, 0.0),
            b_tht: Vec::new(),
            b_back,
            tier: Tier::Physical,
        };
        let stacked: HashMap<String, Placement> = [
            (
                "R1".to_string(),
                Placement {
                    x_mm: 50.0,
                    y_mm: 50.0,
                    rotation_deg: 0.0,
                    back: false,
                },
            ),
            (
                "R2".to_string(),
                Placement {
                    x_mm: 50.0,
                    y_mm: 50.0,
                    rotation_deg: 0.0,
                    back: true,
                },
            ),
        ]
        .into();
        assert!(
            evaluate(&[rule], &stacked).is_empty(),
            "front and back surface parts may occupy the same footprint area"
        );
    }

    /// Same side, bodies overlapping — the ordinary case, and the one that must
    /// carry a repair vector so legalize can act on it.
    #[test]
    fn same_side_bodies_overlap_and_offer_an_escape() {
        let rule = Rule::Overlap {
            a: "RV2".into(),
            a_extent: (14.5, 14.32),
            a_offset: (5.35, 2.5),
            a_tht: Vec::new(),
            a_back: false,
            b: "SW1".into(),
            b_extent: (9.13, 10.14),
            b_offset: (0.0, 0.0),
            b_tht: Vec::new(),
            b_back: false,
            tier: Tier::Physical,
        };
        let p = |x: f64, y: f64| Placement {
            x_mm: x,
            y_mm: y,
            rotation_deg: 0.0,
            back: false,
        };
        let close: HashMap<String, Placement> = [
            ("RV2".to_string(), p(50.0, 50.0)),
            ("SW1".to_string(), p(52.0, 50.0)),
        ]
        .into();
        let hit = evaluate(&[rule], &close);
        assert_eq!(hit.len(), 1, "overlapping same-side bodies must violate");
        let repair = hit[0].repair.as_ref().expect("must offer a repair");
        assert_eq!(repair.refdes, "RV2");
        // The escape must actually separate them, not just move something.
        assert!(
            (repair.toward_mm.0 - 50.0).abs() > 1e-6 || (repair.toward_mm.1 - 50.0).abs() > 1e-6,
            "repair must move the part"
        );
    }
}
