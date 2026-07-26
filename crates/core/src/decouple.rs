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

use crate::board::{place_point, PartFacts, Placement};
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
        let reach = clearance(facts, &cap, &ic) + PAD_GAP_MM;
        let target = (pin.0 + dx * reach, pin.1 + dy * reach);

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
        let moved = Placement {
            x_mm: target.0 - pad_from_origin.0,
            y_mm: target.1 - pad_from_origin.1,
            ..cap_at
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
fn clearance(facts: &HashMap<String, PartFacts>, cap: &str, ic: &str) -> f64 {
    let get = |r: &str| facts.get(r).map(|f| f.extent).unwrap_or((2.0, 2.0));
    let (ca, ia) = (get(cap), get(ic));
    ((ca.0 + ia.0) / 2.0).min((ca.1 + ia.1) / 2.0) * 0.5
}

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
