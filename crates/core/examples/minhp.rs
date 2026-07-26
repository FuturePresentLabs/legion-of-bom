//! Report minimum_hp for a built circuit's netlist.
use legion_of_bom_core::{build_facts, minimum_hp, parse_netlist_file};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let net = std::env::args()
        .nth(1)
        .ok_or("usage: minhp <circuit.net>")?;
    let c = parse_netlist_file(std::path::Path::new(&net))?;
    let dir = legion_of_bom_core::skidl::kicad_footprint_dir().ok_or("no KiCad library")?;
    println!("minimum_hp = {}", minimum_hp(&c, &build_facts(&c, &dir)?));
    Ok(())
}
