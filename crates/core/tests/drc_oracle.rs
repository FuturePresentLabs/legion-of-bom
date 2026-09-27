//! Oracle regression test for the first-party copper DRC (`legion-of-bom-orld`).
//!
//! Generates two fixture boards — one engineered to violate the checks the
//! first-party checker actually implements (clearance and board-edge), one
//! engineered to be clean — runs them through BOTH `pcb_drc::check_copper` and
//! `kicad-cli pcb drc`, and asserts the two agree on the implemented kinds.
//! The agreement that matters is the absence of a **false-clean**: the dirty
//! fixture must be flagged by both, the clean fixture passed by both.
//!
//! This test is `#[ignore]`d because it shells out to KiCad. Run it
//! explicitly on a machine with KiCad installed:
//!
//! ```text
//! cargo test -p legion-of-bom-core --test drc_oracle -- --ignored
//! ```
//!
//! It fails LOUD if `kicad-cli` is missing rather than returning quietly — a
//! green run that asserted nothing is the exact bug this repo tracks under
//! `legion-of-bom-69v` ("tests that cannot fail").

use std::path::PathBuf;

use legion_of_bom_core::pcb_drc::{check_copper, CopperGeometry};
use legion_of_bom_core::route::{RouteNet, Track};

/// One fixture: a label, the copper it lays down, and whether it is engineered
/// to be dirty under the implemented kinds.
struct Fixture {
    name: &'static str,
    /// `(net_idx, x1, y1, x2, y2)` for each F.Cu track at 0.25mm width.
    tracks: Vec<(usize, f64, f64, f64, f64)>,
    /// Nets referenced by those tracks, by index.
    nets: Vec<(usize, &'static str)>,
    dirty: bool,
}

const EDGE_CLEARANCE_MM: f64 = 0.5;
const CLEARANCE_MM: f64 = 0.2;
const WIDTH_MM: f64 = 0.25;

/// The single source both the checker and the emitted `.kicad_pcb` read: a
/// 10x10mm board outline and one `Fixture`'s tracks. The fixture is defined
/// once and rendered two ways, so the geometry the checker reasons about is
/// byte-for-byte the geometry KiCad inspects.
fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "tight",
            tracks: vec![
                (1, 2.0, 5.0, 8.0, 5.0),   // SIG_A, mid-board
                (2, 2.0, 5.35, 8.0, 5.35), // SIG_B, 0.10mm of air — illegal at 0.2
                (1, 2.0, 0.3, 8.0, 0.3),   // SIG_A, 0.175mm from the bottom edge
            ],
            nets: vec![(1, "SIG_A"), (2, "SIG_B")],
            dirty: true,
        },
        Fixture {
            name: "clean",
            tracks: vec![
                (1, 2.0, 5.0, 8.0, 5.0), // SIG_A
                (2, 2.0, 5.6, 8.0, 5.6), // SIG_B, 0.35mm of air — legal at 0.2
                (1, 2.0, 1.0, 8.0, 1.0), // SIG_A, 0.875mm from the edge — legal at 0.5
            ],
            nets: vec![(1, "SIG_A"), (2, "SIG_B")],
            dirty: false,
        },
    ]
}

/// Render a fixture as a minimal but loadable `.kicad_pcb`, mirroring the
/// header `board.rs` emits (same layer stack, same Edge.Cuts rectangle).
fn render_pcb(fx: &Fixture) -> String {
    let mut nets_decl = String::from("\t(net 0 \"\")\n");
    for (idx, name) in &fx.nets {
        nets_decl.push_str(&format!("\t(net {idx} \"{name}\")\n"));
    }
    let mut segs = String::new();
    for (net, x1, y1, x2, y2) in &fx.tracks {
        segs.push_str(&format!(
            "\t(segment (start {x1} {y1}) (end {x2} {y2}) (width {WIDTH_MM}) \
             (layer \"F.Cu\") (net {net}))\n"
        ));
    }
    format!(
        "(kicad_pcb\n\
         \t(version 20241229)\n\
         \t(generator \"legion-of-bom-oracle-test\")\n\
         \t(general (thickness 1.6))\n\
         \t(paper \"A4\")\n\
         \t(layers\n\
         \t\t(0 \"F.Cu\" signal)\n\
         \t\t(2 \"B.Cu\" signal)\n\
         \t\t(25 \"Edge.Cuts\" user))\n\
         \t(setup (pad_to_mask_clearance 0))\n\
         {nets_decl}\
         \t(gr_rect (start 0 0) (end 10 10) (stroke (width 0.1) (type solid)) \
         (fill no) (layer \"Edge.Cuts\"))\n\
         {segs})\n"
    )
}

/// The companion `.kicad_dru`: the rules the checker is asked with, so KiCad
/// judges the same copper against the same question.
const DRU: &str = "(version 1)\n\
(rule \"routed clearance\"   (constraint clearance (min 0.2mm)))\n\
(rule \"routed track width\" (constraint track_width (min 0.25mm)))\n";

/// Mirror of the fixture for the first-party checker: `CopperGeometry` reads
/// tracks from the router's `Track` type and needs a `RouteNet` per net.
fn copper(fx: &Fixture) -> (Vec<RouteNet>, Vec<Track>) {
    let tracks = fx
        .tracks
        .iter()
        .map(|(net, x1, y1, x2, y2)| Track {
            start: (*x1, *y1),
            end: (*x2, *y2),
            width_mm: WIDTH_MM,
            layer: "F.Cu".into(),
            net_idx: *net,
        })
        .collect();
    let nets = fx
        .nets
        .iter()
        .map(|(idx, name)| RouteNet {
            net_idx: *idx,
            name: (*name).into(),
            pads: Vec::new(),
        })
        .collect();
    (nets, tracks)
}

/// KiCad's rule key for the kind we call `edge_clearance`.
fn kicad_kind(ours: &'static str) -> &'static str {
    match ours {
        "clearance" => "clearance",
        "edge_clearance" => "copper_edge_clearance",
        other => other,
    }
}

#[test]
#[ignore = "runs kicad-cli pcb drc; enable with `-- --ignored` on a machine with KiCad"]
fn first_party_drc_agrees_with_kicad_on_implemented_kinds() {
    let kicad = legion_of_bom_core::kicad_cli_path().expect(
        "kicad-cli not found: this oracle test must not be run in --ignored \
                 mode without KiCad installed (install KiCad or drop the --ignored flag)",
    );

    let dir = temp_dir("drc-oracle");
    for fx in fixtures() {
        let pcb_path = dir.join(format!("{}.kicad_pcb", fx.name));
        std::fs::write(&pcb_path, render_pcb(&fx)).expect("write fixture pcb");
        std::fs::write(pcb_path.with_extension("kicad_dru"), DRU).expect("write fixture dru");

        // First-party checker, over the same geometry it should have emitted.
        let (nets, tracks) = copper(&fx);
        let geo = CopperGeometry {
            nets: &nets,
            tracks: &tracks,
            vias: &[],
            outline: Some((0.0, 0.0, 10.0, 10.0)),
            edge_clearance_mm: EDGE_CLEARANCE_MM,
            poured_nets: &[],
        };
        let ours = check_copper(&geo, CLEARANCE_MM);

        // KiCad oracle, over the written board.
        let oracle = legion_of_bom_core::run_drc(&pcb_path, &kicad)
            .unwrap_or_else(|e| panic!("kicad-cli drc failed on {}: {e}", fx.name));

        // Compare the two kinds the first-party checker implements. KiCad
        // always reports clearance issues at error severity, so count error
        // violations per kind.
        for kind in ["clearance", "edge_clearance"] {
            let ours_count = count(&ours, kind);
            let oracle_count = count(&oracle, kicad_kind(kind));
            assert_eq!(
                ours_count > 0,
                oracle_count > 0,
                "{}: first-party says {kind}={} but KiCad says {kind}={} ({}); \
                 presence must agree",
                fx.name,
                ours_count,
                oracle_count,
                kicad_kind(kind)
            );
        }

        // The whole point of the dirty fixture: neither checker may call it
        // clean. A first-party clean verdict on engineered-dirty copper is a
        // false-clean, the failure this test exists to catch.
        if fx.dirty {
            assert!(
                !(count(&ours, "clearance") == 0 && count(&ours, "edge_clearance") == 0),
                "{}: first-party checker called engineered-dirty copper CLEAN",
                fx.name
            );
            assert!(
                oracle.error_count() > 0,
                "{}: KiCad oracle itself found no error — the fixture no longer \
                 exercises a real violation, so this test is green for nothing",
                fx.name
            );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

fn count(report: &legion_of_bom_core::DrcReport, kind: &str) -> usize {
    report.errors().filter(|v| v.kind == kind).count()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lob-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}
