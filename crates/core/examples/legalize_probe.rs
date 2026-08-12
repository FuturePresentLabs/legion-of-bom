//! Scratch harness: what does `legalize` actually do to a rotated, offset part?
//!
//! The robustness audit reported `nearest_free`'s clash test measuring
//! origin-to-origin with unrotated extents while the checker it must satisfy
//! applies rotation and `origin_offset`. Two hand-built fixtures failed to
//! reproduce it, so this prints the geometry rather than guessing at it.
//!
//! ```text
//! cargo run --release -p legion-of-bom-core --example legalize_probe
//! ```

use std::collections::HashMap;

use legion_of_bom_core::{legalize, rules, PartFacts, Placement, Rule, Side, Tier};

fn facts(w: f64, h: f64, off: (f64, f64)) -> PartFacts {
    PartFacts {
        extent: (w, h),
        body_extent: (w, h),
        origin_offset: off,
        side: Side::Front,
        height_mm: 1.0,
        standoff_mm: None,
        tht_pads: Vec::new(),
        pin_offsets: HashMap::new(),
    }
}

fn main() {
    let bounds = (0.0, 0.0, 40.0, 100.0);
    let pot = facts(14.5, 14.32, (5.35, 0.0));
    let sw = facts(9.13, 10.14, (0.0, 0.0));

    for rot in [0.0_f64, 90.0, 180.0, 270.0] {
        for pot_x in [36.0_f64, 38.0, 39.5] {
            for sw_x in [20.0_f64, 24.0, 28.0, 30.0] {
                let f: HashMap<String, PartFacts> =
                    [("RV1".into(), pot.clone()), ("SW1".into(), sw.clone())].into();
                let mut p: HashMap<String, Placement> = [
                    (
                        "RV1".to_string(),
                        Placement {
                            x_mm: pot_x,
                            y_mm: 50.0,
                            rotation_deg: rot,
                            back: false,
                        },
                    ),
                    (
                        "SW1".to_string(),
                        Placement {
                            x_mm: sw_x,
                            y_mm: 50.0,
                            rotation_deg: 0.0,
                            back: false,
                        },
                    ),
                ]
                .into();

                let edge = vec![Rule::EdgeClearance {
                    refdes: "RV1".into(),
                    extent: pot.extent,
                    origin_offset: pot.origin_offset,
                    bounds,
                    min_mm: 1.5,
                    tier: Tier::Physical,
                }];
                let report = legalize(&mut p, &edge, &f);

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
                let worst = rules::assess(&overlap, &p)
                    .into_iter()
                    .map(|a| a.margin_mm)
                    .fold(f64::INFINITY, f64::min);
                let travelled: f64 = report.moved.iter().map(|(_, d)| d).sum();
                let rv = p["RV1"];
                let bad = worst < -1e-6;
                println!(
                    "rot {rot:>5.0}  pot_x {pot_x:>5.1}  sw_x {sw_x:>5.1}  ->  \
                     RV1 ({:>6.2},{:>6.2})  moved {travelled:>7.3}  overlap margin {worst:>8.3}{}{}",
                    rv.x_mm,
                    rv.y_mm,
                    if bad { "  <<< OVERLAPS" } else { "" },
                    if travelled > 40.0 { "  <<< RAN" } else { "" },
                );
            }
        }
    }
}
