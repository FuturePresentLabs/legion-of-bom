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
    Router, Track, Via,
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
#[derive(Debug, Clone)]
pub struct PartFacts {
    /// Keep-out size `(width, height)` — the courtyard (or pad box + margin).
    pub extent: (f64, f64),
    /// Physical **body** span `(width, height)` — the courtyard alone (or raw pad
    /// box), *without* the clearance margins and pin/lug inflation baked into
    /// `extent`. Those margins can distort a part's aspect (an Alpha pot's solder
    /// lugs stretch its keep-out taller than wide even though its body is
    /// landscape), so orientation decisions read this, not `extent`.
    pub body_extent: (f64, f64),
    /// Keep-out centre offset from the footprint origin. Not every footprint is
    /// centred on its origin — a DIP places the origin at pin 1, so its courtyard
    /// sits ~half the body away. Placers must offset the keep-out by this or a
    /// tightly-packed board trips `courtyards_overlap` even though the *origins*
    /// look clear. Defaults to `(0, 0)` (centred).
    pub origin_offset: (f64, f64),
    pub side: Side,
    /// Component height (mm). A part shorter than a sub-board's standoff may sit in
    /// the gap under its body (DESIGN 6.7).
    pub height_mm: f64,
    /// `Some(standoff)` if this part is itself a stacked sub-board.
    pub standoff_mm: Option<f64>,
    /// Through-hole pad keep-outs (mm rects, relative to the footprint origin,
    /// already clearance-expanded). Pins occupy *both* copper layers, so these are
    /// hard keep-outs every body avoids regardless of side — while the part's body
    /// itself is only on its own side.
    pub tht_pads: Vec<Rect>,
    /// Each pad's centre in footprint-local mm, by pad number.
    ///
    /// Decoupling is the reason this exists. "Put the bypass cap near the IC" is
    /// the wrong instruction — the loop that matters runs from the cap to the
    /// chip's *power pin*, and on a 16-pin package that pin is at the end, not
    /// the middle. Measuring part centres let a cap pass the rule at 8.7mm while
    /// sitting 10mm from the pin it was supposed to bypass, or on the far side of
    /// the chip entirely.
    pub pin_offsets: HashMap<String, (f64, f64)>,
}

impl PartFacts {
    /// The absolute keep-out rect `(min_x, min_y, max_x, max_y)` for this part
    /// placed with its origin at `(x, y)`. A back-side part is mirrored in Y (its
    /// footprint flips onto the bottom copper), so the offset mirrors too.
    fn keepout_at(&self, x: f64, y: f64, back: bool) -> Rect {
        let (ox, oy) = self.origin_offset;
        let oy = if back { -oy } else { oy };
        let (w, h) = self.extent;
        (
            x + ox - w / 2.0,
            y + oy - h / 2.0,
            x + ox + w / 2.0,
            y + oy + h / 2.0,
        )
    }

    /// [`keepout_at`](Self::keepout_at) for a footprint rotated `rot_deg`. Quarter
    /// turns swap width and height **and** carry the keep-out's origin offset
    /// around with them — a footprint whose origin isn't its centre (a pot's origin
    /// sits at pin 1, its body several mm away) lands somewhere quite different
    /// once rotated, so the offset must rotate too.
    fn keepout_at_rot(&self, x: f64, y: f64, back: bool, rot_deg: f64) -> Rect {
        // Mirror the *local* Y before rotating, not the rotated result. A
        // back-side footprint is flipped in its own frame and then turned; doing
        // it the other way round is only harmless at 0°/180°, and put a
        // back-mounted 90° power header's keep-out ~10mm from its copper.
        let local = if back {
            (self.origin_offset.0, -self.origin_offset.1)
        } else {
            self.origin_offset
        };
        let (ox, oy) = rotate_local(local, rot_deg);
        let (w, h) = self.extent;
        let (ew, eh) = if (rot_deg / 90.0).round() as i64 % 2 != 0 {
            (h, w)
        } else {
            (w, h)
        };
        (
            x + ox - ew / 2.0,
            y + oy - eh / 2.0,
            x + ox + ew / 2.0,
            y + oy + eh / 2.0,
        )
    }

    /// This part's through-hole pad keep-outs in absolute board coordinates for a
    /// placement with origin at `(x, y)`, rotated `rot_deg` (Y mirrored on the back,
    /// like the pads). Pins occupy both copper layers, so these gate what may sit
    /// opposite them — and a rotated part's pins move, so the rotation must be
    /// applied here or the placer reserves the wrong squares (a rotated pot's
    /// mounting lugs land on top of a back-side SMD pad).
    fn tht_pads_at(&self, x: f64, y: f64, back: bool, rot_deg: f64) -> Vec<Rect> {
        self.tht_pads
            .iter()
            .map(|&r| {
                // Flip in the footprint's own frame first, then rotate — the
                // same order as keepout_at_rot, so a part's pads and its
                // keep-out stay together on the back as well as the front.
                let r = if back { (r.0, -r.3, r.2, -r.1) } else { r };
                let (x0, y0, x1, y1) = rotate_rect(r, rot_deg);
                (x + x0, y + y0, x + x1, y + y1)
            })
            .collect()
    }
}

/// Rotate a footprint-relative rect by `deg` (quarter turns) about the footprint
/// origin, re-normalised to `(min_x, min_y, max_x, max_y)`.
///
/// Uses **KiCad's** footprint-rotation sense — a `(at x y 90)` footprint maps a
/// local pad `(x, y)` to `(y, -x)` (KiCad's Y axis points down) — which is
/// [`rotate_offset`] with the angle negated, and matches the free-part path's own
/// `(origin_offset.1, -origin_offset.0)`. Getting this backwards still yields a
/// DRC-clean board (the reserved squares just land elsewhere) but reserves the
/// wrong space and measurably degrades routing, so it is pinned here deliberately.
fn rotate_rect(rect: Rect, deg: f64) -> Rect {
    let (a, b, c, d) = rect;
    let (x0, y0) = rotate_local((a, b), deg);
    let (x1, y1) = rotate_local((c, d), deg);
    (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
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

    /// Parts pinned to a panel cutout, which downstream passes must not move.
    ///
    /// A jack's position is not this placer's opinion — it is where the hole is.
    /// Legalization repairs physical violations by moving parts, and moving a
    /// panel control off its cutout silently produces a board that will not mate
    /// its own panel. So a violation involving one of these is *reported*, not
    /// quietly fixed: the panel is what needs changing.
    fn anchored(&self) -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }
}

/// Extra gap (mm) left between adjacent grid cells, on top of each part's extent.
const PLACE_GAP_MM: f64 = 1.0;

/// Margin (mm) added around a part's pad bounding box to approximate its
/// courtyard when sizing placement cells.
const COURTYARD_MARGIN_MM: f64 = 1.0;

/// Clearance (mm) around a through-hole pad's keep-out — enough that a neighbour's
/// copper clears the pin (copper-to-copper clearance is 0.2 mm).
const THT_PAD_CLEAR_MM: f64 = 0.4;

/// Placement clearance (mm) left between part courtyards — a routing/soldermask
/// allowance on top of the courtyard the placers space by. This is the knob that
/// trades board density against how much room the (still-crude) router needs to
/// connect neighbours without shorting: 3 mm wasted enormous space; ~1.2 mm is the
/// tightest the current router routes DRC-clean. Lower it as the router improves.
const PLACE_CLEARANCE_MM: f64 = 1.5;

/// Board-edge margin (mm) — keep parts off the outline.
const EDGE_MARGIN_MM: f64 = 1.5;

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

/// Orient a panel-facing control to sit *narrow* on the board's width axis (the
/// HP-limited one), returning 0 or 90 degrees.
///
/// The primary signal is the courtyard: a part whose body is wider than it is
/// tall (an Alpha pot — round body plus solder-lug shoulders spans ~14 mm wide,
/// ~13 mm tall) is turned a quarter-turn so it stands on end, which also lays its
/// pin column into a horizontal row (the ideal, user-directed pot orientation).
/// A jack, taller than wide, is left upright. As a fallback, a square-bodied part
/// whose *pins* form a wide horizontal row is rotated so the pins face into the
/// board.
fn control_rotation(f: &PartFacts) -> f64 {
    // Stand a control narrow on the board's width axis. Decide on the physical
    // *body* (courtyard), not the keep-out — a pot's solder lugs inflate its
    // keep-out taller-than-wide even though the body is landscape, so rotating it a
    // quarter turn lays its pin column into a horizontal row (the ideal). A jack,
    // taller than wide, stays upright. A square-bodied part whose pins form a wide
    // row still rotates so the pins face into the board.
    let (ew, eh) = f.body_extent;
    if ew > eh * 1.05 {
        return 90.0;
    }
    if let Some((pw, ph)) = pad_span(f) {
        if pw > ph * 1.2 {
            return 90.0;
        }
    }
    0.0
}

/// Physical body span `(w, h)` for orientation decisions — the courtyard if the
/// footprint has one, else its raw pad box. Feeds [`PartFacts::body_extent`], the
/// groundwork for orienting a control by its body rather than its keep-out.
fn body_span(courtyard: Option<Rect>, pad_box: Option<Rect>) -> (f64, f64) {
    match courtyard.or(pad_box) {
        Some((x0, y0, x1, y1)) => (x1 - x0, y1 - y0),
        None => (0.0, 0.0),
    }
}

/// Rotate an `(x, y)` offset by a footprint rotation of `deg` (quarter turns).
/// Rotate a footprint-local point into board coordinates, in **KiCad's** sense:
/// an `(at x y 90)` footprint maps a local `(x, y)` to `(y, -x)`, because KiCad's
/// Y axis points down.
///
/// This is the only rotation any caller should want, and it exists because the
/// codebase had both senses in it. [`rotate_rect`] (pads) used KiCad's;
/// `keepout_at_rot` and the two anchor-to-origin conversions used the inverse.
/// Self-consistently, so the placer reserved a box exactly on the cutout — but
/// the *real* footprint rotates KiCad's way, so a 90°-rotated pot's shaft landed
/// ~11mm from the panel hole it was anchored to. Latent on every shipped board
/// so far only because nothing had been rotated yet.
pub(crate) fn rotate_local(p: (f64, f64), deg: f64) -> (f64, f64) {
    rotate_offset(p, -deg)
}

/// Quarter-turn rotation in the mathematical sense (counter-clockwise for a
/// Y-up axis). Prefer [`rotate_local`]; this is its primitive.
fn rotate_offset((x, y): (f64, f64), deg: f64) -> (f64, f64) {
    match (((deg / 90.0).round() as i64) % 4 + 4) % 4 {
        1 => (-y, x),
        2 => (-x, -y),
        3 => (y, -x),
        _ => (x, y),
    }
}

/// Bounding span `(width, height)` of a part's through-hole pads, or `None` when
/// it has none (SMD).
fn pad_span(f: &PartFacts) -> Option<(f64, f64)> {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &(a, b, c, d) in &f.tht_pads {
        x0 = x0.min(a);
        y0 = y0.min(b);
        x1 = x1.max(c);
        y1 = y1.max(d);
    }
    (x1 > x0).then_some((x1 - x0, y1 - y0))
}

/// Whether a footprint id is a Eurorack power header — a 2×N pin header.
fn is_power_header_footprint(footprint: &str) -> bool {
    footprint.contains("PinHeader_2x")
}

/// A Eurorack power header mounts on the **back** of a module board.
///
/// The front face carries the panel controls and sits against the panel, so a
/// shrouded header there would foul it and the ribbon would have nowhere to go.
/// Mounting it on the back also puts its silkscreen — refdes and the −12 V mark
/// from [`power_polarity_silk`] — on the face the builder is looking at while
/// they install it. This is a house rule for [`EurorackPlacer`] specifically, and
/// it overrides the circuit's declared side: SKiDL writes `Side = front` by
/// default on every part, so honouring that declaration here would mean no
/// Eurorack board ever gets it right.
const POWER_HEADER_ON_BACK: bool = true;

/// Refdes of the free (non-anchored) Eurorack power headers — a 2×N pin header
/// that exits the board (not the panel), which both placers lay horizontal
/// against the top edge, clear of the control field.
fn power_header_refdes(
    circuit: &dyn CircuitSource,
    anchors: &HashMap<String, (f64, f64)>,
) -> Vec<String> {
    circuit
        .parts()
        .iter()
        .filter(|p| !anchors.contains_key(&p.refdes.0))
        .filter(|p| {
            p.footprint
                .as_deref()
                .is_some_and(is_power_header_footprint)
        })
        .map(|p| p.refdes.0.clone())
        .collect()
}

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
    fn anchored(&self) -> std::collections::HashSet<String> {
        self.anchors.keys().cloned().collect()
    }

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
        // A landscape footprint (clearly wider than tall) is stood on end so panel
        // controls — pots especially — sit portrait with their pins facing into the
        // board, not splayed sideways. The keep-out swaps with it.
        let oriented = |refdes: &str| -> (f64, (f64, f64)) {
            let ext = facts.get(refdes).map(|f| f.extent).unwrap_or((8.0, 8.0));
            let rot = facts.get(refdes).map(control_rotation).unwrap_or(0.0);
            if rot != 0.0 {
                (rot, (ext.1, ext.0))
            } else {
                (0.0, ext)
            }
        };
        for (refdes, &(x, y)) in &self.anchors {
            let (rotation_deg, ext) = oriented(refdes);
            let back = side_of(refdes);
            // Align the control's mount point (courtyard centre ≈ shaft/barrel) to
            // the cutout by placing the footprint origin at `cutout - offset`.
            let (ox, oy) = facts
                .get(refdes)
                .map(|f| f.origin_offset)
                .unwrap_or((0.0, 0.0));
            let (ox, oy) = if back { (ox, -oy) } else { (ox, oy) };
            let (rox, roy) = rotate_local((ox, oy), rotation_deg);
            out.insert(
                refdes.clone(),
                Placement {
                    x_mm: self.origin_mm.0 + x - rox,
                    y_mm: self.origin_mm.1 + y - roy,
                    rotation_deg,
                    back,
                },
            );
            // Keep-out sits at the mount point (the cutout), where the body now is.
            boxes.push(box_of(x, y, ext));
        }

        let margin = EDGE_MARGIN_MM;

        // The Eurorack power header exits the board (not the panel), so lay it
        // horizontal and tuck it against the top edge, clear of the control field.
        let power_headers: Vec<&str> = circuit
            .parts()
            .iter()
            .filter(|p| !self.anchors.contains_key(&p.refdes.0))
            .filter(|p| {
                p.footprint
                    .as_deref()
                    .is_some_and(|f| f.contains("PinHeader_2x"))
            })
            .map(|p| p.refdes.0.as_str())
            .collect();
        let mut header_x = margin;
        for r in &power_headers {
            let (ew, eh) = facts.get(*r).map(|f| f.extent).unwrap_or((12.0, 5.0));
            // Horizontal = long axis along X; rotate a portrait header 90°.
            let (rotation_deg, (w, h)) = if eh > ew {
                (90.0, (eh, ew))
            } else {
                (0.0, (ew, eh))
            };
            let cx = (header_x + w / 2.0).min(self.width_mm - margin - w / 2.0);
            let cy = margin + h / 2.0;
            out.insert(
                r.to_string(),
                Placement {
                    x_mm: self.origin_mm.0 + cx,
                    y_mm: self.origin_mm.1 + cy,
                    rotation_deg,
                    back: POWER_HEADER_ON_BACK || side_of(r),
                },
            );
            boxes.push(box_of(cx, cy, (w, h)));
            header_x += w + 2.0;
        }

        // Free parts: first-fit into the interior, top→bottom then left→right,
        // taking the first spot whose courtyard box clears everything placed so
        // far (anchors + header + earlier free parts). Robust against extent
        // quirks — no overlap can slip through, unlike shelf math.
        let clearance = PLACE_CLEARANCE_MM;
        let step = 0.5;
        let (x0, x1) = (margin, self.width_mm - margin);
        let (y0, y1) = (margin, self.height_mm - margin);
        let mut free: Vec<&str> = circuit
            .parts()
            .iter()
            .map(|p| p.refdes.0.as_str())
            .filter(|r| !self.anchors.contains_key(*r))
            .filter(|r| !power_headers.contains(r))
            .collect();
        free.sort();

        let n = free.len().max(1) as f64;
        // Running top of the off-board overflow lane (board-local, below the edge).
        let mut overflow_top = self.height_mm + OVERFLOW_GAP_MM;
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
            // No room found → drop it into the off-board overflow lane (clear of
            // the edge; surfaces downstream as its nets left unrouted).
            let cand = spot.unwrap_or_else(|| overflow_drop(x0, (w, h), &mut overflow_top));
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

/// How hard a decoupling cap is bonded to the IC it decouples — well above any
/// net pull, so it lands hard against the chip (the shortest power loop, w95).
const DECOUPLE_PULL: f64 = 12.0;

/// Strong placement-attractor edges pairing each **decoupling cap** — a capacitor
/// tied between a power rail and GND — to an IC on that rail, so it seeds hard
/// against the chip's power pin (the shortest decoupling loop, and the single
/// biggest routing win). Returns `(cap, ic, weight)` edges to fold into the
/// seeded placer's adjacency; when a rail has several ICs, caps are spread across
/// them fewest-first. Roles come from topology, so this needs no SKiDL tags.
fn decoupling_bonus(circuit: &dyn CircuitSource) -> Vec<(String, String, f64)> {
    decoupling_pairs(circuit)
        .into_iter()
        .map(|(cap, ic)| (cap, ic, DECOUPLE_PULL))
        .collect()
}

/// Every (bypass capacitor, the IC it decouples) pair a circuit implies.
///
/// A decoupling cap is one bridging a power rail and ground; its IC is the one
/// on that rail with the fewest caps claimed so far, so two caps on one rail
/// spread across two ICs rather than piling onto the first.
///
/// Shared by the placer's attractor and [`rules::derive`](crate::rules::derive):
/// the pull is a hint, the rule is the guarantee, and they must not be able to
/// disagree about which cap belongs to which IC.
pub fn decoupling_pairs(circuit: &dyn CircuitSource) -> Vec<(String, String)> {
    let is_gnd = |n: &str| {
        let u = n.trim().to_ascii_uppercase();
        matches!(u.as_str(), "GND" | "GNDA" | "AGND" | "DGND" | "VSS" | "0") || u.ends_with("GND")
    };
    let is_power = |n: &str| {
        let u = n.trim().to_ascii_uppercase();
        !is_gnd(n)
            && (u.starts_with('+')
                || u.starts_with('-')
                || matches!(u.as_str(), "VCC" | "VDD" | "VEE" | "V+" | "V-"))
    };
    let fp: HashMap<&str, &str> = circuit
        .parts()
        .iter()
        .map(|p| (p.refdes.0.as_str(), p.footprint.as_deref().unwrap_or("")))
        .collect();
    let is_cap = |r: &str| fp.get(r).is_some_and(|f| f.contains("Capacitor"));
    let is_ic = |r: &str| fp.get(r).is_some_and(|f| f.contains("Package_"));

    let mut net_refs: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut part_nets: HashMap<&str, Vec<&str>> = HashMap::new();
    for net in circuit.nets() {
        for pin in &net.pins {
            let (rd, nm) = (pin.refdes.0.as_str(), net.name.as_str());
            net_refs.entry(nm).or_default().push(rd);
            part_nets.entry(rd).or_default().push(nm);
        }
    }

    let mut bonuses: Vec<(String, String)> = Vec::new();
    let mut cap_count: HashMap<String, usize> = HashMap::new();
    for part in circuit.parts() {
        let r = part.refdes.0.as_str();
        if !is_cap(r) {
            continue;
        }
        let nets = part_nets.get(r).cloned().unwrap_or_default();
        // A decoupling cap bridges a power rail and GND.
        if !nets.iter().any(|n| is_gnd(n)) {
            continue;
        }
        let Some(pnet) = nets.iter().copied().find(|n| is_power(n)) else {
            continue;
        };
        let mut ics: Vec<&str> = net_refs
            .get(pnet)
            .map(|v| v.iter().copied().filter(|x| is_ic(x)).collect())
            .unwrap_or_default();
        ics.sort_unstable();
        ics.dedup();
        ics.sort_by_key(|ic| *cap_count.get(*ic).unwrap_or(&0));
        let Some(&ic) = ics.first() else {
            continue;
        };
        *cap_count.entry(ic.to_string()).or_default() += 1;
        bonuses.push((r.to_string(), ic.to_string()));
    }
    bonuses
}

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
    fn anchored(&self) -> std::collections::HashSet<String> {
        self.anchors.keys().cloned().collect()
    }

    fn place(
        &self,
        circuit: &dyn CircuitSource,
        facts: &HashMap<String, PartFacts>,
    ) -> HashMap<String, Placement> {
        // A part's facts (keep-out size + origin offset), defaulting to a small
        // centred box for a part with no footprint measured.
        let facts_of = |refdes: &str| {
            facts.get(refdes).cloned().unwrap_or(PartFacts {
                extent: (3.0, 3.0),
                body_extent: (3.0, 3.0),
                origin_offset: (0.0, 0.0),
                side: Side::Front,
                height_mm: 2.0,
                standoff_mm: None,
                tht_pads: Vec::new(),
                pin_offsets: HashMap::new(),
            })
        };
        let side_of = |refdes: &str| facts_of(refdes).side == Side::Back;
        // The keep-out centre offset from the footprint origin, mirrored in Y for a
        // back-side part. Placement works in keep-out-centre space and converts
        // back to a footprint origin on output.
        let offset_of = |f: &PartFacts, back: bool| {
            let (ox, oy) = f.origin_offset;
            (ox, if back { -oy } else { oy })
        };

        let margin = EDGE_MARGIN_MM;
        // `extent` is already the real courtyard (KiCad's keep-out); the clearance
        // is only a routing/soldermask allowance on top.
        let clearance = PLACE_CLEARANCE_MM;
        let step = 0.5;
        let bounds = (
            margin,
            self.width_mm - margin,
            margin,
            self.height_mm - margin,
        );
        let (x0, x1, y0, y1) = bounds;

        let mut out = HashMap::new();
        let mut boxes: Vec<Placed> = Vec::new();
        // Board-local centres of everything placed so far (anchors + free), for
        // centroid seeding. Kept separate from `out` (which is origin-shifted).
        let mut pos: HashMap<String, (f64, f64)> = HashMap::new();
        let mut placed: HashSet<String> = HashSet::new();

        // Anchored parts: fixed at their cutouts, exactly like EurorackPlacer. The
        // anchor is the footprint origin; its keep-out centre is offset from that.
        for (refdes, &(x, y)) in &self.anchors {
            let back = side_of(refdes);
            let f = facts_of(refdes);
            // Stand landscape controls (pots) on end; pins face into the board.
            let rotation_deg = control_rotation(&f);
            // Align the mount point (courtyard centre ≈ shaft/barrel) to the
            // cutout: put the footprint origin at `cutout - offset`.
            let (ox, oy) = offset_of(&f, back);
            let (rox, roy) = rotate_local((ox, oy), rotation_deg);
            let (px, py) = (x - rox, y - roy);
            out.insert(
                refdes.clone(),
                Placement {
                    x_mm: self.origin_mm.0 + px,
                    y_mm: self.origin_mm.1 + py,
                    rotation_deg,
                    back,
                },
            );
            boxes.push(Placed {
                body: f.keepout_at_rot(px, py, back, rotation_deg),
                back,
                height_mm: f.height_mm,
                standoff_mm: f.standoff_mm,
                tht_pads: f.tht_pads_at(px, py, back, rotation_deg),
            });
            // Centroid seed = the mount point (cutout), where the body sits.
            pos.insert(refdes.clone(), (x, y));
            placed.insert(refdes.clone());
        }

        // Power header(s): laid horizontal against the top edge, out of the
        // control field (the cable exits the board, not the panel).
        let power_headers = power_header_refdes(circuit, &self.anchors);
        let mut header_x = margin;
        for refdes in &power_headers {
            let back = POWER_HEADER_ON_BACK || side_of(refdes);
            let f = facts_of(refdes);
            let (ew, eh) = f.extent;
            // Horizontal = long axis along X; rotate a portrait header 90°.
            let (rotation_deg, (w, h)) = if eh > ew {
                (90.0, (eh, ew))
            } else {
                (0.0, (ew, eh))
            };
            let cx = (header_x + w / 2.0).min(self.width_mm - margin - w / 2.0);
            let cy = margin + h / 2.0;
            out.insert(
                refdes.clone(),
                Placement {
                    x_mm: self.origin_mm.0 + cx,
                    y_mm: self.origin_mm.1 + cy,
                    rotation_deg,
                    back,
                },
            );
            boxes.push(Placed {
                body: f.keepout_at_rot(cx, cy, back, rotation_deg),
                back,
                height_mm: f.height_mm,
                standoff_mm: f.standoff_mm,
                tht_pads: f.tht_pads_at(cx, cy, back, rotation_deg),
            });
            pos.insert(refdes.clone(), (cx, cy));
            placed.insert(refdes.clone());
            header_x += w + 2.0;
        }

        // Free parts, in a deterministic base order (also the even-spread fallback
        // order for parts with no placed neighbour yet).
        let mut free: Vec<String> = circuit
            .parts()
            .iter()
            .map(|p| p.refdes.0.clone())
            .filter(|r| !self.anchors.contains_key(r))
            .filter(|r| !power_headers.contains(r))
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

        // Design rule (w95): bond each decoupling cap hard to its IC so it seeds
        // against the chip's power pin — the shortest loop, and short traces the
        // router can actually finish. Codified in placement, not fixed up later.
        for (cap, ic, w) in decoupling_bonus(circuit) {
            adj.entry(cap.clone()).or_default().push((ic.clone(), w));
            adj.entry(ic).or_default().push((cap, w));
        }

        // Greedy: repeatedly place the unplaced free part most tied to what's down.
        let mut remaining = free.clone();
        // Running top of the off-board overflow lane (board-local, below the edge).
        let mut overflow_top = self.height_mm + OVERFLOW_GAP_MM;
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
            // Only turn THROUGH-HOLE parts portrait. Rotating a fine-pitch SMD IC
            // (a SOIC) hurts pin fanout AND — the j54.25 bug — its rotated pads
            // trip shorting/soldermask DRC on the back layer; leave SMD unrotated.
            let rot =
                if !f.tht_pads.is_empty() && f.extent.0 > f.extent.1 && f.extent.0 > usable_w * 0.5
                {
                    90.0
                } else {
                    0.0
                };
            // Flip in the footprint's own frame first, then turn it — the same
            // order as `keepout_at_rot`, so this target agrees with the keep-out
            // the placer will go on to reserve for the part.
            let flipped = if back {
                (f.origin_offset.0, -f.origin_offset.1)
            } else {
                f.origin_offset
            };
            let (ext, (ox, oy)) = if rot == 90.0 {
                ((f.extent.1, f.extent.0), rotate_local(flipped, 90.0))
            } else {
                (f.extent, flipped)
            };

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

            // Through-hole pins at a candidate keep-out centre (cx,cy): the origin
            // sits at (cx-ox, cy-oy), and the pins rotate with the part.
            let cand_tht =
                |cx: f64, cy: f64| -> Vec<Rect> { f.tht_pads_at(cx - ox, cy - oy, back, rot) };
            // Nearest spot whose body + pins clash with nothing already placed
            // (side- and height-aware — 25z.5); if the board is full, drop it just
            // below the outline where DRC flags it (never overlap).
            let clear = |cx: f64, cy: f64| {
                let body = (
                    cx - ext.0 / 2.0,
                    cy - ext.1 / 2.0,
                    cx + ext.0 / 2.0,
                    cy + ext.1 / 2.0,
                );
                placement_clear(
                    &body,
                    back,
                    f.height_mm,
                    f.standoff_mm,
                    &cand_tht(cx, cy),
                    &boxes,
                    clearance,
                )
            };
            let cand = nearest_clear_spot(target, ext, bounds, step, clear)
                .unwrap_or_else(|| overflow_drop(x0, ext, &mut overflow_top));
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
            boxes.push(Placed {
                body: cand,
                back,
                height_mm: f.height_mm,
                standoff_mm: f.standoff_mm,
                tht_pads: cand_tht(cx, cy),
            });
            pos.insert(r.clone(), (cx, cy));
            placed.insert(r);
        }
        out
    }
}

/// One placed part's keep-out for placement: its body (on its own side), plus its
/// through-hole pins (both sides), height, and — if a sub-board — standoff.
struct Placed {
    body: Rect,
    back: bool,
    height_mm: f64,
    standoff_mm: Option<f64>,
    tht_pads: Vec<Rect>,
}

/// Whether two parts may occupy the same board area vertically: one is a sub-board
/// tall enough (its standoff) to clear the other beneath it (DESIGN 6.7). A pin
/// counts as height 0, so it always clears a standoff.
fn fits_under(a_h: f64, a_standoff: Option<f64>, b_h: f64, b_standoff: Option<f64>) -> bool {
    a_standoff.is_some_and(|s| b_h < s) || b_standoff.is_some_and(|s| a_h < s)
}

/// Whether a candidate part (body + through-hole pins, on side `back`) fits at a
/// spot without clashing with anything already `placed`. The rules (25z.5):
/// bodies clash only on the *same side* and only when neither passes under the
/// other; through-hole pins are both-side hard keep-outs every body avoids; a
/// candidate's own pins may pass over a sub-board's standoff but not into a
/// surface part.
fn placement_clear(
    cand_body: &Rect,
    cand_back: bool,
    cand_h: f64,
    cand_standoff: Option<f64>,
    cand_tht: &[Rect],
    placed: &[Placed],
    clr: f64,
) -> bool {
    for p in placed {
        if p.back == cand_back
            && !fits_under(cand_h, cand_standoff, p.height_mm, p.standoff_mm)
            && rects_overlap(cand_body, &p.body, clr)
        {
            return false;
        }
        if p.tht_pads.iter().any(|q| rects_overlap(cand_body, q, clr)) {
            return false;
        }
        for c in cand_tht {
            // A pin (height 0) may pass over a sub-board's standoff, but not into a
            // surface part's body, nor onto another pin.
            if !fits_under(0.0, None, p.height_mm, p.standoff_mm) && rects_overlap(c, &p.body, clr)
            {
                return false;
            }
            if p.tht_pads.iter().any(|q| rects_overlap(c, q, clr)) {
                return false;
            }
        }
    }
    true
}

/// The collision-free `w×h` spot whose centre is nearest `(tx, ty)`, searched in
/// expanding rings so an occupied target spills to the closest free space rather
/// than jumping across the board. `clear(cx, cy)` decides whether the part fits
/// with its centre there. Returns the rect `(min_x, min_y, max_x, max_y)`, or
/// `None` if it fits nowhere. Deterministic: rings are sampled in a fixed order.
fn nearest_clear_spot(
    (tx, ty): (f64, f64),
    (w, h): (f64, f64),
    (x0, x1, y0, y1): (f64, f64, f64, f64),
    step: f64,
    clear: impl Fn(f64, f64) -> bool,
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

    let max_r = (x1 - x0).hypot(y1 - y0);
    let mut d = 0.0;
    while d <= max_r {
        if d == 0.0 {
            let (cx, cy) = clamp_center(tx, ty);
            if clear(cx, cy) {
                return Some(rect_at(cx, cy));
            }
        } else {
            let mut t = -d;
            while t <= d + 1e-9 {
                for (dx, dy) in [(t, -d), (t, d), (-d, t), (d, t)] {
                    let (cx, cy) = clamp_center(tx + dx, ty + dy);
                    if clear(cx, cy) {
                        return Some(rect_at(cx, cy));
                    }
                }
                t += step;
            }
        }
        d += step;
    }
    None
}

/// Gap (mm) around an overflow part: below the board edge and between stacked
/// overflow parts. > the usual edge clearance so a dropped part never trips
/// `copper_edge_clearance`.
const OVERFLOW_GAP_MM: f64 = 2.0;

/// Board-local drop rect (keep-out box) for a free part that fits nowhere on the
/// board. It goes into an off-board lane a clear gap *below* the bottom edge —
/// never straddling the edge (a drop within edge clearance manufactures a
/// misleading `copper_edge_clearance` DRC error instead of the honest "this part
/// could not be placed", which surfaces downstream as its nets left unrouted).
/// `lane_top` is the running top of the lane, advanced past this part so
/// successive overflow parts stack instead of overlapping.
fn overflow_drop(x0: f64, (w, h): (f64, f64), lane_top: &mut f64) -> Rect {
    let top = *lane_top;
    *lane_top = top + h + OVERFLOW_GAP_MM;
    (x0, top, x0 + w, top + h)
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
    /// Which components get their value ("47nF", "TL072") on silk next to the
    /// refdes (DESIGN 6.10). Defaults to [`SilkValues::HandSoldered`].
    pub silk_values: SilkValues,
    /// A silkscreen title (the board's name) placed at the bottom edge. `None`
    /// omits it.
    pub title: Option<String>,
    /// Maker, revision and a free-form note, stacked under the title.
    pub legend: SilkLegend,
    /// A brand logo, rendered on the **back** silk (B.SilkS) bottom-centre so it
    /// doesn't fight the front component legend (DESIGN §7.9). `None` omits it.
    pub logo: Option<Logo>,
}

/// Which parts get their value printed on silk beside the refdes.
///
/// Values are for the person holding the soldering iron. On a kit where JLCPCB
/// assembles the SMD and the buyer fits the through-hole panel hardware, "100nF"
/// on a 0603 is read by nobody — and it is not free: a value string is wider
/// than the 0603 it labels, so on a dense board it collides with the neighbours.
/// Measured on the slew limiter, printing values for every part cost 4
/// silkscreen overlaps and 4 silk-over-pad violations; printing them only for
/// hand-soldered parts costs 1 and 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SilkValues {
    /// Every part's value — a fully hand-assembled board, where the builder
    /// places the passives too.
    All,
    /// Only parts a person solders: through-hole. The default, because that is
    /// the kit this project ships (`kit = "mixed"`).
    #[default]
    HandSoldered,
    /// No values anywhere; refdes only.
    None,
}

/// Maker, revision and a design note, printed under the board title.
///
/// A board is a product, and an unmarked one is hard to identify on a bench, in
/// a photo, or in a support thread six months later. Each line is optional and
/// omitted entirely when `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SilkLegend {
    /// Maker or brand, e.g. "Puget Audio".
    pub brand: Option<String>,
    /// Revision, e.g. "v1.2" — what a support request needs to quote.
    pub rev: Option<String>,
    /// A free-form design note: topology, licence, a URL.
    pub note: Option<String>,
}

impl SilkLegend {
    /// The lines to print, top to bottom. Brand and revision share a line —
    /// they are read together and the bottom edge is scarce.
    fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let head = [self.brand.as_deref(), self.rev.as_deref()]
            .into_iter()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        if !head.is_empty() {
            out.push(head);
        }
        if let Some(note) = self.note.as_deref().filter(|s| !s.trim().is_empty()) {
            out.push(note.to_string());
        }
        out
    }
}

impl BoardOptions {
    /// Default options: grid placement, **grid routing**, a `GND` ground pour,
    /// 5 mm outline margin, values on silk for hand-soldered parts. This is what
    /// the CLI builds with — a harness that overrides any of it is measuring a
    /// board nothing ships (`legion-of-bom-nz1`).
    pub fn new(footprint_dir: impl Into<PathBuf>) -> Self {
        BoardOptions {
            footprint_dir: footprint_dir.into(),
            placer: Box::new(GridPlacer::default()),
            router: Some(Box::new(GridRouter)),
            route_options: RouteOptions::default(),
            ground_net: Some("GND".into()),
            outline_margin_mm: 5.0,
            fixed_outline: None,
            silk_values: SilkValues::default(),
            title: None,
            legend: SilkLegend::default(),
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
    /// Mechanical clearance problems (DESIGN 6.7): a part standing under a
    /// stacked sub-board that is taller than the sub-board's standoff. Surfaced,
    /// not auto-fixed. Empty when nothing collides.
    pub collisions: Vec<String>,
}

/// Load every part's footprint and measure its placement facts (keep-out extent,
/// origin offset, through-hole pads, side) — the same measurement
/// [`generate_board_artifacts`] does in its first pass, exposed so sizing tools
/// (e.g. [`minimum_hp`]) can reason about a board without generating it.
pub fn build_facts(
    circuit: &dyn CircuitSource,
    footprint_dir: &Path,
) -> Result<HashMap<String, PartFacts>, BoardError> {
    let mut facts = HashMap::new();
    for part in circuit.parts() {
        let refdes = part.refdes.0.as_str();
        let lib_part = part
            .footprint
            .as_deref()
            .ok_or_else(|| BoardError::NoFootprint {
                refdes: refdes.to_string(),
            })?;
        let fp = load_footprint(footprint_dir, lib_part)?;
        let pads = footprint_pads(&fp);
        let courtyard = courtyard_extent(&fp);
        let keepout = match (part_extent(&pads, COURTYARD_MARGIN_MM), courtyard) {
            (Some(p), Some(c)) => (p.0.min(c.0), p.1.min(c.1), p.2.max(c.2), p.3.max(c.3)),
            (Some(b), None) | (None, Some(b)) => b,
            (None, None) => (0.0, 0.0, 0.0, 0.0),
        };
        let tht_pads: Vec<Rect> = pads
            .iter()
            .filter(|p| matches!(p.layer, PadLayer::Both))
            .map(|p| {
                let m = THT_PAD_CLEAR_MM + p.w.max(p.h) / 2.0;
                (p.px - m, p.py - m, p.px + m, p.py + m)
            })
            .collect();
        let pin_offsets: HashMap<String, (f64, f64)> =
            pads.iter().map(|p| (p.num.clone(), (p.px, p.py))).collect();
        facts.insert(
            refdes.to_string(),
            PartFacts {
                extent: (keepout.2 - keepout.0, keepout.3 - keepout.1),
                body_extent: body_span(courtyard, part_extent(&pads, 0.0)),
                origin_offset: ((keepout.0 + keepout.2) / 2.0, (keepout.1 + keepout.3) / 2.0),
                side: part.side.unwrap_or(Side::Front),
                height_mm: part_height_mm(lib_part),
                standoff_mm: subboard_standoff(lib_part),
                tht_pads,
                pin_offsets,
            },
        );
    }
    Ok(facts)
}

/// The **minimum Eurorack HP** that fits a circuit — the "PCB drives the panel"
/// primitive (DESIGN 6.1). Auto-arranges the panel controls (via
/// [`crate::panel::derive_panel`]), then, for each candidate width smallest-first,
/// runs [`EurorackPlacer`] and takes the first HP where no part is pushed into the
/// off-board overflow lane. Height is fixed (3U), so this optimizes width only.
///
/// The search starts at [`crate::panel::min_panel_hp`], never below: a width the
/// *board* squeezes into is useless if the panel hardware it must carry doesn't
/// physically fit there (a 3 HP panel is 15.24 mm; an Alpha pot body is 13.75 mm).
///
/// # This is a floor, not a buildable width
///
/// It answers "does the copper fit between the edges", which is a true lower
/// bound and cheap — no routing, no KiCad. It does **not** ask whether the router
/// can complete every net in the space left over, and a board can fit and still
/// be unroutable. Quoting this as *the* minimum width is what produced a 3 HP
/// slew limiter with parts hanging off the edge (`legion-of-bom-t5t`).
///
/// For a width that is actually proven to build, feed this in as the floor to
/// [`crate::layout::minimum_routable_hp`], which trials each width for real.
pub fn minimum_hp(circuit: &dyn CircuitSource, facts: &HashMap<String, PartFacts>) -> u16 {
    use crate::panel::PanelSpec;
    const MAX_HP: u16 = 42;
    let floor_hp = crate::panel::min_panel_hp(circuit, &crate::panel::BuiltinCutouts).max(2);
    for hp in floor_hp..=MAX_HP {
        let dims = crate::panel::EurorackPanel::new(hp);
        let (w, h) = (dims.width_mm(), dims.height_mm());
        // Auto-arranged controls become the anchors (cutout y is bottom-up).
        let panel = crate::panel::derive_panel(circuit, hp, &crate::panel::BuiltinCutouts);
        let anchors: HashMap<String, (f64, f64)> = panel
            .cutouts
            .iter()
            .filter_map(|c| c.refdes.clone().map(|r| (r, (c.x_mm, h - c.y_mm))))
            .collect();
        // Measure with the SAME placer the build uses (SeededPlacer): it packs
        // back-side SMD *under* front-side THT controls, so the min HP reflects
        // the real, tight layout — not the looser side-unaware EurorackPlacer.
        let placer = SeededPlacer {
            width_mm: w,
            height_mm: h,
            origin_mm: (0.0, 0.0),
            anchors,
            nudges: HashMap::new(),
        };
        let mut placements = placer.place(circuit, facts);
        let pinned = placer.anchored();
        // A part in the overflow lane sits below the board bottom (y > height).
        let overflowed = placements.values().any(|p| p.y_mm > h + 0.01);
        // …but "nothing overflowed" is not "buildable". The lane only catches
        // parts the packer gave up on; it says nothing about a part hanging off
        // the side, or one simply wider than the panel. That is what reported
        // 3 HP for a board whose pots do not fit in 3 HP, and produced copper
        // edge-clearance errors at 4 HP. Ask the physical rules instead.
        let rules = crate::rules::derive_in(
            circuit,
            &crate::rules::Context {
                facts: Some(facts),
                outline: Some((0.0, 0.0, w, h)),
            },
        );
        // Legalize before judging, because the build does. Asking whether the
        // *global* placement is legal reports a wider board than we would
        // actually manufacture.
        //
        // This was reverted once, when the rules did not yet bound the copper
        // and it made minimum_hp answer a width that failed DRC. Two coordinate
        // bugs later — keep-outs rotated against KiCad's sense, and back-side
        // parts mirrored after rotation instead of before — the rule's box now
        // contains the real copper with the expected clearance, and
        // copper_edge_clearance errors at 4 HP went from 5 to 0. Restored.
        crate::legalize::legalize_pinning(&mut placements, &rules, facts, &pinned);
        let broken = crate::rules::by_tier(&crate::rules::evaluate(&rules, &placements));
        if !overflowed && broken[0] <= 0.0 {
            return hp;
        }
    }
    MAX_HP
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
    // For sub-boards: refdes → (pad number → its function names), so a net can be
    // wired to a pad by function (`AUDIO_OUT_L`) as well as by number (25z.3).
    let mut pin_labels: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    for part in circuit.parts() {
        let refdes = part.refdes.0.as_str();
        let lib_part = part
            .footprint
            .as_deref()
            .ok_or_else(|| BoardError::NoFootprint {
                refdes: refdes.to_string(),
            })?;
        if let Some((crate::subboard::SUBBOARD_LIB, name)) = lib_part.split_once(':') {
            if let Some(spec) = crate::subboard::from_name(name) {
                let map: HashMap<String, Vec<String>> = spec
                    .pins
                    .iter()
                    .map(|p| {
                        let mut names = vec![p.name.to_string()];
                        names.extend(p.aliases.iter().map(|a| a.to_string()));
                        (p.pad.to_string(), names)
                    })
                    .collect();
                pin_labels.insert(refdes.to_string(), map);
            }
        }
        let fp = load_footprint(&options.footprint_dir, lib_part)?;
        let pads = footprint_pads(&fp);
        // Keep-out = the union of the pad bbox (+margin) and the real courtyard,
        // both relative to the footprint origin. The union (not a max of sizes)
        // preserves *where* the keep-out sits — a DIP's courtyard is offset from
        // its pin-1 origin, and that offset must survive into placement.
        let courtyard = courtyard_extent(&fp);
        let keepout = match (part_extent(&pads, COURTYARD_MARGIN_MM), courtyard) {
            (Some(p), Some(c)) => (p.0.min(c.0), p.1.min(c.1), p.2.max(c.2), p.3.max(c.3)),
            (Some(b), None) | (None, Some(b)) => b,
            (None, None) => (0.0, 0.0, 0.0, 0.0),
        };
        // Through-hole pads (both copper layers) become hard both-side keep-outs,
        // each clearance-expanded, relative to the footprint origin.
        let tht_pads: Vec<Rect> = pads
            .iter()
            .filter(|p| matches!(p.layer, PadLayer::Both))
            .map(|p| {
                let m = THT_PAD_CLEAR_MM + p.w.max(p.h) / 2.0;
                (p.px - m, p.py - m, p.px + m, p.py + m)
            })
            .collect();
        let pin_offsets: HashMap<String, (f64, f64)> =
            pads.iter().map(|p| (p.num.clone(), (p.px, p.py))).collect();
        facts.insert(
            refdes.to_string(),
            PartFacts {
                extent: (keepout.2 - keepout.0, keepout.3 - keepout.1),
                body_extent: body_span(courtyard, part_extent(&pads, 0.0)),
                origin_offset: ((keepout.0 + keepout.2) / 2.0, (keepout.1 + keepout.3) / 2.0),
                side: part.side.unwrap_or(Side::Front),
                height_mm: part_height_mm(lib_part),
                standoff_mm: subboard_standoff(lib_part),
                tht_pads,
                pin_offsets,
            },
        );
        loaded.push((refdes, lib_part, part.value.as_str(), fp, pads));
    }

    let mut placements = options.placer.place(circuit, &facts);
    // Panel controls are pinned to their cutouts; nothing downstream may slide
    // them off, or the board stops mating its own panel.
    let pinned = options.placer.anchored();

    // Bypass caps go against the power pin they bypass, before anything else
    // gets a say. The placer's decoupling pull is one attractor among many and
    // lands them "near the IC", which on a 16-pin package can still be 10mm of
    // copper from the pin that matters — see `crate::decouple`. There is no
    // competing claim on that exact spot, so this is set, not scored.
    crate::decouple::snap(&mut placements, circuit, &facts);

    // Legalization — the middle stage. Global placement decides roughly where
    // things want to be; this moves whatever is physically illegal the minimum
    // distance to make it legal, and leaves everything else alone. Only Physical
    // rules: a part over the board edge is not a trade-off, whereas moving one
    // to improve decoupling is, and trade-offs belong in the score.
    //
    // Requires a known outline, so it is a no-op on a board whose outline is the
    // pad bounding box — there, nothing can be outside by construction.
    if options.fixed_outline.is_some() {
        let rules = crate::rules::derive_in(
            circuit,
            &crate::rules::Context {
                facts: Some(&facts),
                outline: options.fixed_outline,
            },
        );
        crate::legalize::legalize_pinning(&mut placements, &rules, &facts, &pinned);
    }

    // Height/collision check (DESIGN 6.7): a sub-board stands off the main board on
    // its headers; a taller part directly under it on the same side would hit it.
    // Surfaced (never silently moved) — the author raises the standoff, moves the
    // part, or flips it to the other side.
    let placed_parts: Vec<PlacedPart> = loaded
        .iter()
        .filter_map(|(refdes, lib_part, ..)| {
            let f = facts.get(*refdes)?;
            let p = placements.get(*refdes)?;
            Some(PlacedPart {
                refdes: refdes.to_string(),
                keepout: f.keepout_at(p.x_mm, p.y_mm, p.back),
                back: p.back,
                height_mm: part_height_mm(lib_part),
                standoff_mm: subboard_standoff(lib_part),
            })
        })
        .collect();
    let collisions = detect_collisions(&placed_parts);

    // Pass 2: transform each footprint into a placed, net-wired board footprint,
    // collect every connected pad's absolute position (for routing), and track the
    // real pad bounding box (for the outline — a big part's pads must not spill
    // past the board edge).
    let mut footprints = Vec::new();
    // Power headers, as placed — board-level silk is drawn from these once the
    // outline is known (the −12 V mark is clamped inside it).
    let mut power_headers_placed: Vec<(String, Vec<FpPad>, Option<Rect>, Placement)> = Vec::new();
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
            // Resolve the pad's net by pad number, then (for a sub-board) by any of
            // the pad's function names — so `AUDIO_OUT_L` wires to pad 18.
            let net_name: Option<&str> = pin_net
                .get(&(refdes.to_string(), pad.num.clone()))
                .copied()
                .or_else(|| {
                    pin_labels
                        .get(refdes)
                        .and_then(|m| m.get(&pad.num))
                        .and_then(|names| {
                            names.iter().find_map(|n| {
                                pin_net.get(&(refdes.to_string(), n.clone())).copied()
                            })
                        })
                });
            match net_name.and_then(|name| net_index.get(name).map(|&idx| (idx, name))) {
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
        // A power header gets its −12 V end marked on the silk of the face it
        // mounts on, so the ribbon's red stripe has something to line up against.
        // Deferred: the mark is clamped inside the outline, which isn't known
        // until every pad has been seen.
        if is_power_header_footprint(lib_part) {
            power_headers_placed.push((
                refdes.to_string(),
                pads.clone(),
                courtyard_extent(&fp),
                placement,
            ));
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
        // Maker / revision / note, stacked upward from the title. Smaller than
        // the title but never below the fab's silk minimum (1.0mm high, and
        // `silk_text_on` strokes at size/6, so 1.0mm gives 0.167mm — clear of
        // JLCPCB's 0.15mm). Front silk: the back is the logo's.
        for (i, line) in options.legend.lines().iter().enumerate() {
            board.push(silk_text_on(
                line,
                (minx + maxx) / 2.0,
                maxy - 4.6 - 1.7 * i as f64,
                0.0,
                &format!("board.legend.{i}"),
                "F.SilkS",
                1.0,
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
    for (refdes, pads, courtyard, placement) in &power_headers_placed {
        board.extend(power_polarity_silk(
            refdes, pads, *courtyard, *placement, &pin_net, outline,
        ));
    }
    board.extend(footprints);

    // Route the nets into copper tracks (DESIGN 6.5). Ground still gets the pour;
    // routing traces the rest (and any multi-pad ground net) on the copper layers.
    let mut route = RouteOutput::default();
    if let Some(router) = &options.router {
        let mut nets: Vec<RouteNet> = net_pads.into_values().collect();
        // By net index, because `into_values` hands them over in hash order and
        // the router paints every net's clearance halo in the order it is given:
        // where two halos overlap, the last one written owns the cell. Rust
        // reseeds hash iteration per process, so this was a board that changed
        // between identical runs — measured on a 13-part demo, 179 tracks and 0
        // conflicts or 191 and 2, depending on the run. Routing *order* was
        // already deterministic (`GridRouter::route` sorts by net index); the
        // obstacle painting that happens before it was not (`legion-of-bom-gns`).
        nets.sort_by_key(|n| n.net_idx);
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
        // Hand the router the board outline as its `bounds` so it keeps copper an
        // edge-clearance inset inside the real edge (a narrow-HP board otherwise
        // routes traces onto Edge.Cuts → copper_edge_clearance DRC errors).
        let mut route_opts = options.route_options.clone();
        if route_opts.bounds.is_none() {
            route_opts.bounds = outline;
        }
        route = router.route(&nets, &route_opts);
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
        // GND stitching vias tie the two-sided pour together next to each
        // through-hole ground pad, so a pour island fenced off by a dense THT grid
        // (a sub-board header) reconnects — fixes starved_thermal (25z.2).
        if let (Some(rect), Some(gnd)) = (outline, &options.ground_net) {
            if let Some(name) = net_names.iter().find(|n| n.eq_ignore_ascii_case(gnd)) {
                let gnd_idx = net_index[name.as_str()];
                let pad_geo: Vec<PadGeo> = nets
                    .iter()
                    .flat_map(|n| {
                        let idx = n.net_idx;
                        n.pads.iter().map(move |p| PadGeo {
                            x: p.x_mm,
                            y: p.y_mm,
                            w: p.w_mm,
                            h: p.h_mm,
                            net_idx: idx,
                            tht: matches!(p.layer, PadLayer::Both),
                        })
                    })
                    .collect();
                for via in ground_stitching_vias(
                    rect,
                    gnd_idx,
                    &pad_geo,
                    &route.tracks,
                    &route.vias,
                    &route_opts,
                ) {
                    board.push(via_sexpr(
                        &via,
                        &options.route_options.front,
                        &options.route_options.back,
                    ));
                }
            }
        }
    }

    Ok(BoardArtifacts {
        pcb: Sexpr::list(board).to_sexpr_string() + "\n",
        placements,
        route,
        collisions,
    })
}

/// A footprint pad's local geometry, for routing.
#[derive(Clone)]
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
        let rot = at
            .nth_atom(3)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let size = item.get("size");
        let sw = size
            .and_then(|s| s.nth_atom(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let sh = size
            .and_then(|s| s.nth_atom(2))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        // A pad rotated an odd multiple of 90° swaps its width/height in the
        // footprint frame. Without this a rotated SMD pad is painted at the wrong
        // extent — fine-pitch rotated pads (a Daisy sub-board's 1.27 mm headers)
        // merge into an unroutable blob.
        let (w, h) = if (rot / 90.0).round() as i64 % 2 != 0 {
            (sh, sw)
        } else {
            (sw, sh)
        };
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
pub fn place_point(placement: Placement, px: f64, py: f64) -> (f64, f64) {
    // A back-placed footprint mirrors local Y (matching `flip_to_back`), then the
    // whole footprint rotates about its origin.
    let py = if placement.back { -py } else { py };
    let (s, c) = placement.rotation_deg.to_radians().sin_cos();
    let rx = px * c + py * s;
    let ry = py * c - px * s;
    (placement.x_mm + rx, placement.y_mm + ry)
}

/// Load and parse a footprint `.kicad_mod` by `lib:name`.
/// A placed part reduced to what the mechanical-clearance check needs.
struct PlacedPart {
    refdes: String,
    keepout: Rect,
    back: bool,
    height_mm: f64,
    /// `Some(standoff)` if this part is itself a stacked sub-board.
    standoff_mm: Option<f64>,
}

/// Mechanical clearance findings (DESIGN 6.7): for each stacked sub-board, any
/// same-side part whose keep-out overlaps the sub-board body and is taller than
/// the sub-board's standoff would physically collide. Sub-boards are not checked
/// against each other. Deterministic (sorted); empty when nothing collides.
fn detect_collisions(parts: &[PlacedPart]) -> Vec<String> {
    let mut out = Vec::new();
    for s in parts.iter().filter(|p| p.standoff_mm.is_some()) {
        let standoff = s.standoff_mm.unwrap();
        for p in parts {
            if p.refdes == s.refdes || p.standoff_mm.is_some() || p.back != s.back {
                continue;
            }
            if p.height_mm > standoff && rects_overlap(&p.keepout, &s.keepout, 0.0) {
                out.push(format!(
                    "{} (~{:.1}mm) sits under {} but is taller than its {:.1}mm standoff",
                    p.refdes, p.height_mm, s.refdes, standoff
                ));
            }
        }
    }
    out.sort();
    out
}

/// The standoff (mm) a sub-board mounts at, if `lib_part` is a synthesized
/// sub-board; `None` for an ordinary footprint.
fn subboard_standoff(lib_part: &str) -> Option<f64> {
    let (lib, name) = lib_part.split_once(':')?;
    (lib == crate::subboard::SUBBOARD_LIB)
        .then(|| crate::subboard::from_name(name).map(|s| s.standoff_mm))
        .flatten()
}

/// Rough component height (mm) by footprint family — enough to tell a low-profile
/// SMD part (fine under a stacked sub-board) from a tall through-hole /
/// electrolytic / connector one that would hit it. A heuristic pending real
/// 3D-model heights (DESIGN 6.7); deliberately errs toward flagging.
fn part_height_mm(lib_part: &str) -> f64 {
    let f = lib_part.to_ascii_lowercase();
    let has = |s: &str| f.contains(s);
    if has("pinheader")
        || has("pinsocket")
        || has("idc")
        || has("connector")
        || has("jack")
        || has("terminal")
        || has("lobmodule")
    {
        8.5
    } else if has("potentiometer") || has("trimmer") || has("rotaryencoder") {
        10.0
    } else if has("cp_") || has("electrolytic") || has("radial") {
        6.0
    } else if has("to-92") || has("to92") || has("to-220") || has("to220") {
        9.0
    } else if has("crystal") || has("oscillator") || has("hc49") {
        3.5
    } else if has("soic") || has("sop") || has("tssop") || has("qfp") || has("qfn") || has("dfn") {
        1.8
    } else if has("0402") || has("0201") {
        0.5
    } else if has("0603") || has("0805") || has("1206") || has("1210") {
        1.0
    } else {
        2.0 // unknown → assume a modest low profile
    }
}

fn load_footprint(dir: &Path, lib_part: &str) -> Result<Sexpr, BoardError> {
    let (lib, name) = lib_part.split_once(':').ok_or_else(|| {
        BoardError::Other(format!("bad footprint id '{lib_part}' (want lib:name)"))
    })?;
    // Sub-board / header footprints (Daisy Seed, board-to-board headers) are
    // synthesized in-memory, not read from a `.pretty` dir — a part with footprint
    // `LobModule:Daisy_Seed` places + routes through the normal pipeline (25z).
    if lib == crate::subboard::SUBBOARD_LIB {
        let text =
            crate::subboard::footprint_text(name).ok_or_else(|| BoardError::FootprintNotFound {
                lib_part: lib_part.to_string(),
                path: format!("{}:<synthesized/vendored>", crate::subboard::SUBBOARD_LIB),
            })?;
        return Sexpr::parse(&text).map_err(|msg| BoardError::FootprintParse {
            lib_part: lib_part.to_string(),
            msg,
        });
    }
    // House library first, so a project can carry footprints KiCad does not ship
    // (a sub-mini toggle, a PCB-mount RCA, a slide pot) and can override a stock
    // one. It lives beside the part metadata and photos, because a footprint is
    // part data like a pinout or a product shot — see `crate::parts`.
    let house = crate::parts::house_footprint_dir();
    let candidates: Vec<std::path::PathBuf> = house
        .iter()
        .chain(std::iter::once(&dir.to_path_buf()))
        .map(|root| {
            root.join(format!("{lib}.pretty"))
                .join(format!("{name}.kicad_mod"))
        })
        .collect();
    let Some((path, text)) = candidates
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok().map(|t| (p, t)))
    else {
        return Err(BoardError::FootprintNotFound {
            lib_part: lib_part.to_string(),
            path: candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" | "),
        });
    };
    let _ = path;
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
    silk_values: SilkValues,
    placement: Placement,
    pin_net: &HashMap<(String, String), &str>,
    net_index: &HashMap<&str, usize>,
) -> Sexpr {
    let items = fp.as_list_mut().expect("a footprint is a list");
    if items.len() >= 2 {
        items[1] = Sexpr::string(lib_part); // "R_0805" → "Resistor_SMD:R_0805…"
    }

    // A back-placed footprint is flipped to the bottom: swap every child item's
    // F./B. layer and mirror its local Y (KiCad's flip-to-back). Do it on the
    // library-local geometry, before the board-level placement is inserted — and
    // note `place_point` mirrors pad Y the same way so routing matches the pads.
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

    // A through-hole pad is the marker of a part somebody fits by hand; SMD
    // arrives on the board from the assembler. `np_thru_hole` counts too — a
    // mounting post is still something a human puts through the panel.
    let hand_soldered = items.iter().any(|c| {
        c.head() == Some("pad") && matches!(c.nth_atom(2), Some("thru_hole") | Some("np_thru_hole"))
    });

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
                    // Whether a person will ever solder this part, and so whether
                    // its value is worth the silk it costs: a through-hole pad
                    // means hand assembly (the SMD arrives pre-populated).
                    let wanted = match silk_values {
                        SilkValues::All => true,
                        SilkValues::HandSoldered => hand_soldered,
                        SilkValues::None => false,
                    };
                    if wanted && presentable {
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
        // Coordinate lists: mirror the Y component, and reverse any angle that
        // rides along on an `(at x y rot)` — a reflection reverses handedness, so
        // a pad turned +90° in the library is turned −90° once flipped.
        //
        // Mirroring *Y* (not X) is KiCad's own storage convention for a footprint
        // flipped to the back, verified against `pcbnew`'s `FOOTPRINT::Flip`. The
        // two differ by a 180° turn, so getting it wrong is invisible on a
        // symmetric part and puts the 3D body a footprint-length away from its
        // pads on everything else: KiCad renders the model from the convention it
        // reads the pads with, so ours has to be the same one.
        Some("at") | Some("start") | Some("end") | Some("center") | Some("mid") | Some("xy") => {
            if let Some(y) = list.get_mut(2) {
                if let Some(v) = y.as_atom().and_then(|s| s.parse::<f64>().ok()) {
                    *y = Sexpr::sym(mm(-v));
                }
            }
            if head.as_deref() == Some("at") {
                if let Some(a) = list.get_mut(3) {
                    if let Some(v) = a.as_atom().and_then(|s| s.parse::<f64>().ok()) {
                        *a = Sexpr::sym(mm(-v));
                    }
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
    silk_text_on(text, x, y, rot, seed, "F.SilkS", 1.5)
}

/// A silkscreen `gr_text` on a named layer at a chosen size. Back silk gets
/// `(justify mirror)` so the text reads the right way round when you are looking
/// at the back of the board — which, for a back-mounted part, is the only time
/// anybody reads it.
fn silk_text_on(text: &str, x: f64, y: f64, rot: f64, seed: &str, layer: &str, size: f64) -> Sexpr {
    let font = Sexpr::list(vec![
        Sexpr::sym("font"),
        Sexpr::list(vec![
            Sexpr::sym("size"),
            Sexpr::sym(mm(size)),
            Sexpr::sym(mm(size)),
        ]),
        kv("thickness", Sexpr::sym(mm(size / 6.0))),
    ]);
    // `justify` is a sibling of `font` inside `effects`, not a child of it —
    // nested, KiCad refuses to load the board at all.
    let mut effects = vec![Sexpr::sym("effects"), font];
    if layer.starts_with("B.") {
        effects.push(Sexpr::list(vec![
            Sexpr::sym("justify"),
            Sexpr::sym("mirror"),
        ]));
    }
    Sexpr::list(vec![
        Sexpr::sym("gr_text"),
        Sexpr::string(text),
        Sexpr::list(vec![
            Sexpr::sym("at"),
            Sexpr::sym(mm(x)),
            Sexpr::sym(mm(y)),
            Sexpr::sym(mm(rot)),
        ]),
        kv("layer", Sexpr::string(layer)),
        kv("uuid", Sexpr::string(det_uuid(seed))),
        Sexpr::list(effects),
    ])
}

/// A silkscreen `gr_line` from `(x1, y1)` to `(x2, y2)`.
fn silk_line(x1: f64, y1: f64, x2: f64, y2: f64, width: f64, layer: &str, seed: &str) -> Sexpr {
    Sexpr::list(vec![
        Sexpr::sym("gr_line"),
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
            kv("width", Sexpr::sym(mm(width))),
            kv("type", Sexpr::sym("solid")),
        ]),
        kv("layer", Sexpr::string(layer)),
        kv("uuid", Sexpr::string(det_uuid(seed))),
    ])
}

/// Net names that mean "the negative rail" on a Eurorack power connector.
const NEG_RAIL_NETS: &[&str] = &["-12V", "-12", "VEE", "V-", "-15V", "-15"];

/// Silk marking the **−12 V end** of a power header: a bar across that end plus a
/// `-12V` label, on whichever face the header mounts.
///
/// Reversing a Eurorack power header is the one assembly mistake that destroys
/// the module, and the build guide already tells the builder to "check the −12 V
/// stripe against the silkscreen" — so the board has to actually draw one. The
/// end is read from the netlist (which pads sit on [`NEG_RAIL_NETS`]), not from a
/// hardcoded pinout: a board that wires its header differently gets its own
/// answer, and a header we can't read gets no mark rather than a wrong one.
fn power_polarity_silk(
    refdes: &str,
    pads: &[FpPad],
    courtyard: Option<Rect>,
    placement: Placement,
    pin_net: &HashMap<(String, String), &str>,
    outline: Option<Rect>,
) -> Vec<Sexpr> {
    let placed = |p: &FpPad| place_point(placement, p.px, p.py);
    let is_neg = |p: &FpPad| {
        pin_net
            .get(&(refdes.to_string(), p.num.clone()))
            .is_some_and(|n| NEG_RAIL_NETS.iter().any(|r| n.eq_ignore_ascii_case(r)))
    };
    let neg: Vec<(f64, f64)> = pads.iter().filter(|p| is_neg(p)).map(placed).collect();
    if neg.is_empty() || neg.len() == pads.len() {
        return Vec::new(); // nothing to distinguish — say nothing
    }
    let all: Vec<(f64, f64)> = pads.iter().map(placed).collect();
    let pad_bb = all.iter().fold(
        (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
        |(x0, y0, x1, y1), &(x, y)| (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
    );
    // Clear the *body*, not just the pads. A shrouded IDC power header overhangs
    // its pad box by millimetres, and a mark printed under the plastic is a mark
    // you can only read before you fit the connector — i.e. never, when it
    // matters. The courtyard is the footprint's own statement of its body size.
    let bb = match courtyard {
        Some((cx0, cy0, cx1, cy1)) => {
            let (a, b) = (
                place_point(placement, cx0, cy0),
                place_point(placement, cx1, cy1),
            );
            (
                pad_bb.0.min(a.0).min(b.0),
                pad_bb.1.min(a.1).min(b.1),
                pad_bb.2.max(a.0).max(b.0),
                pad_bb.3.max(a.1).max(b.1),
            )
        }
        None => pad_bb,
    };
    let mean =
        |v: &[(f64, f64)], f: fn(&(f64, f64)) -> f64| v.iter().map(f).sum::<f64>() / v.len() as f64;
    let (ncx, ncy) = (mean(&neg, |p| p.0), mean(&neg, |p| p.1));
    let (acx, acy) = (mean(&all, |p| p.0), mean(&all, |p| p.1));

    // Which end is it? The axis along which the −12 V pads sit furthest off the
    // header's centre — for a 2×N header that is the long axis.
    let (dx, dy) = (ncx - acx, ncy - acy);
    let gap = 1.4; // clear of the pads, still visibly "this end"
    let layer = if placement.back { "B.SilkS" } else { "F.SilkS" };
    let seed = |what: &str| format!("board.power.{refdes}.{what}");
    let (bar, label_at, rot) = if dx.abs() >= dy.abs() {
        let x = if dx < 0.0 { bb.0 - gap } else { bb.2 + gap };
        (
            (x, bb.1 - gap, x, bb.3 + gap),
            (
                if dx < 0.0 { x - 1.6 } else { x + 1.6 },
                (bb.1 + bb.3) / 2.0,
            ),
            90.0,
        )
    } else {
        let y = if dy < 0.0 { bb.1 - gap } else { bb.3 + gap };
        (
            (bb.0 - gap, y, bb.2 + gap, y),
            (
                (bb.0 + bb.2) / 2.0,
                if dy < 0.0 { y - 1.6 } else { y + 1.6 },
            ),
            0.0,
        )
    };
    // The bar sits hard against the pads and always fits; the label hangs past
    // it and, on a narrow board with the header at the edge, can hang off the
    // board entirely. Pull it back inside — a mark printed past the edge is a
    // mark nobody sees.
    let (lx, ly) = match outline {
        Some((x0, y0, x1, y1)) => {
            let m = 3.0;
            (
                label_at.0.clamp(x0 + m, (x1 - m).max(x0 + m)),
                label_at.1.clamp(y0 + m, (y1 - m).max(y0 + m)),
            )
        }
        None => label_at,
    };
    vec![
        silk_line(bar.0, bar.1, bar.2, bar.3, 0.5, layer, &seed("bar")),
        silk_text_on("-12V", lx, ly, rot, &seed("label"), layer, 1.1),
    ]
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
        // Solid-connect pads to the plane (a low-impedance ground; jack sleeves +
        // header GND especially want it). Also removes the fragile thermal-spoke
        // dependency so a tightly-placed edge-hugging GND pad can't "starve" to a
        // single spoke.
        Sexpr::list(vec![
            Sexpr::sym("connect_pads"),
            Sexpr::sym("yes"),
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

/// A placed pad's geometry for stitching-via clearance (absolute board coords).
struct PadGeo {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    net_idx: usize,
    /// Through-hole (both layers) — the pads that punch a hole in *both* ground
    /// pours and so can strand a pour island.
    tht: bool,
}

/// Distance from point `p` to segment `a`–`b`.
fn point_seg_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= 1e-12 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    (p.0 - (a.0 + t * dx)).hypot(p.1 - (a.1 + t * dy))
}

/// GND **stitching vias**: one beside each through-hole ground pad, tying the
/// F.Cu and B.Cu pours together there. The two-sided pour (DESIGN 6.2) relies on
/// through-hole GND pads to bridge its layers; where a GND pad is alone in a
/// region a dense THT grid (a sub-board header) fenced off, its pour becomes an
/// isolated island (KiCad `starved_thermal`). A via just off the pad reconnects
/// that island to the intact pour on the other layer. Each via is kept inside the
/// board edge and clear of all *other-net* copper (pads, tracks, vias); a pad
/// with no clear spot around it is skipped rather than forced. (25z.2)
fn ground_stitching_vias(
    outline: Rect,
    gnd_idx: usize,
    pads: &[PadGeo],
    tracks: &[Track],
    routed_vias: &[Via],
    opts: &RouteOptions,
) -> Vec<Via> {
    let via_r = opts.via_size_mm / 2.0;
    let clr = opts.clearance_mm;
    let (x0, y0, x1, y1) = outline;
    let inset = opts.edge_clearance_mm + via_r;

    let clear = |vx: f64, vy: f64, placed: &[Via]| -> bool {
        if vx < x0 + inset || vx > x1 - inset || vy < y0 + inset || vy > y1 - inset {
            return false;
        }
        for p in pads {
            // Overlap-avoid for same-net pads; full clearance for other nets.
            let m = via_r + if p.net_idx == gnd_idx { 0.0 } else { clr };
            if vx > p.x - p.w / 2.0 - m
                && vx < p.x + p.w / 2.0 + m
                && vy > p.y - p.h / 2.0 - m
                && vy < p.y + p.h / 2.0 + m
            {
                return false;
            }
        }
        for t in tracks.iter().filter(|t| t.net_idx != gnd_idx) {
            if point_seg_dist((vx, vy), t.start, t.end) < via_r + t.width_mm / 2.0 + clr {
                return false;
            }
        }
        for v in routed_vias.iter().filter(|v| v.net_idx != gnd_idx) {
            if (vx - v.at.0).hypot(vy - v.at.1) < via_r + v.size_mm / 2.0 + clr {
                return false;
            }
        }
        // Don't cluster two stitches on the same island.
        !placed
            .iter()
            .any(|v| (vx - v.at.0).hypot(vy - v.at.1) < 1.0)
    };

    let mut vias: Vec<Via> = Vec::new();
    for p in pads.iter().filter(|p| p.net_idx == gnd_idx && p.tht) {
        let off = p.w.max(p.h) / 2.0 + via_r + clr + 0.2;
        for k in 0..8 {
            let ang = k as f64 * std::f64::consts::FRAC_PI_4;
            let (vx, vy) = (p.x + off * ang.cos(), p.y + off * ang.sin());
            if clear(vx, vy, &vias) {
                vias.push(Via {
                    at: (vx, vy),
                    size_mm: opts.via_size_mm,
                    drill_mm: opts.via_drill_mm,
                    net_idx: gnd_idx,
                });
                break;
            }
        }
    }
    vias
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

    fn a_fact(extent: (f64, f64), origin_offset: (f64, f64), tht: Vec<Rect>) -> PartFacts {
        PartFacts {
            extent,
            body_extent: extent,
            origin_offset,
            side: Side::Front,
            height_mm: 5.0,
            standoff_mm: None,
            tht_pads: tht,
            pin_offsets: HashMap::new(),
        }
    }

    #[test]
    fn rotate_offset_quarter_turns() {
        assert_eq!(rotate_offset((3.0, 1.0), 0.0), (3.0, 1.0));
        assert_eq!(rotate_offset((3.0, 1.0), 90.0), (-1.0, 3.0));
        assert_eq!(rotate_offset((3.0, 1.0), 180.0), (-3.0, -1.0));
        assert_eq!(rotate_offset((3.0, 1.0), 270.0), (1.0, -3.0));
    }

    /// The THT keep-outs of a *rotated* part must move with it, in KiCad's
    /// rotation sense — a local pad `(x, y)` maps to `(y, -x)` at +90°. Reserving
    /// the un-rotated (or oppositely-rotated) squares still yields a DRC-clean
    /// board, it just reserves the wrong space and degrades routing, so pin it.
    #[test]
    fn tht_keepouts_rotate_with_the_part_in_kicad_sense() {
        // A lug 7.5mm out on +X, like an Alpha pot's mounting tab.
        let f = a_fact((14.0, 14.0), (0.0, 0.0), vec![(7.0, -0.5, 8.0, 0.5)]);

        // Un-rotated: the lug stays on +X of the origin.
        let flat = f.tht_pads_at(100.0, 50.0, false, 0.0);
        assert_eq!(flat, vec![(107.0, 49.5, 108.0, 50.5)]);

        // Rotated 90°: (x, y) -> (y, -x), so the +X lug swings to -Y.
        let turned = f.tht_pads_at(100.0, 50.0, false, 90.0);
        assert_eq!(turned, vec![(99.5, 42.0, 100.5, 43.0)]);

        // A rotated part's keep-out box carries its origin offset around too —
        // and in the SAME sense as the pads above. This assertion used to say
        // (0, +3), the opposite way, and the two lived side by side in this test
        // without anyone noticing. Self-consistent inside the placer (it
        // reserved a box exactly on the anchor) but wrong against the real
        // footprint, so a 90°-rotated pot's shaft landed ~11mm from its panel
        // hole. Latent until something actually got rotated.
        let off = a_fact((10.0, 4.0), (3.0, 0.0), vec![]);
        let box_rot = off.keepout_at_rot(0.0, 0.0, false, 90.0);
        // extent swaps to (4,10); the +X offset swings to -Y, like the lug.
        assert_eq!(box_rot, (-2.0, -8.0, 2.0, 2.0));

        // The invariant, stated directly: a part's keep-out and its pads move
        // together, on both faces and at every rotation. A lug on +X and an
        // offset on +X must always land in the same place.
        //
        // The back-side cases are the ones that bit: flipping AFTER rotating is
        // only harmless at 0°/180°, and put a back-mounted 90° power header's
        // keep-out ~10mm from its own copper. KiCad flips a footprint in its own
        // frame and then turns it.
        let both = a_fact((4.0, 4.0), (5.0, 0.0), vec![(4.5, -0.5, 5.5, 0.5)]);
        for back in [false, true] {
            for rot in [0.0, 90.0, 180.0, 270.0] {
                let b = both.keepout_at_rot(0.0, 0.0, back, rot);
                let (kx, ky) = ((b.0 + b.2) / 2.0, (b.1 + b.3) / 2.0);
                let pad = both.tht_pads_at(0.0, 0.0, back, rot)[0];
                let (px, py) = ((pad.0 + pad.2) / 2.0, (pad.1 + pad.3) / 2.0);
                assert!(
                    (kx - px).abs() < 1e-9 && (ky - py).abs() < 1e-9,
                    "back={back} rot={rot}: keep-out ({kx},{ky}) and pad ({px},{py}) \
                     must move together"
                );
            }
        }
    }

    #[test]
    fn control_rotation_stands_up_a_horizontal_pin_row() {
        // Pins in a horizontal row (like a badly-oriented pot) → stand it up.
        let wide = a_fact((10.0, 10.0), (0.0, 0.0), vec![(-3.0, -0.5, 3.0, 0.5)]);
        assert_eq!(control_rotation(&wide), 90.0);
        // Pins in a vertical column (the Alpha pot's real layout) → leave it.
        let tall = a_fact((10.0, 10.0), (0.0, 0.0), vec![(-0.5, -3.0, 0.5, 3.0)]);
        assert_eq!(control_rotation(&tall), 0.0);
        // No through-hole pads → fall back to the courtyard extent.
        let smd = a_fact((4.0, 1.0), (0.0, 0.0), vec![]);
        assert_eq!(control_rotation(&smd), 90.0);
    }

    #[test]
    fn decoupling_bonus_pairs_bypass_caps_to_their_ic() {
        use crate::model::{Circuit, Net, Part, PinRef, RefDes};
        let part = |r: &str, fp: &str| Part {
            refdes: RefDes(r.into()),
            value: String::new(),
            footprint: Some(fp.into()),
            library_part: None,
            mpn: None,
            sim: None,
            side: None,
        };
        let node = |r: &str, p: &str| PinRef {
            refdes: RefDes(r.into()),
            pin: p.into(),
        };
        let net = |name: &str, pins: Vec<PinRef>| Net {
            name: name.into(),
            pins,
            net_class: None,
        };
        let c = Circuit {
            name: "t".into(),
            parts: vec![
                part("U1", "Package_SO:SOIC-8"),
                part("C2", "Capacitor_SMD:C_0603_1608Metric"), // decoupling +12V↔GND
                part("C1", "Capacitor_SMD:C_0603_1608Metric"), // signal SIG↔GND
            ],
            nets: vec![
                net("+12V", vec![node("U1", "8"), node("C2", "1")]),
                net(
                    "GND",
                    vec![node("U1", "4"), node("C2", "2"), node("C1", "2")],
                ),
                net("SIG", vec![node("U1", "1"), node("C1", "1")]),
            ],
        };
        let bonus = decoupling_bonus(&c);
        // C2 bridges +12V↔GND → bonded to U1; C1 is a signal cap → no bond.
        assert!(bonus.iter().any(|(cap, ic, _)| cap == "C2" && ic == "U1"));
        assert!(!bonus.iter().any(|(cap, _, _)| cap == "C1"));
    }

    /// legion-of-bom-t5t: minimum_hp reported 3 HP for a board whose 9mm pots
    /// need more than 3 HP of width, because it only checked the overflow lane.
    /// It now asks the physical rules, which say the outline is too small.
    #[test]
    fn minimum_hp_rejects_a_width_where_a_part_does_not_fit() {
        use crate::model::{Circuit, Part, RefDes};
        let Some(dir) = crate::skidl::kicad_footprint_dir() else {
            return;
        };
        // One Alpha 9mm pot: its keep-out is ~14.5mm wide, so it cannot sit in
        // a 3 HP panel (15.24mm) with edge clearance on both sides.
        let circuit = Circuit {
            name: "t".into(),
            parts: vec![Part {
                refdes: RefDes("RV1".into()),
                value: "100k".into(),
                footprint: Some(
                    "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical".into(),
                ),
                library_part: None,
                mpn: None,
                sim: None,
                side: None,
            }],
            nets: vec![],
        };
        let Ok(facts) = build_facts(&circuit, &dir) else {
            return; // library layout differs; don't fail the unit suite
        };
        let pot = facts["RV1"].extent.0;
        let hp = minimum_hp(&circuit, &facts);
        use crate::panel::PanelSpec;
        let width = crate::panel::EurorackPanel::new(hp).width_mm();
        assert!(
            width >= pot + 2.0 * crate::rules::EDGE_CLEARANCE_MM,
            "min {hp} HP = {width:.1}mm cannot hold a {pot:.1}mm part with edge clearance"
        );
    }

    #[test]
    fn power_header_is_identified_and_only_when_free() {
        use crate::model::{Circuit, Part, RefDes};
        let part = |refdes: &str, fp: &str| Part {
            refdes: RefDes(refdes.into()),
            value: String::new(),
            footprint: Some(fp.into()),
            library_part: None,
            mpn: None,
            sim: None,
            side: None,
        };
        let c = Circuit {
            name: "t".into(),
            parts: vec![
                part("J3", "Connector_PinHeader_2.54mm:PinHeader_2x05"),
                part("J1", "Connector_Audio:Jack_3.5mm"),
            ],
            nets: vec![],
        };
        let none = HashMap::new();
        assert_eq!(power_header_refdes(&c, &none), vec!["J3".to_string()]);
        // An anchored header is not a free power header.
        let anchored: HashMap<String, (f64, f64)> = [("J3".to_string(), (0.0, 0.0))].into();
        assert!(power_header_refdes(&c, &anchored).is_empty());
    }

    /// A 2×5 Eurorack header, KiCad odd/even numbering: pins 1+2 are one rank,
    /// 9+10 the other. Pads carry no net until `nets` says so.
    fn header_pads() -> Vec<FpPad> {
        (1..=10)
            .map(|n: u32| FpPad {
                num: n.to_string(),
                px: if n % 2 == 1 { 0.0 } else { 2.54 },
                py: ((n - 1) / 2) as f64 * 2.54,
                w: 1.7,
                h: 1.7,
                layer: PadLayer::Both,
            })
            .collect()
    }

    /// The −12 V mark goes on the silk of the face the header mounts on, at the
    /// end whose pads are actually on the negative rail — read from the netlist,
    /// not from an assumed pinout.
    #[test]
    fn power_header_gets_a_minus_12v_mark_at_the_end_the_netlist_says() {
        let pads = header_pads();
        let nets: HashMap<(String, String), &str> = [
            (("J3".to_string(), "1".to_string()), "-12V"),
            (("J3".to_string(), "2".to_string()), "-12V"),
            (("J3".to_string(), "9".to_string()), "+12V"),
            (("J3".to_string(), "10".to_string()), "+12V"),
        ]
        .into();
        let placement = Placement {
            x_mm: 100.0,
            y_mm: 50.0,
            rotation_deg: 0.0,
            back: true,
        };
        let silk = power_polarity_silk("J3", &pads, None, placement, &nets, None);
        let text: String = silk.iter().map(|s| s.to_sexpr_string()).collect();
        assert!(text.contains("-12V"), "labelled: {text}");
        // Back-mounted, so the mark belongs on the back silk — the face the
        // builder is looking at while installing it — and mirrored to read.
        assert!(text.contains("B.SilkS"), "on the back silk: {text}");
        assert!(text.contains("mirror"), "back text reads correctly: {text}");
        assert!(!text.contains("F.SilkS"), "not on the front: {text}");
        // Pins 1+2 are the -12 V rank and 9+10 the +12 V one; which absolute end
        // each lands on is the flip's business, so derive it rather than pinning a
        // side. The bar must sit beyond the -12 V rank, away from +12 V.
        let neg_y = place_point(placement, 0.0, 0.0).1;
        let pos_y = place_point(placement, 0.0, 4.0 * 2.54).1;
        let bar = &silk[0];
        let pt = |key: &str| -> (f64, f64) {
            let p = bar.get(key).expect(key);
            (
                p.nth_atom(1).unwrap().parse().unwrap(),
                p.nth_atom(2).unwrap().parse().unwrap(),
            )
        };
        let (sx, sy) = pt("start");
        let (ex, ey) = pt("end");
        let away = (neg_y - pos_y).signum();
        assert!(
            (sy - neg_y) * away > 0.0 && (ey - neg_y) * away > 0.0,
            "bar is off the -12V end (neg {neg_y}, pos {pos_y}): {sy}, {ey}"
        );
        assert!((sy - ey).abs() < 1e-9, "bar runs across the end, not along");
        assert!(sx < ex, "bar spans the header's width");
    }

    /// No readable negative rail → no mark. An orientation stripe in the wrong
    /// place is worse than none: it is the mistake that destroys the module.
    #[test]
    fn power_header_with_no_readable_negative_rail_gets_no_mark() {
        let pads = header_pads();
        let placement = Placement {
            x_mm: 100.0,
            y_mm: 50.0,
            rotation_deg: 0.0,
            back: true,
        };
        assert!(
            power_polarity_silk("J3", &pads, None, placement, &HashMap::new(), None).is_empty()
        );
        // Every pad on the negative rail distinguishes no end either.
        let all_neg: HashMap<(String, String), &str> = (1..=10)
            .map(|n: u32| (("J3".to_string(), n.to_string()), "-12V"))
            .collect();
        assert!(power_polarity_silk("J3", &pads, None, placement, &all_neg, None).is_empty());
    }

    fn facts(entries: &[(&str, (f64, f64), Side)]) -> HashMap<String, PartFacts> {
        entries
            .iter()
            .map(|(r, extent, side)| {
                (
                    r.to_string(),
                    PartFacts {
                        extent: *extent,
                        body_extent: *extent,
                        origin_offset: (0.0, 0.0),
                        side: *side,
                        height_mm: 2.0,
                        standoff_mm: None,
                        tht_pads: Vec::new(),
                        pin_offsets: HashMap::new(),
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
    fn subboard_places_and_routes_via_synthesized_footprint() {
        // A sub-board (Daisy Seed) is a part whose footprint is synthesized from
        // `LobModule:` — no `.pretty` file, so this needs no KiCad install. It must
        // place (courtyard → keep-out) and route to its header pins (25z.1).
        let daisy = Circuit {
            name: "daisy".into(),
            parts: vec![Part::new("A1", "Daisy_Seed").with_footprint("LobModule:Daisy_Seed")],
            // A 2-pin net across the header rows must become copper; a lone GND pin
            // is exercised by the pour.
            nets: vec![
                Net::new(
                    "AUDIO",
                    vec![PinRef::new("A1", "1"), PinRef::new("A1", "40")],
                ),
                Net::new("GND", vec![PinRef::new("A1", "20")]),
            ],
        };
        // The footprint dir is never read (the module is synthesized), so any path
        // works — proving the sub-board path is self-contained.
        let art = generate_board_artifacts(&daisy, &BoardOptions::new("/nonexistent"))
            .expect("sub-board board generates");
        assert!(Sexpr::parse(&art.pcb).is_ok(), "board must parse");
        assert!(
            art.pcb.contains(r#""LobModule:Daisy_Seed""#),
            "the Daisy footprint is placed on the board"
        );
        let pads = art.pcb.matches("(pad \"").count();
        assert!(pads >= 40, "all 40 header pads emitted (got {pads})");
        assert!(
            art.route.conflicts.is_empty(),
            "the AUDIO net routes to the header pins: {:?}",
            art.route.conflicts
        );
        assert!(art.pcb.contains("(segment"), "AUDIO net becomes a track");
    }

    #[test]
    fn footprint_pads_swaps_wh_for_rotated_pads() {
        // A pad rotated 90°/270° swaps its width/height in the footprint frame —
        // without this a fine-pitch rotated SMD row (a Daisy sub-board's 1.27mm
        // headers) merges into one unroutable blob (25z.7).
        let fp = crate::sexpr::Sexpr::parse(
            r#"(footprint "T" (layer "F.Cu")
                 (pad "1" smd rect (at 0 0 0) (size 1.98 0.65) (layers "F.Cu"))
                 (pad "2" smd rect (at 5 0 90) (size 1.98 0.65) (layers "F.Cu"))
                 (pad "3" smd rect (at 10 0 270) (size 1.98 0.65) (layers "F.Cu"))
                 (pad "4" smd rect (at 15 0 180) (size 1.98 0.65) (layers "F.Cu")))"#,
        )
        .unwrap();
        let pads = footprint_pads(&fp);
        let g = |n: &str| pads.iter().find(|p| p.num == n).unwrap();
        assert_eq!((g("1").w, g("1").h), (1.98, 0.65), "0°: as-is");
        assert_eq!((g("2").w, g("2").h), (0.65, 1.98), "90°: swapped");
        assert_eq!((g("3").w, g("3").h), (0.65, 1.98), "270°: swapped");
        assert_eq!((g("4").w, g("4").h), (1.98, 0.65), "180°: as-is");
    }

    #[test]
    fn vendored_daisy_footprints_place_and_route() {
        // Real Electrosmith footprints (Patch SM = 40 THT pads, Seed2 DFM = 50 SMD
        // pads), embedded and named by physical pin — a net wired to pin "B2"/"B4"
        // connects straight through and routes. No KiCad install needed.
        for (fp, pads_min, a, b) in [
            ("LobModule:DAISY_PATCH_SM", 40, "B2", "B4"),
            // A3/A8 are inner fine-pitch SMD pads — routing them exercises the
            // rotated-pad fanout fix (25z.7).
            ("LobModule:DAISY_SEED2_DFM", 50, "A3", "A8"),
        ] {
            let c = Circuit {
                name: "mod".into(),
                parts: vec![Part::new("M1", "mod").with_footprint(fp)],
                nets: vec![Net::new(
                    "SIG",
                    vec![PinRef::new("M1", a), PinRef::new("M1", b)],
                )],
            };
            let art = generate_board_artifacts(&c, &BoardOptions::new("/nonexistent"))
                .unwrap_or_else(|e| panic!("{fp} generates: {e:?}"));
            assert!(Sexpr::parse(&art.pcb).is_ok(), "{fp} parses");
            assert!(art.pcb.contains(&format!("{fp:?}")), "{fp} placed");
            let pads = art.pcb.matches("(pad \"").count();
            assert!(pads >= pads_min, "{fp}: {pads_min} pads (got {pads})");
            assert!(
                art.route.conflicts.is_empty(),
                "{fp} SIG routes: {:?}",
                art.route.conflicts
            );
        }
    }

    #[test]
    fn subboard_net_wires_to_a_pad_by_function_name() {
        // A net references the Daisy pin by function name, not pad number; it must
        // resolve to the right pad and route (25z.3).
        let daisy = Circuit {
            name: "daisy".into(),
            parts: vec![Part::new("A1", "Daisy_Seed").with_footprint("LobModule:Daisy_Seed")],
            nets: vec![Net::new(
                "SIG",
                vec![
                    PinRef::new("A1", "AUDIO_OUT_L"), // pad 18, by name
                    PinRef::new("A1", "A0"),          // pad 22, by ADC alias
                ],
            )],
        };
        let art = generate_board_artifacts(&daisy, &BoardOptions::new("/nonexistent"))
            .expect("generates");
        // If name resolution failed, both pads would be obstacles → no SIG track.
        assert!(
            art.route.conflicts.is_empty(),
            "SIG routes: {:?}",
            art.route.conflicts
        );
        assert!(
            art.pcb.contains("(segment"),
            "the function-named net becomes copper — resolution worked"
        );
    }

    #[test]
    fn part_height_and_standoff_heuristics() {
        assert!(part_height_mm("Connector_PinHeader_2.54mm:PinHeader_2x05") > 5.0);
        assert!(part_height_mm("Capacitor_THT:CP_Radial_D6.3mm") > 5.0);
        assert!(part_height_mm("Package_SO:SOIC-8_3.9x4.9mm") < 3.0);
        assert!(part_height_mm("Resistor_SMD:R_0603_1608Metric") < 1.5);
        assert_eq!(subboard_standoff("LobModule:Daisy_Seed"), Some(11.0));
        assert_eq!(subboard_standoff("Resistor_SMD:R_0603_1608Metric"), None);
    }

    #[test]
    fn collision_flags_only_tall_same_side_parts_under_a_subboard() {
        let sub = |refdes: &str| PlacedPart {
            refdes: refdes.into(),
            keepout: (10.0, 10.0, 28.0, 61.0), // 18×51 Daisy body
            back: false,
            height_mm: 8.5,
            standoff_mm: Some(11.0),
        };
        let part = |refdes: &str, x: f64, h: f64, back: bool| PlacedPart {
            refdes: refdes.into(),
            keepout: (x, 20.0, x + 4.0, 24.0),
            back,
            height_mm: h,
            standoff_mm: None,
        };
        let parts = vec![
            sub("A1"),
            part("C1", 15.0, 3.0, false),  // under it, short → fine
            part("J1", 15.0, 12.0, false), // under it, tall → collides
            part("J2", 15.0, 12.0, true),  // under it but on the back → fine
            part("J3", 40.0, 12.0, false), // tall but not under it → fine
        ];
        let hits = detect_collisions(&parts);
        assert_eq!(hits.len(), 1, "only J1 collides: {hits:?}");
        assert!(hits[0].contains("J1") && hits[0].contains("A1"));
    }

    #[test]
    fn placement_clear_enforces_side_height_and_pin_rules() {
        let clr = 0.2;
        // A back-side sub-board with an 11mm standoff and one pin at (2..4, 2..4).
        let sub = Placed {
            body: (0.0, 0.0, 18.0, 51.0),
            back: true,
            height_mm: 8.5,
            standoff_mm: Some(11.0),
            tht_pads: vec![(2.0, 2.0, 4.0, 4.0)],
        };
        let placed = [sub];
        let body = (8.0, 8.0, 10.0, 10.0); // same-side, clear of the pin
        let on_pin = (2.5, 2.5, 3.5, 3.5);
        // Short same-side body tucks under the sub-board.
        assert!(placement_clear(&body, true, 1.0, None, &[], &placed, clr));
        // A tall same-side body can't fit under it.
        assert!(!placement_clear(&body, true, 12.0, None, &[], &placed, clr));
        // Any body over a through-hole pin is blocked, regardless of side/height.
        assert!(!placement_clear(
            &on_pin,
            true,
            1.0,
            None,
            &[],
            &placed,
            clr
        ));
        assert!(!placement_clear(
            &on_pin,
            false,
            1.0,
            None,
            &[],
            &placed,
            clr
        ));
        // An opposite-side body (over the floating body, not a pin) is independent
        // — even a tall one.
        assert!(placement_clear(&body, false, 20.0, None, &[], &placed, clr));
        // A candidate's own pin may pass over the standoff gap, not into a pin.
        assert!(placement_clear(
            &body,
            false,
            1.0,
            None,
            &[(8.0, 8.0, 10.0, 10.0)],
            &placed,
            clr
        ));
        assert!(!placement_clear(
            &body,
            false,
            1.0,
            None,
            &[(2.5, 2.5, 3.5, 3.5)],
            &placed,
            clr
        ));
    }

    #[test]
    fn short_parts_pack_under_a_subboard_tall_ones_do_not() {
        // A1 is a sub-board (18×51, 11mm standoff); C1 (short) and U1 (tall) are
        // both netted to it. The placer tucks C1 under A1's body and keeps U1 clear.
        let c = Circuit {
            name: "stack".into(),
            parts: vec![
                Part::new("A1", "Daisy"),
                Part::new("C1", "100n"),
                Part::new("U1", "reg"),
            ],
            nets: vec![
                Net::new("N1", vec![PinRef::new("A1", "1"), PinRef::new("C1", "1")]),
                Net::new("N2", vec![PinRef::new("A1", "2"), PinRef::new("U1", "1")]),
            ],
        };
        let mk = |w: f64, h: f64, height: f64, standoff: Option<f64>| PartFacts {
            extent: (w, h),
            body_extent: (w, h),
            origin_offset: (0.0, 0.0),
            side: Side::Front,
            height_mm: height,
            standoff_mm: standoff,
            tht_pads: Vec::new(),
            pin_offsets: HashMap::new(), // no pins → the whole body is packable space
        };
        let mut facts = HashMap::new();
        facts.insert("A1".to_string(), mk(18.0, 51.0, 8.5, Some(11.0)));
        facts.insert("C1".to_string(), mk(3.0, 3.0, 1.0, None));
        facts.insert("U1".to_string(), mk(3.0, 3.0, 12.0, None));
        let placer = SeededPlacer::new(40.0, 100.0, (0.0, 0.0), HashMap::new());
        let p = placer.place(&c, &facts);
        let ko = |r: &str| facts[r].keepout_at(p[r].x_mm, p[r].y_mm, p[r].back);
        assert!(
            rects_overlap(&ko("A1"), &ko("C1"), 0.0),
            "short C1 packs under the sub-board"
        );
        assert!(
            !rects_overlap(&ko("A1"), &ko("U1"), 0.0),
            "tall U1 stays clear of the sub-board"
        );
    }

    #[test]
    fn stitching_via_lands_beside_a_through_hole_ground_pad() {
        let opts = RouteOptions::default();
        let gnd = 5;
        let outline = (0.0, 0.0, 40.0, 40.0);
        let pads = vec![
            // A through-hole GND pad in open board → gets a stitch beside it.
            PadGeo {
                x: 20.0,
                y: 20.0,
                w: 1.7,
                h: 1.7,
                net_idx: gnd,
                tht: true,
            },
            // An unrelated SMD pad, far away.
            PadGeo {
                x: 5.0,
                y: 5.0,
                w: 1.0,
                h: 1.0,
                net_idx: 2,
                tht: false,
            },
        ];
        let vias = ground_stitching_vias(outline, gnd, &pads, &[], &[], &opts);
        assert_eq!(vias.len(), 1, "one stitch for the one THT GND pad");
        assert_eq!(vias[0].net_idx, gnd);
        let d = (vias[0].at.0 - 20.0).hypot(vias[0].at.1 - 20.0);
        assert!(d > 0.85 && d < 3.0, "stitch sits just off the pad (d={d})");
    }

    #[test]
    fn stitching_skips_smd_pads_and_boxed_in_pads() {
        let opts = RouteOptions::default();
        let gnd = 5;
        let outline = (0.0, 0.0, 40.0, 40.0);
        // An SMD (single-layer) GND pad doesn't bridge the pours → no stitch.
        let smd = vec![PadGeo {
            x: 20.0,
            y: 20.0,
            w: 1.0,
            h: 1.0,
            net_idx: gnd,
            tht: false,
        }];
        assert!(ground_stitching_vias(outline, gnd, &smd, &[], &[], &opts).is_empty());
        // A THT GND pad fenced in on every side by other-net copper is skipped,
        // never forced into a clearance violation.
        let wall = |x: f64, y: f64, w: f64, h: f64| PadGeo {
            x,
            y,
            w,
            h,
            net_idx: 2,
            tht: false,
        };
        let boxed = vec![
            PadGeo {
                x: 20.0,
                y: 20.0,
                w: 1.7,
                h: 1.7,
                net_idx: gnd,
                tht: true,
            },
            wall(17.5, 20.0, 2.0, 10.0),
            wall(22.5, 20.0, 2.0, 10.0),
            wall(20.0, 17.5, 10.0, 2.0),
            wall(20.0, 22.5, 10.0, 2.0),
        ];
        assert!(
            ground_stitching_vias(outline, gnd, &boxed, &[], &[], &opts).is_empty(),
            "a boxed-in pad is skipped, not forced"
        );
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
    fn overflow_parts_drop_clear_below_the_board_edge() {
        // A free part that fits nowhere must land in the off-board lane below the
        // bottom edge — never straddling it (that would fake a copper_edge_clearance
        // DRC error instead of surfacing as an unrouted net). j54.24.
        let board_h = 128.5;
        let mut lane = board_h + OVERFLOW_GAP_MM;
        let a = overflow_drop(3.0, (9.0, 9.0), &mut lane);
        let b = overflow_drop(3.0, (5.0, 5.0), &mut lane);
        // Both start below the board edge with clearance to spare.
        assert!(
            a.1 >= board_h + OVERFLOW_GAP_MM,
            "first overflow clears the edge"
        );
        assert!(
            b.1 >= board_h + OVERFLOW_GAP_MM,
            "second overflow clears the edge"
        );
        // Stacked, never overlapping the previous overflow part.
        assert!(
            b.1 >= a.3,
            "overflow parts stack instead of overlapping: {a:?} {b:?}"
        );
    }

    #[test]
    fn flip_layer_swaps_front_and_back() {
        assert_eq!(flip_layer("F.SilkS").as_deref(), Some("B.SilkS"));
        assert_eq!(flip_layer("B.Cu").as_deref(), Some("F.Cu"));
        assert_eq!(flip_layer("Edge.Cuts"), None); // non-sided, unchanged
    }

    #[test]
    fn flip_to_back_mirrors_y_and_flips_layers_and_text() {
        let mut e = crate::sexpr::Sexpr::parse(
            r#"(fp_text user "R1" (at 1.5 2) (layer "F.SilkS") (effects (font (size 1 1))))"#,
        )
        .unwrap();
        flip_to_back(&mut e);
        let out = e.to_sexpr_string();
        assert!(out.contains("(at 1.5 -2)"), "y mirrored: {out}");
        assert!(out.contains(r#"(layer "B.SilkS")"#), "layer flipped: {out}");
        assert!(out.contains("mirror"), "back text mirrored: {out}");
    }

    /// KiCad's ground truth, from `pcbnew`'s `FOOTPRINT::Flip`: flipping
    /// `TQFP-120_14x14mm_P0.4mm` sends a pad at `(-5.8, 7.7, 90°)` to
    /// `(-5.8, -7.7, 270°)` — X held, Y negated, angle reversed. Pin this, because
    /// the X-mirrored convention we used before is a 180° turn away from it and
    /// looks identical on every symmetric part.
    #[test]
    fn flip_to_back_matches_kicads_own_flip_of_a_rotated_pad() {
        let mut e = crate::sexpr::Sexpr::parse(
            r#"(pad "31" smd roundrect (at -5.8 7.7 90) (size 0.28 1.5) (layers "F.Cu" "F.Mask"))"#,
        )
        .unwrap();
        flip_to_back(&mut e);
        let out = e.to_sexpr_string();
        assert!(out.contains("(at -5.8 -7.7 -90)"), "{out}");
        assert!(out.contains(r#"(layers "B.Cu" "B.Mask")"#), "{out}");
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
            SilkValues::All,
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

    /// The kit this project ships has JLCPCB place the SMD and the buyer fit the
    /// through-hole panel hardware, so a value on a 0603 is silk nobody reads —
    /// and it collides with its neighbours. Through-hole parts keep theirs.
    #[test]
    fn values_go_on_silk_only_for_the_parts_a_person_solders() {
        let render = |pad_kind: &str, mode| {
            let fp = Sexpr::parse(&format!(
                r#"(footprint "X" (layer "F.Cu")
                     (property "Reference" "REF**" (at 0 0) (layer "F.SilkS"))
                     (property "Value" "X" (at 0 0) (layer "F.Fab") (hide yes))
                     (pad "1" {pad_kind} rect (at 0 0) (size 1 1) (layers "F.Cu")))"#
            ))
            .unwrap();
            transform_footprint(
                fp,
                "lib:X",
                "C7",
                "100nF",
                mode,
                Placement {
                    x_mm: 10.0,
                    y_mm: 10.0,
                    rotation_deg: 0.0,
                    back: false,
                },
                &HashMap::new(),
                &HashMap::new(),
            )
            .to_sexpr_string()
        };
        // SMD: only the refdes reaches silk — the assembler fits this one.
        let smd = render("smd", SilkValues::HandSoldered);
        assert_eq!(
            smd.matches("F.SilkS").count(),
            1,
            "an SMD part should keep its value off the silk:\n{smd}"
        );
        // The same part through-hole: a person solders it, so label it.
        let tht = render("thru_hole", SilkValues::HandSoldered);
        assert!(
            tht.matches("F.SilkS").count() >= 2,
            "a hand-soldered part keeps its value on silk:\n{tht}"
        );
        // Both escape hatches still work.
        assert!(render("smd", SilkValues::All).matches("F.SilkS").count() >= 2);
        assert_eq!(
            render("thru_hole", SilkValues::None)
                .matches("F.SilkS")
                .count(),
            1
        );
    }

    #[test]
    fn the_legend_joins_brand_and_rev_and_drops_what_is_missing() {
        let l = SilkLegend {
            brand: Some("Puget Audio".into()),
            rev: Some("v1.2".into()),
            note: Some("VC slew limiter".into()),
        };
        assert_eq!(l.lines(), vec!["Puget Audio · v1.2", "VC slew limiter"]);
        // A missing piece vanishes rather than leaving a stray separator.
        let brand_only = SilkLegend {
            brand: Some("Puget Audio".into()),
            ..Default::default()
        };
        assert_eq!(brand_only.lines(), vec!["Puget Audio"]);
        // Whitespace is not content.
        let blank = SilkLegend {
            rev: Some("  ".into()),
            ..Default::default()
        };
        assert!(blank.lines().is_empty());
        assert!(SilkLegend::default().lines().is_empty());
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
                SilkValues::All,
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
            SilkValues::All,
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

    /// End to end over the real KiCad library: a Eurorack power header lands on
    /// the back copper, and its −12 V end is marked on the back silk.
    #[test]
    fn a_generated_board_puts_the_power_header_on_the_back_and_marks_minus_12v() {
        use crate::model::{Circuit, Net, Part, PinRef, RefDes};
        let Some(dir) = crate::skidl::kicad_footprint_dir() else {
            return;
        };
        let header = Part {
            refdes: RefDes("J3".into()),
            value: "Conn_02x05_Odd_Even".into(),
            footprint: Some("Connector_PinHeader_2.54mm:PinHeader_2x05_P2.54mm_Vertical".into()),
            library_part: None,
            mpn: None,
            sim: None,
            // Declared front, exactly as SKiDL writes it — the house rule for a
            // Eurorack power header still has to win, or no board gets it right.
            side: Some(Side::Front),
        };
        let pin = |p: &str| PinRef {
            refdes: RefDes("J3".into()),
            pin: p.into(),
        };
        let circuit = Circuit {
            name: "pwr".into(),
            parts: vec![
                header,
                Part::new("R1", "1k").with_footprint("Resistor_SMD:R_0805_2012Metric"),
            ],
            nets: vec![
                Net {
                    name: "-12V".into(),
                    pins: vec![pin("1"), pin("2")],
                    net_class: None,
                },
                Net {
                    name: "+12V".into(),
                    pins: vec![pin("9"), pin("10")],
                    net_class: None,
                },
            ],
        };
        // The rule belongs to Eurorack module boards, so place it like one — a
        // grid-placed bench board is not a module and keeps its parts on top.
        let mut opts = BoardOptions::new(dir);
        opts.placer = Box::new(EurorackPlacer {
            width_mm: 40.0,
            height_mm: 128.5,
            origin_mm: (100.0, 100.0),
            anchors: HashMap::new(),
        });
        let board = match generate_board(&circuit, &opts) {
            Ok(b) => b,
            Err(_) => return, // library layout differs; don't fail the unit suite
        };
        assert!(crate::sexpr::Sexpr::parse(&board).is_ok(), "must parse");
        // The header's own footprint sits on the back copper…
        let j3 = board
            .split("(footprint ")
            .find(|b| b.contains(r#""Reference" "J3""#))
            .expect("J3 emitted");
        assert!(
            j3.contains(r#"(layer "B.Cu")"#),
            "power header mounts on the back: {}",
            &j3[..j3.len().min(200)]
        );
        // …and the orientation mark the build guide promises is on the back silk.
        assert!(board.contains(r#""-12V""#), "-12V label drawn");
        let mark = board
            .split("(gr_text ")
            .find(|b| b.starts_with(r#""-12V""#))
            .expect("-12V gr_text");
        assert!(
            mark.contains(r#"(layer "B.SilkS")"#),
            "the mark is on the face the header mounts on: {}",
            &mark[..mark.len().min(200)]
        );
        // `justify` must sit beside `font` inside `effects`, not inside `font`.
        // Nested, KiCad refuses to load the whole board — which our own parser
        // accepts happily, so only this shape check catches it.
        let effects = mark.split("(effects").nth(1).expect("effects block");
        let font_end = effects.find("(justify").expect("mirrored");
        // Balanced before `justify` means `font` already closed, so `justify` is
        // its sibling; unbalanced would mean it is nested inside.
        assert_eq!(
            effects[..font_end].matches('(').count(),
            effects[..font_end].matches(')').count(),
            "justify is a sibling of font, not a child: {effects:.200}"
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
