//! Measure decoupling-cap-to-IC distance on a *built* board — the check that
//! settles legion-of-bom-1xm, since the score going down proves nothing.
//!
//! `cargo run -p legion-of-bom-core --example decouple_check -- <board.kicad_pcb> <circuit.net>`
use legion_of_bom_core::{build_facts, decoupling_pairs, guide, parse_netlist_file, CircuitSource};

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
    // Pin→net from the netlist, and pad→position from the built board, so the
    // *loop* can be measured rather than the proxy.
    let mut pin_net: std::collections::HashMap<(&str, &str), &str> = Default::default();
    for net in circuit.nets() {
        for p in &net.pins {
            pin_net.insert((p.refdes.0.as_str(), p.pin.as_str()), net.name.as_str());
        }
    }
    let mut worst_pin: f64 = 0.0;
    println!("  cap     IC     centre-to-centre     pad -> power pin");
    for (cap, ic) in decoupling_pairs(&circuit) {
        let (Some((x1, y1)), Some((x2, y2))) = (at(&cap), at(&ic)) else {
            println!("  {cap:>4} -> {ic:<4}   (not placed)");
            continue;
        };
        let d = (x1 - x2).hypot(y1 - y2);
        worst = worst.max(d);
        let slack = floor(&cap, &ic).map(|f| d - f);

        // The quantity that actually sets loop inductance: the cap's rail pad to
        // the IC's pin on that same rail. Centre-to-centre can look fine while
        // this is twice as far, because a power pin sits at the package's end.
        let rail = pin_net
            .iter()
            .filter(|((r, _), n)| *r == cap && is_rail(n))
            .map(|(_, n)| *n)
            .find(|n| pin_net.iter().any(|((r, _), m)| *r == ic && m == n));
        let pin_d = rail.and_then(|rail| {
            let pad = |refdes: &str| -> Option<(f64, f64)> {
                let p = placed.iter().find(|p| p.refdes == refdes)?;
                let mut pins: Vec<&str> = pin_net
                    .iter()
                    .filter(|((r, _), n)| *r == refdes && **n == rail)
                    .map(|((_, pin), _)| *pin)
                    .collect();
                pins.sort_unstable();
                let f = facts.as_ref()?.get(refdes)?;
                let &(px, py) = pins.iter().find_map(|k| f.pin_offsets.get(*k))?;
                Some(legion_of_bom_core::board::place_point(
                    legion_of_bom_core::Placement {
                        x_mm: p.cx,
                        y_mm: p.cy,
                        rotation_deg: p.rotation_deg,
                        back: p.back,
                    },
                    px,
                    py,
                ))
            };
            let (a, b) = (pad(&cap)?, pad(&ic)?);
            Some((a.0 - b.0).hypot(a.1 - b.1))
        });
        if let Some(pd) = pin_d {
            worst_pin = worst_pin.max(pd);
        }
        println!(
            "  {cap:>4} -> {ic:<4} {d:8.1} mm{:<12} {}",
            slack
                .map(|s| format!(" (slack {s:.1})"))
                .unwrap_or_default(),
            pin_d
                .map(|p| format!("{p:8.2} mm"))
                .unwrap_or_else(|| "       — ".into()),
        );
    }
    println!("worst centre-to-centre: {worst:.1} mm");
    println!("worst pad -> power pin: {worst_pin:.2} mm   <- the one that matters");
    Ok(())
}

/// A power rail (not ground) — the net a bypass cap shares with its IC.
fn is_rail(net: &str) -> bool {
    let u = net.trim().to_ascii_uppercase();
    let gnd =
        matches!(u.as_str(), "GND" | "GNDA" | "AGND" | "DGND" | "VSS" | "0") || u.ends_with("GND");
    !gnd && (u.starts_with('+')
        || u.starts_with('-')
        || matches!(u.as_str(), "VCC" | "VDD" | "VEE" | "V+" | "V-"))
}
