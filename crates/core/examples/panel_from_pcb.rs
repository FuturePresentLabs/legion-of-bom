//! Emit a panel spec (TOML) from a built board — cutouts where the parts really
//! are. Useful for capturing a shipped board's real configuration as a fixture.
//! `cargo run -p legion-of-bom-core --example panel_from_pcb -- <board.kicad_pcb> <circuit.net>`
use legion_of_bom_core::{panel_from_board, parse_netlist_file, BuiltinCutouts};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let pcb = a
        .next()
        .ok_or("usage: panel_from_pcb <board.kicad_pcb> <circuit.net>")?;
    let net = a.next().ok_or("missing netlist")?;
    let circuit = parse_netlist_file(std::path::Path::new(&net))?;
    let panel = panel_from_board(&std::fs::read_to_string(&pcb)?, &circuit, &BuiltinCutouts)?;
    print!("{}", panel.to_toml()?);
    Ok(())
}
