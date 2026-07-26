//! Render a circuit's derived panel to SVG — the fast loop for checking control
//! order, label badges and dial art without cutting metal.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example panel_preview -- \
//!     out/slew_limiter/slew_limiter.net /tmp/panel.svg [hp]
//! ```

use std::path::PathBuf;

use legion_of_bom_core::{
    build_facts, derive_panel_for, min_panel_hp_for, minimum_hp, panel_to_svg, parse_netlist_file,
    BuiltinCutouts, CircuitSource, PanelFinish, PanelFormat,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let net = PathBuf::from(
        args.next()
            .ok_or("usage: panel_preview <x.net> <out.svg> [hp]")?,
    );
    let out = PathBuf::from(args.next().ok_or("missing output path")?);
    let circuit = parse_netlist_file(&net)?;

    // Optional trailing `--format <name>`; default 3U.
    let rest: Vec<String> = args.collect();
    let format = rest
        .iter()
        .position(|a| a == "--format")
        .and_then(|i| rest.get(i + 1))
        .and_then(|f| PanelFormat::parse(f))
        .unwrap_or(PanelFormat::Eurorack3U);
    let mut args = rest
        .into_iter()
        .filter(|a| a != "--format" && PanelFormat::parse(a).is_none());
    let hp = match args.next().and_then(|s| s.parse::<u16>().ok()) {
        Some(hp) => hp,
        None => {
            let dir = legion_of_bom_core::skidl::kicad_footprint_dir()
                .ok_or("no KiCad footprint library")?;
            minimum_hp(&circuit, &build_facts(&circuit, &dir)?).max(min_panel_hp_for(
                &circuit,
                format,
                &BuiltinCutouts,
            ))
        }
    };
    let panel = derive_panel_for(&circuit, format, hp, &BuiltinCutouts);
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
    eprintln!(
        "wrote {} ({} HP, {}, {:.2}mm tall)",
        out.display(),
        panel.hp.unwrap_or(hp),
        format.as_str(),
        format.height_mm()
    );
    Ok(())
}
