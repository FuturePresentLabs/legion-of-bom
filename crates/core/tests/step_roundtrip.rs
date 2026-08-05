//! Independent validation of the hand-written STEP export.
//!
//! `crates/core/src/step.rs` authors AP214 B-rep text directly rather than
//! driving a CAD kernel, because the geometry an enclosure needs — a prismatic
//! shell with cylindrical bores — is a fixed, tiny family that needs no boolean
//! operations. The risk that buys is that nothing checks our understanding of
//! the format against a second implementation.
//!
//! This test closes that. `truck-stepio` is a **dev-dependency only**; its
//! reader parses our output through `ruststep` and rebuilds the topology from
//! scratch. If our orientation flags, entity references, or surface definitions
//! were wrong, the reconstruction would disagree with what we emitted — so the
//! assertions below are a cross-implementation agreement check, not a
//! restatement of our own code.
//!
//! (truck is deliberately *not* the writer: `truck_modeling::Curve` has no
//! circle variant and `Surface` has no cylinder variant, so everything conic
//! comes out as NURBS, and its header declares `ISO-10303-042` rather than an
//! application protocol. Analytic `CIRCLE`/`CYLINDRICAL_SURFACE` geometry is
//! what makes the bores machine-recognisable downstream.)

use legion_of_bom_core::enclosure::{
    enclosure_brep, enclosure_to_step, standard_size, Enclosure, Face, FeatureKind, Hole,
};
use truck_stepio::r#in::{alias::*, Table};

/// What a second implementation made of our file.
struct Reconstructed {
    faces: usize,
    edges: usize,
    vertices: usize,
    planes: usize,
    cylinders: usize,
    other: usize,
    points: Vec<[i64; 3]>,
}

/// Parse a STEP string with truck's reader and rebuild its single closed shell.
fn reparse(step: &str) -> Reconstructed {
    let exchange = ruststep::parser::parse(step).expect("truck could not parse our STEP file");
    let table = Table::from_data_section(&exchange.data[0]);
    assert_eq!(table.shell.len(), 1, "exactly one closed shell");
    let shell = table.shell.values().next().unwrap();
    let cshell = table
        .to_compressed_shell(shell)
        .expect("truck could not rebuild the shell topology");

    let (mut planes, mut cylinders, mut other) = (0, 0, 0);
    for f in &cshell.faces {
        match &f.surface {
            Surface::ElementarySurface(e) => match **e {
                ElementarySurface::Plane(_) => planes += 1,
                ElementarySurface::CylindricalSurface(_) => cylinders += 1,
                _ => other += 1,
            },
            _ => other += 1,
        }
    }
    // Round to a micron so the comparison survives float formatting.
    let points = cshell
        .vertices
        .iter()
        .map(|p| {
            [
                (p.x * 1000.0).round() as i64,
                (p.y * 1000.0).round() as i64,
                (p.z * 1000.0).round() as i64,
            ]
        })
        .collect();

    Reconstructed {
        faces: cshell.faces.len(),
        edges: cshell.edges.len(),
        vertices: cshell.vertices.len(),
        planes,
        cylinders,
        other,
        points,
    }
}

fn demo_125b() -> Enclosure {
    Enclosure::standard("demo-125b", standard_size("125B").unwrap())
        .with_hole(Hole::new(Face::Top, -16.0, 34.0, FeatureKind::Pot))
        .with_hole(Hole::new(Face::Top, 16.0, 34.0, FeatureKind::Pot))
        .with_hole(Hole::new(Face::Top, 0.0, 8.0, FeatureKind::Switch))
        .with_hole(Hole::new(Face::Top, 0.0, -20.5, FeatureKind::Led))
        .with_hole(Hole::new(Face::Top, 0.0, -40.5, FeatureKind::Footswitch))
        .with_hole(Hole::new(Face::Right, 38.5, 0.0, FeatureKind::Jack))
        .with_hole(Hole::new(Face::Left, 38.5, 0.0, FeatureKind::Jack))
        .with_hole(Hole::new(Face::Back, 0.0, 0.0, FeatureKind::DcJack))
}

#[test]
fn an_independent_reader_rebuilds_our_enclosure_exactly() {
    let enc = demo_125b();
    let ours = enclosure_brep(&enc);
    assert!(ours.manifold_errors().is_empty());
    let theirs = reparse(&enclosure_to_step(&enc));

    assert_eq!(theirs.faces, ours.faces.len(), "face count");
    assert_eq!(theirs.edges, ours.edges.len(), "edge count");
    assert_eq!(theirs.vertices, ours.verts.len(), "vertex count");

    // The shell: 4 outer walls + 4 inner walls + top plate + cavity ceiling +
    // bottom rim = 11 planes; 4 outer corner radii + 4 inner + one bore per
    // hole = 16 cylinders.
    assert_eq!(theirs.planes, 11, "planar faces");
    assert_eq!(
        theirs.cylinders,
        8 + enc.holes.len(),
        "corner radii + bores"
    );
    assert_eq!(
        theirs.other, 0,
        "every surface stays analytic — nothing degraded to a spline"
    );

    // Same points, to the micron, in whatever order the reader chose.
    let mut mine: Vec<[i64; 3]> = ours
        .verts
        .iter()
        .map(|p| {
            [
                (p[0] * 1000.0).round() as i64,
                (p[1] * 1000.0).round() as i64,
                (p[2] * 1000.0).round() as i64,
            ]
        })
        .collect();
    let mut theirs = theirs.points;
    mine.sort();
    theirs.sort();
    assert_eq!(mine, theirs, "vertex geometry survives the round trip");
}

#[test]
fn square_cornered_shell_also_round_trips() {
    // Wall thicker than the corner radius degenerates the *inner* profile to
    // square corners while the outer stays rounded, so the two prisms have
    // different segment counts — the case most likely to mis-index a wall.
    let mut enc = demo_125b();
    enc.corner_radius_mm = 2.0;
    enc.wall_mm = 3.0;
    let ours = enclosure_brep(&enc);
    let theirs = reparse(&enclosure_to_step(&enc));
    assert_eq!(theirs.faces, ours.faces.len());
    assert_eq!(theirs.edges, ours.edges.len());
    assert_eq!(theirs.other, 0);
    // Only the four outer corners are round now.
    assert_eq!(theirs.cylinders, 4 + enc.holes.len());
}

#[test]
fn a_bare_shell_with_no_holes_round_trips() {
    let enc = Enclosure::standard("bare", standard_size("1590B").unwrap());
    let ours = enclosure_brep(&enc);
    let theirs = reparse(&enclosure_to_step(&enc));
    assert_eq!(theirs.faces, ours.faces.len());
    assert_eq!(theirs.planes, 11);
    assert_eq!(theirs.cylinders, 8);
    assert_eq!(theirs.other, 0);
}
