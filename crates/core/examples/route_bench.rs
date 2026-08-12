//! Router A/B harness — the same placement, routed two ways.
//!
//! The question a router has to answer is not "how much copper" but "did every
//! connection get made", so this reports **unrouted connections first** and DRC
//! errors second. Wirelength and via count are tie-breaks and nothing more: a
//! board with shorter copper and an unrouted net is not the better board, it is
//! the one that cannot be built.
//!
//! Both routers get the identical placement (same seed, same panel, same
//! iteration count), so any difference is the routing algorithm and nothing else.
//!
//! ```text
//! cargo run --release -p legion-of-bom-core --example route_bench -- \
//!     out/slew_limiter/slew_limiter.net [panel.toml] [hp]
//! ```

use std::collections::HashMap;

use legion_of_bom_core::{
    kicad_cli_path, parse_netlist_file, run_layout_loop, unroutable_by_placement, BoardOptions,
    GridRouter, LayoutLoop, PanelFile, PathfinderRouter, RouteNet, RouteOptions, RouteOutput,
    Router, SeededPlacer,
};

struct Run {
    label: &'static str,
    unrouted: usize,
    drc: usize,
    copper_mm: f64,
    vias: usize,
    score: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let net = args
        .next()
        .ok_or("usage: route_bench <x.net> [panel.toml] [hp]")?;
    let spec_path = args
        .next()
        .unwrap_or_else(|| "crates/core/examples/fixtures/slew_limiter_panel.toml".into());
    let hp_override: Option<u16> = args.next().and_then(|s| s.parse().ok());
    // Optional 4th arg: a directory to write each router's board into, so the two
    // can be rendered and compared by eye as well as by number.
    let out_dir = args.next();

    let circuit = parse_netlist_file(std::path::Path::new(&net))?;
    let dir = legion_of_bom_core::skidl::kicad_footprint_dir().ok_or("no KiCad footprints")?;

    let mut file = PanelFile::from_toml(&std::fs::read_to_string(&spec_path)?)?;
    if let Some(hp) = hp_override {
        file.hp = Some(hp);
    }
    let spec = file.to_spec().map_err(|e| format!("panel: {e}"))?;
    let (w, h) = (spec.width_mm(), spec.height_mm());
    let origin = (((297.0 - w) / 2.0).max(10.0), ((210.0 - h) / 2.0).max(10.0));
    let anchors: HashMap<String, (f64, f64)> = spec
        .cutouts()
        .iter()
        .filter_map(|c| c.refdes.clone().map(|r| (r, (c.x_mm, h - c.y_mm))))
        .collect();

    println!("{net}  ({:.0} HP, {} anchored)", w / 5.08, anchors.len());
    println!(
        "  {:<12} {:>9} {:>5} {:>11} {:>5} {:>9}",
        "router", "unrouted", "drc", "copper", "vias", "score"
    );

    let mut runs = Vec::new();
    for (label, router) in [
        ("grid", Box::new(GridRouter) as Box<dyn Router>),
        (
            "pathfinder",
            Box::new(PathfinderRouter {
                max_iters: std::env::var("LOB_PF_ITERS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(24),
            }) as Box<dyn Router>,
        ),
    ] {
        let mut opts = BoardOptions::new(dir.clone());
        opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));
        opts.router = Some(router);
        let template = SeededPlacer::new(w, h, origin, anchors.clone());
        // One iteration: the layout loop's job is to *repair* a bad first pass by
        // re-placing, which would hide which router did the work.
        let cfg = LayoutLoop {
            max_iters: 1,
            kicad_cli: kicad_cli_path(),
            ..LayoutLoop::default()
        };
        let report = run_layout_loop(&circuit, opts, template, &cfg)?;
        if let Some(dir) = &out_dir {
            std::fs::create_dir_all(dir)?;
            std::fs::write(
                std::path::Path::new(dir).join(format!("{label}.kicad_pcb")),
                &report.board,
            )?;
        }
        let run = Run {
            label,
            unrouted: report.unresolved.len(),
            drc: report.drc.as_ref().map_or(0, |d| d.errors().count()),
            copper_mm: report.metrics.routed_len_mm,
            vias: report.metrics.via_count,
            score: report.score,
        };
        println!(
            "  {:<12} {:>9} {:>5} {:>8.1}mm {:>5} {:>9.1}",
            run.label, run.unrouted, run.drc, run.copper_mm, run.vias, run.score
        );
        runs.push(run);
    }

    // Is any of this the ROUTER's fault? A probe router receives exactly the nets
    // and options the real one would, and asks a different question of them:
    // which connections are impossible for this placement, whatever routes them.
    {
        struct Probe(std::sync::Mutex<Vec<String>>);
        impl Router for Probe {
            fn route(&self, nets: &[RouteNet], opts: &RouteOptions) -> RouteOutput {
                *self.0.lock().unwrap() = unroutable_by_placement(nets, opts);
                RouteOutput::default()
            }
        }
        let probe = std::sync::Arc::new(Probe(std::sync::Mutex::new(Vec::new())));
        struct Shared(std::sync::Arc<Probe>);
        impl Router for Shared {
            fn route(&self, nets: &[RouteNet], opts: &RouteOptions) -> RouteOutput {
                self.0.route(nets, opts)
            }
        }
        let mut opts = BoardOptions::new(dir.clone());
        opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));
        if let Ok(g) = std::env::var("LOB_GRID_MM") {
            if let Ok(g) = g.parse::<f64>() {
                opts.route_options.grid_mm = g;
            }
        }
        opts.router = Some(Box::new(Shared(probe.clone())));
        let template = SeededPlacer::new(w, h, origin, anchors.clone());
        let cfg = LayoutLoop {
            max_iters: 1,
            kicad_cli: None,
            ..LayoutLoop::default()
        };
        run_layout_loop(&circuit, opts, template, &cfg)?;

        // What keep-out does the placer actually reserve for panel hardware?
        let facts = legion_of_bom_core::build_facts(&circuit, &dir)?;
        println!();
        println!("  FACTS refdes ext_w ext_h off_x off_y");
        let mut refs: Vec<&String> = facts.keys().collect();
        refs.sort();
        for r in refs {
            let f = &facts[r];
            println!(
                "  FACTS {} {:.4} {:.4} {:.4} {:.4} tht={} side={:?}",
                r,
                f.extent.0,
                f.extent.1,
                f.origin_offset.0,
                f.origin_offset.1,
                f.tht_pads.len(),
                f.side
            );
        }
        let stuck = probe.0.lock().unwrap().clone();
        println!();
        println!(
            "  impossible by placement (each net alone on the board): {}",
            stuck.len()
        );
        for line in stuck.iter().take(14) {
            println!("    {line}");
        }
    }

    // The verdict, stated in the terms that decide whether a board can be built.
    if let [grid, pf] = runs.as_slice() {
        println!();
        let d = |a: usize, b: usize| (b as i64) - (a as i64);
        println!(
            "  pathfinder vs grid: unrouted {:+}, drc {:+}, copper {:+.1}mm, vias {:+}",
            d(grid.unrouted, pf.unrouted),
            d(grid.drc, pf.drc),
            pf.copper_mm - grid.copper_mm,
            d(grid.vias, pf.vias)
        );
    }
    Ok(())
}
