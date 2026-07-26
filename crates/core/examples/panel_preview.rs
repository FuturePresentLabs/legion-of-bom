//! Render a circuit's derived panel to SVG — the fast loop for checking control
//! order, label badges and dial art without cutting metal.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example panel_preview -- \
//!     out/slew_limiter/slew_limiter.net /tmp/panel.svg [hp]
//! ```

use std::path::PathBuf;

use legion_of_bom_core::{
    build_facts, derive_panel, minimum_hp, panel_to_svg, parse_netlist_file, BuiltinCutouts,
    CircuitSource, PanelFinish,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let net = PathBuf::from(
        args.next()
            .ok_or("usage: panel_preview <x.net> <out.svg> [hp]")?,
    );
    let out = PathBuf::from(args.next().ok_or("missing output path")?);
    let circuit = parse_netlist_file(&net)?;

    let hp = match args.next().and_then(|s| s.parse::<u16>().ok()) {
        Some(hp) => hp,
        None => {
            let dir = legion_of_bom_core::skidl::kicad_footprint_dir()
                .ok_or("no KiCad footprint library")?;
            minimum_hp(&circuit, &build_facts(&circuit, &dir)?)
        }
    };
    let panel = derive_panel(&circuit, hp, &BuiltinCutouts);
    for c in &panel.cutouts {
        eprintln!(
            "  {:>4}  {:<12} {:<14} y={:6.1}",
            c.refdes.as_deref().unwrap_or("-"),
            c.label.as_deref().unwrap_or("-"),
            c.role.as_deref().unwrap_or("-"),
            c.y_mm,
        );
    }
    let spec = panel.to_spec().map_err(|e| format!("panel: {e}"))?;
    let svg = panel_to_svg(
        spec.as_ref(),
        circuit.name(),
        &PanelFinish::named("black"),
        None,
    );
    std::fs::write(&out, svg)?;
    eprintln!("wrote {} ({hp} HP)", out.display());
    Ok(())
}
