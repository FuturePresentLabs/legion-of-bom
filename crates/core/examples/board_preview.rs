//! Generate a Eurorack board straight from a parsed netlist, skipping SKiDL —
//! the fast loop for checking placement and silkscreen rules against a real
//! circuit whose SKiDL source lives in another repo.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example board_preview -- \
//!     out/slew_limiter/slew_limiter.net /tmp/board.kicad_pcb 5
//! ```
//! The third argument is the panel width in HP (default 5), which sizes the
//! board and selects the Eurorack placer.

use std::collections::HashMap;
use std::path::PathBuf;

use legion_of_bom_core::{
    generate_board, parse_netlist_file, BoardOptions, EurorackPlacer, MstRouter,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let net = PathBuf::from(
        args.next()
            .ok_or("usage: board_preview <x.net> <out.kicad_pcb> [hp]")?,
    );
    let out = PathBuf::from(args.next().ok_or("missing output path")?);
    let hp: f64 = args.next().unwrap_or_else(|| "5".into()).parse()?;

    let circuit = parse_netlist_file(&net)?;
    let dir =
        legion_of_bom_core::skidl::kicad_footprint_dir().ok_or("no KiCad footprint library")?;
    let (w, h) = (hp * 5.08, 128.5);
    let origin = (100.0, 40.0);
    let mut opts = BoardOptions::new(dir);
    opts.router = Some(Box::new(MstRouter));
    opts.placer = Box::new(EurorackPlacer {
        width_mm: w,
        height_mm: h,
        origin_mm: origin,
        anchors: HashMap::new(),
    });
    opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));

    std::fs::write(&out, generate_board(&circuit, &opts)?)?;
    eprintln!("wrote {}", out.display());
    Ok(())
}
