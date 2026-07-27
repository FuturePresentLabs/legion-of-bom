//! Placement A/B harness — one number per thing placement is supposed to buy.
//!
//! `board_preview` prints the winning board's score, which is the layout loop's
//! answer after up to six repair attempts. That is the right gate and the wrong
//! measurement for judging a *placer*: repair can paper over a bad first pass,
//! and a score is one number over five terms.
//!
//! This defaults to `max_iters = 1` — the pure placement, no repair — and prints
//! the terms separately, plus a fingerprint of the placement so two runs can be
//! compared for reproducibility.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example place_bench -- \
//!     out/slew_limiter/slew_limiter.net [panel.toml] [iters]
//! ```

use std::collections::HashMap;

use legion_of_bom_core::board::{build_facts, Placer};
use legion_of_bom_core::{
    parse_netlist_file, run_layout_loop, BoardOptions, LayoutLoop, PanelFile, SeededPlacer,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let net = args
        .next()
        .ok_or("usage: place_bench <x.net> [panel.toml]")?;
    let spec_path = args
        .next()
        .unwrap_or_else(|| "crates/core/examples/fixtures/slew_limiter_panel.toml".into());
    let iters: usize = args.next().map_or(1, |s| s.parse().unwrap_or(1));

    let circuit = parse_netlist_file(std::path::Path::new(&net))?;
    let dir =
        legion_of_bom_core::skidl::kicad_footprint_dir().ok_or("no KiCad footprint library")?;

    let file = PanelFile::from_toml(&std::fs::read_to_string(&spec_path)?)?;
    let spec = file.to_spec().map_err(|e| format!("panel: {e}"))?;
    let (w, h) = (spec.width_mm(), spec.height_mm());
    let origin = (((297.0 - w) / 2.0).max(10.0), ((210.0 - h) / 2.0).max(10.0));
    let anchors: HashMap<String, (f64, f64)> = spec
        .cutouts()
        .iter()
        .filter_map(|c| c.refdes.clone().map(|r| (r, (c.x_mm, h - c.y_mm))))
        .collect();

    let opts_footprint_dir = dir.clone();
    let mut opts = BoardOptions::new(dir);
    opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));
    let template = SeededPlacer::new(w, h, origin, anchors.clone());
    let cfg = LayoutLoop {
        max_iters: iters,
        kicad_cli: legion_of_bom_core::kicad_cli_path(),
        ..LayoutLoop::default()
    };
    let report = run_layout_loop(&circuit, opts, template, &cfg)?;
    let m = &report.metrics;
    println!(
        "{net}  ({:.0} HP, {} anchored, {} iters)",
        w / 5.08,
        anchors.len(),
        report.iterations
    );
    println!("  score          {:8.1}", report.score);
    println!("  hpwl           {:8.1} mm", m.hpwl_mm);
    println!("  signal hpwl    {:8.1} mm", m.signal_hpwl_mm);
    println!("  critical hpwl  {:8.1} mm", m.critical_hpwl_mm);
    println!("  routed copper  {:8.1} mm", m.routed_len_mm);
    println!("  vias           {:8}", m.via_count);
    println!("  unrouted       {:8}", m.unrouted);
    println!("  rule penalty   {:8.1}", m.rule_penalty);
    println!(
        "  DRC errors     {:8}",
        report
            .drc
            .as_ref()
            .map_or("n/a".to_string(), |d| d.error_count().to_string())
    );
    for v in &m.violations {
        println!("  ! {v:?}");
    }

    // A fingerprint of the placement. Run this twice and compare: Rust reseeds
    // hash iteration *per process*, so anything that leaks hash order cannot be
    // caught by comparing two calls inside one run — only by comparing two runs.
    // That is how the router's net ordering was caught (`legion-of-bom-gns`),
    // and it had been shipping boards that differed between builds.
    let facts = build_facts(&circuit, &opts_footprint_dir)?;
    let packed = SeededPlacer::new(w, h, (0.0, 0.0), anchors).place(&circuit, &facts);
    let mut sig: Vec<String> = packed
        .iter()
        .map(|(r, p)| format!("{r}:{:.3},{:.3},{}", p.x_mm, p.y_mm, p.rotation_deg))
        .collect();
    sig.sort();
    let mut fnv: u64 = 1469598103934665603;
    for byte in sig.join(" ").bytes() {
        fnv ^= byte as u64;
        fnv = fnv.wrapping_mul(1099511628211);
    }
    println!("  placement      {fnv:016x}  (same across runs, or it is not deterministic)");
    Ok(())
}
