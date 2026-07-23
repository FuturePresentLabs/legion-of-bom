//! Routing — turning placed pads + nets into copper tracks. DESIGN.md 6.5.
//!
//! Architecture (forward-looking to the iterative layout loop, j54.6): routing is
//! a seam, [`Router`], analogous to [`Placer`](crate::board::Placer) for
//! placement. The board generator computes each pad's absolute rectangle, groups
//! pads by net, and hands them to a `Router`; the returned [`Track`]s and
//! [`Via`]s are appended to the same board assembly as footprints and the pour.
//!
//! Two routers ship:
//!
//! * [`MstRouter`] — a naive baseline: connect each net's pads with a Euclidean
//!   minimum spanning tree of straight tracks on the front copper. It ignores
//!   obstacles, so a track will happily short across an intervening pad; useful
//!   only for trivial/pre-cleared layouts and as a test double.
//! * [`GridRouter`] — the real first cut: an obstacle-aware **two-layer maze
//!   router**. It rasterises the board into a routing grid, marks other nets'
//!   pads and already-routed copper as obstacles (inflated by clearance), and
//!   finds a least-cost path per connection with Dijkstra, dropping a [`Via`] to
//!   the back copper when it must cross. It routes *around* pads rather than
//!   through them.
//!
//! `GridRouter` routes nets in order and does **not** rip up and retry: if a net
//! can't reach a pad given what's already committed, that pad is reported in
//! [`RouteOutput::conflicts`] for the iterative loop (j54.6) or the manual escape
//! hatch (6.8). Rip-up-and-retry, net ordering by criticality, and cost-weighted
//! routing are that loop's job; this is the routing primitive it drives.
//!
//! Trace/via defaults follow real Eurorack practice (Mutable Instruments boards):
//! 0.25 mm signal traces, 0.8/0.4 mm vias, ~0.2 mm clearance.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::board::{det_uuid, mm};
use crate::sexpr::Sexpr;

/// Which copper a pad is on. SMD pads sit on one side; through-hole pads (`*.Cu`)
/// are on both and so are obstacles on both layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadLayer {
    Front,
    Back,
    Both,
}

impl PadLayer {
    fn on(self, layer: usize) -> bool {
        match self {
            PadLayer::Front => layer == FRONT,
            PadLayer::Back => layer == BACK,
            PadLayer::Both => true,
        }
    }
}

/// A pad rectangle in board coordinates (mm) — the input the router connects.
#[derive(Debug, Clone)]
pub struct PadPoint {
    pub refdes: String,
    pub pad: String,
    pub x_mm: f64,
    pub y_mm: f64,
    pub w_mm: f64,
    pub h_mm: f64,
    pub layer: PadLayer,
}

/// A net to route: its board net index, name, and the pads to join.
#[derive(Debug, Clone)]
pub struct RouteNet {
    pub net_idx: usize,
    pub name: String,
    pub pads: Vec<PadPoint>,
}

/// A routed copper track segment.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub start: (f64, f64),
    pub end: (f64, f64),
    pub width_mm: f64,
    pub layer: String,
    pub net_idx: usize,
}

/// A routed via (a layer transition), joining front and back copper.
#[derive(Debug, Clone, PartialEq)]
pub struct Via {
    pub at: (f64, f64),
    pub size_mm: f64,
    pub drill_mm: f64,
    pub net_idx: usize,
}

/// What a router produced.
#[derive(Debug, Default)]
pub struct RouteOutput {
    pub tracks: Vec<Track>,
    pub vias: Vec<Via>,
    /// Connections left unrouted — handed to the iterative loop (j54.6) or manual
    /// routing (6.8). Empty when everything routed cleanly.
    pub conflicts: Vec<String>,
}

/// Track/via geometry, clearance, grid resolution, and the routable area.
#[derive(Debug, Clone)]
pub struct RouteOptions {
    pub signal_width_mm: f64,
    pub via_size_mm: f64,
    pub via_drill_mm: f64,
    pub clearance_mm: f64,
    /// Routing-grid cell size (mm). Finer = more precise, slower.
    pub grid_mm: f64,
    /// Cost (mm-equivalent) charged for a layer change, to discourage vias.
    pub via_cost_mm: f64,
    /// Routable rectangle `(min_x, min_y, max_x, max_y)`; defaults to the pads'
    /// bounding box (plus a margin) when `None`.
    pub bounds: Option<(f64, f64, f64, f64)>,
    pub front: String,
    pub back: String,
}

impl Default for RouteOptions {
    fn default() -> Self {
        // Eurorack-conventional defaults (see module docs).
        RouteOptions {
            signal_width_mm: 0.25,
            via_size_mm: 0.8,
            via_drill_mm: 0.4,
            clearance_mm: 0.2,
            grid_mm: 0.2,
            via_cost_mm: 2.0,
            bounds: None,
            front: "F.Cu".into(),
            back: "B.Cu".into(),
        }
    }
}

/// Turns placed pads + nets into copper — **the** routing extensibility seam. The
/// iterative layout loop (j54.6) is a smarter `Router`; the board generator just
/// consumes the [`RouteOutput`].
pub trait Router {
    fn route(&self, nets: &[RouteNet], opts: &RouteOptions) -> RouteOutput;
}

// ---- MstRouter: naive baseline --------------------------------------------

/// Naive baseline: a per-net Euclidean MST of straight tracks on the front
/// copper, ignoring obstacles. See the module docs — use [`GridRouter`] for real
/// boards.
#[derive(Debug, Clone, Default)]
pub struct MstRouter;

impl Router for MstRouter {
    fn route(&self, nets: &[RouteNet], opts: &RouteOptions) -> RouteOutput {
        let mut out = RouteOutput::default();
        let mut nets: Vec<&RouteNet> = nets.iter().filter(|n| n.pads.len() >= 2).collect();
        nets.sort_by_key(|n| n.net_idx);
        for net in nets {
            for (i, j) in mst_edges(&net.pads) {
                let a = (net.pads[i].x_mm, net.pads[i].y_mm);
                let b = (net.pads[j].x_mm, net.pads[j].y_mm);
                if a == b {
                    continue;
                }
                out.tracks.push(Track {
                    start: a,
                    end: b,
                    width_mm: opts.signal_width_mm,
                    layer: opts.front.clone(),
                    net_idx: net.net_idx,
                });
            }
        }
        out
    }
}

/// Minimum spanning tree over `pads` (complete Euclidean graph) as index pairs —
/// Prim's, fine for the small nets on a Eurorack module.
fn mst_edges(pads: &[PadPoint]) -> Vec<(usize, usize)> {
    let n = pads.len();
    let mut edges = Vec::new();
    if n < 2 {
        return edges;
    }
    let dist2 = |i: usize, j: usize| {
        let dx = pads[i].x_mm - pads[j].x_mm;
        let dy = pads[i].y_mm - pads[j].y_mm;
        dx * dx + dy * dy
    };
    let mut in_tree = vec![false; n];
    in_tree[0] = true;
    for _ in 1..n {
        let (mut best, mut ends) = (f64::INFINITY, None);
        for i in (0..n).filter(|&i| in_tree[i]) {
            for j in (0..n).filter(|&j| !in_tree[j]) {
                let d = dist2(i, j);
                if d < best {
                    best = d;
                    ends = Some((i, j));
                }
            }
        }
        match ends {
            Some((i, j)) => {
                in_tree[j] = true;
                edges.push((i, j));
            }
            None => break,
        }
    }
    edges
}

// ---- GridRouter: two-layer maze router ------------------------------------

const FRONT: usize = 0;
const BACK: usize = 1;

/// Per-cell, per-layer ownership on the routing grid.
#[derive(Clone, Copy, PartialEq)]
enum Cell {
    /// Free copper — any net may route here.
    Free,
    /// Owned by a net (its pad, its trace, or their clearance halo); only that
    /// net may route here.
    Owner(usize),
    /// A clearance pinch between two nets — no net may route here.
    Blocked,
}

/// Obstacle-aware two-layer maze router. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct GridRouter;

impl Router for GridRouter {
    fn route(&self, nets: &[RouteNet], opts: &RouteOptions) -> RouteOutput {
        let mut out = RouteOutput::default();
        let routable: Vec<&RouteNet> = nets.iter().filter(|n| n.pads.len() >= 2).collect();
        if routable.is_empty() {
            return out;
        }

        let res = opts.grid_mm.max(0.01);
        let (minx, miny, maxx, maxy) = opts.bounds.unwrap_or_else(|| bounds_of(nets, 2.0));
        let cols = (((maxx - minx) / res).ceil() as usize).max(1) + 1;
        let rows = (((maxy - miny) / res).ceil() as usize).max(1) + 1;
        let cell_of = |x: f64, y: f64| {
            let c = (((x - minx) / res).round() as isize).clamp(0, cols as isize - 1) as usize;
            let r = (((y - miny) / res).round() as isize).clamp(0, rows as isize - 1) as usize;
            (c, r)
        };
        let mm_of = |c: usize, r: usize| (minx + c as f64 * res, miny + r as f64 * res);

        let mut grid = Grid {
            cols,
            rows,
            cells: vec![Cell::Free; cols * rows * 2],
        };

        // Clearance halo radius, in cells: keep other nets a trace-half + clearance away.
        let halo = ((opts.clearance_mm + opts.signal_width_mm / 2.0) / res).ceil() as isize;
        // A via is bigger than a track, so it needs a wider keep-out.
        let via_halo = ((opts.via_size_mm / 2.0 + opts.clearance_mm + opts.signal_width_mm / 2.0)
            / res)
            .ceil() as isize;

        // Paint every pad (and its clearance halo) as its net's territory. Do the
        // cores first so a halo never overwrites a real pad connection point.
        let mut pad_cells: Vec<Vec<(usize, (usize, usize))>> = Vec::new(); // per routable net
        for net in nets {
            for pad in &net.pads {
                for layer in [FRONT, BACK] {
                    if !pad.layer.on(layer) {
                        continue;
                    }
                    let (cc, cr) = cell_of(pad.x_mm, pad.y_mm);
                    for (c, r) in pad_core_cells(cc, cr, pad.w_mm, pad.h_mm, res, cols, rows) {
                        grid.claim_core(c, r, layer, net.net_idx);
                    }
                }
            }
        }
        for net in nets {
            for pad in &net.pads {
                for layer in [FRONT, BACK] {
                    if !pad.layer.on(layer) {
                        continue;
                    }
                    let (cc, cr) = cell_of(pad.x_mm, pad.y_mm);
                    let (pw, ph) = (
                        (pad.w_mm / 2.0 / res).ceil() as isize,
                        (pad.h_mm / 2.0 / res).ceil() as isize,
                    );
                    grid.halo(cc, cr, layer, net.net_idx, pw + halo, ph + halo);
                }
            }
        }

        // Record each routable net's pad cells (for source/target sets).
        for net in &routable {
            let mut cells = Vec::new();
            for (pi, pad) in net.pads.iter().enumerate() {
                let (c, r) = cell_of(pad.x_mm, pad.y_mm);
                let layer = if pad.layer.on(FRONT) { FRONT } else { BACK };
                cells.push((pi, (c, r)));
                // Ensure the exact pad centre is a connection point for its net.
                grid.set(c, r, layer, Cell::Owner(net.net_idx));
            }
            pad_cells.push(cells);
        }

        // Route each net (deterministic order), growing a tree from pad 0.
        let mut order: Vec<usize> = (0..routable.len()).collect();
        order.sort_by_key(|&i| routable[i].net_idx);
        for &ni in &order {
            let net = routable[ni];
            let step = (res * 1000.0) as i64;
            let via_cost = (opts.via_cost_mm * 1000.0) as i64;

            // Connected component: cells already part of this net's routed tree.
            let mut connected: Vec<(usize, usize, usize)> = Vec::new(); // (c,r,layer)
            let (p0c, p0r) = pad_cells[ni][0].1;
            let l0 = if net.pads[0].layer.on(FRONT) {
                FRONT
            } else {
                BACK
            };
            connected.push((p0c, p0r, l0));

            let mut pending: Vec<usize> = (1..net.pads.len()).collect();
            while let Some(k) = pending.pop() {
                let (tc, tr) = pad_cells[ni][k].1;
                let tl = if net.pads[k].layer.on(FRONT) {
                    FRONT
                } else {
                    BACK
                };
                match grid.route_one(
                    net.net_idx,
                    &connected,
                    (tc, tr, tl),
                    step,
                    via_cost,
                    via_halo,
                ) {
                    Some(path) => {
                        emit_path(&path, net.net_idx, opts, &mm_of, &mut out);
                        for &(c, r, l) in &path {
                            grid.commit(c, r, l, net.net_idx, halo);
                            if !connected.contains(&(c, r, l)) {
                                connected.push((c, r, l));
                            }
                        }
                        // Reserve each via's wider body so later nets keep clear.
                        for w in path.windows(2) {
                            if w[0].2 != w[1].2 {
                                grid.commit_via(w[0].0, w[0].1, net.net_idx, via_halo);
                            }
                        }
                    }
                    None => out.conflicts.push(format!(
                        "net {} ({}): could not route to pad {}.{}",
                        net.net_idx, net.name, net.pads[k].refdes, net.pads[k].pad
                    )),
                }
            }
        }
        out
    }
}

/// The routing grid: `cols × rows × 2` layers of [`Cell`].
struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
}

impl Grid {
    #[inline]
    fn idx(&self, c: usize, r: usize, layer: usize) -> usize {
        (layer * self.rows + r) * self.cols + c
    }
    #[inline]
    fn get(&self, c: usize, r: usize, layer: usize) -> Cell {
        self.cells[self.idx(c, r, layer)]
    }
    #[inline]
    fn set(&mut self, c: usize, r: usize, layer: usize, v: Cell) {
        let i = self.idx(c, r, layer);
        self.cells[i] = v;
    }
    /// A pad core: this net owns it outright (overrides Free; a genuine overlap
    /// of two nets' cores becomes Blocked).
    fn claim_core(&mut self, c: usize, r: usize, layer: usize, net: usize) {
        match self.get(c, r, layer) {
            Cell::Free => self.set(c, r, layer, Cell::Owner(net)),
            Cell::Owner(m) if m == net => {}
            Cell::Owner(_) => self.set(c, r, layer, Cell::Blocked),
            Cell::Blocked => {}
        }
    }
    /// Paint a clearance halo (`±rx, ±ry` cells) as `net`'s territory; where it
    /// meets another net's territory the pinch is [`Cell::Blocked`].
    fn halo(&mut self, cc: usize, cr: usize, layer: usize, net: usize, rx: isize, ry: isize) {
        for dr in -ry..=ry {
            for dc in -rx..=rx {
                let (c, r) = (cc as isize + dc, cr as isize + dr);
                if c < 0 || r < 0 || c >= self.cols as isize || r >= self.rows as isize {
                    continue;
                }
                let (c, r) = (c as usize, r as usize);
                match self.get(c, r, layer) {
                    Cell::Free => self.set(c, r, layer, Cell::Owner(net)),
                    Cell::Owner(m) if m != net => self.set(c, r, layer, Cell::Blocked),
                    _ => {}
                }
            }
        }
    }
    /// Can `net` occupy this cell?
    #[inline]
    fn passable(&self, c: usize, r: usize, layer: usize, net: usize) -> bool {
        match self.get(c, r, layer) {
            Cell::Free => true,
            Cell::Owner(n) => n == net,
            Cell::Blocked => false,
        }
    }
    /// Commit a routed cell plus its clearance halo to `net`.
    fn commit(&mut self, c: usize, r: usize, layer: usize, net: usize, halo: isize) {
        self.set(c, r, layer, Cell::Owner(net));
        self.halo(c, r, layer, net, halo, halo);
    }
    /// Reserve a via's body + clearance at `(c, r)` on both layers for `net`.
    fn commit_via(&mut self, c: usize, r: usize, net: usize, via_halo: isize) {
        for layer in [FRONT, BACK] {
            self.set(c, r, layer, Cell::Owner(net));
            self.halo(c, r, layer, net, via_halo, via_halo);
        }
    }
    /// Is a via's whole body (`±via_halo` on both layers) free for `net`? Also
    /// false if the via would sit too near the grid edge for its clearance.
    fn via_area_clear(&self, c: usize, r: usize, net: usize, via_halo: isize) -> bool {
        for dr in -via_halo..=via_halo {
            for dc in -via_halo..=via_halo {
                let (nc, nr) = (c as isize + dc, r as isize + dr);
                if nc < 0 || nr < 0 || nc >= self.cols as isize || nr >= self.rows as isize {
                    return false;
                }
                let (nc, nr) = (nc as usize, nr as usize);
                if !self.passable(nc, nr, FRONT, net) || !self.passable(nc, nr, BACK, net) {
                    return false;
                }
            }
        }
        true
    }

    /// Dijkstra from any `sources` cell to the `target` cell for `net`. Moves are
    /// 4-connected on a layer (cost `step`) or a via to the other layer (cost
    /// `via_cost`). Returns the path as `(c, r, layer)` cells, or `None`.
    fn route_one(
        &self,
        net: usize,
        sources: &[(usize, usize, usize)],
        target: (usize, usize, usize),
        step: i64,
        via_cost: i64,
        via_halo: isize,
    ) -> Option<Vec<(usize, usize, usize)>> {
        let n = self.cols * self.rows * 2;
        let mut dist = vec![i64::MAX; n];
        let mut prev = vec![usize::MAX; n];
        let mut heap = BinaryHeap::new();
        for &(c, r, l) in sources {
            let i = self.idx(c, r, l);
            if dist[i] != 0 {
                dist[i] = 0;
                heap.push(Reverse((0i64, i)));
            }
        }
        let tgt = self.idx(target.0, target.1, target.2);
        while let Some(Reverse((d, i))) = heap.pop() {
            if d > dist[i] {
                continue;
            }
            if i == tgt {
                // Reconstruct.
                let mut path = Vec::new();
                let mut cur = i;
                while cur != usize::MAX {
                    let layer = cur / (self.cols * self.rows);
                    let rem = cur % (self.cols * self.rows);
                    path.push((rem % self.cols, rem / self.cols, layer));
                    cur = prev[cur];
                }
                path.reverse();
                return Some(path);
            }
            let layer = i / (self.cols * self.rows);
            let rem = i % (self.cols * self.rows);
            let (c, r) = (rem % self.cols, rem / self.cols);
            // 4-connected neighbours on this layer.
            let neigh = [(-1isize, 0isize), (1, 0), (0, -1), (0, 1)];
            for (dc, dr) in neigh {
                let (nc, nr) = (c as isize + dc, r as isize + dr);
                if nc < 0 || nr < 0 || nc >= self.cols as isize || nr >= self.rows as isize {
                    continue;
                }
                let (nc, nr) = (nc as usize, nr as usize);
                if !self.passable(nc, nr, layer, net) {
                    continue;
                }
                relax(
                    self.idx(nc, nr, layer),
                    d + step,
                    i,
                    &mut dist,
                    &mut prev,
                    &mut heap,
                );
            }
            // Via to the other layer (same cell) — a via is bigger than a track,
            // so its whole body must clear other nets on both layers.
            let other = layer ^ 1;
            if self.via_area_clear(c, r, net, via_halo) {
                relax(
                    self.idx(c, r, other),
                    d + via_cost,
                    i,
                    &mut dist,
                    &mut prev,
                    &mut heap,
                );
            }
        }
        None
    }
}

fn relax(
    j: usize,
    nd: i64,
    from: usize,
    dist: &mut [i64],
    prev: &mut [usize],
    heap: &mut BinaryHeap<Reverse<(i64, usize)>>,
) {
    if nd < dist[j] {
        dist[j] = nd;
        prev[j] = from;
        heap.push(Reverse((nd, j)));
    }
}

/// Turn a routed cell path into merged track segments (one per straight run,
/// per layer) plus a via at each layer change. The path moves one cell at a time
/// on a layer, or switches layer in place (a via at that cell).
fn emit_path(
    path: &[(usize, usize, usize)],
    net_idx: usize,
    opts: &RouteOptions,
    mm_of: &impl Fn(usize, usize) -> (f64, f64),
    out: &mut RouteOutput,
) {
    if path.len() < 2 {
        return;
    }
    let layer_name = |l: usize| {
        if l == FRONT {
            opts.front.clone()
        } else {
            opts.back.clone()
        }
    };
    // Split into maximal same-layer runs; a via bridges consecutive runs (the two
    // runs share the transition cell).
    let mut i = 0;
    while i < path.len() {
        let layer = path[i].2;
        let mut j = i;
        while j + 1 < path.len() && path[j + 1].2 == layer {
            j += 1;
        }
        emit_straight_runs(path, i, j, net_idx, opts, &layer_name, mm_of, out);
        if j + 1 < path.len() {
            let (vc, vr, _) = path[j];
            out.vias.push(Via {
                at: mm_of(vc, vr),
                size_mm: opts.via_size_mm,
                drill_mm: opts.via_drill_mm,
                net_idx,
            });
        }
        i = j + 1;
    }
}

/// Split the same-layer sub-path `path[i..=j]` into straight tracks at each turn.
#[allow(clippy::too_many_arguments)]
fn emit_straight_runs(
    path: &[(usize, usize, usize)],
    i: usize,
    j: usize,
    net_idx: usize,
    opts: &RouteOptions,
    layer_name: &impl Fn(usize) -> String,
    mm_of: &impl Fn(usize, usize) -> (f64, f64),
    out: &mut RouteOutput,
) {
    if j <= i {
        return;
    }
    let dir = |a: usize, b: usize| {
        (
            (path[b].0 as isize - path[a].0 as isize).signum(),
            (path[b].1 as isize - path[a].1 as isize).signum(),
        )
    };
    let mut seg_start = i;
    let mut cur = dir(i, i + 1);
    for k in (i + 1)..j {
        let d = dir(k, k + 1);
        if d != cur {
            push_track(path, seg_start, k, net_idx, opts, layer_name, mm_of, out);
            seg_start = k;
            cur = d;
        }
    }
    push_track(path, seg_start, j, net_idx, opts, layer_name, mm_of, out);
}

/// Emit one straight track for `path[a..=b]` (same layer), skipping empties.
#[allow(clippy::too_many_arguments)]
fn push_track(
    path: &[(usize, usize, usize)],
    a: usize,
    b: usize,
    net_idx: usize,
    opts: &RouteOptions,
    layer_name: &impl Fn(usize) -> String,
    mm_of: &impl Fn(usize, usize) -> (f64, f64),
    out: &mut RouteOutput,
) {
    if b <= a {
        return;
    }
    let (ac, ar, layer) = path[a];
    let (bc, br, _) = path[b];
    if (ac, ar) == (bc, br) {
        return;
    }
    out.tracks.push(Track {
        start: mm_of(ac, ar),
        end: mm_of(bc, br),
        width_mm: opts.signal_width_mm,
        layer: layer_name(layer),
        net_idx,
    });
}

/// Pad core cells: grid cells whose centre lies within the pad rectangle around
/// centre cell `(cc, cr)`.
fn pad_core_cells(
    cc: usize,
    cr: usize,
    w_mm: f64,
    h_mm: f64,
    res: f64,
    cols: usize,
    rows: usize,
) -> Vec<(usize, usize)> {
    let hw = (w_mm / 2.0 / res).floor() as isize;
    let hh = (h_mm / 2.0 / res).floor() as isize;
    let mut cells = Vec::new();
    for dr in -hh.max(0)..=hh.max(0) {
        for dc in -hw.max(0)..=hw.max(0) {
            let (c, r) = (cc as isize + dc, cr as isize + dr);
            if c >= 0 && r >= 0 && c < cols as isize && r < rows as isize {
                cells.push((c as usize, r as usize));
            }
        }
    }
    if cells.is_empty() {
        cells.push((cc, cr));
    }
    cells
}

/// Bounding box of all pads, expanded by `margin` (mm).
fn bounds_of(nets: &[RouteNet], margin: f64) -> (f64, f64, f64, f64) {
    let mut b = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for net in nets {
        for p in &net.pads {
            b.0 = b.0.min(p.x_mm - p.w_mm / 2.0);
            b.1 = b.1.min(p.y_mm - p.h_mm / 2.0);
            b.2 = b.2.max(p.x_mm + p.w_mm / 2.0);
            b.3 = b.3.max(p.y_mm + p.h_mm / 2.0);
        }
    }
    if !b.0.is_finite() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    (b.0 - margin, b.1 - margin, b.2 + margin, b.3 + margin)
}

// ---- serialization --------------------------------------------------------

/// Serialize a track to a KiCad `(segment …)`.
pub(crate) fn track_sexpr(t: &Track) -> Sexpr {
    let pt = |k: &str, (x, y): (f64, f64)| {
        Sexpr::list(vec![Sexpr::sym(k), Sexpr::sym(mm(x)), Sexpr::sym(mm(y))])
    };
    Sexpr::list(vec![
        Sexpr::sym("segment"),
        pt("start", t.start),
        pt("end", t.end),
        Sexpr::list(vec![Sexpr::sym("width"), Sexpr::sym(mm(t.width_mm))]),
        Sexpr::list(vec![Sexpr::sym("layer"), Sexpr::string(&t.layer)]),
        Sexpr::list(vec![Sexpr::sym("net"), Sexpr::sym(t.net_idx.to_string())]),
        Sexpr::list(vec![
            Sexpr::sym("uuid"),
            Sexpr::string(det_uuid(&format!(
                "seg:{}:{:?}:{:?}:{}",
                t.net_idx, t.start, t.end, t.layer
            ))),
        ]),
    ])
}

/// Serialize a via to a KiCad `(via …)`.
pub(crate) fn via_sexpr(v: &Via, front: &str, back: &str) -> Sexpr {
    Sexpr::list(vec![
        Sexpr::sym("via"),
        Sexpr::list(vec![
            Sexpr::sym("at"),
            Sexpr::sym(mm(v.at.0)),
            Sexpr::sym(mm(v.at.1)),
        ]),
        Sexpr::list(vec![Sexpr::sym("size"), Sexpr::sym(mm(v.size_mm))]),
        Sexpr::list(vec![Sexpr::sym("drill"), Sexpr::sym(mm(v.drill_mm))]),
        Sexpr::list(vec![
            Sexpr::sym("layers"),
            Sexpr::string(front),
            Sexpr::string(back),
        ]),
        Sexpr::list(vec![Sexpr::sym("net"), Sexpr::sym(v.net_idx.to_string())]),
        Sexpr::list(vec![
            Sexpr::sym("uuid"),
            Sexpr::string(det_uuid(&format!("via:{}:{:?}", v.net_idx, v.at))),
        ]),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(refdes: &str, num: &str, x: f64, y: f64) -> PadPoint {
        PadPoint {
            refdes: refdes.into(),
            pad: num.into(),
            x_mm: x,
            y_mm: y,
            w_mm: 1.0,
            h_mm: 1.0,
            layer: PadLayer::Front,
        }
    }

    #[test]
    fn mst_two_pad_net_makes_one_track() {
        let net = RouteNet {
            net_idx: 3,
            name: "OUT".into(),
            pads: vec![pad("R1", "2", 100.0, 100.0), pad("C1", "1", 104.0, 100.0)],
        };
        let out = MstRouter.route(&[net], &RouteOptions::default());
        assert_eq!(out.tracks.len(), 1);
        assert_eq!(out.tracks[0].layer, "F.Cu");
    }

    #[test]
    fn mst_n_pads_n_minus_one_tracks() {
        let net = RouteNet {
            net_idx: 2,
            name: "N".into(),
            pads: (0..4).map(|i| pad("P", "1", i as f64, 0.0)).collect(),
        };
        assert_eq!(
            MstRouter
                .route(&[net], &RouteOptions::default())
                .tracks
                .len(),
            3
        );
    }

    #[test]
    fn grid_routes_two_pad_net() {
        let net = RouteNet {
            net_idx: 3,
            name: "OUT".into(),
            pads: vec![pad("R1", "2", 100.0, 100.0), pad("C1", "1", 104.0, 100.0)],
        };
        let out = GridRouter.route(&[net], &RouteOptions::default());
        assert!(!out.tracks.is_empty(), "should route the net");
        assert!(
            out.conflicts.is_empty(),
            "no conflicts: {:?}",
            out.conflicts
        );
        assert_eq!(out.tracks[0].net_idx, 3);
    }

    #[test]
    fn grid_routes_around_an_obstructing_pad() {
        // OUT connects the two outer pads; GND and IN pads sit on the straight
        // line between them. A correct router must detour around them.
        let nets = vec![
            RouteNet {
                net_idx: 1,
                name: "GND".into(),
                pads: vec![pad("C1", "2", 101.0, 100.0)],
            },
            RouteNet {
                net_idx: 2,
                name: "IN".into(),
                pads: vec![pad("R1", "1", 104.0, 100.0)],
            },
            RouteNet {
                net_idx: 3,
                name: "OUT".into(),
                pads: vec![pad("C1", "1", 99.0, 100.0), pad("R1", "2", 106.0, 100.0)],
            },
        ];
        let out = GridRouter.route(&nets, &RouteOptions::default());
        assert!(
            out.conflicts.is_empty(),
            "must route around: {:?}",
            out.conflicts
        );
        assert!(!out.tracks.is_empty());
        // The straight line at y=100 is blocked, so the route must leave y=100.
        let detours = out
            .tracks
            .iter()
            .any(|t| t.start.1 != 100.0 || t.end.1 != 100.0);
        assert!(detours, "route should detour off the pad row");
    }

    #[test]
    fn single_pad_net_routes_nothing() {
        let net = RouteNet {
            net_idx: 1,
            name: "IN".into(),
            pads: vec![pad("R1", "1", 100.0, 100.0)],
        };
        assert!(GridRouter
            .route(&[net], &RouteOptions::default())
            .tracks
            .is_empty());
    }

    #[test]
    fn track_and_via_serialize() {
        let t = Track {
            start: (100.0, 100.0),
            end: (104.0, 100.5),
            width_mm: 0.25,
            layer: "F.Cu".into(),
            net_idx: 3,
        };
        let s = track_sexpr(&t).to_sexpr_string();
        assert!(s.contains("(segment") && s.contains(r#"(layer "F.Cu")"#) && s.contains("(net 3)"));
        let v = Via {
            at: (101.0, 100.0),
            size_mm: 0.8,
            drill_mm: 0.4,
            net_idx: 3,
        };
        assert!(via_sexpr(&v, "F.Cu", "B.Cu")
            .to_sexpr_string()
            .contains(r#"(layers "F.Cu" "B.Cu")"#));
    }
}
