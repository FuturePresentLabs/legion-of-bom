//! Render a circuit's schematic SVG straight from a parsed netlist — the fast
//! loop for checking symbol resolution, pin labelling and wire routing against a
//! real circuit without running the whole pipeline.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example schem_preview -- \
//!     out/slew_limiter/slew_limiter.net /tmp/schem.svg
//! ```

use std::path::PathBuf;

use legion_of_bom_core::{parse_netlist_file, schematic_to_svg};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let net = PathBuf::from(
        args.next()
            .ok_or("usage: schem_preview <x.net> <out.svg>")?,
    );
    let out = PathBuf::from(args.next().ok_or("missing output path")?);
    let circuit = parse_netlist_file(&net)?;
    std::fs::write(&out, schematic_to_svg(&circuit))?;
    eprintln!("wrote {}", out.display());
    Ok(())
}
