//! Sweep the router's via cost against a real circuit and report what it buys.
//!
//! A via cost is not a number you can reason your way to — it trades layer
//! changes against detour length, and which is cheaper depends on how congested
//! the board is. So measure it. This is what set the 10mm default: at 2mm the
//! router hopped layers rather than looking for a way round, and both the via
//! count AND the total copper were worse.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example viasweep -- out/slew_limiter/slew_limiter.net
//! ```
use legion_of_bom_core::*;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let net = std::env::args().nth(1).ok_or("usage: viasweep <x.net>")?;
    let circuit = parse_netlist_file(std::path::Path::new(&net))?;
    let dir = skidl::kicad_footprint_dir().ok_or("no footprints")?;
    let spec_path = "crates/core/examples/fixtures/slew_limiter_panel.toml";
    let file = PanelFile::from_toml(&std::fs::read_to_string(spec_path)?)?;
    let spec = file.to_spec().map_err(|e| format!("panel: {e}"))?;
    let (w, h) = (spec.width_mm(), spec.height_mm());
    let origin = (((297.0 - w) / 2.0).max(10.0), ((210.0 - h) / 2.0).max(10.0));
    let anchors: std::collections::HashMap<String, (f64, f64)> = spec
        .cutouts()
        .iter()
        .filter_map(|c| c.refdes.clone().map(|r| (r, (c.x_mm, h - c.y_mm))))
        .collect();
    println!(
        "{:>8} {:>7} {:>10} {:>8}",
        "via_mm", "vias", "copper", "45deg%"
    );
    for via_mm in [2.0f64, 4.0, 6.0, 10.0, 16.0] {
        let mut opts = BoardOptions::new(&dir);
        opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));
        opts.route_options.via_cost_mm = via_mm;
        opts.placer = Box::new(SeededPlacer::new(w, h, origin, anchors.clone()));
        let arts = generate_board_artifacts(&circuit, &opts)?;
        let s = &arts.pcb;
        let vias = s
            .match_indices("(via")
            .filter(|(i, _)| !s[i + 4..].starts_with('s'))
            .count();
        let mut total = 0.0;
        let (mut diag, mut n) = (0usize, 0usize);
        let re = segments_of(s);
        for (x1, y1, x2, y2) in re {
            let (dx, dy) = (x2 - x1, y2 - y1);
            total += dx.hypot(dy);
            n += 1;
            if dx.abs() > 1e-9 && dy.abs() > 1e-9 {
                diag += 1;
            }
        }
        println!(
            "{via_mm:>8.1} {vias:>7} {total:>8.0}mm {:>7.1}",
            diag as f64 / n.max(1) as f64 * 100.0
        );
    }
    Ok(())
}
fn segments_of(s: &str) -> Vec<(f64, f64, f64, f64)> {
    let mut out = Vec::new();
    for part in s.split("(segment").skip(1) {
        let g = |tag: &str| -> Option<(f64, f64)> {
            let i = part.find(tag)? + tag.len();
            let rest = &part[i..];
            let end = rest.find(')')?;
            let mut it = rest[..end].split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        };
        if let (Some(a), Some(b)) = (g("(start "), g("(end ")) {
            out.push((a.0, a.1, b.0, b.1));
        }
    }
    out
}
