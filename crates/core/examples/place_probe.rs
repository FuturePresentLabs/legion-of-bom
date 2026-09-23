//! Diagnostic: what does `SeededPlacer` actually do at a given HP?
//!
//! Prints, per width: the derived panel anchors, every part's placement, which
//! parts landed in the off-board overflow lane (y > board height) and the
//! occupancy per side. This is the "instrument the placer" harness for
//! `legion-of-bom-unc`.
//!
//! ```text
//! cargo run --release -p legion-of-bom-core --example place_probe -- \
//!     out/slew_limiter/slew_limiter.net 4 6 8
//! ```

use std::collections::HashMap;

use legion_of_bom_core::{
    build_facts, derive_panel, parse_netlist_file, BuiltinCutouts, CircuitSource, EurorackPanel,
    PanelSpec, PartFacts, Placer, SeededPlacer,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let net = args.next().ok_or("usage: place_probe <x.net> [hp...]")?;
    let hps: Vec<u16> = args.filter_map(|s| s.parse().ok()).collect();
    let hps = if hps.is_empty() { vec![4, 6, 8] } else { hps };

    let circuit = parse_netlist_file(std::path::Path::new(&net))?;
    let dir = legion_of_bom_core::skidl::kicad_footprint_dir().ok_or("no KiCad footprints")?;
    let facts: HashMap<String, PartFacts> = build_facts(&circuit, &dir)?;

    for hp in hps {
        let dims = EurorackPanel::new(hp);
        let (w, h) = (dims.width_mm(), dims.height_mm());
        let panel = derive_panel(&circuit, hp, &BuiltinCutouts);
        let anchors: HashMap<String, (f64, f64)> = panel
            .cutouts
            .iter()
            .filter_map(|c| c.refdes.clone().map(|r| (r, (c.x_mm, h - c.y_mm))))
            .collect();
        // Hand the *derived* panel (the one the real build uses) to downstream
        // tools, so `lob fab --panel` and route_bench measure this width the way
        // the product build would.
        if let Some(dir) = std::env::var_os("LOB_PANEL_OUT") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("panel_{hp}.toml"));
            std::fs::write(&path, panel.to_toml()?)?;
            println!("   wrote {}", path.display());
        }
        let placer = SeededPlacer::new(w, h, (0.0, 0.0), anchors.clone());
        let placements = placer.place(&circuit, &facts);

        println!(
            "== {hp} HP  ({w:.2} x {h:.2} mm), {} anchored",
            anchors.len()
        );
        let mut refs: Vec<&String> = placements.keys().collect();
        refs.sort();
        let mut over = Vec::new();
        let (mut front_area, mut back_area) = (0.0, 0.0);
        for r in &refs {
            let p = &placements[*r];
            let Some(f) = facts.get(*r) else { continue };
            let k = f.keepout_at_rot(p.x_mm, p.y_mm, p.back, p.rotation_deg);
            let a = (k.2 - k.0) * (k.3 - k.1);
            if p.back {
                back_area += a
            } else {
                front_area += a
            }
            let flag = if p.y_mm > h + 0.01 { " OVERFLOW" } else { "" };
            if !flag.is_empty() {
                over.push((*r).clone());
            }
            println!(
                "   {:<4} x={:7.2} y={:7.2} rot={:3.0} {:<5} keepout=[{:.2},{:.2} .. {:.2},{:.2}]{}",
                r,
                p.x_mm,
                p.y_mm,
                p.rotation_deg,
                if p.back { "back" } else { "front" },
                k.0,
                k.1,
                k.2,
                k.3,
                flag
            );
            for q in f.tht_pads_at(p.x_mm, p.y_mm, p.back, p.rotation_deg) {
                println!(
                    "        pad [{:.2},{:.2} .. {:.2},{:.2}]",
                    q.0, q.1, q.2, q.3
                );
            }
        }
        let side = w * h;
        println!(
            "   occupancy: front {:.1}%  back {:.1}%   overflowed: {}",
            100.0 * front_area / side,
            100.0 * back_area / side,
            if over.is_empty() {
                "none".to_string()
            } else {
                over.join(", ")
            }
        );

        // For each part the packer gave up on: does a legal spot exist at ALL,
        // at either quarter turn, with everything else where it ended up? That
        // separates "the board is full" from "the packer looked in one pose".
        for r in &over {
            let f = &facts[r];
            for rot in [0.0f64, 90.0] {
                let hit = scan(r, f, rot, &placements, &facts, w, h);
                match hit {
                    Some((x, y)) => println!(
                        "   {r} @rot{rot:.0}: FITS at x={x:.2} y={y:.2} (nearest clear spot exists)"
                    ),
                    None => println!("   {r} @rot{rot:.0}: no legal spot on the board"),
                }
            }
        }

        // The honest floor. A panel control is not negotiable — it is where the
        // cutout is — and neither is the power header. So strip every free part
        // away and ask, of an otherwise EMPTY board, whether each one has a legal
        // spot at either quarter turn. Anything that fails here is refused by the
        // board, not by the packer; anything that passes here and still ended up
        // in the overflow lane is the packer's doing.
        let fixed: HashMap<String, legion_of_bom_core::Placement> = placements
            .iter()
            .filter(|(k, _)| {
                anchors.contains_key(*k)
                    || circuit.parts().iter().any(|p| {
                        &p.refdes.0 == *k
                            && p.footprint
                                .as_deref()
                                .is_some_and(|f| f.contains("PinHeader_2x"))
                    })
            })
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let mut homeless = Vec::new();
        for r in &refs {
            if fixed.contains_key(*r) {
                continue;
            }
            let Some(f) = facts.get(*r) else { continue };
            let poses: Vec<f64> = [0.0f64, 90.0]
                .into_iter()
                .filter(|&rot| scan(r, f, rot, &fixed, &facts, w, h).is_some())
                .collect();
            if poses.is_empty() {
                homeless.push((*r).clone());
            }
        }
        println!(
            "   on an EMPTY board (panel controls + power header only), no pose fits: {}",
            if homeless.is_empty() {
                "none — every free part has somewhere to go".to_string()
            } else {
                homeless.join(", ")
            }
        );
        println!();
    }
    Ok(())
}

/// Placement clearance the packer leaves between courtyards (board.rs
/// `PLACE_CLEARANCE_MM`), and the board-edge margin (`EDGE_MARGIN_MM`).
const CLEARANCE_MM: f64 = 1.5;
const EDGE_MM: f64 = 1.5;

/// An already-placed part as this scan sees it: which side its body is on, the
/// body's keep-out, and its through-hole pads (which are copper on *both*
/// sides).
type Obstacle = (bool, Rect, Vec<Rect>);

/// A `(min_x, min_y, max_x, max_y)` box in board millimetres.
type Rect = (f64, f64, f64, f64);

/// Exhaustive 0.5mm scan for a spot where `r` at `rot` clashes with nothing
/// already placed. Uses the same keep-out helpers the placer does, so the answer
/// is the placer's own geometry, not a re-derivation of it.
fn scan(
    r: &str,
    f: &PartFacts,
    rot: f64,
    placements: &HashMap<String, legion_of_bom_core::Placement>,
    facts: &HashMap<String, PartFacts>,
    w: f64,
    h: f64,
) -> Option<(f64, f64)> {
    let back = f.side == legion_of_bom_core::Side::Back;
    let others: Vec<Obstacle> = placements
        .iter()
        .filter(|(k, _)| k.as_str() != r)
        .filter_map(|(k, p)| {
            let of = facts.get(k)?;
            Some((
                p.back,
                of.keepout_at_rot(p.x_mm, p.y_mm, p.back, p.rotation_deg),
                of.tht_pads_at(p.x_mm, p.y_mm, p.back, p.rotation_deg),
            ))
        })
        .collect();
    let ov = |a: &Rect, b: &Rect| {
        a.0 - CLEARANCE_MM < b.2
            && b.0 - CLEARANCE_MM < a.2
            && a.1 - CLEARANCE_MM < b.3
            && b.1 - CLEARANCE_MM < a.3
    };
    let mut y = EDGE_MM;
    while y <= h - EDGE_MM {
        let mut x = EDGE_MM;
        while x <= w - EDGE_MM {
            let body = f.keepout_at_rot(x, y, back, rot);
            let pads = f.tht_pads_at(x, y, back, rot);
            let inside = |b: &Rect| {
                b.0 >= EDGE_MM && b.1 >= EDGE_MM && b.2 <= w - EDGE_MM && b.3 <= h - EDGE_MM
            };
            if inside(&body) && pads.iter().all(inside) {
                let clash = others.iter().any(|(ob, obody, opads)| {
                    (*ob == back && ov(&body, obody))
                        || opads.iter().any(|q| ov(&body, q))
                        || pads
                            .iter()
                            .any(|c| ov(c, obody) || opads.iter().any(|q| ov(c, q)))
                });
                if !clash {
                    return Some((x, y));
                }
            }
            x += 0.5;
        }
        y += 0.5;
    }
    None
}
