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
use std::collections::{BinaryHeap, HashMap, HashSet};

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
    /// The copper layers this pad connects to. A through-hole pad ([`Both`]) is on
    /// both, so it bridges them for free — no via needed to change layers there.
    ///
    /// [`Both`]: PadLayer::Both
    fn layers(self) -> &'static [usize] {
        match self {
            PadLayer::Front => &[FRONT],
            PadLayer::Back => &[BACK],
            PadLayer::Both => &[FRONT, BACK],
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
    /// Minimum copper-to-board-edge clearance (mm). When `bounds` is the board
    /// outline, the router keeps every track and via this far inside it. Matches
    /// KiCad's default `copper_edge_clearance`; without it a net routed to the edge
    /// of a tight (narrow-HP) board trips `copper_edge_clearance` DRC.
    pub edge_clearance_mm: f64,
    /// Routing-grid cell size (mm). Finer = more precise, slower.
    pub grid_mm: f64,
    /// Cost (mm-equivalent) charged for a layer change, to discourage vias.
    pub via_cost_mm: f64,
    /// Extra cost (mm-equivalent) per grid step routed on the **back** layer, so
    /// signals prefer the front and the back stays a mostly-intact ground plane
    /// (short crossings still dip to the back; long runs get pushed to the front).
    pub back_penalty_mm: f64,
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
            edge_clearance_mm: 0.5,
            grid_mm: 0.2,
            via_cost_mm: 2.0,
            // Small per-cell surcharge on the back layer: it accumulates so a long
            // back run loses to a front detour (keeping the ground plane intact),
            // but stays under one via's cost so a short same-layer hop still beats
            // crossing over. Crossover vs a via is ~via_cost/penalty ≈ 20 cells.
            back_penalty_mm: 0.1,
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

/// Per-move maze-search costs (mm × 1000, integer for a total ordering): one grid
/// step, a layer change (via), and the surcharge for a step on the back layer.
#[derive(Clone, Copy)]
struct Costs {
    step: i64,
    via: i64,
    back: i64,
}

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

/// Max rip-up-and-reroute iterations. Each adds an ordering constraint (a boxed-in
/// net must route before whatever boxed it in), so the constraint set only grows —
/// convergence is bounded and a handful resolves the common mutual-conflict cases.
const RIPUP_MAX_ITERS: usize = 16;

impl Router for GridRouter {
    /// Route all nets with **rip-up-and-reroute**: route in an order; any net that
    /// can't reach a pad reports which committed nets boxed it in (blame); those
    /// blockers are then forced to route *after* it (ripped up and rerouted around
    /// it) on the next iteration. Ordering constraints only accumulate (bounded),
    /// unsatisfiable cycles are dropped, and the fewest-conflict attempt wins.
    fn route(&self, nets: &[RouteNet], opts: &RouteOptions) -> RouteOutput {
        let routable: Vec<&RouteNet> = nets.iter().filter(|n| n.pads.len() >= 2).collect();
        if routable.is_empty() {
            return RouteOutput::default();
        }
        // Base order: deterministic by net index. `net_idx → routable index` maps a
        // blocker (known by net index) back to an orderable net.
        let mut base: Vec<usize> = (0..routable.len()).collect();
        base.sort_by_key(|&i| routable[i].net_idx);
        let net_to_rt: HashMap<usize, usize> = routable
            .iter()
            .enumerate()
            .map(|(i, n)| (n.net_idx, i))
            .collect();

        // Accumulated "a must route before b" constraints (routable indices).
        let mut before: Vec<(usize, usize)> = Vec::new();
        let mut best: Option<RouteOutput> = None;
        for _ in 0..RIPUP_MAX_ITERS {
            let order = topo_order(&base, &before);
            let (out, failed, blame) = self.route_pass(nets, &routable, &order, &net_to_rt, opts);
            if failed.is_empty() {
                return out;
            }
            if best
                .as_ref()
                .is_none_or(|b| out.conflicts.len() < b.conflicts.len())
            {
                best = Some(out);
            }
            // Rip up: each boxed-in net must precede the nets that boxed it in.
            let mut added = false;
            for (&victim, blockers) in &blame {
                for &b in blockers {
                    if victim != b
                        && !before.contains(&(victim, b))
                        && !before.contains(&(b, victim))
                    {
                        before.push((victim, b));
                        added = true;
                    }
                }
            }
            if !added {
                break; // no new constraint to try — take the best attempt
            }
        }
        best.unwrap_or_default()
    }
}

/// Order `base` (a permutation of routable indices) so every `(a, b)` in `before`
/// has `a` ahead of `b`, using the base order as the tie-break. A constraint that
/// would form a cycle is dropped (those nets can't both win; one stays unrouted).
fn topo_order(base: &[usize], before: &[(usize, usize)]) -> Vec<usize> {
    let n = base.len();
    let rank: HashMap<usize, usize> = base.iter().enumerate().map(|(p, &i)| (i, p)).collect();
    let mut succ: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut indeg: HashMap<usize, usize> = base.iter().map(|&i| (i, 0)).collect();
    for &(a, b) in before {
        succ.entry(a).or_default().push(b);
        *indeg.get_mut(&b).unwrap() += 1;
    }
    // Kahn's algorithm, always taking the ready node with the lowest base rank.
    let mut ready: Vec<usize> = indeg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&i, _)| i)
        .collect();
    let mut out = Vec::with_capacity(n);
    let mut done: HashSet<usize> = HashSet::new();
    while !ready.is_empty() {
        ready.sort_by_key(|i| rank[i]);
        let i = ready.remove(0);
        out.push(i);
        done.insert(i);
        if let Some(ss) = succ.get(&i) {
            for &s in ss {
                let d = indeg.get_mut(&s).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push(s);
                }
            }
        }
    }
    // Any nodes left are in a constraint cycle — append them in base order.
    for &i in base {
        if !done.contains(&i) {
            out.push(i);
        }
    }
    out
}

impl GridRouter {
    /// One routing pass over `routable` in the given `order` (indices into
    /// `routable`). Returns the routed output and the indices of nets that did
    /// not fully route this pass.
    #[allow(clippy::type_complexity)]
    fn route_pass(
        &self,
        nets: &[RouteNet],
        routable: &[&RouteNet],
        order: &[usize],
        net_to_rt: &HashMap<usize, usize>,
        opts: &RouteOptions,
    ) -> (RouteOutput, Vec<usize>, HashMap<usize, HashSet<usize>>) {
        let mut out = RouteOutput::default();
        let res = opts.grid_mm.max(0.01);
        // `bounds`, when given, is the board outline: inset the routable area by the
        // edge clearance plus the widest copper half (a via) so no track/via lands
        // within `edge_clearance_mm` of the edge. The pad-bbox fallback has no board
        // edge, so it is used as-is.
        let (minx, miny, maxx, maxy) = match opts.bounds {
            Some((x0, y0, x1, y1)) => {
                let inset = opts.edge_clearance_mm
                    + (opts.via_size_mm / 2.0).max(opts.signal_width_mm / 2.0)
                    + res / 2.0;
                if x1 - x0 > 2.0 * inset && y1 - y0 > 2.0 * inset {
                    (x0 + inset, y0 + inset, x1 - inset, y1 - inset)
                } else {
                    (x0, y0, x1, y1) // too small to inset — leave it (surfaces as conflicts)
                }
            }
            None => bounds_of(nets, 2.0),
        };
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
        // Per routable net: each pad's cell + which layers it connects.
        let mut pad_cells: Vec<Vec<((usize, usize), PadLayer)>> = Vec::new();
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

        // Record each routable net's pad cells (for source/target sets). A pad is
        // a connection point on every layer it touches — so a through-hole pad is
        // reachable on both, letting the router meet it without a via.
        for net in routable {
            let mut cells = Vec::new();
            for pad in &net.pads {
                let (c, r) = cell_of(pad.x_mm, pad.y_mm);
                for &layer in pad.layer.layers() {
                    grid.set(c, r, layer, Cell::Owner(net.net_idx));
                }
                cells.push(((c, r), pad.layer));
            }
            pad_cells.push(cells);
        }

        // Route each net in the given order, growing a tree from pad 0.
        let mut failed: Vec<usize> = Vec::new();
        // Blame: victim routable-idx → the routable nets whose committed copper
        // boxes in one of its pads (candidates to rip up and reroute).
        let mut blame: HashMap<usize, HashSet<usize>> = HashMap::new();
        // How far around an unreachable pad to look for the nets fencing it in.
        let blame_radius = (halo * 3).max(6);
        for &ni in order {
            let net = routable[ni];
            let costs = Costs {
                step: (res * 1000.0) as i64,
                via: (opts.via_cost_mm * 1000.0) as i64,
                back: (opts.back_penalty_mm * 1000.0) as i64,
            };

            // Connected component: cells already part of this net's routed tree.
            // A through-hole pad seeds both layers (it bridges them).
            let mut connected: Vec<(usize, usize, usize)> = Vec::new(); // (c,r,layer)
            let ((p0c, p0r), p0layer) = pad_cells[ni][0];
            for &l in p0layer.layers() {
                connected.push((p0c, p0r, l));
            }

            let mut pending: Vec<usize> = (1..net.pads.len()).collect();
            while let Some(k) = pending.pop() {
                let ((tc, tr), tlayer) = pad_cells[ni][k];
                // Reach the pad on any layer it touches (cheapest wins).
                let targets: Vec<(usize, usize, usize)> =
                    tlayer.layers().iter().map(|&l| (tc, tr, l)).collect();
                match grid.route_one(net.net_idx, &connected, &targets, costs, via_halo) {
                    Some(path) => {
                        emit_path(&path, net.net_idx, opts, &mm_of, &mut out);
                        for &(c, r, l) in &path {
                            grid.commit(c, r, l, net.net_idx, halo);
                            if !connected.contains(&(c, r, l)) {
                                connected.push((c, r, l));
                            }
                        }
                        // A through-hole target bridges both layers into the tree.
                        for &l in tlayer.layers() {
                            if !connected.contains(&(tc, tr, l)) {
                                connected.push((tc, tr, l));
                            }
                        }
                        // Reserve each via's wider body so later nets keep clear.
                        for w in path.windows(2) {
                            if w[0].2 != w[1].2 {
                                grid.commit_via(w[0].0, w[0].1, net.net_idx, via_halo);
                            }
                        }
                    }
                    None => {
                        out.conflicts.push(format!(
                            "net {} ({}): could not route to pad {}.{}",
                            net.net_idx, net.name, net.pads[k].refdes, net.pads[k].pad
                        ));
                        if !failed.contains(&ni) {
                            failed.push(ni);
                        }
                        // Blame the routable nets whose copper boxes in this pad.
                        for &l in tlayer.layers() {
                            for dr in -blame_radius..=blame_radius {
                                for dc in -blame_radius..=blame_radius {
                                    let (c, r) = (tc as isize + dc, tr as isize + dr);
                                    if c < 0 || r < 0 || c >= cols as isize || r >= rows as isize {
                                        continue;
                                    }
                                    if let Cell::Owner(m) = grid.get(c as usize, r as usize, l) {
                                        if m != net.net_idx {
                                            if let Some(&rt) = net_to_rt.get(&m) {
                                                blame.entry(ni).or_default().insert(rt);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        (out, failed, blame)
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
        targets: &[(usize, usize, usize)],
        costs: Costs,
        via_halo: isize,
    ) -> Option<Vec<(usize, usize, usize)>> {
        let Costs {
            step,
            via: via_cost,
            back: back_penalty,
        } = costs;
        let n = self.cols * self.rows * 2;
        let cr = self.cols * self.rows;
        let mut dist = vec![i64::MAX; n];
        let mut prev = vec![usize::MAX; n];
        // A* heuristic: cost-scaled Manhattan distance to the nearest target.
        // Admissible + consistent on this 4-connected grid, so the least-cost path
        // is still found — but the search heads for the target instead of flooding
        // the whole board (routing a wide board in ms instead of not finishing).
        let heuristic = |i: usize| -> i64 {
            let rem = i % cr;
            let (c, r) = ((rem % self.cols) as isize, (rem / self.cols) as isize);
            targets
                .iter()
                .map(|&(tc, tr, _)| (c - tc as isize).abs() + (r - tr as isize).abs())
                .min()
                .unwrap_or(0) as i64
                * step
        };
        // Heap of (f = g + h, g, cell); `dist` tracks g (the actual cost so far).
        let mut heap: BinaryHeap<Reverse<(i64, i64, usize)>> = BinaryHeap::new();
        for &(c, r, l) in sources {
            let i = self.idx(c, r, l);
            if dist[i] != 0 {
                dist[i] = 0;
                heap.push(Reverse((heuristic(i), 0i64, i)));
            }
        }
        let tgt: Vec<usize> = targets.iter().map(|&(c, r, l)| self.idx(c, r, l)).collect();
        while let Some(Reverse((_f, g, i))) = heap.pop() {
            if g > dist[i] {
                continue;
            }
            if tgt.contains(&i) {
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
                // Bias signals to the front: routing on the back (pour layer)
                // costs extra, so the back stays a mostly-intact ground plane.
                let move_cost = step + if layer == BACK { back_penalty } else { 0 };
                let j = self.idx(nc, nr, layer);
                relax(
                    j,
                    g + move_cost,
                    heuristic(j),
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
                let j = self.idx(c, r, other);
                relax(
                    j,
                    g + via_cost,
                    heuristic(j),
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
    hj: i64,
    from: usize,
    dist: &mut [i64],
    prev: &mut [usize],
    heap: &mut BinaryHeap<Reverse<(i64, i64, usize)>>,
) {
    // `nd` is the actual cost g to reach j; the heap is ordered by f = g + h.
    if nd < dist[j] {
        dist[j] = nd;
        prev[j] = from;
        heap.push(Reverse((nd + hj, nd, j)));
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

    fn pad_on(refdes: &str, num: &str, x: f64, y: f64, layer: PadLayer) -> PadPoint {
        PadPoint {
            layer,
            ..pad(refdes, num, x, y)
        }
    }

    #[test]
    fn through_hole_pad_reached_without_a_via() {
        // A back SMD pad and a through-hole pad on the same net. The through-hole
        // pad is on both layers, so the router should meet it on the back — no via.
        let net = RouteNet {
            net_idx: 1,
            name: "N".into(),
            pads: vec![
                pad_on("R1", "2", 100.0, 100.0, PadLayer::Back),
                pad_on("J1", "1", 103.0, 100.0, PadLayer::Both),
            ],
        };
        let out = GridRouter.route(&[net], &RouteOptions::default());
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
        assert!(!out.tracks.is_empty());
        assert!(
            out.vias.is_empty(),
            "through-hole pad bridges layers — no via needed"
        );
        assert!(
            out.tracks.iter().all(|t| t.layer == "B.Cu"),
            "route stays on the back where both pads are reachable"
        );
    }

    #[test]
    fn no_net_pad_forces_traces_to_route_around_it() {
        // board.rs seeds each no-net pad (unused IC pin, jack switch contact, …)
        // as a single-pad net so it's an obstacle, not a connection. Prove it: a
        // signal net whose straight path crosses a through-hole no-net pad must
        // detour around it (fixes the shorting_items class from the slew limiter).
        let opts = RouteOptions::default();
        let sig = || RouteNet {
            net_idx: 1,
            name: "SIG".into(),
            pads: vec![pad("U1", "1", 100.0, 100.0), pad("U2", "1", 108.0, 100.0)],
        };
        // Baseline (no obstacle): the router runs straight along y = 100.
        let base = GridRouter.route(&[sig()], &opts);
        assert!(base.conflicts.is_empty() && !base.tracks.is_empty());
        assert!(
            base.tracks
                .iter()
                .all(|t| (t.start.1 - 100.0).abs() < 0.11 && (t.end.1 - 100.0).abs() < 0.11),
            "baseline route is a straight line"
        );
        // A lone through-hole no-net pad on that line (blocks both layers).
        let obstacle = RouteNet {
            net_idx: 0,
            name: String::new(),
            pads: vec![pad_on("U3", "2", 104.0, 100.0, PadLayer::Both)],
        };
        let out = GridRouter.route(&[sig(), obstacle], &opts);
        assert!(!out.tracks.is_empty(), "signal still routes");
        assert!(
            out.tracks
                .iter()
                .any(|t| (t.start.1 - 100.0).abs() > 0.3 || (t.end.1 - 100.0).abs() > 0.3),
            "trace must route around the no-net pad, not through it"
        );
        // The lone obstacle pad is never itself connected.
        assert!(
            out.tracks.iter().all(|t| t.net_idx != 0),
            "a no-net obstacle pad is not routed"
        );
    }

    #[test]
    fn signals_prefer_the_front_layer() {
        // Two through-hole pads reachable on either layer. The back-layer surcharge
        // keeps the run on the front, leaving the back as an intact ground plane
        // (fixes the pour-fragmentation / starved_thermal class from the slew limiter).
        let net = RouteNet {
            net_idx: 1,
            name: "N".into(),
            pads: vec![
                pad_on("U1", "1", 100.0, 100.0, PadLayer::Both),
                pad_on("U2", "1", 106.0, 100.0, PadLayer::Both),
            ],
        };
        let out = GridRouter.route(&[net], &RouteOptions::default());
        assert!(out.conflicts.is_empty() && !out.tracks.is_empty());
        assert!(
            out.tracks.iter().all(|t| t.layer == "F.Cu"),
            "front-biased routing keeps signals off the back ground plane"
        );
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

    #[test]
    fn topo_order_respects_ripup_constraints_and_drops_cycles() {
        // The rip-up loop feeds `topo_order` accumulated "a must route before b"
        // constraints (a boxed-in victim must precede whatever boxed it in). This
        // pins down that ordering logic directly, since the end-to-end boxed-in
        // case only reproduces under real board congestion.

        // No constraints: the base (net-index) order is preserved verbatim.
        assert_eq!(topo_order(&[0, 1, 2], &[]), vec![0, 1, 2]);

        // A single "1 before 0" constraint (net 1 was boxed in by net 0) puts the
        // victim first so net 0 reroutes around it.
        assert_eq!(topo_order(&[0, 1], &[(1, 0)]), vec![1, 0]);

        // Ties break by base rank, so the order stays as close to the base as the
        // constraints allow (deterministic, minimal churn): "3 before 1" moves 1
        // only, leaving 0 and 2 where they were.
        assert_eq!(topo_order(&[0, 1, 2, 3], &[(3, 1)]), vec![0, 2, 3, 1]);

        // A contradictory pair (0<1 and 1<0) is unsatisfiable — the cycle is
        // dropped and the tangled nets fall back to base order (one loses and
        // stays unrouted, rather than the loop spinning forever).
        assert_eq!(topo_order(&[0, 1], &[(0, 1), (1, 0)]), vec![0, 1]);

        // Even with a cycle, every input index appears exactly once in the output.
        let mut got = topo_order(&[0, 1, 2, 3], &[(2, 1), (1, 2)]);
        got.sort();
        assert_eq!(got, vec![0, 1, 2, 3]);
    }

    #[test]
    fn routing_refuses_to_enter_the_board_edge_band() {
        // When `bounds` is the board outline, the router keeps copper an
        // edge-clearance inset inside it. A net whose only path runs through the
        // edge band (here: a wall sealing the board except a gap hard against the
        // bottom edge) must NOT be routed into the band — that trips
        // copper_edge_clearance DRC. It is surfaced as a conflict instead.
        let opts = RouteOptions {
            bounds: Some((0.0, 0.0, 20.0, 10.0)),
            ..RouteOptions::default()
        };
        let mut nets = vec![RouteNet {
            net_idx: 1,
            name: "SIG".into(),
            pads: vec![pad("A", "1", 5.0, 5.0), pad("B", "1", 15.0, 5.0)],
        }];
        // A solid vertical wall of through-hole obstacle pads at x = 10, spanning
        // y = 1..=10 (the top edge). The only crossing is y < 1 — inside the band.
        for y in 1..=10 {
            nets.push(RouteNet {
                net_idx: 0,
                name: String::new(),
                pads: vec![pad_on("W", "1", 10.0, y as f64, PadLayer::Both)],
            });
        }
        let out = GridRouter.route(&nets, &opts);
        assert!(
            !out.conflicts.is_empty(),
            "a net routable only through the edge band must be surfaced, not routed into it"
        );
        assert!(
            out.tracks.iter().all(|t| t.net_idx != 1),
            "the boxed-in net must not be routed into the edge band"
        );
    }
}
