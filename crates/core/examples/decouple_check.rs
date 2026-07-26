//! Measure decoupling-cap-to-IC distance on a *built* board — the check that
//! settles legion-of-bom-1xm, since the score going down proves nothing.
//!
//! `cargo run -p legion-of-bom-core --example decouple_check -- <board.kicad_pcb> <circuit.net>`
use legion_of_bom_core::{build_facts, decoupling_pairs, guide, parse_netlist_file};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let pcb = a
        .next()
        .ok_or("usage: decouple_check <board.kicad_pcb> <circuit.net>")?;
    let net = a.next().ok_or("missing netlist")?;
    let circuit = parse_netlist_file(std::path::Path::new(&net))?;
    let placed = guide::parse_board(&std::fs::read_to_string(&pcb)?)?;
    let at = |r: &str| placed.iter().find(|p| p.refdes == r).map(|p| (p.cx, p.cy));
    // The physical floor: two keep-outs touching. Distance above this is the
    // placer's to give back; distance at it means the cap is already as close as
    // a cap can get, and the rule threshold is what needs revisiting.
    let facts = legion_of_bom_core::skidl::kicad_footprint_dir()
        .and_then(|dir| build_facts(&circuit, &dir).ok());
    let floor = |a: &str, b: &str| -> Option<f64> {
        let f = facts.as_ref()?;
        let (ea, eb) = (f.get(a)?.extent, f.get(b)?.extent);
        // Closest approach is along whichever axis needs least room.
        Some(((ea.0 + eb.0) / 2.0).min((ea.1 + eb.1) / 2.0))
    };
    let mut worst: f64 = 0.0;
    for (cap, ic) in decoupling_pairs(&circuit) {
        match (at(&cap), at(&ic)) {
            (Some((x1, y1)), Some((x2, y2))) => {
                let d = (x1 - x2).hypot(y1 - y2);
                worst = worst.max(d);
                match floor(&cap, &ic) {
                    Some(f) => println!(
                        "  {cap:>4} -> {ic:<4} {d:7.1} mm   (floor {f:5.1} mm, slack {:5.1} mm)",
                        d - f
                    ),
                    None => println!("  {cap:>4} -> {ic:<4} {d:7.1} mm"),
                }
            }
            _ => println!("  {cap:>4} -> {ic:<4}   (not placed)"),
        }
    }
    println!("worst: {worst:.1} mm");
    Ok(())
}
