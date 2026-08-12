//! Scratch harness: find a board where the ordering search genuinely fails and
//! negotiated congestion genuinely succeeds.
//!
//! `GridRouter` is not naive — it does rip-up-and-reroute over net orderings — so
//! any board solvable by *reordering* proves nothing about PathFinder. And a
//! single narrow gap does not work either: the cost model is Manhattan, so many
//! equal-cost paths exist and nothing pushes two nets onto the same cell. Measured
//! over gaps 0.6..2.9mm, both routers clear every one of them.
//!
//! So this sweeps the topology the module docs actually claim the advantage for:
//! MANY nets with diffuse congestion. A reversal permutation across a corridor
//! makes every pair of nets cross, which is what starves an order-based search —
//! whoever commits first takes a path that boxes in someone it cannot blame.
//!
//! ```text
//! cargo run --release -p legion-of-bom-core --example channel_sweep
//! ```

use legion_of_bom_core::{
    unroutable_by_placement, GridRouter, PadLayer, PadPoint, PathfinderRouter, RouteNet,
    RouteOptions, Router,
};

fn pad(refdes: &str, x: f64, y: f64, layer: PadLayer) -> PadPoint {
    PadPoint {
        refdes: refdes.into(),
        pad: "1".into(),
        x_mm: x,
        y_mm: y,
        w_mm: 0.8,
        h_mm: 0.8,
        layer,
    }
}

/// `n` nets crossing a corridor `span` mm wide, left pad `i` wired to right pad
/// `n-1-i` so every pair must cross. `pitch` is the pad spacing on each side.
fn crossbar(n: usize, pitch: f64, span: f64, both_layers: bool) -> (Vec<RouteNet>, RouteOptions) {
    let layer = if both_layers {
        PadLayer::Both
    } else {
        PadLayer::Front
    };
    let (lx, rx) = (100.0, 100.0 + span);
    let y0 = 100.0;
    let nets: Vec<RouteNet> = (0..n)
        .map(|i| RouteNet {
            net_idx: i + 1,
            name: format!("N{i}"),
            pads: vec![
                pad(&format!("L{i}"), lx, y0 + i as f64 * pitch, layer),
                pad(&format!("R{i}"), rx, y0 + (n - 1 - i) as f64 * pitch, layer),
            ],
        })
        .collect();
    let h = (n - 1) as f64 * pitch;
    let opts = RouteOptions {
        bounds: Some((lx - 1.0, y0 - 1.0, rx + 1.0, y0 + h + 1.0)),
        ..Default::default()
    };
    (nets, opts)
}

fn main() {
    println!(
        "  {:>3} {:>6} {:>6} {:>5} {:>5} {:>5} {:>11}",
        "n", "pitch", "span", "2lyr", "grid", "pf", "impossible"
    );
    let mut hits = 0;
    for &n in &[4_usize, 6, 8, 10] {
        for &pitch in &[0.8_f64, 1.0, 1.27] {
            for &span in &[4.0_f64, 8.0, 14.0] {
                for &both in &[true, false] {
                    let (nets, opts) = crossbar(n, pitch, span, both);
                    let grid = GridRouter.route(&nets, &opts).conflicts.len();
                    let pf = PathfinderRouter::default()
                        .route(&nets, &opts)
                        .conflicts
                        .len();
                    let imp = unroutable_by_placement(&nets, &opts).len();
                    let flag = if grid > pf && imp == 0 {
                        hits += 1;
                        "  <<< DISCRIMINATES"
                    } else {
                        ""
                    };
                    println!(
                        "  {n:>3} {pitch:>6.2} {span:>6.1} {:>5} {grid:>5} {pf:>5} {imp:>11}{flag}",
                        if both { "yes" } else { "no" }
                    );
                }
            }
        }
    }
    println!("\n  {hits} discriminating configuration(s)");
}
