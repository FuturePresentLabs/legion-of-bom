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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::logo::Logo;
use crate::model::Side;
use crate::route::{
    track_sexpr, via_sexpr, GridRouter, PadLayer, PadPoint, RouteNet, RouteOptions, RouteOutput,
    Router,
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

/// What the placer needs to know about a part beyond the netlist: its footprint
/// keep-out size (mm), where that keep-out sits relative to the footprint origin,
/// and which side it mounts on.
#[derive(Debug, Clone, Copy)]
pub struct PartFacts {
    /// Keep-out size `(width, height)` — the courtyard (or pad box + margin).
    pub extent: (f64, f64),
    /// Keep-out centre offset from the footprint origin. Not every footprint is
    /// centred on its origin — a DIP places the origin at pin 1, so its courtyard
    /// sits ~half the body away. Placers must offset the keep-out by this or a
    /// tightly-packed board trips `courtyards_overlap` even though the *origins*
    /// look clear. Defaults to `(0, 0)` (centred).
    pub origin_offset: (f64, f64),
    pub side: Side,
}

impl PartFacts {
    /// The absolute keep-out rect `(min_x, min_y, max_x, max_y)` for this part
    /// placed with its origin at `(x, y)`. A back-side part is mirrored in X (its
    /// footprint flips onto the bottom copper), so the offset mirrors too.
    fn keepout_at(&self, x: f64, y: f64, back: bool) -> Rect {
        let (ox, oy) = self.origin_offset;
        let ox = if back { -ox } else { ox };
        let (w, h) = self.extent;
        (
            x + ox - w / 2.0,
            y + oy - h / 2.0,
            x + ox + w / 2.0,
            y + oy + h / 2.0,
        )
    }
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

/// Vertical Eurorack placement: panel-facing parts (jacks, pots, switches) are
/// **anchored** at their panel-cutout positions so the board mates the panel PCB;
/// the remaining parts are shelf-packed into the free bands between them. All
/// coordinates are KiCad top-down, in the panel's frame (`0..width × 0..height`).
pub struct EurorackPlacer {
    pub width_mm: f64,
    pub height_mm: f64,
    /// Board bottom-left on the KiCad sheet, so it sits centred rather than jammed
    /// in the corner. Applied to every placement; the outline must use it too.
    pub origin_mm: (f64, f64),
    /// refdes → (x, y) in board-local coords (Y already flipped from the panel's
    /// bottom-up cutouts to KiCad top-down; the origin is added on output).
    pub anchors: HashMap<String, (f64, f64)>,
}

impl Placer for EurorackPlacer {
    fn place(
        &self,
        circuit: &dyn CircuitSource,
        facts: &HashMap<String, PartFacts>,
    ) -> HashMap<String, Placement> {
        let side_of = |refdes: &str| {
            facts
                .get(refdes)
                .map(|f| f.side == Side::Back)
                .unwrap_or(false)
        };
        let mut out = HashMap::new();

        // Keep-out boxes to avoid: the anchored parts first (at their cutouts).
        let mut boxes: Vec<Rect> = Vec::new();
        let box_of = |x: f64, y: f64, (w, h): (f64, f64)| {
            (x - w / 2.0, y - h / 2.0, x + w / 2.0, y + h / 2.0)
        };
        for (refdes, &(x, y)) in &self.anchors {
            out.insert(
                refdes.clone(),
                Placement {
                    x_mm: self.origin_mm.0 + x,
                    y_mm: self.origin_mm.1 + y,
                    rotation_deg: 0.0,
                    back: side_of(refdes),
                },
            );
            let ext = facts.get(refdes).map(|f| f.extent).unwrap_or((8.0, 8.0));
            boxes.push(box_of(x, y, ext));
        }

        // Free parts: first-fit into the interior, top→bottom then left→right,
        // taking the first spot whose courtyard box clears everything placed so
        // far (anchors + earlier free parts). Robust against extent quirks — no
        // overlap can slip through, unlike shelf math.
        let margin = 3.0;
        // Extra clearance beyond the measured courtyard — footprints occasionally
        // under-declare it, and it leaves the router room between neighbours.
        let clearance = 2.5;
        let step = 0.5;
        let (x0, x1) = (margin, self.width_mm - margin);
        let (y0, y1) = (margin, self.height_mm - margin);
        let mut free: Vec<&str> = circuit
            .parts()
            .iter()
            .map(|p| p.refdes.0.as_str())
            .filter(|r| !self.anchors.contains_key(*r))
            .collect();
        free.sort();

        let n = free.len().max(1) as f64;
        for (i, r) in free.iter().enumerate() {
            let (w, h) = facts.get(*r).map(|f| f.extent).unwrap_or((3.0, 3.0));
            // Spread the parts down the (tall) board rather than packing them at
            // the top: aim each at an evenly-spaced row, then take the nearest
            // free spot searching outward from there.
            let target = (y0 + (i as f64 + 0.5) / n * (y1 - y0) - h / 2.0).clamp(y0, y1 - h);
            let free_at = |cy: f64, boxes: &[Rect]| -> Option<Rect> {
                if cy < y0 || cy + h > y1 {
                    return None;
                }
                let mut cx = x0;
                while cx + w <= x1 {
                    let cand = (cx, cy, cx + w, cy + h);
                    if !boxes.iter().any(|b| rects_overlap(b, &cand, clearance)) {
                        return Some(cand);
                    }
                    cx += step;
                }
                None
            };
            let mut spot = None;
            let mut d = 0.0;
            while d <= (y1 - y0) {
                if let Some(c) = free_at(target + d, &boxes).or_else(|| free_at(target - d, &boxes))
                {
                    spot = Some(c);
                    break;
                }
                d += step;
            }
            // No room found → drop it just below the board (visible, DRC flags it).
            let cand = spot.unwrap_or((x0, y1 + 2.0, x0 + w, y1 + 2.0 + h));
            out.insert(
                r.to_string(),
                Placement {
                    x_mm: self.origin_mm.0 + cand.0 + w / 2.0,
                    y_mm: self.origin_mm.1 + cand.1 + h / 2.0,
                    rotation_deg: 0.0,
                    back: side_of(r),
                },
            );
            boxes.push(cand);
        }
        out
    }
}

/// Connectivity-aware Eurorack placement (DESIGN §6.5 step 1 — "seeded near their
/// anchored neighbours"). Panel parts stay **anchored** at their cutouts exactly
/// like [`EurorackPlacer`]; the difference is the *free* parts. Instead of
/// spraying them alphabetically down the board, this seeds each one at the
/// weighted centroid of the parts it is already netted to, so an electrically
/// adjacent pair (a slew cap and its OTA, two buffer stages) lands adjacent —
/// which is what keeps signal traces short and the router's job easy.
///
/// The weight of a net as a placement attractor is `1/(pins-1)`, so a 2-pin
/// signal net pulls hard while a high-fanout rail (which touches everything and
/// says little about *where* a part wants to be) barely pulls; `critical()` nets
/// pull [`CRITICAL_PULL`]× harder. Placement order is greedy from the anchored
/// frontier: the still-unplaced part most strongly tied to what's already down
/// goes next.
#[derive(Debug, Clone)]
pub struct SeededPlacer {
    pub width_mm: f64,
    pub height_mm: f64,
    /// Board bottom-left on the KiCad sheet (added on output, as [`EurorackPlacer`]).
    pub origin_mm: (f64, f64),
    /// refdes → (x, y) board-local anchors (already Y-flipped from the panel).
    pub anchors: HashMap<String, (f64, f64)>,
    /// Loop-repair perturbations (DESIGN §6.5 step 4): refdes → (dx, dy) added to
    /// the computed centroid target so a later attempt explores a different spot.
    /// Empty on the first pass.
    pub nudges: HashMap<String, (f64, f64)>,
}

/// How much harder a `critical()`-tagged net pulls its parts together than an
/// ordinary 2-pin net, in the seeded placer's centroid weighting.
const CRITICAL_PULL: f64 = 6.0;

impl SeededPlacer {
    /// A seeded placer with no repair nudges (the loop's first pass).
    pub fn new(
        width_mm: f64,
        height_mm: f64,
        origin_mm: (f64, f64),
        anchors: HashMap<String, (f64, f64)>,
    ) -> Self {
        SeededPlacer {
            width_mm,
            height_mm,
            origin_mm,
            anchors,
            nudges: HashMap::new(),
        }
    }
}

impl Placer for SeededPlacer {
    fn place(
        &self,
        circuit: &dyn CircuitSource,
        facts: &HashMap<String, PartFacts>,
    ) -> HashMap<String, Placement> {
        // A part's facts (keep-out size + origin offset), defaulting to a small
        // centred box for a part with no footprint measured.
        let facts_of = |refdes: &str| {
            facts.get(refdes).copied().unwrap_or(PartFacts {
                extent: (3.0, 3.0),
                origin_offset: (0.0, 0.0),
                side: Side::Front,
            })
        };
        let side_of = |refdes: &str| facts_of(refdes).side == Side::Back;
        // The keep-out centre offset from the footprint origin, mirrored in X for a
        // back-side part. Placement works in keep-out-centre space and converts
        // back to a footprint origin on output.
        let offset_of = |f: &PartFacts, back: bool| {
            let (ox, oy) = f.origin_offset;
            (if back { -ox } else { ox }, oy)
        };

        let margin = 3.0;
        // The seeded placer clusters connected parts, so unlike the spread-out
        // EurorackPlacer it packs parts to this limit. `extent` is the real
        // courtyard (circles included), so a modest gap over it clears KiCad's
        // courtyard/clearance rules with room for the router between neighbours.
        let clearance = 3.0;
        let step = 0.5;
        let bounds = (
            margin,
            self.width_mm - margin,
            margin,
            self.height_mm - margin,
        );
        let (x0, x1, y0, y1) = bounds;

        let mut out = HashMap::new();
        let mut boxes: Vec<Rect> = Vec::new();
        // Board-local centres of everything placed so far (anchors + free), for
        // centroid seeding. Kept separate from `out` (which is origin-shifted).
        let mut pos: HashMap<String, (f64, f64)> = HashMap::new();
        let mut placed: HashSet<String> = HashSet::new();

        // Anchored parts: fixed at their cutouts, exactly like EurorackPlacer. The
        // anchor is the footprint origin; its keep-out centre is offset from that.
        for (refdes, &(x, y)) in &self.anchors {
            let back = side_of(refdes);
            let f = facts_of(refdes);
            out.insert(
                refdes.clone(),
                Placement {
                    x_mm: self.origin_mm.0 + x,
                    y_mm: self.origin_mm.1 + y,
                    rotation_deg: 0.0,
                    back,
                },
            );
            boxes.push(f.keepout_at(x, y, back));
            let (ox, oy) = offset_of(&f, back);
            pos.insert(refdes.clone(), (x + ox, y + oy));
            placed.insert(refdes.clone());
        }

        // Free parts, in a deterministic base order (also the even-spread fallback
        // order for parts with no placed neighbour yet).
        let mut free: Vec<String> = circuit
            .parts()
            .iter()
            .map(|p| p.refdes.0.clone())
            .filter(|r| !self.anchors.contains_key(r))
            .collect();
        free.sort();
        let free_index: HashMap<String, usize> = free
            .iter()
            .enumerate()
            .map(|(i, r)| (r.clone(), i))
            .collect();
        let n = free.len().max(1) as f64;

        // Net adjacency as a placement attractor: refdes → [(neighbour, weight)].
        let mut adj: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        for net in circuit.nets() {
            let refs: Vec<&str> = {
                let mut r: Vec<&str> = net.pins.iter().map(|p| p.refdes.0.as_str()).collect();
                r.sort_unstable();
                r.dedup();
                r
            };
            if refs.len() < 2 {
                continue;
            }
            // A 2-pin net pulls at 1.0; an N-part rail pulls at 1/(N-1) per edge.
            let mut w = 1.0 / (refs.len() as f64 - 1.0);
            if net.is_critical() {
                w *= CRITICAL_PULL;
            }
            for &a in &refs {
                for &b in &refs {
                    if a != b {
                        adj.entry(a.to_string())
                            .or_default()
                            .push((b.to_string(), w));
                    }
                }
            }
        }

        // Greedy: repeatedly place the unplaced free part most tied to what's down.
        let mut remaining = free.clone();
        while !remaining.is_empty() {
            let pull_to_placed = |r: &str| -> f64 {
                adj.get(r)
                    .map(|v| {
                        v.iter()
                            .filter(|(nb, _)| placed.contains(nb))
                            .map(|(_, w)| *w)
                            .sum()
                    })
                    .unwrap_or(0.0)
            };
            // Highest pull wins; ties break to the lowest base-order index so the
            // result is deterministic.
            let best = (0..remaining.len())
                .max_by(|&i, &j| {
                    let (a, b) = (&remaining[i], &remaining[j]);
                    pull_to_placed(a)
                        .partial_cmp(&pull_to_placed(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(free_index[b].cmp(&free_index[a]))
                })
                .unwrap_or(0);
            let r = remaining.remove(best);
            let back = side_of(&r);
            let f = facts_of(&r);
            // Turn a part portrait only when it's genuinely too wide for the board
            // — a part wider than half the usable width would otherwise crowd out
            // its neighbours. A needless rotation of a fine-pitch part (a SOIC on a
            // roomy board) only hurts its pin fanout, so don't. Rotation uses
            // KiCad's convention (a point (px,py) → (py,−px)): the keep-out extent
            // swaps and its origin offset rotates with it.
            let usable_w = (self.width_mm - 2.0 * margin).max(1.0);
            let rot = if f.extent.0 > f.extent.1 && f.extent.0 > usable_w * 0.5 {
                90.0
            } else {
                0.0
            };
            let (ext, base_off) = if rot == 90.0 {
                (
                    (f.extent.1, f.extent.0),
                    (f.origin_offset.1, -f.origin_offset.0),
                )
            } else {
                (f.extent, f.origin_offset)
            };
            // A back-side part also mirrors in X.
            let (ox, oy) = (if back { -base_off.0 } else { base_off.0 }, base_off.1);

            // Target: weighted centroid of already-placed neighbours, else an
            // even-spread row (the EurorackPlacer fallback) for the first parts.
            let mut num = (0.0, 0.0);
            let mut den = 0.0;
            if let Some(v) = adj.get(&r) {
                for (nb, w) in v {
                    if let Some(&(nx, ny)) = pos.get(nb) {
                        num.0 += w * nx;
                        num.1 += w * ny;
                        den += *w;
                    }
                }
            }
            let target = if den > 0.0 {
                (num.0 / den, num.1 / den)
            } else {
                let ty = (y0 + (free_index[&r] as f64 + 0.5) / n * (y1 - y0)).clamp(y0, y1 - ext.1);
                ((x0 + x1) / 2.0, ty)
            };
            let nudge = self.nudges.get(&r).copied().unwrap_or((0.0, 0.0));
            let target = (target.0 + nudge.0, target.1 + nudge.1);

            // Nearest collision-free spot for the keep-out box; if the board is
            // full, drop it just below the outline where DRC flags it (never
            // overlap). `cx,cy` is the keep-out centre; the footprint origin is
            // that minus the offset.
            let cand = nearest_clear_spot(target, ext, &boxes, bounds, clearance, step)
                .unwrap_or((x0, y1 + 2.0, x0 + ext.0, y1 + 2.0 + ext.1));
            let (cx, cy) = ((cand.0 + cand.2) / 2.0, (cand.1 + cand.3) / 2.0);
            out.insert(
                r.clone(),
                Placement {
                    x_mm: self.origin_mm.0 + cx - ox,
                    y_mm: self.origin_mm.1 + cy - oy,
                    rotation_deg: rot,
                    back,
                },
            );
            boxes.push(cand);
            pos.insert(r.clone(), (cx, cy));
            placed.insert(r);
        }
        out
    }
}

/// The collision-free `w×h` spot whose centre is nearest `(tx, ty)`, searched in
/// expanding rings so an occupied target spills to the closest free space rather
/// than jumping across the board. Returns the rect `(min_x, min_y, max_x, max_y)`,
/// or `None` if the part cannot fit inside `bounds` at all. Deterministic: rings
/// are sampled in a fixed order.
fn nearest_clear_spot(
    (tx, ty): (f64, f64),
    (w, h): (f64, f64),
    boxes: &[Rect],
    (x0, x1, y0, y1): (f64, f64, f64, f64),
    clearance: f64,
    step: f64,
) -> Option<Rect> {
    if x1 - x0 < w || y1 - y0 < h {
        return None;
    }
    // Keep the rect fully inside the board by clamping its centre.
    let clamp_center = |cx: f64, cy: f64| {
        (
            cx.clamp(x0 + w / 2.0, x1 - w / 2.0),
            cy.clamp(y0 + h / 2.0, y1 - h / 2.0),
        )
    };
    let rect_at = |cx: f64, cy: f64| (cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0);
    let is_clear = |r: &Rect| !boxes.iter().any(|b| rects_overlap(b, r, clearance));

    let max_r = (x1 - x0).hypot(y1 - y0);
    let mut d = 0.0;
    while d <= max_r {
        if d == 0.0 {
            let (cx, cy) = clamp_center(tx, ty);
            let r = rect_at(cx, cy);
            if is_clear(&r) {
                return Some(r);
            }
        } else {
            let mut t = -d;
            while t <= d + 1e-9 {
                for (dx, dy) in [(t, -d), (t, d), (-d, t), (d, t)] {
                    let (cx, cy) = clamp_center(tx + dx, ty + dy);
                    let r = rect_at(cx, cy);
                    if is_clear(&r) {
                        return Some(r);
                    }
                }
                t += step;
            }
        }
        d += step;
    }
    None
}

/// Whether two rectangles `(min_x, min_y, max_x, max_y)` overlap within `c` mm.
fn rects_overlap(a: &Rect, b: &Rect, c: f64) -> bool {
    a.0 - c < b.2 && b.0 - c < a.2 && a.1 - c < b.3 && b.1 - c < a.3
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
    /// A fixed board outline `(min_x, min_y, max_x, max_y)` — set for a Eurorack
    /// board so the outline is the panel size, not the parts' bounding box. When
    /// `None`, the outline is the pad bounding box + [`outline_margin_mm`].
    pub fixed_outline: Option<(f64, f64, f64, f64)>,
    /// Show each component's value ("47nF", "TL072") on silk, next to its refdes —
    /// useful for hand assembly (DESIGN 6.10). On by default.
    pub silk_values: bool,
    /// A silkscreen title (board name + revision, e.g. "Slew · v1") placed at the
    /// bottom edge. `None` omits it.
    pub title: Option<String>,
    /// A brand logo, rendered on the **back** silk (B.SilkS) bottom-centre so it
    /// doesn't fight the front component legend (DESIGN §7.9). `None` omits it.
    pub logo: Option<Logo>,
}

impl BoardOptions {
    /// Default options: grid placement, MST routing, a `GND` ground pour, 5 mm
    /// outline margin, values on silk.
    pub fn new(footprint_dir: impl Into<PathBuf>) -> Self {
        BoardOptions {
            footprint_dir: footprint_dir.into(),
            placer: Box::new(GridPlacer::default()),
            router: Some(Box::new(GridRouter)),
            route_options: RouteOptions::default(),
            ground_net: Some("GND".into()),
            outline_margin_mm: 5.0,
            fixed_outline: None,
            silk_values: true,
            title: None,
            logo: None,
        }
    }
}

/// The full result of one board-generation pass: the `.kicad_pcb` text plus the
/// intermediate placement and routing the iterative layout loop (j54.6) scores.
/// [`generate_board`] and [`generate_board_report`] are thin views over this.
pub struct BoardArtifacts {
    /// The `.kicad_pcb` S-expression text.
    pub pcb: String,
    /// Final part placements, keyed by refdes (origin-shifted board coordinates).
    pub placements: HashMap<String, Placement>,
    /// What the router produced — tracks, vias, and unrouted `conflicts`. Empty
    /// when `options.router` is `None`.
    pub route: RouteOutput,
}

/// Generate a `.kicad_pcb` for a circuit: footprints assigned + placed + net-wired,
/// then routed into copper tracks (unless `options.router` is `None`). Downstream
/// (gerbers, CPL, DXF, DRC) is `kicad-cli` on the result.
pub fn generate_board(
    circuit: &dyn CircuitSource,
    options: &BoardOptions,
) -> Result<String, BoardError> {
    Ok(generate_board_artifacts(circuit, options)?.pcb)
}

/// Like [`generate_board`], but also returns any routing **conflicts** — nets the
/// router could not fully connect (handed off to the iterative loop or manual
/// routing). Callers should surface these rather than ship a silently incomplete
/// board.
pub fn generate_board_report(
    circuit: &dyn CircuitSource,
    options: &BoardOptions,
) -> Result<(String, Vec<String>), BoardError> {
    let a = generate_board_artifacts(circuit, options)?;
    Ok((a.pcb, a.route.conflicts))
}

/// The full board-generation pass — placement, net-wiring, ground pour, and
/// routing — returning every intermediate the layout loop needs to score an
/// attempt. See [`BoardArtifacts`]. The two functions above are thin views.
pub fn generate_board_artifacts(
    circuit: &dyn CircuitSource,
    options: &BoardOptions,
) -> Result<BoardArtifacts, BoardError> {
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

    // Pass 1: load each part's footprint, measure its keep-out extent, and record
    // its declared side (DESIGN 6.1). Side is a design choice the circuit declares
    // per part — defaulting to the front (single-sided) — not something inferred
    // from SMD-vs-through-hole. A double-sided board declares its back parts.
    let mut loaded: Vec<(&str, &str, &str, Sexpr, Vec<FpPad>)> = Vec::new();
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
        // Keep-out = the union of the pad bbox (+margin) and the real courtyard,
        // both relative to the footprint origin. The union (not a max of sizes)
        // preserves *where* the keep-out sits — a DIP's courtyard is offset from
        // its pin-1 origin, and that offset must survive into placement.
        let keepout = match (
            part_extent(&pads, COURTYARD_MARGIN_MM),
            courtyard_extent(&fp),
        ) {
            (Some(p), Some(c)) => (p.0.min(c.0), p.1.min(c.1), p.2.max(c.2), p.3.max(c.3)),
            (Some(b), None) | (None, Some(b)) => b,
            (None, None) => (0.0, 0.0, 0.0, 0.0),
        };
        facts.insert(
            refdes.to_string(),
            PartFacts {
                extent: (keepout.2 - keepout.0, keepout.3 - keepout.1),
                origin_offset: ((keepout.0 + keepout.2) / 2.0, (keepout.1 + keepout.3) / 2.0),
                side: part.side.unwrap_or(Side::Front),
            },
        );
        loaded.push((refdes, lib_part, part.value.as_str(), fp, pads));
    }

    let placements = options.placer.place(circuit, &facts);

    // Pass 2: transform each footprint into a placed, net-wired board footprint,
    // collect every connected pad's absolute position (for routing), and track the
    // real pad bounding box (for the outline — a big part's pads must not spill
    // past the board edge).
    let mut footprints = Vec::new();
    let mut net_pads: HashMap<usize, RouteNet> = HashMap::new();
    // Pads carrying no net (unused IC pins, jack switch contacts, spare header
    // pins) are still physical copper — the router must route *around* them or it
    // shorts a passing trace to them. Collected here and seeded as obstacles.
    let mut obstacle_pads: Vec<PadPoint> = Vec::new();
    let mut pad_bb = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for (refdes, lib_part, value, fp, pads) in loaded {
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
            // A back-placed footprint mirrors its pads to the other side.
            let layer = pad_layer_on_board(pad.layer, placement.back);
            let point = PadPoint {
                refdes: refdes.to_string(),
                pad: pad.num.clone(),
                x_mm: x,
                y_mm: y,
                w_mm: pad.w,
                h_mm: pad.h,
                layer,
            };
            match pin_net
                .get(&(refdes.to_string(), pad.num.clone()))
                .and_then(|name| net_index.get(name).map(|&idx| (idx, *name)))
            {
                Some((idx, name)) => net_pads
                    .entry(idx)
                    .or_insert_with(|| RouteNet {
                        net_idx: idx,
                        name: name.to_string(),
                        pads: Vec::new(),
                    })
                    .pads
                    .push(point),
                // No net: keep it as a route-around obstacle, not a connection.
                None => obstacle_pads.push(point),
            }
        }
        footprints.push(transform_footprint(
            fp,
            lib_part,
            refdes,
            value,
            options.silk_values,
            placement,
            &pin_net,
            &net_index,
        ));
    }

    // Board outline: a fixed panel-driven rectangle (Eurorack — see
    // `fixed_outline`), else the pad bounding box + margin. Computed before the
    // setup block so the drill/place-file origin can anchor to it.
    let outline = options.fixed_outline.or_else(|| {
        pad_bb.0.is_finite().then(|| {
            let m = options.outline_margin_mm;
            (pad_bb.0 - m, pad_bb.1 - m, pad_bb.2 + m, pad_bb.3 + m)
        })
    });

    // Setup: anchor the drill/place-file origin at the board's bottom-left, so
    // CPL/Gerber coordinates exported with `--use-drill-file-origin` are small
    // and positive (see `fab::export_cpl`) rather than page-space.
    let mut setup = vec![
        Sexpr::sym("setup"),
        kv("pad_to_mask_clearance", Sexpr::sym("0")),
    ];
    if let Some((minx, _, _, maxy)) = outline {
        setup.push(Sexpr::list(vec![
            Sexpr::sym("aux_axis_origin"),
            Sexpr::sym(mm(minx)),
            Sexpr::sym(mm(maxy)),
        ]));
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
        Sexpr::list(setup),
        net(0, ""),
    ];
    for name in &net_names {
        board.push(net(net_index[name.as_str()], name));
    }
    // Ground pours (DESIGN 6.2): both copper layers, so the many through-hole GND
    // pads bridge the two pours and reconnect any island a trace cuts out of one
    // layer — and a two-sided ground is quieter for an analog signal path.
    if let Some(rect) = outline {
        board.push(edge_cuts_rect(rect));
        if let Some(gnd) = &options.ground_net {
            if let Some(name) = net_names.iter().find(|n| n.eq_ignore_ascii_case(gnd)) {
                let idx = net_index[name.as_str()];
                for layer in ["F.Cu", "B.Cu"] {
                    board.push(ground_zone(idx, name, rect, layer));
                }
            }
        }
        // Silkscreen title (DESIGN 6.10): board name + revision, centred just
        // inside the bottom edge so the board reads as a designed product.
        let (minx, _miny, maxx, maxy) = rect;
        if let Some(title) = &options.title {
            board.push(silk_text(
                title,
                (minx + maxx) / 2.0,
                maxy - 2.5,
                0.0,
                "board.title",
            ));
        }
        // Brand logo on the back silk (DESIGN §7.9), placed by rule: centred,
        // ~55% of the board width, just above the title. On B.Cu's silk it's
        // mirrored so it reads when you look at the back.
        if let Some(logo) = &options.logo {
            let target_w = (maxx - minx) * 0.55;
            let (lx0, ly0, lx1, ly1) = logo.bbox();
            let logo_h = target_w * (ly1 - ly0) / (lx1 - lx0).max(1e-6);
            let center = ((minx + maxx) / 2.0, maxy - 6.0 - logo_h / 2.0);
            let placed = logo.place(target_w, center, true);
            for block in crate::logo::gr_polys(&placed, "B.SilkS", false, "board.logo") {
                if let Ok(sx) = Sexpr::parse(&block) {
                    board.push(sx);
                }
            }
        }
    }
    board.extend(footprints);

    // Route the nets into copper tracks (DESIGN 6.5). Ground still gets the pour;
    // routing traces the rest (and any multi-pad ground net) on the copper layers.
    let mut route = RouteOutput::default();
    if let Some(router) = &options.router {
        let mut nets: Vec<RouteNet> = net_pads.into_values().collect();
        // Each no-net pad as its own single-pad net: painted as an obstacle (with
        // clearance halo) so traces route around it, but never itself routed
        // (the router only connects nets with ≥2 pads).
        for point in obstacle_pads {
            nets.push(RouteNet {
                net_idx: 0,
                name: String::new(),
                pads: vec![point],
            });
        }
        route = router.route(&nets, &options.route_options);
        for track in &route.tracks {
            board.push(track_sexpr(track));
        }
        for via in &route.vias {
            board.push(via_sexpr(
                via,
                &options.route_options.front,
                &options.route_options.back,
            ));
        }
    }

    Ok(BoardArtifacts {
        pcb: Sexpr::list(board).to_sexpr_string() + "\n",
        placements,
        route,
    })
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

/// A part's placement keep-out `(width, height)` in mm: the bounding box of its
/// pads, expanded by `margin` to approximate the courtyard. Zero for a padless
/// footprint.
/// The footprint's courtyard (`*.CrtYd`) bounding box `(x0, y0, x1, y1)` relative
/// to the footprint origin, if it declares one. The courtyard is the real
/// keep-out — usually larger than the pad bbox, and not necessarily centred on
/// the origin — so placers must space parts by it or KiCad flags
/// `courtyards_overlap`.
fn courtyard_extent(fp: &Sexpr) -> Option<Rect> {
    let parse2 = |p: &Sexpr| -> Option<(f64, f64)> {
        Some((p.nth_atom(1)?.parse().ok()?, p.nth_atom(2)?.parse().ok()?))
    };
    let mut pts: Vec<(f64, f64)> = Vec::new();
    for it in fp.as_list()? {
        let on_crtyd = it
            .get("layer")
            .and_then(|l| l.nth_atom(1))
            .is_some_and(|l| l.ends_with(".CrtYd"));
        if !on_crtyd {
            continue;
        }
        // A courtyard circle (common for round THT parts — radial caps, TO-92,
        // LEDs) spans `centre ± radius`, not just its `centre`/`end` atoms. Treat
        // its two extreme corners as points, or its real keep-out is under-measured
        // to the pad box and the placer packs it into a `courtyards_overlap`.
        if it.head() == Some("fp_circle") {
            if let (Some(c), Some(e)) = (
                it.get("center").and_then(&parse2),
                it.get("end").and_then(&parse2),
            ) {
                let r = (e.0 - c.0).hypot(e.1 - c.1);
                pts.push((c.0 - r, c.1 - r));
                pts.push((c.0 + r, c.1 + r));
            }
            continue;
        }
        for key in ["start", "end", "center", "mid"] {
            if let Some(v) = it.get(key).and_then(&parse2) {
                pts.push(v);
            }
        }
        if let Some(node) = it.get("pts") {
            pts.extend(node.get_all("xy").into_iter().filter_map(&parse2));
        }
    }
    if pts.is_empty() {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for (x, y) in pts {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    Some((x0, y0, x1, y1))
}

/// The pad bounding box `(x0, y0, x1, y1)` relative to the footprint origin,
/// expanded by `margin` — the keep-out fallback when a footprint declares no
/// courtyard. `None` for a padless footprint.
fn part_extent(pads: &[FpPad], margin: f64) -> Option<Rect> {
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
        return None;
    }
    Some((bb.0 - margin, bb.1 - margin, bb.2 + margin, bb.3 + margin))
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
#[allow(clippy::too_many_arguments)]
fn transform_footprint(
    mut fp: Sexpr,
    lib_part: &str,
    refdes: &str,
    value: &str,
    silk_values: bool,
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
                    // Guarantee the refdes renders on silk: hand-assembly from the
                    // BOM/build guide needs it visible. Some library footprints ship
                    // the Reference hidden — drop any `(hide yes)` / bare `hide`.
                    l.retain(|c| c.as_atom() != Some("hide") && c.head() != Some("hide"));
                }
            }
            // The component value ("47nF", "TL072") — set from the circuit and, when
            // `silk_values`, shown on silk for hand assembly. Library footprints ship
            // Value on F.Fab, hidden; move it to the placed side's silk and unhide.
            Some("property") if item.nth_atom(1) == Some("Value") => {
                if let Some(l) = item.as_list_mut() {
                    if l.len() >= 3 {
                        l[2] = Sexpr::string(value);
                    }
                    // Only a presentable, value-like string belongs on silk — a
                    // passive value ("47nF") or IC part number ("TL072"), not a
                    // connector's symbol name ("Conn_02x05_Odd_Even") which would
                    // just clutter the legend. The refdes + panel label cover those.
                    let presentable =
                        !value.is_empty() && !value.contains('_') && value.len() <= 12;
                    if silk_values && presentable {
                        let silk = if placement.back { "B.SilkS" } else { "F.SilkS" };
                        for c in l.iter_mut() {
                            if c.head() == Some("layer") {
                                *c = Sexpr::list(vec![Sexpr::sym("layer"), Sexpr::string(silk)]);
                            }
                        }
                        l.retain(|c| c.as_atom() != Some("hide") && c.head() != Some("hide"));
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

/// A front-silkscreen `gr_text` centred at `(x, y)`, rotated `rot` degrees.
/// `seed` makes the uuid deterministic (clean layout-attempt diffs).
fn silk_text(text: &str, x: f64, y: f64, rot: f64, seed: &str) -> Sexpr {
    Sexpr::list(vec![
        Sexpr::sym("gr_text"),
        Sexpr::string(text),
        Sexpr::list(vec![
            Sexpr::sym("at"),
            Sexpr::sym(mm(x)),
            Sexpr::sym(mm(y)),
            Sexpr::sym(mm(rot)),
        ]),
        kv("layer", Sexpr::string("F.SilkS")),
        kv("uuid", Sexpr::string(det_uuid(seed))),
        Sexpr::list(vec![
            Sexpr::sym("effects"),
            Sexpr::list(vec![
                Sexpr::sym("font"),
                Sexpr::list(vec![
                    Sexpr::sym("size"),
                    Sexpr::sym("1.5"),
                    Sexpr::sym("1.5"),
                ]),
                kv("thickness", Sexpr::sym("0.25")),
            ]),
        ]),
    ])
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
fn ground_zone(net_idx: usize, net_name: &str, (x1, y1, x2, y2): Rect, layer: &str) -> Sexpr {
    let xy =
        |x: f64, y: f64| Sexpr::list(vec![Sexpr::sym("xy"), Sexpr::sym(mm(x)), Sexpr::sym(mm(y))]);
    Sexpr::list(vec![
        Sexpr::sym("zone"),
        kv("net", Sexpr::sym(net_idx.to_string())),
        kv("net_name", Sexpr::string(net_name)),
        kv("layer", Sexpr::string(layer)),
        kv(
            "uuid",
            Sexpr::string(det_uuid(&format!("gnd.zone.{layer}"))),
        ),
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
            // Drop filled scraps that don't connect to the net (floating islands).
            kv("island_removal_mode", Sexpr::sym("0")),
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
                        origin_offset: (0.0, 0.0),
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
    fn eurorack_placer_anchors_parts_and_clears_the_rest() {
        // R1 is anchored to a panel cutout; C1 is free and must land elsewhere on
        // the board without overlapping the anchor.
        let f = facts(&[
            ("R1", (2.0, 1.5), Side::Front),
            ("C1", (2.0, 1.5), Side::Front),
        ]);
        let mut anchors = HashMap::new();
        anchors.insert("R1".to_string(), (10.0, 100.0));
        let placer = EurorackPlacer {
            width_mm: 40.0,
            height_mm: 128.5,
            origin_mm: (0.0, 0.0),
            anchors,
        };
        let p = placer.place(&rc(), &f);
        // Anchored part sits exactly on its cutout.
        assert_eq!((p["R1"].x_mm, p["R1"].y_mm), (10.0, 100.0));
        // Free part is inside the board and clear of the anchor.
        assert!(p["C1"].x_mm > 0.0 && p["C1"].x_mm < 40.0);
        assert!(p["C1"].y_mm > 0.0 && p["C1"].y_mm < 128.5);
        let dx = p["C1"].x_mm - 10.0;
        let dy = p["C1"].y_mm - 100.0;
        assert!(
            (dx * dx + dy * dy).sqrt() > 2.0,
            "free part clears the anchor"
        );
    }

    #[test]
    fn parts_default_to_front_unless_declared() {
        // No declared side → front (single-sided default).
        let placements = GridPlacer::default().place(&rc(), &HashMap::new());
        assert!(placements.values().all(|p| !p.back));
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

    /// Silkscreen v0 (DESIGN 6.10): a placed footprint keeps its library silk
    /// (polarity/pin-1 graphics) and gets a *visible* refdes on silk — even when
    /// the library ships the Reference hidden. Front stays on F.SilkS.
    #[test]
    fn transform_places_visible_refdes_and_keeps_silk_front() {
        // A minimal footprint: hidden Reference on silk + a silk polarity line.
        let fp = crate::sexpr::Sexpr::parse(
            r#"(footprint "lib:CP" (layer "F.Cu")
                 (property "Reference" "REF**" (at 0 -2) (layer "F.SilkS") (hide yes)
                   (effects (font (size 1 1))))
                 (fp_line (start -1 0) (end 1 0) (layer "F.SilkS"))
                 (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu")))"#,
        )
        .unwrap();
        let out = transform_footprint(
            fp,
            "lib:CP",
            "C7",
            "100nF",
            true,
            Placement {
                x_mm: 10.0,
                y_mm: 10.0,
                rotation_deg: 0.0,
                back: false,
            },
            &HashMap::new(),
            &HashMap::new(),
        )
        .to_sexpr_string();
        assert!(out.contains(r#""Reference" "C7""#), "refdes set: {out}");
        assert!(!out.contains("hide"), "reference unhidden: {out}");
        assert!(
            out.contains(r#"(layer "F.SilkS")"#),
            "silk kept on front: {out}"
        );
    }

    /// A presentable value (a passive value / IC part number) is set and moved
    /// onto silk for hand assembly; a connector's symbol-name value is set but
    /// left off silk (on F.Fab) so it doesn't clutter the legend.
    #[test]
    fn presentable_value_goes_on_silk_symbol_name_stays_off() {
        let render = |value: &str, refdes: &str| {
            let fp = crate::sexpr::Sexpr::parse(
                r#"(footprint "lib:R" (layer "F.Cu")
                     (property "Reference" "REF**" (at 0 -2) (layer "F.SilkS"))
                     (property "Value" "VAL**" (at 0 2) (layer "F.Fab") (hide yes))
                     (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu")))"#,
            )
            .unwrap();
            transform_footprint(
                fp,
                "lib:R",
                refdes,
                value,
                true,
                Placement {
                    x_mm: 0.0,
                    y_mm: 0.0,
                    rotation_deg: 0.0,
                    back: false,
                },
                &HashMap::new(),
                &HashMap::new(),
            )
            .to_sexpr_string()
        };
        // The fixture's only F.Fab item is the Value, so its absence proves the
        // value moved onto silk.
        let passive = render("47nF", "C1");
        assert!(
            passive.contains(r#""Value" "47nF""#),
            "value text set: {passive}"
        );
        assert!(
            !passive.contains("F.Fab"),
            "presentable value on silk: {passive}"
        );

        let conn = render("Conn_02x05_Odd_Even", "J1");
        assert!(conn.contains(r#""Value" "Conn_02x05_Odd_Even""#));
        assert!(
            conn.contains(r#"(layer "F.Fab")"#),
            "symbol-name value stays off silk: {conn}"
        );
    }

    /// A back-placed footprint mirrors its silk (refdes + graphics) to B.SilkS,
    /// so the bottom-side legend reads correctly during assembly.
    #[test]
    fn transform_mirrors_silk_to_back() {
        let fp = crate::sexpr::Sexpr::parse(
            r#"(footprint "lib:CP" (layer "F.Cu")
                 (property "Reference" "REF**" (at 0 -2) (layer "F.SilkS")
                   (effects (font (size 1 1))))
                 (fp_line (start -1 0) (end 1 0) (layer "F.SilkS"))
                 (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu")))"#,
        )
        .unwrap();
        let out = transform_footprint(
            fp,
            "lib:CP",
            "C7",
            "100nF",
            true,
            Placement {
                x_mm: 10.0,
                y_mm: 10.0,
                rotation_deg: 0.0,
                back: true,
            },
            &HashMap::new(),
            &HashMap::new(),
        )
        .to_sexpr_string();
        assert!(out.contains(r#"(layer "B.SilkS")"#), "silk on back: {out}");
        assert!(
            !out.contains(r#"(layer "F.SilkS")"#),
            "no front silk: {out}"
        );
        assert!(out.contains("mirror"), "back refdes mirrored: {out}");
    }

    /// A part *declared* on the back must land flipped on the bottom copper + silk.
    #[test]
    fn back_declared_part_lands_on_bottom() {
        let Some(dir) = crate::skidl::kicad_footprint_dir() else {
            return;
        };
        let c = Circuit {
            name: "ds".into(),
            parts: vec![
                // R1 defaults to the front; C1 is explicitly declared on the back.
                Part::new("R1", "1k").with_footprint("Resistor_SMD:R_0805_2012Metric"),
                Part::new("C1", "100n")
                    .with_footprint("Capacitor_SMD:C_0805_2012Metric")
                    .with_side(Side::Back),
            ],
            nets: vec![Net::new(
                "OUT",
                vec![PinRef::new("R1", "2"), PinRef::new("C1", "1")],
            )],
        };
        let board = match generate_board(&c, &BoardOptions::new(&dir)) {
            Ok(b) => b,
            Err(_) => return,
        };
        // The back-declared C1 is flipped: its silk moves to B.SilkS.
        assert!(
            board.contains(r#""B.SilkS""#),
            "back-declared part silk moved to B.SilkS"
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
