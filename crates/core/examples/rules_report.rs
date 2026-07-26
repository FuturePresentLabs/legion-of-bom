//! What the dashboard's design-rule panel shows, from the terminal.
//! `cargo run -p legion-of-bom-core --example rules_report -- <board.kicad_pcb> <circuit.net>`
use legion_of_bom_core::{build_facts, guide, parse_netlist_file, rules};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let pcb = a
        .next()
        .ok_or("usage: rules_report <board.kicad_pcb> <circuit.net>")?;
    let net = a.next().ok_or("missing netlist")?;
    let circuit = parse_netlist_file(std::path::Path::new(&net))?;
    let src = std::fs::read_to_string(&pcb)?;
    let facts = legion_of_bom_core::skidl::kicad_footprint_dir()
        .and_then(|d| build_facts(&circuit, &d).ok());
    let derived = rules::derive_in(
        &circuit,
        &rules::Context {
            facts: facts.as_ref(),
            outline: guide::board_outline(&src),
        },
    );
    let assessed = rules::assess(&derived, &guide::placements_from_board(&src)?);
    for c in &assessed {
        println!(
            "  [{:<10}] {:>8}  {}",
            format!("{:?}", c.tier).to_lowercase(),
            format!("{:+.1}mm", c.margin_mm),
            c.detail
        );
    }
    println!(
        "{} of {} broken",
        assessed.iter().filter(|a| !a.ok()).count(),
        assessed.len()
    );
    Ok(())
}
