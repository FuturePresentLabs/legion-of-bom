//! Put each bypass capacitor against the power pin it bypasses.
//!
//! # Why this is its own pass
//!
//! "Keep the decoupling cap near the IC" is the folk version of the rule, and it
//! is not quite the rule. What matters is the **loop**: current leaves the cap,
//! reaches the chip's power pin, returns through ground. The loop's inductance is
//! what sets how well the cap can supply a fast transient, and inductance goes
//! with loop *area*, so the quantity to minimise is the distance from the cap's
//! pads to that specific pin — not to the package's centre of mass.
//!
//! On a 16-pin part the difference is most of the package. Measuring part centres
//! let a cap sit 8.7mm from an LM13700's middle, satisfy the rule, and still be
//! 10mm of copper away from pin 11 — or on the opposite side of the chip, with
//! the loop wrapping right around it.
//!
//! # Why a pass and not a stronger pull
//!
//! The seeded placer already pulls caps toward their IC hard
//! ([`crate::board::decoupling_pairs`]), and a pull is a suggestion competing
//! with every other net's suggestion. This is not a trade-off worth having: there
//! is one right place for a bypass cap and nothing else is bidding for that exact
//! spot. So the pull seeds it roughly and this pass puts it where it belongs, in
//! the same spirit as legalization — take the placement as given, fix the thing
//! that is definitely wrong, disturb nothing else.
//!
//! Runs *before* legalization, so anything this pushes into an illegal position
//! still gets repaired.

use std::collections::HashMap;

use crate::board::{first_overlap, place_point, PartFacts, Placement};
use crate::source::CircuitSource;

/// Clearance (mm) between the cap's pad edge and the pin's, once snapped. Small,
/// because the whole point is a short loop, but not zero: the pads need room for
/// their own soldermask relief and a track to leave.
const PAD_GAP_MM: f64 = 1.2;

/// What a snap pass did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    /// `(cap, ic, distance from the cap's power pad to the IC's power pin)` after
    /// snapping, in mm.
    pub snapped: Vec<(String, String, f64)>,
    /// Caps that could not be snapped — no shared rail, or no pin geometry for
    /// one of the two parts. Left where they were.
    pub skipped: Vec<String>,
}

/// Move every decoupling capacitor so its power pad sits beside the power pin it
/// bypasses, in place.
///
/// The cap keeps its rotation and side; only its position changes. A cap whose
/// IC has no locatable power pin is left alone and reported in
/// [`Report::skipped`] — a cap moved on a guess is worse than one left where the
/// placer put it.
pub fn snap(
    placements: &mut HashMap<String, Placement>,
    circuit: &dyn CircuitSource,
    facts: &HashMap<String, PartFacts>,
) -> Report {
    let mut report = Report::default();
    // pin → net, per part, so we can find which pad carries the rail.
    let mut pin_net: HashMap<(&str, &str), &str> = HashMap::new();
    for net in circuit.nets() {
        for p in &net.pins {
            pin_net.insert((p.refdes.0.as_str(), p.pin.as_str()), net.name.as_str());
        }
    }

    for (cap, ic) in crate::board::decoupling_pairs(circuit) {
        let Some(rail) = shared_rail(&pin_net, &cap, &ic) else {
            report.skipped.push(cap);
            continue;
        };
        // The cap's pad on the rail, and the IC's pin on the same rail, both as
        // footprint-local offsets.
        let (Some(cap_local), Some(ic_local)) = (
            pad_on_net(facts, &pin_net, &cap, rail),
            pad_on_net(facts, &pin_net, &ic, rail),
        ) else {
            report.skipped.push(cap);
            continue;
        };
        let (Some(&cap_at), Some(&ic_at)) = (placements.get(&cap), placements.get(&ic)) else {
            report.skipped.push(cap);
            continue;
        };
        let pin = place_point(ic_at, ic_local.0, ic_local.1);

        // Step out from the pin, away from the IC body, far enough that the two
        // keep-outs do not overlap. Going *outward* matters: parking the cap on
        // the IC's own footprint would be shorter and unbuildable.
        let body = ic_body_centre(facts, ic_at, &ic);
        let (mut dx, mut dy) = (pin.0 - body.0, pin.1 - body.1);
        let len = dx.hypot(dy);
        if len < 1e-6 {
            // A pin at the body centre has no outward direction; step +X.
            (dx, dy) = (1.0, 0.0);
        } else {
            (dx, dy) = (dx / len, dy / len);
        }
        let base = clearance(facts, &cap, &ic) + PAD_GAP_MM;

        // Place the cap's *origin* such that its rail pad lands on the target.
        let pad_from_origin = place_point(
            Placement {
                x_mm: 0.0,
                y_mm: 0.0,
                ..cap_at
            },
            cap_local.0,
            cap_local.1,
        );

        // "There is no competing claim on that exact spot" is only true if you
        // check. When a panel control is anchored across the IC's power pin, the
        // spot is already taken, and setting the cap there anyway buries its pad
        // under the control's copper: on the slew limiter that put an 0603 1.1mm
        // from a 3.5mm jack and made every pad of the -12V net unreachable by any
        // router. A worse-decoupled cap is a trade-off; an unroutable net is not.
        //
        // But ONE guess was the whole search. When it was taken the pass gave up
        // and the cap stayed wherever global placement dropped it — on the dual,
        // 4 of 6 caps skipped and one sat 20.1mm from its IC against an 8.7mm
        // rule (`legion-of-bom-ku4`). Giving up is the worst of the options: a
        // slightly longer loop beats no decoupling at all. So walk outward, and
        // fan a little either side, taking the first spot that is really clear.
        // Nearest-and-straightest wins because that is the order tried.
        let mut found: Option<(Placement, String)> = None;
        'search: for step in 0..SNAP_STEPS {
            let reach = base + step as f64 * SNAP_STEP_MM;
            for deg in SNAP_FAN_DEG {
                let (s, c) = deg.to_radians().sin_cos();
                let (rx, ry) = (dx * c - dy * s, dx * s + dy * c);
                let target = (pin.0 + rx * reach, pin.1 + ry * reach);
                let cand = Placement {
                    x_mm: target.0 - pad_from_origin.0,
                    y_mm: target.1 - pad_from_origin.1,
                    ..cap_at
                };
                match first_overlap(&cap, &cand, placements, facts) {
                    None => {
                        found = Some((cand, String::new()));
                        break 'search;
                    }
                    Some(blocker) => {
                        if found.is_none() {
                            // Remember the first thing in the way, so a total
                            // failure still reports something useful.
                            found = Some((cand, blocker));
                        }
                    }
                }
            }
        }
        let moved = match found {
            Some((p, blocker)) if blocker.is_empty() => p,
            other => {
                let blocker = other.map(|(_, b)| b).unwrap_or_default();
                report.skipped.push(format!(
                    "{cap}: no clear spot against {ic}'s rail pin within \
                     {:.1}mm — nearest blocker {blocker}",
                    base + (SNAP_STEPS - 1) as f64 * SNAP_STEP_MM
                ));
                continue;
            }
        };
        let landed = place_point(moved, cap_local.0, cap_local.1);
        let d = (landed.0 - pin.0).hypot(landed.1 - pin.1);
        placements.insert(cap.clone(), moved);
        report.snapped.push((cap, ic, d));
    }
    report.snapped.sort_by(|a, b| b.2.total_cmp(&a.2));
    report.skipped.sort();
    report.skipped.dedup();
    report
}

/// The power net both parts sit on, if there is exactly one obvious candidate.
fn shared_rail<'a>(
    pin_net: &HashMap<(&str, &str), &'a str>,
    cap: &str,
    ic: &str,
) -> Option<&'a str> {
    let mut cap_rails: Vec<&str> = pin_net
        .iter()
        .filter(|((r, _), n)| *r == cap && is_power(n))
        .map(|(_, n)| *n)
        .collect();
    cap_rails.sort_unstable();
    cap_rails.dedup();
    cap_rails
        .into_iter()
        .find(|rail| pin_net.iter().any(|((r, _), n)| *r == ic && n == rail))
}

/// A part's pad carrying `net`, as a footprint-local offset.
fn pad_on_net(
    facts: &HashMap<String, PartFacts>,
    pin_net: &HashMap<(&str, &str), &str>,
    refdes: &str,
    net: &str,
) -> Option<(f64, f64)> {
    let offsets = &facts.get(refdes)?.pin_offsets;
    // Deterministic: lowest pin number wins when a part has several pins on the
    // rail, so the same board is produced every run.
    let mut pins: Vec<&str> = pin_net
        .iter()
        .filter(|((r, _), n)| *r == refdes && **n == net)
        .map(|((_, p), _)| *p)
        .collect();
    pins.sort_unstable();
    pins.into_iter().find_map(|p| offsets.get(p).copied())
}

/// The IC's keep-out centre in board space — what "away from the body" is
/// measured from.
fn ic_body_centre(facts: &HashMap<String, PartFacts>, at: Placement, ic: &str) -> (f64, f64) {
    match facts.get(ic) {
        Some(f) => place_point(at, f.origin_offset.0, f.origin_offset.1),
        None => (at.x_mm, at.y_mm),
    }
}

/// How far the cap's origin must clear the IC's edge for their keep-outs not to
/// overlap, along the tighter axis.
/// The smallest reach FROM THE PIN that could possibly clear: the cap's own half
/// extent plus a pad gap.
///
/// The distance that matters is the current loop — the cap's pad to the IC's
/// power pin — so the reach is measured from the pin, and the only thing that has
/// to fit in it is the cap itself. The IC's own size does not belong here: the
/// pin is already ON the IC, so adding the IC's half-extent to a distance
/// starting at the pin counts it twice.
///
/// That double-count is what this function used to do. It returned the
/// centre-to-centre half-sum `min((ca+ia)/2)` — an IC-centre quantity — and
/// [`snap`] applied it from the pin, putting caps 9-12mm out against an 8.6mm
/// rule. A trailing `* 0.5` had been bolted on, which cancelled roughly half the
/// error and made the pass look approximately right; removing it (correctly, it
/// asked for less than could ever clear) made the caps go FURTHER out, which is
/// how the real mistake surfaced (`legion-of-bom-ku4`).
///
/// Deliberately optimistic, because [`snap`] verifies and walks outward from
/// here. A short loop that is checked beats a long one that is assumed.
fn clearance(facts: &HashMap<String, PartFacts>, cap: &str, _ic: &str) -> f64 {
    let ce = facts.get(cap).map(|f| f.extent).unwrap_or((2.0, 2.0));
    ce.0.min(ce.1) / 2.0
}

/// How far past the first guess to keep looking, and in what increments.
///
/// A decoupling cap 6mm from its pin is worth having; one left 20mm away because
/// the first spot was taken is not.
/// Range is `SNAP_STEPS * SNAP_STEP_MM` past the first guess. Fine steps because
/// the search now starts at the physical minimum and walks out, so the step size
/// is the precision of the answer, not just its granularity.
const SNAP_STEPS: usize = 30;
const SNAP_STEP_MM: f64 = 0.4;
/// Angles either side of straight-out to try at each radius, in degrees. The
/// outward normal is tried first at every radius, so a clear straight-out spot
/// always wins over an angled nearer one.
const SNAP_FAN_DEG: [f64; 5] = [0.0, 25.0, -25.0, 50.0, -50.0];

fn is_power(net: &str) -> bool {
    let u = net.trim().to_ascii_uppercase();
    let gnd =
        matches!(u.as_str(), "GND" | "GNDA" | "AGND" | "DGND" | "VSS" | "0") || u.ends_with("GND");
    !gnd && (u.starts_with('+')
        || u.starts_with('-')
        || matches!(u.as_str(), "VCC" | "VDD" | "VEE" | "V+" | "V-"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef};

    fn facts_for() -> HashMap<String, PartFacts> {
        let mk = |w: f64, h: f64, pins: &[(&str, (f64, f64))]| PartFacts {
            extent: (w, h),
            body_extent: (w, h),
            origin_offset: (0.0, 0.0),
            side: crate::model::Side::Front,
            height_mm: 2.0,
            standoff_mm: None,
            tht_pads: Vec::new(),
            pin_offsets: pins.iter().map(|(n, o)| (n.to_string(), *o)).collect(),
        };
        [
            // A 16-pin IC 10mm long: its +12V pin is at one END, 5mm off centre.
            (
                "U1".to_string(),
                mk(6.0, 10.0, &[("11", (0.0, -5.0)), ("6", (0.0, 5.0))]),
            ),
            (
                "C2".to_string(),
                mk(1.6, 0.8, &[("1", (-0.8, 0.0)), ("2", (0.8, 0.0))]),
            ),
        ]
        .into()
    }

    /// **Reproduces legion-of-bom's P0 placement bug.**
    ///
    /// `snap` is documented as "set, not scored" because "there is no competing
    /// claim on that exact spot". That is false whenever an anchored panel
    /// control already sits across the IC's power pin — and unlike
    /// [`crate::summing::snap`], this pass is not given the anchored set at all,
    /// so it cannot know.
    ///
    /// On the real slew limiter this drops an 0603 (C3) 1.10mm from a 3.5mm jack
    /// (J1) — concentric, for practical purposes. C3.1 is pad 0 of the -12V net,
    /// so burying it makes every other -12V pad unreachable by ANY router: six
    /// connections dead from one placement move.
    #[test]
    fn a_bypass_cap_is_not_snapped_on_top_of_an_anchored_control() {
        let mut facts = facts_for();
        // A Thonkiconn-sized panel jack: ~10mm across the body.
        facts.insert(
            "J1".to_string(),
            PartFacts {
                extent: (10.0, 10.0),
                body_extent: (10.0, 10.0),
                origin_offset: (0.0, 0.0),
                side: crate::model::Side::Front,
                height_mm: 12.0,
                standoff_mm: None,
                tht_pads: Vec::new(),
                pin_offsets: HashMap::new(),
            },
        );
        let mut circuit = circuit();
        let mut j1 = Part::new("J1", "Thonkiconn");
        j1.footprint = Some("Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical".into());
        circuit.parts.push(j1);

        let at = |x: f64, y: f64| Placement {
            x_mm: x,
            y_mm: y,
            rotation_deg: 0.0,
            back: false,
        };
        let mut placements: HashMap<String, Placement> = [
            ("U1".to_string(), at(50.0, 50.0)),
            // The jack is anchored to its panel cutout, sitting right across
            // U1's +12V pin (pin 11, offset (0, -5) => 50, 45).
            ("J1".to_string(), at(50.0, 45.0)),
            // The cap starts somewhere legal and empty.
            ("C2".to_string(), at(50.0, 62.0)),
        ]
        .into();

        snap(&mut placements, &circuit, &facts);

        let (c, j) = (placements["C2"], placements["J1"]);
        let (dx, dy) = ((c.x_mm - j.x_mm).abs(), (c.y_mm - j.y_mm).abs());
        // Half-extents: the jack is 10mm across, the 0603 1.6 x 0.8.
        let (need_x, need_y) = ((10.0 + 1.6) / 2.0, (10.0 + 0.8) / 2.0);
        assert!(
            dx >= need_x || dy >= need_y,
            "C2 was snapped inside the anchored jack: centres {dx:.2},{dy:.2} mm apart, \
             need {need_x:.2} or {need_y:.2}. A part cannot be placed where another \
             part already is, whatever the decoupling gain."
        );
    }

    /// **The bug that actually killed the slew limiter's -12V net.**
    ///
    /// The cap is on the BACK, the jack on the FRONT — so a side-aware collision
    /// test says they cannot clash. They can: the jack's pads are THROUGH-HOLE
    /// and exist on both copper layers. On the real board this put C3's pad 2
    /// 0.67 x 0.29mm into the back annulus of J1's pin, walling in pad 0 of -12V
    /// and making six connections unreachable by any router.
    ///
    /// The fixture is built empirically — snap once with no jack to learn where
    /// the cap lands, then put the jack's through-hole pad exactly there. A
    /// hand-guessed position made this test pass while exercising nothing.
    #[test]
    fn a_back_side_cap_is_not_snapped_onto_a_front_parts_through_hole_pad() {
        let at = |x: f64, y: f64, back: bool| Placement {
            x_mm: x,
            y_mm: y,
            rotation_deg: 0.0,
            back,
        };
        let mut facts = facts_for();
        if let Some(f) = facts.get_mut("C2") {
            f.side = crate::model::Side::Back;
        }

        // Where does the cap want to go when nothing is in the way?
        let mut solo: HashMap<String, Placement> = [
            ("U1".to_string(), at(50.0, 50.0, true)),
            ("C2".to_string(), at(50.0, 62.0, true)),
        ]
        .into();
        snap(&mut solo, &circuit(), &facts);
        let landed = solo["C2"];
        assert_ne!(
            (landed.x_mm, landed.y_mm),
            (50.0, 62.0),
            "fixture is inert — snap did not move the cap at all"
        );

        // Now put a FRONT-side jack whose through-hole pin is exactly there.
        facts.insert(
            "J1".to_string(),
            PartFacts {
                extent: (10.0, 15.38),
                body_extent: (10.0, 14.4),
                origin_offset: (0.0, 5.775),
                side: crate::model::Side::Front,
                height_mm: 12.0,
                standoff_mm: None,
                tht_pads: vec![(-0.97, -0.92, 0.97, 0.92)],
                pin_offsets: HashMap::new(),
            },
        );
        let mut circuit = circuit();
        let mut j1 = Part::new("J1", "Thonkiconn");
        j1.footprint = Some("Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical".into());
        circuit.parts.push(j1);

        let mut placements: HashMap<String, Placement> = [
            ("U1".to_string(), at(50.0, 50.0, true)),
            ("J1".to_string(), at(landed.x_mm, landed.y_mm, false)),
            ("C2".to_string(), at(50.0, 62.0, true)),
        ]
        .into();
        snap(&mut placements, &circuit, &facts);

        let c = placements["C2"];
        let body = facts["C2"].keepout_at_rot(c.x_mm, c.y_mm, c.back, c.rotation_deg);
        let j = placements["J1"];
        for pad in facts["J1"].tht_pads_at(j.x_mm, j.y_mm, j.back, j.rotation_deg) {
            let clash = body.0 < pad.2 && pad.0 < body.2 && body.1 < pad.3 && pad.1 < body.3;
            assert!(
                !clash,
                "back-side cap {body:?} landed on the front jack's through-hole pad \
                 {pad:?} — a THT pad is copper on BOTH layers"
            );
        }
    }

    fn circuit() -> Circuit {
        let mut c = Circuit::new("t");
        // Footprints matter: `decoupling_pairs` identifies a cap and an IC by
        // footprint family, not by refdes letter.
        let mut u1 = Part::new("U1", "LM13700");
        u1.footprint = Some("Package_DIP:DIP-16_W7.62mm".into());
        let mut c2 = Part::new("C2", "100nF");
        c2.footprint = Some("Capacitor_SMD:C_0603_1608Metric".into());
        c.parts = vec![u1, c2];
        c.nets = vec![
            Net::new(
                "+12V",
                vec![PinRef::new("U1", "11"), PinRef::new("C2", "1")],
            ),
            Net::new("GND", vec![PinRef::new("C2", "2")]),
        ];
        c
    }

    /// The cap ends up beside the *pin*, not the package centre — and outside the
    /// IC's body, not parked on top of it.
    #[test]
    fn a_bypass_cap_lands_next_to_the_power_pin_it_bypasses() {
        let at = |x: f64, y: f64| Placement {
            x_mm: x,
            y_mm: y,
            rotation_deg: 0.0,
            back: false,
        };
        let mut p: HashMap<String, Placement> = [
            ("U1".to_string(), at(50.0, 50.0)),
            ("C2".to_string(), at(70.0, 70.0)),
        ]
        .into();
        let r = snap(&mut p, &circuit(), &facts_for());
        assert!(r.skipped.is_empty(), "{r:?}");
        assert_eq!(r.snapped.len(), 1);

        // U1's pin 11 sits at (50, 45). The cap's pad 1 must be close to it…
        let facts = facts_for();
        let pad = place_point(
            p["C2"],
            facts["C2"].pin_offsets["1"].0,
            facts["C2"].pin_offsets["1"].1,
        );
        let d = (pad.0 - 50.0).hypot(pad.1 - 45.0);
        assert!(d < 6.0, "cap pad {pad:?} is {d:.1}mm from the pin");
        // …and on the far side of the pin from the body, not over the package.
        assert!(pad.1 < 45.0, "cap sits outside the IC body: {pad:?}");
        // Measured distance is reported honestly.
        assert!((r.snapped[0].2 - d).abs() < 1e-6);
    }

    /// A cap with no rail in common with its IC is left where it was: moving it
    /// on a guess is worse than not moving it.
    #[test]
    fn a_cap_sharing_no_rail_is_left_alone() {
        let mut c = circuit();
        c.nets = vec![
            Net::new("+12V", vec![PinRef::new("U1", "11")]),
            Net::new("+5V", vec![PinRef::new("C2", "1")]),
            Net::new("GND", vec![PinRef::new("C2", "2")]),
        ];
        let at = |x: f64, y: f64| Placement {
            x_mm: x,
            y_mm: y,
            rotation_deg: 0.0,
            back: false,
        };
        let mut p: HashMap<String, Placement> = [
            ("U1".to_string(), at(50.0, 50.0)),
            ("C2".to_string(), at(70.0, 70.0)),
        ]
        .into();
        let before = p["C2"];
        snap(&mut p, &c, &facts_for());
        assert_eq!(p["C2"], before);
    }
}
