//! Search for the narrowest width a circuit actually **builds** in, printing the
//! evidence for every width tried.
//!
//! `minimum_hp` answers "do the parts fit between the edges", which is a floor.
//! This runs the real thing — place → route → KiCad DRC per candidate width —
//! and is the harness for `legion-of-bom-t5t`, where a 3 HP answer came back for
//! a board whose parts hang off the edge at 3 HP.
//!
//! Pass a starting width to see the rejections rather than just the answer:
//! starting at the geometric floor usually accepts immediately, which proves the
//! accept path and nothing about the reject path.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example hp_search -- \
//!     out/slew_limiter/slew_limiter.net [start-hp]
//! ```

use std::path::PathBuf;

use legion_of_bom_core::{
    build_facts, eurorack_trial_build, minimum_hp, minimum_routable_hp, parse_netlist_file,
    HpSearch, LayoutLoop,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let net = PathBuf::from(args.next().ok_or("usage: hp_search <x.net> [start-hp]")?);
    let circuit = parse_netlist_file(&net)?;
    let dir =
        legion_of_bom_core::skidl::kicad_footprint_dir().ok_or("no KiCad footprint library")?;
    let facts = build_facts(&circuit, &dir)?;

    let floor = minimum_hp(&circuit, &facts);
    let start = args
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(floor);
    eprintln!("geometric floor (parts fit): {floor} HP — searching from {start} HP");

    let cfg = LayoutLoop {
        kicad_cli: legion_of_bom_core::kicad_cli_path(),
        max_iters: 2,
        ..LayoutLoop::default()
    };
    let search = HpSearch {
        max_widths: (floor + 2).saturating_sub(start).max(1),
    };
    let found = minimum_routable_hp(&circuit, start, &search, &cfg, |hp| {
        eurorack_trial_build(&circuit, &dir, hp)
    });

    for t in &found.tried {
        match t.errors {
            None => eprintln!("  {:>2} HP · could not be built", t.hp),
            Some(0) => eprintln!("  {:>2} HP · DRC clean", t.hp),
            Some(n) => {
                let why: Vec<String> = t.kinds.iter().map(|(k, c)| format!("{c}× {k}")).collect();
                eprintln!("  {:>2} HP · {n} DRC error(s): {}", t.hp, why.join(", "));
            }
        }
    }
    match (found.unproven, found.hp) {
        (true, _) => eprintln!("routability unchecked — no kicad-cli"),
        (_, Some(hp)) => eprintln!("minimum buildable: {hp} HP"),
        (_, None) => eprintln!("nothing in range built clean"),
    }
    Ok(())
}
