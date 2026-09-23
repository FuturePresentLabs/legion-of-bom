//! Why did `decouple::snap` leave that cap where it was?
//!
//! `decouple_check` measures cap-to-IC distance on a BUILT board, which tells you
//! there is a problem but not which of `snap`'s four exits caused it. This runs
//! the pass directly and prints the pairing, the report, and the before/after
//! distance for every cap — so "it did not move" and "it moved somewhere useless"
//! stop looking alike (`legion-of-bom-ku4`).
//!
//! ```text
//! cargo run --release -p legion-of-bom-core --example decouple_probe -- <circuit.net>
//! ```

use std::collections::HashMap;

use legion_of_bom_core::{
    build_facts, decoupling_pairs, snap_decoupling, EurorackPlacer, PanelFile, Placer,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let net = a
        .next()
        .ok_or("usage: decouple_probe <circuit.net> [panel.toml]")?;
    let panel = a.next();
    let circuit = legion_of_bom_core::parse_netlist_file(std::path::Path::new(&net))?;
    let dir = legion_of_bom_core::skidl::kicad_footprint_dir().ok_or("no KiCad footprints")?;
    let facts = build_facts(&circuit, &dir)?;

    // Place the same way a build does, so the input to `snap` is the input it
    // really gets rather than a synthetic one.
    let (w, h, origin, anchors) = match &panel {
        Some(p) => {
            let file = PanelFile::from_toml(&std::fs::read_to_string(p)?)?;
            let spec = file.to_spec().map_err(|e| format!("panel: {e}"))?;
            let (w, h) = (spec.width_mm(), spec.height_mm());
            let anchors: HashMap<String, (f64, f64)> = spec
                .cutouts()
                .iter()
                .filter_map(|c| c.refdes.clone().map(|r| (r, (c.x_mm, h - c.y_mm))))
                .collect();
            (
                w,
                h,
                (((297.0 - w) / 2.0).max(10.0), ((210.0 - h) / 2.0).max(10.0)),
                anchors,
            )
        }
        None => (50.0, 128.5, (10.0, 10.0), HashMap::new()),
    };

    let placer = EurorackPlacer {
        width_mm: w,
        height_mm: h,
        origin_mm: origin,
        anchors: anchors.clone(),
    };
    let mut placements = placer.place(&circuit, &facts);
    let before = placements.clone();

    let pairs = decoupling_pairs(&circuit);
    println!("  PAIRING ({} caps)", pairs.len());
    // Which IC is nearest, versus which one it was assigned? decoupling_pairs
    // balances cap COUNT across ICs and never looks at distance.
    for (cap, ic) in &pairs {
        let d = |a: &str, b: &str| {
            let (pa, pb) = (before.get(a)?, before.get(b)?);
            Some((pa.x_mm - pb.x_mm).hypot(pa.y_mm - pb.y_mm))
        };
        let assigned = d(cap, ic).unwrap_or(f64::NAN);
        let nearest = facts
            .keys()
            .filter(|r| r.starts_with('U'))
            .filter_map(|r| d(cap, r).map(|x| (r.clone(), x)))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        match nearest {
            Some((nr, nd)) if nr != *ic => println!(
                "    {cap:<4} -> {ic:<4} ({assigned:6.2}mm)   nearest is {nr} at {nd:.2}mm  <<< NOT NEAREST"
            ),
            _ => println!("    {cap:<4} -> {ic:<4} ({assigned:6.2}mm)"),
        }
    }

    let report = snap_decoupling(&mut placements, &circuit, &facts);
    println!("\n  SNAPPED ({})", report.snapped.len());
    for (cap, ic, d) in &report.snapped {
        let moved = before
            .get(cap)
            .zip(placements.get(cap))
            .map(|(a, b)| (a.x_mm - b.x_mm).hypot(a.y_mm - b.y_mm))
            .unwrap_or(f64::NAN);
        println!("    {cap:<4} -> {ic:<4} pad-to-pin {d:5.2}mm   (moved {moved:.2}mm)");
    }
    println!("\n  SKIPPED ({})", report.skipped.len());
    for s in &report.skipped {
        println!("    {s}");
    }
    Ok(())
}
