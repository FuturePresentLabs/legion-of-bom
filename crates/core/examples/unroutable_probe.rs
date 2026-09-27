//! Which connections are impossible for a board's placement, whatever routes
//! them.
//!
//! `lob board` reports connections it could not route, but that number mixes
//! two very different problems: the router ran out of budget, or the parts are
//! placed so no path exists. This probe installs a router that answers the
//! second question directly — every net is routed as if it were alone on the
//! board — so a high count here means no amount of routing budget or router
//! tuning will finish the board; the placement has to move.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example unroutable_probe -- out/node/node.net
//! ```

use legion_of_bom_core::{
    generate_board_artifacts, parse_netlist_file, unroutable_by_placement, BoardOptions, RouteNet,
    RouteOptions, RouteOutput, Router,
};

struct Probe(std::sync::Mutex<Vec<String>>);

impl Router for Probe {
    fn route(&self, nets: &[RouteNet], opts: &RouteOptions) -> RouteOutput {
        *self.0.lock().unwrap() = unroutable_by_placement(nets, opts);
        RouteOutput::default()
    }
}

struct Shared(std::sync::Arc<Probe>);

impl Router for Shared {
    fn route(&self, nets: &[RouteNet], opts: &RouteOptions) -> RouteOutput {
        self.0.route(nets, opts)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let net = std::env::args()
        .nth(1)
        .ok_or("usage: unroutable_probe <x.net>")?;
    let circuit = parse_netlist_file(std::path::Path::new(&net))?;
    let dir = legion_of_bom_core::skidl::kicad_footprint_dir()
        .ok_or("no KiCad footprint library found")?;

    let probe = std::sync::Arc::new(Probe(std::sync::Mutex::new(Vec::new())));
    let mut options = BoardOptions::new(dir);
    options.router = Some(Box::new(Shared(probe.clone())));
    let _ = generate_board_artifacts(&circuit, &options)?;

    let stuck = probe.0.lock().unwrap().clone();
    println!("{net}");
    println!(
        "  impossible by placement (each net alone on the board): {}",
        stuck.len()
    );
    for line in &stuck {
        println!("    {line}");
    }
    Ok(())
}
