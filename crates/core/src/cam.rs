//! CAM setup planning for enclosure machining — DESIGN.md §7.7.
//!
//! Turns the hole table an [`Enclosure`] already carries into the thing a person
//! actually needs at the machine: which setups, in which order, on which face,
//! with which tools, and why. The geometry is the same data the STEP export and
//! the drill templates use, so a plan can't drift from the solid.
//!
//! ### Why setups are per-face
//!
//! A pedal enclosure is drilled on up to four faces, in three different planes.
//! On a 3-axis machine each face is its own setup: re-fixture, re-zero, re-run.
//! Splitting that out explicitly is most of the value here — the tool list and
//! the feeds are the easy part, and getting the box square and zeroed the same
//! way twice is the part that scraps parts.
//!
//! ### Why big holes aren't drilled
//!
//! A 12 mm twist drill in a 2.5 mm die-cast wall has almost no material to
//! stabilise it: it grabs, snatches the part, and leaves a triangular hole. Any
//! opening meaningfully larger than the wall is thick gets bored by helical
//! interpolation with a small end mill instead — see [`OpKind::HelicalBore`].
//!
//! ### Output is a starting point
//!
//! The emitted G-code is generic ISO/Fanuc-style with no machine-specific
//! preamble, tool-change convention, or coolant handling. **Simulate it and
//! read it before running it.** [`CamPlan::to_gcode`] stamps that on every file.

use std::fmt::Write as _;

use crate::enclosure::{Enclosure, Face, Hole};

// ---------------------------------------------------------------------------
//  Cutting parameters
// ---------------------------------------------------------------------------

/// Machining parameters for the stock being cut.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Material {
    pub name: &'static str,
    /// Cutting speed in m/min for HSS/carbide in this material.
    pub surface_speed_m_min: f64,
    /// Chip load scaling — die-cast alloy is gummier than 6061 and wants a
    /// slightly lighter feed per revolution.
    pub feed_factor: f64,
}

/// Die-cast aluminium (ADC12 / A380), what a Hammond 1590-series box is.
pub const DIECAST_ALUMINIUM: Material = Material {
    name: "die-cast aluminium (ADC12/A380)",
    surface_speed_m_min: 75.0,
    feed_factor: 0.85,
};

/// Wrought aluminium, for a milled-from-solid or folded-sheet enclosure.
pub const ALUMINIUM_6061: Material = Material {
    name: "aluminium 6061",
    surface_speed_m_min: 90.0,
    feed_factor: 1.0,
};

/// Machine and tooling limits the plan has to respect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CamOptions {
    pub material: Material,
    /// Spindle ceiling — small drills would otherwise ask for impossible RPM.
    pub max_rpm: f64,
    /// Spot-drill diameter, used to start every hole.
    pub spot_drill_mm: f64,
    /// End mill used to bore openings too large to drill.
    pub end_mill_mm: f64,
    /// Largest hole still drilled conventionally, as a multiple of wall
    /// thickness. Above this, bore it.
    ///
    /// The default comes from the drill's own geometry rather than a rule of
    /// thumb: a 118° point is about `0.3 × D` long, so once `0.3 × D` exceeds
    /// the wall thickness the drill has broken through the far side before its
    /// full diameter ever engaged. Past that point it stops cutting and starts
    /// punching — grabbing the part and leaving a lobed hole. `1 / 0.3 ≈ 3.3`.
    pub max_drill_wall_ratio: f64,
    /// Rapid-plane height above the face.
    pub clearance_mm: f64,
    /// How far past the far side of the wall to run, so the hole breaks fully
    /// through and the burr lands outside the part.
    pub breakthrough_mm: f64,
}

impl Default for CamOptions {
    fn default() -> Self {
        CamOptions {
            material: DIECAST_ALUMINIUM,
            max_rpm: 5000.0,
            spot_drill_mm: 6.0,
            end_mill_mm: 4.0,
            max_drill_wall_ratio: 3.3,
            clearance_mm: 5.0,
            breakthrough_mm: 1.0,
        }
    }
}

/// Spindle speed for a tool of diameter `d`, clamped to the machine ceiling.
fn rpm_for(d_mm: f64, opts: &CamOptions) -> f64 {
    let ideal = 1000.0 * opts.material.surface_speed_m_min / (std::f64::consts::PI * d_mm);
    ideal.min(opts.max_rpm).round()
}

/// Drill feed per revolution (mm/rev) — grows with diameter, the usual rule.
fn feed_per_rev(d_mm: f64, opts: &CamOptions) -> f64 {
    (0.02 * d_mm + 0.05) * opts.material.feed_factor
}

/// Steepest helical ramp, in degrees, for entering a full-width slot.
///
/// A helical bore is cutting the full width of the cutter, so the ramp angle is
/// the whole story on tool load: the cutter's end has to shear material its
/// flutes are not designed to clear. Aluminium tolerates a few degrees; much
/// past that the end loads up, rubs, and welds chips to the tool.
const MAX_RAMP_DEG: f64 = 4.0;

/// How many revolutions a helical bore takes to reach depth without exceeding
/// [`MAX_RAMP_DEG`]. Always at least one.
fn helix_revolutions(helix_radius_mm: f64, depth_mm: f64) -> usize {
    let circumference = 2.0 * std::f64::consts::PI * helix_radius_mm;
    let rise_per_rev = circumference * MAX_RAMP_DEG.to_radians().tan();
    if rise_per_rev <= 0.0 {
        return 1;
    }
    (depth_mm / rise_per_rev).ceil().max(1.0) as usize
}

// ---------------------------------------------------------------------------
//  Plan structure
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    SpotDrill,
    Drill,
    EndMill,
}

/// One tool, as it appears in the setup sheet and the tool-change lines.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    /// `T` number, 1-based.
    pub number: usize,
    pub kind: ToolKind,
    pub diameter_mm: f64,
    pub rpm: f64,
    /// Feed along the tool axis, mm/min.
    pub plunge_mm_min: f64,
    /// Feed in the cutting plane, mm/min (end mills only).
    pub feed_mm_min: f64,
}

impl Tool {
    pub fn describe(&self) -> String {
        match self.kind {
            ToolKind::SpotDrill => format!("Ø{:.1} spot drill", self.diameter_mm),
            ToolKind::Drill => format!("Ø{:.2} drill", self.diameter_mm),
            ToolKind::EndMill => format!("Ø{:.1} end mill", self.diameter_mm),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// Break the surface so the drill can't walk on the die-cast skin.
    Spot,
    /// Straight drilled through-hole.
    Drill,
    /// Helical interpolation with an end mill — for openings too big to drill
    /// safely in a thin wall.
    HelicalBore,
}

/// One operation: one tool, one kind of cut, over a set of holes.
#[derive(Debug, Clone, PartialEq)]
pub struct Op {
    pub kind: OpKind,
    /// Index into [`CamPlan::tools`].
    pub tool: usize,
    /// Target hole diameter (mm) — the finished size, not the tool size.
    pub diameter_mm: f64,
    /// Positions in the face's local `(u, v)` frame, with their labels.
    pub targets: Vec<(f64, f64, String)>,
    /// Total depth of cut below the face surface.
    pub depth_mm: f64,
}

/// Everything done in one fixturing of the part.
#[derive(Debug, Clone, PartialEq)]
pub struct Setup {
    pub face: Face,
    /// How to hold and orient the part.
    pub fixture: String,
    /// Where to zero, in words.
    pub datum: String,
    pub ops: Vec<Op>,
}

/// The full machining plan for an enclosure.
#[derive(Debug, Clone, PartialEq)]
pub struct CamPlan {
    pub part: String,
    pub material: Material,
    pub wall_mm: f64,
    pub tools: Vec<Tool>,
    pub setups: Vec<Setup>,
    /// Things the plan could not decide for you.
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
//  Planning
// ---------------------------------------------------------------------------

/// Build a machining plan: one setup per drilled face, spot-then-cut within each,
/// holes grouped by diameter so the tool changes as few times as possible.
pub fn plan_cam(enc: &Enclosure, opts: &CamOptions) -> CamPlan {
    let mut tools: Vec<Tool> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // The spot drill and the end mill are shared across every setup, so they get
    // the low tool numbers and stay in the changer.
    let spot = push_tool(&mut tools, ToolKind::SpotDrill, opts.spot_drill_mm, opts);
    let bore_limit = enc.wall_mm * opts.max_drill_wall_ratio;

    let mut setups = Vec::new();
    for face in enc.drilled_faces() {
        let holes: Vec<&Hole> = enc.holes_on(face).collect();
        // Group by finished diameter: one tool change per distinct size.
        let mut sizes: Vec<f64> = holes.iter().map(|h| h.diameter_mm).collect();
        sizes.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sizes.dedup_by(|a, b| (*a - *b).abs() < 1e-6);

        let depth = enc.wall_mm + opts.breakthrough_mm;
        let mut ops = Vec::new();

        // Spot every hole first, in one pass, before any tool that can wander.
        ops.push(Op {
            kind: OpKind::Spot,
            tool: spot,
            diameter_mm: opts.spot_drill_mm,
            targets: holes.iter().map(|h| target(h)).collect(),
            depth_mm: 1.0,
        });

        for d in sizes {
            let targets: Vec<(f64, f64, String)> = holes
                .iter()
                .filter(|h| (h.diameter_mm - d).abs() < 1e-6)
                .map(|h| target(h))
                .collect();
            if d > bore_limit {
                let tool = push_tool(&mut tools, ToolKind::EndMill, opts.end_mill_mm, opts);
                ops.push(Op {
                    kind: OpKind::HelicalBore,
                    tool,
                    diameter_mm: d,
                    targets,
                    depth_mm: depth,
                });
            } else {
                let tool = push_tool(&mut tools, ToolKind::Drill, d, opts);
                ops.push(Op {
                    kind: OpKind::Drill,
                    tool,
                    diameter_mm: d,
                    targets,
                    depth_mm: depth,
                });
            }
        }

        setups.push(Setup {
            face,
            fixture: fixture_for(face, enc),
            datum: datum_for(face, enc),
            ops,
        });
    }

    if enc.holes.iter().any(|h| h.diameter_mm > bore_limit) {
        notes.push(format!(
            "Openings over Ø{bore_limit:.1} are bored by helical interpolation, not \
             drilled: a twist drill that big has a point longer than the {:.1} mm \
             wall, so it breaks through before the full diameter engages and \
             grabs the part. A stepped drill or a chassis punch is the hand \
             equivalent.",
            enc.wall_mm
        ));
        notes.push(
            "A helical bore cuts a ring, so the disc inside it drops free on the \
             last revolution rather than being milled away. Expect a slug per \
             bored hole — inside the box on the side setups — and make sure it \
             cannot be flung or re-cut."
                .to_string(),
        );
        // A cutter that does not fit inside the bore has nothing to interpolate.
        let smallest_bore = enc
            .holes
            .iter()
            .map(|h| h.diameter_mm)
            .filter(|d| *d > bore_limit)
            .fold(f64::INFINITY, f64::min);
        if opts.end_mill_mm >= smallest_bore {
            notes.push(format!(
                "The Ø{:.1} end mill does not fit inside the Ø{smallest_bore:.1} \
                 opening it has to bore — choose a smaller cutter with \
                 `end_mill_mm`, or that hole needs a different process.",
                opts.end_mill_mm
            ));
        }
    }
    if setups.len() > 1 {
        notes.push(format!(
            "{} setups: the part is re-fixtured and re-zeroed between each. \
             Position tolerance between faces is only as good as that re-zero — \
             hold the same datum corner every time.",
            setups.len()
        ));
    }
    notes.push(
        "Back the wall with a sacrificial block so the far side doesn't tear out \
         on breakthrough, and deburr both sides before test-fitting hardware."
            .to_string(),
    );
    notes.push(
        "Die-cast alloy is gummy and inconsistent — it has a hard, sandy skin and \
         porosity underneath. Take the first hole slow and adjust."
            .to_string(),
    );

    CamPlan {
        part: enc.name.clone(),
        material: opts.material,
        wall_mm: enc.wall_mm,
        tools,
        setups,
        notes,
    }
}

fn target(h: &Hole) -> (f64, f64, String) {
    let name = h
        .label
        .clone()
        .or_else(|| h.refdes.clone())
        .unwrap_or_else(|| h.kind.as_str().to_string());
    (h.u_mm, h.v_mm, name)
}

/// Reuse an identical tool if one is already in the list, else add it.
fn push_tool(tools: &mut Vec<Tool>, kind: ToolKind, d: f64, opts: &CamOptions) -> usize {
    if let Some(i) = tools
        .iter()
        .position(|t| t.kind == kind && (t.diameter_mm - d).abs() < 1e-6)
    {
        return i;
    }
    let rpm = rpm_for(d, opts);
    // An end mill cuts on its flank; its plunge rate is deliberately much lower
    // than the rate it travels around the bore.
    let (plunge, feed) = match kind {
        ToolKind::EndMill => {
            let f = rpm * 3.0 * 0.03 * opts.material.feed_factor; // 3 flutes
            (f * 0.3, f)
        }
        _ => {
            let f = rpm * feed_per_rev(d, opts);
            (f, f)
        }
    };
    tools.push(Tool {
        number: tools.len() + 1,
        kind,
        diameter_mm: d,
        rpm,
        plunge_mm_min: plunge.round(),
        feed_mm_min: feed.round(),
    });
    tools.len() - 1
}

fn fixture_for(face: Face, enc: &Enclosure) -> String {
    let (w, d, h) = (enc.width_mm, enc.depth_mm, enc.height_mm);
    match face {
        Face::Top => format!(
            "Open side down on parallels or a soft-jaw fixture, top face up and \
             level. Clamp on the {w:.0} mm sides; do not clamp across the open \
             rim, it will spring."
        ),
        Face::Back => format!(
            "Stand the box on its front edge, back face up. Support the inside \
             of the back wall — a {d:.0} mm cantilever will chatter otherwise."
        ),
        Face::Left | Face::Right => format!(
            "Lay the box on its side, {} face up, packed against a stop so the \
             {h:.0} mm height stays square to the spindle.",
            face.as_str()
        ),
        Face::Front => "Stand the box on its back edge, front face up.".to_string(),
        Face::Bottom => "Not machinable on the shell — this is the lid.".to_string(),
    }
}

fn datum_for(face: Face, enc: &Enclosure) -> String {
    let (u, v) = enc.face_extents(face);
    format!(
        "G54 X0 Y0 at the centre of the {} face ({:.1} × {:.1} mm), Z0 on the \
         face surface. Every coordinate below is measured from there, matching \
         the drill template for this face.",
        face.as_str(),
        u * 2.0,
        v * 2.0
    )
}

// ---------------------------------------------------------------------------
//  Reports
// ---------------------------------------------------------------------------

impl CamPlan {
    /// Rough cycle time in minutes, cutting moves only — for sequencing
    /// decisions, not for quoting.
    pub fn estimated_minutes(&self) -> f64 {
        let mut total = 0.0;
        for setup in &self.setups {
            for op in &setup.ops {
                let tool = &self.tools[op.tool];
                let per_hole = match op.kind {
                    OpKind::HelicalBore => {
                        let r = (op.diameter_mm - tool.diameter_mm) / 2.0;
                        // The ramp revolutions plus the finishing one.
                        let revs = helix_revolutions(r, op.depth_mm) + 1;
                        let path = 2.0 * std::f64::consts::PI * r * revs as f64;
                        path / tool.feed_mm_min.max(1.0)
                    }
                    _ => op.depth_mm / tool.plunge_mm_min.max(1.0),
                };
                // Retract, reposition, and the approach move, per hole.
                total += op.targets.len() as f64 * (per_hole + 0.06);
            }
            total += 0.5; // tool changes and settling
        }
        total
    }

    /// The setup sheet: what to do, in order, and what the machine needs to know.
    pub fn to_markdown(&self) -> String {
        let mut s = String::with_capacity(4096);
        let _ = writeln!(s, "# Machining plan — {}\n", self.part);
        let _ = writeln!(
            s,
            "**Material:** {}  \n**Wall:** {:.1} mm  \n**Setups:** {}  \
             \n**Estimated cutting time:** ~{:.0} min\n",
            self.material.name,
            self.wall_mm,
            self.setups.len(),
            self.estimated_minutes()
        );

        let _ = writeln!(s, "## Tools\n");
        let _ = writeln!(s, "| T | Tool | RPM | Plunge | Feed |");
        let _ = writeln!(s, "|---|------|-----|--------|------|");
        for t in &self.tools {
            let _ = writeln!(
                s,
                "| T{} | {} | {:.0} | {:.0} mm/min | {:.0} mm/min |",
                t.number,
                t.describe(),
                t.rpm,
                t.plunge_mm_min,
                t.feed_mm_min
            );
        }
        s.push('\n');

        for (i, setup) in self.setups.iter().enumerate() {
            let _ = writeln!(
                s,
                "## Setup {} — {} face\n",
                i + 1,
                setup.face.as_str().to_uppercase()
            );
            let _ = writeln!(s, "**Fixturing:** {}\n", setup.fixture);
            let _ = writeln!(s, "**Datum:** {}\n", setup.datum);
            for (j, op) in setup.ops.iter().enumerate() {
                let tool = &self.tools[op.tool];
                let verb = match op.kind {
                    OpKind::Spot => "Spot".to_string(),
                    OpKind::Drill => format!("Drill Ø{:.2}", op.diameter_mm),
                    OpKind::HelicalBore => format!(
                        "Helical bore Ø{:.1} (Ø{:.1} end mill, {:.2} mm helix radius)",
                        op.diameter_mm,
                        tool.diameter_mm,
                        (op.diameter_mm - tool.diameter_mm) / 2.0
                    ),
                };
                let _ = writeln!(
                    s,
                    "{}. **{}** — T{}, {} hole(s), {:.1} mm deep",
                    j + 1,
                    verb,
                    tool.number,
                    op.targets.len(),
                    op.depth_mm
                );
                for (u, v, name) in &op.targets {
                    let _ = writeln!(s, "   - `X{u:+.2} Y{v:+.2}`  {name}");
                }
            }
            s.push('\n');
        }

        let _ = writeln!(s, "## Notes\n");
        for n in &self.notes {
            let _ = writeln!(s, "- {n}");
        }
        s
    }

    /// G-code for one setup, in the face's local frame.
    ///
    /// Generic ISO/Fanuc dialect: no machine preamble, no tool-change macro, no
    /// coolant. Treat it as a starting point to adapt to your post, and simulate
    /// before you cut.
    pub fn to_gcode(&self, setup: usize, opts: &CamOptions) -> Option<String> {
        let setup = self.setups.get(setup)?;
        let clear = opts.clearance_mm;
        let retract = clear / 2.0;
        let mut s = String::with_capacity(2048);
        let _ = writeln!(s, "( {} — {} face )", self.part, setup.face.as_str());
        let _ = writeln!(s, "( {} )", self.material.name);
        let _ = writeln!(s, "( {} )", setup.datum.replace('\n', " "));
        let _ = writeln!(
            s,
            "( GENERATED BY legion-of-bom — VERIFY AND SIMULATE BEFORE RUNNING )"
        );
        let _ = writeln!(s, "G21 G90 G94 G17 G54");
        let _ = writeln!(s, "G0 Z{clear:.1}");

        for op in &setup.ops {
            let tool = &self.tools[op.tool];
            let _ = writeln!(s, "\n( {} — {} )", tool.describe(), op_label(op));
            let _ = writeln!(s, "M5");
            let _ = writeln!(s, "T{} M6", tool.number);
            let _ = writeln!(s, "S{:.0} M3", tool.rpm);
            match op.kind {
                OpKind::Spot | OpKind::Drill => {
                    // Canned cycle: G99 retracts to R between holes, G98 clears
                    // at the end so the rapid out is safe.
                    for (i, (u, v, name)) in op.targets.iter().enumerate() {
                        if i == 0 {
                            let _ = writeln!(s, "G0 X{u:.3} Y{v:.3}");
                            let _ = writeln!(
                                s,
                                "G99 G81 X{u:.3} Y{v:.3} Z-{:.3} R{retract:.1} F{:.0} ( {name} )",
                                op.depth_mm, tool.plunge_mm_min
                            );
                        } else {
                            let _ = writeln!(s, "X{u:.3} Y{v:.3} ( {name} )");
                        }
                    }
                    let _ = writeln!(s, "G80");
                    let _ = writeln!(s, "G0 Z{clear:.1}");
                }
                OpKind::HelicalBore => {
                    // The cutter sweeps an annulus, so the disc inside it comes
                    // free rather than having to be milled away — trepanning,
                    // not pocketing. Depth is reached over as many revolutions
                    // as the ramp limit needs.
                    let r = (op.diameter_mm - tool.diameter_mm) / 2.0;
                    let revs = helix_revolutions(r, op.depth_mm);
                    for (u, v, name) in &op.targets {
                        let _ = writeln!(
                            s,
                            "( {name} — Ø{:.1}, {revs} rev helix at r{r:.3} )",
                            op.diameter_mm
                        );
                        let _ = writeln!(s, "G0 X{:.3} Y{v:.3}", u + r);
                        let _ = writeln!(s, "G0 Z{retract:.1}");
                        let _ = writeln!(s, "G1 Z0.000 F{:.0}", tool.plunge_mm_min);
                        for rev in 1..=revs {
                            let z = op.depth_mm * rev as f64 / revs as f64;
                            let _ = writeln!(
                                s,
                                "G3 X{:.3} Y{v:.3} I-{r:.3} J0.000 Z-{z:.3} F{:.0}",
                                u + r,
                                tool.feed_mm_min
                            );
                        }
                        // Flat finishing revolution, so the bore is round and
                        // full-size at the bottom where the helix was still
                        // descending.
                        let _ = writeln!(s, "G3 X{:.3} Y{v:.3} I-{r:.3} J0.000", u + r);
                        let _ = writeln!(s, "G0 Z{clear:.1}");
                    }
                }
            }
        }
        let _ = writeln!(s, "\nM5");
        let _ = writeln!(s, "G0 Z{clear:.1}");
        let _ = writeln!(s, "M30");
        Some(s)
    }
}

fn op_label(op: &Op) -> String {
    match op.kind {
        OpKind::Spot => "spot all holes".to_string(),
        OpKind::Drill => format!("drill Ø{:.2}", op.diameter_mm),
        OpKind::HelicalBore => format!("bore Ø{:.1}", op.diameter_mm),
    }
}

// ---------------------------------------------------------------------------
//  Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enclosure::{standard_size, FeatureKind, Hole};

    fn demo() -> Enclosure {
        Enclosure::standard("demo", standard_size("125B").unwrap())
            .with_hole(Hole::new(Face::Top, -16.0, 34.0, FeatureKind::Pot))
            .with_hole(Hole::new(Face::Top, 16.0, 34.0, FeatureKind::Pot))
            .with_hole(Hole::new(Face::Top, 0.0, -40.5, FeatureKind::Footswitch))
            .with_hole(Hole::new(Face::Right, 38.5, 0.0, FeatureKind::Jack))
            .with_hole(Hole::new(Face::Back, 0.0, 0.0, FeatureKind::DcJack))
    }

    #[test]
    fn one_setup_per_drilled_face_in_a_stable_order() {
        let plan = plan_cam(&demo(), &CamOptions::default());
        let faces: Vec<Face> = plan.setups.iter().map(|s| s.face).collect();
        assert_eq!(faces, vec![Face::Top, Face::Back, Face::Right]);
    }

    #[test]
    fn every_setup_spots_before_it_cuts() {
        let plan = plan_cam(&demo(), &CamOptions::default());
        for setup in &plan.setups {
            assert_eq!(
                setup.ops[0].kind,
                OpKind::Spot,
                "{} starts with a spot pass",
                setup.face.as_str()
            );
            // …and the spot pass covers every hole on that face.
            assert_eq!(
                setup.ops[0].targets.len(),
                setup.ops[1..]
                    .iter()
                    .map(|o| o.targets.len())
                    .sum::<usize>()
            );
        }
    }

    #[test]
    fn holes_too_big_for_the_wall_are_bored_not_drilled() {
        let plan = plan_cam(&demo(), &CamOptions::default());
        let top = &plan.setups[0];
        // A Ø7 pot hole still cuts properly in a 2.5 mm wall (0.3 × 7 = 2.1 mm
        // of point, inside the wall); the Ø12 footswitch cannot.
        let drilled: Vec<f64> = top
            .ops
            .iter()
            .filter(|o| o.kind == OpKind::Drill)
            .map(|o| o.diameter_mm)
            .collect();
        assert_eq!(drilled, vec![7.0]);
        let bored: Vec<f64> = top
            .ops
            .iter()
            .filter(|o| o.kind == OpKind::HelicalBore)
            .map(|o| o.diameter_mm)
            .collect();
        assert_eq!(bored, vec![12.0]);
        assert!(plan.notes.iter().any(|n| n.contains("helical")));
    }

    #[test]
    fn identical_tools_are_shared_across_setups() {
        let plan = plan_cam(&demo(), &CamOptions::default());
        // Spot drill, Ø7 drill, Ø4 end mill, Ø9.5 drill — the end mill serves
        // both the Ø12 footswitch and the Ø12 DC jack on another face.
        let mills = plan
            .tools
            .iter()
            .filter(|t| t.kind == ToolKind::EndMill)
            .count();
        assert_eq!(mills, 1);
        let spots = plan
            .tools
            .iter()
            .filter(|t| t.kind == ToolKind::SpotDrill)
            .count();
        assert_eq!(spots, 1);
        // Tool numbers are 1-based and dense.
        for (i, t) in plan.tools.iter().enumerate() {
            assert_eq!(t.number, i + 1);
        }
    }

    #[test]
    fn speeds_are_sane_and_respect_the_spindle_ceiling() {
        let opts = CamOptions::default();
        let plan = plan_cam(&demo(), &opts);
        for t in &plan.tools {
            assert!(t.rpm > 0.0 && t.rpm <= opts.max_rpm, "{t:?}");
            assert!(t.plunge_mm_min > 0.0, "{t:?}");
        }
        // A small tool wants more RPM than a big one.
        let mut by_dia = plan.tools.clone();
        by_dia.sort_by(|a, b| a.diameter_mm.partial_cmp(&b.diameter_mm).unwrap());
        assert!(by_dia.first().unwrap().rpm >= by_dia.last().unwrap().rpm);
    }

    #[test]
    fn gcode_is_wellformed_and_carries_the_safety_banner() {
        let opts = CamOptions::default();
        let plan = plan_cam(&demo(), &opts);
        let nc = plan.to_gcode(0, &opts).unwrap();
        assert!(nc.contains("VERIFY AND SIMULATE"));
        assert!(nc.contains("G21 G90"), "mm, absolute");
        assert!(nc.contains("G81"), "canned drill cycle");
        assert!(nc.contains("G80"), "and it is cancelled");
        assert!(nc.contains("G3 "), "helical bore for the footswitch");
        assert!(nc.trim_end().ends_with("M30"));
        // Every cycle that starts is cancelled before the next tool change.
        assert_eq!(nc.matches("G81").count(), nc.matches("G80").count());
        assert!(plan.to_gcode(99, &opts).is_none());
    }

    #[test]
    fn helical_bores_never_ramp_steeper_than_the_limit() {
        // The whole point of interpolating instead of drilling is tool load, so
        // diving to depth in one revolution would give the gain back. Check the
        // actual emitted Z steps, not just the helper.
        let opts = CamOptions::default();
        let plan = plan_cam(&demo(), &opts);
        let nc = plan.to_gcode(0, &opts).unwrap();
        let r = (12.0 - opts.end_mill_mm) / 2.0;
        let circumference = 2.0 * std::f64::consts::PI * r;
        let zs: Vec<f64> = nc
            .lines()
            .filter(|l| l.starts_with("G3 ") && l.contains(" Z-"))
            .map(|l| {
                let z = l.split(" Z-").nth(1).unwrap();
                z.split_whitespace().next().unwrap().parse::<f64>().unwrap()
            })
            .collect();
        assert!(zs.len() > 1, "the ramp is split across revolutions: {zs:?}");
        let mut prev = 0.0;
        for z in &zs {
            let rise = z - prev;
            let angle = (rise / circumference).atan().to_degrees();
            assert!(
                angle <= MAX_RAMP_DEG + 1e-9,
                "{angle:.2}° ramp exceeds limit"
            );
            prev = *z;
        }
        // …and the last revolution lands exactly on the full depth.
        assert!((prev - (2.5 + opts.breakthrough_mm)).abs() < 1e-9, "{prev}");
        // A finishing pass at depth follows the ramp.
        assert_eq!(
            nc.matches("G3 ").count(),
            zs.len() + 1,
            "one flat finishing revolution after the helix"
        );
        assert!(plan.notes.iter().any(|n| n.contains("slug")));
    }

    #[test]
    fn a_cutter_too_big_for_its_bore_is_called_out() {
        let opts = CamOptions {
            end_mill_mm: 12.0,
            ..Default::default()
        };
        let plan = plan_cam(&demo(), &opts);
        assert!(plan.notes.iter().any(|n| n.contains("does not fit")));
    }

    #[test]
    fn gcode_coordinates_match_the_hole_table() {
        let opts = CamOptions::default();
        let enc = demo();
        let plan = plan_cam(&enc, &opts);
        let nc = plan.to_gcode(0, &opts).unwrap();
        // The pots are at u = ±16 on the top face; they must appear verbatim.
        assert!(nc.contains("X-16.000"), "{nc}");
        assert!(nc.contains("X16.000"));
    }

    #[test]
    fn markdown_covers_every_setup_tool_and_note() {
        let plan = plan_cam(&demo(), &CamOptions::default());
        let md = plan.to_markdown();
        assert!(md.starts_with("# Machining plan — demo"));
        for setup in &plan.setups {
            assert!(md.contains(&setup.face.as_str().to_uppercase()));
        }
        for t in &plan.tools {
            assert!(md.contains(&format!("| T{} |", t.number)));
        }
        assert!(md.contains("Estimated cutting time"));
        assert!(plan.estimated_minutes() > 0.0);
    }

    #[test]
    fn an_enclosure_with_no_holes_plans_nothing() {
        let enc = Enclosure::standard("blank", standard_size("1590B").unwrap());
        let plan = plan_cam(&enc, &CamOptions::default());
        assert!(plan.setups.is_empty());
        assert_eq!(plan.estimated_minutes(), 0.0);
    }
}
