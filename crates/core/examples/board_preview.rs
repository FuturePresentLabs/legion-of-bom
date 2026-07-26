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
    derive_panel, parse_netlist_file, run_layout_loop, BoardOptions, BuiltinCutouts, LayoutLoop,
    MstRouter, SeededPlacer,
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
    // Anchor the panel controls exactly as the CLI does. Without this the greedy
    // placer has no skeleton and seeds from an arbitrary part, which is not the
    // configuration any real board is built in — and tuning against it produces
    // numbers that mean nothing.
    let panel = derive_panel(&circuit, hp as u16, &BuiltinCutouts);
    let spec = panel.to_spec().map_err(|e| format!("panel: {e}"))?;
    let anchors: HashMap<String, (f64, f64)> = spec
        .cutouts()
        .iter()
        .filter_map(|c| c.refdes.clone().map(|r| (r, (c.x_mm, h - c.y_mm))))
        .collect();
    eprintln!("anchored {} panel control(s)", anchors.len());
    let mut opts = BoardOptions::new(dir);
    opts.router = Some(Box::new(MstRouter));
    opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));

    // Through the iterative loop, not one-shot: the attempt-selection score is
    // where a good placement used to get discarded.
    let template = SeededPlacer::new(w, h, origin, anchors);
    let report = run_layout_loop(&circuit, opts, template, &LayoutLoop::default())?;
    std::fs::write(&out, &report.board)?;
    eprintln!(
        "wrote {} — {} iteration(s), score {:.1}, rule penalty {:.1}",
        out.display(),
        report.iterations,
        report.score,
        report.metrics.rule_penalty
    );
    for v in &report.metrics.violations {
        eprintln!("  broke: {}", v.what);
    }
    Ok(())
}
