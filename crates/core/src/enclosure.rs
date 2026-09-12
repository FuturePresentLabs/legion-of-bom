//! Guitar-pedal enclosure specification, derivation, DFM checks, and 3D export.
//!
//! DESIGN.md §6.1 (format profiles — Guitar Pedal), §6.7 (mechanical checks),
//! §7.7 (drilling jigs / templates).
//!
//! This is the pedal counterpart to [`crate::panel`]'s Eurorack `PanelSpec`, and
//! deliberately *not* an implementation of that trait: a Eurorack panel is a flat
//! plate with one working face, while a pedal enclosure is a five-sided box whose
//! holes land on four different faces in three different planes. Forcing the 2D
//! trait to carry that would be the "hard-code one format's concepts into the
//! shared abstraction" mistake DESIGN §6.9 warns about. What *is* shared is the
//! [`CutoutSource`] seam — a part's mechanical data belongs with the part, not in
//! the generator — so the same parts-library data feeds both.
//!
//! ### Orientation
//!
//! The enclosure is modelled as it sits on the floor, in playing position:
//!
//! ```text
//!         +Z  up (Top face — knobs, footswitch, LED)
//!          |
//!          |   +Y  away from the player (Back face — power)
//!          |  /
//!          | /
//!          +--------- +X  to the player's right
//!
//!   origin = the front-left-bottom corner of the outer envelope
//! ```
//!
//! A die-cast pedal box is used *upside down* relative to how it ships: the
//! closed face of the shell becomes the Top of the pedal and gets drilled, and
//! the removable lid becomes the base plate. So the solid exported here is an
//! **open-bottom shell** — the lid is a separate flat part and is not modelled.

use crate::model::Part;
use crate::panel::{ControlKind, CutoutSource};
use crate::source::CircuitSource;
use crate::stage::{Finding, StageOutcome};
use crate::step::{BFace, Brep, Profile, Surface};

// ---------------------------------------------------------------------------
//  Faces
// ---------------------------------------------------------------------------

/// One face of the enclosure.
///
/// Every face has a local `(u, v)` frame with its **origin at the face centre**,
/// so a symmetric layout is symmetric about zero. `u` and `v` always track world
/// axes rather than "as you look at the face", so a hole's `u` means the same
/// world position on the Front and Back faces — which is what matters when
/// cross-referencing a hole against a board coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Face {
    /// `z = height`, normal `+Z`. `u → +X`, `v → +Y`. Controls live here.
    Top,
    /// `z = 0`, normal `-Z` — the open side, closed by the separate lid.
    Bottom,
    /// `y = 0`, normal `-Y` (nearest the player). `u → +X`, `v → +Z`.
    Front,
    /// `y = depth`, normal `+Y`. `u → +X`, `v → +Z`. Power lives here.
    Back,
    /// `x = 0`, normal `-X`. `u → +Y`, `v → +Z`.
    Left,
    /// `x = width`, normal `+X`. `u → +Y`, `v → +Z`.
    Right,
}

impl Face {
    /// Outward unit normal in world coordinates.
    pub fn normal(self) -> [f64; 3] {
        match self {
            Face::Top => [0.0, 0.0, 1.0],
            Face::Bottom => [0.0, 0.0, -1.0],
            Face::Front => [0.0, -1.0, 0.0],
            Face::Back => [0.0, 1.0, 0.0],
            Face::Left => [-1.0, 0.0, 0.0],
            Face::Right => [1.0, 0.0, 0.0],
        }
    }

    /// Whether this is one of the four vertical walls.
    pub fn is_side(self) -> bool {
        matches!(self, Face::Front | Face::Back | Face::Left | Face::Right)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Face::Top => "top",
            Face::Bottom => "bottom",
            Face::Front => "front",
            Face::Back => "back",
            Face::Left => "left",
            Face::Right => "right",
        }
    }

    /// Index into [`Profile::side_segments`] (`[front, right, back, left]`).
    fn side_index(self) -> Option<usize> {
        match self {
            Face::Front => Some(0),
            Face::Right => Some(1),
            Face::Back => Some(2),
            Face::Left => Some(3),
            _ => None,
        }
    }
}

impl std::str::FromStr for Face {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "top" => Ok(Face::Top),
            "bottom" | "base" | "lid" => Ok(Face::Bottom),
            "front" => Ok(Face::Front),
            "back" | "rear" => Ok(Face::Back),
            "left" => Ok(Face::Left),
            "right" => Ok(Face::Right),
            other => Err(format!("unknown face: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
//  Standard enclosure sizes
// ---------------------------------------------------------------------------

/// The dimensions of a standard die-cast enclosure, in mm.
///
/// **These are nominal catalogue values, not measured ones.** Die-cast tolerances
/// are loose (±0.5 mm is normal) and wall thickness varies with draft; check a
/// real part before committing to a tight-fitting board. Every field is
/// overridable per spec — see [`EnclosureFile`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnclosureSize {
    /// Catalogue name, e.g. `"125B"`.
    pub name: &'static str,
    /// Across the pedal, left-to-right (world X).
    pub width_mm: f64,
    /// Front-to-back, toward the player (world Y). The catalogue "length".
    pub depth_mm: f64,
    /// Floor to top face (world Z).
    pub height_mm: f64,
    /// Nominal wall/top thickness.
    pub wall_mm: f64,
    /// Outer vertical corner radius.
    pub corner_radius_mm: f64,
}

/// Nominal wall thickness for the Hammond 1590/125 die-cast family.
const DIECAST_WALL_MM: f64 = 2.5;
/// Nominal outer vertical corner radius for the same family.
const DIECAST_CORNER_MM: f64 = 5.0;

/// The supported standard sizes. Catalogue dimensions are quoted length × width
/// × height; here length becomes `depth_mm` because the long axis of a pedal
/// runs front-to-back.
pub const STANDARD_SIZES: &[EnclosureSize] = &[
    EnclosureSize {
        name: "1590A",
        width_mm: 38.5,
        depth_mm: 92.5,
        height_mm: 31.0,
        wall_mm: DIECAST_WALL_MM,
        corner_radius_mm: 4.0,
    },
    EnclosureSize {
        name: "1590B",
        width_mm: 60.3,
        depth_mm: 112.4,
        height_mm: 31.0,
        wall_mm: DIECAST_WALL_MM,
        corner_radius_mm: DIECAST_CORNER_MM,
    },
    EnclosureSize {
        name: "125B",
        width_mm: 66.0,
        depth_mm: 121.0,
        height_mm: 39.5,
        wall_mm: DIECAST_WALL_MM,
        corner_radius_mm: DIECAST_CORNER_MM,
    },
    EnclosureSize {
        name: "1590BB",
        width_mm: 94.0,
        depth_mm: 119.0,
        height_mm: 34.0,
        wall_mm: DIECAST_WALL_MM,
        corner_radius_mm: DIECAST_CORNER_MM,
    },
    EnclosureSize {
        name: "1590XX",
        width_mm: 121.0,
        depth_mm: 145.0,
        height_mm: 39.0,
        wall_mm: DIECAST_WALL_MM,
        corner_radius_mm: DIECAST_CORNER_MM,
    },
    EnclosureSize {
        name: "1590DD",
        width_mm: 119.5,
        depth_mm: 188.0,
        height_mm: 56.0,
        wall_mm: 3.0,
        corner_radius_mm: 6.0,
    },
];

/// Look up a standard size by name (case-insensitive). `1590N1` is Hammond's
/// part number for the same box the pedal world calls a 125B.
pub fn standard_size(name: &str) -> Option<EnclosureSize> {
    let want = name.trim().to_ascii_uppercase();
    let want = if want == "1590N1" {
        "125B".to_string()
    } else {
        want
    };
    STANDARD_SIZES.iter().copied().find(|s| s.name == want)
}

// ---------------------------------------------------------------------------
//  Features
// ---------------------------------------------------------------------------

/// What a hole is for. Drives the default drill diameter, the body clearance
/// used for spacing checks, and which face the derivation puts it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureKind {
    /// Panel-mount potentiometer (9 mm bushing).
    Pot,
    /// Mini toggle / rotary switch bushing.
    Switch,
    /// 3PDT stomp switch.
    Footswitch,
    /// Indicator LED (bare 5 mm; a bezel needs the diameter overridden).
    Led,
    /// 1/4" audio jack.
    Jack,
    /// 2.1 mm barrel power jack.
    DcJack,
    /// A plain mounting / clearance hole.
    Mounting,
}

impl FeatureKind {
    /// Default drill diameter (mm) for the usual hardware.
    pub fn hole_mm(self) -> f64 {
        match self {
            FeatureKind::Pot => 7.0,
            FeatureKind::Switch => 6.0,
            FeatureKind::Footswitch => 12.0,
            FeatureKind::Led => 5.0,
            FeatureKind::Jack => 9.5,
            FeatureKind::DcJack => 12.0,
            FeatureKind::Mounting => 3.2,
        }
    }

    /// Diameter (mm) of the largest thing that occupies the space around the
    /// hole — the knob, the nut, the switch boot. Two features must not overlap
    /// by this measure even though their holes are much smaller.
    pub fn body_mm(self) -> f64 {
        match self {
            FeatureKind::Pot => 20.0,
            FeatureKind::Switch => 12.0,
            FeatureKind::Footswitch => 24.0,
            FeatureKind::Led => 10.0,
            FeatureKind::Jack => 15.0,
            FeatureKind::DcJack => 14.0,
            FeatureKind::Mounting => 6.0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FeatureKind::Pot => "pot",
            FeatureKind::Switch => "switch",
            FeatureKind::Footswitch => "footswitch",
            FeatureKind::Led => "led",
            FeatureKind::Jack => "jack",
            FeatureKind::DcJack => "dc_jack",
            FeatureKind::Mounting => "mounting",
        }
    }
}

impl From<ControlKind> for FeatureKind {
    fn from(k: ControlKind) -> Self {
        match k {
            ControlKind::Pot => FeatureKind::Pot,
            ControlKind::Switch => FeatureKind::Switch,
            ControlKind::Led => FeatureKind::Led,
            ControlKind::Jack => FeatureKind::Jack,
        }
    }
}

/// One drilled hole, anchored to a face in that face's local `(u, v)` frame.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Hole {
    pub face: Face,
    pub u_mm: f64,
    pub v_mm: f64,
    pub diameter_mm: f64,
    pub kind: FeatureKind,
    /// The board part this hole is for (`"RV1"`), when it came from a circuit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refdes: Option<String>,
    /// Legend text for this control (`"GAIN"`, `"IN"`), for the drill template
    /// and, later, engraving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Hole {
    pub fn new(face: Face, u_mm: f64, v_mm: f64, kind: FeatureKind) -> Self {
        Hole {
            face,
            u_mm,
            v_mm,
            diameter_mm: kind.hole_mm(),
            kind,
            refdes: None,
            label: None,
        }
    }

    pub fn with_refdes(mut self, refdes: impl Into<String>) -> Self {
        self.refdes = Some(refdes.into());
        self
    }

    pub fn with_label(mut self, label: Option<String>) -> Self {
        self.label = label;
        self
    }

    pub fn radius_mm(&self) -> f64 {
        self.diameter_mm / 2.0
    }
}

// ---------------------------------------------------------------------------
//  The enclosure
// ---------------------------------------------------------------------------

/// A resolved enclosure: outer envelope plus every drilled hole.
#[derive(Debug, Clone, PartialEq)]
pub struct Enclosure {
    pub name: String,
    pub width_mm: f64,
    pub depth_mm: f64,
    pub height_mm: f64,
    pub wall_mm: f64,
    pub corner_radius_mm: f64,
    pub holes: Vec<Hole>,
}

impl Enclosure {
    /// A bare enclosure of a standard size, with no holes.
    pub fn standard(name: &str, size: EnclosureSize) -> Self {
        Enclosure {
            name: name.to_string(),
            width_mm: size.width_mm,
            depth_mm: size.depth_mm,
            height_mm: size.height_mm,
            wall_mm: size.wall_mm,
            corner_radius_mm: size.corner_radius_mm,
            holes: Vec::new(),
        }
    }

    pub fn with_hole(mut self, hole: Hole) -> Self {
        self.holes.push(hole);
        self
    }

    /// Usable interior of the cavity: `(width, depth, height)` in mm. The height
    /// is floor-to-ceiling; a board also has to clear whatever hangs off the
    /// controls.
    pub fn cavity_mm(&self) -> (f64, f64, f64) {
        (
            self.width_mm - 2.0 * self.wall_mm,
            self.depth_mm - 2.0 * self.wall_mm,
            self.height_mm - self.wall_mm,
        )
    }

    /// The `(u, v)` extents of a face: half-width and half-height of its local
    /// frame, before corner radii are taken into account.
    pub fn face_extents(&self, face: Face) -> (f64, f64) {
        match face {
            Face::Top | Face::Bottom => (self.width_mm / 2.0, self.depth_mm / 2.0),
            Face::Front | Face::Back => (self.width_mm / 2.0, self.height_mm / 2.0),
            Face::Left | Face::Right => (self.depth_mm / 2.0, self.height_mm / 2.0),
        }
    }

    /// World-space centre of a hole on the **outer** surface of its face.
    pub fn hole_point(&self, hole: &Hole) -> [f64; 3] {
        let (w, d, h) = (self.width_mm, self.depth_mm, self.height_mm);
        let (u, v) = (hole.u_mm, hole.v_mm);
        match hole.face {
            Face::Top => [w / 2.0 + u, d / 2.0 + v, h],
            Face::Bottom => [w / 2.0 + u, d / 2.0 + v, 0.0],
            Face::Front => [w / 2.0 + u, 0.0, h / 2.0 + v],
            Face::Back => [w / 2.0 + u, d, h / 2.0 + v],
            Face::Left => [0.0, d / 2.0 + u, h / 2.0 + v],
            Face::Right => [w, d / 2.0 + u, h / 2.0 + v],
        }
    }

    /// Holes on one face, in the order they were declared.
    pub fn holes_on(&self, face: Face) -> impl Iterator<Item = &Hole> {
        self.holes.iter().filter(move |h| h.face == face)
    }

    /// The faces that actually carry holes, in a stable drilling order (the
    /// order [`crate::cam`] turns into setups).
    pub fn drilled_faces(&self) -> Vec<Face> {
        [Face::Top, Face::Back, Face::Left, Face::Right, Face::Front]
            .into_iter()
            .filter(|f| self.holes_on(*f).next().is_some())
            .collect()
    }
}

// ---------------------------------------------------------------------------
//  TOML file format
// ---------------------------------------------------------------------------

/// An enclosure spec as written on disk.
///
/// ```toml
/// name = "fuzz-v1"
/// size = "125B"
/// # any of width_mm / depth_mm / height_mm / wall_mm / corner_radius_mm
/// # override the catalogue value — measure your actual box before machining.
///
/// [[holes]]
/// face = "top"
/// u_mm = -16.0
/// v_mm = 34.5
/// kind = "pot"
/// label = "GAIN"
/// ```
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnclosureFile {
    /// Project name; becomes the STEP part name.
    #[serde(default = "default_name")]
    pub name: String,
    /// Standard size name, or `"custom"` with explicit dimensions.
    pub size: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corner_radius_mm: Option<f64>,
    #[serde(default)]
    pub holes: Vec<Hole>,
}

fn default_name() -> String {
    "enclosure".to_string()
}

impl EnclosureFile {
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Resolve to a concrete [`Enclosure`], applying any dimension overrides.
    ///
    /// `size = "custom"` requires width/depth/height to be given explicitly.
    pub fn to_enclosure(&self) -> Result<Enclosure, String> {
        let base = if self.size.eq_ignore_ascii_case("custom") {
            EnclosureSize {
                name: "custom",
                width_mm: self.width_mm.ok_or("custom size requires `width_mm`")?,
                depth_mm: self.depth_mm.ok_or("custom size requires `depth_mm`")?,
                height_mm: self.height_mm.ok_or("custom size requires `height_mm`")?,
                wall_mm: DIECAST_WALL_MM,
                corner_radius_mm: DIECAST_CORNER_MM,
            }
        } else {
            standard_size(&self.size).ok_or_else(|| {
                let known: Vec<&str> = STANDARD_SIZES.iter().map(|s| s.name).collect();
                format!(
                    "unknown enclosure size: {} (known: {}, or \"custom\")",
                    self.size,
                    known.join(", ")
                )
            })?
        };
        Ok(Enclosure {
            name: self.name.clone(),
            width_mm: self.width_mm.unwrap_or(base.width_mm),
            depth_mm: self.depth_mm.unwrap_or(base.depth_mm),
            height_mm: self.height_mm.unwrap_or(base.height_mm),
            wall_mm: self.wall_mm.unwrap_or(base.wall_mm),
            corner_radius_mm: self.corner_radius_mm.unwrap_or(base.corner_radius_mm),
            holes: self.holes.clone(),
        })
    }

    /// The editable spec for an already-resolved enclosure.
    pub fn from_enclosure(enc: &Enclosure, size: &str) -> Self {
        EnclosureFile {
            name: enc.name.clone(),
            size: size.to_string(),
            width_mm: None,
            depth_mm: None,
            height_mm: None,
            wall_mm: None,
            corner_radius_mm: None,
            holes: enc.holes.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
//  Derivation from a circuit
// ---------------------------------------------------------------------------

/// House layout rules for a derived pedal (the §7.9 "designed once, applied
/// consistently" principle, pedal edition). All mm.
mod rules {
    /// Footswitch centre, measured forward from the front edge of the top face.
    pub const FOOTSWITCH_FROM_FRONT: f64 = 20.0;
    /// Indicator LED, directly above the footswitch.
    pub const LED_FROM_FRONT: f64 = 40.0;
    /// Toggle-switch row.
    pub const SWITCH_FROM_FRONT: f64 = 60.0;
    /// First pot row, measured back from the rear edge of the top face.
    pub const POT_FROM_BACK: f64 = 24.0;
    /// Row-to-row and knob-to-knob spacing for 16 mm pots with finger room.
    pub const POT_PITCH: f64 = 26.0;
    /// Toggle spacing within the switch row.
    pub const SWITCH_PITCH: f64 = 20.0;
    /// Rearmost jack centre, measured forward from the rear edge of a side face.
    pub const JACK_FROM_BACK: f64 = 22.0;
    /// Spacing between jacks sharing a face.
    pub const JACK_PITCH: f64 = 24.0;
    /// Keep-out from a face edge (also the minimum web between two holes).
    pub const EDGE_MARGIN: f64 = 4.0;
}

/// Knobs to which face each derived control goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeriveOptions {
    /// Add a 3PDT true-bypass footswitch even when no such part is in the
    /// circuit. Almost every pedal has one and most schematics leave the bypass
    /// wiring out, so this defaults on.
    pub footswitch: bool,
    /// Add a status LED on the same basis.
    pub led: bool,
}

impl Default for DeriveOptions {
    fn default() -> Self {
        DeriveOptions {
            footswitch: true,
            led: true,
        }
    }
}

/// Classify a circuit part into pedal hardware.
///
/// Pedal-specific hardware (3PDT stomp switches, barrel power jacks) is matched
/// first by keyword, because the [`CutoutSource`] catalogue is shared with
/// Eurorack and would call both of them plain switches and jacks. Everything
/// else falls through to the seam, so a parts library carrying verified
/// mechanical data drives this without a change here.
fn classify(part: &Part, cutouts: &dyn CutoutSource) -> Option<FeatureKind> {
    let hay = format!(
        "{} {}",
        part.footprint.as_deref().unwrap_or(""),
        part.mpn.as_deref().unwrap_or("")
    )
    .to_ascii_lowercase();
    if ["3pdt", "footswitch", "foot_switch", "stomp"]
        .iter()
        .any(|k| hay.contains(k))
    {
        return Some(FeatureKind::Footswitch);
    }
    if [
        "dc_jack",
        "dcjack",
        "barrel",
        "pj-102",
        "pj102",
        "power_jack",
    ]
    .iter()
    .any(|k| hay.contains(k))
    {
        return Some(FeatureKind::DcJack);
    }
    cutouts
        .cutout(part.mpn.as_deref(), part.footprint.as_deref().unwrap_or(""))
        .map(|spec| FeatureKind::from(spec.kind))
}

/// Whether every net a part touches is a power rail — the signal that a jack
/// classified generically is really the DC input.
fn is_power_only(circuit: &dyn CircuitSource, refdes: &str) -> bool {
    let mut touched = 0usize;
    let mut power = 0usize;
    for net in circuit.nets() {
        if net.pins.iter().any(|p| p.refdes.0 == refdes) {
            touched += 1;
            if crate::panel::is_power_net(&net.name) {
                power += 1;
            }
        }
    }
    touched > 0 && touched == power
}

/// Derive an editable enclosure spec from a circuit.
///
/// Controls (pots, toggles, LED, footswitch) go on the **top**, audio jacks on
/// the **sides** — input right, output left, matching signal flow across a
/// pedalboard — and power on the **back**. Positions come from the house rules
/// above; like [`crate::panel::derive_panel`] this is a starting point to edit,
/// not a straitjacket. Run [`check_enclosure`] on the result before machining.
pub fn derive_enclosure(
    circuit: &dyn CircuitSource,
    size: EnclosureSize,
    cutouts: &dyn CutoutSource,
    opts: DeriveOptions,
) -> Enclosure {
    let mut enc = Enclosure::standard(circuit.name(), size);

    // Bucket the panel-facing parts by hardware kind.
    let mut pots = Vec::new();
    let mut switches = Vec::new();
    let mut leds = Vec::new();
    let mut jacks = Vec::new();
    let mut dc = Vec::new();
    let mut footswitches = Vec::new();
    for part in circuit.parts() {
        let Some(mut kind) = classify(part, cutouts) else {
            continue;
        };
        if kind == FeatureKind::Jack && is_power_only(circuit, &part.refdes.0) {
            kind = FeatureKind::DcJack;
        }
        let entry = (
            part.refdes.0.clone(),
            control_label(circuit, &part.refdes.0),
        );
        match kind {
            FeatureKind::Pot => pots.push(entry),
            FeatureKind::Switch => switches.push(entry),
            FeatureKind::Led => leds.push(entry),
            FeatureKind::Jack => jacks.push(entry),
            FeatureKind::DcJack => dc.push(entry),
            FeatureKind::Footswitch => footswitches.push(entry),
            FeatureKind::Mounting => {}
        }
    }
    for v in [
        &mut pots,
        &mut switches,
        &mut leds,
        &mut jacks,
        &mut dc,
        &mut footswitches,
    ] {
        v.sort();
    }

    let (hw, hd) = (enc.width_mm / 2.0, enc.depth_mm / 2.0);

    // --- top face: footswitch at the front, LED above it, toggles, then pots ---
    if footswitches.is_empty() && opts.footswitch {
        footswitches.push(("SW_BYPASS".to_string(), None));
    }
    for (i, (refdes, _)) in footswitches.iter().enumerate() {
        let u = spread(i, footswitches.len(), rules::POT_PITCH + 8.0);
        enc.holes.push(
            Hole::new(
                Face::Top,
                u,
                -hd + rules::FOOTSWITCH_FROM_FRONT,
                FeatureKind::Footswitch,
            )
            .with_refdes(refdes.clone()),
        );
    }
    if leds.is_empty() && opts.led {
        leds.push(("D_LED".to_string(), None));
    }
    for (i, (refdes, label)) in leds.iter().enumerate() {
        let u = spread(i, leds.len(), rules::SWITCH_PITCH);
        enc.holes.push(
            Hole::new(Face::Top, u, -hd + rules::LED_FROM_FRONT, FeatureKind::Led)
                .with_refdes(refdes.clone())
                .with_label(label.clone()),
        );
    }
    for (i, (refdes, label)) in switches.iter().enumerate() {
        let u = spread(i, switches.len(), rules::SWITCH_PITCH);
        enc.holes.push(
            Hole::new(
                Face::Top,
                u,
                -hd + rules::SWITCH_FROM_FRONT,
                FeatureKind::Switch,
            )
            .with_refdes(refdes.clone())
            .with_label(label.clone()),
        );
    }
    // Pots fill rows back-to-front, as many per row as the width allows.
    let usable = enc.width_mm - 2.0 * (rules::EDGE_MARGIN + FeatureKind::Pot.body_mm() / 2.0);
    let per_row = ((usable / rules::POT_PITCH).floor() as usize + 1).max(1);
    for (i, (refdes, label)) in pots.iter().enumerate() {
        let (row, col) = (i / per_row, i % per_row);
        let in_row = per_row.min(pots.len() - row * per_row);
        let u = spread(col, in_row, rules::POT_PITCH);
        let v = hd - rules::POT_FROM_BACK - row as f64 * rules::POT_PITCH;
        enc.holes.push(
            Hole::new(Face::Top, u, v, FeatureKind::Pot)
                .with_refdes(refdes.clone())
                .with_label(label.clone()),
        );
    }

    // --- sides: input right, output left (signal flows right to left) ---------
    let side_v = |enc: &Enclosure, kind: FeatureKind| -> f64 {
        // Centre the hole in the wall's height, then pull it clear of the top
        // plate and the open bottom if the box is short.
        let r = kind.hole_mm() / 2.0;
        let high = enc.height_mm / 2.0 - enc.wall_mm - r - rules::EDGE_MARGIN;
        let low = -enc.height_mm / 2.0 + r + rules::EDGE_MARGIN;
        0.0f64.clamp(low.min(high), high.max(low))
    };
    let mut right = 0usize;
    let mut left = 0usize;
    for (refdes, label) in &jacks {
        let is_input = label
            .as_deref()
            .map(|l| l.contains("IN"))
            .unwrap_or(right <= left);
        let (face, n) = if is_input {
            (Face::Right, &mut right)
        } else {
            (Face::Left, &mut left)
        };
        let u = hd - rules::JACK_FROM_BACK - *n as f64 * rules::JACK_PITCH;
        *n += 1;
        let v = side_v(&enc, FeatureKind::Jack);
        enc.holes.push(
            Hole::new(face, u, v, FeatureKind::Jack)
                .with_refdes(refdes.clone())
                .with_label(label.clone()),
        );
    }

    // --- back: power ----------------------------------------------------------
    for (i, (refdes, _)) in dc.iter().enumerate() {
        let u = spread(i, dc.len(), rules::JACK_PITCH).clamp(-hw + 12.0, hw - 12.0);
        let v = side_v(&enc, FeatureKind::DcJack);
        enc.holes.push(
            Hole::new(Face::Back, u, v, FeatureKind::DcJack)
                .with_refdes(refdes.clone())
                .with_label(Some("9V".to_string())),
        );
    }

    enc
}

/// Position `i` of `n` items centred on zero at the given pitch.
fn spread(i: usize, n: usize, pitch: f64) -> f64 {
    (i as f64 - (n as f64 - 1.0) / 2.0) * pitch
}

/// A short legend for a control, from the most signal-like net it touches —
/// same rule the Eurorack panel derivation uses.
fn control_label(circuit: &dyn CircuitSource, refdes: &str) -> Option<String> {
    let mut sig: Vec<&str> = circuit
        .nets()
        .iter()
        .filter(|n| n.pins.iter().any(|p| p.refdes.0 == refdes))
        .map(|n| n.name.as_str())
        .filter(|n| !crate::panel::is_power_net(n))
        .collect();
    sig.sort();
    sig.first().map(|n| crate::panel::label_from_net(n))
}

// ---------------------------------------------------------------------------
//  Mechanical checks (DESIGN §6.7)
// ---------------------------------------------------------------------------

/// Signed distance (mm) from `(u, v)` to the nearest edge of a rounded
/// rectangle of half-extents `(hu, hv)` and corner radius `r`. Positive inside.
fn dist_to_edge(u: f64, v: f64, hu: f64, hv: f64, r: f64) -> f64 {
    let r = r.min(hu).min(hv).max(0.0);
    let (au, av) = (u.abs(), v.abs());
    let (cu, cv) = (hu - r, hv - r);
    if au <= cu || av <= cv {
        (hu - au).min(hv - av)
    } else {
        // In a corner quadrant: distance to the fillet arc.
        r - ((au - cu).hypot(av - cv))
    }
}

/// Check an enclosure for the mechanical problems that make a spec unbuildable
/// — a hole running off its face or into a corner radius, holes whose hardware
/// collides, or a bore that has no wall behind it.
///
/// Errors here are the same errors the STEP export would turn into invalid
/// geometry, so this runs first and the export skips anything it rejects.
pub fn check_enclosure(enc: &Enclosure) -> StageOutcome {
    let mut out = StageOutcome::passed("enclosure");
    let (cw, cd, ch) = enc.cavity_mm();
    out = out.with(Finding::info(format!(
        "{}: {:.1} × {:.1} × {:.1} mm outer, cavity {:.1} × {:.1} × {:.1} mm, \
         {:.1} mm wall, {} hole(s)",
        enc.name,
        enc.width_mm,
        enc.depth_mm,
        enc.height_mm,
        cw,
        cd,
        ch,
        enc.wall_mm,
        enc.holes.len(),
    )));

    for hole in &enc.holes {
        let id = hole
            .refdes
            .clone()
            .unwrap_or_else(|| format!("{} hole", hole.kind.as_str()));
        let r = hole.radius_mm();
        if hole.diameter_mm <= 0.0 {
            out = out.with(Finding::error(format!(
                "{id}: diameter must be positive (got {:.2} mm)",
                hole.diameter_mm
            )));
            continue;
        }
        if hole.face == Face::Bottom {
            out = out.with(Finding::error(format!(
                "{id}: the bottom face is the removable lid, which is a separate \
                 part and is not modelled — put the hole on another face or drill \
                 the lid from the same hole table"
            )));
            continue;
        }
        // A hole has to sit wholly inside the flat part of its face. On the side
        // walls the corner radius eats into the flat run at each end; on the top
        // it rounds all four corners.
        let (hu, hv) = enc.face_extents(hole.face);
        let clear = if hole.face == Face::Top {
            dist_to_edge(hole.u_mm, hole.v_mm, hu, hv, enc.corner_radius_mm)
        } else {
            // Side faces are flat rectangles once the corner radii are trimmed
            // off the horizontal run; the vertical run is unaffected.
            (hu - enc.corner_radius_mm - hole.u_mm.abs()).min(hv - hole.v_mm.abs())
        };
        if clear < r + rules::EDGE_MARGIN {
            out = out.with(Finding::error(format!(
                "{id} on {}: only {:.1} mm from the edge of the flat face, needs \
                 {:.1} mm for a Ø{:.1} hole plus a {:.0} mm margin",
                hole.face.as_str(),
                clear,
                r + rules::EDGE_MARGIN,
                hole.diameter_mm,
                rules::EDGE_MARGIN,
            )));
        }
        // A side hole must have cavity wall behind it for its whole depth, i.e.
        // it must not break into the top plate.
        if hole.face.is_side() {
            let z = enc.height_mm / 2.0 + hole.v_mm;
            if z + r > enc.height_mm - enc.wall_mm {
                out = out.with(Finding::error(format!(
                    "{id} on {}: breaks into the top plate (hole reaches z={:.1} mm, \
                     ceiling is at {:.1} mm) — lower it",
                    hole.face.as_str(),
                    z + r,
                    enc.height_mm - enc.wall_mm,
                )));
            }
            if z - r < 0.0 {
                out = out.with(Finding::error(format!(
                    "{id} on {}: runs off the open bottom edge — raise it",
                    hole.face.as_str()
                )));
            }
        }
    }

    // Pairwise collisions, per face. Holes must not merge, and the hardware
    // mounted in them must not fight for the same space.
    for a in 0..enc.holes.len() {
        for b in (a + 1)..enc.holes.len() {
            let (x, y) = (&enc.holes[a], &enc.holes[b]);
            if x.face != y.face {
                continue;
            }
            let d = (x.u_mm - y.u_mm).hypot(x.v_mm - y.v_mm);
            let name = |h: &Hole| {
                h.refdes
                    .clone()
                    .unwrap_or_else(|| h.kind.as_str().to_string())
            };
            let bore = x.radius_mm() + y.radius_mm() + rules::EDGE_MARGIN;
            let body = (x.kind.body_mm() + y.kind.body_mm()) / 2.0;
            if d < bore {
                out = out.with(Finding::error(format!(
                    "{} and {} on {}: {:.1} mm apart, holes need {:.1} mm to leave \
                     a {:.0} mm web",
                    name(x),
                    name(y),
                    x.face.as_str(),
                    d,
                    bore,
                    rules::EDGE_MARGIN,
                )));
            } else if d < body {
                out = out.with(Finding::warning(format!(
                    "{} and {} on {}: {:.1} mm apart — the hardware bodies want \
                     {:.1} mm ({} + {})",
                    name(x),
                    name(y),
                    x.face.as_str(),
                    d,
                    body,
                    x.kind.as_str(),
                    y.kind.as_str(),
                )));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
//  3D export
// ---------------------------------------------------------------------------

/// Build the enclosure's solid: an open-bottom shell with every hole bored
/// through the wall it sits on.
///
/// Holes that [`check_enclosure`] rejects are skipped rather than emitted as
/// broken geometry; run the check first and act on its findings.
pub fn enclosure_brep(enc: &Enclosure) -> Brep {
    let (w, d, h, t) = (enc.width_mm, enc.depth_mm, enc.height_mm, enc.wall_mm);
    let r = enc.corner_radius_mm;
    let outer = Profile::rounded_rect(0.0, 0.0, w, d, r);
    let inner = Profile::rounded_rect(t, t, w - t, d - t, (r - t).max(0.0));

    let mut brep = Brep::default();
    let outer_prism = brep.add_prism(&outer, 0.0, h, false);
    let inner_prism = brep.add_prism(&inner, 0.0, h - t, true);

    // Top plate (outside), cavity ceiling (inside), and the rim the lid bolts to.
    let top_face = brep.add_face(BFace {
        surface: Surface::Plane {
            origin: [0.0, 0.0, h],
            axis: [0.0, 0.0, 1.0],
            ref_dir: [1.0, 0.0, 0.0],
        },
        bounds: vec![Brep::ring_loop(&outer_prism.top, false)],
        same_sense: true,
    });
    let ceiling_face = brep.add_face(BFace {
        surface: Surface::Plane {
            origin: [0.0, 0.0, h - t],
            axis: [0.0, 0.0, -1.0],
            ref_dir: [1.0, 0.0, 0.0],
        },
        bounds: vec![Brep::ring_loop(&inner_prism.top, true)],
        same_sense: true,
    });
    let _rim = brep.add_face(BFace {
        surface: Surface::Plane {
            origin: [0.0, 0.0, 0.0],
            axis: [0.0, 0.0, -1.0],
            ref_dir: [1.0, 0.0, 0.0],
        },
        bounds: vec![
            Brep::ring_loop(&outer_prism.bottom, true),
            Brep::ring_loop(&inner_prism.bottom, false),
        ],
        same_sense: true,
    });

    let outer_sides = outer.side_segments();
    let inner_sides = inner.side_segments();
    let rejected = check_enclosure(enc);
    let blocked: Vec<&str> = rejected
        .findings
        .iter()
        .filter(|f| f.severity == crate::stage::Severity::Error)
        .map(|f| f.message.as_str())
        .collect();

    for hole in &enc.holes {
        // Anything the checker called an error would produce a rim that runs off
        // its face — invalid B-rep, not merely wrong. Skip it.
        let id = hole.refdes.as_deref().unwrap_or(hole.kind.as_str());
        if blocked.iter().any(|m| m.starts_with(id)) {
            continue;
        }
        let (outer_face, inner_face) = match hole.face {
            Face::Top => (top_face, ceiling_face),
            Face::Bottom => continue,
            side => {
                let Some(i) = side.side_index() else { continue };
                (
                    outer_prism.lateral[outer_sides[i]],
                    inner_prism.lateral[inner_sides[i]],
                )
            }
        };
        brep.add_through_hole(
            outer_face,
            inner_face,
            enc.hole_point(hole),
            hole.face.normal(),
            t,
            hole.radius_mm(),
        );
    }
    brep
}

/// Export the enclosure as a STEP AP214 solid.
pub fn enclosure_to_step(enc: &Enclosure) -> String {
    enclosure_brep(enc).to_step(&enc.name)
}

/// A 2D drill template for one face: the face outline plus every hole, as DXF —
/// print it 1:1, spray-glue it to the box, centre-punch through it. This is
/// DESIGN §7.7's drilling aid, reusing the same hole table the solid does so the
/// two can't drift apart.
pub fn enclosure_face_dxf(enc: &Enclosure, face: Face) -> String {
    let (hu, hv) = enc.face_extents(face);
    let mut s = String::with_capacity(2048);
    s.push_str("0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1032\n0\nENDSEC\n");
    s.push_str("0\nSECTION\n2\nENTITIES\n");
    // Outline, in the face's own (u, v) frame with the centre at the origin.
    let pts = [(-hu, -hv), (hu, -hv), (hu, hv), (-hu, hv)];
    s.push_str("0\nLWPOLYLINE\n8\n0\n100\nAcDbEntity\n100\nAcDbPolyline\n90\n4\n70\n1\n43\n0.0\n");
    for (x, y) in pts {
        s.push_str(&format!("10\n{x}\n20\n{y}\n"));
    }
    for hole in enc.holes_on(face) {
        s.push_str(&format!(
            "0\nCIRCLE\n8\n0\n10\n{}\n20\n{}\n40\n{}\n",
            hole.u_mm,
            hole.v_mm,
            hole.radius_mm()
        ));
        // Cross-hairs give the centre punch something to aim at.
        let c = hole.radius_mm() + 2.0;
        for (x0, y0, x1, y1) in [
            (hole.u_mm - c, hole.v_mm, hole.u_mm + c, hole.v_mm),
            (hole.u_mm, hole.v_mm - c, hole.u_mm, hole.v_mm + c),
        ] {
            s.push_str(&format!(
                "0\nLINE\n8\n0\n10\n{x0}\n20\n{y0}\n11\n{x1}\n21\n{y1}\n"
            ));
        }
    }
    s.push_str("0\nENDSEC\n0\nEOF\n");
    s
}

// ---------------------------------------------------------------------------
//  Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef};
    use crate::panel::BuiltinCutouts;
    use crate::stage::Severity;

    fn demo_125b() -> Enclosure {
        Enclosure::standard("demo", standard_size("125B").unwrap())
            .with_hole(Hole::new(Face::Top, -16.0, 34.0, FeatureKind::Pot))
            .with_hole(Hole::new(Face::Top, 16.0, 34.0, FeatureKind::Pot))
            .with_hole(Hole::new(Face::Top, 0.0, -40.5, FeatureKind::Footswitch))
            .with_hole(Hole::new(Face::Right, 38.5, 0.0, FeatureKind::Jack))
            .with_hole(Hole::new(Face::Left, 38.5, 0.0, FeatureKind::Jack))
            .with_hole(Hole::new(Face::Back, 0.0, 0.0, FeatureKind::DcJack))
    }

    #[test]
    fn standard_sizes_resolve_and_alias() {
        let b = standard_size("125b").unwrap();
        assert_eq!(b.name, "125B");
        assert_eq!(b.width_mm, 66.0);
        assert_eq!(b.depth_mm, 121.0);
        // Hammond's own part number for the same box.
        assert_eq!(standard_size("1590N1").unwrap(), b);
        assert!(standard_size("nonesuch").is_none());
    }

    #[test]
    fn hole_points_land_on_the_right_faces() {
        let enc = Enclosure::standard("e", standard_size("125B").unwrap());
        let (w, d, h) = (enc.width_mm, enc.depth_mm, enc.height_mm);
        // Face-centre holes sit at the middle of their face, on its outer surface.
        let top = enc.hole_point(&Hole::new(Face::Top, 0.0, 0.0, FeatureKind::Pot));
        assert_eq!(top, [w / 2.0, d / 2.0, h]);
        let back = enc.hole_point(&Hole::new(Face::Back, 0.0, 0.0, FeatureKind::DcJack));
        assert_eq!(back, [w / 2.0, d, h / 2.0]);
        let left = enc.hole_point(&Hole::new(Face::Left, 0.0, 0.0, FeatureKind::Jack));
        assert_eq!(left, [0.0, d / 2.0, h / 2.0]);
        // u tracks world X on both Front and Back, so the same u is the same place.
        let f = enc.hole_point(&Hole::new(Face::Front, 10.0, 0.0, FeatureKind::Jack));
        let b = enc.hole_point(&Hole::new(Face::Back, 10.0, 0.0, FeatureKind::Jack));
        assert_eq!(f[0], b[0]);
    }

    #[test]
    fn shell_with_holes_on_every_face_is_manifold() {
        let brep = enclosure_brep(&demo_125b());
        let errs = brep.manifold_errors();
        assert!(errs.is_empty(), "{errs:?}");
        // 6 holes → 6 bores, each one cylindrical face on top of the shell's own.
        let cyls = brep
            .faces
            .iter()
            .filter(|f| matches!(f.surface, Surface::Cylinder { .. }))
            .count();
        // 4 outer corners + 4 inner corners + 6 bores.
        assert_eq!(cyls, 14);
    }

    #[test]
    fn square_cornered_enclosure_is_also_manifold() {
        // Wall thicker than the corner radius degenerates the inner profile to
        // square corners while the outer profile stays rounded — the two
        // profiles then have different segment counts, which the side-face
        // lookup has to survive.
        let mut enc = demo_125b();
        enc.corner_radius_mm = 2.0;
        enc.wall_mm = 3.0;
        let brep = enclosure_brep(&enc);
        assert!(brep.manifold_errors().is_empty());
        assert!(enclosure_to_step(&enc).contains("MANIFOLD_SOLID_BREP"));
    }

    #[test]
    fn step_export_names_the_part_and_carries_units() {
        let step = enclosure_to_step(&demo_125b());
        assert!(step.contains("MANIFOLD_SOLID_BREP('demo'"));
        assert!(step.contains("SI_UNIT(.MILLI.,.METRE.)"));
        // Deterministic: the same spec exports byte-identically.
        assert_eq!(step, enclosure_to_step(&demo_125b()));
    }

    #[test]
    fn check_passes_a_sane_layout() {
        let outcome = check_enclosure(&demo_125b());
        assert!(outcome.passed, "{:?}", outcome.findings);
    }

    #[test]
    fn check_catches_a_hole_off_the_edge() {
        let enc = Enclosure::standard("e", standard_size("125B").unwrap()).with_hole(Hole::new(
            Face::Top,
            32.0,
            0.0,
            FeatureKind::Pot,
        ));
        let outcome = check_enclosure(&enc);
        assert!(!outcome.passed);
        assert!(outcome
            .findings
            .iter()
            .any(|f| f.severity == Severity::Error && f.message.contains("from the edge")));
    }

    #[test]
    fn check_catches_a_corner_radius_intrusion() {
        // Dead in the corner of the top face: inside the bounding rectangle by
        // the naive measure, but outside the rounded outline.
        let enc = Enclosure::standard("e", standard_size("125B").unwrap()).with_hole(Hole::new(
            Face::Top,
            29.0,
            56.0,
            FeatureKind::Mounting,
        ));
        assert!(!check_enclosure(&enc).passed);
    }

    #[test]
    fn check_catches_a_side_hole_breaking_into_the_top_plate() {
        let mut enc = Enclosure::standard("e", standard_size("125B").unwrap());
        enc.holes
            .push(Hole::new(Face::Right, 0.0, 16.0, FeatureKind::Jack));
        let outcome = check_enclosure(&enc);
        assert!(!outcome.passed);
        assert!(outcome
            .findings
            .iter()
            .any(|f| f.message.contains("top plate")));
    }

    #[test]
    fn check_catches_colliding_holes_and_crowded_knobs() {
        // 8 mm apart: the bores nearly merge.
        let enc = Enclosure::standard("e", standard_size("125B").unwrap())
            .with_hole(Hole::new(Face::Top, -4.0, 0.0, FeatureKind::Pot))
            .with_hole(Hole::new(Face::Top, 4.0, 0.0, FeatureKind::Pot));
        assert!(!check_enclosure(&enc).passed);
        // 15 mm apart: drillable, but the knobs rub.
        let enc = Enclosure::standard("e", standard_size("125B").unwrap())
            .with_hole(Hole::new(Face::Top, -7.5, 0.0, FeatureKind::Pot))
            .with_hole(Hole::new(Face::Top, 7.5, 0.0, FeatureKind::Pot));
        let outcome = check_enclosure(&enc);
        assert!(outcome.passed);
        assert!(outcome
            .findings
            .iter()
            .any(|f| f.severity == Severity::Warning && f.message.contains("hardware bodies")));
    }

    #[test]
    fn bottom_face_holes_are_rejected_and_skipped() {
        let enc = Enclosure::standard("e", standard_size("125B").unwrap()).with_hole(Hole::new(
            Face::Bottom,
            0.0,
            0.0,
            FeatureKind::Mounting,
        ));
        assert!(!check_enclosure(&enc).passed);
        // …and the solid still comes out valid rather than broken.
        assert!(enclosure_brep(&enc).manifold_errors().is_empty());
    }

    #[test]
    fn derive_puts_controls_up_top_io_on_the_sides_and_power_at_the_back() {
        let mut circ = Circuit::new("fuzz");
        circ.parts = vec![
            Part::new("RV1", "100k").with_footprint("Potentiometer_THT:Potentiometer_Alpha_RD901F"),
            Part::new("RV2", "1k").with_footprint("Potentiometer_THT:Potentiometer_Alpha_RD901F"),
            Part::new("J1", "in").with_footprint("Connector_Audio:Jack_6.35mm"),
            Part::new("J2", "out").with_footprint("Connector_Audio:Jack_6.35mm"),
            Part::new("J3", "dc").with_footprint("Connector:Barrel_Jack_PJ-102A"),
            Part::new("Q1", "2N3904").with_footprint("Package_TO_SOT_THT:TO-92"),
        ];
        circ.nets = vec![
            Net::new("SIG_IN", vec![PinRef::new("J1", "T")]),
            Net::new("SIG_OUT", vec![PinRef::new("J2", "T")]),
            Net::new("VOLUME", vec![PinRef::new("RV2", "2")]),
            Net::new("FUZZ", vec![PinRef::new("RV1", "2")]),
            Net::new("+9V", vec![PinRef::new("J3", "1")]),
        ];
        let enc = derive_enclosure(
            &circ,
            standard_size("125B").unwrap(),
            &BuiltinCutouts,
            DeriveOptions::default(),
        );

        let by = |r: &str| enc.holes.iter().find(|h| h.refdes.as_deref() == Some(r));
        // Pots on top, at the back.
        assert_eq!(by("RV1").unwrap().face, Face::Top);
        assert!(by("RV1").unwrap().v_mm > 0.0);
        // Input right, output left.
        assert_eq!(by("J1").unwrap().face, Face::Right);
        assert_eq!(by("J2").unwrap().face, Face::Left);
        // Power on the back, as a DC jack rather than the generic jack the
        // shared Eurorack catalogue would have called it.
        assert_eq!(by("J3").unwrap().face, Face::Back);
        assert_eq!(by("J3").unwrap().kind, FeatureKind::DcJack);
        // The transistor is board-only.
        assert!(by("Q1").is_none());
        // A bypass footswitch and status LED are added even though the netlist
        // has neither.
        assert_eq!(by("SW_BYPASS").unwrap().kind, FeatureKind::Footswitch);
        assert!(by("D_LED").is_some());
        // The derived layout is buildable and exports cleanly.
        let outcome = check_enclosure(&enc);
        assert!(outcome.passed, "{:?}", outcome.findings);
        assert!(enclosure_brep(&enc).manifold_errors().is_empty());
    }

    #[test]
    fn derive_respects_the_no_extras_options() {
        let mut circ = Circuit::new("clean");
        circ.parts =
            vec![Part::new("RV1", "10k")
                .with_footprint("Potentiometer_THT:Potentiometer_Alpha_RD901F")];
        let enc = derive_enclosure(
            &circ,
            standard_size("1590B").unwrap(),
            &BuiltinCutouts,
            DeriveOptions {
                footswitch: false,
                led: false,
            },
        );
        assert_eq!(enc.holes.len(), 1);
        assert_eq!(enc.holes[0].kind, FeatureKind::Pot);
    }

    #[test]
    fn derive_wraps_many_pots_onto_extra_rows() {
        let mut circ = Circuit::new("many");
        circ.parts = (1..=6)
            .map(|i| {
                Part::new(format!("RV{i}"), "10k")
                    .with_footprint("Potentiometer_THT:Potentiometer_Alpha_RD901F")
            })
            .collect();
        let enc = derive_enclosure(
            &circ,
            standard_size("125B").unwrap(),
            &BuiltinCutouts,
            DeriveOptions {
                footswitch: false,
                led: false,
            },
        );
        let rows: std::collections::BTreeSet<i64> = enc
            .holes
            .iter()
            .map(|h| (h.v_mm * 10.0).round() as i64)
            .collect();
        // 66 mm wide fits two 26 mm-pitch knobs per row, so six pots need three.
        assert_eq!(rows.len(), 3);
        assert!(check_enclosure(&enc).passed);
    }

    #[test]
    fn toml_round_trips_through_a_resolved_enclosure() {
        let toml = r#"
name = "fuzz-v1"
size = "125B"

[[holes]]
face = "top"
u_mm = -16.0
v_mm = 34.0
diameter_mm = 7.0
kind = "pot"
label = "GAIN"

[[holes]]
face = "back"
u_mm = 0.0
v_mm = 0.0
diameter_mm = 12.0
kind = "dc_jack"
"#;
        let file = EnclosureFile::from_toml(toml).unwrap();
        let enc = file.to_enclosure().unwrap();
        assert_eq!(enc.name, "fuzz-v1");
        assert_eq!(enc.width_mm, 66.0);
        assert_eq!(enc.holes.len(), 2);
        assert_eq!(enc.holes[0].label.as_deref(), Some("GAIN"));
        assert_eq!(enc.holes[1].face, Face::Back);
        // And back out to an editable spec.
        let round = EnclosureFile::from_enclosure(&enc, "125B");
        let reparsed = EnclosureFile::from_toml(&round.to_toml().unwrap()).unwrap();
        assert_eq!(reparsed.holes, file.holes);
    }

    #[test]
    fn custom_size_requires_dimensions_and_overrides_apply() {
        let file = EnclosureFile::from_toml("size = \"custom\"").unwrap();
        assert!(file.to_enclosure().is_err());
        let file =
            EnclosureFile::from_toml("size = \"125B\"\nheight_mm = 50.0\nwall_mm = 3.0\n").unwrap();
        let enc = file.to_enclosure().unwrap();
        assert_eq!(enc.height_mm, 50.0);
        assert_eq!(enc.wall_mm, 3.0);
        assert_eq!(enc.width_mm, 66.0, "unset fields keep the catalogue value");
    }

    #[test]
    fn unknown_size_names_the_known_ones() {
        let file = EnclosureFile::from_toml("size = \"1590Z\"").unwrap();
        let err = file.to_enclosure().unwrap_err();
        assert!(err.contains("125B"), "{err}");
    }

    #[test]
    fn face_dxf_draws_the_outline_and_every_hole() {
        let enc = demo_125b();
        let dxf = enclosure_face_dxf(&enc, Face::Top);
        assert!(dxf.contains("LWPOLYLINE"));
        assert_eq!(dxf.matches("CIRCLE").count(), 3, "2 pots + footswitch");
        assert!(dxf.trim_end().ends_with("EOF"));
    }

    #[test]
    fn drilled_faces_are_reported_in_setup_order() {
        assert_eq!(
            demo_125b().drilled_faces(),
            vec![Face::Top, Face::Back, Face::Left, Face::Right]
        );
    }
}
