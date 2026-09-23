//! Put the passives on an op-amp's input against the pin they feed.
//!
//! # The node this is about
//!
//! An op-amp input is a **high-impedance** node. On an inverting stage it is a
//! virtual-ground summing junction: the amplifier holds it at zero volts by
//! feedback, and the only thing setting the current into it is the resistors
//! attached to it. Copper hanging off that node does two bad things — it picks
//! up whatever is radiating nearby, because a high-impedance node has no
//! strength to shrug off injected charge, and it adds stray capacitance to a
//! feedback path, which is where op-amps go to oscillate.
//!
//! So the rule is not "keep the feedback network tidy". It is: the summing node
//! should be almost no copper at all.
//!
//! # Why a snap pass and not another pull
//!
//! This has been tried twice as a placement *force* and failed twice:
//!
//! * `FEEDBACK_PULL` — strong attractor edges between a passive and an IC on
//!   any net touching ≤ 3 parts. Went 5 DRC errors to 7 and broke a critical
//!   net. The rule matched `SLEW_NODE`, `IABC` and everything else that happens
//!   to be a small net near an IC, so the placer got a dozen competing
//!   maximum-strength pulls and satisfied none (`legion-of-bom-yck`).
//! * Analytical placement — solving all the attractors at once instead of
//!   greedily. Better in principle, and it still put the summing resistors 43mm
//!   from the amplifier, because the pull was competing with a real constraint:
//!   `RA` is tied to `RATE_CV` at a panel-anchored pot *and* to the summing node
//!   at the op-amp, so its optimum is halfway between two fixed points. Reverted
//!   (`legion-of-bom-10k`).
//!
//! Measured, that midpoint does not move: at 8 HP and at 10 HP the placer put
//! `RA` 43.5mm from `U2` — identical, to the decimal. Extra board area is a
//! longer rope, not a fix. No weight solves this, because the weight is not
//! wrong; the *representation* is. "Against this pin" is a position, and a
//! position is set, not scored — exactly the argument [`crate::decouple`] makes
//! for bypass capacitors, and that pass works.
//!
//! The trade this makes is deliberate and correct: `RA`'s other net,
//! `RATE_CV`, gets *longer*. That is a pot wiper driving an op-amp input
//! through a series resistor — comparatively low impedance, and the resistor
//! sits at the far end where it belongs. Length there is cheap; length on the
//! summing node is not.
//!
//! # How the pins are identified
//!
//! From the **symbol**, not from topology. `Amplifier_Operational:TL074` names
//! pin 13 `-` and pin 12 `+`; the LM13700 names pins 4 and 13 `-`. That is the
//! authoritative statement that a pin is an amplifier input, and it costs
//! nothing to read. Guessing it from net size is what failed before.
//!
//! Runs *before* legalization, so anything this pushes into an illegal position
//! still gets repaired.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::board::{place_point, PartFacts, Placement};
use crate::source::CircuitSource;

/// Clearance (mm) between the passive's pad edge and the pin's once snapped.
/// Small on purpose — the whole point is a short node — but not zero: the pads
/// need soldermask relief and room for a track to leave.
const PAD_GAP_MM: f64 = 1.2;

/// What a snap pass did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    /// `(passive, ic, pin, distance from the passive's pad to that pin)` in mm,
    /// after snapping.
    pub snapped: Vec<(String, String, String, f64)>,
    /// Passives left alone, and why they could not be placed — no pad geometry,
    /// or the part is pinned to a panel cutout.
    pub skipped: Vec<String>,
}

/// Move every passive sitting on an op-amp input so its pad on that node lands
/// beside the pin, in place.
///
/// `symbol_dir` is where the pin names come from; `None` (no KiCad symbol
/// library) makes this a no-op rather than an error — the board is still a
/// board, it just keeps the placer's arrangement. `pinned` parts are never
/// moved: a panel control's position is where the hole is, not this pass's
/// opinion.
pub fn snap(
    placements: &mut HashMap<String, Placement>,
    circuit: &dyn CircuitSource,
    facts: &HashMap<String, PartFacts>,
    pinned: &HashSet<String>,
    symbol_dir: Option<&Path>,
    outline: Option<(f64, f64, f64, f64)>,
) -> Report {
    let mut report = Report::default();
    let Some(symbol_dir) = symbol_dir else {
        return report;
    };

    let mut pin_net: HashMap<(&str, &str), &str> = HashMap::new();
    for net in circuit.nets() {
        for p in &net.pins {
            pin_net.insert((p.refdes.0.as_str(), p.pin.as_str()), net.name.as_str());
        }
    }
    // Parts on each net, so a node's passives can be found from its IC pin.
    let mut net_parts: HashMap<&str, Vec<&str>> = HashMap::new();
    for net in circuit.nets() {
        let e = net_parts.entry(net.name.as_str()).or_default();
        for p in &net.pins {
            if !e.contains(&p.refdes.0.as_str()) {
                e.push(p.refdes.0.as_str());
            }
        }
    }

    // Every amplifier input pin in the circuit: (ic, pin, net). Symbols are read
    // once per library part, not once per pin.
    let mut symbols: HashMap<String, Option<HashMap<String, String>>> = HashMap::new();
    let mut inputs: Vec<(&str, String, &str)> = Vec::new();
    for part in circuit.parts() {
        let Some(libpart) = part.library_part.as_deref() else {
            continue;
        };
        let names = symbols.entry(libpart.to_string()).or_insert_with(|| {
            let (lib, name) = libpart.split_once(':')?;
            crate::symbols::read_symbol_graphics(symbol_dir, lib, name).map(|g| g.pin_names)
        });
        let Some(names) = names else { continue };
        let refdes = part.refdes.0.as_str();
        let mut pins: Vec<(&String, &String)> = names.iter().collect();
        // Deterministic: by pin number, so the same board comes out twice.
        pins.sort_by_key(|(n, _)| n.parse::<u32>().unwrap_or(u32::MAX));
        for (pin, pin_name) in pins {
            if !is_amp_input(pin_name) {
                continue;
            }
            if let Some(&net) = pin_net.get(&(refdes, pin.as_str())) {
                inputs.push((refdes, pin.clone(), net));
            }
        }
    }

    for (ic, pin, net) in inputs {
        // A supply rail is the lowest-impedance node on the board, not a summing
        // junction — an amplifier input tied to ground (a unity-gain stage's `+`,
        // U2 pin 10 here) is still ground. Without this the pass drags every
        // decoupling cap on GND onto one pin and undoes `crate::decouple`.
        if is_supply(net) {
            continue;
        }
        // Passives on the node, excluding the amplifier itself and anything
        // pinned to the panel. Two pads or fewer: a resistor or a capacitor,
        // never another IC — moving one of those is a placement decision, not a
        // repair.
        let mut movable: Vec<&str> = net_parts
            .get(net)
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|r| *r != ic && !pinned.contains(*r) && pad_count(&pin_net, r) <= 2)
                    .collect()
            })
            .unwrap_or_default();
        movable.sort_unstable();
        // A summing junction is a handful of resistors — four, on this board.
        // Anything with a crowd on it is a bus doing another job, and dragging a
        // crowd onto one pin is how the two previous attempts at this made the
        // board worse rather than better.
        if movable.is_empty() || movable.len() > MAX_NODE_PARTS {
            continue;
        }

        let (Some(&ic_at), Some(ic_local)) = (
            placements.get(ic),
            facts.get(ic).and_then(|f| f.pin_offsets.get(&pin)).copied(),
        ) else {
            report.skipped.extend(movable.iter().map(|s| s.to_string()));
            continue;
        };
        let pin_at = place_point(ic_at, ic_local.0, ic_local.1);
        // Step away from the package, not across it: parking a resistor on the
        // amplifier's own footprint is shorter and unbuildable.
        let body = match facts.get(ic) {
            Some(f) => place_point(ic_at, f.origin_offset.0, f.origin_offset.1),
            None => (ic_at.x_mm, ic_at.y_mm),
        };
        let (mut dx, mut dy) = (pin_at.0 - body.0, pin_at.1 - body.1);
        let len = dx.hypot(dy);
        if len < 1e-6 {
            (dx, dy) = (1.0, 0.0);
        } else {
            (dx, dy) = (dx / len, dy / len);
        }

        // Several passives on one node queue outward from the pin, nearest
        // first, each clearing the last. They cannot all have the closest spot,
        // and a queue keeps every one of them on the short side of the board.
        let mut reach = ic_extent(facts, ic) + PAD_GAP_MM;
        for refdes in movable {
            let (Some(&at), Some(local)) = (
                placements.get(refdes),
                pad_on_net(facts, &pin_net, refdes, net),
            ) else {
                report.skipped.push(refdes.to_string());
                continue;
            };
            let ext = facts.get(refdes).map(|f| f.extent).unwrap_or((2.0, 2.0));
            let step = ext.0.max(ext.1);
            let target = (
                pin_at.0 + dx * (reach + step / 2.0),
                pin_at.1 + dy * (reach + step / 2.0),
            );
            // A pin on the outward face of a package near the edge points off
            // the board, and stepping out from it lands the part in space. The
            // placer's position is imperfect but on the board; that beats a
            // shorter node nobody can manufacture. Leave it and say so.
            if !fits_inside(outline, target, ext) {
                report.skipped.push(refdes.to_string());
                continue;
            }
            // Place the origin so the pad on this node lands on the target.
            let pad_from_origin = place_point(
                Placement {
                    x_mm: 0.0,
                    y_mm: 0.0,
                    ..at
                },
                local.0,
                local.1,
            );
            let moved = Placement {
                x_mm: target.0 - pad_from_origin.0,
                y_mm: target.1 - pad_from_origin.1,
                ..at
            };
            // The same guard decouple::snap carries: a shorter node is a
            // trade-off, a part on another part's copper is not. Through-hole
            // pads count on BOTH sides, which is the case that cost the slew
            // limiter its whole -12V net (legion-of-bom-ude).
            if let Some(blocker) = crate::board::first_overlap(refdes, &moved, placements, facts) {
                report.skipped.push(format!(
                    "{refdes}: the spot on {ic}.{pin} is occupied by {blocker}"
                ));
                continue;
            }
            let landed = place_point(moved, local.0, local.1);
            let d = (landed.0 - pin_at.0).hypot(landed.1 - pin_at.1);
            placements.insert(refdes.to_string(), moved);
            report
                .snapped
                .push((refdes.to_string(), ic.to_string(), pin.clone(), d));
            reach += step + PAD_GAP_MM;
        }
    }
    report.snapped.sort_by(|a, b| b.3.total_cmp(&a.3));
    report.skipped.sort();
    report.skipped.dedup();
    report
}

/// Whether a part of `extent` centred on `at` sits inside the board, with the
/// same edge margin the placer uses. No outline (a board sized to its own parts)
/// means nothing to fall off.
fn fits_inside(outline: Option<(f64, f64, f64, f64)>, at: (f64, f64), extent: (f64, f64)) -> bool {
    const EDGE_MM: f64 = 1.5;
    let Some((x0, y0, x1, y1)) = outline else {
        return true;
    };
    at.0 - extent.0 / 2.0 >= x0 + EDGE_MM
        && at.0 + extent.0 / 2.0 <= x1 - EDGE_MM
        && at.1 - extent.1 / 2.0 >= y0 + EDGE_MM
        && at.1 + extent.1 / 2.0 <= y1 - EDGE_MM
}

/// How many movable passives a node may have before this pass decides it is not
/// a summing junction at all. The feedback network here is four resistors across
/// two nodes; a node with more than this on it is a bus.
const MAX_NODE_PARTS: usize = 3;

/// Supply rails and ground — never high-impedance, whatever pin they land on.
/// Same test `crate::decouple` uses to find a bypass cap's rail, for the same
/// reason: these nets are defined by being stiff.
fn is_supply(net: &str) -> bool {
    let u = net.trim().to_ascii_uppercase();
    let gnd =
        matches!(u.as_str(), "GND" | "GNDA" | "AGND" | "DGND" | "VSS" | "0") || u.ends_with("GND");
    gnd || u.starts_with('+')
        || u.starts_with('-')
        || matches!(u.as_str(), "VCC" | "VDD" | "VEE" | "V+" | "V-")
}

/// Whether a symbol pin name marks an amplifier input. KiCad names them exactly
/// `-` and `+`; anything else (`V+`, `DIODE_BIAS`, `OUT`) is not an input, and
/// `V+`/`V-` in particular must not match — they are the supply pins, and
/// snapping the decoupling network onto them is [`crate::decouple`]'s job with
/// different geometry.
fn is_amp_input(pin_name: &str) -> bool {
    matches!(pin_name.trim(), "-" | "+")
}

/// How many pads a part has on any net — the cheap "is this a passive" test.
fn pad_count(pin_net: &HashMap<(&str, &str), &str>, refdes: &str) -> usize {
    pin_net.keys().filter(|(r, _)| *r == refdes).count()
}

/// Half the IC's larger extent — how far out of the package the pin sits.
fn ic_extent(facts: &HashMap<String, PartFacts>, ic: &str) -> f64 {
    facts
        .get(ic)
        .map(|f| f.extent.0.max(f.extent.1) / 2.0)
        .unwrap_or(2.0)
}

/// A part's pad carrying `net`, as a footprint-local offset. Lowest pin number
/// wins when a part has several, so the same board is produced every run.
fn pad_on_net(
    facts: &HashMap<String, PartFacts>,
    pin_net: &HashMap<(&str, &str), &str>,
    refdes: &str,
    net: &str,
) -> Option<(f64, f64)> {
    let offsets = &facts.get(refdes)?.pin_offsets;
    let mut pins: Vec<&str> = pin_net
        .iter()
        .filter(|((r, _), n)| *r == refdes && **n == net)
        .map(|((_, p), _)| *p)
        .collect();
    pins.sort_by_key(|p| p.parse::<u32>().unwrap_or(u32::MAX));
    pins.iter().find_map(|p| offsets.get(*p)).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_amplifier_inputs_count_as_high_impedance() {
        assert!(is_amp_input("-"));
        assert!(is_amp_input("+"));
        assert!(is_amp_input(" - "));
        // Supply pins share the glyph and must not match — that is decouple's
        // territory, and snapping a resistor onto V+ would be nonsense.
        assert!(!is_amp_input("V+"));
        assert!(!is_amp_input("V-"));
        assert!(!is_amp_input("DIODE_BIAS"));
        assert!(!is_amp_input("OUT"));
        assert!(!is_amp_input(""));
    }

    /// Without a symbol library there is no authoritative statement about which
    /// pin is an input, and guessing is what failed twice before. Do nothing.
    #[test]
    fn no_symbol_library_means_no_opinion() {
        use crate::model::Circuit;
        let mut placements = HashMap::new();
        placements.insert(
            "R1".to_string(),
            Placement {
                x_mm: 50.0,
                y_mm: 50.0,
                rotation_deg: 0.0,
                back: false,
            },
        );
        let before = placements.clone();
        let report = snap(
            &mut placements,
            &Circuit::new("t"),
            &HashMap::new(),
            &HashSet::new(),
            None,
            None,
        );
        assert_eq!(placements, before);
        assert_eq!(report, Report::default());
    }
}
