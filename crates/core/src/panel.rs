//! Panel specification, DXF export, and order tracking.
//!
//! DESIGN.md §6.9, §7.1, §7.5.
//!
//! The [`PanelSpec`] trait is the format-agnostic seam: dimensions in mm,
//! mounting holes, and anchored cutouts. The only v1 implementation is
//! Eurorack; pedal/rack/500-series are deliberately unimplemented.
//!
//! DXF export consumes any `&dyn PanelSpec` — it does not know about HP, U,
//! or enclosure size classes.

use std::path::PathBuf;
use std::process::Command;

use crate::logo::Logo;
use crate::parts::PartsError;
use crate::source::CircuitSource;
use crate::tools::find_on_path;

// ---------------------------------------------------------------------------
//  PanelSpec trait + geometry types
// ---------------------------------------------------------------------------

/// A mounting hole on a panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MountingHole {
    pub x_mm: f64,
    pub y_mm: f64,
    pub diameter_mm: f64,
}

/// An anchored cutout (jack, pot, switch, LED, etc.).
#[derive(Debug, Clone, PartialEq)]
pub struct Cutout {
    pub x_mm: f64,
    pub y_mm: f64,
    pub rotation_deg: f64,
    /// Footprint name, e.g. `"Thonkiconn"`, `"Alpha9mm"`, `"LED_3mm"`.
    pub footprint: String,
    /// The board part this cutout is for (e.g. `"J1"`). When set, the board
    /// placer anchors that part at this position so the board mates the panel.
    pub refdes: Option<String>,
    /// A silkscreen/engraving label for this control (e.g. `"IN"`, `"OUT"`,
    /// `"RATE"`), rendered next to the cutout. `None` omits it.
    pub label: Option<String>,
}

/// The shape of a cutout, derived from its footprint name.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CutoutShape {
    /// A round hole (mounting holes, pots, LEDs).
    Circle { diameter_mm: f64 },
    /// A rectangular cutout (jacks, some switches).
    RoundedRect {
        width_mm: f64,
        height_mm: f64,
        corner_radius_mm: f64,
    },
}

/// Thonkiconn / PJ301M panel barrel-hole diameter (mm): the threaded barrel
/// passes through and a nut tightens on the front, so the cutout is this hole,
/// not the jack body.
const JACK_BARREL_MM: f64 = 6.0;
/// Alpha 9 mm pot bushing hole diameter (mm).
const POT_BUSHING_MM: f64 = 7.0;
/// Toggle switch bushing hole diameter (mm).
const TOGGLE_MM: f64 = 6.5;
const LED_5MM_MM: f64 = 5.0;
const LED_3MM_MM: f64 = 3.0;

/// Mechanical envelopes `(width, height)` mm — the space each control really
/// occupies on a built panel, versus the much smaller hole it pokes through. Each
/// is the larger of the panel-side hardware and the PCB body that anchors to it,
/// measured from the parts we actually build with. See [`CutoutSpec::envelope_mm`].
mod envelope {
    /// Thonkiconn / PJ301M: the KiCad body is 10.0 × 14.4 mm — larger than the
    /// ~7.6 mm nut, and larger than the ~12 mm spacing dense modules use to leave
    /// finger room for a plug, so the body governs.
    pub const JACK: (f64, f64) = (10.0, 14.4);
    /// Alpha 9 mm vertical pot: the PCB body spans 13.75 × 12.82 mm including its
    /// solder lugs; a common small Eurorack knob (Davies 1900h ≈ 13.8 mm, Rogan
    /// 1PS ≈ 12.7 mm) is about the same, so 14 mm covers both.
    pub const POT: (f64, f64) = (14.0, 14.0);
    /// Sub-mini toggle: bushing plus the lever's throw and finger room.
    pub const SWITCH: (f64, f64) = (10.0, 12.0);
    /// An LED needs only its bezel plus a little material.
    pub const LED: (f64, f64) = (6.0, 6.0);
    /// Minimum panel material left between a control envelope and the panel edge —
    /// a knob may not overhang, or it fouls the neighbouring module.
    pub const EDGE_MM: f64 = 1.0;
    /// Minimum gap between two adjacent control envelopes.
    pub const GAP_MM: f64 = 2.0;
}

/// The default mechanical envelope for a control kind.
fn kind_envelope(kind: ControlKind) -> (f64, f64) {
    match kind {
        ControlKind::Jack => envelope::JACK,
        ControlKind::Pot => envelope::POT,
        ControlKind::Switch => envelope::SWITCH,
        ControlKind::Led => envelope::LED,
    }
}

/// What kind of front-panel control a part is — drives panel-layout grouping
/// (knobs/switches up top, jacks at the bottom) and label defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    Pot,
    Switch,
    Led,
    Jack,
}

impl ControlKind {
    /// The canonical cutout-footprint name a derived panel records for this kind
    /// (round-trips through [`footprint_shape`] on render).
    pub fn cutout_name(self) -> &'static str {
        match self {
            ControlKind::Pot => "Alpha9mm",
            ControlKind::Switch => "Toggle",
            ControlKind::Led => "LED_5mm",
            ControlKind::Jack => "Thonkiconn",
        }
    }
    fn is_jack(self) -> bool {
        matches!(self, ControlKind::Jack)
    }
}

/// A part's panel-mount cutout: opening geometry + control kind + the space the
/// hardware really occupies. This is **part data** — see [`CutoutSource`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CutoutSpec {
    pub shape: CutoutShape,
    pub kind: ControlKind,
    /// The mechanical envelope `(width, height)` in mm this control actually needs
    /// on the panel — **not** its hole. It is the larger of the panel-side hardware
    /// (a knob's skirt, a jack's nut plus room to grip a plug) and the PCB body the
    /// panel anchors, because both must clear their neighbours: knobs must not
    /// collide, and the anchored footprints must not overlap on the board.
    ///
    /// Deriving a panel from hole sizes alone is what produced panels that looked
    /// fine and could not be built — a 3 HP panel is 15.24 mm wide, which cannot
    /// hold a 13.75 mm pot body with any material left at the edges.
    pub envelope_mm: (f64, f64),
}

/// Resolves a part to its panel-mount cutout — **the seam**. A part's mechanical
/// cutout belongs *with the part* (same principle as its SPICE model riding with
/// the component, not special-cased in the generator), so the real source is the
/// parts library's verified mechanical data. Until that library carries it,
/// [`BuiltinCutouts`] backs this with a table of common Eurorack controls; a
/// library-backed `CutoutSource` then swaps in with no change to panel/derivation
/// code.
pub trait CutoutSource {
    /// The cutout for a part, by MPN (preferred) and/or its KiCad footprint or
    /// cutout name. `None` = board-only (not panel-mounted) or unknown.
    fn cutout(&self, mpn: Option<&str>, footprint: &str) -> Option<CutoutSpec>;
}

/// Fallback catalogue of common Eurorack controls, matched by cutout name or by a
/// keyword in a full KiCad footprint. An explicit stand-in for the parts
/// library's verified mechanical data (epic `okm`), not the intended long-term
/// source.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinCutouts;

impl CutoutSource for BuiltinCutouts {
    fn cutout(&self, _mpn: Option<&str>, footprint: &str) -> Option<CutoutSpec> {
        let name = footprint
            .rsplit_once(':')
            .map(|(_, r)| r)
            .unwrap_or(footprint)
            .to_ascii_lowercase();
        let round = |kind, diameter_mm| {
            Some(CutoutSpec {
                kind,
                shape: CutoutShape::Circle { diameter_mm },
                // An LED's envelope is its own bezel; every other kind carries the
                // hardware/body envelope for its class.
                envelope_mm: match kind {
                    ControlKind::Led => {
                        let d: f64 = diameter_mm;
                        (d + 1.0, d + 1.0)
                    }
                    k => kind_envelope(k),
                },
            })
        };
        // Exact LED sizes first (a bare "led" defaults to 5 mm below).
        match name.as_str() {
            "led_3mm" | "led3mm" | "led_3" => return round(ControlKind::Led, LED_3MM_MM),
            "led_5mm" | "led5mm" | "led_5" => return round(ControlKind::Led, LED_5MM_MM),
            _ => {}
        }
        // Then keyword match — works for both cutout names ("Thonkiconn") and full
        // KiCad footprints a circuit part carries ("…:Jack_3.5mm_…PJ398SM…").
        if ["jack", "thonkiconn", "pj301", "pj398"]
            .iter()
            .any(|k| name.contains(k))
        {
            round(ControlKind::Jack, JACK_BARREL_MM)
        } else if ["potentiometer", "alpha9mm", "alphapot", "_pot"]
            .iter()
            .any(|k| name.contains(k))
        {
            round(ControlKind::Pot, POT_BUSHING_MM)
        } else if name.contains("led") {
            round(ControlKind::Led, LED_5MM_MM)
        } else if ["switch", "toggle", "_sw_"]
            .iter()
            .any(|k| name.contains(k))
        {
            round(ControlKind::Switch, TOGGLE_MM)
        } else {
            None
        }
    }
}

/// House silkscreen-layout rules (DESIGN §7.9 — designed once, applied
/// consistently, not invented per-panel). Millimetres.
mod silk {
    /// Title text height and its distance below the top edge. Sized like a real
    /// Eurorack faceplate — the module name reads across the room, not a 2 mm
    /// whisper.
    pub const TITLE_FONT_MM: f64 = 3.6;
    pub const TITLE_TOP_MARGIN_MM: f64 = 8.0;
    /// Control-label text height and its offset above the cutout centre (clears a
    /// [`super::JACK_BARREL_MM`]/2 barrel with margin).
    pub const LABEL_FONT_MM: f64 = 2.4;
    pub const LABEL_OFFSET_MM: f64 = 6.5;
    /// Brand logo: fraction of panel width, the minimum width worth drawing, and
    /// the clearances keeping it off the lowest cutout and the bottom edge/holes.
    /// The logo is the maker's mark — give it real presence in the bottom band.
    pub const LOGO_WIDTH_FRAC: f64 = 0.66;
    pub const LOGO_MIN_WIDTH_MM: f64 = 4.0;
    pub const LOGO_CUTOUT_GAP_MM: f64 = 2.5;
    pub const BOTTOM_MARGIN_MM: f64 = 6.0;
}

/// The cutout **geometry** for a cutout footprint/name — the render-time lookup,
/// resolving an already-chosen cutout by name through the [`BuiltinCutouts`]
/// catalogue. `None` if unknown. (Classification of a *circuit part* into a
/// control goes through [`CutoutSource::cutout`].)
pub fn footprint_shape(footprint: &str) -> Option<CutoutShape> {
    BuiltinCutouts.cutout(None, footprint).map(|s| s.shape)
}

/// The format-agnostic panel specification.
///
/// All dimensions are in millimeters. No format-specific concepts (HP, U,
/// enclosure size class) appear in the trait itself — those are internal to
/// each implementation.
pub trait PanelSpec {
    /// Panel width in mm.
    fn width_mm(&self) -> f64;
    /// Panel height in mm.
    fn height_mm(&self) -> f64;
    /// Panel thickness in mm.
    fn thickness_mm(&self) -> f64;
    /// Mounting holes.
    fn mounting_holes(&self) -> &[MountingHole];
    /// Anchored cutouts (jacks, pots, switches, LEDs, etc.).
    fn cutouts(&self) -> &[Cutout];
}

// ---------------------------------------------------------------------------
//  Eurorack implementation
// ---------------------------------------------------------------------------

const EURORACK_HEIGHT_MM: f64 = 128.5;
const HP_MM: f64 = 5.08;
const EURORACK_HOLE_DIAMETER_MM: f64 = 3.2;
const EURORACK_HOLE_INSET_X_MM: f64 = 7.5;
const EURORACK_HOLE_INSET_Y_MM: f64 = 3.0;

/// A Eurorack panel.
///
/// Constructed in HP (horizontal pitch) internally, but exposes only mm
/// through the [`PanelSpec`] trait.
#[derive(Debug, Clone, PartialEq)]
pub struct EurorackPanel {
    hp: u16,
    thickness_mm: f64,
    extra_holes: Vec<MountingHole>,
    cutouts: Vec<Cutout>,
}

impl EurorackPanel {
    /// Create a new Eurorack panel of the given HP width.
    ///
    /// Standard height (128.5 mm) and thickness (2.0 mm) are applied.
    /// Default mounting holes are added automatically based on HP width.
    pub fn new(hp: u16) -> Self {
        let mut panel = EurorackPanel {
            hp,
            thickness_mm: 2.0,
            extra_holes: Vec::new(),
            cutouts: Vec::new(),
        };
        panel.rebuild_default_holes();
        panel
    }

    /// Override the default thickness (mm).
    pub fn with_thickness(mut self, mm: f64) -> Self {
        self.thickness_mm = mm;
        self
    }

    /// Add a cutout at the given position (mm from bottom-left).
    pub fn with_cutout(mut self, x_mm: f64, y_mm: f64, footprint: impl Into<String>) -> Self {
        self.cutouts.push(Cutout {
            x_mm,
            y_mm,
            rotation_deg: 0.0,
            footprint: footprint.into(),
            refdes: None,
            label: None,
        });
        self
    }

    /// Add a cutout with explicit rotation, an optional anchored refdes, and an
    /// optional silkscreen/engraving label.
    pub fn with_cutout_rotated(
        mut self,
        x_mm: f64,
        y_mm: f64,
        rotation_deg: f64,
        footprint: impl Into<String>,
        refdes: Option<String>,
        label: Option<String>,
    ) -> Self {
        self.cutouts.push(Cutout {
            x_mm,
            y_mm,
            rotation_deg,
            footprint: footprint.into(),
            refdes,
            label,
        });
        self
    }

    /// Width in mm (HP × 5.08).
    pub fn width_mm_value(&self) -> f64 {
        f64::from(self.hp) * HP_MM
    }

    /// The HP width (internal unit, not part of `PanelSpec`).
    pub fn hp(&self) -> u16 {
        self.hp
    }

    fn rebuild_default_holes(&mut self) {
        let w = self.width_mm_value();
        let h = EURORACK_HEIGHT_MM;
        // Left side holes (always present).
        self.extra_holes.push(MountingHole {
            x_mm: EURORACK_HOLE_INSET_X_MM,
            y_mm: h - EURORACK_HOLE_INSET_Y_MM,
            diameter_mm: EURORACK_HOLE_DIAMETER_MM,
        });
        self.extra_holes.push(MountingHole {
            x_mm: EURORACK_HOLE_INSET_X_MM,
            y_mm: EURORACK_HOLE_INSET_Y_MM,
            diameter_mm: EURORACK_HOLE_DIAMETER_MM,
        });
        // Right side holes for panels ≥ 8 HP.
        if self.hp >= 8 {
            self.extra_holes.push(MountingHole {
                x_mm: w - EURORACK_HOLE_INSET_X_MM,
                y_mm: h - EURORACK_HOLE_INSET_Y_MM,
                diameter_mm: EURORACK_HOLE_DIAMETER_MM,
            });
            self.extra_holes.push(MountingHole {
                x_mm: w - EURORACK_HOLE_INSET_X_MM,
                y_mm: EURORACK_HOLE_INSET_Y_MM,
                diameter_mm: EURORACK_HOLE_DIAMETER_MM,
            });
        }
    }
}

impl PanelSpec for EurorackPanel {
    fn width_mm(&self) -> f64 {
        self.width_mm_value()
    }

    fn height_mm(&self) -> f64 {
        EURORACK_HEIGHT_MM
    }

    fn thickness_mm(&self) -> f64 {
        self.thickness_mm
    }

    fn mounting_holes(&self) -> &[MountingHole] {
        &self.extra_holes
    }

    fn cutouts(&self) -> &[Cutout] {
        &self.cutouts
    }
}

// ---------------------------------------------------------------------------
//  TOML file format (interim — until layout loop generates panels)
// ---------------------------------------------------------------------------

/// A panel definition read from a TOML file.
///
/// Example:
/// ```toml
/// format = "eurorack"
/// hp = 8
/// thickness_mm = 2.0
///
/// [[cutouts]]
/// x_mm = 10.0
/// y_mm = 50.0
/// footprint = "Thonkiconn"
/// ```
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct PanelFile {
    pub format: String,
    #[serde(default)]
    pub hp: Option<u16>,
    #[serde(default = "default_thickness")]
    pub thickness_mm: f64,
    /// Panel finish — a named material (`black`/`silver`/`white`/`green`) or a
    /// `#rrggbb` face color. Drives the 2D panel render's color (DESIGN §7.9).
    /// Absent → black.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<String>,
    #[serde(default)]
    pub cutouts: Vec<CutoutFile>,
}

fn default_thickness() -> f64 {
    2.0
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct CutoutFile {
    pub x_mm: f64,
    pub y_mm: f64,
    #[serde(default)]
    pub rotation_deg: f64,
    pub footprint: String,
    /// Board part anchored here (e.g. `"J1"`) — the board placer mates to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refdes: Option<String>,
    /// Silkscreen/engraving label for this control (e.g. `"IN"`, `"RATE"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl PanelFile {
    /// Parse from TOML bytes.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Convert to a concrete `PanelSpec` implementation.
    ///
    /// Returns `Err` if the format is unknown or required fields are missing.
    pub fn to_spec(&self) -> Result<Box<dyn PanelSpec>, String> {
        match self.format.as_str() {
            "eurorack" => {
                let hp = self.hp.ok_or("eurorack panel requires `hp`")?;
                let mut panel = EurorackPanel::new(hp).with_thickness(self.thickness_mm);
                for c in &self.cutouts {
                    panel = panel.with_cutout_rotated(
                        c.x_mm,
                        c.y_mm,
                        c.rotation_deg,
                        c.footprint.clone(),
                        c.refdes.clone(),
                        c.label.clone(),
                    );
                }
                Ok(Box::new(panel))
            }
            other => Err(format!("unsupported panel format: {other}")),
        }
    }

    /// Serialize back to a TOML document (an editable panel spec).
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// The resolved visual finish (defaults to black).
    pub fn resolved_finish(&self) -> PanelFinish {
        self.finish
            .as_deref()
            .map(PanelFinish::named)
            .unwrap_or_default()
    }
}

/// The named finishes the panel editor offers as swatches; each resolves via
/// [`PanelFinish::named`]. A `#rrggbb` custom color is also accepted.
pub const NAMED_FINISHES: &[&str] = &["black", "silver", "white", "green", "blue", "red"];

/// A panel's visual finish: the face color plus the color of its engraved /
/// printed legends (labels, title, logo). A panel is a flat front face in a real
/// material — not a green PCB — so it renders in this color (DESIGN §7.9).
#[derive(Debug, Clone, PartialEq)]
pub struct PanelFinish {
    /// A human name for the finish (for UIs).
    pub name: String,
    /// Panel face color (CSS hex).
    pub face: String,
    /// Legend / logo color (CSS hex).
    pub legend: String,
}

impl PanelFinish {
    /// Whether `spec` is a *recognized* finish token — a known material name or a
    /// valid `#rgb`/`#rrggbb` color. Unknown names render as black; the editor
    /// rejects them via this check so a typo isn't silently swallowed.
    pub fn is_recognized(spec: &str) -> bool {
        let s = spec.trim().to_ascii_lowercase();
        normalize_hex(spec.trim()).is_some()
            || NAMED_FINISHES.contains(&s.as_str())
            || matches!(s.as_str(), "aluminum" | "aluminium" | "raw" | "pcb")
    }

    /// Resolve a finish from a named material (`black`/`silver`/`white`/`green`/
    /// `blue`/`red`) or a `#rgb`/`#rrggbb` face color (legend auto-picked for
    /// contrast). Anything unknown falls back to black.
    pub fn named(spec: &str) -> PanelFinish {
        let s = spec.trim();
        if let Some(face) = normalize_hex(s) {
            let legend = if relative_luminance(&face) > 0.5 {
                "#1b1c1e"
            } else {
                "#f2f2ef"
            };
            return PanelFinish {
                name: spec.to_string(),
                face,
                legend: legend.to_string(),
            };
        }
        let (face, legend) = match s.to_ascii_lowercase().as_str() {
            "silver" | "aluminum" | "aluminium" | "raw" => ("#c9ccce", "#1b1c1e"),
            "white" => ("#f4f4f0", "#1b1c1e"),
            "green" | "pcb" => ("#0f5c3f", "#f2f2ef"),
            "blue" => ("#1c3f8f", "#f2f2ef"),
            "red" => ("#8f1c22", "#f2f2ef"),
            _ => ("#1c1d1f", "#f2f2ef"), // black — the default
        };
        PanelFinish {
            name: s.to_ascii_lowercase(),
            face: face.to_string(),
            legend: legend.to_string(),
        }
    }
}

impl Default for PanelFinish {
    fn default() -> Self {
        PanelFinish::named("black")
    }
}

/// Normalize `#rgb` / `#rrggbb` to lowercase `#rrggbb`; `None` if not a hex color.
fn normalize_hex(s: &str) -> Option<String> {
    let h = s.strip_prefix('#')?;
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match h.len() {
        6 => Some(format!("#{}", h.to_ascii_lowercase())),
        3 => {
            let mut out = String::from("#");
            for c in h.chars() {
                out.push(c.to_ascii_lowercase());
                out.push(c.to_ascii_lowercase());
            }
            Some(out)
        }
        _ => None,
    }
}

/// Rough relative luminance (0..1) of a `#rrggbb` color, for legend contrast.
fn relative_luminance(hex: &str) -> f64 {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 {
        return 0.0;
    }
    let ch = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0) as f64 / 255.0;
    0.2126 * ch(0) + 0.7152 * ch(2) + 0.0722 * ch(4)
}

/// House rules for the derived layout (DESIGN §7.9), designed once. The pitches
/// are minimum centre-to-centre spacings by the control's physical body/knob (not
/// its panel hole), so anchored footprints don't overlap — the failure the tight
/// even-spacing hit (a jack body is ~13 mm, so 12.8 mm spacing overlapped).
mod derive_rules {
    /// Minimum control pitch (mm) — realistic Eurorack spacings.
    pub const JACK_PITCH_MM: f64 = 16.0;
    pub const POT_PITCH_MM: f64 = 20.0;
    pub const SWITCH_PITCH_MM: f64 = 14.0;
    pub const LED_PITCH_MM: f64 = 9.0;
    /// Clear zones: below the top-edge title, above the bottom logo + holes.
    pub const TOP_MARGIN_MM: f64 = 14.0;
    pub const BOTTOM_MARGIN_MM: f64 = 16.0;
    /// Default Eurorack panel-PCB thickness (mm).
    pub const THICKNESS_MM: f64 = 1.6;
}

/// Minimum centre-to-centre pitch for a control kind.
fn control_pitch(kind: ControlKind) -> f64 {
    match kind {
        ControlKind::Jack => derive_rules::JACK_PITCH_MM,
        ControlKind::Pot => derive_rules::POT_PITCH_MM,
        ControlKind::Switch => derive_rules::SWITCH_PITCH_MM,
        ControlKind::Led => derive_rules::LED_PITCH_MM,
    }
}

/// Derive an editable [`PanelFile`] from a circuit: classify its panel-facing
/// parts (jacks/pots/switches/LEDs) through a [`CutoutSource`], arrange them in a
/// centred column — controls up top, jacks at the bottom (the Eurorack
/// convention) — and label each from the signal net it carries. Board-only parts
/// (passives, ICs, power headers) are skipped. Override any position by hand
/// afterwards; this is a starting point, not a straitjacket.
/// The panel-facing controls of a circuit, in layout order (knobs/switches first,
/// jacks last), each with the mechanical envelope it needs.
fn panel_controls(
    circuit: &dyn CircuitSource,
    cutouts: &dyn CutoutSource,
) -> Vec<(String, ControlKind, (f64, f64))> {
    let mut controls = Vec::new();
    let mut jacks = Vec::new();
    for part in circuit.parts() {
        let fp = part.footprint.as_deref().unwrap_or("");
        if let Some(spec) = cutouts.cutout(part.mpn.as_deref(), fp) {
            let entry = (part.refdes.0.clone(), spec.kind, spec.envelope_mm);
            if spec.kind.is_jack() {
                jacks.push(entry);
            } else {
                controls.push(entry);
            }
        }
    }
    controls.sort_by(|a, b| a.0.cmp(&b.0));
    jacks.sort_by(|a, b| a.0.cmp(&b.0));
    controls.into_iter().chain(jacks).collect()
}

/// The narrowest panel (HP) whose **hardware actually fits** — the widest control
/// envelope plus edge material on both sides (DESIGN §6.1).
///
/// This is the panel-side constraint, independent of whether the PCB's parts and
/// traces fit (see `board::minimum_hp`); a buildable module needs both. Without
/// it a derivation happily emits, say, a 3 HP panel (15.24 mm) carrying a 13.75 mm
/// pot body, which cannot be built.
pub fn min_panel_hp(circuit: &dyn CircuitSource, cutouts: &dyn CutoutSource) -> u16 {
    let widest = panel_controls(circuit, cutouts)
        .iter()
        .map(|(_, _, env)| env.0)
        .fold(0.0f64, f64::max);
    if widest <= 0.0 {
        return 1;
    }
    let needed = widest + 2.0 * envelope::EDGE_MM;
    (needed / HP_MM).ceil().max(1.0) as u16
}

pub fn derive_panel(circuit: &dyn CircuitSource, hp: u16, cutouts: &dyn CutoutSource) -> PanelFile {
    let ordered = panel_controls(circuit, cutouts);

    // Never emit a panel too narrow for its own hardware — a derived spec that
    // can't be built is worse than a wider one.
    let hp = hp.max(min_panel_hp(circuit, cutouts));

    let w = f64::from(hp) * HP_MM;
    let cx = w / 2.0;
    let h = EURORACK_HEIGHT_MM;

    // Stack controls top→bottom (knobs above jacks), spaced by the real envelope
    // each one needs plus a gap — never closer than the class minimum — and centre
    // the stack in the clear zone between the title and the bottom logo/holes.
    let pitches: Vec<f64> = ordered
        .iter()
        .map(|(_, k, env)| (env.1 + envelope::GAP_MM).max(control_pitch(*k)))
        .collect();
    let total: f64 = pitches.iter().sum();
    let avail_top = h - derive_rules::TOP_MARGIN_MM;
    let avail_bot = derive_rules::BOTTOM_MARGIN_MM;
    let avail = avail_top - avail_bot;
    // Centre the stack; if it overflows the panel height it still lays out (tightly
    // packed) — a signal the module has more controls than the height comfortably
    // holds, which the caller can act on (wider HP won't help; height is fixed).
    let mut y = avail_top - (avail - total).max(0.0) / 2.0;
    let mut out: Vec<CutoutFile> = Vec::new();
    for ((refdes, kind, _), pitch) in ordered.iter().zip(&pitches) {
        out.push(CutoutFile {
            x_mm: cx,
            y_mm: y - pitch / 2.0,
            rotation_deg: 0.0,
            footprint: kind.cutout_name().to_string(),
            refdes: Some(refdes.clone()),
            label: control_label(circuit, refdes),
        });
        y -= pitch;
    }

    PanelFile {
        format: "eurorack".into(),
        hp: Some(hp),
        thickness_mm: derive_rules::THICKNESS_MM,
        finish: None,
        cutouts: out,
    }
}

/// A short panel label for a control, from the most signal-like net it touches
/// (excluding power/ground). `SIG_IN` → "IN", `RATE_CV` → "RATE".
fn control_label(circuit: &dyn CircuitSource, refdes: &str) -> Option<String> {
    let mut sig: Vec<&str> = circuit
        .nets()
        .iter()
        .filter(|n| n.pins.iter().any(|p| p.refdes.0 == refdes))
        .map(|n| n.name.as_str())
        .filter(|n| !is_power_net(n))
        .collect();
    sig.sort();
    sig.first().map(|n| label_from_net(n))
}

/// Whether a net is a power rail / ground (so it isn't used as a control label).
fn is_power_net(name: &str) -> bool {
    let u = name.to_ascii_uppercase();
    u == "GND"
        || u.ends_with("GND")
        || matches!(u.as_str(), "VCC" | "VEE" | "VDD" | "VSS")
        || ((u.starts_with('+') || u.starts_with('-')) && u.contains('V'))
}

/// Shorten a net name into a control label: drop a `SIG_` prefix / `_CV` suffix,
/// spaces for underscores, upper-cased.
fn label_from_net(net: &str) -> String {
    let s = net.strip_prefix("SIG_").unwrap_or(net);
    let s = s.strip_suffix("_CV").unwrap_or(s);
    s.replace('_', " ").to_uppercase()
}

// ---------------------------------------------------------------------------
//  DXF export (ASCII, minimal, SendCutSend-compatible)
// ---------------------------------------------------------------------------

/// Write a DXF representation of `panel` to `w`.
///
/// The DXF contains:
/// * a closed `LWPOLYLINE` for the panel outline,
/// * `CIRCLE`s for round cutouts and mounting holes,
/// * `LWPOLYLINE`s for rectangular cutouts.
pub fn write_dxf<W: std::fmt::Write>(w: &mut W, panel: &dyn PanelSpec) -> std::fmt::Result {
    let width = panel.width_mm();
    let height = panel.height_mm();

    // Header ----------------------------------------------------------------
    writeln!(w, "0")?;
    writeln!(w, "SECTION")?;
    writeln!(w, "2")?;
    writeln!(w, "HEADER")?;
    writeln!(w, "9")?;
    writeln!(w, "$ACADVER")?;
    writeln!(w, "1")?;
    writeln!(w, "AC1032")?;
    writeln!(w, "0")?;
    writeln!(w, "ENDSEC")?;

    // Tables ----------------------------------------------------------------
    writeln!(w, "0")?;
    writeln!(w, "SECTION")?;
    writeln!(w, "2")?;
    writeln!(w, "TABLES")?;
    writeln!(w, "0")?;
    writeln!(w, "TABLE")?;
    writeln!(w, "2")?;
    writeln!(w, "LAYER")?;
    writeln!(w, "5")?;
    writeln!(w, "2")?;
    writeln!(w, "100")?;
    writeln!(w, "AcDbSymbolTable")?;
    writeln!(w, "70")?;
    writeln!(w, "1")?;
    writeln!(w, "0")?;
    writeln!(w, "LAYER")?;
    writeln!(w, "5")?;
    writeln!(w, "10")?;
    writeln!(w, "100")?;
    writeln!(w, "AcDbSymbolTableRecord")?;
    writeln!(w, "100")?;
    writeln!(w, "AcDbLayerTableRecord")?;
    writeln!(w, "2")?;
    writeln!(w, "0")?;
    writeln!(w, "70")?;
    writeln!(w, "0")?;
    writeln!(w, "62")?;
    writeln!(w, "7")?;
    writeln!(w, "6")?;
    writeln!(w, "Continuous")?;
    writeln!(w, "0")?;
    writeln!(w, "ENDTAB")?;
    writeln!(w, "0")?;
    writeln!(w, "ENDSEC")?;

    // Entities --------------------------------------------------------------
    writeln!(w, "0")?;
    writeln!(w, "SECTION")?;
    writeln!(w, "2")?;
    writeln!(w, "ENTITIES")?;

    // Panel outline.
    write_lwpolyline_rect(w, 0.0, 0.0, width, height)?;

    // Mounting holes.
    for hole in panel.mounting_holes() {
        write_circle(w, hole.x_mm, hole.y_mm, hole.diameter_mm / 2.0)?;
    }

    // Cutouts.
    for cutout in panel.cutouts() {
        match footprint_shape(&cutout.footprint) {
            Some(CutoutShape::Circle { diameter_mm }) => {
                write_circle(w, cutout.x_mm, cutout.y_mm, diameter_mm / 2.0)?;
            }
            Some(CutoutShape::RoundedRect {
                width_mm,
                height_mm,
                corner_radius_mm: _,
            }) => {
                // For laser/waterjet, a sharp rectangle is fine; radius is
                // handled by the cutter kerf or post-processing.
                let hw = width_mm / 2.0;
                let hh = height_mm / 2.0;
                write_lwpolyline_rect(
                    w,
                    cutout.x_mm - hw,
                    cutout.y_mm - hh,
                    cutout.x_mm + hw,
                    cutout.y_mm + hh,
                )?;
            }
            None => {
                // Unknown footprint — emit a small circle as a visual marker.
                write_circle(w, cutout.x_mm, cutout.y_mm, 1.5)?;
            }
        }
    }

    writeln!(w, "0")?;
    writeln!(w, "ENDSEC")?;
    writeln!(w, "0")?;
    writeln!(w, "EOF")?;
    Ok(())
}

/// Generate a DXF string from a panel.
pub fn panel_to_dxf(panel: &dyn PanelSpec) -> String {
    let mut s = String::with_capacity(4096);
    write_dxf(&mut s, panel).expect("write to String is infallible");
    s
}

/// Generate a **panel PCB** (`.kicad_pcb`): the outline, jack/pot/LED cutouts and
/// mounting holes as `Edge.Cuts` loops (inner loops become board cutouts), plus a
/// silkscreen title. This is the "PCB panel" many Eurorack builders order instead
/// of a milled aluminium one — it runs through the same gerber export as any
/// board. Mechanical only: no copper, no components.
///
/// Panel coordinates are measured from the bottom-left; KiCad's are top-down, so
/// Y is flipped here.
/// Render the panel as a flat 2D SVG in its real finish color — the front face a
/// builder sees, not a green PCB. Cutouts are drawn as holes; labels, title, and
/// the brand logo go in the legend color, placed by the same house rules as the
/// KiCad panel. No external tools, so it's cheap to generate per request.
pub fn panel_to_svg(
    panel: &dyn PanelSpec,
    title: &str,
    finish: &PanelFinish,
    logo: Option<&Logo>,
) -> String {
    let w = panel.width_mm();
    let h = panel.height_mm();
    let pad = 3.0;
    // Panel y is measured up from the bottom; SVG y runs down from the top.
    let sy = |y: f64| h - y;

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{:.2} {:.2} {:.2} {:.2}\" \
         width=\"{:.0}\" height=\"{:.0}\" role=\"img\" aria-label=\"{} panel\">",
        -pad,
        -pad,
        w + 2.0 * pad,
        h + 2.0 * pad,
        (w + 2.0 * pad) * 4.0,
        (h + 2.0 * pad) * 4.0,
        xml_escape(title),
    ));
    // Panel face.
    s.push_str(&format!(
        "<rect x=\"0\" y=\"0\" width=\"{:.2}\" height=\"{:.2}\" rx=\"1.2\" fill=\"{}\" \
         stroke=\"rgba(0,0,0,0.28)\" stroke-width=\"0.2\"/>",
        w, h, finish.face
    ));
    // Mounting holes.
    for mh in panel.mounting_holes() {
        s.push_str(&svg_hole_circle(mh.x_mm, sy(mh.y_mm), mh.diameter_mm / 2.0));
    }
    // Control cutouts + their labels.
    for c in panel.cutouts() {
        let (cx, cy) = (c.x_mm, sy(c.y_mm));
        match footprint_shape(&c.footprint) {
            Some(CutoutShape::Circle { diameter_mm }) => {
                s.push_str(&svg_hole_circle(cx, cy, diameter_mm / 2.0));
            }
            Some(CutoutShape::RoundedRect {
                width_mm,
                height_mm,
                corner_radius_mm,
            }) => {
                s.push_str(&svg_hole_rect(
                    cx,
                    cy,
                    width_mm,
                    height_mm,
                    corner_radius_mm,
                ));
            }
            None => s.push_str(&svg_hole_circle(cx, cy, 1.5)),
        }
        if let Some(label) = &c.label {
            s.push_str(&svg_text(
                cx,
                cy - silk::LABEL_OFFSET_MM,
                silk::LABEL_FONT_MM,
                &finish.legend,
                label,
            ));
        }
    }
    // Title, top-centre.
    if !title.is_empty() {
        s.push_str(&svg_text(
            w / 2.0,
            silk::TITLE_TOP_MARGIN_MM,
            silk::TITLE_FONT_MM,
            &finish.legend,
            title,
        ));
    }
    // Brand logo in the clear band below the lowest cutout (ported house rule).
    if let Some(logo) = logo {
        let lowest = panel
            .cutouts()
            .iter()
            .map(|c| {
                let r = match footprint_shape(&c.footprint) {
                    Some(CutoutShape::Circle { diameter_mm }) => diameter_mm / 2.0,
                    Some(CutoutShape::RoundedRect { height_mm, .. }) => height_mm / 2.0,
                    None => 1.5,
                };
                sy(c.y_mm) + r
            })
            .fold(10.0, f64::max);
        let bottom_limit = h - silk::BOTTOM_MARGIN_MM;
        let gap = bottom_limit - lowest;
        let (lx0, ly0, lx1, ly1) = logo.bbox();
        let aspect = (ly1 - ly0) / (lx1 - lx0).max(1e-6);
        let target_w = (w * silk::LOGO_WIDTH_FRAC)
            .min((gap - silk::LOGO_CUTOUT_GAP_MM).max(0.0) / aspect.max(1e-6));
        if target_w >= silk::LOGO_MIN_WIDTH_MM {
            let logo_h = target_w * aspect;
            let center = (w / 2.0, lowest + silk::LOGO_CUTOUT_GAP_MM + logo_h / 2.0);
            let placed = logo.place(target_w, center, false);
            let mut d = String::new();
            for sp in &placed {
                for (i, (x, y)) in sp.iter().enumerate() {
                    d.push_str(&format!(
                        "{}{:.2} {:.2} ",
                        if i == 0 { 'M' } else { 'L' },
                        x,
                        y
                    ));
                }
                d.push('Z');
            }
            if !d.is_empty() {
                s.push_str(&format!(
                    "<path d=\"{}\" fill=\"{}\" fill-rule=\"evenodd\"/>",
                    d, finish.legend
                ));
            }
        }
    }
    s.push_str("</svg>");
    s
}

fn svg_hole_circle(cx: f64, cy: f64, r: f64) -> String {
    format!(
        "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"rgba(0,0,0,0.55)\" \
         stroke=\"rgba(255,255,255,0.18)\" stroke-width=\"0.25\"/>",
        cx, cy, r
    )
}

fn svg_hole_rect(cx: f64, cy: f64, w: f64, h: f64, r: f64) -> String {
    format!(
        "<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" rx=\"{:.2}\" \
         fill=\"rgba(0,0,0,0.55)\" stroke=\"rgba(255,255,255,0.18)\" stroke-width=\"0.25\"/>",
        cx - w / 2.0,
        cy - h / 2.0,
        w,
        h,
        r
    )
}

fn svg_text(x: f64, y: f64, size: f64, color: &str, text: &str) -> String {
    format!(
        "<text x=\"{:.2}\" y=\"{:.2}\" font-family=\"'Helvetica Neue',Arial,sans-serif\" \
         font-size=\"{:.2}\" font-weight=\"600\" fill=\"{}\" text-anchor=\"middle\" \
         dominant-baseline=\"central\">{}</text>",
        x,
        y,
        size,
        color,
        xml_escape(text)
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn panel_to_kicad_pcb(panel: &dyn PanelSpec, title: &str, logo: Option<&Logo>) -> String {
    use crate::board::{det_uuid, mm};
    let (w, h) = (panel.width_mm(), panel.height_mm());
    // Centre on KiCad's A4 sheet (297×210 landscape) instead of the (0,0) corner.
    let ox = ((297.0 - w) / 2.0).max(10.0);
    let oy = ((210.0 - h) / 2.0).max(10.0);
    let fx = |x: f64| ox + x;
    let fy = |y: f64| oy + (h - y);
    let edge_rect = |x1: f64, y1: f64, x2: f64, y2: f64, seed: &str| {
        format!(
            "  (gr_rect (start {} {}) (end {} {}) (stroke (width 0.15) (type solid)) \
             (fill no) (layer \"Edge.Cuts\") (uuid \"{}\"))\n",
            mm(x1),
            mm(y1),
            mm(x2),
            mm(y2),
            det_uuid(seed)
        )
    };
    let edge_circle = |cx: f64, cy: f64, r: f64, seed: &str| {
        format!(
            "  (gr_circle (center {} {}) (end {} {}) (stroke (width 0.15) (type solid)) \
             (fill no) (layer \"Edge.Cuts\") (uuid \"{}\"))\n",
            mm(cx),
            mm(cy),
            mm(cx + r),
            mm(cy),
            det_uuid(seed)
        )
    };

    let mut s = String::new();
    s.push_str(
        "(kicad_pcb (version 20241229) (generator \"legion-of-bom\") (generator_version \"9.0\")\n\
         \x20 (general (thickness 1.6))\n  (paper \"A4\")\n\
         \x20 (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (5 \"F.SilkS\" user) \
         (7 \"B.SilkS\" user) (1 \"F.Mask\" user) (3 \"B.Mask\" user) (25 \"Edge.Cuts\" user) \
         (35 \"F.Fab\" user) (33 \"B.Fab\" user))\n\
         \x20 (setup (pad_to_mask_clearance 0))\n  (net 0 \"\")\n",
    );
    // Panel outline.
    s.push_str(&edge_rect(ox, oy, ox + w, oy + h, "panel.outline"));
    // Mounting holes.
    for (i, hole) in panel.mounting_holes().iter().enumerate() {
        s.push_str(&edge_circle(
            fx(hole.x_mm),
            fy(hole.y_mm),
            hole.diameter_mm / 2.0,
            &format!("panel.hole.{i}"),
        ));
    }
    // Cutouts (jack rects, pot/LED circles), as inner Edge.Cuts loops.
    for (i, c) in panel.cutouts().iter().enumerate() {
        let (cx, cy) = (fx(c.x_mm), fy(c.y_mm));
        let seed = format!("panel.cut.{i}");
        match footprint_shape(&c.footprint) {
            Some(CutoutShape::Circle { diameter_mm }) => {
                s.push_str(&edge_circle(cx, cy, diameter_mm / 2.0, &seed))
            }
            Some(CutoutShape::RoundedRect {
                width_mm,
                height_mm,
                ..
            }) => s.push_str(&edge_rect(
                cx - width_mm / 2.0,
                cy - height_mm / 2.0,
                cx + width_mm / 2.0,
                cy + height_mm / 2.0,
                &seed,
            )),
            None => s.push_str(&edge_circle(cx, cy, 1.5, &seed)),
        }
        // Control label (IN / OUT / RATE), horizontal, just above the cutout so
        // it reads with the module upright (DESIGN 6.10 / j54.21).
        if let Some(label) = &c.label {
            s.push_str(&format!(
                "  (gr_text \"{}\" (at {} {} 0) (layer \"F.SilkS\") (uuid \"{}\") \
                 (effects (font (size {f} {f}) (thickness 0.3))))\n",
                label,
                mm(cx),
                mm(cy - silk::LABEL_OFFSET_MM),
                det_uuid(&format!("panel.label.{i}")),
                f = silk::LABEL_FONT_MM,
            ));
        }
    }
    // Title, horizontal, along the top edge (below the top mounting holes) so it
    // never crosses a centred control column — a vertical centre title collides
    // with the knobs/jacks (the "writing hitting a jack" failure, j54-6f8).
    s.push_str(&format!(
        "  (gr_text \"{}\" (at {} {} 0) (layer \"F.SilkS\") (uuid \"{}\") \
         (effects (font (size {f} {f}) (thickness 0.3))))\n",
        title,
        mm(ox + w / 2.0),
        mm(oy + silk::TITLE_TOP_MARGIN_MM),
        det_uuid("panel.title"),
        f = silk::TITLE_FONT_MM,
    ));
    // Brand logo on the front silk (DESIGN §7.9), placed in the clear band below
    // the lowest cutout (above the bottom mounting holes) so it doesn't land on a
    // jack. Skipped if there's no room.
    if let Some(logo) = logo {
        // Lowest cutout edge in KiCad y (larger y = nearer the panel bottom).
        let lowest = panel
            .cutouts()
            .iter()
            .map(|c| {
                let r = match footprint_shape(&c.footprint) {
                    Some(CutoutShape::Circle { diameter_mm }) => diameter_mm / 2.0,
                    Some(CutoutShape::RoundedRect { height_mm, .. }) => height_mm / 2.0,
                    None => 1.5,
                };
                fy(c.y_mm) + r
            })
            .fold(oy + 10.0, f64::max);
        let bottom_limit = oy + h - silk::BOTTOM_MARGIN_MM; // clear bottom holes/edge
        let gap = bottom_limit - lowest;
        let (lx0, ly0, lx1, ly1) = logo.bbox();
        let aspect = (ly1 - ly0) / (lx1 - lx0).max(1e-6);
        // Fit within the house width fraction and the available vertical gap.
        let target_w = (w * silk::LOGO_WIDTH_FRAC)
            .min((gap - silk::LOGO_CUTOUT_GAP_MM).max(0.0) / aspect.max(1e-6));
        if target_w >= silk::LOGO_MIN_WIDTH_MM {
            let logo_h = target_w * aspect;
            let center = (
                ox + w / 2.0,
                lowest + silk::LOGO_CUTOUT_GAP_MM + logo_h / 2.0,
            );
            let placed = logo.place(target_w, center, false);
            for block in crate::logo::gr_polys(&placed, "F.SilkS", false, "panel.logo") {
                s.push_str(&block);
            }
        }
    }
    s.push_str(")\n");
    s
}

fn write_circle<W: std::fmt::Write>(w: &mut W, cx: f64, cy: f64, r: f64) -> std::fmt::Result {
    writeln!(w, "0")?;
    writeln!(w, "CIRCLE")?;
    writeln!(w, "8")?;
    writeln!(w, "0")?;
    writeln!(w, "10")?;
    writeln!(w, "{cx}")?;
    writeln!(w, "20")?;
    writeln!(w, "{cy}")?;
    writeln!(w, "40")?;
    writeln!(w, "{r}")
}

fn write_lwpolyline_rect<W: std::fmt::Write>(
    w: &mut W,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
) -> std::fmt::Result {
    writeln!(w, "0")?;
    writeln!(w, "LWPOLYLINE")?;
    writeln!(w, "8")?;
    writeln!(w, "0")?;
    writeln!(w, "100")?;
    writeln!(w, "AcDbEntity")?;
    writeln!(w, "100")?;
    writeln!(w, "AcDbPolyline")?;
    writeln!(w, "90")?;
    writeln!(w, "4")?;
    writeln!(w, "70")?;
    writeln!(w, "1")?; // closed
    writeln!(w, "43")?;
    writeln!(w, "0.0")?;

    for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
        writeln!(w, "10")?;
        writeln!(w, "{x}")?;
        writeln!(w, "20")?;
        writeln!(w, "{y}")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
//  Panel order tracking (Dolt)
// ---------------------------------------------------------------------------

const PANEL_ORDERS_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS panel_orders (\
  id INT AUTO_INCREMENT PRIMARY KEY,\
  module VARCHAR(64) NOT NULL,\
  dxf_path TEXT NOT NULL,\
  vendor VARCHAR(32),\
  status VARCHAR(16) DEFAULT 'not_ordered',\
  ordered_at DATETIME,\
  tracking_ref TEXT,\
  notes TEXT);";

/// One row in the panel-orders table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelOrder {
    pub id: i64,
    pub module: String,
    pub dxf_path: String,
    pub vendor: Option<String>,
    pub status: PanelOrderStatus,
    pub ordered_at: Option<String>,
    pub tracking_ref: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelOrderStatus {
    NotOrdered,
    Ordered,
    Shipped,
    Received,
}

impl PanelOrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PanelOrderStatus::NotOrdered => "not_ordered",
            PanelOrderStatus::Ordered => "ordered",
            PanelOrderStatus::Shipped => "shipped",
            PanelOrderStatus::Received => "received",
        }
    }
}

impl std::str::FromStr for PanelOrderStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "not_ordered" => Ok(PanelOrderStatus::NotOrdered),
            "ordered" => Ok(PanelOrderStatus::Ordered),
            "shipped" => Ok(PanelOrderStatus::Shipped),
            "received" => Ok(PanelOrderStatus::Received),
            other => Err(format!("unknown panel order status: {other}")),
        }
    }
}

/// A handle to the Dolt-backed panel-orders store.
#[derive(Debug, Clone)]
pub struct PanelOrders {
    root: PathBuf,
    dolt: PathBuf,
}

impl PanelOrders {
    /// Open (initialising if needed) the panel-orders store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, PartsError> {
        let dolt = find_on_path("dolt").ok_or(PartsError::DoltNotFound)?;
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let store = PanelOrders { root, dolt };
        if !store.root.join(".dolt").is_dir() {
            store.dolt(&["init"], "init")?;
        }
        store.sql(PANEL_ORDERS_SCHEMA)?;
        Ok(store)
    }

    /// Record a new panel order row.
    pub fn create(
        &self,
        module: &str,
        dxf_path: &str,
        vendor: Option<&str>,
        notes: Option<&str>,
    ) -> Result<i64, PartsError> {
        let sql = format!(
            "INSERT INTO panel_orders (module, dxf_path, vendor, status, notes) \
             VALUES ({}, {}, {}, {}, {});",
            sql_str(module),
            sql_str(dxf_path),
            sql_opt(vendor),
            sql_str(PanelOrderStatus::NotOrdered.as_str()),
            sql_opt(notes),
        );
        self.sql(&sql)?;
        // Fetch the auto-increment id back.
        let rows = self.query("SELECT LAST_INSERT_ID() AS id;")?;
        let id = rows
            .into_iter()
            .next()
            .and_then(|r| r.get("id").and_then(serde_json::Value::as_i64))
            .unwrap_or(0);
        Ok(id)
    }

    /// Find the most recent order for a module, if any.
    pub fn latest(&self, module: &str) -> Result<Option<PanelOrder>, PartsError> {
        let rows = self.query(&format!(
            "SELECT * FROM panel_orders WHERE module={} ORDER BY id DESC LIMIT 1",
            sql_str(module)
        ))?;
        Ok(rows.into_iter().next().and_then(parse_order_row))
    }

    /// List all orders for a module, newest first.
    pub fn list(&self, module: &str) -> Result<Vec<PanelOrder>, PartsError> {
        let rows = self.query(&format!(
            "SELECT * FROM panel_orders WHERE module={} ORDER BY id DESC",
            sql_str(module)
        ))?;
        Ok(rows.into_iter().filter_map(parse_order_row).collect())
    }

    /// Update status to `ordered` and record vendor + tracking ref.
    pub fn mark_ordered(
        &self,
        module: &str,
        vendor: &str,
        tracking_ref: Option<&str>,
    ) -> Result<(), PartsError> {
        let sql = format!(
            "UPDATE panel_orders SET status={}, ordered_at=NOW(), vendor={}, tracking_ref={} \
             WHERE module={} AND status={};",
            sql_str(PanelOrderStatus::Ordered.as_str()),
            sql_str(vendor),
            sql_opt(tracking_ref),
            sql_str(module),
            sql_str(PanelOrderStatus::NotOrdered.as_str()),
        );
        self.sql(&sql)
    }

    /// Update status for a given order id.
    pub fn set_status(&self, id: i64, status: PanelOrderStatus) -> Result<(), PartsError> {
        let sql = format!(
            "UPDATE panel_orders SET status={} WHERE id={id};",
            sql_str(status.as_str()),
        );
        self.sql(&sql)
    }

    // ---- dolt plumbing (mirrors PartsLibrary) -----------------------------

    fn dolt(&self, args: &[&str], context: &str) -> Result<String, PartsError> {
        let output = Command::new(&self.dolt)
            .current_dir(&self.root)
            .args(args)
            .output()?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(PartsError::Dolt {
                context: context.to_string(),
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    fn sql(&self, sql: &str) -> Result<(), PartsError> {
        self.dolt(&["sql", "-q", sql], "sql").map(|_| ())
    }

    fn query(&self, sql: &str) -> Result<Vec<serde_json::Value>, PartsError> {
        let stdout = self.dolt(&["sql", "-q", sql, "-r", "json"], "query")?;
        if stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        let value: serde_json::Value = serde_json::from_str(&stdout)?;
        Ok(value
            .get("rows")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default())
    }
}

/// The default panel-orders location (override with `LOB_PANEL_ORDERS_DIR`).
pub fn default_panel_orders_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOB_PANEL_ORDERS_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("legion-of-bom").join("panels")
}

fn parse_order_row(row: serde_json::Value) -> Option<PanelOrder> {
    Some(PanelOrder {
        id: row.get("id").and_then(serde_json::Value::as_i64)?,
        module: row.get("module")?.as_str()?.to_string(),
        dxf_path: row.get("dxf_path")?.as_str()?.to_string(),
        vendor: row
            .get("vendor")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        status: row
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse().ok())?,
        ordered_at: row
            .get("ordered_at")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        tracking_ref: row
            .get("tracking_ref")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        notes: row
            .get("notes")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
    })
}

// ---- SQL helpers (copied from parts.rs; kept private to avoid pub) --------

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn sql_opt(s: Option<&str>) -> String {
    s.map(sql_str).unwrap_or_else(|| "NULL".to_string())
}

// ---------------------------------------------------------------------------
//  Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eurorack_dimensions_in_mm() {
        let panel = EurorackPanel::new(6);
        assert_eq!(panel.width_mm(), 6.0 * 5.08);
        assert_eq!(panel.height_mm(), 128.5);
        assert_eq!(panel.thickness_mm(), 2.0);
    }

    #[test]
    fn finish_resolves_materials_and_hex() {
        assert_eq!(PanelFinish::default().face, "#1c1d1f"); // black default
        assert_eq!(PanelFinish::named("silver").face, "#c9ccce");
        assert_eq!(PanelFinish::named("white").legend, "#1b1c1e"); // dark legend, light face
        assert_eq!(PanelFinish::named("green").face, "#0f5c3f");
        assert_eq!(PanelFinish::named("bogus").face, "#1c1d1f"); // unknown → black
                                                                 // Hex passthrough with auto-contrast legend; #rgb expands to #rrggbb.
        let white = PanelFinish::named("#ffffff");
        assert_eq!(white.face, "#ffffff");
        assert_eq!(white.legend, "#1b1c1e");
        assert_eq!(PanelFinish::named("#000").face, "#000000");
        assert_eq!(PanelFinish::named("#000").legend, "#f2f2ef");
    }

    #[test]
    fn panel_svg_uses_finish_color_labels_and_cutouts() {
        let panel = EurorackPanel::new(8)
            .with_cutout_rotated(
                20.32,
                100.0,
                0.0,
                "Alpha9mm",
                None,
                Some("RATE".to_string()),
            )
            .with_cutout_rotated(
                20.32,
                14.0,
                0.0,
                "Thonkiconn",
                None,
                Some("OUT".to_string()),
            );
        let svg = panel_to_svg(&panel, "Slew Limiter", &PanelFinish::named("black"), None);
        assert!(svg.starts_with("<svg"));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains("#1c1d1f"), "black face color present");
        assert!(svg.contains(">Slew Limiter</text>"), "title");
        assert!(
            svg.contains(">RATE</text>") && svg.contains(">OUT</text>"),
            "labels"
        );
        assert!(
            svg.matches("<circle").count() >= 2,
            "control holes as circles"
        );
    }

    #[test]
    fn panel_pcb_has_outline_cutouts_and_title() {
        let panel = EurorackPanel::new(6)
            .with_cutout(15.24, 100.0, "Alpha9mm") // pot -> circle
            .with_cutout(15.24, 30.0, "Thonkiconn"); // jack -> barrel circle
        let pcb = panel_to_kicad_pcb(&panel, "demo", None);
        assert!(pcb.starts_with("(kicad_pcb"));
        assert!(pcb.contains(r#"(layer "Edge.Cuts")"#));
        // Pot + jack are both round holes now (jack = barrel, not a body rect).
        assert!(
            pcb.matches("gr_circle").count() >= 2,
            "pot + jack barrel circles"
        );
        // The outline is the one rectangle.
        assert!(pcb.contains("gr_rect"));
        assert!(pcb.contains(r#"(gr_text "demo""#));
        assert!(pcb.contains(r#"(layer "F.SilkS")"#));
    }

    #[test]
    fn cutout_label_renders_on_panel_silk() {
        let panel = EurorackPanel::new(4)
            .with_cutout_rotated(
                10.0,
                20.0,
                0.0,
                "Thonkiconn",
                Some("J1".into()),
                Some("IN".into()),
            )
            .with_cutout(10.0, 60.0, "Alpha9mm"); // no label → no extra gr_text
        let pcb = panel_to_kicad_pcb(&panel, "Demo", None);
        assert!(
            pcb.contains(r#"(gr_text "IN""#),
            "labelled cutout gets a silk label"
        );
        // Only the title + the one labelled cutout produce silk text.
        assert_eq!(pcb.matches("gr_text").count(), 2);
    }

    #[test]
    fn eurorack_small_panel_two_holes() {
        let panel = EurorackPanel::new(4);
        assert_eq!(panel.mounting_holes().len(), 2);
    }

    #[test]
    fn eurorack_large_panel_four_holes() {
        let panel = EurorackPanel::new(10);
        assert_eq!(panel.mounting_holes().len(), 4);
    }

    #[test]
    fn eurorack_cutouts_round_trip() {
        let panel = EurorackPanel::new(8)
            .with_cutout(10.0, 50.0, "Thonkiconn")
            .with_cutout(25.0, 50.0, "Alpha9mm");
        assert_eq!(panel.cutouts().len(), 2);
        assert_eq!(panel.cutouts()[0].footprint, "Thonkiconn");
        assert_eq!(panel.cutouts()[1].footprint, "Alpha9mm");
    }

    #[test]
    fn footprint_shape_lookup() {
        assert!(matches!(
            footprint_shape("Thonkiconn"),
            Some(CutoutShape::Circle { diameter_mm: 6.0 })
        ));
        assert!(matches!(
            footprint_shape("Alpha9mm"),
            Some(CutoutShape::Circle { diameter_mm: 7.0 })
        ));
        assert!(matches!(
            footprint_shape("LED_3mm"),
            Some(CutoutShape::Circle { diameter_mm: 3.0 })
        ));
        assert!(footprint_shape("UnknownThing").is_none());
    }

    #[test]
    fn builtin_cutouts_classify_real_footprints() {
        let c = BuiltinCutouts;
        // Full KiCad footprints a circuit part carries → control + barrel geometry.
        let jack = c
            .cutout(None, "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM")
            .unwrap();
        assert_eq!(jack.kind, ControlKind::Jack);
        assert!(matches!(
            jack.shape,
            CutoutShape::Circle { diameter_mm } if (diameter_mm - JACK_BARREL_MM).abs() < 1e-9
        ));
        assert_eq!(
            c.cutout(None, "Potentiometer_THT:Potentiometer_Alpha_RD901F")
                .unwrap()
                .kind,
            ControlKind::Pot
        );
        // Board-only parts are not panel-facing.
        assert!(c.cutout(None, "Package_DIP:DIP-16_W7.62mm").is_none());
        assert!(c
            .cutout(None, "Connector_PinHeader_2.54mm:PinHeader_2x05")
            .is_none());
    }

    /// A derived panel must be physically buildable: every control's mechanical
    /// envelope (knob skirt / PCB body — not the little hole) has to fit inside the
    /// panel with edge material left, and envelopes must not overlap vertically.
    /// Deriving from hole sizes alone produced 3 HP panels carrying 13.75 mm pot
    /// bodies, which look fine on screen and cannot be built (5p5).
    #[test]
    fn derived_panel_hardware_physically_fits() {
        use crate::model::{Circuit, Net, Part, PinRef};
        let mut circ = Circuit::new("m");
        circ.parts = vec![
            Part::new("RV1", "100k").with_footprint("Potentiometer_THT:Potentiometer_Alpha_RD901F"),
            Part::new("RV2", "100k").with_footprint("Potentiometer_THT:Potentiometer_Alpha_RD901F"),
            Part::new("J1", "jack").with_footprint("Connector_Audio:Jack_3.5mm_PJ398SM"),
        ];
        circ.nets = vec![Net::new("SIG_IN", vec![PinRef::new("J1", "T")])];

        // A pot body is 14 mm; 2 HP is 10.16 mm, so it cannot possibly fit — the
        // derivation must widen rather than emit an unbuildable panel.
        let min = min_panel_hp(&circ, &BuiltinCutouts);
        assert_eq!(min, 4, "14mm control + 2x1mm edge needs 16mm => 4 HP");
        let panel = derive_panel(&circ, 2, &BuiltinCutouts);
        assert_eq!(panel.hp, Some(4), "asked for 2 HP, widened to what fits");

        let w = f64::from(panel.hp.unwrap()) * HP_MM;
        let env = |fp: &str| match fp {
            "Alpha9mm" => envelope::POT,
            "Thonkiconn" => envelope::JACK,
            _ => (6.0, 6.0),
        };
        // Every envelope sits inside the panel with edge material to spare.
        for c in &panel.cutouts {
            let (ew, _) = env(&c.footprint);
            assert!(
                c.x_mm - ew / 2.0 >= envelope::EDGE_MM - 1e-9
                    && c.x_mm + ew / 2.0 <= w - envelope::EDGE_MM + 1e-9,
                "{:?} envelope runs off the panel",
                c.refdes
            );
        }
        // And no two envelopes overlap vertically.
        let mut stack: Vec<(f64, f64)> = panel
            .cutouts
            .iter()
            .map(|c| (c.y_mm, env(&c.footprint).1))
            .collect();
        stack.sort_by(|a, b| b.0.total_cmp(&a.0));
        for w in stack.windows(2) {
            let gap = (w[0].0 - w[0].1 / 2.0) - (w[1].0 + w[1].1 / 2.0);
            assert!(gap >= -1e-9, "control envelopes overlap by {:.2}mm", -gap);
        }
    }

    #[test]
    fn derive_panel_classifies_labels_and_orders() {
        use crate::model::{Circuit, Net, Part, PinRef};
        let mut circ = Circuit::new("m");
        circ.parts = vec![
            Part::new("RV1", "100k").with_footprint("Potentiometer_THT:Potentiometer_Alpha_RD901F"),
            Part::new("J1", "jack").with_footprint("Connector_Audio:Jack_3.5mm_PJ398SM"),
            Part::new("U1", "TL072").with_footprint("Package_SO:SOIC-8"), // board-only
        ];
        circ.nets = vec![
            Net::new("RATE_CV", vec![PinRef::new("RV1", "2")]),
            Net::new("SIG_IN", vec![PinRef::new("J1", "T")]),
            Net::new("GND", vec![PinRef::new("J1", "S")]),
        ];
        let panel = derive_panel(&circ, 8, &BuiltinCutouts);
        // Only the pot + jack; the IC is skipped.
        assert_eq!(panel.cutouts.len(), 2);
        let rv1 = panel
            .cutouts
            .iter()
            .find(|c| c.refdes.as_deref() == Some("RV1"))
            .unwrap();
        assert_eq!(rv1.footprint, "Alpha9mm");
        assert_eq!(rv1.label.as_deref(), Some("RATE")); // RATE_CV → RATE
        let j1 = panel
            .cutouts
            .iter()
            .find(|c| c.refdes.as_deref() == Some("J1"))
            .unwrap();
        assert_eq!(j1.footprint, "Thonkiconn");
        assert_eq!(j1.label.as_deref(), Some("IN")); // SIG_IN (not GND) → IN
                                                     // The knob sits above the jack (larger y in panel bottom-up coords).
        assert!(rv1.y_mm > j1.y_mm);
        // The derived spec round-trips through TOML.
        assert!(panel.to_toml().unwrap().contains("Thonkiconn"));
    }

    #[test]
    fn dxf_contains_entities() {
        let panel = EurorackPanel::new(8)
            .with_cutout(10.0, 50.0, "Thonkiconn")
            .with_cutout(25.0, 50.0, "Alpha9mm");
        let dxf = panel_to_dxf(&panel);
        assert!(dxf.contains("LWPOLYLINE"));
        assert!(dxf.contains("CIRCLE"));
        assert!(dxf.contains("EOF"));
        // Mounting holes + Alpha9mm = at least 5 circles (4 holes + 1 cutout).
        assert!(dxf.matches("CIRCLE").count() >= 5);
    }

    #[test]
    fn panel_file_roundtrip() {
        let toml = r#"
format = "eurorack"
hp = 8
thickness_mm = 2.0

[[cutouts]]
x_mm = 10.0
y_mm = 50.0
footprint = "Thonkiconn"

[[cutouts]]
x_mm = 25.0
y_mm = 50.0
footprint = "Alpha9mm"
"#;
        let file = PanelFile::from_toml(toml).unwrap();
        assert_eq!(file.format, "eurorack");
        assert_eq!(file.hp, Some(8));
        assert_eq!(file.cutouts.len(), 2);

        let spec = file.to_spec().unwrap();
        assert_eq!(spec.width_mm(), 8.0 * 5.08);
        assert_eq!(spec.cutouts().len(), 2);
    }

    #[test]
    fn panel_file_rejects_unknown_format() {
        let toml = r#"format = "pedal""#;
        let file = PanelFile::from_toml(toml).unwrap();
        assert!(file.to_spec().is_err());
    }

    #[test]
    fn panel_order_status_roundtrip() {
        assert_eq!(
            "not_ordered".parse::<PanelOrderStatus>().unwrap(),
            PanelOrderStatus::NotOrdered
        );
        assert_eq!(
            "ordered".parse::<PanelOrderStatus>().unwrap(),
            PanelOrderStatus::Ordered
        );
        assert!("bogus".parse::<PanelOrderStatus>().is_err());
    }

    /// Full round-trip against a real Dolt repo. Skipped if `dolt` is absent.
    #[test]
    fn panel_orders_roundtrip_when_dolt_available() {
        if find_on_path("dolt").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("lob-panel-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = PanelOrders::open(&root).expect("open");

        let id = store
            .create(
                "crossfader-v1",
                "/tmp/crossfader.dxf",
                Some("sendcutsend"),
                None,
            )
            .expect("create");
        assert!(id >= 0);

        let latest = store
            .latest("crossfader-v1")
            .expect("latest")
            .expect("present");
        assert_eq!(latest.module, "crossfader-v1");
        assert_eq!(latest.dxf_path, "/tmp/crossfader.dxf");
        assert_eq!(latest.vendor.as_deref(), Some("sendcutsend"));
        assert_eq!(latest.status, PanelOrderStatus::NotOrdered);

        store
            .mark_ordered("crossfader-v1", "sendcutsend", Some("TRK-12345"))
            .expect("mark ordered");

        let ordered = store
            .latest("crossfader-v1")
            .expect("latest")
            .expect("present");
        assert_eq!(ordered.status, PanelOrderStatus::Ordered);
        assert_eq!(ordered.tracking_ref.as_deref(), Some("TRK-12345"));

        let all = store.list("crossfader-v1").expect("list");
        assert_eq!(all.len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }
}
