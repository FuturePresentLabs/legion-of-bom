//! A minimal STEP AP214 (ISO 10303-21) writer for exact B-rep solids.
//!
//! DESIGN.md §6.7, §7.7 — the 3D side of the mechanical outputs. `panel.rs`
//! exports flat geometry (DXF, panel PCB); this exports a *solid*, which is what
//! an enclosure needs: a fit check against the board and the input to CAM.
//!
//! Scope is deliberately narrow. Rather than pull in a CAD kernel (and its
//! boolean-op failure modes) to subtract holes from a box, the geometry we
//! actually need — a prismatic shell with cylindrical through-holes — is
//! *authored* directly as a boundary representation: planar and cylindrical
//! faces, exact circles, no tessellation. Everything here is generic geometry;
//! the enclosure domain layer lives in [`crate::enclosure`].
//!
//! ### The B-rep contract
//!
//! A [`Brep`] is a closed, oriented, 2-manifold shell: every edge is used by
//! exactly two faces, once forward and once reversed, and every face normal
//! points *out of* the material. [`Brep::manifold_errors`] checks the first half
//! of that and is asserted in the tests — it is the strongest structural
//! guarantee available without linking a geometry kernel.
//!
//! Orientation convention used throughout: walking a face's boundary loop with
//! the face normal pointing at you, material is on your **left**
//! (`left = normal × direction`).

use std::fmt::Write as _;

/// A point in 3D, millimetres.
pub type Pt = [f64; 3];
/// A unit direction in 3D.
pub type Dir = [f64; 3];

/// The curve an edge follows. `Line` needs no data — its endpoints are the
/// edge's vertices. `Circle` covers both full circles (a hole's rim, where the
/// edge's two vertices coincide) and arcs (a rounded corner).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Curve {
    Line,
    Circle {
        center: Pt,
        axis: Dir,
        ref_dir: Dir,
        radius: f64,
    },
}

/// A topological edge: two vertices plus the curve between them, traversed from
/// `v0` to `v1` in the curve's own parameter direction (counter-clockwise about
/// `axis` for a circle).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edge {
    pub v0: usize,
    pub v1: usize,
    pub curve: Curve,
}

/// A closed boundary loop: edge indices with the orientation each is used in.
/// `true` = along the edge's own direction.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EdgeLoop {
    pub edges: Vec<(usize, bool)>,
}

/// The surface a face lies on. For a plane, `axis` is its normal; for a
/// cylinder, `axis` is the centreline and the natural normal points *away* from
/// it (so a face whose material is outside the cylinder sets `same_sense`
/// false).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Surface {
    Plane {
        origin: Pt,
        axis: Dir,
        ref_dir: Dir,
    },
    Cylinder {
        origin: Pt,
        axis: Dir,
        ref_dir: Dir,
        radius: f64,
    },
}

/// A trimmed face: the first bound is the outer boundary, any others are holes.
#[derive(Debug, Clone, PartialEq)]
pub struct BFace {
    pub surface: Surface,
    pub bounds: Vec<EdgeLoop>,
    /// Whether the face normal agrees with the surface's own normal.
    pub same_sense: bool,
}

/// A boundary-representation solid: vertices, edges, and oriented faces forming
/// one closed shell.
#[derive(Debug, Clone, Default)]
pub struct Brep {
    pub verts: Vec<Pt>,
    pub edges: Vec<Edge>,
    pub faces: Vec<BFace>,
}

// ---------------------------------------------------------------------------
//  Vector helpers
// ---------------------------------------------------------------------------

fn sub(a: Pt, b: Pt) -> Dir {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn norm(v: Dir) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn unit(v: Dir) -> Dir {
    let n = norm(v);
    if n < 1e-12 {
        [0.0, 0.0, 1.0]
    } else {
        [v[0] / n, v[1] / n, v[2] / n]
    }
}

fn cross(a: Dir, b: Dir) -> Dir {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Some unit direction perpendicular to `axis` — an arbitrary but deterministic
/// choice of the surface's u-axis, which does not affect the solid's geometry.
pub fn perp(axis: Dir) -> Dir {
    let a = unit(axis);
    let seed = if a[2].abs() < 0.9 {
        [0.0, 0.0, 1.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    unit(cross(seed, a))
}

// ---------------------------------------------------------------------------
//  2D profiles (the cross-section a prism is swept from)
// ---------------------------------------------------------------------------

/// A closed 2D profile in the XY plane, wound **counter-clockwise** (material to
/// the left, i.e. inside). Segment `i` runs `pts[i] → pts[(i+1) % n]`; it is a
/// straight line when `arcs[i]` is `None`, otherwise a CCW arc about the given
/// centre.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub pts: Vec<(f64, f64)>,
    pub arcs: Vec<Option<((f64, f64), f64)>>,
}

impl Profile {
    /// A rectangle from `(x0, y0)` to `(x1, y1)` with corner radius `r`.
    ///
    /// `r <= 0` (or a radius too large for the rectangle) degenerates to square
    /// corners — which is what an inner cavity profile does when the wall is
    /// thicker than the outer corner radius.
    pub fn rounded_rect(x0: f64, y0: f64, x1: f64, y1: f64, r: f64) -> Profile {
        let r = r.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0);
        if r <= 1e-6 {
            return Profile {
                pts: vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)],
                arcs: vec![None; 4],
            };
        }
        Profile {
            pts: vec![
                (x0 + r, y0), // seg 0: line along y0  (front)
                (x1 - r, y0), // seg 1: arc
                (x1, y0 + r), // seg 2: line along x1  (right)
                (x1, y1 - r), // seg 3: arc
                (x1 - r, y1), // seg 4: line along y1  (back)
                (x0 + r, y1), // seg 5: arc
                (x0, y1 - r), // seg 6: line along x0  (left)
                (x0, y0 + r), // seg 7: arc
            ],
            arcs: vec![
                None,
                Some(((x1 - r, y0 + r), r)),
                None,
                Some(((x1 - r, y1 - r), r)),
                None,
                Some(((x0 + r, y1 - r), r)),
                None,
                Some(((x0 + r, y0 + r), r)),
            ],
        }
    }

    /// Segment indices of the four straight sides, in the order
    /// `[-Y, +X, +Y, -X]` (front, right, back, left).
    pub fn side_segments(&self) -> [usize; 4] {
        if self.pts.len() == 8 {
            [0, 2, 4, 6]
        } else {
            [0, 1, 2, 3]
        }
    }

    fn len(&self) -> usize {
        self.pts.len()
    }
}

/// The edges and faces a swept profile contributed, so the caller can cap it and
/// hang holes off its walls.
#[derive(Debug, Clone, Default)]
pub struct Prism {
    /// Edge index per profile segment, at `z0`.
    pub bottom: Vec<usize>,
    /// Edge index per profile segment, at `z1`.
    pub top: Vec<usize>,
    /// Vertical edge index per profile vertex.
    pub vertical: Vec<usize>,
    /// Face index per profile segment.
    pub lateral: Vec<usize>,
}

impl Brep {
    pub fn add_vertex(&mut self, p: Pt) -> usize {
        self.verts.push(p);
        self.verts.len() - 1
    }

    pub fn add_edge(&mut self, v0: usize, v1: usize, curve: Curve) -> usize {
        self.edges.push(Edge { v0, v1, curve });
        self.edges.len() - 1
    }

    pub fn add_face(&mut self, face: BFace) -> usize {
        self.faces.push(face);
        self.faces.len() - 1
    }

    /// A loop over a ring of edges, either in order (each used forward) or fully
    /// reversed (traversed backwards, each used reversed).
    pub fn ring_loop(edges: &[usize], reversed: bool) -> EdgeLoop {
        let edges = if reversed {
            edges.iter().rev().map(|&e| (e, false)).collect()
        } else {
            edges.iter().map(|&e| (e, true)).collect()
        };
        EdgeLoop { edges }
    }

    /// Sweep `profile` from `z0` to `z1`, emitting the lateral wall faces.
    ///
    /// `inward` flips every wall's normal to point at the sweep axis — that is
    /// the cavity of a hollow shell, where material is *outside* the profile.
    /// The caller still has to cap the ends (see [`Brep::ring_loop`]); caps are
    /// left out because they usually carry hole bounds.
    pub fn add_prism(&mut self, profile: &Profile, z0: f64, z1: f64, inward: bool) -> Prism {
        let n = profile.len();
        let bot_v: Vec<usize> = (0..n)
            .map(|i| self.add_vertex([profile.pts[i].0, profile.pts[i].1, z0]))
            .collect();
        let top_v: Vec<usize> = (0..n)
            .map(|i| self.add_vertex([profile.pts[i].0, profile.pts[i].1, z1]))
            .collect();

        let ring = |brep: &mut Brep, vs: &[usize], z: f64| -> Vec<usize> {
            (0..n)
                .map(|i| {
                    let j = (i + 1) % n;
                    let curve = match profile.arcs[i] {
                        None => Curve::Line,
                        Some(((cx, cy), r)) => Curve::Circle {
                            center: [cx, cy, z],
                            axis: [0.0, 0.0, 1.0],
                            ref_dir: unit([profile.pts[i].0 - cx, profile.pts[i].1 - cy, 0.0]),
                            radius: r,
                        },
                    };
                    brep.add_edge(vs[i], vs[j], curve)
                })
                .collect()
        };
        let bottom = ring(self, &bot_v, z0);
        let top = ring(self, &top_v, z1);
        let vertical: Vec<usize> = (0..n)
            .map(|i| self.add_edge(bot_v[i], top_v[i], Curve::Line))
            .collect();

        let lateral = (0..n)
            .map(|i| {
                let j = (i + 1) % n;
                // Outward-wound: bottom → up the far edge → back along the top →
                // down the near edge. Right-hand rule puts the normal outside.
                let out = vec![
                    (bottom[i], true),
                    (vertical[j], true),
                    (top[i], false),
                    (vertical[i], false),
                ];
                let edges = if inward {
                    out.iter().rev().map(|&(e, o)| (e, !o)).collect()
                } else {
                    out
                };
                let surface = match profile.arcs[i] {
                    None => {
                        let (ax, ay) = profile.pts[i];
                        let (bx, by) = profile.pts[j];
                        let d = unit([bx - ax, by - ay, 0.0]);
                        Surface::Plane {
                            origin: [ax, ay, z0],
                            // Outward normal of a CCW segment: rotate -90°.
                            axis: [d[1], -d[0], 0.0],
                            ref_dir: d,
                        }
                    }
                    Some(((cx, cy), r)) => Surface::Cylinder {
                        origin: [cx, cy, z0],
                        axis: [0.0, 0.0, 1.0],
                        ref_dir: unit([profile.pts[i].0 - cx, profile.pts[i].1 - cy, 0.0]),
                        radius: r,
                    },
                };
                self.add_face(BFace {
                    surface,
                    bounds: vec![EdgeLoop { edges }],
                    // Surfaces are authored in the outward convention above; an
                    // inward wall is the same surface seen from the other side.
                    same_sense: !inward,
                })
            })
            .collect();

        Prism {
            bottom,
            top,
            vertical,
            lateral,
        }
    }

    /// Drill a cylindrical through-hole between two parallel faces `depth` apart.
    ///
    /// `outer_face` is the face the hole enters, with outward normal `normal`;
    /// `inner_face` is the face it exits (normal `-normal`), `depth` away along
    /// `-normal`. Adds a circular bound to each and the cylindrical wall
    /// joining them. The caller is responsible for the hole lying wholly within
    /// both faces — see `enclosure::check_enclosure`.
    pub fn add_through_hole(
        &mut self,
        outer_face: usize,
        inner_face: usize,
        p_outer: Pt,
        normal: Dir,
        depth: f64,
        radius: f64,
    ) {
        let n = unit(normal);
        let e1 = perp(n);
        let p_inner = [
            p_outer[0] - n[0] * depth,
            p_outer[1] - n[1] * depth,
            p_outer[2] - n[2] * depth,
        ];
        let start = |c: Pt| {
            [
                c[0] + e1[0] * radius,
                c[1] + e1[1] * radius,
                c[2] + e1[2] * radius,
            ]
        };

        let vo = self.add_vertex(start(p_outer));
        let vi = self.add_vertex(start(p_inner));
        let circ_o = self.add_edge(
            vo,
            vo,
            Curve::Circle {
                center: p_outer,
                axis: n,
                ref_dir: e1,
                radius,
            },
        );
        let circ_i = self.add_edge(
            vi,
            vi,
            Curve::Circle {
                center: p_inner,
                axis: n,
                ref_dir: e1,
                radius,
            },
        );

        // On each plane the rim is traversed so material stays to the left; the
        // cylinder then takes each edge in the opposite sense, which is exactly
        // the two-uses-per-edge manifold condition.
        self.faces[outer_face].bounds.push(EdgeLoop {
            edges: vec![(circ_o, false)],
        });
        self.faces[inner_face].bounds.push(EdgeLoop {
            edges: vec![(circ_i, true)],
        });
        self.add_face(BFace {
            surface: Surface::Cylinder {
                origin: p_inner,
                axis: n,
                ref_dir: e1,
                radius,
            },
            bounds: vec![
                EdgeLoop {
                    edges: vec![(circ_o, true)],
                },
                EdgeLoop {
                    edges: vec![(circ_i, false)],
                },
            ],
            // Material is outside the bore, so the face normal points at the axis.
            same_sense: false,
        });
    }

    /// Structural check: in a closed oriented 2-manifold every edge is used by
    /// exactly two face bounds, once forward and once reversed. Returns one
    /// message per violation; empty means the shell is well-formed.
    pub fn manifold_errors(&self) -> Vec<String> {
        let mut fwd = vec![0usize; self.edges.len()];
        let mut rev = vec![0usize; self.edges.len()];
        for face in &self.faces {
            for bound in &face.bounds {
                for &(e, o) in &bound.edges {
                    if o {
                        fwd[e] += 1
                    } else {
                        rev[e] += 1
                    }
                }
            }
        }
        (0..self.edges.len())
            .filter(|&i| fwd[i] != 1 || rev[i] != 1)
            .map(|i| {
                format!(
                    "edge {i}: used {} forward, {} reversed (want 1/1)",
                    fwd[i], rev[i]
                )
            })
            .collect()
    }

    /// Serialise to a STEP AP214 part file.
    ///
    /// Output is byte-deterministic (including the header timestamp) so a
    /// regenerated enclosure diffs cleanly against the previous revision —
    /// same reasoning as the deterministic UUIDs in board generation.
    pub fn to_step(&self, name: &str) -> String {
        let mut w = StepWriter::default();

        // --- units + geometric context ------------------------------------
        let len = w.add("( LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(.MILLI.,.METRE.) )");
        let ang = w.add("( NAMED_UNIT(*) PLANE_ANGLE_UNIT() SI_UNIT($,.RADIAN.) )");
        let sang = w.add("( NAMED_UNIT(*) SI_UNIT($,.STERADIAN.) SOLID_ANGLE_UNIT() )");
        let unc = w.add(&format!(
            "UNCERTAINTY_MEASURE_WITH_UNIT(LENGTH_MEASURE(1.E-07),#{len},\
             'distance_accuracy_value','confusion accuracy')"
        ));
        let ctx = w.add(&format!(
            "( GEOMETRIC_REPRESENTATION_CONTEXT(3) \
             GLOBAL_UNCERTAINTY_ASSIGNED_CONTEXT((#{unc})) \
             GLOBAL_UNIT_ASSIGNED_CONTEXT((#{len},#{ang},#{sang})) \
             REPRESENTATION_CONTEXT('','3D') )"
        ));

        // --- geometry ------------------------------------------------------
        let vertex_ids: Vec<usize> = self
            .verts
            .iter()
            .map(|p| {
                let pid = w.point(*p);
                w.add(&format!("VERTEX_POINT('',#{pid})"))
            })
            .collect();

        let edge_ids: Vec<usize> = self
            .edges
            .iter()
            .map(|e| {
                let curve = match e.curve {
                    Curve::Line => {
                        let a = self.verts[e.v0];
                        let b = self.verts[e.v1];
                        let d = sub(b, a);
                        let pid = w.point(a);
                        let did = w.direction(unit(d));
                        let vid = w.add(&format!("VECTOR('',#{did},{})", num(norm(d))));
                        w.add(&format!("LINE('',#{pid},#{vid})"))
                    }
                    Curve::Circle {
                        center,
                        axis,
                        ref_dir,
                        radius,
                    } => {
                        let a2p = w.axis2(center, axis, ref_dir);
                        w.add(&format!("CIRCLE('',#{a2p},{})", num(radius)))
                    }
                };
                w.add(&format!(
                    "EDGE_CURVE('',#{},#{},#{curve},.T.)",
                    vertex_ids[e.v0], vertex_ids[e.v1]
                ))
            })
            .collect();

        let face_ids: Vec<usize> = self
            .faces
            .iter()
            .map(|f| {
                let bounds: Vec<String> = f
                    .bounds
                    .iter()
                    .enumerate()
                    .map(|(i, l)| {
                        let oriented: Vec<String> = l
                            .edges
                            .iter()
                            .map(|&(e, o)| {
                                let id = w.add(&format!(
                                    "ORIENTED_EDGE('',*,*,#{},{})",
                                    edge_ids[e],
                                    if o { ".T." } else { ".F." }
                                ));
                                format!("#{id}")
                            })
                            .collect();
                        let loop_id = w.add(&format!("EDGE_LOOP('',({}))", oriented.join(",")));
                        let kind = if i == 0 {
                            "FACE_OUTER_BOUND"
                        } else {
                            "FACE_BOUND"
                        };
                        let bid = w.add(&format!("{kind}('',#{loop_id},.T.)"));
                        format!("#{bid}")
                    })
                    .collect();
                let surf = match f.surface {
                    Surface::Plane {
                        origin,
                        axis,
                        ref_dir,
                    } => {
                        let a2p = w.axis2(origin, axis, ref_dir);
                        w.add(&format!("PLANE('',#{a2p})"))
                    }
                    Surface::Cylinder {
                        origin,
                        axis,
                        ref_dir,
                        radius,
                    } => {
                        let a2p = w.axis2(origin, axis, ref_dir);
                        w.add(&format!("CYLINDRICAL_SURFACE('',#{a2p},{})", num(radius)))
                    }
                };
                w.add(&format!(
                    "ADVANCED_FACE('',({}),#{surf},{})",
                    bounds.join(","),
                    if f.same_sense { ".T." } else { ".F." }
                ))
            })
            .collect();

        let shell = w.add(&format!(
            "CLOSED_SHELL('',({}))",
            face_ids
                .iter()
                .map(|i| format!("#{i}"))
                .collect::<Vec<_>>()
                .join(",")
        ));
        let brep = w.add(&format!("MANIFOLD_SOLID_BREP('{}',#{shell})", esc(name)));

        // --- product structure --------------------------------------------
        let origin = w.axis2([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);
        let absr = w.add(&format!(
            "ADVANCED_BREP_SHAPE_REPRESENTATION('{}',(#{origin},#{brep}),#{ctx})",
            esc(name)
        ));
        let app = w.add("APPLICATION_CONTEXT('automotive design')");
        w.add(&format!(
            "APPLICATION_PROTOCOL_DEFINITION('international standard',\
             'automotive_design',2000,#{app})"
        ));
        let pctx = w.add(&format!("PRODUCT_CONTEXT('',#{app},'mechanical')"));
        let pdctx = w.add(&format!(
            "PRODUCT_DEFINITION_CONTEXT('part definition',#{app},'design')"
        ));
        let product = w.add(&format!("PRODUCT('{n}','{n}','',(#{pctx}))", n = esc(name)));
        w.add(&format!(
            "PRODUCT_RELATED_PRODUCT_CATEGORY('part','',(#{product}))"
        ));
        let pdf = w.add(&format!("PRODUCT_DEFINITION_FORMATION('','',#{product})"));
        let pd = w.add(&format!("PRODUCT_DEFINITION('design','',#{pdf},#{pdctx})"));
        let pds = w.add(&format!("PRODUCT_DEFINITION_SHAPE('','',#{pd})"));
        w.add(&format!("SHAPE_DEFINITION_REPRESENTATION(#{pds},#{absr})"));

        w.finish(name)
    }
}

// ---------------------------------------------------------------------------
//  STEP text emission
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StepWriter {
    entities: Vec<String>,
}

impl StepWriter {
    fn add(&mut self, body: &str) -> usize {
        self.entities.push(body.to_string());
        self.entities.len() // 1-based entity ids
    }

    fn point(&mut self, p: Pt) -> usize {
        self.add(&format!(
            "CARTESIAN_POINT('',({},{},{}))",
            num(p[0]),
            num(p[1]),
            num(p[2])
        ))
    }

    fn direction(&mut self, d: Dir) -> usize {
        self.add(&format!(
            "DIRECTION('',({},{},{}))",
            num(d[0]),
            num(d[1]),
            num(d[2])
        ))
    }

    fn axis2(&mut self, origin: Pt, axis: Dir, ref_dir: Dir) -> usize {
        let o = self.point(origin);
        let a = self.direction(unit(axis));
        let r = self.direction(unit(ref_dir));
        self.add(&format!("AXIS2_PLACEMENT_3D('',#{o},#{a},#{r})"))
    }

    fn finish(self, name: &str) -> String {
        let mut s = String::with_capacity(self.entities.len() * 48 + 1024);
        s.push_str("ISO-10303-21;\nHEADER;\n");
        let _ = writeln!(
            s,
            "FILE_DESCRIPTION(('{}'),'2;1');",
            esc(&format!("{name} enclosure"))
        );
        // Fixed timestamp: the export is a build artifact, and a stable one
        // diffs cleanly across regenerations.
        let _ = writeln!(
            s,
            "FILE_NAME('{}','1970-01-01T00:00:00',('legion-of-bom'),('Future Present Labs'),\
             'legion-of-bom','legion-of-bom','');",
            esc(name)
        );
        s.push_str("FILE_SCHEMA(('AUTOMOTIVE_DESIGN { 1 0 10303 214 3 1 1 }'));\nENDSEC;\nDATA;\n");
        for (i, e) in self.entities.iter().enumerate() {
            let _ = writeln!(s, "#{} = {};", i + 1, e);
        }
        s.push_str("ENDSEC;\nEND-ISO-10303-21;\n");
        s
    }
}

/// STEP numeric literal — always with a decimal point, as the syntax requires.
fn num(v: f64) -> String {
    // Clamp -0.0 to 0.0 so output is stable regardless of how a value was computed.
    let v = if v == 0.0 { 0.0 } else { v };
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').to_string();
    if s.ends_with('.') {
        format!("{s}0")
    } else {
        s
    }
}

/// Escape a STEP string literal (single quotes are doubled).
fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed box: prism plus two caps. The simplest solid that must satisfy
    /// the manifold contract.
    fn solid_box(w: f64, d: f64, h: f64, r: f64) -> Brep {
        let mut brep = Brep::default();
        let p = Profile::rounded_rect(0.0, 0.0, w, d, r);
        let prism = brep.add_prism(&p, 0.0, h, false);
        brep.add_face(BFace {
            surface: Surface::Plane {
                origin: [0.0, 0.0, h],
                axis: [0.0, 0.0, 1.0],
                ref_dir: [1.0, 0.0, 0.0],
            },
            bounds: vec![Brep::ring_loop(&prism.top, false)],
            same_sense: true,
        });
        brep.add_face(BFace {
            surface: Surface::Plane {
                origin: [0.0, 0.0, 0.0],
                axis: [0.0, 0.0, -1.0],
                ref_dir: [1.0, 0.0, 0.0],
            },
            bounds: vec![Brep::ring_loop(&prism.bottom, true)],
            same_sense: true,
        });
        brep
    }

    #[test]
    fn square_box_is_manifold() {
        let b = solid_box(40.0, 30.0, 20.0, 0.0);
        assert_eq!(b.faces.len(), 6);
        assert!(b.manifold_errors().is_empty(), "{:?}", b.manifold_errors());
    }

    #[test]
    fn rounded_box_is_manifold_with_cylindrical_corners() {
        let b = solid_box(40.0, 30.0, 20.0, 5.0);
        // 8 walls (4 flat + 4 corner cylinders) + 2 caps.
        assert_eq!(b.faces.len(), 10);
        assert!(b.manifold_errors().is_empty(), "{:?}", b.manifold_errors());
        let cyls = b
            .faces
            .iter()
            .filter(|f| matches!(f.surface, Surface::Cylinder { .. }))
            .count();
        assert_eq!(cyls, 4, "one cylindrical face per rounded corner");
    }

    #[test]
    fn through_hole_keeps_the_shell_manifold() {
        let mut b = solid_box(40.0, 30.0, 20.0, 0.0);
        let top = 4; // caps are appended after the 4 walls
        let bottom = 5;
        b.add_through_hole(top, bottom, [20.0, 15.0, 20.0], [0.0, 0.0, 1.0], 20.0, 3.0);
        assert!(b.manifold_errors().is_empty(), "{:?}", b.manifold_errors());
        assert_eq!(b.faces[top].bounds.len(), 2, "outer bound + hole rim");
    }

    #[test]
    fn step_output_is_wellformed_and_self_consistent() {
        let b = solid_box(40.0, 30.0, 20.0, 4.0);
        let step = b.to_step("test-box");
        assert!(step.starts_with("ISO-10303-21;"));
        assert!(step.trim_end().ends_with("END-ISO-10303-21;"));
        assert!(step.contains("AUTOMOTIVE_DESIGN"));
        assert!(step.contains("MANIFOLD_SOLID_BREP('test-box'"));
        assert!(step.contains("CYLINDRICAL_SURFACE"));
        assert!(step.contains("ADVANCED_BREP_SHAPE_REPRESENTATION"));

        // Every `#n` reference must resolve to a declared entity, and every
        // entity must be declared exactly once, in order.
        let mut declared = std::collections::HashSet::new();
        for (i, line) in step.lines().filter(|l| l.starts_with('#')).enumerate() {
            let id: usize = line[1..line.find(' ').unwrap()].parse().unwrap();
            assert_eq!(id, i + 1, "entity ids are dense and ordered");
            declared.insert(id);
        }
        for line in step.lines().filter(|l| l.starts_with('#')) {
            let (_, rhs) = line.split_once(" = ").unwrap();
            for token in rhs.split(|c: char| !(c.is_ascii_digit() || c == '#')) {
                if let Some(n) = token.strip_prefix('#') {
                    let n: usize = n.parse().unwrap();
                    assert!(declared.contains(&n), "dangling reference #{n} in {line}");
                }
            }
        }
    }

    #[test]
    fn numbers_are_valid_step_reals() {
        assert_eq!(num(0.0), "0.0");
        assert_eq!(num(-0.0), "0.0");
        assert_eq!(num(1.0), "1.0");
        assert_eq!(num(12.5), "12.5");
        assert_eq!(num(-3.25), "-3.25");
    }

    #[test]
    fn degenerate_radius_gives_square_corners() {
        let p = Profile::rounded_rect(0.0, 0.0, 10.0, 10.0, 0.0);
        assert_eq!(p.pts.len(), 4);
        assert!(p.arcs.iter().all(Option::is_none));
        // A radius larger than half the shorter side is clamped, not rejected.
        let p = Profile::rounded_rect(0.0, 0.0, 10.0, 4.0, 99.0);
        assert_eq!(p.pts.len(), 8);
    }
}
