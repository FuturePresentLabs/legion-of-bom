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
}

/// Resolve a part's polarity from its reference designator and footprint. A
/// ceramic/film cap is unpolarised; an electrolytic/tantalum one is `Plus`.
fn detect_polarity(refdes: &str, footprint: &str) -> Option<Polarity> {
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
    fn label(self) -> &'static str {
        match self {
            KitType::Tht => "Through-hole kit",
            KitType::Smd => "Surface-mount board",
            KitType::Mixed => "Mixed through-hole + SMD build",
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

/// Fraction of the render frame KiCad's `pcb render` fits the board bbox to
/// (orthographic top, centred) — calibrated against real renders. Board-mm →
/// image-px scale is `PHOTOREAL_FIT · min(W/w_mm, H/h_mm)`.
const PHOTOREAL_FIT: f64 = 0.70;

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
    let placed = parse_board(board_pcb)?;
    let values: BTreeMap<&str, &str> = circuit
        .parts()
        .iter()
        .map(|p| (p.refdes.0.as_str(), p.value.as_str()))
        .collect();

    // Attach the circuit value + resolve polarity per part.
    let mut parts: Vec<PlacedPart> = placed
        .into_iter()
        .map(|mut p| {
            if let Some(v) = values.get(p.refdes.as_str()) {
                p.value = v.to_string();
            }
            p.polarity = detect_polarity(&p.refdes, &p.footprint);
            p
        })
        .collect();
    parts.sort_by_key(|p| refdes_key(&p.refdes));

    // Group into ordered steps by side then kind: the BACK side first (mostly SMD
    // + the power header on our boards), then the front — each side low-profile →
    // tall (KINDS order). Anything unmatched becomes a per-side "remaining" step
    // so nothing is silently dropped.
    let mut steps = Vec::new();
    let mut used = vec![false; parts.len()];
    for back in [true, false] {
        let side_has = parts.iter().any(|p| p.back == back);
        if !side_has {
            continue;
        }
        for kind in KINDS {
            let group = take_group(&parts, &mut used, |p| {
                p.back == back && prefix_of(&p.refdes) == kind.prefix
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
        let remaining = take_group(&parts, &mut used, |p| p.back == back);
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

    Ok(BuildGuide {
        name: circuit.name().to_string(),
        outline: board_outline(board_pcb).unwrap_or((0.0, 0.0, 10.0, 10.0)),
        kit: detect_kit(&parts),
        brand: None,
        intro: None,
        tools: Vec::new(),
        kit_cautions: Vec::new(),
        steps,
    })
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

/// Whether a step mounts entirely on the back of the board (so it's shown on the
/// bottom-side render, and flagged so the builder flips the board).
fn step_is_back(step: &BuildStep) -> bool {
    !step.parts.is_empty() && step.parts.iter().all(|p| p.back)
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

/// Parse footprints from a `.kicad_pcb`: refdes, centre, pad bounding box, side.
fn parse_board(board_pcb: &str) -> Result<Vec<PlacedPart>, String> {
    let root = Sexpr::parse(board_pcb)?;
    let mut parts = Vec::new();
    for fp in root.get_all("footprint") {
        let at = fp.get("at");
        let (fx, fy) = (
            at.and_then(|a| a.nth_atom(1)).and_then(f).unwrap_or(0.0),
            at.and_then(|a| a.nth_atom(2)).and_then(f).unwrap_or(0.0),
        );
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
            let (x, y) = (fx + px, fy + py);
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

/// The board outline from the `Edge.Cuts` rectangle, if present.
fn board_outline(board_pcb: &str) -> Option<(f64, f64, f64, f64)> {
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
    let mut body = String::new();
    let sub = format!(
        "Work low-profile → tall{}. Match every polarity / pin-1 marker to the board silkscreen.",
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
            body.push_str(&format!(
                "<h2>Tools you'll need</h2><ul class=\"tools\">{tools}</ul>"
            ));
        }
        body.push_str("</section>");
    }

    // Prep / sort sheet — pull and sort every part up front, in build order.
    body.push_str(
        "<section class=\"prep\"><h2>Before you start — pull &amp; sort your parts</h2>\
         <table><tr><th>Group</th><th>Qty</th><th>Parts</th></tr>",
    );
    for step in &guide.steps {
        let side = if step_is_back(step) {
            " <span class=\"badge\">back</span>"
        } else {
            ""
        };
        let vals = group_by_value(&step.parts)
            .into_iter()
            .map(|(v, refs)| {
                format!(
                    "{}× {} {}<span class=\"refs\">({})</span>",
                    refs.len(),
                    esc(&v),
                    resistor_swatch_html(&step.parts, &refs, &v),
                    esc(&refs.join(", "))
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        body.push_str(&format!(
            "<tr><td>{}{side}</td><td>{}</td><td>{vals}</td></tr>",
            esc(&step.title),
            step.parts.len(),
        ));
    }
    body.push_str("</table></section>");

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
        let svg = match render {
            Some(bp) => photoreal_board_svg(bp, guide.outline, &parts),
            None => schematic_board_svg(guide, &highlight),
        };
        let mut list = String::new();
        for (value, refs) in group_by_value(&step.parts) {
            list.push_str(&format!(
                "<li><b>{}×</b> {} {}— <span class=\"refs\">{}</span></li>",
                refs.len(),
                esc(&value),
                resistor_swatch_html(&step.parts, &refs, &value),
                esc(&refs.join(", "))
            ));
        }
        let howto = step
            .assembly
            .as_deref()
            .map(|a| format!("<p class=\"howto\">{}</p>", esc(a)))
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
        let caution = step
            .caution
            .as_deref()
            .map(|c| format!("<p class=\"caution\">⚠ {}</p>", esc(c)))
            .unwrap_or_default();
        let badge = if back_step {
            "<span class=\"badge back\">↺ BACK side — shown from the back</span>"
        } else {
            ""
        };
        body.push_str(&format!(
            "<section class=\"step\"><header class=\"step-head\">\
             <span class=\"step-no mono\">{stepno}<span class=\"of\"> / {total}</span></span>\
             <div class=\"step-meta\"><h2 class=\"step-title\">{title}{badge}</h2>\
             <p class=\"prog mono\">Place {n} part{s} · {placed} / {total_parts} placed when done</p>\
             </div></header>\
             <div class=\"cols\"><div class=\"diagram\">{svg}</div>\
             <div class=\"parts\"><ul>{list}</ul>{howto}{partnotes}{caution}</div></div></section>",
            stepno = i + 1,
            title = esc(&step.title),
            s = if n == 1 { "" } else { "s" },
        ));
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Build guide — {}</title>\
         <style>{BASE}{CSS}</style></head><body><div class=\"wrap\">{body}</div></body></html>",
        esc(&guide.name),
        BASE = theme::BASE_CSS,
    )
}

/// Build-guide-specific CSS, layered after [`theme::BASE_CSS`]. Numbered steps
/// (the sequence is real), monospace part data, and a bench-note treatment for
/// the per-kind placing copy.
const CSS: &str = "\
.kit-copy{margin:.2rem 0 .4rem}\
.kit-copy .intro{font-size:1.05rem;color:#3a362f;max-width:64ch;margin:.2rem 0 .9rem}\
.kit-copy h2{font-size:.78rem;font-weight:600;text-transform:uppercase;letter-spacing:.08em;\
color:var(--muted);margin:.9rem 0 .4rem}\
.tools{margin:0;padding:0;list-style:none;display:flex;flex-wrap:wrap;gap:.4rem}\
.tools li{font-family:ui-monospace,'SF Mono',Menlo,monospace;font-size:.82rem;background:var(--panel);\
border:1px solid var(--line);border-radius:5px;padding:.25rem .6rem}\
.prep{border-top:1px solid var(--line);padding:1.2rem 0;margin-top:.4rem}\
.prep h2{font-size:1.15rem;font-weight:700;letter-spacing:-.01em;margin:.1rem 0 .7rem}\
.prep table{border-collapse:collapse;width:100%;font-size:.9rem}\
.prep th{text-align:left;color:var(--muted);font-weight:600;font-size:.7rem;letter-spacing:.06em;\
text-transform:uppercase;border-bottom:1.5px solid var(--copper);padding:.4rem .5rem}\
.prep td{border-bottom:1px solid var(--line);padding:.4rem .5rem;vertical-align:top}\
.prep td:first-child{font-weight:600}\
.step{border-top:1px solid var(--line);padding:1.5rem 0}\
.step-head{display:flex;gap:.9rem;align-items:baseline}\
.step-no{font-size:2rem;font-weight:750;color:var(--copper);line-height:1;letter-spacing:-.03em;white-space:nowrap}\
.step-no .of{font-size:.85rem;color:var(--muted);font-weight:500;letter-spacing:0}\
.step-title{font-size:1.2rem;font-weight:700;letter-spacing:-.01em;margin:0}\
.prog{color:var(--muted);margin:.25rem 0 0;font-size:.8rem}\
.badge{display:inline-block;font-size:.66rem;font-weight:700;letter-spacing:.04em;text-transform:uppercase;\
background:var(--copper-soft);color:#8a4f22;border-radius:4px;padding:.1rem .45rem;vertical-align:middle;margin-left:.5rem}\
.badge.back{background:#fbe6d2;color:#8a4b12}\
.cols{display:flex;gap:1.6rem;flex-wrap:wrap;align-items:flex-start;margin-top:.9rem}\
.diagram{flex:1 1 340px;min-width:280px}.parts{flex:1 1 250px}\
.diagram svg{width:100%;height:auto;border:1px solid var(--line);background:#fbfbfa;border-radius:8px}\
ul{margin:0;padding-left:0;list-style:none}\
li{margin:.28rem 0;padding-left:1rem;position:relative}\
li::before{content:'';position:absolute;left:0;top:.55em;width:5px;height:5px;background:var(--copper);border-radius:1px}\
li b{font-family:ui-monospace,'SF Mono',Menlo,monospace}\
.refs{color:var(--muted);font-family:ui-monospace,Menlo,monospace;font-size:.84rem}\
.swatch{display:inline-block;vertical-align:middle;margin:0 .2rem}\
svg.rband{width:58px;height:18px;border:0;background:none;border-radius:0;vertical-align:middle}\
.howto{background:var(--panel);border:1px solid var(--line);border-left:3px solid var(--copper);\
padding:.55rem .8rem;border-radius:5px;margin:.75rem 0 0;color:#4a453d;font-size:.9rem}\
.howto::before{content:'Placing';display:block;font-family:ui-monospace,Menlo,monospace;font-size:.62rem;\
letter-spacing:.16em;text-transform:uppercase;color:var(--copper);margin-bottom:.25rem}\
.partnote{background:var(--copper-soft);border-radius:5px;padding:.5rem .7rem;margin:.5rem 0 0;\
color:#5a4327;font-size:.88rem}\
.pn-ref{font-family:ui-monospace,'SF Mono',Menlo,monospace;font-weight:700;color:#8a4f22;margin-right:.35rem}\
.caution{background:#fdf4de;border-left:3px solid var(--flux);padding:.55rem .8rem;border-radius:5px;\
margin:.6rem 0 0;color:#6b4e07;font-size:.9rem}\
@media print{.prep{break-after:page}\
.step{break-before:page;break-inside:avoid;border-top:none;padding:0}\
.step:first-of-type{break-before:avoid}\
.diagram,.parts{break-inside:avoid}.diagram svg{max-height:150mm}}";

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
    pg.set_line_width(if filled { 0.8 } else { 1.2 });
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
    let fs = (scale * 1.4).clamp(6.0, 12.0);
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

    let m = 36.0; // page margin (pt)
    let cw = pdf::A4_W - 2.0 * m; // content width
    let total = guide.steps.len();
    let total_parts: usize = guide.steps.iter().map(|s| s.parts.len()).sum();
    let any_back = guide.steps.iter().any(step_is_back);
    let (ox0, oy0, ox1, oy1) = guide.outline;
    let cxmm = (ox0 + ox1) / 2.0;
    let (pad, top) = (2.0, pdf::A4_H - m);
    let (bx0, by0, bx1, by1) = (ox0 - pad, oy0 - pad, ox1 + pad, oy1 + pad);
    let (bw, bh) = ((bx1 - bx0).max(1.0), (by1 - by0).max(1.0));

    let mut pages = Vec::new();

    // Prep / sort page — pull and sort every part first, in build order.
    {
        let mut pg = Page::new();
        pg.set_fill(0.1, 0.1, 0.1);
        let title = match &guide.brand {
            Some(b) => format!("{b} — {} build guide", guide.name),
            None => format!("{} - build guide", guide.name),
        };
        pg.text(m, top, 20.0, Font::Bold, &title);
        pg.set_fill(0.3, 0.3, 0.3);
        pg.text(
            m,
            top - 24.0,
            11.0,
            Font::Regular,
            &format!(
                "{}  -  {total} steps, {total_parts} parts. Low-profile to tall{}. \
                 Match every polarity / pin-1 mark to the silkscreen.",
                guide.kit.label(),
                if any_back { ", back side first" } else { "" }
            ),
        );

        // Per-circuit build copy (5uj.5): intro, tools, kit cautions.
        let mut ly = top - 46.0;
        if let Some(intro) = &guide.intro {
            pg.set_fill(0.2, 0.19, 0.17);
            ly = pdf_wrapped(&mut pg, m, ly, cw, 11.0, Font::Regular, intro) - 4.0;
        }
        if !guide.tools.is_empty() {
            pg.set_fill(0.35, 0.35, 0.35);
            let tools = format!("Tools: {}", guide.tools.join("  ·  "));
            ly = pdf_wrapped(&mut pg, m, ly, cw, 10.5, Font::Regular, &tools) - 4.0;
        }
        for c in &guide.kit_cautions {
            pg.set_fill(0.72, 0.45, 0.0);
            ly = pdf_wrapped(&mut pg, m, ly, cw, 10.5, Font::Bold, &format!("[!] {c}")) - 2.0;
        }
        ly -= 6.0;
        pg.set_fill(0.13, 0.13, 0.13);
        pg.text(
            m,
            ly,
            13.0,
            Font::Bold,
            "Before you start — pull & sort your parts:",
        );
        ly -= 22.0;
        for step in &guide.steps {
            let side = if step_is_back(step) { "   [BACK]" } else { "" };
            pg.set_fill(0.13, 0.13, 0.13);
            pg.text(
                m,
                ly,
                11.5,
                Font::Bold,
                &format!("{}  ({}){side}", step.title, step.parts.len()),
            );
            ly -= 15.0;
            for (v, refs) in group_by_value(&step.parts) {
                pg.set_fill(0.35, 0.35, 0.35);
                pg.text(
                    m + 12.0,
                    ly,
                    10.5,
                    Font::Regular,
                    &format!("{}x  {}  ({})", refs.len(), v, refs.join(", ")),
                );
                pdf_resistor_bands(&mut pg, &step.parts, &refs, &v, pdf::A4_W - m, ly);
                ly -= 13.0;
            }
            ly -= 4.0;
        }
        pages.push(pg);
    }

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
        pg.set_fill(0.1, 0.1, 0.1);
        pg.text(
            m,
            top,
            13.0,
            Font::Regular,
            &format!("{} - Step {} of {total}", guide.name, i + 1),
        );
        let title = if back_step {
            format!("{}  [BACK side]", step.title)
        } else {
            step.title.clone()
        };
        pg.text(m, top - 24.0, 19.0, Font::Bold, &title);
        pg.set_fill(0.4, 0.4, 0.4);
        pg.text(
            m,
            top - 40.0,
            10.5,
            Font::Regular,
            &format!(
                "Place {n} part{} - {placed} of {total_parts} placed when done.",
                if n == 1 { "" } else { "s" }
            ),
        );

        // Board diagram under the title/progress.
        let diag_top = top - 58.0;
        let highlight: HashSet<&str> = step.parts.iter().map(|p| p.refdes.as_str()).collect();
        let diag_bottom;

        if let (Some(img), Some(idx)) = (img.as_ref(), idx) {
            // Photoreal render (W×H px, board centred, orthographic). Fit it into
            // the diagram region; map board-mm → image-px → page point so this
            // step's outlined markers land on the bare pads.
            let (iw, ih) = img.size();
            let ds = (cw / iw).min(360.0 / ih); // page pt per image px
            let (dw, dh) = (iw * ds, ih * ds);
            let ix = m + (cw - dw) / 2.0;
            let sc = PHOTOREAL_FIT * (iw / (ox1 - ox0)).min(ih / (oy1 - oy0)); // px/mm
            let (cx, cy) = ((ox0 + ox1) / 2.0, (oy0 + oy1) / 2.0);
            let mapx = |x: f64| ix + (iw / 2.0 + (x - cx) * sc) * ds;
            let mapy = |y: f64| diag_top - (ih / 2.0 + (y - cy) * sc) * ds;
            pg.draw_image(
                [dw, 0.0, 0.0, dh, ix, diag_top - dh],
                (ix, diag_top - dh, dw, dh),
                idx,
            );
            for p in &step_parts {
                pdf_marker(&mut pg, p, &mapx, &mapy, sc * ds, false);
            }
            diag_bottom = diag_top - dh;
        } else {
            let scale = (cw / bw).min(360.0 / bh);
            let rx = m + (cw - bw * scale) / 2.0;
            let mapx = |x: f64| rx + (x - bx0) * scale;
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
                let (cx0, cy0, cx1, cy1) = p.bbox;
                if highlight.contains(p.refdes.as_str()) {
                    pdf_marker(&mut pg, p, &mapx, &mapy, scale, true);
                } else {
                    pg.set_line_width(0.4);
                    pg.set_fill(0.86, 0.86, 0.86);
                    pg.set_stroke(0.67, 0.67, 0.67);
                    pg.rect(
                        mapx(cx0),
                        mapy(cy1),
                        ((cx1 - cx0) * scale).max(1.0),
                        ((cy1 - cy0) * scale).max(1.0),
                        Paint::FillStroke,
                    );
                }
            }
            diag_bottom = diag_top - bh * scale;
        }

        // Parts list + cautions below the diagram.
        let mut ly = diag_bottom - 30.0;
        pg.set_fill(0.13, 0.13, 0.13);
        pg.text(m, ly, 12.0, Font::Bold, "Parts for this step:");
        ly -= 18.0;
        for (value, refs) in group_by_value(&step.parts) {
            pg.set_fill(0.13, 0.13, 0.13);
            pg.text(
                m + 8.0,
                ly,
                11.0,
                Font::Regular,
                &format!("{}x   {}   ({})", refs.len(), value, refs.join(", ")),
            );
            pdf_resistor_bands(&mut pg, &step.parts, &refs, &value, pdf::A4_W - m, ly);
            ly -= 15.0;
        }
        if let Some(a) = &step.assembly {
            ly -= 10.0;
            pg.set_fill(0.20, 0.28, 0.40);
            pg.text(m, ly, 11.0, Font::Bold, "Placing them:");
            ly -= 15.0;
            pg.set_fill(0.28, 0.32, 0.38);
            ly = pdf_wrapped(&mut pg, m, ly, cw, 10.5, Font::Regular, a);
        }
        for pn in &step.part_notes {
            ly -= 7.0;
            pg.set_fill(0.69, 0.41, 0.18); // copper — a part-specific callout
            pg.text(m, ly, 10.5, Font::Bold, &format!("{}:", pn.refs.join(", ")));
            ly -= 14.0;
            pg.set_fill(0.35, 0.27, 0.15);
            ly = pdf_wrapped(
                &mut pg,
                m + 10.0,
                ly,
                cw - 10.0,
                10.5,
                Font::Regular,
                &pn.steps.join(" "),
            );
        }
        if let Some(c) = &step.caution {
            ly -= 8.0;
            pg.set_fill(0.72, 0.45, 0.0);
            pg.text(m, ly, 11.0, Font::Bold, &format!("[!] {c}"));
            ly -= 15.0;
        }
        if back_step {
            pg.set_fill(0.72, 0.45, 0.0);
            pg.text(
                m,
                ly,
                11.0,
                Font::Bold,
                "[back] Flip the board - this step mounts on the BACK (shown from the back).",
            );
        }
        pages.push(pg);
    }
    pdf::document(&pages, &images)
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
    let halo = fs * 0.16;
    // Refdes just above the box so it never crowds the pads.
    let label_y = y - fs * 0.3;
    let mut s = format!(
        "<rect x=\"{x:.3}\" y=\"{y:.3}\" width=\"{w:.3}\" height=\"{h:.3}\" rx=\"0.4\" \
         fill=\"#ffd21f\" fill-opacity=\"{fill_opacity}\" stroke=\"#ff3b30\" stroke-width=\"0.4\"/>\
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

/// The board pad bounding box's short dimension → a legible label size (mm).
fn label_size(outline: (f64, f64, f64, f64)) -> f64 {
    let (x0, y0, x1, y1) = outline;
    ((x1 - x0).min(y1 - y0) / 30.0).clamp(0.7, 2.0)
}

/// A schematic top-down SVG: outline + every part as a box, `highlight`ed parts
/// red, the rest greyed. The fallback when no real KiCad plot is available.
fn schematic_board_svg(guide: &BuildGuide, highlight: &HashSet<&str>) -> String {
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
    let fs = label_size(guide.outline);
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
    svg
}

/// The photorealistic board render (PNG, base64-embedded) with the current step's
/// parts highlighted. `pcb render` is orthographic top-down with the board
/// centred, so board-mm map into the image via `scale = FIT·min(W/w_mm, H/h_mm)`
/// about the image centre (FIT calibrated to KiCad's framing). Overlays are drawn
/// in mm inside an SVG transform group, so [`highlight_svg`] is reused unchanged.
fn photoreal_board_svg(
    board: &BoardPng,
    outline: (f64, f64, f64, f64),
    parts: &[PlacedPart],
) -> String {
    let (x0, y0, x1, y1) = outline;
    let (w, h) = (board.width as f64, board.height as f64);
    let scale = PHOTOREAL_FIT * (w / (x1 - x0)).min(h / (y1 - y0));
    let (cxmm, cymm) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let png = base64::engine::general_purpose::STANDARD.encode(board.png);
    let fs = label_size(outline);
    let overlay: String = parts.iter().map(|p| highlight_svg(p, fs, 0.30)).collect();
    format!(
        "<svg viewBox=\"0 0 {w:.0} {h:.0}\" xmlns=\"http://www.w3.org/2000/svg\">\
         <image x=\"0\" y=\"0\" width=\"{w:.0}\" height=\"{h:.0}\" \
         href=\"data:image/png;base64,{png}\"/>\
         <g transform=\"translate({tx:.3} {ty:.3}) scale({scale:.5}) translate({ntx:.3} {nty:.3})\">\
         {overlay}</g></svg>",
        tx = w / 2.0,
        ty = h / 2.0,
        ntx = -cxmm,
        nty = -cymm,
    )
}

/// Whether a value-group is resistors (refdes prefix `R`, not `RV`/relays).
fn is_resistor_group(refs: &[String]) -> bool {
    refs.first().map(|r| prefix_of(r)) == Some("R")
}

/// Whether a value-group is a *through-hole* resistor group — the gate for the
/// color-code pictogram. SMD resistors carry a printed numeric code (e.g. `513`),
/// not color bands, so they get none; color bands are a THT sorting aid. Looks
/// the group's parts up by refdes in the step.
fn is_tht_resistor_group(step_parts: &[PlacedPart], refs: &[String]) -> bool {
    is_resistor_group(refs)
        && refs.iter().all(|rd| {
            step_parts
                .iter()
                .find(|p| p.refdes.as_str() == rd.as_str())
                .is_some_and(|p| p.through_hole)
        })
}

/// The resistor color-code SVG for a value-group, or empty unless the group is a
/// through-hole resistor whose value parses as a resistance (`"100n"`/`"TL072"`/
/// SMD resistors → nothing).
fn resistor_swatch_html(step_parts: &[PlacedPart], refs: &[String], value: &str) -> String {
    if !is_tht_resistor_group(step_parts, refs) {
        return String::new();
    }
    match crate::resistor::color_code(value) {
        Some(cc) => format!("<span class=\"swatch\">{}</span>", cc.to_svg(58.0, 18.0)),
        None => String::new(),
    }
}

/// Draw a through-hole resistor's color bands as a compact vertical-stripe strip
/// on a PDF page, right edge at `right_x`, sitting on text baseline `y` (a beige
/// backing so light bands read). No-op for SMD / non-resistor / unparseable groups.
fn pdf_resistor_bands(
    pg: &mut Page,
    step_parts: &[PlacedPart],
    refs: &[String],
    value: &str,
    right_x: f64,
    y: f64,
) {
    if !is_tht_resistor_group(step_parts, refs) {
        return;
    }
    let Some(cc) = crate::resistor::color_code(value) else {
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

/// Group a step's parts by value, preserving refdes order.
fn group_by_value(parts: &[PlacedPart]) -> Vec<(String, Vec<String>)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in parts {
        let v = if p.value.is_empty() {
            "(no value)".to_string()
        } else {
            p.value.clone()
        };
        if !map.contains_key(&v) {
            order.push(v.clone());
        }
        map.entry(v).or_default().push(p.refdes.clone());
    }
    order
        .into_iter()
        .map(|v| (v.clone(), map[&v].clone()))
        .collect()
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
        let g = build_guide(&amp(), BOARD).unwrap();
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
        let g = build_guide(&amp(), BOARD).unwrap();
        let html = guide_to_html(&g, None, None);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<svg"));
        // Numbered steps with the resistor step titled in an <h2>.
        assert!(html.contains("class=\"step-no"));
        assert!(html.contains("class=\"step-title\">Resistors"));
        // The IC step cautions about pin 1.
        assert!(html.contains("pin 1"));
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
        let smd = build_guide(&amp(), BOARD).unwrap();
        assert!(!guide_to_html(&smd, None, None).contains("class=\"rband\""));
    }

    #[test]
    fn photoreal_step_highlights_every_grouped_part_on_one_render() {
        let g = build_guide(&amp(), BOARD).unwrap();
        let png = b"PNGBYTES"; // opaque to photoreal_board_svg (it just base64s it)
        let board = BoardPng {
            png,
            width: 800,
            height: 600,
        };
        // Step 0 groups R1 + R2 — both must be marked on the single embedded render.
        let svg = photoreal_board_svg(&board, g.outline, &g.steps[0].parts);
        assert_eq!(svg.matches("<image").count(), 1, "one shared render");
        assert!(svg.contains("data:image/png;base64,"));
        assert!(svg.contains("viewBox=\"0 0 800 600\""));
        assert!(svg.contains(">R1</text>") && svg.contains(">R2</text>"));
        // Overlays sit in an mm→px transform group (so highlight_svg is reused).
        assert!(svg.contains("<g transform=\"translate("));
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
        let g = build_guide(&circ, board).unwrap();
        assert_eq!(g.steps[0].title, "Capacitors");
        assert!(g.steps[0].parts.iter().all(|p| p.back), "cap step is back");
        assert_eq!(g.steps[1].title, "Resistors");
        assert!(
            g.steps[1].parts.iter().all(|p| !p.back),
            "resistor is front"
        );
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
        // ...and the pot step names the locating tab — the per-part-note seam (5uj.3).
        let pot = g
            .steps
            .iter()
            .find(|s| s.title.starts_with("Potentiometers"))
            .unwrap();
        assert!(pot.assembly.as_deref().unwrap().contains("locating tab"));

        // The all-SMD fixture (BOARD, smd pads) → detected SMD, SMD copy variant.
        let g2 = build_guide(&amp(), BOARD).unwrap();
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
        let mut g = build_guide(&amp(), BOARD).unwrap();
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
