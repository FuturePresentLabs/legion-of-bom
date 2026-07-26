//! Generate a Eurorack board straight from a parsed netlist, skipping SKiDL —
//! the fast loop for checking placement and silkscreen rules against a real
//! circuit whose SKiDL source lives in another repo.
//!
//! **What this is not good for.** It derives its own panel with `derive_panel`,
//! which stacks every control in one idealised centred column. The boards it
//! produces carry ~69 DRC errors where the shipped slew_limiter board carries 5.
//! So: fine for A/B-ing a placement change against *itself*, useless for judging
//! whether a board is buildable or what a width's real DRC count is. It has
//! misled this work twice — see `legion-of-bom` harness bead. Use a real
//! declared panel spec before drawing DRC-level conclusions.
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
    // Gate the winning board on real KiCad DRC — the check that says whether a
    // width is actually buildable, as opposed to geometrically plausible.
    let cfg = LayoutLoop {
        kicad_cli: legion_of_bom_core::kicad_cli_path(),
        ..LayoutLoop::default()
    };
    let report = run_layout_loop(&circuit, opts, template, &cfg)?;
    std::fs::write(&out, &report.board)?;
    eprintln!(
        "wrote {} — {} iteration(s), score {:.1}, rule penalty {:.1}",
        out.display(),
        report.iterations,
        report.score,
        report.metrics.rule_penalty
    );
    for f in &report.findings {
        eprintln!("  [{:?}] {}", f.severity, f.message);
    }
    if let Some(drc) = &report.drc {
        let mut by_kind: std::collections::BTreeMap<&str, usize> = Default::default();
        for v in drc
            .violations
            .iter()
            .chain(&drc.unconnected_items)
            .filter(|v| v.severity == "error")
        {
            *by_kind.entry(v.kind.as_str()).or_default() += 1;
        }
        eprintln!("  DRC: {} error(s)", drc.error_count());
        for (k, n) in by_kind {
            eprintln!("       {n}x {k}");
        }
    } else {
        eprintln!("  DRC: not run (no kicad-cli)");
    }
    Ok(())
}
