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
    /// The net poured as a plane on both layers (ground), if any. Its pads are
    /// not joined to each other: each single-layer pad gets a short stub to a
    /// via onto the other layer's pour, and the zone fill makes every other
    /// connection — so a board's fifty ground pads cost fifty tiny searches,
    /// not a spanning tree across the whole board (legion-of-bom-y17.12).
    pub plane_net: Option<usize>,
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
            // A via is worth about this much detour. 2.0mm was far too cheap:
            // at 0.2mm grid it bought a layer change for ten steps, so the
            // router hopped layers rather than looking for a way round. Swept
            // against the slew limiter, 10mm halves the vias (41 -> 21) AND
            // shortens total copper 12% (736 -> 647mm) — the lazy layer-hops
            // were the long routes too. Past ~16mm it starts buying via
            // avoidance with detours that cost more than the via did.
            via_cost_mm: 10.0,
            // Small per-cell surcharge on the back layer: it accumulates so a long
            // back run loses to a front detour (keeping the ground plane intact),
            // but stays under one via's cost so a short same-layer hop still beats
            // crossing over. Crossover vs a via is ~via_cost/penalty ≈ 20 cells.
            back_penalty_mm: 0.1,
            bounds: None,
            plane_net: None,
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

/// SQRT(2) as a fraction, for diagonal step cost on the 8-connected grid. Integer
/// arithmetic keeps the A* costs exact, so ties break the same way every run and
/// the same board comes out twice.
const DIAG_NUM: i64 = 1414;
const DENOM: i64 = 1000;

/// Least slack (cells) a windowed search keeps around a connection's endpoints
/// — room to detour round an obstacle between them. A quarter of the
/// connection's own span is added when that is more.
const SEARCH_MARGIN_CELLS: usize = 50;

/// How far (mm) a plane-net pad looks for a spot to drop its via — far enough
/// to escape a fine-pitch row, near enough that a pad with no legal spot is
/// reported instead of being routed across the board to one.
const PLANE_REACH_MM: f64 = 5.0;

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
        // Base order: deterministic by net index.
        let mut base: Vec<usize> = (0..routable.len()).collect();
        base.sort_by_key(|&i| routable[i].net_idx);
        let neutral = Congestion::neutral();
        search_orderings(nets, &routable, base, opts, &neutral)
    }
}

/// Search net **orderings** for one that routes everything, committing copper
/// hard as it goes.
///
/// Route in `base` order; any net that can't reach a pad reports which committed
/// nets boxed it in (blame), and those blockers are forced to route *after* it on
/// the next attempt. Constraints only accumulate (so this is bounded),
/// unsatisfiable cycles are dropped, and the fewest-conflict attempt wins.
///
/// Shared by both grid routers: [`GridRouter`] starts from net order with no
/// bias, [`PathfinderRouter`] starts from a hardest-first order and biases the
/// search with what its negotiation learned.
fn search_orderings(
    nets: &[RouteNet],
    routable: &[&RouteNet],
    base: Vec<usize>,
    opts: &RouteOptions,
    learned: &Congestion,
) -> RouteOutput {
    {
        // `net_idx → routable index` maps a blocker (known by net index) back to
        // an orderable net.
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
            let (out, failed, blame) =
                route_pass(nets, routable, &order, &net_to_rt, opts, learned);
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
            //
            // Sorted, because `blame` is a hash map of hash sets and this loop is
            // order-sensitive: a constraint is skipped when its reverse is already
            // present, so visiting the same blame in a different order accumulates
            // a *different* constraint set and routes a different board. Rust
            // reseeds hash iteration per process, so that made the pipeline
            // nondeterministic across runs — on a 13-part demo board, 311.5 or
            // 412.4 depending on the run — for any board dense enough to need
            // rip-up at all.
            let blamed: Vec<(usize, Vec<usize>)> = {
                let mut v: Vec<(usize, Vec<usize>)> = blame
                    .iter()
                    .map(|(&victim, blockers)| {
                        let mut b: Vec<usize> = blockers.iter().copied().collect();
                        b.sort_unstable();
                        (victim, b)
                    })
                    .collect();
                v.sort_by_key(|(victim, _)| *victim);
                v
            };
            let mut added = false;
            for (victim, blockers) in &blamed {
                let victim = *victim;
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

/// The routing surface both grid routers work on: the discretised board, the
/// pad territory that is **never** negotiable, and the derived clearance radii.
///
/// Split out because [`GridRouter`] and [`PathfinderRouter`] must discretise the
/// board identically — otherwise their results are not comparable, and a board
/// that routes under one would fail DRC under the other for reasons that have
/// nothing to do with the routing algorithm.
///
/// Every keep-out is a Chebyshev radius in cells around a copper centre within
/// which no *other* net's track centreline may sit (see [`keepout_cells`]).
struct Surface {
    /// Pads and their clearance halos only. Traces are never committed here:
    /// [`GridRouter`] adds them as it goes, [`PathfinderRouter`] keeps them in a
    /// separate congestion map so they stay negotiable.
    grid: Grid,
    cols: usize,
    rows: usize,
    res: f64,
    minx: f64,
    miny: f64,
    /// Keep-out around a track centreline: another track's centreline must be a
    /// full clearance plus a track width away. A disc of cell offsets — see
    /// [`keepout_disc`].
    track_disc: Vec<(isize, isize)>,
    /// [`track_disc`](Self::track_disc)'s radius in cells, for searches that
    /// only need to know how far a net's influence reaches.
    track_halo: isize,
    /// Keep-out around a via: far enough for a track centreline *and* for
    /// another via, whichever is bigger.
    via_disc: Vec<(isize, isize)>,
    /// Square radius a via's body must find clear of other nets' pads.
    via_halo: isize,
    /// [`via_halo`](Self::via_halo) answered for every cell up front, against
    /// the pads-only grid — see [`ViaTable`].
    via_table: ViaTable,
    /// The net poured as a plane ([`RouteOptions::plane_net`]).
    plane_net: Option<usize>,
    /// Cells (`c + r * cols`) a plane via may not drop on: the plane net's own
    /// pad copper, grown by a via radius.
    plane_no_via: HashSet<usize>,
    /// How far (cells) a plane pad searches for its via.
    plane_reach: usize,
    /// Per routable net: each pad's cell and the layers it connects.
    pad_cells: Vec<Vec<((usize, usize), PadLayer)>>,
    /// Routable pads whose own centre cell lies inside another net's pad
    /// keep-out at this resolution: pads the router could never leave.
    buried_pads: usize,
    costs: Costs,
}

impl Surface {
    #[inline]
    fn cells(&self) -> usize {
        self.cols * self.rows * 2
    }
}

/// Air (mm) kept beyond every clearance, so a separation that is exact on paper
/// is not lost to rounding in KiCad's DRC.
const CLEARANCE_MARGIN_MM: f64 = 0.001;

/// The finest grid [`build_surface`] will drop to. Below this a board-sized grid
/// stops being a sensible thing to search; a pitch that needs finer is reported
/// through the buried pads' conflicts rather than routed.
const MIN_GRID_MM: f64 = 0.025;

/// Chebyshev keep-out radius, in cells, for a required centre-to-centre
/// separation of `d_mm`: the nearest cell *outside* it is `radius + 1` cells
/// away on some axis, so at least `d_mm` away in true (Euclidean) distance.
///
/// Measured from each item's own required separation rather than summed from
/// separately rounded-up parts: rounding a pad's half-width and the clearance up
/// to whole cells *independently* over-inflated every pad by up to two cells,
/// which at 0.5mm pitch swallowed each pin's only exit (legion-of-bom-y17.1).
fn keepout_cells(d_mm: f64, res: f64) -> isize {
    (((d_mm + CLEARANCE_MARGIN_MM) / res).ceil() as isize - 1).max(0)
}

/// The cell offsets around a copper centre where another net's centreline may
/// not sit, for a required centre-to-centre separation of `d_mm` — a **disc**.
///
/// A box ([`keepout_cells`]) is exact along the axes but ~41% too wide on the
/// diagonal, so two tracks running 45° side by side had to be 0.6mm apart
/// where 0.45mm is legal, and a QFP fan-out could not turn. A step between two
/// cells outside the disc cuts across it by at most `res² / 4d` (a chord of a
/// diagonal step), which is added to the radius so every segment, not just
/// every cell, keeps the separation.
fn keepout_disc(d_mm: f64, res: f64) -> Vec<(isize, isize)> {
    let reach = (d_mm + CLEARANCE_MARGIN_MM + res * res / (4.0 * d_mm.max(res))) / res;
    let r = reach.ceil() as isize;
    let mut offsets = Vec::new();
    for dr in -r..=r {
        for dc in -r..=r {
            if ((dc * dc + dr * dr) as f64) < reach * reach {
                offsets.push((dc, dr));
            }
        }
    }
    offsets
}

/// How the maze search decides whether a via may drop at a cell.
#[derive(Clone, Copy)]
enum ViaCheck<'a> {
    /// Look the answer up, and additionally refuse the listed cells (global
    /// `c + r * cols` indices) — the plane net's own pad copper, so a plane
    /// via never lands in an SMD pad and wicks its solder away.
    TableExcept(&'a ViaTable, &'a HashSet<usize>),
    /// Scan the live grid, and refuse the listed cells, for a grid that is
    /// still changing ([`GridRouter`]).
    LiveExcept(isize, &'a HashSet<usize>),
    /// Scan the live grid (it changes as [`GridRouter`] commits copper).
    Live(isize),
    /// Look the answer up — for a grid that never changes during the search.
    Table(&'a ViaTable),
}

/// Whether a via's body (a `±radius` square on both layers) is clear of other
/// nets' pads, answered for every cell at once.
///
/// Scanning that square per search step was the router's hot loop: at the
/// 0.05mm grid a fine-pitch board needs, it is ~1,600 cells per step. Summed-
/// area tables over the pads-only grid make it four lookups: the window must
/// hold no blocked cell, and every owned cell in it must belong to one net —
/// tested exactly as "the owners' ids have zero variance" (count, sum and sum of
/// squares), so no per-cell list is needed.
struct ViaTable {
    cols: usize,
    rows: usize,
    radius: isize,
    /// `(cols + 1) × (rows + 1)` prefix sums of `[blocked, owned, Σid, Σid²]`
    /// over both layers.
    sat: Vec<[i64; 4]>,
}

impl ViaTable {
    fn new(grid: &Grid, radius: isize) -> Self {
        let (cols, rows) = (grid.cols, grid.rows);
        let w = cols + 1;
        let mut sat = vec![[0i64; 4]; w * (rows + 1)];
        for r in 0..rows {
            for c in 0..cols {
                let mut v = [0i64; 4];
                for layer in [FRONT, BACK] {
                    match grid.get(c, r, layer) {
                        Cell::Free => {}
                        Cell::Blocked => v[0] += 1,
                        Cell::Owner(n) => {
                            let n = n as i64;
                            v[1] += 1;
                            v[2] += n;
                            v[3] += n * n;
                        }
                    }
                }
                let (up, left, diag) = (sat[r * w + c + 1], sat[(r + 1) * w + c], sat[r * w + c]);
                let cell = &mut sat[(r + 1) * w + c + 1];
                for k in 0..4 {
                    cell[k] = v[k] + up[k] + left[k] - diag[k];
                }
            }
        }
        ViaTable {
            cols,
            rows,
            radius,
            sat,
        }
    }

    fn allows(&self, c: usize, r: usize, net: usize) -> bool {
        let rad = self.radius;
        let (c0, r0) = (c as isize - rad, r as isize - rad);
        let (c1, r1) = (c as isize + rad + 1, r as isize + rad + 1);
        // Too near the grid edge for its clearance, as the live scan says too.
        if c0 < 0 || r0 < 0 || c1 > self.cols as isize || r1 > self.rows as isize {
            return false;
        }
        let w = self.cols + 1;
        let at = |cc: isize, rr: isize| self.sat[rr as usize * w + cc as usize];
        let (a, b, cc, d) = (at(c1, r1), at(c0, r1), at(c1, r0), at(c0, r0));
        let sum = |k: usize| a[k] - b[k] - cc[k] + d[k];
        let (owned, n) = (sum(1), net as i64);
        sum(0) == 0 && sum(2) == n * owned && sum(3) == n * n * owned
    }
}

/// Discretise the board and paint the pads. See [`Surface`].
///
/// The grid resolution is **derived from the pads**, not configured:
/// `opts.grid_mm` is the coarsest the board is searched at, and the grid halves
/// until every routable pad can at least be left — i.e. no pad's centre sits
/// inside a neighbour's keep-out. A board of 2.54mm headers never leaves the
/// coarse grid; a 0.5mm-pitch QFP drops it to where the pin axis is on a cell.
fn build_surface(nets: &[RouteNet], routable: &[&RouteNet], opts: &RouteOptions) -> Surface {
    let mut res = opts.grid_mm.max(0.01);
    loop {
        let surface = paint_surface(nets, routable, opts, res);
        if surface.buried_pads == 0 || res / 2.0 < MIN_GRID_MM - 1e-9 {
            return surface;
        }
        res /= 2.0;
    }
}

/// [`build_surface`] at one fixed resolution.
fn paint_surface(
    nets: &[RouteNet],
    routable: &[&RouteNet],
    opts: &RouteOptions,
    res: f64,
) -> Surface {
    // `bounds`, when given, is the board outline: inset the routable area by the
    // edge clearance plus the widest copper half (a via) so no track/via lands
    // within `edge_clearance_mm` of the edge. The pad-bbox fallback has no board
    // edge, so it is used as-is.
    let (bounded, (minx, miny, maxx, maxy)) = match opts.bounds {
        Some((x0, y0, x1, y1)) => {
            let inset = opts.edge_clearance_mm
                + (opts.via_size_mm / 2.0).max(opts.signal_width_mm / 2.0)
                + CLEARANCE_MARGIN_MM;
            if x1 - x0 > 2.0 * inset && y1 - y0 > 2.0 * inset {
                (true, (x0 + inset, y0 + inset, x1 - inset, y1 - inset))
            } else {
                (false, (x0, y0, x1, y1)) // too small to inset — leave it (surfaces as conflicts)
            }
        }
        None => (false, bounds_of(nets, 2.0)),
    };
    // The grid sits on the absolute `res` lattice, so a part placed on it has
    // its pins on cell centres — at 0.5mm pitch there is 0.025mm of slack
    // either side of a pin's axis, and an arbitrary origin would throw it away.
    // Inside a real outline the lattice rounds *inward*, never into the edge band.
    let (minx, miny) = if bounded {
        (
            (minx / res - 1e-9).ceil() * res,
            (miny / res - 1e-9).ceil() * res,
        )
    } else {
        ((minx / res).floor() * res, (miny / res).floor() * res)
    };
    let span = |lo: f64, hi: f64| {
        let n = (hi - lo) / res;
        let n = if bounded {
            (n + 1e-9).floor()
        } else {
            n.ceil()
        };
        (n.max(0.0) as usize).max(1) + 1
    };
    let (cols, rows) = (span(minx, maxx), span(miny, maxy));
    let cell_of = |x: f64, y: f64| {
        let c = (((x - minx) / res).round() as isize).clamp(0, cols as isize - 1) as usize;
        let r = (((y - miny) / res).round() as isize).clamp(0, rows as isize - 1) as usize;
        (c, r)
    };

    let mut grid = Grid {
        cols,
        rows,
        cells: vec![Cell::Free; cols * rows * 2],
    };
    let (clr, half_track) = (opts.clearance_mm, opts.signal_width_mm / 2.0);
    let track_sep = clr + 2.0 * half_track;
    let via_sep = opts.via_size_mm / 2.0 + clr + half_track.max(opts.via_size_mm / 2.0);
    let (track_disc, via_disc) = (keepout_disc(track_sep, res), keepout_disc(via_sep, res));
    let track_halo = keepout_cells(track_sep, res);
    let via_halo = keepout_cells(via_sep, res);

    // Paint every pad (and its clearance halo) as its net's territory. Do the
    // cores first so a halo never overwrites a real pad connection point.
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
            // Keep-out is the pad's real rectangle grown by a clearance plus a
            // track half-width, in board coordinates: a cell is out when its
            // centre is strictly inside. Exact, so it neither loses the slack a
            // fine-pitch pin has nor depends on which cell the pad snapped to.
            let d = clr + half_track + CLEARANCE_MARGIN_MM;
            let lo = |v: f64, o: f64| ((v - o) / res).floor() as isize + 1;
            let hi = |v: f64, o: f64| ((v - o) / res).ceil() as isize - 1;
            let (c0, c1) = (
                lo(pad.x_mm - pad.w_mm / 2.0 - d, minx),
                hi(pad.x_mm + pad.w_mm / 2.0 + d, minx),
            );
            let (r0, r1) = (
                lo(pad.y_mm - pad.h_mm / 2.0 - d, miny),
                hi(pad.y_mm + pad.h_mm / 2.0 + d, miny),
            );
            for layer in [FRONT, BACK] {
                if pad.layer.on(layer) {
                    grid.halo_span(c0..=c1, r0..=r1, layer, net.net_idx);
                }
            }
        }
    }

    // Record each routable net's pad cells (for source/target sets). A pad is a
    // connection point on every layer it touches — so a through-hole pad is
    // reachable on both, letting the router meet it without a via.
    let mut pad_cells: Vec<Vec<((usize, usize), PadLayer)>> = Vec::new();
    let mut buried_pads = 0;
    for net in routable {
        let mut cells = Vec::new();
        for pad in &net.pads {
            let (c, r) = cell_of(pad.x_mm, pad.y_mm);
            let layers = pad.layer.layers();
            if layers
                .iter()
                .any(|&l| grid.get(c, r, l) != Cell::Owner(net.net_idx))
            {
                buried_pads += 1;
            }
            for &layer in layers {
                grid.set(c, r, layer, Cell::Owner(net.net_idx));
            }
            cells.push(((c, r), pad.layer));
        }
        pad_cells.push(cells);
    }

    let via_table = ViaTable::new(&grid, via_halo);
    let mut plane_no_via = HashSet::new();
    if let Some(plane) = opts.plane_net {
        let grow = opts.via_size_mm / 2.0;
        for pad in nets.iter().filter(|n| n.net_idx == plane).flat_map(|n| &n.pads) {
            let lo = |v: f64, o: f64| (((v - o) / res).floor() as isize).max(0) as usize;
            let hi = |v: f64, o: f64, n: usize| (((v - o) / res).ceil() as usize).min(n - 1);
            let (x0, x1) = (pad.x_mm - pad.w_mm / 2.0 - grow, pad.x_mm + pad.w_mm / 2.0 + grow);
            let (y0, y1) = (pad.y_mm - pad.h_mm / 2.0 - grow, pad.y_mm + pad.h_mm / 2.0 + grow);
            for r in lo(y0, miny)..=hi(y1, miny, rows) {
                for c in lo(x0, minx)..=hi(x1, minx, cols) {
                    plane_no_via.insert(c + r * cols);
                }
            }
        }
    }
    Surface {
        grid,
        cols,
        rows,
        res,
        minx,
        miny,
        via_table,
        plane_net: opts.plane_net,
        plane_no_via,
        plane_reach: (PLANE_REACH_MM / res).ceil() as usize,
        track_disc,
        track_halo,
        via_disc,
        via_halo,
        pad_cells,
        buried_pads,
        costs: Costs {
            step: (res * 1000.0) as i64,
            via: (opts.via_cost_mm * 1000.0) as i64,
            back: (opts.back_penalty_mm * 1000.0) as i64,
        },
    }
}

/// One routing pass over `routable` in the given `order` (indices into
/// `routable`). Returns the routed output and the indices of nets that did not
/// fully route this pass.
///
/// `learned` biases the search away from cells a prior negotiation found
/// contested (see [`PathfinderRouter`]). Pass [`Congestion::neutral`] for a plain
/// pass — the cost arithmetic then reduces to exactly the unbiased cost.
#[allow(clippy::type_complexity)]
fn route_pass(
    nets: &[RouteNet],
    routable: &[&RouteNet],
    order: &[usize],
    net_to_rt: &HashMap<usize, usize>,
    opts: &RouteOptions,
    learned: &Congestion,
) -> (RouteOutput, Vec<usize>, HashMap<usize, HashSet<usize>>) {
    let mut out = RouteOutput::default();
    // Same discretisation as `PathfinderRouter`, so the two are comparable.
    let mut surface = build_surface(nets, routable, opts);
    let (cols, rows, track_halo, via_halo, res) = (
        surface.cols,
        surface.rows,
        surface.track_halo,
        surface.via_halo,
        surface.res,
    );
    let (minx, miny) = (surface.minx, surface.miny);
    let mm_of = move |c: usize, r: usize| (minx + c as f64 * res, miny + r as f64 * res);
    let pad_cells = surface.pad_cells.clone();
    let (track_disc, via_disc) = (surface.track_disc.clone(), surface.via_disc.clone());
    let (plane_net, plane_reach) = (surface.plane_net, surface.plane_reach);
    let plane_no_via = std::mem::take(&mut surface.plane_no_via);
    let grid = &mut surface.grid;

    // Route each net in the given order, growing a tree from pad 0.
    let mut failed: Vec<usize> = Vec::new();
    // Blame: victim routable-idx → the routable nets whose committed copper
    // boxes in one of its pads (candidates to rip up and reroute).
    let mut blame: HashMap<usize, HashSet<usize>> = HashMap::new();
    // How far around an unreachable pad to look for the nets fencing it in.
    let blame_radius = (track_halo * 3).max(6);
    for &ni in order {
        let net = routable[ni];
        let costs = Costs {
            step: (res * 1000.0) as i64,
            via: (opts.via_cost_mm * 1000.0) as i64,
            back: (opts.back_penalty_mm * 1000.0) as i64,
        };

        // A plane net: each single-layer pad's own stub and via onto the pour,
        // exactly as PathfinderRouter does it (see `route_net_soft`).
        if plane_net == Some(net.net_idx) {
            for (k, &((c, r), layer)) in pad_cells[ni].iter().enumerate() {
                let start = match layer {
                    PadLayer::Front => (c, r, FRONT),
                    PadLayer::Back => (c, r, BACK),
                    PadLayer::Both => continue,
                };
                let vias = ViaCheck::LiveExcept(via_halo, &plane_no_via);
                match grid.route_to_plane(net.net_idx, start, plane_reach, costs, vias, learned, 0)
                {
                    Some(path) => {
                        emit_path(&path, net.net_idx, opts, &mm_of, &mut out);
                        for &(c, r, l) in &path {
                            grid.commit(c, r, l, net.net_idx, &track_disc);
                        }
                        if let Some(&(vc, vr, _)) = path.last() {
                            grid.commit_via(vc, vr, net.net_idx, &via_disc);
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
                    }
                }
            }
            continue;
        }

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
            match grid.route_one_soft(
                net.net_idx,
                &connected,
                &targets,
                costs,
                ViaCheck::Live(via_halo),
                learned,
                0,
            ) {
                Some(path) => {
                    emit_path(&path, net.net_idx, opts, &mm_of, &mut out);
                    for &(c, r, l) in &path {
                        grid.commit(c, r, l, net.net_idx, &track_disc);
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
                            grid.commit_via(w[0].0, w[0].1, net.net_idx, &via_disc);
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
    /// Paint a clearance halo over a cell rectangle (clipped to the grid) as
    /// `net`'s territory; where it meets another net's territory the pinch is
    /// [`Cell::Blocked`].
    fn halo_span(
        &mut self,
        cols: std::ops::RangeInclusive<isize>,
        rows: std::ops::RangeInclusive<isize>,
        layer: usize,
        net: usize,
    ) {
        let c0 = (*cols.start()).max(0);
        let c1 = (*cols.end()).min(self.cols as isize - 1);
        let r0 = (*rows.start()).max(0);
        let r1 = (*rows.end()).min(self.rows as isize - 1);
        for r in r0..=r1 {
            for c in c0..=c1 {
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
    /// Paint `offsets` around `(cc, cr)` as `net`'s territory, the way
    /// [`halo_span`](Self::halo_span) paints a rectangle.
    fn halo_offsets(
        &mut self,
        cc: usize,
        cr: usize,
        layer: usize,
        net: usize,
        offsets: &[(isize, isize)],
    ) {
        for &(dc, dr) in offsets {
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
    /// Commit a routed cell plus its clearance disc to `net`.
    fn commit(&mut self, c: usize, r: usize, layer: usize, net: usize, disc: &[(isize, isize)]) {
        self.set(c, r, layer, Cell::Owner(net));
        self.halo_offsets(c, r, layer, net, disc);
    }
    /// Reserve a via's body + clearance at `(c, r)` on both layers for `net`.
    fn commit_via(&mut self, c: usize, r: usize, net: usize, disc: &[(isize, isize)]) {
        for layer in [FRONT, BACK] {
            self.set(c, r, layer, Cell::Owner(net));
            self.halo_offsets(c, r, layer, net, disc);
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

    /// A* from any `sources` cell to the nearest `targets` cell for `net`, where
    /// other nets' **copper is negotiable**: routing through it costs more rather
    /// than being impossible. Pads and their clearance stay hard — those are not
    /// a resource anyone can bargain for.
    ///
    /// With a [`neutral`](Congestion::neutral) congestion and `p_fac = 0` every
    /// multiplier is 1 and this is a plain least-cost maze search, which is how
    /// [`GridRouter`] uses it.
    ///
    /// The toll on a cell is PathFinder's: `base × present × history`. `present`
    /// rises with how many nets are on the cell right now and with `p_fac`, which
    /// grows each iteration; `history` accumulates on cells that keep being
    /// fought over. Both are ≥ 1, so the octile heuristic stays admissible and A*
    /// still finds the true least-cost path.
    #[allow(clippy::too_many_arguments)]
    fn route_one_soft(
        &self,
        net: usize,
        sources: &[(usize, usize, usize)],
        targets: &[(usize, usize, usize)],
        costs: Costs,
        vias: ViaCheck<'_>,
        cong: &Congestion,
        p_fac: i64,
    ) -> Option<Vec<(usize, usize, usize)>> {
        // Search a window around the connection first. A connection that can
        // be made at all nearly always can be made near its own endpoints, and
        // at a fine-pitch board's 0.05mm grid the whole board is millions of
        // cells — searching (and allocating) all of it per connection is what
        // made a 24-pin LQFP board take minutes. Only a connection the window
        // cannot make pays for the whole board.
        //
        // The window spans the targets and the one source nearest them — not
        // every source: a net's sources are its whole routed tree so far, and
        // for a 24-pad GND that tree spans the board, which would make every
        // window the board.
        let chebyshev = |a: &(usize, usize, usize), b: &(usize, usize, usize)| {
            a.0.abs_diff(b.0).max(a.1.abs_diff(b.1))
        };
        let nearest = sources
            .iter()
            .min_by_key(|s| targets.iter().map(|t| chebyshev(s, t)).min().unwrap_or(0));
        let (mut c0, mut r0, mut c1, mut r1) = (usize::MAX, usize::MAX, 0, 0);
        for &(c, r, _) in nearest.into_iter().chain(targets) {
            (c0, r0, c1, r1) = (c0.min(c), r0.min(r), c1.max(c), r1.max(r));
        }
        let span = (c1 - c0).max(r1 - r0);
        let margin = (SEARCH_MARGIN_CELLS).max(span / 4);
        let window = (
            c0.saturating_sub(margin),
            r0.saturating_sub(margin),
            (c1 + margin).min(self.cols - 1),
            (r1 + margin).min(self.rows - 1),
        );
        let whole = (0, 0, self.cols - 1, self.rows - 1);
        let search = |w| self.search(net, sources, targets, false, costs, vias, cong, p_fac, w);
        search(window).or_else(|| (window != whole).then(|| search(whole)).flatten())
    }

    /// The cheapest short path from a single-layer pad cell to a via onto the
    /// other layer — how a plane net's pad reaches its pour. Searched within
    /// `reach` cells of the pad; a pad with no legal via that close is reported
    /// rather than dragged across the board.
    #[allow(clippy::too_many_arguments)]
    fn route_to_plane(
        &self,
        net: usize,
        pad: (usize, usize, usize),
        reach: usize,
        costs: Costs,
        vias: ViaCheck<'_>,
        cong: &Congestion,
        p_fac: i64,
    ) -> Option<Vec<(usize, usize, usize)>> {
        let (c, r, _) = pad;
        let window = (
            c.saturating_sub(reach),
            r.saturating_sub(reach),
            (c + reach).min(self.cols - 1),
            (r + reach).min(self.rows - 1),
        );
        self.search(net, &[pad], &[], true, costs, vias, cong, p_fac, window)
    }

    /// [`route_one_soft`](Self::route_one_soft) confined to the inclusive cell
    /// rectangle `(c0, r0, c1, r1)`, with search state sized to it.
    #[allow(clippy::too_many_arguments)]
    fn search(
        &self,
        net: usize,
        sources: &[(usize, usize, usize)],
        targets: &[(usize, usize, usize)],
        // Instead of `targets`, stop at the first cell on the other layer from
        // the sources — a via onto a plane.
        to_plane: bool,
        costs: Costs,
        vias: ViaCheck<'_>,
        cong: &Congestion,
        p_fac: i64,
        (c0, r0, c1, r1): (usize, usize, usize, usize),
    ) -> Option<Vec<(usize, usize, usize)>> {
        let Costs {
            step,
            via: via_cost,
            back: back_penalty,
        } = costs;
        let (ww, wh) = (c1 - c0 + 1, r1 - r0 + 1);
        let wa = ww * wh;
        // Local (window) index <-> cell.
        let local = |c: usize, r: usize, l: usize| (l * wh + (r - r0)) * ww + (c - c0);
        let cell = |i: usize| {
            let (l, rem) = (i / wa, i % wa);
            (c0 + rem % ww, r0 + rem / ww, l)
        };
        let mut dist = vec![i64::MAX; wa * 2];
        let mut prev = vec![usize::MAX; wa * 2];
        let heuristic = |c: usize, r: usize| -> i64 {
            targets
                .iter()
                .map(|&(tc, tr, _)| {
                    let (dc, dr) = (c.abs_diff(tc) as i64, r.abs_diff(tr) as i64);
                    let (lo, hi) = (dc.min(dr), dc.max(dr));
                    hi * step + lo * (DIAG_NUM - DENOM) * step / DENOM
                })
                .min()
                .unwrap_or(0)
        };
        // What entering cell `j` (a global index) really costs, once contention
        // is priced in.
        let toll = |j: usize, base: i64| -> i64 {
            let present = (PF_ONE + p_fac.saturating_mul(cong.contest(j))).min(PF_PRESENT_MAX);
            let scaled = base.saturating_mul(present) / PF_ONE;
            scaled.saturating_mul(cong.hist_at(j)) / PF_ONE
        };
        let inside = |c: usize, r: usize| c >= c0 && c <= c1 && r >= r0 && r <= r1;
        let mut heap: BinaryHeap<Reverse<(i64, i64, usize)>> = BinaryHeap::new();
        for &(c, r, l) in sources.iter().filter(|&&(c, r, _)| inside(c, r)) {
            let i = local(c, r, l);
            if dist[i] != 0 {
                dist[i] = 0;
                heap.push(Reverse((heuristic(c, r), 0i64, i)));
            }
        }
        let tgt: Vec<usize> = targets
            .iter()
            .filter(|&&(c, r, _)| inside(c, r))
            .map(|&(c, r, l)| local(c, r, l))
            .collect();
        while let Some(Reverse((_f, g, i))) = heap.pop() {
            if g > dist[i] {
                continue;
            }
            if tgt.contains(&i) || (to_plane && cell(i).2 != sources[0].2) {
                let mut path = Vec::new();
                let mut cur = i;
                while cur != usize::MAX {
                    path.push(cell(cur));
                    cur = prev[cur];
                }
                path.reverse();
                return Some(path);
            }
            let (c, r, layer) = cell(i);
            let neigh = [
                (-1isize, 0isize),
                (1, 0),
                (0, -1),
                (0, 1),
                (-1, -1),
                (-1, 1),
                (1, -1),
                (1, 1),
            ];
            for (dc, dr) in neigh {
                let (nc, nr) = (c as isize + dc, r as isize + dr);
                if nc < c0 as isize || nr < r0 as isize || nc > c1 as isize || nr > r1 as isize {
                    continue;
                }
                let (nc, nr) = (nc as usize, nr as usize);
                if !self.passable(nc, nr, layer, net) {
                    continue;
                }
                let diagonal = dc != 0 && dr != 0;
                if diagonal
                    && (!self.passable((c as isize + dc) as usize, r, layer, net)
                        || !self.passable(c, (r as isize + dr) as usize, layer, net))
                {
                    continue;
                }
                let straight = if diagonal {
                    step * DIAG_NUM / DENOM
                } else {
                    step
                };
                let base = straight + if layer == BACK { back_penalty } else { 0 };
                relax(
                    local(nc, nr, layer),
                    g + toll(self.idx(nc, nr, layer), base),
                    heuristic(nc, nr),
                    i,
                    &mut dist,
                    &mut prev,
                    &mut heap,
                );
            }
            // A via still needs its whole body clear of *pads*; other nets' copper
            // there is priced by the toll like any other contention.
            let other = layer ^ 1;
            let via_ok = match vias {
                ViaCheck::Live(via_halo) => self.via_area_clear(c, r, net, via_halo),
                ViaCheck::Table(t) => t.allows(c, r, net),
                ViaCheck::TableExcept(t, refused) => {
                    !refused.contains(&(c + r * self.cols)) && t.allows(c, r, net)
                }
                ViaCheck::LiveExcept(via_halo, refused) => {
                    !refused.contains(&(c + r * self.cols))
                        && self.via_area_clear(c, r, net, via_halo)
                }
            };
            if via_ok {
                relax(
                    local(c, r, other),
                    g + toll(self.idx(c, r, other), via_cost),
                    heuristic(c, r),
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

// ---- PathfinderRouter: negotiated congestion -------------------------------

/// Fixed-point 1.0 for the congestion multipliers.
const PF_ONE: i64 = DENOM;
/// Starting present-congestion factor. Deliberately low: the first iteration is
/// then close to a pure shortest-path solve, so every net asks for the route it
/// actually wants and the contention that shows up is *real* contention rather
/// than an artefact of the penalty.
const PF_PRESENT_START: i64 = PF_ONE / 2;
/// Multiplier applied to the present factor each iteration — the pressure ramp.
/// Gentle on purpose (VPR uses ~1.3): doubling saturates the cost in a handful of
/// rounds, after which every occupied cell looks equally catastrophic and nets
/// stop choosing between corridors and simply stampede away from whatever is
/// occupied *this* round. That is what oscillation looks like from the inside.
const PF_PRESENT_GROWTH: i64 = 1300;
/// Ceiling on the present factor. This is the knob that was wrong: at 1<<22 the
/// present term reached ~4,200,000x base while history could only ever reach
/// ~13x, so history — the thing that is supposed to *settle* the negotiation —
/// could never out-argue it and the router thrashed for 24 rounds.
const PF_PRESENT_MAX: i64 = 64 * PF_ONE;
/// How much a contested cell's history grows per iteration it stays contested.
/// Comparable to the present ceiling over a full run, so a corridor that keeps
/// being fought over eventually loses regardless of who is momentarily on it.
const PF_HISTORY_STEP: i64 = PF_ONE;

/// Per-cell contention: who is on each routing resource now, and how hard it has
/// been fought over across iterations.
struct Congestion {
    /// Distinct nets whose copper *core* occupies this cell.
    core: Vec<u16>,
    /// Distinct nets whose clearance halo covers it. A net's halo never includes
    /// its own core cells, so `core >= 1 && halo >= 1` always means two nets.
    halo: Vec<u16>,
    /// Accumulated historical contention, fixed point; `PF_ONE` is neutral.
    hist: Vec<i64>,
}

impl Congestion {
    /// No occupancy and no history — every cost multiplier is exactly 1, so a
    /// search biased by this is identical to an unbiased one. Allocates nothing:
    /// the accessors read an absent cell as neutral, which is what makes it safe
    /// to hand to a pass whose grid size the caller does not know.
    fn neutral() -> Self {
        Congestion {
            core: Vec::new(),
            halo: Vec::new(),
            hist: Vec::new(),
        }
    }
    fn new(cells: usize) -> Self {
        Congestion {
            core: vec![0; cells],
            halo: vec![0; cells],
            hist: vec![PF_ONE; cells],
        }
    }
    /// What another net's core would collide with here. A core cell and a halo
    /// cell are both clearance violations, so both are priced.
    #[inline]
    fn contest(&self, i: usize) -> i64 {
        let c = self.core.get(i).copied().unwrap_or(0) as i64;
        let h = self.halo.get(i).copied().unwrap_or(0) as i64;
        c + h
    }
    /// Accumulated contention for a cell; `PF_ONE` (neutral) when unknown.
    #[inline]
    fn hist_at(&self, i: usize) -> i64 {
        self.hist.get(i).copied().unwrap_or(PF_ONE)
    }
    #[inline]
    fn overused(&self, i: usize) -> bool {
        self.core[i] > 1 || (self.core[i] >= 1 && self.halo[i] >= 1)
    }
    fn add(&mut self, rt: &NetRoute) {
        for &i in &rt.core {
            self.core[i] += 1;
        }
        for &i in &rt.halo {
            self.halo[i] += 1;
        }
    }
    fn remove(&mut self, rt: &NetRoute) {
        for &i in &rt.core {
            self.core[i] = self.core[i].saturating_sub(1);
        }
        for &i in &rt.halo {
            self.halo[i] = self.halo[i].saturating_sub(1);
        }
    }
    /// Raise the history cost of everything still contested. This is what stops
    /// the router oscillating: a corridor two nets keep swapping into gets
    /// permanently more expensive until one of them gives up on it for good.
    fn age(&mut self) {
        for i in 0..self.core.len() {
            if self.overused(i) {
                self.hist[i] += PF_HISTORY_STEP;
            }
        }
    }
}

/// One net's current routing, and the resources it is holding.
#[derive(Default, Clone)]
struct NetRoute {
    paths: Vec<Vec<(usize, usize, usize)>>,
    /// Cell indices the copper itself occupies.
    core: Vec<usize>,
    /// Cell indices its clearance covers, excluding `core`.
    halo: Vec<usize>,
    /// Pad indices that could not be reached at all, even paying every toll.
    unreached: Vec<usize>,
}

impl NetRoute {
    /// Work out which cells this route holds: the copper, and the clearance
    /// around it. Sorted and deduped, so a net is counted once per cell however
    /// many times its own path crosses it.
    fn claim(&mut self, s: &Surface) {
        let mut core: Vec<usize> = Vec::new();
        let mut halo: Vec<usize> = Vec::new();
        let disc = |v: &mut Vec<usize>, c: usize, r: usize, l: usize, offs: &[(isize, isize)]| {
            for &(dc, dr) in offs {
                let (nc, nr) = (c as isize + dc, r as isize + dr);
                if nc < 0 || nr < 0 || nc >= s.cols as isize || nr >= s.rows as isize {
                    continue;
                }
                v.push(s.grid.idx(nc as usize, nr as usize, l));
            }
        };
        for path in &self.paths {
            for &(c, r, l) in path {
                core.push(s.grid.idx(c, r, l));
            }
            // A via is a hole through both layers, with a wider body than a track.
            for w in path.windows(2) {
                if w[0].2 != w[1].2 {
                    for l in [FRONT, BACK] {
                        core.push(s.grid.idx(w[0].0, w[0].1, l));
                        disc(&mut halo, w[0].0, w[0].1, l, &s.via_disc);
                    }
                }
            }
        }
        for path in &self.paths {
            for &(c, r, l) in path {
                disc(&mut halo, c, r, l, &s.track_disc);
            }
        }
        core.sort_unstable();
        core.dedup();
        halo.sort_unstable();
        halo.dedup();
        halo.retain(|i| core.binary_search(i).is_err());
        self.core = core;
        self.halo = halo;
    }
}

/// Negotiated-congestion router — PathFinder (McMurchie & Ebeling, FPGA'95).
///
/// [`GridRouter`] commits copper as it goes, so the first net to want a corridor
/// owns it outright and a later net simply fails. Its only recourse is a
/// different net *ordering*, and ordering cannot help when two nets both need the
/// same corridor and no order works — which is exactly the shape of failure that
/// left the slew limiter with unroutable nets at every width.
///
/// PathFinder takes ordering out of the answer. Every iteration rips up and
/// reroutes **every** net, and nets are allowed to overlap: overlap is *priced*,
/// not forbidden. The price has two parts.
///
/// * **Present** congestion rises with the number of nets on a cell, scaled by a
///   factor that grows each iteration. Early on sharing is cheap, so every net
///   asks for the route it genuinely wants; as the factor ramps, shared corridors
///   become expensive and nets with an alternative peel away.
/// * **History** accumulates on cells that stay contested, and never decays. This
///   is what stops two nets swapping into the same corridor forever: the corridor
///   itself gets more expensive every iteration it is fought over, until one net
///   abandons it permanently.
///
/// A net with a cheap detour takes it; a net with no alternative keeps the
/// resource. The outcome is decided by *measured contention* rather than by who
/// happened to be routed first.
///
/// When negotiation converges, every cell is held by one net and the result is
/// legal by construction. When it does not, the nets that are still conflict-free
/// are emitted and the rest are reported through [`RouteOutput::conflicts`] —
/// this never emits copper it knows to be shorted.
#[derive(Debug, Clone)]
pub struct PathfinderRouter {
    /// Negotiation rounds before giving up. Each is a full rip-up and reroute of
    /// every net.
    pub max_iters: usize,
}

/// Stop when the best result has not improved for this many consecutive rounds.
///
/// What makes a generous [`PathfinderRouter::max_iters`] safe. Without it the cap
/// is the only stopping condition, so raising it to cover a hard board taxes
/// every board that will never settle — the 42 HP crossfader already spends ~20
/// minutes at 160 (`legion-of-bom-an8`).
///
/// Sized from the measured trace, not picked: on the dual the longest gap between
/// two improvements is 33 rounds (round 123 -> 156). 120 is ~3.6x that, so a board
/// still making progress at this board's worst rate keeps its budget, and one that
/// has genuinely stopped gives up in a bounded time.
const PF_STALL_ROUNDS: usize = 120;

impl Default for PathfinderRouter {
    fn default() -> Self {
        // Generous, because it costs nothing when it is not needed: negotiation
        // breaks out the moment no cell is over capacity, so a board that settles
        // in six rounds pays for six. The cap only bites on boards that are still
        // negotiating, and those are exactly the ones that need the rounds.
        //
        // Measured on the real slew limiter, same placement, only this changed:
        //      24 iters -> 2 unrouted, 2 DRC
        //     150 iters -> 0 unrouted, 0 DRC
        // The board was one budget away from clean and 24 was hiding it.
        //
        // THEN THE SAME THING HAPPENED ONE BOARD SIZE UP (`legion-of-bom-kcq`).
        // The 46-part dual at 160 came back byte-identical to GridRouter, which
        // read as "negotiation never converges on a board this size". It does.
        // `LOB_PF_TRACE=1` shows overused cells falling 1006 -> 237 by round 10,
        // then grinding: 161@33, 115@106, 94@123, 55@156, 5@158, 3@184, and
        // ZERO at round 186. The cap stopped it at 158 holding five contested
        // cells, so the never-worse guard correctly handed back the baseline —
        // the guard was right, the budget was wrong.
        //
        //     160 iters -> 17 unrouted, 16 DRC   (identical to grid)
        //     300 iters ->  0 unrouted,  0 DRC
        //     600 iters ->  0 unrouted,  0 DRC   (same board; nothing bought)
        //
        // So the cap is now well clear of any board we have measured, and
        // `PF_STALL_ROUNDS` — not the cap — is what stops a hopeless one.
        PathfinderRouter { max_iters: 600 }
    }
}

impl Router for PathfinderRouter {
    fn route(&self, nets: &[RouteNet], opts: &RouteOptions) -> RouteOutput {
        let routable: Vec<&RouteNet> = nets.iter().filter(|n| n.pads.len() >= 2).collect();
        if routable.is_empty() {
            return RouteOutput::default();
        }
        // Deterministic net order. Unlike GridRouter this is *not* load-bearing —
        // it only decides who moves first within a round — but it must be stable
        // so the same board comes out twice.
        let mut order: Vec<usize> = (0..routable.len()).collect();
        order.sort_by_key(|&i| routable[i].net_idx);

        let surface = build_surface(nets, &routable, opts);
        let (minx, miny, res) = (surface.minx, surface.miny, surface.res);
        let mm_of = move |c: usize, r: usize| (minx + c as f64 * res, miny + r as f64 * res);
        let mut cong = Congestion::new(surface.cells());
        let mut routes: Vec<NetRoute> = vec![NetRoute::default(); routable.len()];
        let mut p_fac = PF_PRESENT_START;
        // The best round seen, by (connections it could not reach, cells still
        // contested). Negotiation does not improve monotonically — a round can be
        // worse than the one before — so taking the *last* round throws away work.
        let mut best: Option<(usize, usize, Vec<NetRoute>, Vec<usize>)> = None;
        // Consecutive rounds that did not improve on `best`. See PF_STALL_ROUNDS.
        let mut stalled = 0usize;

        for _it in 0..self.max_iters.max(1) {
            for &ni in &order {
                // Rip up first, so the net does not negotiate against itself: with
                // its own copper removed, every contested cell it sees belongs to
                // somebody else.
                cong.remove(&routes[ni]);
                let mut rt = route_net_soft(&surface, routable[ni], ni, &cong, p_fac);
                rt.claim(&surface);
                cong.add(&rt);
                routes[ni] = rt;
            }
            let over = (0..cong.core.len()).filter(|&i| cong.overused(i)).count();
            let unreached: usize = routes.iter().map(|r| r.unreached.len()).sum();
            // `LOB_PF_TRACE=1` prints every round that improves on the best so
            // far. Tuning `max_iters` without this is guesswork: the question is
            // not "how many rounds did it take" but "was it still making progress
            // when the cap cut it off", and only the improvement trace answers
            // that. Matches `LOB_PLACE_TRACE` in the placer.
            let improved = best
                .as_ref()
                .is_none_or(|(u, o, _, _)| (unreached, over) < (*u, *o));
            if improved && std::env::var_os("LOB_PF_TRACE").is_some() {
                eprintln!("  pf round {_it}: unreached {unreached}, overused {over}");
            }
            if improved {
                let contested: Vec<usize> = (0..routable.len())
                    .map(|ni| {
                        routes[ni]
                            .core
                            .iter()
                            .filter(|&&i| cong.overused(i))
                            .count()
                    })
                    .collect();
                best = Some((unreached, over, routes.clone(), contested));
                stalled = 0;
            } else {
                stalled += 1;
            }
            if over == 0 {
                break;
            }
            // Nothing has got better for a long time. More rounds of a
            // negotiation that has stopped negotiating are just wall clock, and
            // the fallback ordering search below still gets the history map.
            if stalled >= PF_STALL_ROUNDS {
                break;
            }
            cong.age();
            p_fac = (p_fac.saturating_mul(PF_PRESENT_GROWTH) / PF_ONE).min(PF_PRESENT_MAX);
        }

        // When negotiation settled, every cell is held by one net and the routes
        // ARE the answer — legal by construction. Shipping them is the whole point
        // of the algorithm; re-routing from scratch here would throw away exactly
        // the corridors the negotiation was run to find.
        if let Some((_, 0, settled, _)) = &best {
            let mut out = RouteOutput::default();
            for &ni in &order {
                let net = routable[ni];
                for path in &settled[ni].paths {
                    emit_path(path, net.net_idx, opts, &mm_of, &mut out);
                }
                for &k in &settled[ni].unreached {
                    out.conflicts.push(format!(
                        "net {} ({}): could not route to pad {}.{}",
                        net.net_idx, net.name, net.pads[k].refdes, net.pads[k].pad
                    ));
                }
            }
            return out;
        }
        // Negotiation did not settle. Spend what it learned on the same
        // hard-clearance ordering search GridRouter uses: nets that ended up most
        // contested go first, and every net pays the history cost of corridors
        // that turned out to be fought over.
        let (contested, hist) = match best {
            Some((_, _, _, c)) => (c, cong.hist),
            None => (vec![0; routable.len()], Vec::new()),
        };
        let mut biased = order.clone();
        biased.sort_by_key(|&ni| (Reverse(contested[ni]), routable[ni].net_idx));
        let learned = Congestion {
            core: Vec::new(),
            halo: Vec::new(),
            hist,
        };
        let guided = search_orderings(nets, &routable, biased, opts, &learned);

        // …and keep it only if it actually helped. A history map from a
        // negotiation that never settled can be misleading, and steering the
        // search with a bad map is how this router ended up LOSING to the plain
        // one on boards the plain one handles. Running the unbiased search too
        // costs one pass and makes "never worse than the baseline" a property of
        // the code rather than a hope.
        let plain = search_orderings(nets, &routable, order, opts, &Congestion::neutral());
        if guided.conflicts.len() <= plain.conflicts.len() {
            guided
        } else {
            plain
        }
    }
}

/// Which connections are **impossible for this placement**, whatever the router.
///
/// Each net is routed as if it were alone on the board: pads and their clearance
/// are obstacles, every other net's copper is ignored entirely. A connection that
/// fails here cannot be made by *any* router, because there is no path through
/// the geometry — the parts are simply in the wrong places.
///
/// This is the measurement that separates "the router gave up" from "placement
/// made it impossible", which are the same symptom (an unrouted net) with
/// completely different fixes. Run it before tuning a router.
pub fn unroutable_by_placement(nets: &[RouteNet], opts: &RouteOptions) -> Vec<String> {
    let routable: Vec<&RouteNet> = nets.iter().filter(|n| n.pads.len() >= 2).collect();
    if routable.is_empty() {
        return Vec::new();
    }
    let surface = build_surface(nets, &routable, opts);
    let free = Congestion::neutral();
    let mut out = Vec::new();
    for (ni, net) in routable.iter().enumerate() {
        // p_fac = 0 and no history: nothing costs more than its own length, and
        // no other net is on the board at all.
        let rt = route_net_soft(&surface, net, ni, &free, 0);
        for &k in &rt.unreached {
            out.push(format!(
                "net {} ({}): pad {}.{} unreachable even with the board to itself",
                net.net_idx, net.name, net.pads[k].refdes, net.pads[k].pad
            ));
        }
    }
    out
}

/// Route one net's whole tree against the current congestion, growing from pad 0
/// and letting each later pad reach whatever the net has already built.
fn route_net_soft(
    s: &Surface,
    net: &RouteNet,
    ni: usize,
    cong: &Congestion,
    p_fac: i64,
) -> NetRoute {
    let mut rt = NetRoute::default();
    // A plane net's pads are joined by the pour, not by each other: each
    // single-layer pad needs only its own stub and via onto the other layer.
    // A through-hole pad is already on both.
    if s.plane_net == Some(net.net_idx) {
        for (k, &((c, r), layer)) in s.pad_cells[ni].iter().enumerate() {
            let start = match layer {
                PadLayer::Front => (c, r, FRONT),
                PadLayer::Back => (c, r, BACK),
                PadLayer::Both => continue,
            };
            let vias = ViaCheck::TableExcept(&s.via_table, &s.plane_no_via);
            match s.grid.route_to_plane(net.net_idx, start, s.plane_reach, s.costs, vias, cong, p_fac) {
                Some(path) => rt.paths.push(path),
                None => rt.unreached.push(k),
            }
        }
        return rt;
    }
    let mut connected: Vec<(usize, usize, usize)> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut join = |c: usize, r: usize, l: usize, conn: &mut Vec<(usize, usize, usize)>| {
        if seen.insert(s.grid.idx(c, r, l)) {
            conn.push((c, r, l));
        }
    };
    let ((p0c, p0r), p0layer) = s.pad_cells[ni][0];
    for &l in p0layer.layers() {
        join(p0c, p0r, l, &mut connected);
    }
    for k in 1..net.pads.len() {
        let ((tc, tr), tlayer) = s.pad_cells[ni][k];
        let targets: Vec<(usize, usize, usize)> =
            tlayer.layers().iter().map(|&l| (tc, tr, l)).collect();
        match s.grid.route_one_soft(
            net.net_idx,
            &connected,
            &targets,
            s.costs,
            ViaCheck::Table(&s.via_table),
            cong,
            p_fac,
        ) {
            Some(path) => {
                for &(c, r, l) in &path {
                    join(c, r, l, &mut connected);
                }
                // A through-hole target bridges both layers into the tree.
                for &l in tlayer.layers() {
                    join(tc, tr, l, &mut connected);
                }
                rt.paths.push(path);
            }
            None => rt.unreached.push(k),
        }
    }
    rt
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

    // ---- PathfinderRouter -------------------------------------------------

    #[test]
    fn pathfinder_routes_a_two_pad_net() {
        let net = RouteNet {
            net_idx: 3,
            name: "OUT".into(),
            pads: vec![pad("R1", "2", 100.0, 100.0), pad("C1", "1", 104.0, 100.0)],
        };
        let out = PathfinderRouter::default().route(&[net], &RouteOptions::default());
        assert!(!out.tracks.is_empty(), "should route the net");
        assert!(out.conflicts.is_empty(), "conflicts: {:?}", out.conflicts);
        assert_eq!(out.tracks[0].net_idx, 3);
    }

    #[test]
    fn pathfinder_routes_around_an_obstructing_pad() {
        let nets = vec![
            RouteNet {
                net_idx: 1,
                name: "OUT".into(),
                pads: vec![pad("R1", "1", 100.0, 100.0), pad("R2", "1", 110.0, 100.0)],
            },
            RouteNet {
                net_idx: 2,
                name: "GND".into(),
                pads: vec![pad("C1", "1", 105.0, 100.0), pad("C1", "2", 105.0, 106.0)],
            },
        ];
        let out = PathfinderRouter::default().route(&nets, &RouteOptions::default());
        assert!(out.conflicts.is_empty(), "conflicts: {:?}", out.conflicts);
        // OUT cannot go straight through GND's pad, so it must leave the y=100 line.
        let straight = out
            .tracks
            .iter()
            .filter(|t| t.net_idx == 1)
            .all(|t| (t.start.1 - 100.0).abs() < 1e-9 && (t.end.1 - 100.0).abs() < 1e-9);
        assert!(!straight, "OUT should detour around the GND pad");
    }

    /// Two signals crossing one gap in a wall. A CORRECTNESS fixture only.
    ///
    /// Named "contested" once; it is not, in two ways that were both measured
    /// rather than argued (`legion-of-bom-69v.1`):
    ///
    /// * The wall pads are `PadLayer::Front`, so the back layer is an open field.
    ///   Both nets simply via down, cross under the wall, and via back up — they
    ///   never touch the gap at all.
    /// * Widening or narrowing the gap does not fix that. Swept 0.6 to 2.9mm with
    ///   a both-layer wall (`examples/channel_sweep.rs`): both routers clear every
    ///   width. The cost model is Manhattan, so a great many routes tie and
    ///   nothing pushes two nets onto the same cell.
    ///
    /// So this proves both routers can get two nets past an obstacle, which is
    /// worth having. It proves nothing about negotiated congestion — see
    /// [`crossbar_board`] for that.
    fn corridor_board() -> (Vec<RouteNet>, RouteOptions) {
        // A wall of no-net pads with a single one-cell gap in it. Both signal
        // nets have to cross the wall.
        let mut nets = vec![
            RouteNet {
                net_idx: 1,
                name: "SIG_A".into(),
                pads: vec![pad("A1", "1", 100.0, 98.0), pad("A2", "1", 112.0, 98.0)],
            },
            RouteNet {
                net_idx: 2,
                name: "SIG_B".into(),
                pads: vec![pad("B1", "1", 100.0, 102.0), pad("B2", "1", 112.0, 102.0)],
            },
        ];
        // The wall: pads down the middle, leaving one gap at y = 100.
        //
        // ONE NET PER PAD. Collecting them into a single `net_idx: 99` with 12
        // pads made `pads.len() >= 2` true, so the routers classified the wall as
        // a net to be ROUTED and dutifully wired it together — 11 tracks of copper
        // down x=106 — instead of treating it as an obstacle. Compare
        // `no_net_pad_forces_traces_to_route_around_it`, which had it right.
        let mut y: f64 = 94.0;
        let mut idx = 90;
        while y <= 106.0 {
            if (y - 100.0).abs() > 0.5 {
                nets.push(RouteNet {
                    net_idx: idx,
                    name: String::new(),
                    pads: vec![pad("W", "1", 106.0, y)],
                });
                idx += 1;
            }
            y += 1.0;
        }
        let opts = RouteOptions {
            bounds: Some((96.0, 92.0, 116.0, 108.0)),
            ..Default::default()
        };
        (nets, opts)
    }

    /// Two signals crossing through one narrow gap in a wall. Both want the
    /// middle of the gap — their straight lines cross there — and the gap only
    /// admits two tracks if each hugs an edge. Routed greedily the first net takes
    /// the middle and leaves neither side wide enough; reordering cannot fix that,
    /// because whoever goes first takes the middle and every order fails alike.
    ///
    /// The pads are deliberately at four *distinct* points. An earlier version of
    /// this fixture put both nets\' pads at identical coordinates, which makes
    /// those cells `Blocked` — the board was unroutable by construction and the
    /// test was measuring nothing.
    ///
    /// The paragraph above is what this fixture was FOR. It does not achieve it,
    /// and the next sentence used to read: "if GridRouter ever passes it, the
    /// board has stopped being a discriminating case and the test is worthless."
    ///
    /// GridRouter passes it, with 0 conflicts. That condition — written down as
    /// fatal, in prose, with no assertion behind it — was true for months and
    /// nothing noticed, which is the whole lesson of `legion-of-bom-69v.1`.
    /// GridRouter is not the naive baseline this assumed: it does rip-up and
    /// reroute over net orderings, so a 2-net board is nearly always solvable by
    /// reordering.
    ///
    /// Kept as a second correctness board. The discriminating case is
    /// [`crossbar_board`], and the guard is now an `assert!`.
    fn channel_board() -> (Vec<RouteNet>, RouteOptions) {
        // Walls of through-hole pads (both layers, so there is no escape to the
        // back) with a 2.2mm gap between them at x = 106.
        let mut wall = Vec::new();
        let mut y: f64 = 90.0;
        while y <= 110.0 {
            if !(99.0..=101.2).contains(&y) {
                wall.push(pad_on("W", "1", 106.0, y, PadLayer::Both));
            }
            y += 0.8;
        }
        let nets = vec![
            RouteNet {
                net_idx: 1,
                name: "SIG_A".into(),
                pads: vec![
                    pad_on("A1", "1", 101.0, 96.0, PadLayer::Both),
                    pad_on("A2", "1", 111.0, 104.0, PadLayer::Both),
                ],
            },
            RouteNet {
                net_idx: 2,
                name: "SIG_B".into(),
                pads: vec![
                    pad_on("B1", "1", 101.0, 104.0, PadLayer::Both),
                    pad_on("B2", "1", 111.0, 96.0, PadLayer::Both),
                ],
            },
            RouteNet {
                net_idx: 99,
                name: "".into(),
                pads: wall,
            },
        ];
        let opts = RouteOptions {
            bounds: Some((99.0, 88.0, 113.0, 112.0)),
            ..Default::default()
        };
        (nets, opts)
    }

    /// **The board the ordering search cannot solve.** Four nets across an 8mm
    /// span, left pad `i` wired to right pad `3-i`, so every pair must cross.
    ///
    /// Found by measurement, not by design (`examples/channel_sweep.rs`), because
    /// two earlier attempts at a discriminating fixture were not:
    ///
    /// * A single narrow gap in a wall does not work at any width. Swept 0.6 to
    ///   2.9mm: both routers clear every one. The cost model is Manhattan, so a
    ///   great many paths tie and nothing pushes two nets onto the same cell —
    ///   there is no contention to negotiate.
    /// * Narrowing it until there IS contention goes straight from "both route"
    ///   to "impossible for both", with no window between.
    ///
    /// What defeats an order-based search is *diffuse* congestion: whoever commits
    /// first takes a path that boxes in a net it cannot single out to blame, and
    /// there is no permutation that avoids it. That is what a full crossing
    /// permutation produces, and it is what the module docs always claimed the
    /// advantage was for. Measured here: grid 2 conflicts, pathfinder 0, and 0
    /// impossible by placement.
    fn crossbar_board() -> (Vec<RouteNet>, RouteOptions) {
        const N: usize = 4;
        const PITCH: f64 = 1.27;
        const SPAN: f64 = 8.0;
        let (lx, rx) = (100.0, 100.0 + SPAN);
        let y0 = 100.0;
        let p = |refdes: String, x: f64, y: f64| PadPoint {
            refdes,
            pad: "1".into(),
            x_mm: x,
            y_mm: y,
            w_mm: 0.8,
            h_mm: 0.8,
            // Through-hole: no escaping to an empty back layer, which is what
            // made `corridor_board` trivial.
            layer: PadLayer::Both,
        };
        let nets = (0..N)
            .map(|i| RouteNet {
                net_idx: i + 1,
                name: format!("N{i}"),
                pads: vec![
                    p(format!("L{i}"), lx, y0 + i as f64 * PITCH),
                    p(format!("R{i}"), rx, y0 + (N - 1 - i) as f64 * PITCH),
                ],
            })
            .collect();
        let opts = RouteOptions {
            bounds: Some((
                lx - 1.0,
                y0 - 1.0,
                rx + 1.0,
                y0 + (N - 1) as f64 * PITCH + 1.0,
            )),
            ..Default::default()
        };
        (nets, opts)
    }

    /// **The claim, stated so it can fail.** Negotiated congestion routes a board
    /// that committing-in-order cannot — including with rip-up, which `GridRouter`
    /// already does.
    ///
    /// Written with its own preconditions asserted, because the previous version
    /// of this test passed with `PathfinderRouter` aliased to `GridRouter`, and
    /// seven of its eight siblings passed with the obstacle-blind `MstRouter`
    /// (`legion-of-bom-69v.1`). Two guards stop that recurring:
    ///
    /// * the board must be routable at all, or "pathfinder routed it" is luck;
    /// * `GridRouter` must actually FAIL it, or the fixture has stopped
    ///   discriminating and this test has quietly become a formality.
    ///
    /// `channel_board` carried the second requirement in prose — "if GridRouter
    /// ever passes it, the test is worthless" — and GridRouter passed it for
    /// months with nothing to notice. Prose is not an assertion.
    #[test]
    fn pathfinder_routes_a_board_the_ordering_search_cannot() {
        let (nets, opts) = crossbar_board();

        let impossible = unroutable_by_placement(&nets, &opts);
        assert!(
            impossible.is_empty(),
            "precondition: every net must be routable alone, else this proves nothing — {impossible:?}"
        );

        let grid = GridRouter.route(&nets, &opts).conflicts;
        assert!(
            !grid.is_empty(),
            "FIXTURE NO LONGER DISCRIMINATES: GridRouter now solves the crossbar, \
             so passing this test says nothing about negotiation. Make the board \
             harder (examples/channel_sweep.rs sweeps n/pitch/span) rather than \
             deleting this assertion."
        );

        let pf = PathfinderRouter::default().route(&nets, &opts).conflicts;
        assert!(
            pf.is_empty(),
            "negotiation left {} unrouted where the ordering search left {} — {pf:?}",
            pf.len(),
            grid.len()
        );
    }

    /// **Never worse than the baseline.** `PathfinderRouter` falls back to the
    /// same ordering search `GridRouter` runs, and keeps the negotiation-guided
    /// result only when it actually beat the unguided one — so it cannot lose to
    /// the plain router on any board.
    ///
    /// This is a real regression guard, not a formality: an earlier version
    /// steered the fallback with a history map from a negotiation that never
    /// settled, and lost 0 conflicts to 1 on the channel board below.
    #[test]
    fn pathfinder_is_never_worse_than_the_grid_router() {
        // The crossbar is here because it is the only one of the three where
        // GridRouter leaves any conflicts at all. On the other two the comparison
        // is 0 <= 0 and holds for any router whatsoever — including one that
        // emits nothing. A "never worse" test needs at least one board where
        // "worse" is expressible.
        let mut grid_ever_failed = false;
        for (name, (nets, opts)) in [
            ("crossbar", crossbar_board()),
            ("corridor", corridor_board()),
            ("channel", channel_board()),
        ] {
            let grid = GridRouter.route(&nets, &opts);
            let pf = PathfinderRouter::default().route(&nets, &opts);
            grid_ever_failed |= !grid.conflicts.is_empty();
            assert!(
                pf.conflicts.len() <= grid.conflicts.len(),
                "{name}: pathfinder {:?} lost to grid {:?}",
                pf.conflicts,
                grid.conflicts
            );
        }
        assert!(
            grid_ever_failed,
            "every board here is trivially routable, so 'never worse' compared \
             0 <= 0 throughout and proved nothing"
        );
    }

    /// Two signals through one gap, both routed, both with copper on the board.
    ///
    /// A CORRECTNESS test — renamed from `pathfinder_clears_a_contested_corridor`,
    /// which oversold it. Nothing here is contested: measured, GridRouter clears
    /// this board too, and so does a single negotiation round. The claim about
    /// negotiation beating an ordering search lives in
    /// `pathfinder_routes_a_board_the_ordering_search_cannot`, which asserts its
    /// own preconditions so it cannot quietly stop being true.
    #[test]
    fn pathfinder_routes_both_signals_through_one_gap() {
        let (nets, opts) = corridor_board();
        let grid = GridRouter.route(&nets, &opts);
        let pf = PathfinderRouter::default().route(&nets, &opts);
        // Negotiation must never do worse than committing in order.
        assert!(
            pf.conflicts.len() <= grid.conflicts.len(),
            "pathfinder {:?} vs grid {:?}",
            pf.conflicts,
            grid.conflicts
        );
        assert!(
            pf.conflicts.is_empty(),
            "both signals should route once the corridor is priced: {:?}",
            pf.conflicts
        );
        for idx in [1, 2] {
            assert!(
                pf.tracks.iter().any(|t| t.net_idx == idx),
                "net {idx} produced no copper"
            );
        }
    }

    /// Emitted copper must never be knowingly shorted: a net that still clashes
    /// after the last round is reported, not drawn.
    ///
    /// On the crossbar with a single round, because that is a board where one
    /// round genuinely fails. This used to run on `corridor_board` under the
    /// premise "one round is not enough to negotiate anything" — measured, that
    /// premise is false: `max_iters: 1` clears the corridor with 0 conflicts, so
    /// the test never saw a failing net at all (`legion-of-bom-69v.1`).
    #[test]
    fn pathfinder_reports_rather_than_emitting_shorted_copper() {
        let (nets, opts) = crossbar_board();
        let out = PathfinderRouter { max_iters: 1 }.route(&nets, &opts);
        let routed: HashSet<usize> = out.tracks.iter().map(|t| t.net_idx).collect();
        let complained: HashSet<usize> = out
            .conflicts
            .iter()
            .filter_map(|c| c.split_whitespace().nth(1))
            .filter_map(|n| n.parse().ok())
            .collect();
        // Without this the loop below is satisfied by a router that simply routes
        // everything, which is not what this test is about.
        assert!(
            !complained.is_empty(),
            "one round was expected to leave something unrouted; if the router got \
             better, move this to a harder board rather than deleting the check"
        );
        // Every signal net either shipped copper or was reported — never silently
        // dropped. A net that was reported must NOT also have copper on the board:
        // that is the knowingly-shorted case this exists to prevent.
        for net in &nets {
            let idx = net.net_idx;
            assert!(
                routed.contains(&idx) || complained.contains(&idx),
                "net {idx} vanished: tracks {routed:?} conflicts {complained:?}"
            );
            assert!(
                !(routed.contains(&idx) && complained.contains(&idx)),
                "net {idx} was reported as unroutable AND had copper emitted"
            );
        }
    }

    /// **The router's actual contract.** If a placement is routable at all — every
    /// connection reachable with the board to itself — then the router must route
    /// it. Anything left over is the router's fault, not the placement's.
    ///
    /// This is the property that the oscillation bug broke and that no test
    /// caught: with the present-congestion factor saturating ~300,000x above
    /// history, negotiation thrashed for 24 rounds and shipped a round that was
    /// not its best. It passed every test there was, because none of them asked
    /// whether the router had achieved what the placement allowed.
    #[test]
    fn pathfinder_routes_everything_the_placement_allows() {
        // The crossbar makes this bound non-trivial: it is the one board here the
        // ordering search fails, so `got <= impossible` is `0 <= 0` only if the
        // router really did achieve what the placement allowed. On the other two
        // it is 0 <= 0 for any router at all.
        for (name, (nets, opts)) in [
            ("crossbar", crossbar_board()),
            ("corridor", corridor_board()),
            ("channel", channel_board()),
        ] {
            let impossible = unroutable_by_placement(&nets, &opts).len();
            let got = PathfinderRouter::default()
                .route(&nets, &opts)
                .conflicts
                .len();
            assert!(
                got <= impossible,
                "{name}: {got} unrouted but only {impossible} are impossible for \
                 this placement — the difference is the router giving up"
            );
        }
    }

    /// The board pipeline must route with the router the benches measure.
    ///
    /// `legion-of-bom-kcq` recorded "PathfinderRouter returns byte-identical
    /// results to GridRouter on a 46-part board" and built a theory on it — the
    /// negotiation never converging, the never-worse guard handing back the
    /// baseline every time. None of that was true. `route_bench` defaulted to
    /// `max_iters: 24` while `board.rs` builds with `PathfinderRouter::default()`
    /// (160), so the bench had measured a router the project does not ship. At
    /// 160 the two separate cleanly: 14 unrouted vs 16, 14 DRC vs 15.
    ///
    /// A day of reasoning about a number that came from the wrong binary. This
    /// asserts the default is what the pipeline uses, so a bench that wants a
    /// different count has to say so out loud.
    /// Deliberately a change-detector — that is the job. It is not asserting 600
    /// is correct; it is asserting that nobody moves the number without seeing
    /// this message, because every routing figure recorded in a bead was measured
    /// against one specific value. It has already earned its keep once: it failed
    /// the moment I raised 160 to 600, which is exactly when the old numbers
    /// stopped meaning anything.
    #[test]
    fn the_shipping_router_is_the_default_one() {
        assert_eq!(
            PathfinderRouter::default().max_iters,
            600,
            "board.rs builds with PathfinderRouter::default(); if this number \
             moves, every recorded routing measurement was taken against a \
             different router and needs re-running"
        );
    }

    #[test]
    fn pathfinder_is_deterministic() {
        let (nets, opts) = corridor_board();
        let a = PathfinderRouter::default().route(&nets, &opts);
        let b = PathfinderRouter::default().route(&nets, &opts);
        assert_eq!(a.tracks.len(), b.tracks.len());
        assert_eq!(a.conflicts, b.conflicts);
        for (x, y) in a.tracks.iter().zip(b.tracks.iter()) {
            assert_eq!((x.start, x.end, x.net_idx), (y.start, y.end, y.net_idx));
        }
    }

    #[test]
    fn pathfinder_single_pad_net_routes_nothing() {
        let net = RouteNet {
            net_idx: 1,
            name: "NC".into(),
            pads: vec![pad("R1", "1", 100.0, 100.0)],
        };
        let out = PathfinderRouter::default().route(&[net], &RouteOptions::default());
        assert!(out.tracks.is_empty());
        assert!(out.conflicts.is_empty());
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
        // edge-clearance inset inside it. This fixture has a vertical wall whose
        // only crossing is the bottom edge band: no edge clearance can use it,
        // but KiCad's default clearance must refuse it.
        let route_with_edge_clearance = |edge_clearance_mm| {
            let opts = RouteOptions {
                bounds: Some((0.0, 0.0, 20.0, 10.0)),
                edge_clearance_mm,
                ..RouteOptions::default()
            };
            let mut nets = vec![RouteNet {
                net_idx: 1,
                name: "SIG".into(),
                pads: vec![pad("A", "1", 5.0, 0.6), pad("B", "1", 15.0, 0.6)],
            }];
            // A vertical wall at x = 10 whose lowest pad's copper ends at
            // y = 1.0, so a track must pass with its centre at y <= 0.675. The
            // path at y ~= 0.6 is deliberately available only when the router
            // may use the edge band (with 0.5mm edge clearance the grid starts
            // at y = 1.0).
            for k in 0..=8 {
                nets.push(RouteNet {
                    net_idx: 0,
                    name: String::new(),
                    pads: vec![pad_on("W", "1", 10.0, 1.5 + k as f64, PadLayer::Both)],
                });
            }
            GridRouter.route(&nets, &opts)
        };

        let unguarded = route_with_edge_clearance(0.0);
        assert!(
            unguarded.conflicts.is_empty() && unguarded.tracks.iter().any(|t| t.net_idx == 1),
            "fixture must be routable before edge clearance is applied: {unguarded:?}"
        );

        let out = route_with_edge_clearance(0.5);
        assert!(
            !out.conflicts.is_empty(),
            "a net routable only through the edge band must be surfaced, not routed into it"
        );
        assert!(
            out.tracks.iter().all(|t| t.net_idx != 1),
            "the boxed-in net must not be routed into the edge band"
        );
    }

    // ---- fine-pitch escape (legion-of-bom-y17.1) ---------------------------

    /// Distance from point `p` to segment `a`-`b`.
    fn pt_seg(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = dx * dx + dy * dy;
        let t = if len2 == 0.0 {
            0.0
        } else {
            (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
        };
        let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
        ((p.0 - qx).powi(2) + (p.1 - qy).powi(2)).sqrt()
    }

    fn segs_cross(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
        let orient = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| {
            (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
        };
        let (o1, o2) = (orient(a, b, c), orient(a, b, d));
        let (o3, o4) = (orient(c, d, a), orient(c, d, b));
        o1 * o2 < 0.0 && o3 * o4 < 0.0
    }

    fn seg_seg(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> f64 {
        if segs_cross(a, b, c, d) {
            return 0.0;
        }
        pt_seg(a, c, d)
            .min(pt_seg(b, c, d))
            .min(pt_seg(c, a, b))
            .min(pt_seg(d, a, b))
    }

    /// Distance from segment `a`-`b` to a pad's rectangle (0 if they touch).
    fn seg_pad(a: (f64, f64), b: (f64, f64), p: &PadPoint) -> f64 {
        let (x0, y0) = (p.x_mm - p.w_mm / 2.0, p.y_mm - p.h_mm / 2.0);
        let (x1, y1) = (p.x_mm + p.w_mm / 2.0, p.y_mm + p.h_mm / 2.0);
        let inside = |q: (f64, f64)| q.0 >= x0 && q.0 <= x1 && q.1 >= y0 && q.1 <= y1;
        if inside(a) || inside(b) {
            return 0.0;
        }
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
        (0..4)
            .map(|i| seg_seg(a, b, corners[i], corners[(i + 1) % 4]))
            .fold(f64::INFINITY, f64::min)
    }

    /// The test oracle the router is judged by: every pair of copper items on
    /// different nets, measured edge to edge in exact geometry — not on the
    /// router's own grid, which is the thing under test. Returns one line per
    /// violation (empty = clean).
    fn clearance_violations(nets: &[RouteNet], out: &RouteOutput, clr: f64) -> Vec<String> {
        const TOL: f64 = 1e-6;
        let layer_of = |name: &str| if name == "F.Cu" { FRONT } else { BACK };
        let mut bad = Vec::new();
        for (i, t) in out.tracks.iter().enumerate() {
            let l = layer_of(&t.layer);
            for u in &out.tracks[i + 1..] {
                if u.net_idx == t.net_idx || layer_of(&u.layer) != l {
                    continue;
                }
                let gap = seg_seg(t.start, t.end, u.start, u.end) - (t.width_mm + u.width_mm) / 2.0;
                if gap < clr - TOL {
                    bad.push(format!(
                        "track {} / track {}: {gap:.4}",
                        t.net_idx, u.net_idx
                    ));
                }
            }
            for net in nets.iter().filter(|n| n.net_idx != t.net_idx) {
                for p in net.pads.iter().filter(|p| p.layer.on(l)) {
                    let gap = seg_pad(t.start, t.end, p) - t.width_mm / 2.0;
                    if gap < clr - TOL {
                        bad.push(format!(
                            "track {} / pad {}.{}: {gap:.4}",
                            t.net_idx, p.refdes, p.pad
                        ));
                    }
                }
            }
            for v in out.vias.iter().filter(|v| v.net_idx != t.net_idx) {
                let gap = pt_seg(v.at, t.start, t.end) - (t.width_mm + v.size_mm) / 2.0;
                if gap < clr - TOL {
                    bad.push(format!("track {} / via {}: {gap:.4}", t.net_idx, v.net_idx));
                }
            }
        }
        for v in &out.vias {
            for net in nets.iter().filter(|n| n.net_idx != v.net_idx) {
                for p in &net.pads {
                    let gap = seg_pad(v.at, v.at, p) - v.size_mm / 2.0;
                    if gap < clr - TOL {
                        bad.push(format!(
                            "via {} / pad {}.{}: {gap:.4}",
                            v.net_idx, p.refdes, p.pad
                        ));
                    }
                }
            }
        }
        bad
    }

    /// A 7x7mm LQFP-48 at 0.5mm pitch, pads exactly as KiCad's
    /// `Package_QFP:LQFP-48_7x7mm_P0.5mm` places them (1.475 x 0.3mm), centred
    /// at `(cx, cy)`. Pads `1..=48` in KiCad order: left side top-down, then
    /// bottom, right, top.
    fn lqfp48(cx: f64, cy: f64) -> Vec<PadPoint> {
        let (row, len, wid) = (4.1625, 1.475, 0.3);
        (0..48)
            .map(|i| {
                let k = (i % 12) as f64 * 0.5 - 2.75;
                let (x, y, w, h) = match i / 12 {
                    0 => (-row, k, len, wid),
                    1 => (k, row, wid, len),
                    2 => (row, -k, len, wid),
                    _ => (-k, -row, wid, len),
                };
                PadPoint {
                    refdes: "U1".into(),
                    pad: (i + 1).to_string(),
                    x_mm: cx + x,
                    y_mm: cy + y,
                    w_mm: w,
                    h_mm: h,
                    layer: PadLayer::Front,
                }
            })
            .collect()
    }

    /// The LQFP's bottom and left rows each fanned out to a row of 1mm test
    /// pads 5mm away at 1.27mm pitch; every other LQFP pad is a distinct,
    /// unrouted net, so it is copper the fan-out must keep clear of — exactly
    /// the situation of an MCU whose unused pins sit between the used ones.
    fn lqfp_fanout_board() -> (Vec<RouteNet>, RouteOptions) {
        let (cx, cy) = (15.0, 15.0);
        let mut nets = Vec::new();
        for (i, p) in lqfp48(cx, cy).into_iter().enumerate() {
            let side = i / 12;
            let k = (i % 12) as f64 * 1.27 - 5.5 * 1.27;
            let mut pads = vec![p];
            match side {
                0 => pads.push(pad("TP", &format!("L{i}"), cx - 10.0, cy + k)),
                1 => pads.push(pad("TP", &format!("B{i}"), cx + k, cy + 10.0)),
                _ => {}
            }
            nets.push(RouteNet {
                net_idx: i + 1,
                name: format!("N{}", i + 1),
                pads,
            });
        }
        let opts = RouteOptions {
            bounds: Some((0.0, 0.0, 30.0, 30.0)),
            ..RouteOptions::default()
        };
        (nets, opts)
    }

    #[test]
    fn the_clearance_oracle_can_fail() {
        // Two parallel tracks 0.3mm apart centre to centre with 0.25mm width
        // leave 0.05mm of air: the oracle must say so, or every "clean" below
        // is vacuous.
        let track = |y: f64, net_idx| Track {
            start: (0.0, y),
            end: (5.0, y),
            width_mm: 0.25,
            layer: "F.Cu".into(),
            net_idx,
        };
        let out = RouteOutput {
            tracks: vec![track(0.0, 1), track(0.3, 2)],
            ..RouteOutput::default()
        };
        assert_eq!(clearance_violations(&[], &out, 0.2).len(), 1);
        let out = RouteOutput {
            tracks: vec![track(0.0, 1), track(0.45, 2)],
            ..RouteOutput::default()
        };
        assert!(
            clearance_violations(&[], &out, 0.2).is_empty(),
            "exactly at clearance is legal"
        );
    }

    /// An MCU's 0.5mm-pitch pins have to be escapable at all before any MCU
    /// board can route: at 0.25mm track / 0.2mm clearance there is 0.025mm of
    /// slack either side of each pin's axis, so this is a test of whether the
    /// grid measures clearance honestly, not of routing cleverness.
    #[test]
    fn a_plane_net_drops_each_pad_to_a_via_instead_of_joining_them() {
        // Two SMD ground pads 10mm apart, and a signal passing between them.
        let smd = |r: &str, x: f64, y: f64| PadPoint {
            w_mm: 0.8,
            h_mm: 0.9,
            ..pad(r, "2", x, y)
        };
        let nets = vec![
            RouteNet {
                net_idx: 1,
                name: "GND".into(),
                pads: vec![smd("C1", 5.0, 5.0), smd("C2", 15.0, 5.0)],
            },
            RouteNet {
                net_idx: 2,
                name: "SIG".into(),
                pads: vec![pad("A", "1", 10.0, 1.5), pad("B", "1", 10.0, 8.5)],
            },
        ];
        for router in [
            &PathfinderRouter::default() as &dyn Router,
            &GridRouter as &dyn Router,
        ] {
            let opts = RouteOptions {
                bounds: Some((0.0, 0.0, 20.0, 10.0)),
                plane_net: Some(1),
                ..RouteOptions::default()
            };
            let out = router.route(&nets, &opts);
            assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
            let gnd_vias: Vec<&Via> = out.vias.iter().filter(|v| v.net_idx == 1).collect();
            assert_eq!(gnd_vias.len(), 2, "one via per ground pad: {:?}", out.vias);
            let gnd_copper: f64 = out
                .tracks
                .iter()
                .filter(|t| t.net_idx == 1)
                .map(|t| (t.end.0 - t.start.0).hypot(t.end.1 - t.start.1))
                .sum();
            assert!(
                gnd_copper < 4.0,
                "ground is stubs to vias, not a track across the board ({gnd_copper:.1}mm)"
            );
            for v in gnd_vias {
                for p in &nets[0].pads {
                    let inside = (v.at.0 - p.x_mm).abs() < p.w_mm / 2.0 + v.size_mm / 2.0
                        && (v.at.1 - p.y_mm).abs() < p.h_mm / 2.0 + v.size_mm / 2.0;
                    assert!(!inside, "a via in pad {}.{} would wick its solder", p.refdes, p.pad);
                }
            }
            let bad = clearance_violations(&nets, &out, opts.clearance_mm);
            assert!(bad.is_empty(), "{bad:#?}");
        }
    }

    #[test]
    fn pathfinder_escapes_a_half_millimetre_pitch_qfp_drc_clean() {
        let (nets, opts) = lqfp_fanout_board();
        let out = PathfinderRouter::default().route(&nets, &opts);
        assert!(out.conflicts.is_empty(), "unrouted: {:#?}", out.conflicts);
        let bad = clearance_violations(&nets, &out, opts.clearance_mm);
        assert!(bad.is_empty(), "clearance violations: {bad:#?}");
    }
}
