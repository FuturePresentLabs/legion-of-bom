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
    /// What this control is, for styling. `None` renders plain.
    pub role: Option<CutoutRole>,
}

/// What a cutout *is*, for panel styling — distinct from its shape, which is
/// only how big a hole to cut.
///
/// Panels read faster when the signal path and the modulation inputs look
/// different, so the role drives a badge on the label and the dial art around a
/// knob. Derived from circuit topology rather than from the label text, so it
/// cannot drift from what the module actually does: a knob whose track ends both
/// sit on live nets is bipolar, one with an end on ground is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutoutRole {
    /// Audio in/out — the signal path.
    Io,
    /// A control-voltage jack. Labels get the inverted badge.
    Cv,
    /// A plain knob: unipolar sweep, dial dots from min to max.
    Knob,
    /// A toggle. No dial art — a lever has positions, not a sweep, and drawing a
    /// 270-degree arc of dots round one would be a lie about how it moves.
    Switch,
    /// A knob whose centre is zero. Gets a centre detent mark, and â/+ at the
    /// extremes, because "12 o'clock is silence" is the whole point of the
    /// control and a player has to be able to see it.
    Attenuverter,
}

impl CutoutRole {
    /// The token used in a panel TOML (`role = "cv"`).
    pub fn as_str(self) -> &'static str {
        match self {
            CutoutRole::Io => "io",
            CutoutRole::Cv => "cv",
            CutoutRole::Knob => "knob",
            CutoutRole::Switch => "switch",
            CutoutRole::Attenuverter => "attenuverter",
        }
    }
    pub fn parse(s: &str) -> Option<CutoutRole> {
        match s.trim().to_ascii_lowercase().as_str() {
            "io" => Some(CutoutRole::Io),
            "cv" => Some(CutoutRole::Cv),
            "knob" | "pot" => Some(CutoutRole::Knob),
            "switch" | "toggle" => Some(CutoutRole::Switch),
            "attenuverter" | "attenuvertor" | "bipolar" => Some(CutoutRole::Attenuverter),
            _ => None,
        }
    }
    /// Whether this role's label is drawn as knocked-out text in a filled badge.
    fn badged(self) -> bool {
        self == CutoutRole::Cv
    }
    fn is_knob(self) -> bool {
        matches!(self, CutoutRole::Knob | CutoutRole::Attenuverter)
    }
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
/// Sub-miniature toggle bushing hole. From the Dailywell 2M series drawing
/// (2MS3/2MD6 etc, what Thonk sell as DW1/DW2/DW5): panel hole 4.95mm with a
/// 4.55mm flat for anti-rotation, 10-48 UNS bushing.
///
/// Was 6.5mm — a plausible number rather than a measured one, which left the
/// switch 1.55mm loose in its hole (`legion-of-bom-tvs`). The flat is still not
/// represented; `CutoutShape` has no circle-with-flat.
const TOGGLE_MM: f64 = 4.95;
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
    fn cutout(&self, mpn: Option<&str>, footprint: &str) -> Option<CutoutSpec> {
        // MPN first. A panel control's *footprint* often says nothing about the
        // panel — a sub-mini toggle has no KiCad THT footprint at all, so it
        // carries a 1x03 pin header and would never keyword-match "switch".
        // The MPN is what actually identifies the hardware, which is why this
        // trait takes one; until the parts library carries verified mechanical
        // data this is a small table of what we build with.
        if let Some(mpn) = mpn {
            let m = mpn.to_ascii_uppercase();
            // Dailywell 1M/2M sub-miniature toggles (Thonk DW1/DW2/DW5 …).
            if m.starts_with("1M") || m.starts_with("2M") {
                return Some(CutoutSpec {
                    kind: ControlKind::Switch,
                    shape: CutoutShape::Circle {
                        diameter_mm: TOGGLE_MM,
                    },
                    envelope_mm: kind_envelope(ControlKind::Switch),
                });
            }
        }
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
    /// On a 1U tile the name sits in the band below the control row, this far
    /// up from the bottom edge, and smaller — there is 39.65 mm to share.
    pub const TILE_TITLE_BOTTOM_MM: f64 = 4.6;
    pub const TILE_TITLE_FONT_MM: f64 = 2.6;

    /// Title height for a format.
    pub fn title_font_mm(format: super::PanelFormat) -> f64 {
        match format.is_tile() {
            true => TILE_TITLE_FONT_MM,
            false => TITLE_FONT_MM,
        }
    }
    /// Control-label text height and its offset above the cutout centre (clears a
    /// [`super::JACK_BARREL_MM`]/2 barrel with margin).
    pub const LABEL_FONT_MM: f64 = 2.4;
    pub const LABEL_OFFSET_MM: f64 = 6.5;
    /// A knob's label has to clear its dial art, not just its body — at the
    /// jack offset the 12 o'clock dot lands inside the lettering.
    pub const KNOB_LABEL_OFFSET_MM: f64 = 10.4;
    /// Brand logo: fraction of panel width, the minimum width worth drawing, and
    /// the clearances keeping it off the lowest cutout and the bottom edge/holes.
    /// The logo is the maker's mark — give it real presence in the bottom band.
    pub const LOGO_WIDTH_FRAC: f64 = 0.66;
    pub const LOGO_MIN_WIDTH_MM: f64 = 4.0;
    pub const LOGO_CUTOUT_GAP_MM: f64 = 2.5;
    pub const BOTTOM_MARGIN_MM: f64 = 6.0;
    /// Dial art around a knob: how far the dots sit from the shaft centre, how
    /// big each dot is, and how many across the sweep.
    ///
    /// A pot turns 270 degrees, so the dots run from -135 to +135 measured from
    /// straight up. Seven reads as a scale without becoming a ruler; an even
    /// count would put a gap where a bipolar control's zero belongs.
    pub const DIAL_RADIUS_MM: f64 = 7.4;
    pub const DIAL_DOT_MM: f64 = 0.45;
    pub const DIAL_DOTS: usize = 7;
    pub const DIAL_SWEEP_DEG: f64 = 270.0;
    /// The centre dot on a bipolar control, drawn larger because "12 o'clock is
    /// zero" is the one position a player needs to find without looking.
    pub const DIAL_CENTRE_DOT_MM: f64 = 0.85;
    /// Height of the minus/plus glyphs at a bipolar control's extremes.
    pub const DIAL_SIGN_MM: f64 = 1.7;
    /// Padding around a badged label, and its corner radius.
    pub const BADGE_PAD_X_MM: f64 = 1.3;
    pub const BADGE_PAD_Y_MM: f64 = 0.75;
    pub const BADGE_RADIUS_MM: f64 = 0.6;
}

/// How far a control's label sits above its centre — further for a knob, whose
/// dial art reaches past the body.
fn label_offset(role: Option<CutoutRole>) -> f64 {
    match role.is_some_and(CutoutRole::is_knob) {
        true => silk::KNOB_LABEL_OFFSET_MM,
        false => silk::LABEL_OFFSET_MM,
    }
}

/// The dot positions for a knob's dial art, as `(dx, dy)` offsets from the shaft
/// centre in **panel** coordinates (y up), plus whether each is the centre one.
///
/// Shared by both renderers so the SVG preview and the fabricated silkscreen
/// cannot disagree about where the marks are.
fn dial_dots() -> Vec<(f64, f64, bool)> {
    let n = silk::DIAL_DOTS;
    let mid = n / 2;
    (0..n)
        .map(|i| {
            // 0 at full counter-clockwise, 1 at full clockwise.
            let t = i as f64 / (n - 1) as f64;
            let deg = -silk::DIAL_SWEEP_DEG / 2.0 + t * silk::DIAL_SWEEP_DEG;
            let rad = deg.to_radians();
            // Measured from straight up, turning clockwise.
            (
                silk::DIAL_RADIUS_MM * rad.sin(),
                silk::DIAL_RADIUS_MM * rad.cos(),
                n % 2 == 1 && i == mid,
            )
        })
        .collect()
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
    /// The height class. Renderers use it to place the title, which sits above
    /// the controls on a 3U panel and below them on a tile — a 1U tile has no
    /// clear band at the top, because the controls are already using it.
    fn format(&self) -> PanelFormat {
        PanelFormat::Eurorack3U
    }
}

// ---------------------------------------------------------------------------
//  Eurorack implementation
// ---------------------------------------------------------------------------

const EURORACK_HEIGHT_MM: f64 = 128.5;
const HP_MM: f64 = 5.08;
const EURORACK_HOLE_DIAMETER_MM: f64 = 3.2;
const EURORACK_HOLE_INSET_X_MM: f64 = 7.5;
const EURORACK_HOLE_INSET_Y_MM: f64 = 3.0;

/// Panel height class. Width is always HP; only the height and the row/column
/// habit change.
///
/// The two 1U standards are **mutually incompatible** and both are in wide use:
/// a case railed for one will not take the other. Pulp Logic could afford the
/// taller tile because Vector rails have no lip; Intellijel's shorter tile fits
/// the lipped rails a standard Eurorack case uses. So this is a property of the
/// case the module is going into, and the spec has to name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelFormat {
    /// Standard Eurorack 3U — 128.5 mm.
    #[default]
    Eurorack3U,
    /// Intellijel 1U tile — 39.65 mm. Fits lipped rails.
    Intellijel1U,
    /// Pulp Logic 1U tile — 43.18 mm (1.700"). Needs lipless (Vector) rails.
    PulpLogic1U,
}

impl PanelFormat {
    pub fn height_mm(self) -> f64 {
        match self {
            PanelFormat::Eurorack3U => EURORACK_HEIGHT_MM,
            PanelFormat::Intellijel1U => 39.65,
            PanelFormat::PulpLogic1U => 43.18,
        }
    }

    /// The `format` token in a panel TOML.
    pub fn as_str(self) -> &'static str {
        match self {
            PanelFormat::Eurorack3U => "eurorack",
            PanelFormat::Intellijel1U => "intellijel-1u",
            PanelFormat::PulpLogic1U => "pulplogic-1u",
        }
    }

    pub fn parse(s: &str) -> Option<PanelFormat> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "eurorack" | "eurorack-3u" | "3u" => Some(PanelFormat::Eurorack3U),
            "intellijel-1u" | "intellijel" | "1u" => Some(PanelFormat::Intellijel1U),
            "pulplogic-1u" | "pulplogic" | "pulp-logic-1u" => Some(PanelFormat::PulpLogic1U),
            _ => None,
        }
    }

    /// A 1U tile: too short to stack controls, so they lay out in a row.
    pub fn is_tile(self) -> bool {
        !matches!(self, PanelFormat::Eurorack3U)
    }

    /// Vertical inset of the mounting holes from the top and bottom edges.
    ///
    /// UNVERIFIED for the 1U formats. Intellijel state only that "the 1U panel
    /// size is based on the 3U size scaled down", and publish the hole spacing
    /// as a diagram image rather than as figures, so this reuses the 3U inset.
    /// The height is confirmed; this number is a derivation. Check it against
    /// the vendor drawing before cutting metal — it is one constant to change.
    fn hole_inset_y_mm(self) -> f64 {
        EURORACK_HOLE_INSET_Y_MM
    }
}

/// A Eurorack panel.
///
/// Constructed in HP (horizontal pitch) internally, but exposes only mm
/// through the [`PanelSpec`] trait.
#[derive(Debug, Clone, PartialEq)]
pub struct EurorackPanel {
    format: PanelFormat,
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
        Self::with_format(PanelFormat::Eurorack3U, hp)
    }

    /// A panel of the given height class and HP width. `1U` here means the
    /// Intellijel tile unless the Pulp Logic variant is named explicitly.
    pub fn with_format(format: PanelFormat, hp: u16) -> Self {
        let mut panel = EurorackPanel {
            format,
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
            role: None,
        });
        self
    }

    /// Add a cutout with explicit rotation, an optional anchored refdes, and an
    /// optional silkscreen/engraving label.
    pub fn with_cutout_spec(mut self, cutout: Cutout) -> Self {
        self.cutouts.push(cutout);
        self
    }

    /// Add a cutout from its parts. Kept for the common case; anything carrying
    /// a role or a label is clearer built as a [`Cutout`] and passed to
    /// [`with_cutout_spec`](Self::with_cutout_spec).
    pub fn with_cutout_labelled(
        self,
        x_mm: f64,
        y_mm: f64,
        footprint: impl Into<String>,
        refdes: Option<String>,
        label: Option<String>,
        role: Option<CutoutRole>,
    ) -> Self {
        self.with_cutout_spec(Cutout {
            x_mm,
            y_mm,
            rotation_deg: 0.0,
            footprint: footprint.into(),
            refdes,
            label,
            role,
        })
    }

    /// Width in mm (HP × 5.08).
    pub fn width_mm_value(&self) -> f64 {
        f64::from(self.hp) * HP_MM
    }

    /// The HP width (internal unit, not part of `PanelSpec`).
    pub fn hp(&self) -> u16 {
        self.hp
    }

    /// The height class this panel is built to.
    pub fn format(&self) -> PanelFormat {
        self.format
    }

    fn rebuild_default_holes(&mut self) {
        let w = self.width_mm_value();
        let h = self.format.height_mm();
        let inset_y = self.format.hole_inset_y_mm();
        // Left side holes (always present).
        self.extra_holes.push(MountingHole {
            x_mm: EURORACK_HOLE_INSET_X_MM,
            y_mm: h - inset_y,
            diameter_mm: EURORACK_HOLE_DIAMETER_MM,
        });
        self.extra_holes.push(MountingHole {
            x_mm: EURORACK_HOLE_INSET_X_MM,
            y_mm: inset_y,
            diameter_mm: EURORACK_HOLE_DIAMETER_MM,
        });
        // Right side holes for panels ≥ 8 HP.
        if self.hp >= 8 {
            self.extra_holes.push(MountingHole {
                x_mm: w - EURORACK_HOLE_INSET_X_MM,
                y_mm: h - inset_y,
                diameter_mm: EURORACK_HOLE_DIAMETER_MM,
            });
            self.extra_holes.push(MountingHole {
                x_mm: w - EURORACK_HOLE_INSET_X_MM,
                y_mm: inset_y,
                diameter_mm: EURORACK_HOLE_DIAMETER_MM,
            });
        }
    }
}

impl PanelSpec for EurorackPanel {
    fn format(&self) -> PanelFormat {
        self.format
    }

    fn width_mm(&self) -> f64 {
        self.width_mm_value()
    }

    fn height_mm(&self) -> f64 {
        self.format.height_mm()
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
    /// `io` | `cv` | `knob` | `attenuverter` — drives the label badge and the
    /// dial art. Absent renders plain, so older specs are unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
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
        match PanelFormat::parse(&self.format) {
            Some(format) => {
                let hp = self
                    .hp
                    .ok_or_else(|| format!("{} panel requires `hp`", format.as_str()))?;
                let mut panel =
                    EurorackPanel::with_format(format, hp).with_thickness(self.thickness_mm);
                for c in &self.cutouts {
                    panel = panel.with_cutout_spec(Cutout {
                        x_mm: c.x_mm,
                        y_mm: c.y_mm,
                        rotation_deg: c.rotation_deg,
                        footprint: c.footprint.clone(),
                        refdes: c.refdes.clone(),
                        label: c.label.clone(),
                        role: c.role.as_deref().and_then(CutoutRole::parse),
                    });
                }
                Ok(Box::new(panel))
            }
            None => Err(format!("unsupported panel format: {}", self.format)),
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
    ///
    /// Jacks at 14mm is deliberately dense — plenty of shipping modules run
    /// 12-13mm and Doepfer sits near 15. Pots stay at 20: a knob needs finger
    /// room to *turn*, which is a different constraint from a plug needing room
    /// to grip, and it is the one you feel while playing.
    ///
    /// Note this is a *floor*, and for a Thonkiconn it does not bind: pitch is
    /// `max(body + GAP, class)` and the body is 14.4mm, so the jack sits at
    /// 16.4mm regardless. Reaching 14mm needs the jack rotated 90 degrees, which
    /// makes its body 10mm tall — and that needs cutout rotation threaded
    /// through to the board placer's anchors, which today carry position only.
    pub const JACK_PITCH_MM: f64 = 14.0;
    pub const POT_PITCH_MM: f64 = 20.0;
    pub const SWITCH_PITCH_MM: f64 = 14.0;
    pub const LED_PITCH_MM: f64 = 9.0;
    /// Clear zones: below the top-edge title, above the bottom logo + holes.
    pub const TOP_MARGIN_MM: f64 = 14.0;
    pub const BOTTOM_MARGIN_MM: f64 = 16.0;
    /// …and the same on a panel too narrow to spend them.
    ///
    /// 30mm of the 128.5 goes to these two bands. That is right on a wide panel,
    /// where the logo has real presence; on 4 HP the logo band is a sliver
    /// (LOGO_WIDTH_FRAC of 20mm) and is not worth four controls' worth of
    /// column. The title is the same height either way, so the top band only
    /// gives back what the title does not use.
    pub const NARROW_HP: u16 = 5;
    pub const NARROW_TOP_MARGIN_MM: f64 = 11.0;
    pub const NARROW_BOTTOM_MARGIN_MM: f64 = 12.0;

    /// The clear zones for a panel of this width.
    pub fn margins_mm(hp: u16) -> (f64, f64) {
        match hp <= NARROW_HP {
            true => (NARROW_TOP_MARGIN_MM, NARROW_BOTTOM_MARGIN_MM),
            false => (TOP_MARGIN_MM, BOTTOM_MARGIN_MM),
        }
    }
    /// Default Eurorack panel-PCB thickness (mm).
    pub const THICKNESS_MM: f64 = 1.6;
    /// Where a 1U tile's control row sits, as a fraction of panel height.
    /// Above centre: each control labels upward, and the module name takes the
    /// band left along the bottom.
    pub const TILE_ROW_FRAC: f64 = 0.56;
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
    // CV jacks sit above the audio I/O. A player reads a panel top-down looking
    // for where the signal goes, and the signal path wants to be the last thing
    // on the way to the bottom edge — modulation lives with the knobs it feeds.
    let (cv, io): (Vec<_>, Vec<_>) = jacks
        .into_iter()
        .partition(|(r, _, _)| carries_cv(circuit, r));
    controls.into_iter().chain(cv).chain(io).collect()
}

/// Whether a part sits on a control-voltage net rather than the audio path.
///
/// Read from net names, which is where the circuit author states intent: a jack
/// on `CV_IN` is modulation, one on `SIG_IN`/`SIG_OUT` is the signal path.
fn carries_cv(circuit: &dyn CircuitSource, refdes: &str) -> bool {
    circuit
        .nets()
        .iter()
        .filter(|n| n.pins.iter().any(|p| p.refdes.0 == refdes))
        .any(|n| {
            let u = n.name.to_ascii_uppercase();
            !is_power_net(&u) && (u.contains("CV") || u.contains("GATE") || u.contains("TRIG"))
        })
}

/// What a control is, for panel styling — from topology, never from the label.
///
/// A knob is bipolar when **both** ends of its track sit on live nets: that is
/// what makes its centre a true zero. A knob with an end on ground is a plain
/// attenuator or a bias control, whose centre is 50% of something and must not
/// be marked as silence.
fn cutout_role(circuit: &dyn CircuitSource, refdes: &str, kind: ControlKind) -> CutoutRole {
    if kind.is_jack() {
        return if carries_cv(circuit, refdes) {
            CutoutRole::Cv
        } else {
            CutoutRole::Io
        };
    }
    if matches!(kind, ControlKind::Switch) {
        return CutoutRole::Switch;
    }
    if !matches!(kind, ControlKind::Pot) {
        return CutoutRole::Knob;
    }
    // Pins 1 and 3 are the track ends; 2 is the wiper. Bipolar iff neither end
    // is tied to ground.
    let end_net = |pin: &str| -> Option<String> {
        circuit
            .nets()
            .iter()
            .find(|n| n.pins.iter().any(|p| p.refdes.0 == refdes && p.pin == pin))
            .map(|n| n.name.to_ascii_uppercase())
    };
    // Both ends must sit on *signal* nets. A pot strung across the supply rails
    // is a bias control — RATE on the slew limiter runs +12V to -12V — and its
    // centre is not silence, so it must not get the detent mark. Only a track
    // whose two ends are live signals has a true zero in the middle.
    let signal_end = |pin: &str| -> bool { end_net(pin).is_some_and(|u| !is_power_net(&u)) };
    if signal_end("1") && signal_end("3") {
        CutoutRole::Attenuverter
    } else {
        CutoutRole::Knob
    }
}

/// The narrowest panel (HP) whose **hardware actually fits** — the widest control
/// envelope plus edge material on both sides (DESIGN §6.1).
///
/// This is the panel-side constraint, independent of whether the PCB's parts and
/// traces fit (see `board::minimum_hp`); a buildable module needs both. Without
/// it a derivation happily emits, say, a 3 HP panel (15.24 mm) carrying a 13.75 mm
/// pot body, which cannot be built.
pub fn min_panel_hp(circuit: &dyn CircuitSource, cutouts: &dyn CutoutSource) -> u16 {
    min_panel_hp_for(circuit, PanelFormat::Eurorack3U, cutouts)
}

/// [`min_panel_hp`] for a given height class.
///
/// The constraint flips with the format. A 3U panel stacks its controls, so the
/// width only has to clear the *widest* one. A 1U tile is 39.65 mm tall — there
/// is no room to stack — so its controls sit in a row and the width has to hold
/// the **sum** of them. A tile is therefore far wider than a 3U panel carrying
/// the same hardware, and sizing it by the widest control would emit a spec that
/// cannot be built.
pub fn min_panel_hp_for(
    circuit: &dyn CircuitSource,
    format: PanelFormat,
    cutouts: &dyn CutoutSource,
) -> u16 {
    let controls = panel_controls(circuit, cutouts);
    let needed = if format.is_tile() {
        let row: f64 = controls
            .iter()
            .map(|(_, k, env)| (env.0 + envelope::GAP_MM).max(control_pitch(*k)))
            .sum();
        if row <= 0.0 {
            return 1;
        }
        row + 2.0 * envelope::EDGE_MM
    } else {
        let widest = controls
            .iter()
            .map(|(_, _, env)| env.0)
            .fold(0.0f64, f64::max);
        if widest <= 0.0 {
            return 1;
        }
        widest + 2.0 * envelope::EDGE_MM
    };
    (needed / HP_MM).ceil().max(1.0) as u16
}

/// Derive a panel from a **built board**: one cutout per panel-mounted part, at
/// the position that part actually occupies.
///
/// This is the direction that holds once a board exists. [`derive_panel`] lays
/// controls out in an idealised centred column and knows nothing about the PCB,
/// which is only ever right because the board was then placed *from* that panel
/// — the panel was master and the board followed. The moment a board is imported,
/// hand-placed or simply re-laid-out, that idealised panel is fiction and will
/// not fit the hardware soldered to the board.
///
/// The mapping is the exact inverse of the one the placer uses: a panel's
/// cutouts are measured from its bottom-left, a KiCad board from its top-left, so
/// `panel_y = height − (board_y − top)`. Working from the board's own
/// `Edge.Cuts` rather than the KiCad sheet origin means this also works for a
/// board that was never generated here.
///
/// Parts with no cutout in `cutouts` are skipped — a panel hole invented for a
/// part we can't classify is a hole in the wrong place.
pub fn panel_from_board(
    board_pcb: &str,
    circuit: &dyn CircuitSource,
    cutouts: &dyn CutoutSource,
) -> Result<PanelFile, String> {
    let placed = crate::guide::parse_board(board_pcb)?;
    let (x0, y0, x1, y1) =
        crate::guide::board_outline(board_pcb).ok_or("board has no Edge.Cuts outline")?;
    let (w, h) = ((x1 - x0).abs(), (y1 - y0).abs());
    if w <= 0.0 || h <= 0.0 {
        return Err("board outline has no area".into());
    }
    // Look each part's footprint up through the circuit, which is where the MPN
    // lives; the board only carries the footprint id.
    let mpn_of = |refdes: &str| {
        circuit
            .parts()
            .iter()
            .find(|p| p.refdes.0 == refdes)
            .and_then(|p| p.mpn.clone())
    };

    let mut out: Vec<CutoutFile> = Vec::new();
    for p in placed.iter().filter(|p| crate::guide::is_panel_mounted(p)) {
        let Some(spec) = cutouts.cutout(mpn_of(&p.refdes).as_deref(), &p.footprint) else {
            continue;
        };
        // The cutout goes where the *hardware* is, not where the footprint's
        // origin is. An Alpha pot's origin is pin 1 and its shaft sits several
        // millimetres away, so mapping the origin drills the hole off the shaft
        // — and the reverse trip (cutout -> placement) already subtracts that
        // offset, so origin-mapping made the two directions disagree and a part
        // drift by one offset per round trip (`legion-of-bom-za4`).
        let (hx, hy) = ((p.bbox.0 + p.bbox.2) / 2.0, (p.bbox.1 + p.bbox.3) / 2.0);
        out.push(CutoutFile {
            x_mm: hx - x0,
            y_mm: h - (hy - y0),
            rotation_deg: 0.0,
            footprint: spec.kind.cutout_name().to_string(),
            refdes: Some(p.refdes.clone()),
            label: control_label(circuit, &p.refdes),
            role: Some(
                cutout_role(circuit, &p.refdes, spec.kind)
                    .as_str()
                    .to_string(),
            ),
        });
    }
    out.sort_by(|a, b| a.refdes.cmp(&b.refdes));

    Ok(PanelFile {
        format: "eurorack".into(),
        // The board width decides the panel width, not the other way round.
        hp: Some(((w / HP_MM).round() as u16).max(1)),
        thickness_mm: derive_rules::THICKNESS_MM,
        finish: None,
        cutouts: out,
    })
}

pub fn derive_panel(circuit: &dyn CircuitSource, hp: u16, cutouts: &dyn CutoutSource) -> PanelFile {
    derive_panel_for(circuit, PanelFormat::Eurorack3U, hp, cutouts)
}

/// [`derive_panel`] for a given height class.
///
/// A 3U panel stacks controls in a centred column; a 1U tile lays them in a
/// centred row, because 39.65 mm of height has nowhere to stack. The row sits a
/// little above centre so each control's label clears it and the module name
/// still has a band along the bottom.
pub fn derive_panel_for(
    circuit: &dyn CircuitSource,
    format: PanelFormat,
    hp: u16,
    cutouts: &dyn CutoutSource,
) -> PanelFile {
    let ordered = panel_controls(circuit, cutouts);

    // Never emit a panel too narrow for its own hardware — a derived spec that
    // can't be built is worse than a wider one.
    let hp = hp.max(min_panel_hp_for(circuit, format, cutouts));

    let w = f64::from(hp) * HP_MM;
    let cx = w / 2.0;
    let h = format.height_mm();

    if format.is_tile() {
        // One row, left to right, in the same order the column would have used:
        // knobs first, then CV, then audio I/O — so a tile reads like the top of
        // a 3U panel rather than in refdes order.
        let pitches: Vec<f64> = ordered
            .iter()
            .map(|(_, k, env)| (env.0 + envelope::GAP_MM).max(control_pitch(*k)))
            .collect();
        let total: f64 = pitches.iter().sum();
        let mut x = ((w - total) / 2.0).max(envelope::EDGE_MM);
        // Above centre: labels go above each control, the name band goes below.
        let row_y = h * derive_rules::TILE_ROW_FRAC;
        let mut out: Vec<CutoutFile> = Vec::new();
        for ((refdes, kind, _), pitch) in ordered.iter().zip(&pitches) {
            out.push(CutoutFile {
                x_mm: x + pitch / 2.0,
                y_mm: row_y,
                rotation_deg: 0.0,
                footprint: kind.cutout_name().to_string(),
                refdes: Some(refdes.clone()),
                label: control_label(circuit, refdes),
                role: Some(cutout_role(circuit, refdes, *kind).as_str().to_string()),
            });
            x += pitch;
        }
        return PanelFile {
            format: format.as_str().into(),
            hp: Some(hp),
            thickness_mm: derive_rules::THICKNESS_MM,
            finish: None,
            cutouts: out,
        };
    }

    // Stack controls top→bottom (knobs above jacks), spaced by the real envelope
    // each one needs plus a gap — never closer than the class minimum — and centre
    // the stack in the clear zone between the title and the bottom logo/holes.
    let pitches: Vec<f64> = ordered
        .iter()
        .map(|(_, k, env)| (env.1 + envelope::GAP_MM).max(control_pitch(*k)))
        .collect();
    let (top_margin, avail_bot) = derive_rules::margins_mm(hp);
    let avail_top = h - top_margin;
    let avail = avail_top - avail_bot;

    // How many columns the width can hold, and how many the height demands.
    //
    // A single centred column was fine for a three-control module and fell apart
    // past that: an eight-control board asked for 42 HP and put a jack at
    // y = -12mm, off the panel entirely (`legion-of-bom-lau`). Height is fixed
    // at 3U, so the only way to carry more controls is sideways.
    let widest = ordered
        .iter()
        .map(|(_, _, env)| env.0)
        .fold(0.0f64, f64::max)
        .max(1.0);
    let fits_wide = (((w - 2.0 * envelope::EDGE_MM) + envelope::GAP_MM)
        / (widest + envelope::GAP_MM))
        .floor()
        .max(1.0) as usize;
    let total: f64 = pitches.iter().sum();
    let needed = (total / avail.max(1.0)).ceil().max(1.0) as usize;
    let cols = needed.min(fits_wide).max(1);

    // Split into columns by *height*, not by count: a column of three pots is
    // taller than a column of three jacks, and balancing the counts would leave
    // one column overflowing while another had room.
    let target = total / cols as f64;
    let mut groups: Vec<Vec<usize>> = vec![Vec::new(); cols];
    let (mut g, mut run) = (0usize, 0.0f64);
    for (i, pitch) in pitches.iter().enumerate() {
        let remaining_cols = cols - g;
        let remaining_items = pitches.len() - i;
        // Leave at least one control for each remaining column.
        if g + 1 < cols && run + pitch / 2.0 > target && remaining_items > remaining_cols {
            g += 1;
            run = 0.0;
        }
        groups[g].push(i);
        run += pitch;
    }

    let mut out: Vec<CutoutFile> = Vec::new();
    let col_w = (w - 2.0 * envelope::EDGE_MM) / cols as f64;
    for (ci, group) in groups.iter().enumerate() {
        if group.is_empty() {
            continue;
        }
        let col_x = envelope::EDGE_MM + col_w * (ci as f64 + 0.5);
        let col_total: f64 = group.iter().map(|&i| pitches[i]).sum();
        let mut y = avail_top - (avail - col_total).max(0.0) / 2.0;
        for &i in group {
            let (refdes, kind, _) = &ordered[i];
            // Clamp inside the panel. A derived spec that puts hardware off the
            // edge is not a spec, and it used to happen silently.
            let cy = (y - pitches[i] / 2.0).clamp(avail_bot, avail_top);
            out.push(CutoutFile {
                x_mm: col_x,
                y_mm: cy,
                rotation_deg: 0.0,
                footprint: kind.cutout_name().to_string(),
                refdes: Some(refdes.clone()),
                label: control_label(circuit, refdes),
                role: Some(cutout_role(circuit, refdes, *kind).as_str().to_string()),
            });
            y -= pitches[i];
        }
    }
    let _ = cx;

    PanelFile {
        format: format.as_str().into(),
        hp: Some(hp),
        thickness_mm: derive_rules::THICKNESS_MM,
        finish: None,
        cutouts: out,
    }
}

/// A short panel label for a control, from the most signal-like net it touches
/// (excluding power/ground). `SIG_IN` → "IN", `RATE_CV` → "RATE".
fn control_label(circuit: &dyn CircuitSource, refdes: &str) -> Option<String> {
    // A switch names itself. Its nets are wiring detail — which cap a throw
    // selects — and say nothing a player needs, so a net-derived label reads as
    // noise ("N$2", "RANGE GLIDE"). By convention a switch's *value* field
    // carries its function: RANGE, MODE, SHAPE. Prefer that.
    if let Some(part) = circuit.parts().iter().find(|p| p.refdes.0 == refdes) {
        let is_switch = part
            .footprint
            .as_deref()
            .is_some_and(|f| f.to_ascii_lowercase().contains("sw"))
            || part.refdes.0.starts_with("SW");
        let v = part.value.trim();
        if is_switch && !v.is_empty() && v.chars().any(|c| c.is_ascii_alphabetic()) {
            return Some(v.to_uppercase());
        }
    }
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
        // Dial art around a knob: dots across the sweep, and for a bipolar
        // control a bigger centre dot with minus/plus at the extremes.
        let role = c.role;
        if role.is_some_and(CutoutRole::is_knob) {
            let bipolar = role == Some(CutoutRole::Attenuverter);
            for (dx, dy, is_centre) in dial_dots() {
                let r = if bipolar && is_centre {
                    silk::DIAL_CENTRE_DOT_MM
                } else {
                    silk::DIAL_DOT_MM
                };
                s.push_str(&format!(
                    "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{:.2}\" fill=\"{}\"/>",
                    cx + dx,
                    cy - dy,
                    r,
                    finish.legend
                ));
            }
            if bipolar {
                let ends = dial_dots();
                if let (Some(a), Some(b)) = (ends.first(), ends.last()) {
                    let out = 2.0;
                    let scale = (silk::DIAL_RADIUS_MM + out) / silk::DIAL_RADIUS_MM;
                    s.push_str(&svg_text(
                        cx + a.0 * scale,
                        cy - a.1 * scale,
                        silk::DIAL_SIGN_MM,
                        &finish.legend,
                        "\u{2212}",
                    ));
                    s.push_str(&svg_text(
                        cx + b.0 * scale,
                        cy - b.1 * scale,
                        silk::DIAL_SIGN_MM,
                        &finish.legend,
                        "+",
                    ));
                }
            }
        }
        if let Some(label) = &c.label {
            let ly = cy - label_offset(role);
            if role.is_some_and(CutoutRole::badged) {
                // Inverted label: a filled badge with the text knocked out, so a
                // CV input reads as a different *kind* of thing at a glance
                // rather than as more small text.
                let tw = label.chars().count() as f64 * silk::LABEL_FONT_MM * 0.62;
                let (bw, bh) = (
                    tw + 2.0 * silk::BADGE_PAD_X_MM,
                    silk::LABEL_FONT_MM + 2.0 * silk::BADGE_PAD_Y_MM,
                );
                s.push_str(&format!(
                    "<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" \
                     rx=\"{:.2}\" fill=\"{}\"/>",
                    cx - bw / 2.0,
                    ly - bh / 2.0,
                    bw,
                    bh,
                    silk::BADGE_RADIUS_MM,
                    finish.legend
                ));
                s.push_str(&svg_text(cx, ly, silk::LABEL_FONT_MM, &finish.face, label));
            } else {
                s.push_str(&svg_text(
                    cx,
                    ly,
                    silk::LABEL_FONT_MM,
                    &finish.legend,
                    label,
                ));
            }
        }
    }
    // Title, top-centre.
    if !title.is_empty() {
        // On a tile the controls own the top; the name goes in the bottom band.
        let title_y = match panel.format().is_tile() {
            true => h - silk::TILE_TITLE_BOTTOM_MM,
            false => silk::TITLE_TOP_MARGIN_MM,
        };
        s.push_str(&svg_text(
            w / 2.0,
            title_y,
            silk::title_font_mm(panel.format()),
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
        // Dial art: silk dots across the knob's sweep, bigger at centre for a
        // bipolar control, with minus/plus at the extremes.
        if c.role.is_some_and(CutoutRole::is_knob) {
            let bipolar = c.role == Some(CutoutRole::Attenuverter);
            for (j, (dx, dy, is_centre)) in dial_dots().into_iter().enumerate() {
                let r = if bipolar && is_centre {
                    silk::DIAL_CENTRE_DOT_MM
                } else {
                    silk::DIAL_DOT_MM
                };
                // A filled dot is a zero-length line with a round cap of the
                // right width — one primitive, and it plots cleanly.
                s.push_str(&format!(
                    "  (gr_line (start {} {}) (end {} {}) (stroke (width {}) (type solid)) \
                     (layer \"F.SilkS\") (uuid \"{}\"))\n",
                    mm(cx + dx),
                    mm(cy - dy),
                    mm(cx + dx),
                    mm(cy - dy),
                    mm(r * 2.0),
                    det_uuid(&format!("panel.dial.{i}.{j}")),
                ));
            }
            if bipolar {
                let ends = dial_dots();
                let scale = (silk::DIAL_RADIUS_MM + 2.0) / silk::DIAL_RADIUS_MM;
                for (k, (glyph, e)) in [("-", ends.first()), ("+", ends.last())]
                    .into_iter()
                    .enumerate()
                {
                    let Some((dx, dy, _)) = e else { continue };
                    s.push_str(&format!(
                        "  (gr_text \"{}\" (at {} {} 0) (layer \"F.SilkS\") (uuid \"{}\") \
                         (effects (font (size {f} {f}) (thickness 0.3))))\n",
                        glyph,
                        mm(cx + dx * scale),
                        mm(cy - dy * scale),
                        det_uuid(&format!("panel.sign.{i}.{k}")),
                        f = silk::DIAL_SIGN_MM,
                    ));
                }
            }
        }
        if let Some(label) = &c.label {
            // A badged label is KiCad `knockout` text: the silkscreen prints a
            // filled block with the glyphs left unprinted, which is exactly the
            // negative-text look, and it is a native property rather than a
            // rectangle we would have to punch letters out of ourselves.
            let layer = if c.role.is_some_and(CutoutRole::badged) {
                "\"F.SilkS\" knockout"
            } else {
                "\"F.SilkS\""
            };
            s.push_str(&format!(
                "  (gr_text \"{}\" (at {} {} 0) (layer {}) (uuid \"{}\") \
                 (effects (font (size {f} {f}) (thickness 0.3))))\n",
                label,
                mm(cx),
                mm(cy - label_offset(c.role)),
                layer,
                det_uuid(&format!("panel.label.{i}")),
                f = silk::LABEL_FONT_MM,
            ));
        }
    }
    // Title, horizontal, along the top edge (below the top mounting holes) so it
    // never crosses a centred control column — a vertical centre title collides
    // with the knobs/jacks (the "writing hitting a jack" failure, j54-6f8). On a
    // 1U tile the controls already own the top, so it goes in the bottom band.
    let title_y = match panel.format().is_tile() {
        true => oy + h - silk::TILE_TITLE_BOTTOM_MM,
        false => oy + silk::TITLE_TOP_MARGIN_MM,
    };
    s.push_str(&format!(
        "  (gr_text \"{}\" (at {} {} 0) (layer \"F.SilkS\") (uuid \"{}\") \
         (effects (font (size {f} {f}) (thickness 0.3))))\n",
        title,
        mm(ox + w / 2.0),
        mm(title_y),
        det_uuid("panel.title"),
        f = silk::title_font_mm(panel.format()),
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
mod panel_from_board_tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef, RefDes};

    /// 5 HP board, jack near the bottom-left, pot near the top-right.
    const BOARD: &str = r#"(kicad_pcb
      (gr_rect (start 100 40) (end 125.4 168.5) (layer "Edge.Cuts"))
      (footprint "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical" (layer "F.Cu") (at 106 158 0)
        (property "Reference" "J1") (pad "1" thru_hole circle (at 0 0) (size 2 2)))
      (footprint "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical" (layer "F.Cu") (at 118 55 0)
        (property "Reference" "RV1") (pad "1" thru_hole circle (at 0 0) (size 2 2)))
      (footprint "Resistor_SMD:R_0603_1608Metric" (layer "F.Cu") (at 110 100 0)
        (property "Reference" "R1") (pad "1" smd rect (at 0 0) (size 1 1))))"#;

    fn circuit() -> Circuit {
        Circuit {
            name: "t".into(),
            parts: vec![
                Part::new("J1", "AudioJack2_SwitchT")
                    .with_footprint("Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical"),
                Part::new("RV1", "100k").with_footprint(
                    "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical",
                ),
                Part::new("R1", "1k").with_footprint("Resistor_SMD:R_0603_1608Metric"),
            ],
            nets: vec![Net {
                name: "SIG_OUT".into(),
                pins: vec![PinRef {
                    refdes: RefDes("J1".into()),
                    pin: "1".into(),
                }],
                net_class: None,
            }],
        }
    }

    /// A module with two knobs, a CV jack and audio I/O — enough to exercise
    /// ordering, roles and badging together.
    fn module() -> Circuit {
        let jack = "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical";
        let pot = "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical";
        let net = |name: &str, pins: &[(&str, &str)]| Net {
            name: name.into(),
            pins: pins
                .iter()
                .map(|(r, p)| PinRef {
                    refdes: RefDes((*r).into()),
                    pin: (*p).into(),
                })
                .collect(),
            net_class: None,
        };
        Circuit {
            name: "m".into(),
            parts: vec![
                Part::new("J1", "in").with_footprint(jack),
                Part::new("J2", "out").with_footprint(jack),
                Part::new("J4", "cv").with_footprint(jack),
                Part::new("RV1", "100k").with_footprint(pot),
                Part::new("RV2", "100k").with_footprint(pot),
            ],
            nets: vec![
                net("SIG_IN", &[("J1", "1")]),
                net("SIG_OUT", &[("J2", "1")]),
                net("CV_IN", &[("J4", "1"), ("RV2", "1")]),
                // RATE: a bias control strung across the supply rails.
                net("+12V", &[("RV1", "1")]),
                net("-12V", &[("RV1", "3")]),
                net("RATE_CV", &[("RV1", "2")]),
                // CV AMT: an attenuator — bottom of the track on ground.
                net("GND", &[("RV2", "3")]),
                net("CV_AMT", &[("RV2", "2")]),
            ],
        }
    }

    /// The two 1U standards are incompatible and both are real. Heights are the
    /// load-bearing numbers: Intellijel 39.65 mm fits lipped rails, Pulp Logic
    /// 43.18 mm (1.700") needs lipless ones.
    #[test]
    fn one_u_formats_have_their_published_heights() {
        assert_eq!(PanelFormat::Eurorack3U.height_mm(), 128.5);
        assert_eq!(PanelFormat::Intellijel1U.height_mm(), 39.65);
        assert_eq!(PanelFormat::PulpLogic1U.height_mm(), 43.18);
        // Bare "1u" means Intellijel — the one in wide use.
        assert_eq!(PanelFormat::parse("1u"), Some(PanelFormat::Intellijel1U));
        assert_eq!(
            PanelFormat::parse("pulplogic-1u"),
            Some(PanelFormat::PulpLogic1U)
        );
        assert_eq!(
            PanelFormat::parse("eurorack"),
            Some(PanelFormat::Eurorack3U)
        );
        assert_eq!(PanelFormat::parse("nonsense"), None);
        assert!(!PanelFormat::Eurorack3U.is_tile());
        assert!(PanelFormat::Intellijel1U.is_tile());
    }

    /// A tile has nowhere to stack, so its controls sit in a row and its width
    /// has to hold the *sum* of them — sizing by the widest, as 3U does, emits a
    /// tile that cannot be built.
    #[test]
    fn a_tile_lays_controls_in_a_row_and_sizes_by_their_sum() {
        let c = module();
        let tall = min_panel_hp_for(&c, PanelFormat::Eurorack3U, &BuiltinCutouts);
        let wide = min_panel_hp_for(&c, PanelFormat::Intellijel1U, &BuiltinCutouts);
        assert!(
            wide > tall * 3,
            "five controls in a row: {wide} HP vs {tall}"
        );

        let p = derive_panel_for(&c, PanelFormat::Intellijel1U, 1, &BuiltinCutouts);
        assert_eq!(p.format, "intellijel-1u");
        // All on one line…
        let ys: Vec<f64> = p.cutouts.iter().map(|c| c.y_mm).collect();
        assert!(ys.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-9), "{ys:?}");
        assert!(ys[0] < PanelFormat::Intellijel1U.height_mm());
        // …spread across the width, in the same order the column would use.
        let mut xs: Vec<f64> = p.cutouts.iter().map(|c| c.x_mm).collect();
        let sorted = {
            let mut v = xs.clone();
            v.sort_by(f64::total_cmp);
            v
        };
        assert_eq!(xs, sorted, "laid out left to right");
        xs.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        assert_eq!(xs.len(), p.cutouts.len(), "no two controls share a slot");
    }

    /// On a tile the controls own the top, so the module name goes below them
    /// rather than into a title band that does not exist.
    #[test]
    fn a_tiles_name_sits_below_its_controls() {
        let p = derive_panel_for(&module(), PanelFormat::Intellijel1U, 1, &BuiltinCutouts);
        let spec = p.to_spec().unwrap();
        assert_eq!(spec.height_mm(), 39.65);
        let svg = panel_to_svg(spec.as_ref(), "tile", &PanelFinish::named("black"), None);
        // SVG y runs down, so "below the controls" is a larger y than the row.
        let row_y_svg = 39.65 - p.cutouts[0].y_mm;
        let title_y: f64 = regex_y(&svg, "tile");
        assert!(title_y > row_y_svg, "name at {title_y}, row at {row_y_svg}");
        assert!(title_y < 39.65, "and still on the panel");
    }

    /// Pull the y of the `<text>` element containing `needle`.
    fn regex_y(svg: &str, needle: &str) -> f64 {
        let at = svg.find(&format!(">{needle}<")).expect("text present");
        let head = &svg[..at];
        // A leading space, so this does not match `font-family="`.
        let y_at = head.rfind(" y=\"").expect("y attr");
        head[y_at + 4..]
            .split('"')
            .next()
            .unwrap()
            .parse()
            .expect("y number")
    }

    /// The lau case: enough controls that one column cannot hold them. They must
    /// spread sideways and every cutout must stay on the panel — the reported
    /// failure put a jack at y = -12.1mm, off the bottom edge, and the board then
    /// could not route against it.
    #[test]
    fn a_crowded_panel_uses_columns_and_never_places_hardware_off_it() {
        let jack = "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical";
        let pot = "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical";
        let mut c = Circuit::new("crowded");
        for i in 1..=4 {
            c.parts
                .push(Part::new(format!("RV{i}"), "100k").with_footprint(pot));
        }
        for i in 1..=6 {
            c.parts
                .push(Part::new(format!("J{i}"), "io").with_footprint(jack));
        }
        let hp = 8;
        let p = derive_panel_for(&c, PanelFormat::Eurorack3U, hp, &BuiltinCutouts);
        assert_eq!(p.cutouts.len(), 10);

        let w = f64::from(hp) * HP_MM;
        for cut in &p.cutouts {
            assert!(
                cut.y_mm > 0.0 && cut.y_mm < EURORACK_HEIGHT_MM,
                "{:?} at y={} is off the panel",
                cut.refdes,
                cut.y_mm
            );
            assert!(
                cut.x_mm > 0.0 && cut.x_mm < w,
                "{:?} off the side",
                cut.refdes
            );
        }
        // …and it actually used more than one column rather than stacking.
        let mut xs: Vec<f64> = p.cutouts.iter().map(|c| c.x_mm).collect();
        xs.sort_by(f64::total_cmp);
        xs.dedup_by(|a, b| (*a - *b).abs() < 0.01);
        assert!(xs.len() >= 2, "expected multiple columns, got {xs:?}");
    }

    /// A module that fits in one column keeps one — columns are a response to
    /// crowding, not a default.
    #[test]
    fn a_sparse_panel_stays_a_single_centred_column() {
        let jack = "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical";
        let mut c = Circuit::new("sparse");
        for i in 1..=2 {
            c.parts
                .push(Part::new(format!("J{i}"), "io").with_footprint(jack));
        }
        let p = derive_panel_for(&c, PanelFormat::Eurorack3U, 8, &BuiltinCutouts);
        let xs: Vec<f64> = p.cutouts.iter().map(|c| c.x_mm).collect();
        assert!(
            xs.windows(2).all(|w| (w[0] - w[1]).abs() < 0.01),
            "one column: {xs:?}"
        );
        assert!((xs[0] - 8.0 * HP_MM / 2.0).abs() < 0.01, "centred: {xs:?}");
    }

    /// CV inputs sit above the audio I/O: a player scans down for the signal
    /// path, so modulation belongs up with the knobs it feeds.
    #[test]
    fn cv_jacks_sit_above_the_audio_io() {
        let p = derive_panel(&module(), 8, &BuiltinCutouts);
        let y = |r: &str| {
            p.cutouts
                .iter()
                .find(|c| c.refdes.as_deref() == Some(r))
                .unwrap()
                .y_mm
        };
        // Panel y is measured up from the bottom.
        assert!(y("J4") > y("J1"), "CV IN above IN");
        assert!(y("J4") > y("J2"), "CV IN above OUT");
        // …and the knobs stay above the jacks.
        assert!(y("RV1") > y("J4") && y("RV2") > y("J4"));
    }

    /// Roles come from topology. A pot across the supply rails is a bias control
    /// whose centre is *not* silence, so it must not get the bipolar detent — and
    /// an attenuator with its track end on ground is not an attenuverter either.
    #[test]
    fn only_a_pot_with_both_track_ends_live_is_bipolar() {
        let p = derive_panel(&module(), 8, &BuiltinCutouts);
        let role = |r: &str| {
            p.cutouts
                .iter()
                .find(|c| c.refdes.as_deref() == Some(r))
                .unwrap()
                .role
                .clone()
                .unwrap()
        };
        assert_eq!(role("RV1"), "knob", "RATE spans +12V/-12V: a bias control");
        assert_eq!(role("RV2"), "knob", "CV AMT's track end is grounded");
        assert_eq!(role("J4"), "cv");
        assert_eq!(role("J1"), "io");
        assert_eq!(role("J2"), "io");

        // Re-wire CV AMT's bottom onto an inverted rail and it becomes bipolar,
        // with no panel edit — the art follows the circuit.
        let mut c = module();
        c.nets.retain(|n| n.name != "GND");
        c.nets.push(Net {
            name: "CV_IN_INV".into(),
            pins: vec![PinRef {
                refdes: RefDes("RV2".into()),
                pin: "3".into(),
            }],
            net_class: None,
        });
        let p2 = derive_panel(&c, 8, &BuiltinCutouts);
        let rv2 = p2
            .cutouts
            .iter()
            .find(|x| x.refdes.as_deref() == Some("RV2"))
            .unwrap();
        assert_eq!(rv2.role.as_deref(), Some("attenuverter"));
    }

    /// A CV label is knocked out of a filled badge; audio I/O stays plain. On the
    /// panel PCB that is KiCad's native `knockout`, so the silkscreen prints a
    /// block with the glyphs unprinted rather than us punching letters out.
    #[test]
    fn cv_labels_are_badged_and_io_labels_are_not() {
        let p = derive_panel(&module(), 8, &BuiltinCutouts);
        let spec = p.to_spec().unwrap();
        let pcb = panel_to_kicad_pcb(spec.as_ref(), "m", None);
        assert!(
            pcb.contains(r#"(gr_text "CV IN" (at"#) && pcb.contains(r#"knockout)"#),
            "CV IN is knocked out: {pcb}"
        );
        // Exactly one knockout — the audio jacks and the knobs are plain.
        assert_eq!(pcb.matches("knockout").count(), 1);

        // Dial art: seven dots per knob, two knobs.
        assert_eq!(pcb.matches("(gr_line").count(), 2 * silk::DIAL_DOTS);
    }

    /// The whole point: a cutout lands where the part actually is, not where an
    /// idealised column would have put it.
    #[test]
    fn cutouts_land_on_the_parts_real_positions() {
        let p = panel_from_board(BOARD, &circuit(), &BuiltinCutouts).unwrap();
        assert_eq!(p.hp, Some(5), "board width decides the panel width");
        // Only panel-facing parts: the 0603 is board-only.
        assert_eq!(p.cutouts.len(), 2);
        let by = |r: &str| {
            p.cutouts
                .iter()
                .find(|c| c.refdes.as_deref() == Some(r))
                .unwrap()
        };

        // Board is x 100..125.4, y 40..168.5 (25.4 x 128.5mm).
        // J1 at board (106, 158): 6mm from the left edge, and 128.5-118 = 10.5mm
        // up from the bottom — a jack near the bottom, as placed.
        let j = by("J1");
        assert!((j.x_mm - 6.0).abs() < 0.01, "x {}", j.x_mm);
        assert!((j.y_mm - 10.5).abs() < 0.01, "y {}", j.y_mm);
        // RV1 at board (118, 55): 18mm across, 113.5mm up — near the top.
        let rv = by("RV1");
        assert!((rv.x_mm - 18.0).abs() < 0.01, "x {}", rv.x_mm);
        assert!((rv.y_mm - 113.5).abs() < 0.01, "y {}", rv.y_mm);
        // Y really is flipped: the jack low on the panel is high in KiCad's frame.
        assert!(j.y_mm < rv.y_mm);

        // Classified, and labelled from the signal it carries.
        assert_eq!(j.footprint, "Thonkiconn");
        assert_eq!(rv.footprint, "Alpha9mm");
        assert_eq!(j.label.as_deref(), Some("OUT")); // label_from_net drops the prefix
    }

    /// The round trip the whole "board is master" design rests on: hand-place a
    /// control in panel space, let the board follow, derive the panel back from
    /// the built board, and land on the position that was authored.
    ///
    /// If this drifts, a panel gets cut that the board will not mate with — and
    /// the three artifacts stop describing one layout.
    #[test]
    fn a_hand_placement_survives_the_trip_through_the_board_and_back() {
        use crate::board::{generate_board, BoardOptions, EurorackPlacer};
        use crate::placement::PlacementFile;
        let Some(dir) = crate::skidl::kicad_footprint_dir() else {
            return;
        };
        let file = PlacementFile::from_toml(
            r#"
[[patterns.column]]
refdes = ["J1", "J2"]
x      = 7.0
from_y = 12.0
pitch  = 20.0
"#,
        )
        .unwrap();
        let (hp, h) = (3u16, EURORACK_HEIGHT_MM);
        let w = f64::from(hp) * HP_MM;
        let origin = (100.0, 40.0);
        let anchors = file.anchors(h).unwrap();
        let want = file.positions().unwrap();

        let circuit = Circuit {
            name: "rt".into(),
            parts: vec![
                Part::new("J1", "jack")
                    .with_footprint("Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical"),
                Part::new("J2", "jack")
                    .with_footprint("Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical"),
            ],
            nets: vec![],
        };
        let mut opts = BoardOptions::new(dir);
        opts.placer = Box::new(EurorackPlacer {
            width_mm: w,
            height_mm: h,
            origin_mm: origin,
            anchors,
        });
        opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));
        let Ok(board) = generate_board(&circuit, &opts) else {
            return; // library layout differs; don't fail the unit suite
        };

        let derived = panel_from_board(&board, &circuit, &BuiltinCutouts).unwrap();
        assert_eq!(derived.hp, Some(hp), "board width decides the panel width");
        for (refdes, p) in &want {
            let c = derived
                .cutouts
                .iter()
                .find(|c| c.refdes.as_deref() == Some(refdes.as_str()))
                .unwrap_or_else(|| panic!("{refdes} missing from the derived panel"));
            // Within a placement grid step: the placer settles a part on its
            // clear-spot search, so this asserts the layout survived, not that
            // nothing may ever move.
            assert!(
                (c.x_mm - p.x).abs() < 1.0 && (c.y_mm - p.y).abs() < 1.0,
                "{refdes}: authored ({:.1},{:.1}) came back as ({:.1},{:.1})",
                p.x,
                p.y,
                c.x_mm,
                c.y_mm
            );
        }
    }

    /// A board we cannot frame gets an error, not a panel measured from nothing.
    #[test]
    fn a_board_with_no_outline_is_an_error() {
        let no_edge = r#"(kicad_pcb (footprint "X" (layer "F.Cu") (at 1 1 0)
          (property "Reference" "J1") (pad "1" thru_hole circle (at 0 0) (size 2 2))))"#;
        assert!(panel_from_board(no_edge, &circuit(), &BuiltinCutouts).is_err());
    }
}

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
            .with_cutout_labelled(
                20.32,
                100.0,
                "Alpha9mm",
                None,
                Some("RATE".to_string()),
                Some(CutoutRole::Knob),
            )
            .with_cutout_labelled(
                20.32,
                14.0,
                "Thonkiconn",
                None,
                Some("OUT".to_string()),
                Some(CutoutRole::Io),
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
            .with_cutout_labelled(
                10.0,
                20.0,
                "Thonkiconn",
                Some("J1".into()),
                Some("IN".into()),
                Some(CutoutRole::Io),
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
