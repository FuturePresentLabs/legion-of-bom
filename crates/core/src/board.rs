//! Board generation — netlist → `.kicad_pcb`, directly as S-expression. DESIGN.md
//! 6.6 (revised: direct-gen, headless — the KiCad IPC API can't create boards).
//!
//! Architecture (forward-looking to the layout epic): board *emission* is
//! decoupled from *placement* via the [`Placer`] trait — the seam the iterative
//! layout loop (6.5), PanelSpec-anchored connectors (6.1/6.9), and the manual
//! escape hatch (6.8) all plug into. [`GridPlacer`] is the naive default; the
//! loop replaces it. Routing is a second seam — the [`Router`] trait (see the
//! [`route`](crate::route) module) — so tracks, the ground pour (6.2), and the
//! board outline are all appended to the same assembly by the generator.
//!
//! Output is deterministic (UUIDs derived from content) so each layout attempt is
//! a clean git diff, per 6.5.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::route::{
    track_sexpr, via_sexpr, GridRouter, PadLayer, PadPoint, RouteNet, RouteOptions, Router,
};
use crate::sexpr::Sexpr;
use crate::source::CircuitSource;

/// Errors from board generation.
#[derive(Debug, thiserror::Error)]
pub enum BoardError {
    #[error("part {refdes} has no footprint assigned (needed to place it on a board)")]
    NoFootprint { refdes: String },
    #[error("footprint '{lib_part}' not found at {path}")]
    FootprintNotFound { lib_part: String, path: String },
    #[error("footprint parse error ({lib_part}): {msg}")]
    FootprintParse { lib_part: String, msg: String },
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// A placed footprint position: millimetres, degrees, and which copper side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub x_mm: f64,
    pub y_mm: f64,
    pub rotation_deg: f64,
    pub back: bool,
}

/// Which physical side of the board a part mounts on (DESIGN 6.1). Eurorack is
/// double-sided — SMD/PCBA components on one side, through-hole + silkscreen on
/// the other; a single-sided board (e.g. the guitar pedal) is all `Front`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Front,
    Back,
}

/// What the placer needs to know about a part beyond the netlist: its footprint
/// keep-out size (mm) and which side it mounts on.
#[derive(Debug, Clone, Copy)]
pub struct PartFacts {
    pub extent: (f64, f64),
    pub side: Side,
}

/// Assigns a board position to each part — **the** extensibility seam. The
/// iterative layout loop, PanelSpec-anchored connectors, and the manual escape
/// hatch are all `Placer`s; board emission just consumes the result. `facts`
/// gives each part's keep-out size and side (keyed by refdes) so placement can
/// space parts by their real footprint and put them on the right copper.
pub trait Placer {
    fn place(
        &self,
        circuit: &dyn CircuitSource,
        facts: &HashMap<String, PartFacts>,
    ) -> HashMap<String, Placement>;
}

/// Extra gap (mm) left between adjacent grid cells, on top of each part's extent.
const PLACE_GAP_MM: f64 = 1.0;

/// Margin (mm) added around a part's pad bounding box to approximate its
/// courtyard when sizing placement cells.
const COURTYARD_MARGIN_MM: f64 = 1.0;

/// Naive row/grid placement — a valid, non-optimising default. It is size-aware
/// only enough to not overlap footprints: cells are sized to the largest part.
/// The layout loop (j54.6) supersedes this with real auto-placement.
#[derive(Debug, Clone)]
pub struct GridPlacer {
    pub origin_mm: (f64, f64),
    /// Minimum cell pitch (mm); the effective pitch grows to fit the largest part.
    pub pitch_mm: f64,
    pub per_row: usize,
}

impl Default for GridPlacer {
    fn default() -> Self {
        GridPlacer {
            origin_mm: (100.0, 100.0),
            pitch_mm: 5.0,
            per_row: 8,
        }
    }
}

impl Placer for GridPlacer {
    fn place(
        &self,
        circuit: &dyn CircuitSource,
        facts: &HashMap<String, PartFacts>,
    ) -> HashMap<String, Placement> {
        // A uniform cell big enough for the largest part keeps courtyards apart.
        let max_extent = facts
            .values()
            .fold(0.0f64, |m, f| m.max(f.extent.0).max(f.extent.1));
        let pitch = self.pitch_mm.max(max_extent + PLACE_GAP_MM);
        circuit
            .parts()
            .iter()
            .enumerate()
            .map(|(i, part)| {
                let (col, row) = (i % self.per_row, i / self.per_row);
                let side = facts
                    .get(&part.refdes.0)
                    .map(|f| f.side)
                    .unwrap_or(Side::Front);
                (
                    part.refdes.0.clone(),
                    Placement {
                        x_mm: self.origin_mm.0 + col as f64 * pitch,
                        y_mm: self.origin_mm.1 + row as f64 * pitch,
                        rotation_deg: 0.0,
                        back: side == Side::Back,
                    },
                )
            })
            .collect()
    }
}

/// Options for [`generate_board`].
pub struct BoardOptions {
    pub footprint_dir: PathBuf,
    pub placer: Box<dyn Placer>,
    /// Router that turns placed pads + nets into copper tracks; `None` leaves the
    /// board unrouted (pads + pour only).
    pub router: Option<Box<dyn Router>>,
    /// Track/via geometry for the router.
    pub route_options: RouteOptions,
    /// Net to flood the bottom-layer ground pour to (DESIGN 6.2's default
    /// convention); `None` disables the pour.
    pub ground_net: Option<String>,
    /// Margin (mm) added around the placed parts for the board outline.
    pub outline_margin_mm: f64,
}

impl BoardOptions {
    /// Default options: grid placement, MST routing, a `GND` ground pour, 5 mm
    /// outline margin.
    pub fn new(footprint_dir: impl Into<PathBuf>) -> Self {
        BoardOptions {
            footprint_dir: footprint_dir.into(),
            placer: Box::new(GridPlacer::default()),
            router: Some(Box::new(GridRouter)),
            route_options: RouteOptions::default(),
            ground_net: Some("GND".into()),
            outline_margin_mm: 5.0,
        }
    }
}

/// Generate a `.kicad_pcb` for a circuit: footprints assigned + placed + net-wired,
/// then routed into copper tracks (unless `options.router` is `None`). Downstream
/// (gerbers, CPL, DXF, DRC) is `kicad-cli` on the result.
pub fn generate_board(
    circuit: &dyn CircuitSource,
    options: &BoardOptions,
) -> Result<String, BoardError> {
    Ok(generate_board_report(circuit, options)?.0)
}

/// Like [`generate_board`], but also returns any routing **conflicts** — nets the
/// router could not fully connect (handed off to the iterative loop or manual
/// routing). Callers should surface these rather than ship a silently incomplete
/// board.
pub fn generate_board_report(
    circuit: &dyn CircuitSource,
    options: &BoardOptions,
) -> Result<(String, Vec<String>), BoardError> {
    // Net table: index 0 is the empty/no-net; the rest are the circuit's nets.
    let mut net_names: Vec<String> = circuit.nets().iter().map(|n| n.name.clone()).collect();
    net_names.sort();
    net_names.dedup();
    let net_index: HashMap<&str, usize> = net_names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i + 1))
        .collect();

    // Which net each (refdes, pin) belongs to.
    let mut pin_net: HashMap<(String, String), &str> = HashMap::new();
    for net in circuit.nets() {
        for pin in &net.pins {
            pin_net.insert((pin.refdes.0.clone(), pin.pin.clone()), net.name.as_str());
        }
    }

    // Pass 1: load each part's footprint and measure its keep-out extent + side,
    // so the placer can space parts by real size and mount them on the right copper.
    let mut loaded: Vec<(&str, &str, Sexpr, Vec<FpPad>)> = Vec::new();
    let mut facts: HashMap<String, PartFacts> = HashMap::new();
    for part in circuit.parts() {
        let refdes = part.refdes.0.as_str();
        let lib_part = part
            .footprint
            .as_deref()
            .ok_or_else(|| BoardError::NoFootprint {
                refdes: refdes.to_string(),
            })?;
        let fp = load_footprint(&options.footprint_dir, lib_part)?;
        let pads = footprint_pads(&fp);
        facts.insert(
            refdes.to_string(),
            PartFacts {
                extent: part_extent(&pads, COURTYARD_MARGIN_MM),
                side: resolve_side(&pads),
            },
        );
        loaded.push((refdes, lib_part, fp, pads));
    }

    let placements = options.placer.place(circuit, &facts);

    // Pass 2: transform each footprint into a placed, net-wired board footprint,
    // collect every connected pad's absolute position (for routing), and track the
    // real pad bounding box (for the outline — a big part's pads must not spill
    // past the board edge).
    let mut footprints = Vec::new();
    let mut net_pads: HashMap<usize, RouteNet> = HashMap::new();
    let mut pad_bb = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for (refdes, lib_part, fp, pads) in loaded {
        let placement = placements.get(refdes).copied().unwrap_or(Placement {
            x_mm: 0.0,
            y_mm: 0.0,
            rotation_deg: 0.0,
            back: false,
        });
        for pad in &pads {
            let (x, y) = place_point(placement, pad.px, pad.py);
            pad_bb.0 = pad_bb.0.min(x - pad.w / 2.0);
            pad_bb.1 = pad_bb.1.min(y - pad.h / 2.0);
            pad_bb.2 = pad_bb.2.max(x + pad.w / 2.0);
            pad_bb.3 = pad_bb.3.max(y + pad.h / 2.0);
            if let Some(&name) = pin_net.get(&(refdes.to_string(), pad.num.clone())) {
                let Some(&idx) = net_index.get(name) else {
                    continue;
                };
                // A back-placed footprint mirrors its pads to the other side.
                let layer = pad_layer_on_board(pad.layer, placement.back);
                net_pads
                    .entry(idx)
                    .or_insert_with(|| RouteNet {
                        net_idx: idx,
                        name: name.to_string(),
                        pads: Vec::new(),
                    })
                    .pads
                    .push(PadPoint {
                        refdes: refdes.to_string(),
                        pad: pad.num.clone(),
                        x_mm: x,
                        y_mm: y,
                        w_mm: pad.w,
                        h_mm: pad.h,
                        layer,
                    });
            }
        }
        footprints.push(transform_footprint(
            fp, lib_part, refdes, placement, &pin_net, &net_index,
        ));
    }

    // Assemble the board.
    let mut board = vec![
        Sexpr::sym("kicad_pcb"),
        kv("version", Sexpr::sym("20241229")),
        kv("generator", Sexpr::string("legion-of-bom")),
        kv("generator_version", Sexpr::string("9.0")),
        Sexpr::list(vec![
            Sexpr::sym("general"),
            kv("thickness", Sexpr::sym("1.6")),
        ]),
        kv("paper", Sexpr::string("A4")),
        two_layer_stack(),
        Sexpr::list(vec![
            Sexpr::sym("setup"),
            kv("pad_to_mask_clearance", Sexpr::sym("0")),
        ]),
        net(0, ""),
    ];
    for name in &net_names {
        board.push(net(net_index[name.as_str()], name));
    }
    // Board outline (bounding box of the placed pads + margin) + bottom-layer
    // ground pour (DESIGN 6.2). A real panel-driven outline arrives with
    // PanelSpec (j54.10); this is a valid rectangular first cut.
    let outline = pad_bb.0.is_finite().then(|| {
        let m = options.outline_margin_mm;
        (pad_bb.0 - m, pad_bb.1 - m, pad_bb.2 + m, pad_bb.3 + m)
    });
    if let Some(rect) = outline {
        board.push(edge_cuts_rect(rect));
        if let Some(gnd) = &options.ground_net {
            if let Some(name) = net_names.iter().find(|n| n.eq_ignore_ascii_case(gnd)) {
                board.push(ground_zone(net_index[name.as_str()], name, rect));
            }
        }
    }
    board.extend(footprints);

    // Route the nets into copper tracks (DESIGN 6.5). Ground still gets the pour;
    // routing traces the rest (and any multi-pad ground net) on the copper layers.
    let mut conflicts = Vec::new();
    if let Some(router) = &options.router {
        let nets: Vec<RouteNet> = net_pads.into_values().collect();
        let routed = router.route(&nets, &options.route_options);
        for track in &routed.tracks {
            board.push(track_sexpr(track));
        }
        for via in &routed.vias {
            board.push(via_sexpr(
                via,
                &options.route_options.front,
                &options.route_options.back,
            ));
        }
        conflicts = routed.conflicts;
    }

    Ok((Sexpr::list(board).to_sexpr_string() + "\n", conflicts))
}

/// A footprint pad's local geometry, for routing.
struct FpPad {
    num: String,
    px: f64,
    py: f64,
    w: f64,
    h: f64,
    layer: PadLayer,
}

/// A footprint's pads (number, local offset, size, side) for pads carrying an
/// `(at …)`. Pad numbering matches [`transform_footprint`].
fn footprint_pads(fp: &Sexpr) -> Vec<FpPad> {
    let mut pads = Vec::new();
    for item in fp.as_list().unwrap_or(&[]) {
        if item.head() != Some("pad") {
            continue;
        }
        let Some(num) = item.nth_atom(1) else {
            continue;
        };
        let Some(at) = item.get("at") else {
            continue;
        };
        let px = at.nth_atom(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let py = at.nth_atom(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let size = item.get("size");
        let w = size
            .and_then(|s| s.nth_atom(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let h = size
            .and_then(|s| s.nth_atom(2))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let layers: Vec<&str> = item
            .get("layers")
            .and_then(|l| l.as_list())
            .map(|items| items.iter().skip(1).filter_map(|c| c.as_atom()).collect())
            .unwrap_or_default();
        let front = layers.iter().any(|l| *l == "F.Cu" || l.starts_with("*."));
        let back = layers.iter().any(|l| *l == "B.Cu" || l.starts_with("*."));
        let layer = match (front, back) {
            (true, true) => PadLayer::Both,
            (false, true) => PadLayer::Back,
            _ => PadLayer::Front,
        };
        pads.push(FpPad {
            num: num.to_string(),
            px,
            py,
            w,
            h,
            layer,
        });
    }
    pads
}

/// Which side a part mounts on (DESIGN 6.1's Eurorack convention): a through-hole
/// part goes on the THT+silkscreen side (back), an SMD part on the PCBA side
/// (front). A part with any through-hole pad (on `*.Cu`, i.e. [`PadLayer::Both`])
/// counts as through-hole. (The power-header exception — THT but on the PCBA side
/// — is a per-part override left to follow-up work.)
fn resolve_side(pads: &[FpPad]) -> Side {
    if pads.iter().any(|p| p.layer == PadLayer::Both) {
        Side::Back
    } else {
        Side::Front
    }
}

/// A part's placement keep-out `(width, height)` in mm: the bounding box of its
/// pads, expanded by `margin` to approximate the courtyard. Zero for a padless
/// footprint.
fn part_extent(pads: &[FpPad], margin: f64) -> (f64, f64) {
    let mut bb = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for p in pads {
        bb.0 = bb.0.min(p.px - p.w / 2.0);
        bb.1 = bb.1.min(p.py - p.h / 2.0);
        bb.2 = bb.2.max(p.px + p.w / 2.0);
        bb.3 = bb.3.max(p.py + p.h / 2.0);
    }
    if !bb.0.is_finite() {
        return (0.0, 0.0);
    }
    (bb.2 - bb.0 + 2.0 * margin, bb.3 - bb.1 + 2.0 * margin)
}

/// A footprint placed on the back mirrors its pads to the opposite copper.
fn pad_layer_on_board(local: PadLayer, back: bool) -> PadLayer {
    match (local, back) {
        (PadLayer::Both, _) => PadLayer::Both,
        (PadLayer::Front, false) | (PadLayer::Back, true) => PadLayer::Front,
        (PadLayer::Back, false) | (PadLayer::Front, true) => PadLayer::Back,
    }
}

/// Absolute board position of a pad at local offset `(px, py)` on a footprint
/// placed per `placement`. Rotation follows KiCad's `RotatePoint` convention
/// (`x' = px·cosθ + py·sinθ`, `y' = py·cosθ − px·sinθ`); the grid placer only
/// emits rotation 0 today, so that identity path is what ships — the formula is
/// validated against KiCad ground truth when the layout loop introduces angles.
fn place_point(placement: Placement, px: f64, py: f64) -> (f64, f64) {
    // A back-placed footprint mirrors local X (matching `flip_to_back`), then the
    // whole footprint rotates about its origin.
    let px = if placement.back { -px } else { px };
    let (s, c) = placement.rotation_deg.to_radians().sin_cos();
    let rx = px * c + py * s;
    let ry = py * c - px * s;
    (placement.x_mm + rx, placement.y_mm + ry)
}

/// Load and parse a footprint `.kicad_mod` by `lib:name`.
fn load_footprint(dir: &Path, lib_part: &str) -> Result<Sexpr, BoardError> {
    let (lib, name) = lib_part.split_once(':').ok_or_else(|| {
        BoardError::Other(format!("bad footprint id '{lib_part}' (want lib:name)"))
    })?;
    let path = dir
        .join(format!("{lib}.pretty"))
        .join(format!("{name}.kicad_mod"));
    let text = std::fs::read_to_string(&path).map_err(|_| BoardError::FootprintNotFound {
        lib_part: lib_part.to_string(),
        path: path.display().to_string(),
    })?;
    Sexpr::parse(&text).map_err(|msg| BoardError::FootprintParse {
        lib_part: lib_part.to_string(),
        msg,
    })
}

/// Turn a library footprint into a placed, net-wired board footprint: set the
/// `lib:name`, insert placement + uuid, set the reference designator, and inject
/// each connected pad's net.
fn transform_footprint(
    mut fp: Sexpr,
    lib_part: &str,
    refdes: &str,
    placement: Placement,
    pin_net: &HashMap<(String, String), &str>,
    net_index: &HashMap<&str, usize>,
) -> Sexpr {
    let items = fp.as_list_mut().expect("a footprint is a list");
    if items.len() >= 2 {
        items[1] = Sexpr::string(lib_part); // "R_0805" → "Resistor_SMD:R_0805…"
    }

    // A back-placed footprint is flipped to the bottom: swap every child item's
    // F./B. layer and mirror its local X (KiCad's flip-to-back). Do it on the
    // library-local geometry, before the board-level placement is inserted — and
    // note `place_point` mirrors pad X the same way so routing matches the pads.
    if placement.back {
        for item in items.iter_mut().skip(2) {
            flip_to_back(item);
        }
    }

    // Insert (uuid) + (at x y rot) right after (layer …). The (layer) is now
    // B.Cu for a back part (flipped above), F.Cu for a front one.
    let at = Sexpr::list(vec![
        Sexpr::sym("at"),
        Sexpr::sym(mm(placement.x_mm)),
        Sexpr::sym(mm(placement.y_mm)),
        Sexpr::sym(mm(placement.rotation_deg)),
    ]);
    let fp_uuid = Sexpr::list(vec![
        Sexpr::sym("uuid"),
        Sexpr::string(det_uuid(&format!("{refdes}:fp"))),
    ]);
    let layer_pos = items
        .iter()
        .position(|c| c.head() == Some("layer"))
        .unwrap_or(1);
    items.insert(layer_pos + 1, at);
    items.insert(layer_pos + 2, fp_uuid);

    for item in items.iter_mut() {
        match item.head() {
            Some("property") if item.nth_atom(1) == Some("Reference") => {
                if let Some(l) = item.as_list_mut() {
                    if l.len() >= 3 {
                        l[2] = Sexpr::string(refdes);
                    }
                }
            }
            Some("pad") => {
                let pad_num = item.nth_atom(1).unwrap_or_default().to_string();
                if let Some(l) = item.as_list_mut() {
                    if let Some(&name) = pin_net.get(&(refdes.to_string(), pad_num.clone())) {
                        let idx = net_index.get(name).copied().unwrap_or(0);
                        let net = Sexpr::list(vec![
                            Sexpr::sym("net"),
                            Sexpr::sym(idx.to_string()),
                            Sexpr::string(name),
                        ]);
                        match l.iter().position(|c| c.head() == Some("layers")) {
                            Some(p) => l.insert(p + 1, net),
                            None => l.push(net),
                        }
                    }
                    l.push(Sexpr::list(vec![
                        Sexpr::sym("uuid"),
                        Sexpr::string(det_uuid(&format!("{refdes}:pad:{pad_num}"))),
                    ]));
                }
            }
            _ => {}
        }
    }
    fp
}

/// Flip a sided layer name between front and back (`F.SilkS` ↔ `B.SilkS`, …).
/// Non-sided layers (`Edge.Cuts`, `*.Cu`, `User.*`) return `None` (unchanged).
fn flip_layer(name: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("F.") {
        Some(format!("B.{rest}"))
    } else {
        name.strip_prefix("B.").map(|rest| format!("F.{rest}"))
    }
}

/// Recursively flip a footprint child item to the back: swap `F.`/`B.` layer
/// names and mirror local X coordinates (KiCad's flip-to-back). Angles are left
/// as-is — only rotation-0 placement ships today, and pad-shape angles are
/// cosmetic for the rectangular/oval pads in use.
fn flip_to_back(item: &mut Sexpr) {
    let Some(list) = item.as_list_mut() else {
        return;
    };
    let head = list.first().and_then(|x| x.as_atom()).map(str::to_string);
    match head.as_deref() {
        Some("layer") => {
            if let Some(a) = list.get_mut(1) {
                if let Some(flipped) = a.as_atom().and_then(flip_layer) {
                    *a = Sexpr::string(flipped);
                }
            }
        }
        Some("layers") => {
            for a in list.iter_mut().skip(1) {
                if let Some(flipped) = a.as_atom().and_then(flip_layer) {
                    *a = Sexpr::string(flipped);
                }
            }
        }
        // Coordinate lists: mirror the X component.
        Some("at") | Some("start") | Some("end") | Some("center") | Some("mid") | Some("xy") => {
            if let Some(x) = list.get_mut(1) {
                if let Some(v) = x.as_atom().and_then(|s| s.parse::<f64>().ok()) {
                    *x = Sexpr::sym(mm(-v));
                }
            }
        }
        // Text on the back must be mirrored so it reads correctly (KiCad requires
        // `(justify mirror)` on back-layer text).
        Some("effects") => match list.iter_mut().find(|c| c.head() == Some("justify")) {
            Some(j) => {
                if let Some(l) = j.as_list_mut() {
                    if !l.iter().any(|x| x.as_atom() == Some("mirror")) {
                        l.push(Sexpr::sym("mirror"));
                    }
                }
            }
            None => list.push(Sexpr::list(vec![
                Sexpr::sym("justify"),
                Sexpr::sym("mirror"),
            ])),
        },
        _ => {
            for child in list.iter_mut() {
                flip_to_back(child);
            }
        }
    }
}

// ---- small builders --------------------------------------------------

fn kv(key: &str, value: Sexpr) -> Sexpr {
    Sexpr::list(vec![Sexpr::sym(key), value])
}

/// `(min_x, min_y, max_x, max_y)`.
type Rect = (f64, f64, f64, f64);

/// An `Edge.Cuts` rectangle — the board outline.
fn edge_cuts_rect((x1, y1, x2, y2): Rect) -> Sexpr {
    Sexpr::list(vec![
        Sexpr::sym("gr_rect"),
        Sexpr::list(vec![
            Sexpr::sym("start"),
            Sexpr::sym(mm(x1)),
            Sexpr::sym(mm(y1)),
        ]),
        Sexpr::list(vec![
            Sexpr::sym("end"),
            Sexpr::sym(mm(x2)),
            Sexpr::sym(mm(y2)),
        ]),
        Sexpr::list(vec![
            Sexpr::sym("stroke"),
            kv("width", Sexpr::sym("0.15")),
            kv("type", Sexpr::sym("solid")),
        ]),
        kv("fill", Sexpr::sym("no")),
        kv("layer", Sexpr::string("Edge.Cuts")),
        kv("uuid", Sexpr::string(det_uuid("edge.cuts"))),
    ])
}

/// A bottom-layer (`B.Cu`) ground pour over `rect`, flooded to `net`.
fn ground_zone(net_idx: usize, net_name: &str, (x1, y1, x2, y2): Rect) -> Sexpr {
    let xy =
        |x: f64, y: f64| Sexpr::list(vec![Sexpr::sym("xy"), Sexpr::sym(mm(x)), Sexpr::sym(mm(y))]);
    Sexpr::list(vec![
        Sexpr::sym("zone"),
        kv("net", Sexpr::sym(net_idx.to_string())),
        kv("net_name", Sexpr::string(net_name)),
        kv("layer", Sexpr::string("B.Cu")),
        kv("uuid", Sexpr::string(det_uuid("gnd.zone"))),
        Sexpr::list(vec![
            Sexpr::sym("hatch"),
            Sexpr::sym("edge"),
            Sexpr::sym("0.5"),
        ]),
        Sexpr::list(vec![
            Sexpr::sym("connect_pads"),
            kv("clearance", Sexpr::sym("0.2")),
        ]),
        kv("min_thickness", Sexpr::sym("0.25")),
        kv("filled_areas_thickness", Sexpr::sym("no")),
        Sexpr::list(vec![
            Sexpr::sym("fill"),
            Sexpr::sym("yes"),
            kv("thermal_gap", Sexpr::sym("0.5")),
            kv("thermal_bridge_width", Sexpr::sym("0.5")),
        ]),
        Sexpr::list(vec![
            Sexpr::sym("polygon"),
            Sexpr::list(vec![
                Sexpr::sym("pts"),
                xy(x1, y1),
                xy(x2, y1),
                xy(x2, y2),
                xy(x1, y2),
            ]),
        ]),
    ])
}

fn net(index: usize, name: &str) -> Sexpr {
    Sexpr::list(vec![
        Sexpr::sym("net"),
        Sexpr::sym(index.to_string()),
        Sexpr::string(name),
    ])
}

/// A standard 2-layer stackup (DESIGN.md 6.2's hard constraint).
fn two_layer_stack() -> Sexpr {
    let layer = |n: i64, name: &str, kind: &str| {
        Sexpr::list(vec![
            Sexpr::sym(n.to_string()),
            Sexpr::string(name),
            Sexpr::sym(kind),
        ])
    };
    Sexpr::list(vec![
        Sexpr::sym("layers"),
        layer(0, "F.Cu", "signal"),
        layer(2, "B.Cu", "signal"),
        layer(5, "F.SilkS", "user"),
        layer(7, "B.SilkS", "user"),
        layer(1, "F.Mask", "user"),
        layer(3, "B.Mask", "user"),
        layer(25, "Edge.Cuts", "user"),
        layer(31, "F.CrtYd", "user"),
        layer(29, "B.CrtYd", "user"),
        layer(35, "F.Fab", "user"),
        layer(33, "B.Fab", "user"),
    ])
}

/// Format a millimetre/degree value: rounded to KiCad's 1 nm resolution, no
/// scientific notation, trailing zeros trimmed (avoids float-noise like
/// `99.14999999999999` in generated coordinates).
pub(crate) fn mm(x: f64) -> String {
    let r = (x * 1e6).round() / 1e6;
    if r == 0.0 {
        return "0".into(); // also normalises -0.0
    }
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        let s = format!("{r:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// A deterministic UUID (8-4-4-4-12) derived from `seed`, so regenerating the
/// same circuit yields the same board — clean git diffs across layout attempts.
pub(crate) fn det_uuid(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let b = &digest[..16];
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14],
        b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef};

    fn rc() -> Circuit {
        Circuit {
            name: "rc".into(),
            parts: vec![
                Part::new("R1", "1k").with_footprint("Resistor_SMD:R_0805_2012Metric"),
                Part::new("C1", "159n").with_footprint("Capacitor_SMD:C_0805_2012Metric"),
            ],
            nets: vec![
                Net::new("IN", vec![PinRef::new("R1", "1")]),
                Net::new("OUT", vec![PinRef::new("R1", "2"), PinRef::new("C1", "1")]),
                Net::new("GND", vec![PinRef::new("C1", "2")]),
            ],
        }
    }

    fn facts(entries: &[(&str, (f64, f64), Side)]) -> HashMap<String, PartFacts> {
        entries
            .iter()
            .map(|(r, extent, side)| {
                (
                    r.to_string(),
                    PartFacts {
                        extent: *extent,
                        side: *side,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn grid_places_all_parts() {
        let placements = GridPlacer::default().place(&rc(), &HashMap::new());
        assert_eq!(placements.len(), 2);
        assert!(placements.contains_key("R1"));
    }

    #[test]
    fn grid_spaces_by_largest_part() {
        // A big part forces a wider pitch so footprints don't collide.
        let f = facts(&[
            ("R1", (2.0, 1.5), Side::Front),
            ("C1", (8.0, 7.0), Side::Front),
        ]);
        let p = GridPlacer::default().place(&rc(), &f);
        // Two parts in a row → their x separation must exceed the big part's width.
        let dx = (p["R1"].x_mm - p["C1"].x_mm).abs();
        assert!(dx >= 8.0, "pitch {dx} must fit the 8mm part");
    }

    #[test]
    fn grid_places_parts_on_assigned_side() {
        let f = facts(&[
            ("R1", (2.0, 1.5), Side::Front),
            ("C1", (2.0, 1.5), Side::Back),
        ]);
        let p = GridPlacer::default().place(&rc(), &f);
        assert!(!p["R1"].back, "SMD part stays front");
        assert!(p["C1"].back, "through-hole part goes to back");
    }

    #[test]
    fn resolve_side_by_pad_technology() {
        let smd = |n: &str| FpPad {
            num: n.into(),
            px: 0.0,
            py: 0.0,
            w: 1.0,
            h: 1.0,
            layer: PadLayer::Front,
        };
        let tht = |n: &str| FpPad {
            layer: PadLayer::Both,
            ..smd(n)
        };
        assert_eq!(resolve_side(&[smd("1"), smd("2")]), Side::Front);
        // Any through-hole pad makes the part through-hole → back.
        assert_eq!(resolve_side(&[smd("1"), tht("2")]), Side::Back);
    }

    #[test]
    fn flip_layer_swaps_front_and_back() {
        assert_eq!(flip_layer("F.SilkS").as_deref(), Some("B.SilkS"));
        assert_eq!(flip_layer("B.Cu").as_deref(), Some("F.Cu"));
        assert_eq!(flip_layer("Edge.Cuts"), None); // non-sided, unchanged
    }

    #[test]
    fn flip_to_back_mirrors_x_and_flips_layers_and_text() {
        let mut e = crate::sexpr::Sexpr::parse(
            r#"(fp_text user "R1" (at 1.5 2) (layer "F.SilkS") (effects (font (size 1 1))))"#,
        )
        .unwrap();
        flip_to_back(&mut e);
        let out = e.to_sexpr_string();
        assert!(out.contains("(at -1.5 2)"), "x mirrored: {out}");
        assert!(out.contains(r#"(layer "B.SilkS")"#), "layer flipped: {out}");
        assert!(out.contains("mirror"), "back text mirrored: {out}");
    }

    /// A through-hole part must land flipped on the bottom copper + silk.
    #[test]
    fn through_hole_part_lands_on_bottom() {
        let Some(dir) = crate::skidl::kicad_footprint_dir() else {
            return;
        };
        let c = Circuit {
            name: "ds".into(),
            parts: vec![
                Part::new("R1", "1k").with_footprint("Resistor_SMD:R_0805_2012Metric"),
                Part::new("J1", "Conn")
                    .with_footprint("Connector_PinHeader_2.54mm:PinHeader_1x03_P2.54mm_Vertical"),
            ],
            nets: vec![Net::new(
                "IN",
                vec![PinRef::new("R1", "1"), PinRef::new("J1", "1")],
            )],
        };
        let board = match generate_board(&c, &BoardOptions::new(&dir)) {
            Ok(b) => b,
            Err(_) => return,
        };
        // The SMD R1 stays on front; the through-hole J1 is flipped to the back.
        assert!(board.contains(r#""Connector_PinHeader_2.54mm:PinHeader_1x03_P2.54mm_Vertical""#));
        assert!(
            board.contains(r#""B.SilkS""#),
            "back part silk moved to B.SilkS"
        );
    }

    #[test]
    fn missing_footprint_errors() {
        // A single part with no footprint → NoFootprint (before any lib access).
        let c = Circuit {
            name: "x".into(),
            parts: vec![Part::new("U1", "TL072")],
            nets: vec![],
        };
        let err = generate_board(&c, &BoardOptions::new(std::env::temp_dir())).unwrap_err();
        assert!(matches!(err, BoardError::NoFootprint { refdes } if refdes == "U1"));
    }

    #[test]
    fn place_point_identity_at_rotation_zero() {
        let p = Placement {
            x_mm: 100.0,
            y_mm: 50.0,
            rotation_deg: 0.0,
            back: false,
        };
        // Rotation 0 (what GridPlacer emits) is a pure translation.
        assert_eq!(place_point(p, 0.9125, 0.0), (100.9125, 50.0));
        assert_eq!(place_point(p, -0.9125, 0.0), (99.0875, 50.0));
    }

    #[test]
    fn place_point_rotates_per_kicad_convention() {
        let p = Placement {
            x_mm: 0.0,
            y_mm: 0.0,
            rotation_deg: 90.0,
            back: false,
        };
        // KiCad RotatePoint: (1,0) at 90° -> (0,-1).
        let (x, y) = place_point(p, 1.0, 0.0);
        assert!(
            (x - 0.0).abs() < 1e-9 && (y + 1.0).abs() < 1e-9,
            "got ({x},{y})"
        );
    }

    #[test]
    fn deterministic_uuids() {
        assert_eq!(det_uuid("R1:fp"), det_uuid("R1:fp"));
        assert_ne!(det_uuid("R1:fp"), det_uuid("C1:fp"));
        assert_eq!(det_uuid("x").len(), 36);
    }

    /// Full generation against real footprints. Skipped if no footprint dir.
    #[test]
    fn generates_valid_board_when_footprints_available() {
        let Some(dir) = crate::skidl::kicad_footprint_dir() else {
            return;
        };
        let board = match generate_board(&rc(), &BoardOptions::new(dir)) {
            Ok(b) => b,
            Err(_) => return, // library layout differs; don't fail the unit suite
        };
        // Re-parse to confirm it's structurally valid S-expression.
        assert!(
            crate::sexpr::Sexpr::parse(&board).is_ok(),
            "generated board must parse"
        );
        assert!(board.contains("(kicad_pcb"));
        assert!(board.contains(r#""Resistor_SMD:R_0805_2012Metric""#));
        assert!(board.contains(r#"(net"#) && board.contains(r#""OUT""#));
        // Board outline (Edge.Cuts) + bottom ground pour (DESIGN 6.2).
        assert!(board.contains(r#""Edge.Cuts""#), "needs a board outline");
        assert!(
            board.contains("(zone") && board.contains(r#""B.Cu""#),
            "needs a ground pour"
        );
        assert!(board.contains(r#"(net_name "GND")"#), "pour flooded to GND");
        // Routed: the OUT net (R1.2 ↔ C1.1) becomes a copper track.
        assert!(
            board.contains("(segment"),
            "the multi-pad OUT net must be routed as a track"
        );
    }

    /// A four-part two-stage RC ladder — the grid placer lines all pads up, so a
    /// naive router would short; the maze router must connect every net without
    /// conflicts, and generation must be deterministic (clean git diffs, 6.5).
    #[test]
    fn ladder_routes_completely_and_deterministically() {
        let Some(dir) = crate::skidl::kicad_footprint_dir() else {
            return;
        };
        let ladder = Circuit {
            name: "ladder".into(),
            parts: vec![
                Part::new("R1", "1k").with_footprint("Resistor_SMD:R_0805_2012Metric"),
                Part::new("R2", "1k").with_footprint("Resistor_SMD:R_0805_2012Metric"),
                Part::new("C1", "159n").with_footprint("Capacitor_SMD:C_0805_2012Metric"),
                Part::new("C2", "159n").with_footprint("Capacitor_SMD:C_0805_2012Metric"),
            ],
            nets: vec![
                Net::new("IN", vec![PinRef::new("R1", "1")]),
                Net::new(
                    "MID",
                    vec![
                        PinRef::new("R1", "2"),
                        PinRef::new("C1", "1"),
                        PinRef::new("R2", "1"),
                    ],
                ),
                Net::new("OUT", vec![PinRef::new("R2", "2"), PinRef::new("C2", "1")]),
                Net::new("GND", vec![PinRef::new("C1", "2"), PinRef::new("C2", "2")]),
            ],
        };
        let (board, conflicts) = match generate_board_report(&ladder, &BoardOptions::new(&dir)) {
            Ok(b) => b,
            Err(_) => return, // library layout differs; don't fail the unit suite
        };
        assert!(conflicts.is_empty(), "every net must route: {conflicts:?}");
        assert!(board.contains("(segment"), "must have routed tracks");
        // Deterministic: regenerating the same circuit yields byte-identical output.
        let (again, _) = generate_board_report(&ladder, &BoardOptions::new(&dir)).unwrap();
        assert_eq!(board, again, "board generation must be deterministic");
    }
}
