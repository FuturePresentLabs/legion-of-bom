//! Build guide — a step-by-step visual assembly guide (DESIGN.md 7.6/7.8).
//!
//! Parses a generated `.kicad_pcb` for component positions (decoupled from board
//! generation, like [`drc`](crate::drc)/[`fab`](crate::fab)), groups the parts
//! into low-profile-first build steps, and renders a self-contained HTML page.
//! Each step shows a top-down board diagram with *that* step's parts highlighted
//! — the "red boxes over all the resistors, then the caps" a human follows —
//! plus a sorted parts list and polarity/pin-1 callouts.
//!
//! Values and part types come from the circuit; positions come from the board.
//! The diagram is a schematic top-down (accurate boxes, no render/camera
//! dependency); overlaying on a photoreal render is a later refinement.

use std::collections::{BTreeMap, HashSet};

use base64::Engine;

use crate::pdf::{self, Font, Page, Paint};
use crate::sexpr::Sexpr;
use crate::source::CircuitSource;
use crate::theme;

/// A placed component: board-space centre + pad bounding box (mm).
#[derive(Debug, Clone)]
pub struct PlacedPart {
    pub refdes: String,
    pub value: String,
    pub footprint: String,
    pub cx: f64,
    pub cy: f64,
    pub bbox: (f64, f64, f64, f64),
    pub back: bool,
    /// Whether this part mounts through the board (has ≥1 through-hole pad) — a
    /// hand-soldered THT part — versus surface-mount. Drives kit-type detection
    /// and which per-kind assembly-copy variant a step shows.
    pub through_hole: bool,
    /// Position of the reference pad (pin 1) — where the polarity marker sits.
    pub pin1: Option<(f64, f64)>,
    /// Polarity/orientation reference, for polarised parts only.
    pub polarity: Option<Polarity>,
}

/// A polarity/orientation reference: what to align to the board's silkscreen
/// mark, resolved per part (a ceramic cap has none, an electrolytic has `Plus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    /// Diode/LED cathode — the banded / flat end.
    Cathode,
    /// Positive terminal of a polarised (electrolytic/tantalum) capacitor.
    Plus,
    /// Pin 1 of an IC / connector / transistor (notch / dot / flat).
    Pin1,
}

impl Polarity {
    /// Short marker drawn at the reference pad on the diagram.
    fn label(self) -> &'static str {
        match self {
            Polarity::Cathode => "K",
            Polarity::Plus => "+",
            Polarity::Pin1 => "1",
        }
    }
    /// The assembly caution, phrased against the board silkscreen.
    fn caution(self) -> &'static str {
        match self {
            Polarity::Cathode => {
                "Polarity: match each diode/LED cathode (K — banded/flat end) to the silkscreen band."
            }
            Polarity::Plus => {
                "Polarity: match each capacitor's + terminal to the silkscreen + / stripe."
            }
            Polarity::Pin1 => "Orientation: align pin 1 (notch/dot) to the silkscreen pin-1 mark.",
        }
    }
    /// The orientation cue as a parts-table cell — what to look for on the part
    /// and what to line it up with, short enough to sit in a column.
    ///
    /// This replaces repeating [`Self::caution`] as a banner on every step: the
    /// same sentence on four sheets in a row teaches the reader to skip banners,
    /// and orientation belongs on the row of the part it applies to.
    fn cue(self) -> &'static str {
        match self {
            Polarity::Cathode => "banded end (K) → silkscreen band",
            Polarity::Plus => "+ / long lead → silkscreen +",
            Polarity::Pin1 => "notch or dot → pin-1 mark",
        }
    }
}

/// Resolve a part's polarity from its reference designator and footprint. A
/// ceramic/film cap is unpolarised; an electrolytic/tantalum one is `Plus`.
pub(crate) fn detect_polarity(refdes: &str, footprint: &str) -> Option<Polarity> {
    let name = footprint
        .rsplit(':')
        .next()
        .unwrap_or(footprint)
        .to_ascii_uppercase();
    match prefix_of(refdes) {
        "D" => Some(Polarity::Cathode),
        "U" | "Q" | "J" => Some(Polarity::Pin1),
        "C" if name.starts_with("CP")
            || name.contains("ELEC")
            || name.contains("TANTAL")
            || name.contains("POLAR") =>
        {
            Some(Polarity::Plus)
        }
        _ => None,
    }
}

/// Whether a build targets through-hole (hand assembly — the default for DIY
/// kits), surface-mount, or a mix of both. Drives the guide's framing and which
/// per-kind assembly-copy variant a step shows. Auto-detected from the board's
/// pad types, overridable via `lob guide --kit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KitType {
    /// Through-hole — leaded parts, hand-soldered. The DIY-kit default.
    Tht,
    /// Surface-mount — usually machine-assembled, but hand-solderable.
    Smd,
    /// A mix of through-hole and surface-mount parts.
    Mixed,
}

impl KitType {
    /// Parse a `--kit` argument (`tht`/`smd`/`mixed`, with common synonyms).
    pub fn parse(s: &str) -> Option<KitType> {
        match s.trim().to_ascii_lowercase().as_str() {
            "tht" | "through-hole" | "thru" | "th" => Some(KitType::Tht),
            "smd" | "smt" | "surface" => Some(KitType::Smd),
            "mixed" | "both" => Some(KitType::Mixed),
            _ => None,
        }
    }

    /// The framing label shown at the top of the guide.
    ///
    /// `Mixed` reads as "through-hole kit, SMD pre-assembled" rather than
    /// "mixed build" because that is what the builder is holding: the fab
    /// reflows the surface-mount side and the kit is the through-hole work.
    /// Calling it a mixed build implies hand-soldering 0603s that are already
    /// on the board. A board where the SMD genuinely is hand-work turns
    /// [`GuideOptions::include_smd`] back on, which also brings back its steps.
    fn label(self) -> &'static str {
        match self {
            KitType::Tht => "Through-hole kit",
            KitType::Smd => "Surface-mount board",
            KitType::Mixed => "Through-hole kit · SMD pre-assembled",
        }
    }
}

/// Auto-detect the kit type from placed parts' pad types: any through-hole *and*
/// any surface-mount part → `Mixed`; otherwise whichever is present; an
/// unclassifiable / empty board defaults to `Tht` (the DIY-kit assumption).
fn detect_kit(parts: &[PlacedPart]) -> KitType {
    let (mut tht, mut smd) = (false, false);
    for p in parts {
        if p.through_hole {
            tht = true;
        } else {
            smd = true;
        }
    }
    match (tht, smd) {
        (true, true) => KitType::Mixed,
        (false, true) => KitType::Smd,
        _ => KitType::Tht,
    }
}

/// A part-specific assembly callout, sourced from the parts library and attached
/// to a step: the reference designators it applies to (grouped when they share the
/// same steps) plus the ordered note text — e.g. `RV1, RV2` → "snap off the
/// locating tab if unused". Augments the generic per-kind copy.
#[derive(Debug, Clone)]
pub struct PartNote {
    pub refs: Vec<String>,
    pub steps: Vec<String>,
}

/// One build step: a group of same-kind parts placed together.
#[derive(Debug, Clone)]
pub struct BuildStep {
    pub title: String,
    pub parts: Vec<PlacedPart>,
    /// How to physically place this kind of part — the per-kind assembly copy
    /// (THT or SMD variant, chosen from the step's parts). This is the *default*;
    /// a per-part note from the parts library (`part_notes`) augments it.
    pub assembly: Option<String>,
    /// Part-specific notes for this step, resolved from the parts library by MPN
    /// via [`BuildGuide::attach_part_notes`]. Empty until attached.
    pub part_notes: Vec<PartNote>,
    /// A polarity / orientation warning to show, if the parts are polarised.
    pub caution: Option<String>,
}

/// The whole guide: the board outline + the ordered steps.
#[derive(Debug, Clone)]
pub struct BuildGuide {
    pub name: String,
    pub outline: (f64, f64, f64, f64),
    pub steps: Vec<BuildStep>,
    /// Through-hole / SMD / mixed — auto-detected, overridable. Frames the guide.
    pub kit: KitType,
    /// Per-circuit build copy from the repo manifest (5uj.5), set by the CLI via
    /// [`BuildGuide::set_build_copy`]. `brand` fronts the masthead; `intro`/`tools`/
    /// `kit_cautions` render a "Before you build" section above the steps.
    pub brand: Option<String>,
    pub intro: Option<String>,
    pub tools: Vec<String>,
    pub kit_cautions: Vec<String>,
}

impl BuildGuide {
    /// Attach the per-circuit build copy (from the circuits-repo manifest). Kept
    /// off [`build_guide`] so the core stays free of the project model — the CLI
    /// resolves the manifest and calls this, mirroring [`Self::attach_part_notes`].
    pub fn set_build_copy(
        &mut self,
        brand: Option<String>,
        intro: Option<String>,
        tools: Vec<String>,
        cautions: Vec<String>,
    ) {
        self.brand = brand;
        self.intro = intro;
        self.tools = tools;
        self.kit_cautions = cautions;
    }
}

/// A photorealistic board render (PNG bytes + pixel size) for the guide diagram —
/// produced by [`fab::render_board_png`](crate::fab::render_board_png) (an
/// unpopulated top-down `pcb render`). The guide maps board-mm into it
/// analytically, so no pixels are decoded.
pub struct BoardPng<'a> {
    pub png: &'a [u8],
    pub width: u32,
    pub height: u32,
}

/// The fraction of extra room `pcb render` leaves around the board's longest
/// dimension — measured, not documented. See [`render_scale`].
const RENDER_FIT_MARGIN: f64 = 1.023;

/// Board-mm → image-px scale for a `pcb render` frame of `w_px` × `h_px` showing a
/// board of `w_mm` × `h_mm`, centred.
///
/// KiCad's orthographic camera frames the board's **longest dimension against the
/// image height**, with ~2.3% headroom. The board's centre always lands on the
/// image centre, and the visible height in mm depends only on the board — not on
/// the frame's aspect at all.
///
/// This was previously modelled as the board's bounding *circle* fitted to the
/// frame's smaller dimension. That is wrong, and wrong by an amount that varies
/// with the board: measured against real renders it under-predicted by 2.8% on a
/// tall 5 HP Eurorack panel and 9.1% on a small landscape test board. Since the
/// overlay is drawn at this scale, too small a scale shrinks every highlight box
/// *and* drags it toward the image centre, which is why highlights sat inside
/// their silkscreen outlines. Against six boards spanning aspect 0.32–2.67 and
/// five frame sizes, this form holds to ±0.35%.
///
/// If a KiCad upgrade moves the camera, re-measure rather than nudging the
/// number: `scripts/measure_render_scale.py <render.png> <board.kicad_pcb>`
/// reports the true scale by finding the board in a real render.
fn render_scale(w_px: f64, h_px: f64, w_mm: f64, h_mm: f64) -> f64 {
    let _ = w_px; // the frame's width does not enter the framing
    let longest = w_mm.max(h_mm);
    if longest <= 0.0 || h_px <= 0.0 {
        return 1.0;
    }
    h_px / (RENDER_FIT_MARGIN * longest)
}

/// A build-step kind, in low-profile-first assembly order (DESIGN 7.8). Polarity
/// cautions are derived per part (see [`detect_polarity`]), not fixed per kind —
/// a ceramic-cap step shows no caution, an electrolytic one does.
struct Kind {
    prefix: &'static str,
    title: &'static str,
    /// How to place this kind, through-hole (the default) — the per-kind copy.
    note_tht: &'static str,
    /// The surface-mount variant, when the technique genuinely differs; `None`
    /// falls back to `note_tht` (parts that are rarely SMD, e.g. panel hardware).
    note_smd: Option<&'static str>,
}

// Order matters — this *is* the build sequence: low-profile → tall, so a part
// never blocks soldering access to a shorter neighbour, and panel hardware
// (pots, jacks) goes last since it mates to the front panel. Within this order,
// build_guide does the BACK side first (SMD + power header) then the front.
// `prefix_of` yields the whole letter prefix, so `RV`/`SW` never collide with
// `R`/`S`. Each kind carries its own assembly copy — THT-first, since few DIY
// kits are surface-mount; a per-part note from the parts library overrides it.
const KINDS: &[Kind] = &[
    Kind {
        prefix: "R",
        title: "Resistors",
        note_tht: "Bend each resistor's leads to the pad spacing, seat it flat against the board, \
                   and splay the leads on the back to hold it. Solder both pads, then flush-cut the \
                   excess lead. Match each value by its color bands (shown in the sort list).",
        note_smd: Some(
            "Tack one pad, set the body square, solder the opposite pad, then reflow the first.",
        ),
    },
    Kind {
        prefix: "D",
        title: "Diodes & LEDs",
        note_tht: "Match the cathode (banded / flat) end to the silkscreen before soldering. Bend \
                   the leads, seat, splay to hold, solder, then clip.",
        note_smd: Some(
            "Match the cathode mark to the silkscreen; tack one end, align, solder the other.",
        ),
    },
    Kind {
        prefix: "Q",
        title: "Transistors",
        note_tht: "Match the flat / pin-1 to the outline. Leave the body a few mm proud of the \
                   board, and solder each lead briefly so the part doesn't overheat.",
        note_smd: None,
    },
    Kind {
        prefix: "U",
        title: "ICs & sockets",
        note_tht: "Fit a socket first — don't solder the IC directly. Match the notch / pin-1 to \
                   the silkscreen, solder two diagonal corner pins, check it sits flat, then solder \
                   the rest. Insert the IC last, notch matched.",
        note_smd: Some(
            "Match pin-1 to the mark. Tack one corner, align, solder the diagonal corner, then run \
             the rows and check for solder bridges.",
        ),
    },
    Kind {
        prefix: "C",
        title: "Capacitors",
        note_tht: "Ceramic and film caps go in either way. Electrolytics are polarised — match the \
                   + terminal to the silkscreen (the longer lead is +). Seat flat, solder, clip.",
        note_smd: Some(
            "Ceramics are unpolarised; match a tantalum's + to the mark. Tack, align, solder the \
             far pad.",
        ),
    },
    Kind {
        prefix: "SW",
        title: "Switches",
        note_tht: "Seat the switch fully flush against the board before soldering so it lines up \
                   with the panel cut-out.",
        note_smd: None,
    },
    Kind {
        prefix: "RV",
        title: "Potentiometers & trimmers",
        note_tht: "Mount the pot through the panel and tighten its nut before soldering, so it \
                   self-aligns. Some pots have a small locating tab — snap it off if your panel has \
                   no matching hole.",
        note_smd: None,
    },
    Kind {
        prefix: "J",
        title: "Connectors, jacks & headers",
        note_tht: "Fit these last. Mount panel jacks through the panel and tighten before soldering \
                   so everything lines up; seat headers square to the board.",
        note_smd: None,
    },
];

/// The per-kind assembly note for a step, choosing the SMD variant only when the
/// whole group is surface-mount (else the THT default).
fn kind_note(kind: &Kind, parts: &[PlacedPart]) -> String {
    let all_smd = !parts.is_empty() && parts.iter().all(|p| !p.through_hole);
    match (all_smd, kind.note_smd) {
        (true, Some(smd)) => smd.to_string(),
        _ => kind.note_tht.to_string(),
    }
}

/// Build the guide from a circuit (values, types) and its generated board
/// (positions). Parts default to the front; back parts are noted per step.
pub fn build_guide(circuit: &dyn CircuitSource, board_pcb: &str) -> Result<BuildGuide, String> {
    build_guide_with(circuit, board_pcb, GuideOptions::default())
}

/// [`build_guide`], with control over what the guide covers.
pub fn build_guide_with(
    circuit: &dyn CircuitSource,
    board_pcb: &str,
    opts: GuideOptions,
) -> Result<BuildGuide, String> {
    let placed = parse_board(board_pcb)?;
    let values: BTreeMap<&str, &str> = circuit
        .parts()
        .iter()
        .map(|p| (p.refdes.0.as_str(), p.value.as_str()))
        .collect();

    // Attach the circuit value + resolve polarity per part.
    let parts: Vec<PlacedPart> = placed
        .into_iter()
        .map(|mut p| {
            if let Some(v) = values.get(p.refdes.as_str()) {
                p.value = v.to_string();
            }
            p.polarity = detect_polarity(&p.refdes, &p.footprint);
            p
        })
        .collect();
    Ok(guide_from_parts_with(
        circuit.name(),
        parts,
        board_outline(board_pcb).unwrap_or((0.0, 0.0, 10.0, 10.0)),
        opts,
    ))
}

/// Build the ordered steps from already-placed parts.
///
/// Shared by the two ways a board reaches us: one we laid out (parsed from its
/// `.kicad_pcb`) and one that was imported, where position and side come from a
/// pick-and-place file and there is no footprint library behind them. The
/// sequencing is a property of the parts, not of where they were read from.
/// Copy for the seating step: how to seat panel hardware, followed by the
/// per-kind notes for the kinds actually present.
///
/// Those notes carry the detail that matters (a pot's locating tab, a jack's
/// nut and washer order) and would otherwise be lost by grouping every panel
/// part into one step.
fn seat_copy(parts: &[PlacedPart]) -> String {
    let mut copy = String::from(
        "Seat every pot, jack and switch in its holes — but solder nothing yet. Push each one \
         fully down against the board and leave it loose.",
    );
    for kind in KINDS {
        let group: Vec<PlacedPart> = parts
            .iter()
            .filter(|p| prefix_of(&p.refdes) == kind.prefix)
            .cloned()
            .collect();
        if group.is_empty() {
            continue;
        }
        copy.push_str("\n\n");
        copy.push_str(&kind_note(kind, &group));
    }
    copy
}

/// What to put in a build guide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GuideOptions {
    /// Include surface-mount parts as build steps.
    ///
    /// Off by default: an SMD board is assembled by the fab house, so the
    /// builder's guide is about the through-hole parts they actually fit. On a
    /// board where the SMD *is* hand-work, turn it back on.
    pub include_smd: bool,
}

/// The power header — fitted first because it is on the back, and once the
/// front is populated the board will not sit flat to solder it.
fn is_power_header(p: &PlacedPart) -> bool {
    let f = p.footprint.to_ascii_uppercase();
    // Both spellings of a 2x5 shrouded header appear in the wild: KiCad writes
    // `PinHeader_2x05`, fab packages write `2x5`.
    [
        "EURO", "IDC", "SHROUD", "POWER", "2X5", "2X05", "10P", "16P",
    ]
    .iter()
    .any(|k| f.contains(k))
}

/// Hardware that mates to the front panel: it is seated, then aligned by the
/// panel itself, and only soldered once the nuts are tight.
pub fn is_panel_mounted(p: &PlacedPart) -> bool {
    let f = p.footprint.to_ascii_uppercase();
    let prefix = prefix_of(&p.refdes);
    if is_power_header(p) {
        return false;
    }
    // `LED1` as well as `D1`: an indicator shines through the panel either way.
    matches!(prefix, "RV" | "SW" | "J" | "LED")
        || (prefix == "D" && f.contains("LED"))
        || ["THONKICONN", "JACK", "POT", "ENCODER"]
            .iter()
            .any(|k| f.contains(k))
}

pub fn guide_from_parts(
    name: &str,
    parts: Vec<PlacedPart>,
    outline: (f64, f64, f64, f64),
) -> BuildGuide {
    guide_from_parts_with(name, parts, outline, GuideOptions::default())
}

/// [`guide_from_parts`], with control over what the guide covers.
///
/// The order is the one a builder actually works in, which is not the order the
/// parts appear on the board:
///
/// 1. the power header, on the back, while the board still sits flat;
/// 2. everything else on the back, then the front, low-profile first;
/// 3. panel hardware seated but *not* soldered;
/// 4. the panel fitted and its nuts tightened, which aligns that hardware;
/// 5. only then, soldering it.
///
/// Soldering a jack before the panel is on is how a panel ends up not fitting.
pub fn guide_from_parts_with(
    name: &str,
    mut parts: Vec<PlacedPart>,
    outline: (f64, f64, f64, f64),
    opts: GuideOptions,
) -> BuildGuide {
    parts.sort_by_key(|p| refdes_key(&p.refdes));
    // Read the kit type from *every* placed part, before the surface-mount ones
    // are filtered out of the steps. A builder holding a fab-reflowed board
    // should be told the SMD is meant to be there — detecting after the filter
    // reports a plain through-hole kit and leaves them wondering.
    let kit = detect_kit(&parts);
    if !opts.include_smd {
        parts.retain(|p| p.through_hole);
    }

    // Group into ordered steps by side then kind: the BACK side first (mostly SMD
    // + the power header on our boards), then the front — each side low-profile →
    // tall (KINDS order). Anything unmatched becomes a per-side "remaining" step
    // so nothing is silently dropped.
    let mut steps = Vec::new();
    let mut used = vec![false; parts.len()];

    // The power header goes in first, whichever side it is on: it is the one part
    // that must be soldered while the board still lies flat. Split by side, back
    // first — a step's diagram is one face, so a step that mixes faces cannot be
    // drawn correctly (see [`step_is_back`]).
    for back in [true, false] {
        let power = take_group(&parts, &mut used, |p| is_power_header(p) && p.back == back);
        if power.is_empty() {
            continue;
        }
        let where_it_sits = if back {
            "It mounts on the back, and once the front is populated the board will not sit \
             flat to solder it."
        } else {
            "Once anything else stands proud of the board it will not sit flat to solder it."
        };
        steps.push(BuildStep {
            assembly: Some(format!(
                "Fit the power header first, before anything else stands proud of the board. \
                 {where_it_sits} Check the -12 V stripe against the silkscreen: a reversed \
                 header is the one mistake that damages the module."
            )),
            part_notes: Vec::new(),
            caution: step_caution(&power),
            title: "Power header".to_string(),
            parts: power,
        });
    }

    for back in [true, false] {
        let side_has = parts.iter().any(|p| p.back == back);
        if !side_has {
            continue;
        }
        for kind in KINDS {
            // Panel hardware is held back: it is fitted with the panel on, not
            // in board order.
            let group = take_group(&parts, &mut used, |p| {
                p.back == back && prefix_of(&p.refdes) == kind.prefix && !is_panel_mounted(p)
            });
            if group.is_empty() {
                continue;
            }
            steps.push(BuildStep {
                assembly: Some(kind_note(kind, &group)),
                part_notes: Vec::new(),
                caution: step_caution(&group),
                title: kind.title.to_string(),
                parts: group,
            });
        }
        let remaining = take_group(&parts, &mut used, |p| {
            p.back == back && !is_panel_mounted(p)
        });
        if !remaining.is_empty() {
            steps.push(BuildStep {
                assembly: None,
                part_notes: Vec::new(),
                caution: step_caution(&remaining),
                title: "Remaining parts".to_string(),
                parts: remaining,
            });
        }
    }

    // Seat → fit the panel → tighten → solder. The panel is the jig that aligns
    // every jack and pot; soldering first is how a panel ends up not fitting.
    let panel_parts = take_group(&parts, &mut used, is_panel_mounted);
    if !panel_parts.is_empty() {
        steps.push(BuildStep {
            assembly: Some(seat_copy(&panel_parts)),
            part_notes: Vec::new(),
            caution: Some(
                "Do not solder these until the panel is on. A jack soldered square to the \
                 board, but not to the panel, will hold the panel off at an angle."
                    .to_string(),
            ),
            title: "Seat the panel hardware".to_string(),
            parts: panel_parts.clone(),
        });
        steps.push(BuildStep {
            assembly: Some(
                "Drop the front panel over the seated hardware and start every nut by hand: \
                 the jack nuts, the pot nuts, and any washers that go under them. Tighten them \
                 down snug. This pulls each part square to the panel rather than to the board, \
                 which is what makes the finished module line up."
                    .to_string(),
            ),
            part_notes: Vec::new(),
            caution: None,
            title: "Fit the panel and tighten the nuts".to_string(),
            parts: Vec::new(),
        });
        steps.push(BuildStep {
            assembly: Some(
                "Now solder the panel hardware, with the panel still bolted on. Work around \
                 the board rather than finishing one part at a time, so nothing is pulled out \
                 of alignment by heat. Then check every nut is still tight."
                    .to_string(),
            ),
            part_notes: Vec::new(),
            caution: None,
            title: "Solder the panel hardware".to_string(),
            parts: panel_parts,
        });
    }

    BuildGuide {
        name: name.to_string(),
        outline,
        kit,
        brand: None,
        intro: None,
        tools: Vec::new(),
        kit_cautions: Vec::new(),
        steps,
    }
}

impl BuildGuide {
    /// Attach part-specific assembly notes to the steps they belong to. `notes`
    /// maps a reference designator to its ordered note text — the CLI builds it
    /// from [`PartsLibrary::resolve_circuit`](crate::parts::PartsLibrary::resolve_circuit)
    /// (per-MPN library records) so [`build_guide`] itself stays free of the parts
    /// library. Parts in a step that share the same note collapse into one callout.
    pub fn attach_part_notes(&mut self, notes: &BTreeMap<String, Vec<String>>) {
        for step in &mut self.steps {
            let mut grouped: Vec<PartNote> = Vec::new();
            for p in &step.parts {
                let Some(steps) = notes.get(&p.refdes).filter(|s| !s.is_empty()) else {
                    continue;
                };
                match grouped.iter_mut().find(|g| &g.steps == steps) {
                    Some(g) => g.refs.push(p.refdes.clone()),
                    None => grouped.push(PartNote {
                        refs: vec![p.refdes.clone()],
                        steps: steps.clone(),
                    }),
                }
            }
            step.part_notes = grouped;
        }
    }
}

/// Collect — and mark used — every not-yet-used part matching `pred`, preserving
/// the pre-sorted refdes order.
fn take_group(
    parts: &[PlacedPart],
    used: &mut [bool],
    pred: impl Fn(&PlacedPart) -> bool,
) -> Vec<PlacedPart> {
    let mut group = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if !used[i] && pred(p) {
            used[i] = true;
            group.push(p.clone());
        }
    }
    group
}

/// Mirror a placed part's X about `axis` (the board centre), for drawing on the
/// bottom-side render — `pcb render --side bottom` flips the board left↔right. The
/// bbox's left/right edges swap; Y is unchanged. We mirror *coordinates* (not the
/// SVG group), so overlay text stays upright and the refdes reads normally.
fn mirror_part_x(p: &PlacedPart, axis: f64) -> PlacedPart {
    let mx = |x: f64| 2.0 * axis - x;
    let (bx0, by0, bx1, by1) = p.bbox;
    PlacedPart {
        cx: mx(p.cx),
        bbox: (mx(bx1), by0, mx(bx0), by1),
        pin1: p.pin1.map(|(x, y)| (mx(x), y)),
        ..p.clone()
    }
}

/// Which face a step is drawn on: `true` for the back, so it renders on the
/// bottom-side plot (parts mirrored) and is flagged for the builder to flip.
///
/// Steps are built one face at a time, so this is normally unanimous. It takes a
/// majority rather than requiring one, because the failure mode of the old
/// all-or-nothing test was silent and wrong: a step with a single front part
/// among back ones fell through to the top render, and every back part in it was
/// then drawn unmirrored — highlights on the wrong side of the board, with
/// nothing saying so.
fn step_is_back(step: &BuildStep) -> bool {
    let back = step.parts.iter().filter(|p| p.back).count();
    back * 2 > step.parts.len()
}

/// A step's polarity caution — from its first polarised part (parts in a
/// kind-step share a polarity), or `None` if nothing in the step is polarised.
fn step_caution(parts: &[PlacedPart]) -> Option<String> {
    parts
        .iter()
        .find_map(|p| p.polarity)
        .map(|pol| pol.caution().to_string())
}

/// The leading letters of a refdes (`R12` → `R`, `SW1` → `SW`).
fn prefix_of(refdes: &str) -> &str {
    let end = refdes
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(refdes.len());
    &refdes[..end]
}

/// Sort key: prefix then numeric index (`R2` before `R10`).
fn refdes_key(refdes: &str) -> (String, u64) {
    let p = prefix_of(refdes);
    let n = refdes[p.len()..].parse().unwrap_or(u64::MAX);
    (p.to_string(), n)
}

/// Quarter turns in `deg`, normalised to 0..=3.
fn quarter_turns(deg: f64) -> i64 {
    (((deg / 90.0).round() as i64) % 4 + 4) % 4
}

/// Rotate a footprint-local point by a footprint orientation, in **KiCad's** sense:
/// a `(at x y 90)` footprint maps a local `(x, y)` to `(y, -x)` (KiCad's Y axis
/// points down). Matches `board::rotate_rect`, which places the pads in the first
/// place — the two must agree or the highlight drifts off the part.
fn rotate_kicad((x, y): (f64, f64), deg: f64) -> (f64, f64) {
    match quarter_turns(deg) {
        1 => (y, -x),
        2 => (-x, -y),
        3 => (-y, x),
        _ => (x, y),
    }
}

/// Parse footprints from a `.kicad_pcb`: refdes, centre, pad bounding box, side.
pub fn parse_board(board_pcb: &str) -> Result<Vec<PlacedPart>, String> {
    let root = Sexpr::parse(board_pcb)?;
    let mut parts = Vec::new();
    for fp in root.get_all("footprint") {
        let at = fp.get("at");
        let (fx, fy) = (
            at.and_then(|a| a.nth_atom(1)).and_then(f).unwrap_or(0.0),
            at.and_then(|a| a.nth_atom(2)).and_then(f).unwrap_or(0.0),
        );
        // A footprint's rotation is the third atom of its `(at …)`. Pad positions
        // are stored *un*-rotated (KiCad applies the footprint's orientation when it
        // draws), so the highlight box must apply it too — otherwise every rotated
        // part (a 90° pot, the power header) gets a box of the wrong shape in the
        // wrong place.
        let frot = at.and_then(|a| a.nth_atom(3)).and_then(f).unwrap_or(0.0);
        let refdes = fp
            .get_all("property")
            .into_iter()
            .find(|p| p.nth_atom(1) == Some("Reference"))
            .and_then(|p| p.nth_atom(2))
            .unwrap_or_default()
            .to_string();
        if refdes.is_empty() {
            continue;
        }
        let back = fp.get("layer").and_then(|l| l.nth_atom(1)) == Some("B.Cu");
        let footprint = fp.nth_atom(1).unwrap_or_default().to_string();

        // Pad bounding box, in board coordinates (rotation-0 grid placement), and
        // the position of pad 1 (the polarity/pin-1 reference).
        let mut bb = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        let mut pin1 = None;
        let mut through_hole = false;
        for pad in fp.get_all("pad") {
            // Pad type is the atom after the pad number: `(pad "1" thru_hole …)`.
            if pad
                .nth_atom(2)
                .is_some_and(|t| t == "thru_hole" || t == "np_thru_hole")
            {
                through_hole = true;
            }
            let pat = pad.get("at");
            let (px, py) = (
                pat.and_then(|a| a.nth_atom(1)).and_then(f).unwrap_or(0.0),
                pat.and_then(|a| a.nth_atom(2)).and_then(f).unwrap_or(0.0),
            );
            let size = pad.get("size");
            let (pw, ph) = (
                size.and_then(|s| s.nth_atom(1)).and_then(f).unwrap_or(0.5),
                size.and_then(|s| s.nth_atom(2)).and_then(f).unwrap_or(0.5),
            );
            // Rotate the pad about the footprint origin, then translate. A 90° turn
            // also swaps the pad's own width/height.
            let (rx, ry) = rotate_kicad((px, py), frot);
            let (pw, ph) = if quarter_turns(frot) % 2 != 0 {
                (ph, pw)
            } else {
                (pw, ph)
            };
            let (x, y) = (fx + rx, fy + ry);
            bb.0 = bb.0.min(x - pw / 2.0);
            bb.1 = bb.1.min(y - ph / 2.0);
            bb.2 = bb.2.max(x + pw / 2.0);
            bb.3 = bb.3.max(y + ph / 2.0);
            if pad.nth_atom(1) == Some("1") {
                pin1 = Some((x, y));
            }
        }
        if !bb.0.is_finite() {
            bb = (fx - 0.5, fy - 0.5, fx + 0.5, fy + 0.5);
        }
        // Grow the box to the footprint's courtyard — its declared body extent,
        // which is what the silkscreen outline traces on the board.
        //
        // The pad box alone is not the part. A 3.5 mm jack's pads span 2.1 mm
        // across while its body is 9 mm, so a pad-box highlight covered under a
        // third of the outline the builder is looking at, and read as "not
        // matching the silkscreen". Union, not replacement: a chip resistor's
        // pads reach slightly outside its courtyard, and the builder needs to see
        // the pads the part lands on either way.
        if let Some(cy) = courtyard_bbox(fp, (fx, fy), frot) {
            bb = (
                bb.0.min(cy.0),
                bb.1.min(cy.1),
                bb.2.max(cy.2),
                bb.3.max(cy.3),
            );
        }
        parts.push(PlacedPart {
            refdes,
            value: String::new(),
            footprint,
            cx: fx,
            cy: fy,
            bbox: bb,
            back,
            through_hole,
            pin1,
            polarity: None,
        });
    }
    Ok(parts)
}

/// The footprint's courtyard as a board-coordinate box, rotated and translated to
/// where the part is placed. `None` when the footprint declares no courtyard.
///
/// The courtyard (`*.CrtYd`) is the footprint's own statement of how much board
/// its body occupies, and it is what the silkscreen outline follows — so it is
/// what a highlight has to match.
fn courtyard_bbox(fp: &Sexpr, origin: (f64, f64), rot_deg: f64) -> Option<(f64, f64, f64, f64)> {
    let (fx, fy) = origin;
    let mut bb = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for kind in ["fp_line", "fp_rect", "fp_poly", "fp_circle", "fp_arc"] {
        for g in fp.get_all(kind) {
            let on_courtyard = g
                .get("layer")
                .and_then(|l| l.nth_atom(1))
                .is_some_and(|l| l.ends_with(".CrtYd"));
            if !on_courtyard {
                continue;
            }
            for point in ["start", "end", "center", "mid"] {
                if let Some(p) = g.get(point) {
                    let (Some(x), Some(y)) = (p.nth_atom(1).and_then(f), p.nth_atom(2).and_then(f))
                    else {
                        continue;
                    };
                    let (rx, ry) = rotate_kicad((x, y), rot_deg);
                    bb.0 = bb.0.min(fx + rx);
                    bb.1 = bb.1.min(fy + ry);
                    bb.2 = bb.2.max(fx + rx);
                    bb.3 = bb.3.max(fy + ry);
                }
            }
        }
    }
    bb.0.is_finite().then_some(bb)
}

/// The board outline from the `Edge.Cuts` rectangle, if present.
pub fn board_outline(board_pcb: &str) -> Option<(f64, f64, f64, f64)> {
    let root = Sexpr::parse(board_pcb).ok()?;
    let rect = root
        .get_all("gr_rect")
        .into_iter()
        .find(|r| r.get("layer").and_then(|l| l.nth_atom(1)) == Some("Edge.Cuts"))?;
    let start = rect.get("start")?;
    let end = rect.get("end")?;
    Some((
        f(start.nth_atom(1)?)?,
        f(start.nth_atom(2)?)?,
        f(end.nth_atom(1)?)?,
        f(end.nth_atom(2)?)?,
    ))
}

fn f(s: &str) -> Option<f64> {
    s.parse().ok()
}

/// Render the guide as a self-contained HTML page (inline CSS): a prep/sort sheet
/// then one photoreal board diagram per step, the step's parts highlighted. `top`
/// / `bottom` are unpopulated `pcb render`s ([`BoardPng`]); a back-side step is
/// drawn on the `bottom` render (parts mirrored) so it matches the board in hand.
/// Falls back to a schematic top-down when no render is available.
pub fn guide_to_html(
    guide: &BuildGuide,
    top: Option<BoardPng>,
    bottom: Option<BoardPng>,
) -> String {
    let total = guide.steps.len();
    let total_parts: usize = guide.steps.iter().map(|s| s.parts.len()).sum();
    let any_back = guide.steps.iter().any(step_is_back);
    let cxmm = (guide.outline.0 + guide.outline.2) / 2.0;
    let sheets = total + 1;
    let mut body = String::new();

    // ---- Sheet 1: what this is, what you need, and the pull-and-sort list.
    body.push_str("<section class=\"sheet\">");
    let sub = format!(
        "Low-profile parts first, tall parts last{} — so nothing blocks the iron.",
        if any_back { ", back side first" } else { "" },
    );
    let eyebrow = match &guide.brand {
        Some(b) => format!("{b} · Build guide"),
        None => "Build guide".to_string(),
    };
    body.push_str(&theme::masthead(
        &eyebrow,
        &guide.name,
        &sub,
        &[
            guide.kit.label().to_string(),
            format!("{total} steps"),
            format!("{total_parts} parts"),
            format!("{sheets} sheets"),
        ],
    ));

    // Per-circuit build copy (5uj.5): kit intro, tools, kit-level cautions.
    if guide.intro.is_some() || !guide.tools.is_empty() || !guide.kit_cautions.is_empty() {
        body.push_str("<section class=\"kit-copy\">");
        if let Some(intro) = &guide.intro {
            body.push_str(&format!("<p class=\"intro\">{}</p>", esc(intro)));
        }
        for c in &guide.kit_cautions {
            body.push_str(&format!("<p class=\"caution\">⚠ {}</p>", esc(c)));
        }
        if !guide.tools.is_empty() {
            let tools = guide
                .tools
                .iter()
                .map(|t| format!("<li>{}</li>", esc(t)))
                .collect::<String>();
            body.push_str(&format!("<h2>Tools</h2><ul class=\"tools\">{tools}</ul>"));
        }
        body.push_str("</section>");
    }

    // The board itself, both sides — so the builder knows which way round it goes
    // before the first step tells them to flip it.
    let overviews: Vec<(&'static str, Diagram)> = [("Front", &top), ("Back", &bottom)]
        .into_iter()
        .filter_map(|(label, r)| {
            r.as_ref()
                .map(|bp| (label, board_overview_svg(bp, guide.outline)))
        })
        .collect();
    if !overviews.is_empty() {
        let n = overviews.len();
        let figs: String = overviews
            .iter()
            .map(|(label, d)| {
                let (w, _h) = overview_fit(d.aspect, n);
                format!(
                    "<figure class=\"overview\" style=\"width:{w:.1}mm\">{}\
                     <figcaption class=\"mono\">{label}</figcaption></figure>",
                    d.svg,
                )
            })
            .collect();
        body.push_str(&format!("<div class=\"overviews\">{figs}</div>"));
    }

    // Pull & sort list — one row per pile you actually make on the bench, which
    // is (value, package): a 0603 47k and a 1206 47k are two piles, not one.
    // Deduped across steps: a part that is seated in one step and soldered in
    // another is still one part to find, and listing it twice would have the
    // builder counting out two.
    body.push_str(
        "<h2 class=\"sec\">Pull &amp; sort your parts</h2>\
         <p class=\"sec-sub\">Tick each off as you find it. Listed in build order — \
         the step that needs it is on the right.</p>\
         <table class=\"ptab prep\"><thead><tr><th class=\"h-chk\"></th><th>Qty</th>\
         <th>Value</th><th>Package</th><th>Reference designators</th><th>First used</th>\
         </tr></thead><tbody>",
    );
    let mut listed: HashSet<String> = HashSet::new();
    for (i, step) in guide.steps.iter().enumerate() {
        let side = if step_is_back(step) { " · back" } else { "" };
        for row in part_rows(&step.parts) {
            if !listed.insert(format!("{}\u{1}{}", row.value, row.package)) {
                continue;
            }
            body.push_str(&format!(
                "<tr><td class=\"c-chk\"><span class=\"chk\"></span></td>\
                 <td class=\"c-qty mono\">{qty}×</td>\
                 <td class=\"c-val\">{val}{swatch}</td>\
                 <td class=\"c-pkg mono\">{pkg}</td>\
                 <td class=\"c-ref mono\">{refs}</td>\
                 <td class=\"c-step\">{no}. {title}{side}</td></tr>",
                qty = row.refs.len(),
                val = esc(&row.value),
                swatch = row_swatch(&row),
                pkg = esc(&row.package),
                refs = esc(&row.refs.join("  ")),
                no = i + 1,
                title = esc(&step.title),
            ));
        }
    }
    // The nuts and washers that arrive with the panel parts and appear in no
    // netlist. Listed with the parts they serve, because that is how you find
    // out one is missing before the panel refuses to go on.
    for hw in hardware_rows(&guide.steps) {
        body.push_str(&format!(
            "<tr class=\"hw\"><td class=\"c-chk\"><span class=\"chk\"></span></td>\
             <td class=\"c-qty mono\">{qty}×</td>\
             <td class=\"c-val\">{name}</td>\
             <td class=\"c-pkg\">hardware</td>\
             <td class=\"c-ref mono\">{refs}</td>\
             <td class=\"c-step\">with the panel hardware</td></tr>",
            qty = hw.refs.len(),
            name = esc(&hw.name),
            refs = esc(&hw.refs.join("  ")),
        ));
    }
    body.push_str("</tbody></table>");
    body.push_str(&theme::page_footer(
        &format!("{} · build guide", guide.name),
        &format!("Sheet 1 of {sheets}"),
    ));
    body.push_str("</section>");

    // ---- One sheet per step: the picture, then what goes on it.
    let mut placed = 0usize;
    for (i, step) in guide.steps.iter().enumerate() {
        let n = step.parts.len();
        placed += n;
        let back_step = step_is_back(step);
        let use_bottom = back_step && bottom.is_some();
        let render = if use_bottom { &bottom } else { &top };
        let parts: Vec<PlacedPart> = if use_bottom {
            step.parts.iter().map(|p| mirror_part_x(p, cxmm)).collect()
        } else {
            step.parts.clone()
        };
        let highlight: HashSet<&str> = step.parts.iter().map(|p| p.refdes.as_str()).collect();
        // Size the picture first, then draw it: the refdes labels are scaled to
        // the printed width so they read the same on every board.
        let (dw, _dh, beside) = diagram_fit(diagram_aspect(render.as_ref(), guide.outline));
        let diagram = match render {
            Some(bp) => photoreal_board_svg(bp, guide.outline, &parts, dw),
            None => schematic_board_svg(guide, &highlight, dw),
        };

        let rows = part_rows(&step.parts);
        let show_orient = rows.iter().any(|r| r.polarity.is_some());
        let orient_head = if show_orient {
            "<th>Orientation</th>"
        } else {
            ""
        };
        let mut list = String::new();
        for row in &rows {
            let ticks: String = row
                .refs
                .iter()
                .map(|r| {
                    format!(
                        "<span class=\"tick\"><span class=\"chk\"></span>{}</span>",
                        esc(r)
                    )
                })
                .collect();
            let orient = if show_orient {
                format!(
                    "<td class=\"c-or\">{}</td>",
                    match row.polarity {
                        Some(p) => p.cue(),
                        None => "any way round",
                    }
                )
            } else {
                String::new()
            };
            list.push_str(&format!(
                "<tr><td class=\"c-ref place\">{ticks}</td>\
                 <td class=\"c-val\">{val}{swatch}</td>\
                 <td class=\"c-pkg mono\">{pkg}</td>{orient}</tr>",
                val = esc(&row.value),
                swatch = row_swatch(row),
                pkg = esc(&row.package),
            ));
        }

        let howto = step
            .assembly
            .as_deref()
            .map(|a| format!("<p class=\"howto\"><b>How.</b> {}</p>", esc(a)))
            .unwrap_or_default();
        let partnotes: String = step
            .part_notes
            .iter()
            .map(|pn| {
                let text = pn
                    .steps
                    .iter()
                    .map(|s| esc(s))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!(
                    "<p class=\"partnote\"><b class=\"pn-ref\">{}</b> {text}</p>",
                    esc(&pn.refs.join(", ")),
                )
            })
            .collect();
        // The generic "match pin 1 to the silkscreen" caution now lives in the
        // table's Orientation column, per part — repeating it as a banner on
        // every step was noise that trained the reader to skip cautions.
        let caution = step
            .caution
            .as_deref()
            .filter(|_| !show_orient)
            .map(|c| format!("<p class=\"caution\">⚠ {}</p>", esc(c)))
            .unwrap_or_default();
        let side_note = if back_step {
            "<p class=\"flip\">↺ Flip the board — these mount on the <b>BACK</b>, \
             and the picture is drawn from the back.</p>"
        } else {
            ""
        };
        body.push_str(&format!(
            "<section class=\"sheet step {layout}\">\
             <header class=\"step-head\">\
             <p class=\"step-of mono\">Step {stepno} of {total}{badge}</p>\
             <h2 class=\"step-title\">{title}</h2>\
             <p class=\"prog mono\">{n} part{s} to place · {placed} of {total_parts} done \
             after this step</p></header>\
             <div class=\"cols\">\
             <figure class=\"diagram\" style=\"width:{dw:.1}mm\">{svg}</figure>\
             <div class=\"parts\"><table class=\"ptab\"><thead><tr>\
             <th>Place &amp; tick</th><th>Value</th><th>Package</th>{orient_head}</tr></thead>\
             <tbody>{list}</tbody></table>{side_note}{howto}{partnotes}{caution}</div></div>\
             {footer}</section>",
            layout = if beside { "beside" } else { "stacked" },
            stepno = i + 1,
            badge = if back_step {
                " <span class=\"badge back\">BACK SIDE</span>"
            } else {
                ""
            },
            title = esc(&step.title),
            s = if n == 1 { "" } else { "s" },
            svg = diagram.svg,
            footer = theme::page_footer(
                &format!("{} · step {} — {}", guide.name, i + 1, step.title),
                &format!("Sheet {} of {sheets}", i + 2),
            ),
        ));
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Build guide — {}</title>\
         <style>{BASE}{CSS}</style></head><body><div class=\"wrap\">{body}</div></body></html>",
        esc(&guide.name),
        BASE = theme::BASE_CSS,
    )
}

/// Build-guide-specific CSS, layered after [`theme::BASE_CSS`].
///
/// Laid out as sheets, not as a scrolling page: every `.sheet` is one side of
/// paper, sized to the Letter∩A4 box, with its footer pinned to the bottom. The
/// step's picture is sized inline in millimetres by [`diagram_fit`] — CSS never
/// gets to squeeze it into a fraction of a text column, which is what used to
/// leave the board occupying under a tenth of the sheet.
const CSS: &str = "\
.sheet{display:flex;flex-direction:column;min-height:255mm;padding-bottom:2mm}\
.sheet+.sheet{border-top:1px dashed var(--hair);margin-top:6mm;padding-top:6mm}\
.kit-copy{margin:0 0 2mm}\
.kit-copy .intro{max-width:72ch;margin:0 0 2mm}\
.kit-copy h2{font-size:8pt;font-weight:700;text-transform:uppercase;margin:3mm 0 1mm}\
.tools{margin:0;padding:0;list-style:none;display:flex;flex-wrap:wrap;gap:0 1.5mm;font-size:8.5pt}\
.tools li{white-space:nowrap}\
.tools li+li::before{content:'·  ';color:var(--hair)}\
.sec{font-size:12pt;font-weight:700;margin:3mm 0 0;text-transform:uppercase;letter-spacing:.01em}\
.sec-sub{font-size:8.5pt;margin:.5mm 0 2mm}\
.ptab{border-collapse:collapse;width:100%;font-size:9pt}\
.ptab th{text-align:left;font-weight:700;font-size:7.5pt;text-transform:uppercase;\
border-top:.8pt solid var(--ink);border-bottom:.8pt solid var(--ink);\
padding:.9mm 1.5mm;white-space:nowrap}\
.ptab td{border-bottom:.4pt solid var(--hair);padding:1mm 1.5mm;vertical-align:middle}\
.ptab tbody tr:last-child td{border-bottom:.8pt solid var(--ink)}\
.ptab tr>*:first-child{padding-left:0}.ptab tr>*:last-child{padding-right:0}\
.h-chk,.c-chk{width:8mm}.c-qty{width:10mm}\
.c-val{font-weight:700;font-size:10pt}\
.c-pkg{font-size:8.5pt}\
.c-ref{font-size:9.5pt;font-weight:700}\
.c-step{font-size:8.5pt;width:38mm;white-space:nowrap;color:var(--muted)}\
.c-or{font-size:8.5pt;line-height:1.2;color:var(--warn);font-weight:700}\
.step .c-val{width:36%}.step .c-pkg{width:18%}.step .c-or{width:26%}\
.hw td{font-style:italic}\
.place{display:flex;flex-wrap:wrap;gap:.5mm 4mm;padding-top:1.2mm;padding-bottom:1.2mm}\
.tick{display:inline-flex;align-items:center;gap:1.6mm;font-family:ui-monospace,'SF Mono',Menlo,monospace;\
font-size:11pt;font-weight:700;white-space:nowrap}\
.overviews{display:flex;gap:6mm;align-items:flex-end;margin:2mm 0 0}\
.overview{margin:0;flex:none;max-width:100%}\
.overview svg{display:block;width:100%;height:auto;border:.5pt solid var(--ink)}\
.overview figcaption{font-size:7.5pt;font-weight:700;text-transform:uppercase;margin-top:1mm}\
.step-head{margin-bottom:2.5mm;border-bottom:1.6pt solid var(--ink);padding-bottom:1.5mm}\
.step-of{font-size:8pt;font-weight:700;text-transform:uppercase;margin:0}\
.step-title{font-size:20pt;font-weight:700;letter-spacing:-.02em;margin:0;line-height:1}\
.prog{margin:.5mm 0 0;font-size:8.5pt;color:var(--muted)}\
.badge{display:inline-block;font-size:7.5pt;font-weight:700;text-transform:uppercase;\
background:var(--ink);color:#fff;padding:.2mm 1.2mm;margin-left:1.5mm;vertical-align:.3mm}\
.cols{display:flex;gap:6mm;align-items:flex-start}\
.stacked .cols{flex-direction:column;gap:3mm}\
.diagram{margin:0;flex:none;max-width:100%}.parts{flex:1 1 auto;min-width:0;align-self:stretch}\
.stacked .parts{width:100%}\
.diagram svg{display:block;width:100%;height:auto;border:.5pt solid var(--ink)}\
.swatch{display:inline-block;vertical-align:middle;margin-left:1.5mm}\
svg.rband{width:58px;height:18px;border:0;background:none;vertical-align:middle}\
.flip{margin:3mm 0 0;padding:1.4mm 2mm;background:var(--ink);color:#fff;font-size:9pt;font-weight:700}\
.howto{margin:3mm 0 0;font-size:9pt}\
.howto b{text-transform:uppercase;font-size:8pt;letter-spacing:.03em}\
.partnote{margin:2mm 0 0;font-size:9pt;padding-left:4mm;border-left:.8pt solid var(--ink)}\
.pn-ref{font-family:ui-monospace,'SF Mono',Menlo,monospace;font-weight:700;margin-right:1.5mm}\
.caution{border:1pt solid var(--warn);color:var(--warn);font-weight:700;padding:1.4mm 2mm;\
margin:3mm 0 0;font-size:9pt}\
@media print{.sheet{break-after:page;min-height:244mm;border:none;margin:0;padding:0}\
.sheet:last-child{break-after:auto}.sheet+.sheet{border-top:none;margin-top:0;padding-top:0}\
.cols,.diagram,.step-head{break-inside:avoid}}";

/// Draw a highlight marker for one placed part on a PDF page: a red box (filled
/// over the schematic fallback; outlined over the real-board image so the part
/// shows through), its refdes, and any polarity marker. `mapx`/`mapy` map board
/// mm to page points.
fn pdf_marker(
    pg: &mut Page,
    p: &PlacedPart,
    mapx: &dyn Fn(f64) -> f64,
    mapy: &dyn Fn(f64) -> f64,
    scale: f64,
    filled: bool,
) {
    let (cx0, cy0, cx1, cy1) = p.bbox;
    // The outline is the whole signal over a photoreal render, so it thickens
    // with the diagram's magnification rather than thinning away on a small
    // board blown up to fill the sheet.
    pg.set_line_width(if filled {
        0.8
    } else {
        (scale * 0.25).clamp(1.0, 2.5)
    });
    pg.set_stroke(0.63, 0.07, 0.07);
    if filled {
        pg.set_fill(0.89, 0.29, 0.29);
        pg.rect(
            mapx(cx0),
            mapy(cy1),
            ((cx1 - cx0) * scale).max(1.0),
            ((cy1 - cy0) * scale).max(1.0),
            Paint::FillStroke,
        );
    } else {
        pg.rect(
            mapx(cx0),
            mapy(cy1),
            ((cx1 - cx0) * scale).max(1.0),
            ((cy1 - cy0) * scale).max(1.0),
            Paint::Stroke,
        );
    }
    // Refdes on a dark chip just above the box, so it stays legible over the busy
    // photoreal board without crowding the pads.
    // Clamped in *points*, not board mm: the diagram's magnification varies ~5×
    // between a 32 mm test board and a 128 mm panel, and the label should not.
    let fs = (scale * 1.4).clamp(7.0, 11.0);
    let tw = p.refdes.len() as f64 * fs * 0.62;
    let (lx, ly) = (mapx(p.cx), mapy(cy0) + fs * 0.85);
    pg.set_fill(0.1, 0.12, 0.14);
    pg.rect(
        lx - tw / 2.0 - 1.5,
        ly - fs * 0.5,
        tw + 3.0,
        fs * 1.2,
        Paint::Fill,
    );
    pg.set_fill(1.0, 1.0, 1.0);
    pg.text(lx - tw / 2.0, ly - fs * 0.32, fs, Font::Bold, &p.refdes);
    if let (Some(pol), Some((qx, qy))) = (p.polarity, p.pin1) {
        let r = (scale * 1.3).clamp(4.0, 9.0);
        pg.set_line_width(0.4);
        pg.set_fill(0.06, 0.06, 0.06);
        pg.set_stroke(1.0, 1.0, 1.0);
        pg.circle(mapx(qx), mapy(qy), r, Paint::FillStroke);
        pg.set_fill(1.0, 1.0, 1.0);
        pg.text(
            mapx(qx) - r * 0.3,
            mapy(qy) - r * 0.5,
            r * 1.2,
            Font::Bold,
            pol.label(),
        );
    }
}

/// Render the guide as a print-ready PDF: a prep/sort page, then one build step
/// per A4 page (clean page breaks), self-contained (no browser). `top_jpeg` /
/// `bottom_jpeg` are the unpopulated `pcb render`s (PNG→JPEG); a back-side step is
/// drawn on the bottom render (parts mirrored) so it matches the board in hand.
/// Falls back to a schematic top-down when no render is available.
pub fn guide_to_pdf(
    guide: &BuildGuide,
    top_jpeg: Option<&[u8]>,
    bottom_jpeg: Option<&[u8]>,
) -> Vec<u8> {
    let top_img = top_jpeg.and_then(|b| pdf::Image::from_jpeg(b.to_vec()));
    let bot_img = bottom_jpeg.and_then(|b| pdf::Image::from_jpeg(b.to_vec()));
    let mut images: Vec<&pdf::Image> = Vec::new();
    let top_idx = top_img.as_ref().map(|im| {
        images.push(im);
        images.len() - 1
    });
    let bot_idx = bot_img.as_ref().map(|im| {
        images.push(im);
        images.len() - 1
    });

    // US Letter at a 12 mm margin — the same 186 mm content box the HTML guide
    // lays out to, so the two artifacts print at the same size.
    let m = 12.0 * pdf::MM;
    let (page_w, page_h) = (pdf::LETTER_W, pdf::LETTER_H);
    let cw = page_w - 2.0 * m; // content width
    let top = page_h - m;
    let foot_y = m + 4.0; // footer baseline
    let body_bottom = foot_y + 14.0;
    let total = guide.steps.len();
    let total_parts: usize = guide.steps.iter().map(|s| s.parts.len()).sum();
    let any_back = guide.steps.iter().any(step_is_back);
    let (ox0, oy0, ox1, oy1) = guide.outline;
    let cxmm = (ox0 + ox1) / 2.0;
    let pad = CROP_MARGIN_MM;
    let (bx0, by0, bx1, by1) = (ox0 - pad, oy0 - pad, ox1 + pad, oy1 + pad);
    let (bw, bh) = ((bx1 - bx0).max(1.0), (by1 - by0).max(1.0));
    let sheets = total + 1;
    let doc_name = guide.name.clone();

    let mut pages = Vec::new();

    // ---- Sheet 1: what this is, what you need, and the pull-and-sort list.
    {
        let mut pg = Page::new();
        pg.set_fill(0.08, 0.09, 0.11);
        let title = match &guide.brand {
            Some(b) => format!("{b} — {} build guide", guide.name),
            None => format!("{} — build guide", guide.name),
        };
        pg.text(m, top - 14.0, 19.0, Font::Bold, &title);
        pg.set_fill(0.36, 0.35, 0.33);
        pg.text(
            m,
            top - 30.0,
            9.5,
            Font::Regular,
            &format!(
                "{} · {total} steps · {total_parts} parts · {sheets} sheets. \
                 Low-profile parts first, tall parts last{}.",
                guide.kit.label(),
                if any_back { ", back side first" } else { "" }
            ),
        );

        // Per-circuit build copy (5uj.5): intro, tools, kit cautions.
        let mut ly = top - 50.0;
        if let Some(intro) = &guide.intro {
            pg.set_fill(0.19, 0.19, 0.18);
            ly = pdf_wrapped(&mut pg, m, ly, cw, 10.0, Font::Regular, intro) - 3.0;
        }
        if !guide.tools.is_empty() {
            pg.set_fill(0.36, 0.35, 0.33);
            let tools = format!("Tools: {}", guide.tools.join("  ·  "));
            ly = pdf_wrapped(&mut pg, m, ly, cw, 9.5, Font::Regular, &tools) - 3.0;
        }
        for c in &guide.kit_cautions {
            pg.set_fill(0.54, 0.39, 0.0);
            ly = pdf_wrapped(&mut pg, m, ly, cw, 9.5, Font::Bold, &format!("[!] {c}")) - 2.0;
        }

        // The board itself, both sides, so the builder can orient it before the
        // first step tells them to flip it.
        let overview: Vec<(&str, usize, &pdf::Image)> =
            [("FRONT", top_idx, &top_img), ("BACK", bot_idx, &bot_img)]
                .into_iter()
                .filter_map(|(l, i, im)| Some((l, i?, im.as_ref()?)))
                .collect();
        if !overview.is_empty() {
            let n = overview.len() as f64;
            let box_w = (cw - 16.0 * (n - 1.0)) / n;
            let box_h = 210.0;
            let mut x = m;
            let mut lowest = ly;
            for (label, idx, img) in &overview {
                let (dw, dh) = pdf_place_render(
                    &mut pg,
                    img,
                    *idx,
                    guide.outline,
                    (x, ly - 4.0, box_w, box_h),
                    &[],
                );
                pg.set_fill(0.42, 0.41, 0.38);
                pg.text(x, ly - 12.0 - dh, 7.5, Font::Bold, label);
                lowest = lowest.min(ly - 16.0 - dh);
                x += dw.max(20.0) + 16.0;
            }
            ly = lowest - 12.0;
        }

        pg.set_fill(0.08, 0.09, 0.11);
        pg.text(m, ly, 13.0, Font::Bold, "Pull & sort your parts");
        ly -= 12.0;
        pg.set_fill(0.42, 0.41, 0.38);
        pg.text(
            m,
            ly,
            9.0,
            Font::Regular,
            "Tick each off as you find it. The step that needs it is on the right.",
        );
        ly -= 16.0;
        // Column rules, in the same order as the HTML sort table.
        let (c_qty, c_val, c_pkg, c_ref, c_step) =
            (m + 16.0, m + 44.0, m + 168.0, m + 250.0, m + 372.0);
        pg.set_fill(0.42, 0.41, 0.38);
        for (x, h) in [
            (c_qty, "QTY"),
            (c_val, "VALUE"),
            (c_pkg, "PACKAGE"),
            (c_ref, "REFERENCE DESIGNATORS"),
            (c_step, "FIRST USED"),
        ] {
            pg.text(x, ly, 7.0, Font::Bold, h);
        }
        ly -= 4.0;
        pg.set_line_width(1.0);
        pg.set_stroke(0.64, 0.36, 0.13);
        pg.rect(m, ly, cw, 0.0, Paint::Stroke);
        ly -= 15.0;

        // One row per (value, package) pile, deduped across steps: a part seated
        // in one step and soldered in another is still one part to go and find.
        let mut listed: HashSet<String> = HashSet::new();
        for (i, step) in guide.steps.iter().enumerate() {
            let side = if step_is_back(step) { " · back" } else { "" };
            for row in part_rows(&step.parts) {
                if !listed.insert(format!("{}\u{1}{}", row.value, row.package)) {
                    continue;
                }
                pdf_checkbox(&mut pg, m, ly - 1.5, 9.0);
                pg.set_fill(0.42, 0.41, 0.38);
                pg.text(
                    c_qty,
                    ly,
                    9.0,
                    Font::Regular,
                    &format!("{}x", row.refs.len()),
                );
                pg.set_fill(0.08, 0.09, 0.11);
                pg.text(c_val, ly, 10.5, Font::Bold, &row.value);
                pg.set_fill(0.48, 0.29, 0.13);
                pg.text(c_pkg, ly, 9.0, Font::Regular, &row.package);
                pg.set_fill(0.08, 0.09, 0.11);
                pg.text(c_ref, ly, 9.5, Font::Bold, &row.refs.join("  "));
                pg.set_fill(0.42, 0.41, 0.38);
                pg.text(
                    c_step,
                    ly,
                    8.5,
                    Font::Regular,
                    &format!("{}. {}{side}", i + 1, step.title),
                );
                ly -= 6.0;
                pg.set_line_width(0.4);
                pg.set_stroke(0.84, 0.82, 0.78);
                pg.rect(m, ly, cw, 0.0, Paint::Stroke);
                ly -= 12.0;
                if ly < body_bottom {
                    break;
                }
            }
        }
        pdf_footer(
            &mut pg,
            m,
            page_w,
            foot_y,
            &format!("{doc_name} · build guide"),
            &format!("Sheet 1 of {sheets}"),
        );
        pages.push(pg);
    }

    // ---- One sheet per step: the picture at page size, then what goes on it.
    let mut placed = 0usize;
    for (i, step) in guide.steps.iter().enumerate() {
        let n = step.parts.len();
        placed += n;
        let back_step = step_is_back(step);
        let use_bottom = back_step && bot_idx.is_some();
        let img = if use_bottom { &bot_img } else { &top_img };
        let idx = if use_bottom { bot_idx } else { top_idx };
        let step_parts: Vec<PlacedPart> = if use_bottom {
            step.parts.iter().map(|p| mirror_part_x(p, cxmm)).collect()
        } else {
            step.parts.clone()
        };

        let mut pg = Page::new();
        pg.set_fill(0.64, 0.36, 0.13);
        pg.text(
            m,
            top - 9.0,
            8.0,
            Font::Bold,
            &format!(
                "STEP {} OF {total}{}",
                i + 1,
                if back_step { "     BACK SIDE" } else { "" }
            ),
        );
        pg.set_fill(0.08, 0.09, 0.11);
        pg.text(m, top - 26.0, 19.0, Font::Bold, &step.title);
        pg.set_fill(0.42, 0.41, 0.38);
        pg.text(
            m,
            top - 38.0,
            8.5,
            Font::Regular,
            &format!(
                "{n} part{} to place · {placed} of {total_parts} done after this step",
                if n == 1 { "" } else { "s" }
            ),
        );

        // The picture gets the page: a tall board runs the full body height with
        // the parts beside it, a wide one spans the full width with them below.
        let diag_top = top - 50.0;
        let body_h = diag_top - body_bottom;
        let aspect = match (img.as_ref(), idx) {
            (Some(im), Some(_)) => {
                let (iw, ih) = im.size();
                let win = board_window(
                    &BoardPng {
                        png: &[],
                        width: iw as u32,
                        height: ih as u32,
                    },
                    guide.outline,
                );
                win.2 / win.3
            }
            _ => bw / bh,
        };
        let beside = aspect < 0.72;
        let (box_w, box_h) = if beside {
            ((body_h * aspect).min(cw * 0.55), body_h)
        } else {
            (cw, body_h * 0.62)
        };

        let (dw, dh) = match (img.as_ref(), idx) {
            (Some(im), Some(idx)) => pdf_place_render(
                &mut pg,
                im,
                idx,
                guide.outline,
                (m, diag_top, box_w, box_h),
                &step_parts,
            ),
            _ => {
                // Schematic fallback: the same box, filled the same way.
                let highlight: HashSet<&str> =
                    step.parts.iter().map(|p| p.refdes.as_str()).collect();
                let scale = (box_w / bw).min(box_h / bh);
                let mapx = |x: f64| m + (x - bx0) * scale;
                let mapy = |y: f64| diag_top - (y - by0) * scale;
                pg.set_line_width(0.8);
                pg.set_fill(0.93, 0.95, 0.93);
                pg.set_stroke(0.2, 0.6, 0.4);
                pg.rect(
                    mapx(ox0),
                    mapy(oy1),
                    (ox1 - ox0) * scale,
                    (oy1 - oy0) * scale,
                    Paint::FillStroke,
                );
                for p in guide.steps.iter().flat_map(|s| &s.parts) {
                    let (px0, py0, px1, py1) = p.bbox;
                    if highlight.contains(p.refdes.as_str()) {
                        pdf_marker(&mut pg, p, &mapx, &mapy, scale, true);
                    } else {
                        pg.set_line_width(0.4);
                        pg.set_fill(0.86, 0.86, 0.86);
                        pg.set_stroke(0.67, 0.67, 0.67);
                        pg.rect(
                            mapx(px0),
                            mapy(py1),
                            ((px1 - px0) * scale).max(1.0),
                            ((py1 - py0) * scale).max(1.0),
                            Paint::FillStroke,
                        );
                    }
                }
                (bw * scale, bh * scale)
            }
        };

        // Parts: beside the picture when it's tall, under it when it's wide.
        let (tx, tw, mut ly) = if beside {
            (m + dw + 18.0, cw - dw - 18.0, diag_top - 2.0)
        } else {
            (m, cw, diag_top - dh - 22.0)
        };
        pg.set_fill(0.42, 0.41, 0.38);
        pg.text(tx, ly, 7.0, Font::Bold, "PLACE & TICK");
        ly -= 4.0;
        pg.set_line_width(1.0);
        pg.set_stroke(0.64, 0.36, 0.13);
        pg.rect(tx, ly, tw, 0.0, Paint::Stroke);
        ly -= 16.0;
        for row in part_rows(&step.parts) {
            // Line 1: a tick box per reference designator — the thing you hunt
            // for on the silkscreen, so it is the biggest text in the row.
            let mut x = tx;
            for r in &row.refs {
                let w = 12.0 + pdf_text_w(r, 11.0, true);
                if x > tx && x + w > tx + tw {
                    x = tx;
                    ly -= 15.0;
                }
                pdf_checkbox(&mut pg, x, ly - 1.0, 9.0);
                pg.set_fill(0.08, 0.09, 0.11);
                pg.text(x + 12.0, ly, 11.0, Font::Bold, r);
                x += w + 10.0;
            }
            ly -= 13.0;
            // Line 2: which part it is, and which way round.
            pg.set_fill(0.08, 0.09, 0.11);
            pg.text(tx, ly, 10.0, Font::Bold, &row.value);
            let mut dx = tx + pdf_text_w(&row.value, 10.0, true) + 10.0;
            pg.set_fill(0.48, 0.29, 0.13);
            pg.text(dx, ly, 9.0, Font::Regular, &row.package);
            dx += pdf_text_w(&row.package, 9.0, false) + 10.0;
            if let Some(pol) = row.polarity {
                pg.set_fill(0.42, 0.31, 0.02);
                pdf_wrapped(
                    &mut pg,
                    dx,
                    ly,
                    (tx + tw - dx).max(60.0),
                    8.5,
                    Font::Regular,
                    pol.cue(),
                );
            }
            pdf_row_bands(&mut pg, &row, tx + tw, ly);
            ly -= 18.0;
            if ly < body_bottom {
                break;
            }
        }

        if back_step {
            ly -= 2.0;
            pg.set_fill(0.23, 0.16, 0.09);
            ly = pdf_wrapped(
                &mut pg,
                tx,
                ly,
                tw,
                9.5,
                Font::Bold,
                "Flip the board - these mount on the BACK, and the picture is drawn \
                 from the back.",
            ) - 5.0;
        }
        if let Some(a) = &step.assembly {
            pg.set_fill(0.64, 0.36, 0.13);
            pg.text(tx, ly, 9.5, Font::Bold, "How.");
            pg.set_fill(0.27, 0.25, 0.22);
            ly = pdf_wrapped(
                &mut pg,
                tx + 26.0,
                ly,
                (tw - 26.0).max(60.0),
                9.5,
                Font::Regular,
                a,
            ) - 4.0;
        }
        for pn in &step.part_notes {
            pg.set_fill(0.64, 0.36, 0.13);
            pg.text(tx, ly, 9.0, Font::Bold, &format!("{}:", pn.refs.join(", ")));
            ly -= 12.0;
            pg.set_fill(0.27, 0.25, 0.22);
            ly = pdf_wrapped(
                &mut pg,
                tx + 8.0,
                ly,
                (tw - 8.0).max(60.0),
                9.0,
                Font::Regular,
                &pn.steps.join(" "),
            ) - 4.0;
        }
        // The generic pin-1 caution now rides on the part row it applies to; only
        // a caution the table can't carry is still worth a banner.
        if let Some(c) = step
            .caution
            .as_deref()
            .filter(|_| !part_rows(&step.parts).iter().any(|r| r.polarity.is_some()))
        {
            pg.set_fill(0.54, 0.39, 0.0);
            pdf_wrapped(&mut pg, tx, ly, tw, 9.5, Font::Bold, &format!("[!] {c}"));
        }

        pdf_footer(
            &mut pg,
            m,
            page_w,
            foot_y,
            &format!("{doc_name} · step {} — {}", i + 1, step.title),
            &format!("Sheet {} of {sheets}", i + 2),
        );
        pages.push(pg);
    }
    pdf::document(&pages, &images, (page_w, page_h))
}

/// Approximate the width of a Helvetica run at `size` pt. The built-in fonts have
/// no metrics table here, and this only has to be good enough to flow tick chips
/// and butt a package name up against a value.
fn pdf_text_w(s: &str, size: f64, bold: bool) -> f64 {
    s.chars().count() as f64 * size * if bold { 0.58 } else { 0.52 }
}

/// An empty tick box with its bottom-left at `(x, y)`, side `s` pt.
fn pdf_checkbox(pg: &mut Page, x: f64, y: f64, s: f64) {
    pg.set_line_width(0.8);
    pg.set_stroke(0.42, 0.40, 0.37);
    pg.set_fill(1.0, 1.0, 1.0);
    pg.rect(x, y, s, s, Paint::FillStroke);
}

/// The per-sheet footer: what this page is, and where it sits in the document.
fn pdf_footer(pg: &mut Page, m: f64, page_w: f64, y: f64, left: &str, right: &str) {
    pg.set_line_width(0.4);
    pg.set_stroke(0.84, 0.82, 0.78);
    pg.rect(m, y + 9.0, page_w - 2.0 * m, 0.0, Paint::Stroke);
    pg.set_fill(0.42, 0.41, 0.38);
    pg.text(m, y, 7.5, Font::Regular, left);
    pg.text(
        page_w - m - pdf_text_w(right, 7.5, false),
        y,
        7.5,
        Font::Regular,
        right,
    );
}

/// Draw a board render **cropped to the board** ([`board_window`]) filling the box
/// `(bx, top_y, box_w, box_h)` (page pt, `top_y` is the box's top edge), with a
/// highlight marker on each of `parts`. Returns the drawn `(width, height)`.
///
/// The crop is the whole point: `pcb render` letterboxes anything that isn't
/// square, and drawing the frame whole is what left the board a fifth of the
/// picture and its refdes labels too small to read on paper. PDF's image operator
/// has no crop, so the image is scaled up and clipped to the visible window.
fn pdf_place_render(
    pg: &mut Page,
    img: &pdf::Image,
    idx: usize,
    outline: (f64, f64, f64, f64),
    (bx, top_y, box_w, box_h): (f64, f64, f64, f64),
    parts: &[PlacedPart],
) -> (f64, f64) {
    let (iw, ih) = img.size();
    let (vx, vy, vw, vh) = board_window(
        &BoardPng {
            png: &[],
            width: iw as u32,
            height: ih as u32,
        },
        outline,
    );
    let s = (box_w / vw).min(box_h / vh); // page pt per image px
    let (dw, dh) = (vw * s, vh * s);
    // Place the full image so its cropped window lands in the box, then clip.
    let ex = bx - vx * s;
    let ey = top_y - dh - (ih - vy - vh) * s;
    pg.draw_image(
        [iw * s, 0.0, 0.0, ih * s, ex, ey],
        (bx, top_y - dh, dw, dh),
        idx,
    );
    let (x0, y0, x1, y1) = outline;
    let sc = render_scale(iw, ih, x1 - x0, y1 - y0); // image px per board mm
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let mapx = |x: f64| ex + (iw / 2.0 + (x - cx) * sc) * s;
    let mapy = |y: f64| ey + (ih - (ih / 2.0 + (y - cy) * sc)) * s;
    for p in parts {
        pdf_marker(pg, p, &mapx, &mapy, sc * s, false);
    }
    (dw, dh)
}

/// The highlight overlay for one part (SVG, board-mm coords): a rounded amber
/// box (framing the pads, edged red so it reads over green soldermask) + a
/// halo'd white refdes + a polarity marker (K/+/1) at the reference pad, so the
/// assembler can match it to the board's silkscreen mark. `fill_opacity` lets the
/// bare pads show through so the builder still sees where the pins land.
fn highlight_svg(p: &PlacedPart, fs: f64, fill_opacity: f64) -> String {
    let (bx0, by0, bx1, by1) = p.bbox;
    let m = 0.35; // frame just outside the pads
    let (x, y, w, h) = (
        bx0 - m,
        by0 - m,
        (bx1 - bx0) + 2.0 * m,
        (by1 - by0) + 2.0 * m,
    );
    let halo = fs * 0.18;
    // Stroke scales with the label, so the frame stays visible at whatever size
    // the diagram lands on the page rather than thinning to nothing.
    let sw = fs * 0.2;
    // Refdes just above the box so it never crowds the pads.
    let label_y = y - fs * 0.3;
    let mut s = format!(
        "<rect x=\"{x:.3}\" y=\"{y:.3}\" width=\"{w:.3}\" height=\"{h:.3}\" rx=\"0.4\" \
         fill=\"#ffd21f\" fill-opacity=\"{fill_opacity}\" stroke=\"#e01b0c\" \
         stroke-width=\"{sw:.3}\"/>\
         <text x=\"{cx:.3}\" y=\"{label_y:.3}\" font-size=\"{fs:.3}\" text-anchor=\"middle\" \
         dominant-baseline=\"baseline\" fill=\"#fff\" stroke=\"#111\" stroke-width=\"{halo:.3}\" \
         paint-order=\"stroke\" font-weight=\"bold\">{refdes}</text>",
        cx = p.cx,
        refdes = esc(&p.refdes)
    );
    if let (Some(pol), Some((mx, my))) = (p.polarity, p.pin1) {
        let r = fs * 0.95;
        s.push_str(&format!(
            "<circle cx=\"{mx:.3}\" cy=\"{my:.3}\" r=\"{r:.3}\" fill=\"#111\" stroke=\"#fff\" \
             stroke-width=\"0.2\"/>\
             <text x=\"{mx:.3}\" y=\"{my:.3}\" font-size=\"{fss:.3}\" text-anchor=\"middle\" \
             dominant-baseline=\"central\" fill=\"#fff\" font-weight=\"bold\">{lbl}</text>",
            fss = fs * 1.15,
            lbl = pol.label(),
        ));
    }
    s
}

/// How tall a diagram refdes should be **on paper**, in mm (≈ 10 pt).
const LABEL_PRINT_MM: f64 = 3.5;

/// Refdes label size in *board* mm, chosen so it lands at [`LABEL_PRINT_MM`] once
/// a diagram covering `crop_w_mm` of board is printed `printed_w_mm` wide.
///
/// Sizing the label off the board's own dimensions — what this used to do —
/// couples it to the wrong thing. A 32 mm test board is magnified 5× to fill the
/// sheet and a 128 mm panel only 1.6×, so one fixed board-mm size prints as 17 pt
/// on the first and 6 pt on the second. Working back from the printed size makes
/// every guide's labels the same size in the reader's hand.
fn label_size_for(crop_w_mm: f64, printed_w_mm: f64) -> f64 {
    // NaN-safe: an unmeasurable board falls back rather than emitting a NaN size.
    if !crop_w_mm.is_finite()
        || !printed_w_mm.is_finite()
        || crop_w_mm <= 0.0
        || printed_w_mm <= 0.0
    {
        return 1.0;
    }
    (LABEL_PRINT_MM * crop_w_mm / printed_w_mm).max(0.15)
}

/// How much board (in mm across) a cropped render of `board` shows.
fn crop_width_mm(board: &BoardPng, outline: (f64, f64, f64, f64)) -> f64 {
    let (x0, y0, x1, y1) = outline;
    let scale = render_scale(board.width as f64, board.height as f64, x1 - x0, y1 - y0);
    let (_, _, vw, _) = board_window(board, outline);
    if scale > 0.0 {
        vw / scale
    } else {
        (x1 - x0).max(1.0)
    }
}

/// A schematic top-down SVG: outline + every part as a box, `highlight`ed parts
/// red, the rest greyed. The fallback when no real KiCad plot is available.
fn schematic_board_svg(
    guide: &BuildGuide,
    highlight: &HashSet<&str>,
    printed_w_mm: f64,
) -> Diagram {
    let (x0, y0, x1, y1) = guide.outline;
    let (w, h) = (x1 - x0, y1 - y0);
    let pad = 2.0;
    let mut svg = format!(
        "<svg viewBox=\"{} {} {} {}\" xmlns=\"http://www.w3.org/2000/svg\">\
         <rect x=\"{x0}\" y=\"{y0}\" width=\"{w}\" height=\"{h}\" fill=\"#eef3ee\" \
         stroke=\"#3a6\" stroke-width=\"0.3\"/>",
        x0 - pad,
        y0 - pad,
        w + 2.0 * pad,
        h + 2.0 * pad
    );
    let fs = label_size_for(w + 2.0 * pad, printed_w_mm);
    for p in guide.steps.iter().flat_map(|s| &s.parts) {
        if highlight.contains(p.refdes.as_str()) {
            svg.push_str(&highlight_svg(p, fs, 0.85));
        } else {
            let (bx0, by0, bx1, by1) = p.bbox;
            svg.push_str(&format!(
                "<rect x=\"{bx0}\" y=\"{by0}\" width=\"{}\" height=\"{}\" rx=\"0.2\" fill=\"#dcdcdc\" \
                 fill-opacity=\"0.5\" stroke=\"#aaa\" stroke-width=\"0.15\"/>",
                bx1 - bx0,
                by1 - by0
            ));
        }
    }
    svg.push_str("</svg>");
    Diagram {
        svg,
        aspect: (w + 2.0 * pad) / (h + 2.0 * pad),
    }
}

/// Breathing room left around the board when cropping a render, in board mm.
const CROP_MARGIN_MM: f64 = 3.0;

/// The image-pixel window `(x, y, w, h)` that tightly frames the board, with
/// [`CROP_MARGIN_MM`] of margin, clamped to the image.
///
/// This is the difference between a readable diagram and an unreadable one.
/// `pcb render` frames the board's bounding *circle* (see [`render_scale`]), so
/// anything that isn't square is delivered letterboxed: a 40 × 128 mm Eurorack
/// board lands as a narrow strip covering 21% of a 4:3 frame. Printing the frame
/// whole spent four fifths of the picture on empty backdrop and shrank the part
/// highlights below legibility. Cropping to the board costs nothing and is worth
/// more than any amount of layout tuning.
fn board_window(board: &BoardPng, outline: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let (x0, y0, x1, y1) = outline;
    let (iw, ih) = (board.width as f64, board.height as f64);
    let scale = render_scale(iw, ih, x1 - x0, y1 - y0);
    let m = CROP_MARGIN_MM * scale;
    // Board-mm → image-px: the board's centre sits at the image's centre.
    let half_w = (x1 - x0) / 2.0 * scale + m;
    let half_h = (y1 - y0) / 2.0 * scale + m;
    let (cx, cy) = (iw / 2.0, ih / 2.0);
    let (wx0, wy0) = ((cx - half_w).max(0.0), (cy - half_h).max(0.0));
    let (wx1, wy1) = ((cx + half_w).min(iw), (cy + half_h).min(ih));
    (
        wx0,
        wy0,
        (wx1 - wx0).max(1.0).min(iw),
        (wy1 - wy0).max(1.0).min(ih),
    )
}

/// A rendered step diagram: the SVG itself plus its width-over-height ratio, so
/// the page can size it to fill the sheet ([`diagram_fit`]).
struct Diagram {
    svg: String,
    aspect: f64,
}

/// The photorealistic board render (PNG, base64-embedded) with the current step's
/// parts highlighted, **cropped to the board** ([`board_window`]). `pcb render`
/// is orthographic top-down with the board centred, so board-mm map into the
/// image via `scale = FIT·min(W/w_mm, H/h_mm)` about the image centre (FIT
/// calibrated to KiCad's framing). Overlays are drawn in mm inside an SVG
/// transform group, so [`highlight_svg`] is reused unchanged; the crop is a
/// `viewBox` change only, so the overlay maths is untouched by it.
fn photoreal_board_svg(
    board: &BoardPng,
    outline: (f64, f64, f64, f64),
    parts: &[PlacedPart],
    printed_w_mm: f64,
) -> Diagram {
    let (x0, y0, x1, y1) = outline;
    let (w, h) = (board.width as f64, board.height as f64);
    let scale = render_scale(w, h, x1 - x0, y1 - y0);
    let (cxmm, cymm) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (vx, vy, vw, vh) = board_window(board, outline);
    let png = base64::engine::general_purpose::STANDARD.encode(board.png);
    let fs = label_size_for(crop_width_mm(board, outline), printed_w_mm);
    let overlay: String = parts.iter().map(|p| highlight_svg(p, fs, 0.34)).collect();
    let svg = format!(
        "<svg viewBox=\"{vx:.1} {vy:.1} {vw:.1} {vh:.1}\" xmlns=\"http://www.w3.org/2000/svg\">\
         <image x=\"0\" y=\"0\" width=\"{w:.0}\" height=\"{h:.0}\" \
         href=\"data:image/png;base64,{png}\"/>\
         <g transform=\"translate({tx:.3} {ty:.3}) scale({scale:.5}) translate({ntx:.3} {nty:.3})\">\
         {overlay}</g></svg>",
        tx = w / 2.0,
        ty = h / 2.0,
        ntx = -cxmm,
        nty = -cymm,
    );
    Diagram {
        svg,
        aspect: vw / vh,
    }
}

/// The bare board with nothing highlighted — the "what am I building, and which
/// way round is it" picture that opens the guide. Carries no labels, so the
/// printed width it would be sized against is irrelevant.
fn board_overview_svg(board: &BoardPng, outline: (f64, f64, f64, f64)) -> Diagram {
    photoreal_board_svg(board, outline, &[], theme::CONTENT_W_MM)
}

/// The aspect a step's diagram will have, known before it is drawn so
/// [`diagram_fit`] can size it and the labels can be scaled to the result.
fn diagram_aspect(render: Option<&BoardPng>, outline: (f64, f64, f64, f64)) -> f64 {
    match render {
        Some(bp) => {
            let (_, _, vw, vh) = board_window(bp, outline);
            vw / vh
        }
        None => {
            let (x0, y0, x1, y1) = outline;
            (x1 - x0 + 4.0) / (y1 - y0 + 4.0)
        }
    }
}

/// Size an overview figure so `n` of them sit side by side on the kit sheet
/// without crowding out the sort table: `(width mm, height mm)`.
fn overview_fit(aspect: f64, n: usize) -> (f64, f64) {
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        1.0
    };
    let max_w = (theme::CONTENT_W_MM - 8.0 * (n.max(1) - 1) as f64) / n.max(1) as f64;
    // Capped well under the sheet so the sort table it shares a sheet with is not
    // pushed onto a second page — the overview is orientation, not the content.
    let h = (max_w / aspect).min(90.0);
    (h * aspect, h)
}

/// How tall a step's diagram may be when it sits *beside* the parts table, in mm:
/// the page body less the step header and footer.
const STEP_BODY_H_MM: f64 = 214.0;
/// The widest a beside-the-table diagram may be — past this the parts table has
/// nowhere to go, so the step stacks instead.
const SIDE_MAX_W_MM: f64 = 106.0;
/// How tall a stacked (above-the-table) diagram may be, leaving the table room.
const STACK_MAX_H_MM: f64 = 146.0;

/// Size a step diagram to fill the sheet: `(width mm, height mm, beside)`.
///
/// Two shapes of board, two layouts. A tall board (Eurorack panel, pedal) runs
/// the full page height in a column with the parts table beside it; a wide or
/// square board spans the full page width with the table underneath. Either way
/// the picture is sized to the paper rather than to a fraction of a text column —
/// on a 5 HP board that is the difference between a 19 × 62 mm diagram and a
/// 74 × 214 mm one.
fn diagram_fit(aspect: f64) -> (f64, f64, bool) {
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        1.0
    };
    if aspect < 0.72 {
        let h = STEP_BODY_H_MM.min(SIDE_MAX_W_MM / aspect);
        return (h * aspect, h, true);
    }
    let h = (theme::CONTENT_W_MM / aspect).min(STACK_MAX_H_MM);
    (h * aspect, h, false)
}

/// Whether a value-group is resistors (refdes prefix `R`, not `RV`/relays).
fn is_resistor_group(refs: &[String]) -> bool {
    refs.first().map(|r| prefix_of(r)) == Some("R")
}

/// Draw a through-hole resistor's color bands as a compact vertical-stripe strip
/// on a PDF page, right edge at `right_x`, sitting on text baseline `y` (a beige
/// backing so light bands read). No-op for SMD / non-resistor / unparseable groups.
fn pdf_row_bands(pg: &mut Page, row: &PartRow, right_x: f64, y: f64) {
    if !row.through_hole || !is_resistor_group(&row.refs) {
        return;
    }
    let Some(cc) = crate::resistor::color_code(&row.value) else {
        return;
    };
    let (bw, gap, h) = (3.2, 1.3, 9.0);
    let strip_w = cc.bands.len() as f64 * (bw + gap);
    let x0 = right_x - strip_w;
    let by = y - 1.5;
    pg.set_fill(0.91, 0.83, 0.63);
    pg.rect(x0 - 1.5, by - 1.0, strip_w + 3.0, h + 2.0, Paint::Fill);
    let mut cx = x0;
    for band in &cc.bands {
        let (r, g, b) = band.rgb();
        pg.set_fill(r, g, b);
        pg.rect(cx, by, bw, h, Paint::Fill);
        pg.set_line_width(0.2);
        pg.set_stroke(0.5, 0.5, 0.5);
        pg.rect(cx, by, bw, h, Paint::Stroke);
        cx += bw + gap;
    }
}

/// Draw `text` word-wrapped to `max_w` points at font `size`, starting at
/// baseline `y`, and return the baseline just below the last line. The PDF text
/// primitive doesn't wrap, so this greedily packs words by an estimated advance
/// width (monospace-ish 0.5·size per char — conservative for the built-in fonts).
fn pdf_wrapped(
    pg: &mut Page,
    x: f64,
    y: f64,
    max_w: f64,
    size: f64,
    font: Font,
    text: &str,
) -> f64 {
    let max_chars = ((max_w / (size * 0.5)).floor() as usize).max(10);
    let mut cy = y;
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > max_chars {
            pg.text(x, cy, size, font, &line);
            cy -= size * 1.35;
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        pg.text(x, cy, size, font, &line);
        cy -= size * 1.35;
    }
    cy
}

/// A piece of loose hardware and the reference designators it serves.
struct HardwareRow {
    name: String,
    refs: Vec<String>,
}

/// The loose hardware every placed part in the guide arrives with
/// ([`crate::hardware`]), grouped by item and carrying the refdes it belongs to.
///
/// Derived here rather than plumbed in from the BOM: the guide already knows
/// every part's footprint, and the pull-and-sort sheet is the second place (with
/// the Visual BOM) where a builder counts the kit out.
fn hardware_rows(steps: &[BuildStep]) -> Vec<HardwareRow> {
    let mut order: Vec<&'static str> = Vec::new();
    let mut serves: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    let mut seen: HashSet<(&'static str, String)> = HashSet::new();
    for part in steps.iter().flat_map(|s| &s.parts) {
        for item in crate::hardware::for_footprint(&part.footprint) {
            // A part fitted across two steps (seated, then soldered) still needs
            // exactly one nut.
            if !seen.insert((item.name, part.refdes.clone())) {
                continue;
            }
            if !serves.contains_key(item.name) {
                order.push(item.name);
            }
            serves
                .entry(item.name)
                .or_default()
                .push(part.refdes.clone());
        }
    }
    order
        .into_iter()
        .filter_map(|name| {
            let mut refs = serves.remove(name)?;
            refs.sort_by_key(|r| refdes_key(r));
            Some(HardwareRow {
                name: name.to_string(),
                refs,
            })
        })
        .collect()
}

/// One row of a parts table: every part sharing a value *and* a package, plus the
/// orientation cue they share.
struct PartRow {
    value: String,
    package: String,
    refs: Vec<String>,
    polarity: Option<Polarity>,
    through_hole: bool,
}

/// Group parts into table rows by `(value, package)`, first-seen order.
///
/// Grouping by value alone — what the old list did — merges a 0603 47k with a
/// 1206 47k into one "2× 47k" line. On the bench those are two different piles
/// and two different reels, so the table would be telling the builder something
/// false at exactly the moment they're counting parts out.
fn part_rows(parts: &[PlacedPart]) -> Vec<PartRow> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut map: BTreeMap<(String, String), PartRow> = BTreeMap::new();
    for p in parts {
        let value = if p.value.is_empty() {
            "(no value)".to_string()
        } else {
            p.value.clone()
        };
        let package = crate::package::short_name(&p.footprint);
        let key = (value.clone(), package.clone());
        match map.get_mut(&key) {
            Some(row) => {
                row.refs.push(p.refdes.clone());
                // A mixed group takes the stricter reading: if any part in it is
                // polarised, the row has to say so.
                row.polarity = row.polarity.or(p.polarity);
                row.through_hole &= p.through_hole;
            }
            None => {
                order.push(key.clone());
                map.insert(
                    key,
                    PartRow {
                        value,
                        package,
                        refs: vec![p.refdes.clone()],
                        polarity: p.polarity,
                        through_hole: p.through_hole,
                    },
                );
            }
        }
    }
    order.into_iter().filter_map(|k| map.remove(&k)).collect()
}

/// The resistor colour-code swatch for a table row, or empty for anything that
/// isn't a through-hole resistor with a parseable value (an SMD resistor carries
/// a printed numeric code, not bands).
fn row_swatch(row: &PartRow) -> String {
    if !row.through_hole || !is_resistor_group(&row.refs) {
        return String::new();
    }
    match crate::resistor::color_code(&row.value) {
        Some(cc) => format!("<span class=\"swatch\">{}</span>", cc.to_svg(58.0, 18.0)),
        None => String::new(),
    }
}

/// Minimal HTML/XML escaping for text content and attributes.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most fixtures below are surface-mount and exercise ordering, grouping and
    /// rendering — none of which is about SMD policy. They opt in explicitly so
    /// the default (through-hole only) stays testable on its own.
    const SMD: GuideOptions = GuideOptions { include_smd: true };

    use crate::model::{Circuit, Net, Part, PinRef};

    const BOARD: &str = r#"(kicad_pcb
      (gr_rect (start 95 95) (end 130 105) (layer "Edge.Cuts"))
      (footprint "R" (layer "F.Cu") (at 100 100 0)
        (property "Reference" "R1") (pad "1" smd rect (at -1 0) (size 1 1)) (pad "2" smd rect (at 1 0) (size 1 1)))
      (footprint "R" (layer "F.Cu") (at 110 100 0)
        (property "Reference" "R2") (pad "1" smd rect (at -1 0) (size 1 1)) (pad "2" smd rect (at 1 0) (size 1 1)))
      (footprint "U" (layer "F.Cu") (at 120 100 0)
        (property "Reference" "U1") (pad "1" smd rect (at -2 0) (size 1 1)) (pad "8" smd rect (at 2 0) (size 1 1))))"#;

    fn amp() -> Circuit {
        Circuit {
            name: "amp".into(),
            parts: vec![
                Part::new("R1", "9k"),
                Part::new("R2", "1k"),
                Part::new("U1", "TL072"),
            ],
            nets: vec![Net::new("N", vec![PinRef::new("R1", "1")])],
        }
    }

    #[test]
    fn orders_steps_low_profile_first_with_values() {
        let g = build_guide_with(&amp(), BOARD, SMD).unwrap();
        // Resistors before ICs.
        assert_eq!(g.steps.len(), 2);
        assert_eq!(g.steps[0].title, "Resistors");
        assert_eq!(g.steps[1].title, "ICs & sockets");
        assert!(g.steps[1].caution.as_deref().unwrap().contains("pin 1"));
        // Values attached from the circuit.
        assert_eq!(g.steps[0].parts.len(), 2);
        assert!(g.steps[0].parts.iter().any(|p| p.value == "9k"));
        assert_eq!(g.outline, (95.0, 95.0, 130.0, 105.0));
    }

    #[test]
    fn html_highlights_and_is_self_contained() {
        let g = build_guide_with(&amp(), BOARD, SMD).unwrap();
        let html = guide_to_html(&g, None, None);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<svg"));
        // Numbered steps with the resistor step titled in an <h2>.
        assert!(html.contains("class=\"step-of mono\">Step 1 of"));
        assert!(html.contains("class=\"step-title\">Resistors"));
        // The IC step cues pin 1 — on the part's own row, not as a banner.
        assert!(html.contains("pin-1 mark"));
        // Every sheet is identified and numbered: a printed guide gets shuffled.
        assert!(html.contains("class=\"docfoot\""));
        assert!(html.contains("Sheet 1 of"));
        // Each part gets its own tick box, on the sort sheet and on its step.
        assert!(html.matches("class=\"chk\"").count() >= g.steps.len());
    }

    #[test]
    fn a_part_is_listed_once_to_pull_however_many_steps_use_it() {
        // Panel hardware is seated in one step and soldered in another; the sort
        // list is what you go to the parts drawer with, so it must say "3x J1 J2
        // J4" once, not twice.
        let g = build_guide_with(&amp(), BOARD, SMD).unwrap();
        let html = guide_to_html(&g, None, None);
        let sort_sheet = html
            .split("<section class=\"sheet step")
            .next()
            .expect("sort sheet precedes the step sheets");
        for p in g.steps.iter().flat_map(|s| &s.parts) {
            assert_eq!(
                sort_sheet.matches(&format!(">{}", p.refdes)).count(),
                1,
                "{} listed more than once to pull",
                p.refdes
            );
        }
    }

    #[test]
    fn through_hole_resistors_get_color_bands_smd_dont() {
        // A THROUGH-HOLE board with R1=9k, R2=1k, U1=TL072 (color bands are a THT
        // sorting aid, so they only render for through-hole resistors).
        let tht_board = r#"(kicad_pcb
          (gr_rect (start 95 95) (end 130 105) (layer "Edge.Cuts"))
          (footprint "R" (layer "F.Cu") (at 100 100 0)
            (property "Reference" "R1") (pad "1" thru_hole circle (at -1 0) (size 1 1)) (pad "2" thru_hole circle (at 1 0) (size 1 1)))
          (footprint "R" (layer "F.Cu") (at 110 100 0)
            (property "Reference" "R2") (pad "1" thru_hole circle (at -1 0) (size 1 1)) (pad "2" thru_hole circle (at 1 0) (size 1 1)))
          (footprint "U" (layer "F.Cu") (at 120 100 0)
            (property "Reference" "U1") (pad "1" thru_hole circle (at -2 0) (size 1 1)) (pad "8" thru_hole circle (at 2 0) (size 1 1))))"#;
        let g = build_guide(&amp(), tht_board).unwrap();
        let html = guide_to_html(&g, None, None);
        assert!(html.contains("class=\"rband\""));
        assert!(html.contains("<title>brown black red gold</title>")); // 1k
        assert!(html.contains("<title>white black red gold</title>")); // 9k
                                                                       // Each resistor value appears both in the prep sheet and its step list.
        assert_eq!(html.matches("class=\"swatch\"").count(), 4);
        // The IC value (TL072) is not a resistance → no band pictogram for it.
        assert!(!html.contains("<title>TL072"));

        // The SMD fixture (BOARD, smd pads) gets NO color bands — SMD resistors are
        // marked with a printed numeric code, not bands.
        let smd = build_guide_with(&amp(), BOARD, SMD).unwrap();
        assert!(!guide_to_html(&smd, None, None).contains("class=\"rband\""));
    }

    #[test]
    fn photoreal_step_highlights_every_grouped_part_on_one_render() {
        let g = build_guide_with(&amp(), BOARD, SMD).unwrap();
        let png = b"PNGBYTES"; // opaque to photoreal_board_svg (it just base64s it)
        let board = BoardPng {
            png,
            width: 800,
            height: 600,
        };
        // Step 0 groups R1 + R2 — both must be marked on the single embedded render.
        let d = photoreal_board_svg(&board, g.outline, &g.steps[0].parts, 120.0);
        assert_eq!(d.svg.matches("<image").count(), 1, "one shared render");
        assert!(d.svg.contains("data:image/png;base64,"));
        assert!(d.svg.contains(">R1</text>") && d.svg.contains(">R2</text>"));
        // Overlays sit in an mm→px transform group (so highlight_svg is reused).
        assert!(d.svg.contains("<g transform=\"translate("));
        // The frame is cropped to the board, not the whole render: the fixture's
        // 35 × 10mm board in a 4:3 frame would otherwise be a fifth of the picture.
        assert!(
            !d.svg.contains("viewBox=\"0.0 0.0 800.0 600.0\""),
            "must not print the whole letterboxed frame"
        );
        // …and the cropped window has the board's aspect, plus the mm margin.
        let want = (35.0 + 2.0 * CROP_MARGIN_MM) / (10.0 + 2.0 * CROP_MARGIN_MM);
        assert!(
            (d.aspect - want).abs() < 0.01,
            "cropped aspect {} != board aspect {want}",
            d.aspect
        );
    }

    #[test]
    fn a_tall_board_gets_the_page_height_and_a_wide_one_the_page_width() {
        // A 5 HP Eurorack panel: the picture runs the full body height beside the
        // parts table, instead of being squeezed into half a text column.
        let (w, h, beside) = diagram_fit(46.6 / 134.5);
        assert!(beside, "a tall board puts the table alongside");
        assert_eq!(h, STEP_BODY_H_MM);
        assert!(w > 70.0 && w < 80.0, "width {w} follows the aspect");
        // A wide board spans the page instead, with the table underneath.
        let (w, h, beside) = diagram_fit(1.6);
        assert!(!beside);
        assert!(w <= theme::CONTENT_W_MM && h <= STACK_MAX_H_MM);
        // Degenerate aspects must not produce a NaN width in the inline style.
        for bad in [0.0, f64::NAN, f64::INFINITY] {
            let (w, h, _) = diagram_fit(bad);
            assert!(w.is_finite() && h.is_finite(), "aspect {bad}");
        }
    }

    #[test]
    fn power_header_opens_and_the_panel_closes_the_build() {
        // A Eurorack board as it is actually built: SMD parts the fab assembles,
        // a power header on the back, and panel hardware on the front.
        let board = r#"(kicad_pcb
          (gr_rect (start 95 95) (end 130 105) (layer "Edge.Cuts"))
          (footprint "R" (layer "F.Cu") (at 100 100 0)
            (property "Reference" "R1") (pad "1" smd rect (at -1 0) (size 1 1)))
          (footprint "PinHeader_2x05_P2.54mm_Vertical" (layer "B.Cu") (at 98 100 0)
            (property "Reference" "J1") (pad "1" thru_hole circle (at -1 0) (size 1 1)))
          (footprint "Jack_3.5mm_QingPu" (layer "F.Cu") (at 110 100 0)
            (property "Reference" "J2") (pad "1" thru_hole circle (at -1 0) (size 1 1)))
          (footprint "Potentiometer" (layer "F.Cu") (at 120 100 0)
            (property "Reference" "RV1") (pad "1" thru_hole circle (at -2 0) (size 1 1))))"#;
        let circ = Circuit {
            name: "euro".into(),
            parts: vec![
                Part::new("R1", "10k"),
                Part::new("J1", "power"),
                Part::new("J2", "out"),
                Part::new("RV1", "A100k"),
            ],
            nets: vec![],
        };
        let g = build_guide(&circ, board).unwrap();

        // The surface-mount resistor is not the builder's work by default.
        assert!(
            !g.steps
                .iter()
                .any(|s| s.parts.iter().any(|p| p.refdes == "R1")),
            "SMD is skipped unless asked for"
        );
        // The power header is soldered while the board still lies flat, so it
        // opens the build whichever side it is on.
        assert_eq!(g.steps[0].title, "Power header");
        assert!(g.steps[0].parts.iter().any(|p| p.refdes == "J1"));

        // The panel is the jig that aligns the jacks and pots: seat, fit,
        // tighten, and only then solder.
        let titles: Vec<&str> = g.steps.iter().map(|s| s.title.as_str()).collect();
        let seat = titles.iter().position(|t| *t == "Seat the panel hardware");
        let fit = titles
            .iter()
            .position(|t| *t == "Fit the panel and tighten the nuts");
        let solder = titles
            .iter()
            .position(|t| *t == "Solder the panel hardware");
        assert!(seat < fit && fit < solder, "{titles:?}");
        // The jack and the pot are both held for that sequence — and the power
        // header is not, since it is not panel hardware.
        let seated = &g.steps[seat.unwrap()].parts;
        assert!(seated.iter().any(|p| p.refdes == "J2"));
        assert!(seated.iter().any(|p| p.refdes == "RV1"));
        assert!(!seated.iter().any(|p| p.refdes == "J1"));

        // Opting in brings the surface-mount work back.
        let with_smd = build_guide_with(&circ, board, SMD).unwrap();
        assert!(with_smd
            .steps
            .iter()
            .any(|s| s.parts.iter().any(|p| p.refdes == "R1")));
    }

    #[test]
    fn back_side_steps_come_first_across_kinds() {
        // R1 on the front, C1 on the back. Side grouping is outer, so the back
        // capacitor is built before the front resistor even though R precedes C.
        let board = r#"(kicad_pcb
          (gr_rect (start 95 95) (end 130 105) (layer "Edge.Cuts"))
          (footprint "R" (layer "F.Cu") (at 100 100 0)
            (property "Reference" "R1") (pad "1" smd rect (at -1 0) (size 1 1)))
          (footprint "C" (layer "B.Cu") (at 110 100 0)
            (property "Reference" "C1") (pad "1" smd rect (at -1 0) (size 1 1))))"#;
        let circ = Circuit {
            name: "ds".into(),
            parts: vec![Part::new("R1", "1k"), Part::new("C1", "10u")],
            nets: vec![],
        };
        let g = build_guide_with(&circ, board, SMD).unwrap();
        assert_eq!(g.steps[0].title, "Capacitors");
        assert!(g.steps[0].parts.iter().all(|p| p.back), "cap step is back");
        assert_eq!(g.steps[1].title, "Resistors");
        assert!(
            g.steps[1].parts.iter().all(|p| !p.back),
            "resistor is front"
        );
    }

    #[test]
    fn a_fab_reflowed_board_still_reads_as_having_smd_on_it() {
        // The steps are through-hole only (the fab did the SMD), but the kit
        // line must still say the board arrives with parts on it — otherwise a
        // builder sees soldered 0603s and wonders what went wrong.
        // R1/R2 reflowed by the fab, U1 through-hole for the builder.
        let mixed = r#"(kicad_pcb
          (gr_rect (start 95 95) (end 130 105) (layer "Edge.Cuts"))
          (footprint "R" (layer "F.Cu") (at 100 100 0)
            (property "Reference" "R1") (pad "1" smd rect (at -1 0) (size 1 1)) (pad "2" smd rect (at 1 0) (size 1 1)))
          (footprint "R" (layer "F.Cu") (at 110 100 0)
            (property "Reference" "R2") (pad "1" smd rect (at -1 0) (size 1 1)) (pad "2" smd rect (at 1 0) (size 1 1)))
          (footprint "U" (layer "F.Cu") (at 120 100 0)
            (property "Reference" "U1") (pad "1" thru_hole circle (at -2 0) (size 1 1)) (pad "8" thru_hole circle (at 2 0) (size 1 1))))"#;
        let g = build_guide(&amp(), mixed).unwrap();
        assert_eq!(
            g.kit,
            KitType::Mixed,
            "board is mixed even if the kit isn't"
        );
        assert!(g.kit.label().contains("pre-assembled"), "{}", g.kit.label());
        // …and none of those SMD parts became a hand-solder step.
        assert!(g
            .steps
            .iter()
            .flat_map(|s| &s.parts)
            .all(|p| p.through_hole));
    }

    #[test]
    fn kit_type_and_assembly_copy_are_pad_aware() {
        // A THT board (through-hole pads) → detected THT, per-kind THT copy shown.
        let board = r#"(kicad_pcb
          (gr_rect (start 95 95) (end 130 105) (layer "Edge.Cuts"))
          (footprint "R" (layer "F.Cu") (at 100 100 0)
            (property "Reference" "R1") (pad "1" thru_hole circle (at -1 0) (size 1 1)) (pad "2" thru_hole circle (at 1 0) (size 1 1)))
          (footprint "RV" (layer "F.Cu") (at 120 100 0)
            (property "Reference" "RV1") (pad "1" thru_hole circle (at -2 0) (size 1 1)) (pad "2" thru_hole circle (at 2 0) (size 1 1))))"#;
        let circ = Circuit {
            name: "tht".into(),
            parts: vec![Part::new("R1", "10k"), Part::new("RV1", "A100k")],
            nets: vec![],
        };
        let g = build_guide(&circ, board).unwrap();
        assert_eq!(g.kit, KitType::Tht);
        // Resistor step carries THT resistor copy (flush-cut the leads)...
        let r = g.steps.iter().find(|s| s.title == "Resistors").unwrap();
        assert!(r.assembly.as_deref().unwrap().contains("flush-cut"));
        // ...and the pot's per-kind copy survives being grouped into the panel
        // sequence: the locating tab is exactly the detail that grouping loses.
        let seat = g
            .steps
            .iter()
            .find(|s| s.title == "Seat the panel hardware")
            .unwrap();
        assert!(seat.assembly.as_deref().unwrap().contains("locating tab"));

        // The all-SMD fixture (BOARD, smd pads) → detected SMD, SMD copy variant.
        let g2 = build_guide_with(&amp(), BOARD, SMD).unwrap();
        assert_eq!(g2.kit, KitType::Smd);
        let r2 = g2.steps.iter().find(|s| s.title == "Resistors").unwrap();
        assert!(r2.assembly.as_deref().unwrap().contains("reflow"));

        // The kit label + how-to copy surface in the HTML.
        let html = guide_to_html(&g, None, None);
        assert!(html.contains("Through-hole kit"));
        assert!(html.contains("class=\"howto\""));

        assert_eq!(KitType::parse("SMD"), Some(KitType::Smd));
        assert_eq!(KitType::parse("through-hole"), Some(KitType::Tht));
        assert_eq!(KitType::parse("nonsense"), None);
    }

    #[test]
    fn attach_part_notes_groups_and_renders_under_the_step() {
        // amp(): R1, R2 (Resistors step), U1 (ICs step).
        let mut g = build_guide_with(&amp(), BOARD, SMD).unwrap();
        let mut notes = BTreeMap::new();
        // R1 and R2 share a note → one grouped callout; U1 gets its own.
        let trim = vec!["Trim the leads flush after soldering.".to_string()];
        notes.insert("R1".to_string(), trim.clone());
        notes.insert("R2".to_string(), trim.clone());
        notes.insert(
            "U1".to_string(),
            vec!["Use a socket; match the notch.".to_string()],
        );
        g.attach_part_notes(&notes);

        let resistors = g.steps.iter().find(|s| s.title == "Resistors").unwrap();
        assert_eq!(
            resistors.part_notes.len(),
            1,
            "R1+R2 share one grouped note"
        );
        assert_eq!(resistors.part_notes[0].refs, vec!["R1", "R2"]);
        let ics = g.steps.iter().find(|s| s.title == "ICs & sockets").unwrap();
        assert_eq!(ics.part_notes[0].refs, vec!["U1"]);

        // Rendered: the per-part callout carries the grouped refdes + the text.
        let html = guide_to_html(&g, None, None);
        assert!(html.contains("class=\"partnote\""));
        assert!(html.contains("class=\"pn-ref\">R1, R2</b> Trim the leads flush"));
        assert!(html.contains("Use a socket; match the notch."));
    }

    #[test]
    fn mirror_part_x_flips_about_axis_keeping_y() {
        let p = PlacedPart {
            refdes: "C1".into(),
            value: String::new(),
            footprint: String::new(),
            cx: 105.0,
            cy: 100.0,
            bbox: (104.0, 99.0, 106.0, 101.0),
            back: true,
            through_hole: false,
            pin1: Some((104.0, 100.0)),
            polarity: None,
        };
        let m = mirror_part_x(&p, 110.0);
        assert_eq!(m.cx, 115.0); // 2·110 − 105
        assert_eq!(m.bbox, (114.0, 99.0, 116.0, 101.0)); // L/R swapped + mirrored
        assert_eq!(m.pin1, Some((116.0, 100.0)));
        assert_eq!(m.cy, 100.0); // Y unchanged
    }

    /// A rotated part's highlight box must follow the part. Pads are stored
    /// un-rotated in the board file, so parsing has to apply the footprint's
    /// orientation — otherwise a 90° pot or power header gets a box of the wrong
    /// shape in the wrong place (fsn).
    #[test]
    fn pad_boxes_follow_footprint_rotation() {
        // KiCad's sense: +90° maps local (x, y) -> (y, -x).
        assert_eq!(rotate_kicad((3.0, 1.0), 0.0), (3.0, 1.0));
        assert_eq!(rotate_kicad((3.0, 1.0), 90.0), (1.0, -3.0));
        assert_eq!(rotate_kicad((3.0, 1.0), 180.0), (-3.0, -1.0));
        assert_eq!(rotate_kicad((3.0, 1.0), 270.0), (-1.0, 3.0));

        // A two-pad part rotated 90° yields a box that is wide, not tall.
        let pcb = r#"(kicad_pcb (footprint "X" (layer "F.Cu") (at 100 50 90)
              (property "Reference" "RV1")
              (pad "1" thru_hole circle (at 0 0) (size 1 1))
              (pad "2" thru_hole circle (at 0 6) (size 1 1))))"#;
        let parts = parse_board(pcb).expect("parse");
        let p = parts.iter().find(|p| p.refdes == "RV1").unwrap();
        let (x0, y0, x1, y1) = p.bbox;
        assert!(
            (x1 - x0) > (y1 - y0),
            "rotated part's box should be landscape, got {:.1}x{:.1}",
            x1 - x0,
            y1 - y0
        );
        // Pads land at (100,50) and (106,50) after the turn.
        assert!(
            (x0 - 99.5).abs() < 1e-6 && (x1 - 106.5).abs() < 1e-6,
            "{p:?}"
        );
    }

    /// KiCad frames the board's bounding *circle*, so the mm→px scale depends on
    /// the diagonal, not on a fixed fraction of the frame. Pinned against a real
    /// 5 HP render: a 25.4 × 128.5 mm board in a 1568 × 1176 px frame measured
    /// ~9.0 px/mm (the old fixed-fraction constant gave 6.41 and dragged every
    /// highlight off its part).
    #[test]
    fn render_scale_matches_a_real_render() {
        // Measured from actual `kicad-cli pcb render` output at 1568×1176 (what a
        // 1600×1200 request yields). Tolerance is 0.5%, comfortably inside the
        // ±0.35% the model held to across six boards.
        for (w_mm, h_mm, want) in [
            (40.64, 128.50, 8.9764), // slew_limiter, 5 HP Eurorack panel
            (32.44, 16.78, 35.4274), // double_sided
            (64.25, 29.50, 17.9319), // vbom_demo_circuit
            (30.58, 11.45, 37.4962), // rc_ladder
            (34.67, 14.41, 33.0989), // opamp_noninv
        ] {
            let s = render_scale(1568.0, 1176.0, w_mm, h_mm);
            assert!(
                (s / want - 1.0).abs() < 0.005,
                "{w_mm}×{h_mm}mm: got {s:.4}, real render measured {want:.4}"
            );
        }
        // The frame's *width* does not enter the framing — only its height does.
        let a = render_scale(1568.0, 1176.0, 40.64, 128.5);
        let b = render_scale(4000.0, 1176.0, 40.64, 128.5);
        assert_eq!(a, b, "a wider frame must not change the scale");
        // Degenerate inputs fall back rather than dividing by zero.
        assert_eq!(render_scale(100.0, 100.0, 0.0, 0.0), 1.0);
    }

    /// A step's diagram is one face of the board, so a step must not mix faces —
    /// otherwise half its highlights are drawn mirrored-wrong with nothing
    /// saying so. Power headers used to be the one group taken from both faces.
    #[test]
    fn every_step_sits_on_one_face() {
        let hdr = |refdes: &str, back: bool, x: f64| PlacedPart {
            refdes: refdes.into(),
            value: String::new(),
            footprint: "Connector_PinHeader_2.54mm:PinHeader_2x05_P2.54mm_Vertical".into(),
            cx: x,
            cy: 100.0,
            bbox: (x - 2.0, 99.0, x + 2.0, 101.0),
            back,
            through_hole: true,
            pin1: None,
            polarity: None,
        };
        let g = guide_from_parts_with(
            "t",
            vec![hdr("J3", true, 100.0), hdr("J9", false, 120.0)],
            (95.0, 95.0, 130.0, 105.0),
            SMD,
        );
        for step in &g.steps {
            if step.parts.is_empty() {
                continue;
            }
            let backs = step.parts.iter().filter(|p| p.back).count();
            assert!(
                backs == 0 || backs == step.parts.len(),
                "step {:?} mixes faces: {:?}",
                step.title,
                step.parts
                    .iter()
                    .map(|p| (&p.refdes, p.back))
                    .collect::<Vec<_>>()
            );
        }
        // Both headers still get placed, back face first.
        let power: Vec<&BuildStep> = g
            .steps
            .iter()
            .filter(|s| s.title == "Power header")
            .collect();
        assert_eq!(power.len(), 2, "one power-header step per face");
        assert!(step_is_back(power[0]) && !step_is_back(power[1]));
    }

    /// The highlight has to cover what the builder sees drawn on the board, and
    /// on a panel jack the silkscreen outline is four times the pad span.
    #[test]
    fn the_highlight_covers_the_footprint_body_not_just_its_pads() {
        // Pads 2mm apart, courtyard 10mm wide — a jack-shaped mismatch.
        let board = r#"(kicad_pcb
          (gr_rect (start 95 95) (end 130 115) (layer "Edge.Cuts"))
          (footprint "Jack" (layer "F.Cu") (at 110 105 0)
            (property "Reference" "J1")
            (fp_rect (start -5 -7) (end 5 7) (layer "F.CrtYd"))
            (pad "1" thru_hole circle (at -1 0) (size 1 1))
            (pad "2" thru_hole circle (at 1 0) (size 1 1))))"#;
        let parts = parse_board(board).unwrap();
        let j1 = parts.iter().find(|p| p.refdes == "J1").unwrap();
        let (w, h) = (j1.bbox.2 - j1.bbox.0, j1.bbox.3 - j1.bbox.1);
        assert!((w - 10.0).abs() < 0.01, "box spans the courtyard, got {w}");
        assert!((h - 14.0).abs() < 0.01, "got {h}");

        // …but a chip part whose pads reach outside its courtyard keeps them:
        // the builder still needs to see the pads it lands on.
        let chip = r#"(kicad_pcb
          (gr_rect (start 95 95) (end 130 115) (layer "Edge.Cuts"))
          (footprint "R" (layer "F.Cu") (at 110 105 0)
            (property "Reference" "R1")
            (fp_rect (start -1.48 -0.73) (end 1.48 0.73) (layer "F.CrtYd"))
            (pad "1" smd rect (at -0.825 0) (size 0.8 0.95))
            (pad "2" smd rect (at 0.825 0) (size 0.8 0.95))))"#;
        let r1 = parse_board(chip).unwrap();
        let r1 = r1.iter().find(|p| p.refdes == "R1").unwrap();
        let w = r1.bbox.2 - r1.bbox.0;
        assert!((w - 2.96).abs() < 0.01, "courtyard is the wider one: {w}");
        let h = r1.bbox.3 - r1.bbox.1;
        assert!((h - 1.46).abs() < 0.01, "got {h}");
    }

    #[test]
    fn prefix_and_key() {
        assert_eq!(prefix_of("R12"), "R");
        assert_eq!(prefix_of("SW1"), "SW");
        assert!(refdes_key("R2") < refdes_key("R10"));
    }

    #[test]
    fn polarity_is_per_part_footprint_aware() {
        // Ceramic cap: not polarised. Electrolytic/tantalum: +.
        assert_eq!(
            detect_polarity("C1", "Capacitor_SMD:C_0805_2012Metric"),
            None
        );
        assert_eq!(
            detect_polarity("C2", "Capacitor_SMD:CP_Elec_5x5.4"),
            Some(Polarity::Plus)
        );
        assert_eq!(
            detect_polarity("C3", "Capacitor_THT:CP_Radial_Tantalum"),
            Some(Polarity::Plus)
        );
        // Diodes/LEDs → cathode; ICs/transistors/connectors → pin 1.
        assert_eq!(
            detect_polarity("D1", "Diode_SMD:D_SOD-123"),
            Some(Polarity::Cathode)
        );
        assert_eq!(
            detect_polarity("U1", "Package_SO:SOIC-8"),
            Some(Polarity::Pin1)
        );
        assert_eq!(detect_polarity("R1", "Resistor_SMD:R_0805"), None);
    }
}
