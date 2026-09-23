//! Generate a Eurorack board straight from a parsed netlist, skipping SKiDL —
//! the fast loop for checking placement and silkscreen rules against a real
//! circuit whose SKiDL source lives in another repo.
//!
//! Builds against a **real declared panel spec**, defaulting to
//! `examples/fixtures/slew_limiter_panel.toml` — which was itself captured off
//! the shipped board with `panel_from_board`, so it reproduces the configuration
//! that actually got manufactured.
//!
//! It used to derive its own panel with `derive_panel`, stacking every control
//! into one idealised centred column. That produced boards with ~69 DRC errors
//! where the shipped board has 5, and it misled this work twice: once by
//! anchoring nothing at all, and once by making 4 HP vs 5 HP DRC comparisons
//! look meaningful when the router was flailing at both widths for unrelated
//! reasons — the shipped board is 8 HP.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example board_preview -- \
//!     out/slew_limiter/slew_limiter.net /tmp/board.kicad_pcb [panel.toml]
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use legion_of_bom_core::{
    parse_netlist_file, run_layout_loop, BoardOptions, LayoutLoop, PanelFile, SeededPlacer,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let net = PathBuf::from(
        args.next()
            .ok_or("usage: board_preview <x.net> <out.kicad_pcb> [hp]")?,
    );
    let out = PathBuf::from(args.next().ok_or("missing output path")?);
    let spec_path = args
        .next()
        .unwrap_or_else(|| "crates/core/examples/fixtures/slew_limiter_panel.toml".into());

    let circuit = parse_netlist_file(&net)?;
    let dir =
        legion_of_bom_core::skidl::kicad_footprint_dir().ok_or("no KiCad footprint library")?;

    // Anchor the panel controls exactly as the CLI does, from a real declared
    // spec. Without this the greedy placer has no skeleton and seeds from an
    // arbitrary part, which is not the configuration any real board is built in.
    let file = PanelFile::from_toml(&std::fs::read_to_string(&spec_path)?)?;
    let spec = file.to_spec().map_err(|e| format!("panel: {e}"))?;
    let (w, h) = (spec.width_mm(), spec.height_mm());
    let origin = (((297.0 - w) / 2.0).max(10.0), ((210.0 - h) / 2.0).max(10.0));
    let anchors: HashMap<String, (f64, f64)> = spec
        .cutouts()
        .iter()
        .filter_map(|c| c.refdes.clone().map(|r| (r, (c.x_mm, h - c.y_mm))))
        .collect();
    eprintln!(
        "{} — {:.0} HP, {} anchored control(s)",
        spec_path,
        w / 5.08,
        anchors.len()
    );

    // Everything else stays at BoardOptions::new's defaults, which is what the
    // CLI uses — notably the router. Overriding it with MstRouter here was
    // producing 16 crossing tracks and 25 unrouted nets against the shipped
    // board's 5, and none of that was anything to do with placement.
    let mut opts = BoardOptions::new(dir);
    opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));

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
