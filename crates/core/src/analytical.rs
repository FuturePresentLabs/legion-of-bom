//! Analytical global placement — stage one, solved instead of guessed.
//!
//! # Why this replaced a greedy placer
//!
//! [`SeededPlacer`](crate::board::SeededPlacer) used to seed each free part at
//! the weighted centroid of the parts it was *already* netted to, one part at a
//! time, committing as it went. That cannot trade one constraint against
//! another: by the time the second constraint is visible the first is already
//! spent. Everything built on top — a bigger decoupling pull, then a snap pass
//! to undo where the pull landed things, then repair nudges to shake the result
//! — was compensating for the commit order rather than for any missing rule.
//! The attempt to add one more pull (a feedback-network attractor) went 5 DRC
//! errors to 7 and broke a critical net, which is what a saturated heuristic
//! looks like (`legion-of-bom-yck`).
//!
//! # What this does instead
//!
//! Minimise the weighted sum of squared distances over every attractor edge
//!
//! ```text
//! Φ(p) = Σ w_ij · ‖p_i − p_j‖²
//! ```
//!
//! with the panel-anchored parts held fixed. Φ is convex and separable in x and
//! y, so setting ∇Φ = 0 gives two sparse symmetric-positive-definite linear
//! systems `L·x = bx`, `L·y = by`, where `L` is the weighted graph Laplacian of
//! the movable parts and the right-hand side carries the fixed parts. Solving
//! them answers *every* constraint at once: there is no placement order to
//! paint yourself into a corner with, and the squared term punishes long
//! **high-weight** edges hardest — which is exactly the bypass-cap and
//! feedback-network cases, with no special case for either.
//!
//! This is the classical quadratic-placement formulation (GORDIAN, Kleinhans et
//! al. 1991; Kraftwerk, Eisenmann & Johannes 1998), and stage one of the three
//! stages — global placement, legalization, fine-tuning — that the PCB placement
//! literature uses (`legion-of-bom-957`). Stage two is [`crate::legalize`] and
//! stage three is the [`crate::layout`] loop; **both stay**. Analytical
//! placement deliberately produces overlaps: the objective has no idea parts
//! have size, so the answer is where each part *wants* to be, and something
//! downstream has to make room.
//!
//! # Why one solve is not enough
//!
//! The objective has no idea parts have size, so its optimum is *collapsed*.
//! Measured on the 8 HP slew limiter: a single solve asks for every free part
//! inside a 1.7 × 44 mm sliver of a 40 × 128 mm board, and the packing that
//! removes those overlaps then has to move each part 18 mm on average. At that
//! distance the solve's answer has stopped being information.
//!
//! So this iterates, the way modern analytical placers do (SimPL, Kim, Lee &
//! Markov, ICCAD 2012, and its ComPLx/RePlAce descendants): solve for the
//! collapsed optimum (the *lower bound*), legalize it into a placement with no
//! overlaps (the *upper bound*), then re-solve with each part tied by a
//! **pseudo-anchor** to where legalization actually put it
//! ([`solve_spread`]). Tightening those anchors each round walks the two bounds
//! together — the placement spreads out without losing the relative arrangement
//! the wirelength objective wanted. Here the upper-bound step is the packer in
//! [`SeededPlacer::place`](crate::board::SeededPlacer), which already knows
//! about courtyards, board sides and part heights.

use std::collections::HashMap;

use crate::source::CircuitSource;

/// How much harder a `critical()`-tagged net pulls its parts together than an
/// ordinary 2-pin net.
const CRITICAL_PULL: f64 = 6.0;

/// How hard a decoupling cap is bonded to the IC it decouples: worth more than
/// an ordinary signal net, less than a net its author tagged `critical()`.
///
/// It was 12.0 under the greedy placer, where it was only ever a *hint* — it
/// broke ties in a placement order and biased one centroid. A solver obeys it,
/// and at 12.0 it obeys it at everyone else's expense: three bypass caps
/// outweigh the entire signal path, the ICs get dragged onto their caps, and the
/// board came out with 4 unrouted nets and 6 DRC errors. Swept on the 8 HP slew
/// limiter, 2.5–3.5 all route clean in a single pass; 2.0 and below loses the
/// short loop, 4.0 and above resumes distorting the signal path. 3.0 is the
/// middle of that band and the defensible sentence: a power loop beats a signal
/// trace, but it is not a different kind of thing.
const DECOUPLE_PULL: f64 = 3.0;

/// Relative residual at which the solve is called converged.
const CG_TOL: f64 = 1e-10;

/// Every attractor edge a circuit implies: `(a, b, weight)`, each unordered pair
/// listed once per reason it exists, since weights on the same pair add.
///
/// Two sources, both derived from topology so neither needs a DSL annotation:
///
/// * **Nets.** The clique model — every pair of parts on a net gets an edge at
///   `1/(k−1)` for a `k`-part net, the standard weighting that keeps a net's
///   total pull comparable to its half-perimeter length. A 2-pin signal net
///   pulls at 1.0 while a rail that touches everything (and so says nothing
///   about where anything wants to be) barely pulls at all. `critical()` nets
///   pull [`CRITICAL_PULL`]× harder.
/// * **Decoupling.** Each bypass cap is bonded to the IC it decouples at
///   [`DECOUPLE_PULL`] — see [`decoupling_pairs`](crate::board::decoupling_pairs).
pub fn attractors(circuit: &dyn CircuitSource) -> Vec<(String, String, f64)> {
    let mut edges: Vec<(String, String, f64)> = Vec::new();
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
        let mut w = 1.0 / (refs.len() as f64 - 1.0);
        if net.is_critical() {
            w *= CRITICAL_PULL;
        }
        for (i, &a) in refs.iter().enumerate() {
            for &b in &refs[i + 1..] {
                edges.push((a.to_string(), b.to_string(), w));
            }
        }
    }
    for (cap, ic) in crate::board::decoupling_pairs(circuit) {
        edges.push((cap, ic, DECOUPLE_PULL));
    }
    edges
}

/// Total attractor weight per part — how strongly each one is tied to anything
/// at all.
///
/// The packing that removes the overlaps this stage leaves behind has to visit
/// parts in *some* order, and whoever goes first gets their exact answer while
/// later parts are displaced. Sorting by this puts the most constrained parts
/// first, so the displacement lands on the parts that care least.
pub fn pull_totals(edges: &[(String, String, f64)]) -> HashMap<String, f64> {
    let mut totals: HashMap<String, f64> = HashMap::new();
    for (a, b, w) in edges {
        *totals.entry(a.clone()).or_default() += w;
        *totals.entry(b.clone()).or_default() += w;
    }
    totals
}

/// Solve for the position of every `movable` part, given the `fixed` ones.
///
/// `edges` are the attractors ([`attractors`]); any naming a part that is
/// neither movable nor fixed is ignored.
///
/// A part only has a solvable position if some chain of attractors reaches
/// something fixed — otherwise the objective is translation-invariant and every
/// position on that island is equally optimal. Those parts are answered from
/// `homes` rather than by tethering the whole system to a point, which would
/// bias every *anchored* part to buy an answer for the few that float.
///
/// `homes` therefore has to be a real arrangement, not one point repeated: a
/// board with nothing anchored at all (no panel, no power header) is *entirely*
/// this fallback, and answering it with "everything at the centre" packs the
/// parts into a knot the router cannot get through. Pass an even spread over the
/// board. A part missing from `homes` falls back to the centroid of `fixed`, or
/// the origin if nothing is fixed either.
///
/// The result is inside the convex hull of the fixed positions: each row of the
/// solved system states that a part sits at the weighted average of its
/// neighbours, so nothing can be pulled outside everything that pulls it. It is
/// *not* legal — parts overlap, and sizes were never part of the question.
pub fn solve(
    movable: &[String],
    fixed: &HashMap<String, (f64, f64)>,
    edges: &[(String, String, f64)],
    homes: &HashMap<String, (f64, f64)>,
) -> HashMap<String, (f64, f64)> {
    solve_spread(movable, fixed, edges, homes, &HashMap::new(), 0.0)
}

/// [`solve`] with **pseudo-anchors**: each part in `at` is additionally pulled
/// toward that position, at `strength` × its own total attractor weight.
///
/// This is the spreading half of the loop in the module docs. `at` is the last
/// legalized placement, so the pseudo-anchor says "you fit here" while the real
/// edges keep saying "you belong there", and `strength` sets who wins. Ramping
/// it from well below 1 to well above walks a collapsed optimum out to a legal
/// placement that still honours the wirelength it started from.
///
/// Making the anchor proportional to a part's own connectivity is what keeps
/// the ramp meaningful across parts: at `strength = 0.05` every part, whether it
/// sits on twenty nets or two, is held at five percent of what its own circuit
/// is asking for.
pub fn solve_spread(
    movable: &[String],
    fixed: &HashMap<String, (f64, f64)>,
    edges: &[(String, String, f64)],
    homes: &HashMap<String, (f64, f64)>,
    at: &HashMap<String, (f64, f64)>,
    strength: f64,
) -> HashMap<String, (f64, f64)> {
    if movable.is_empty() {
        return HashMap::new();
    }
    // Where a part goes when the circuit cannot place it.
    let fallback = {
        let n = fixed.len().max(1) as f64;
        let sum = fixed
            .values()
            .fold((0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1));
        (sum.0 / n, sum.1 / n)
    };
    let home_of = |r: &str| homes.get(r).copied().unwrap_or(fallback);

    let index: HashMap<&str, usize> = movable
        .iter()
        .enumerate()
        .map(|(i, r)| (r.as_str(), i))
        .collect();
    let n = movable.len();

    // Row form of the Laplacian: `diag[i]` is Σ of every weight touching part i,
    // and `off[i]` holds `(column, weight)` for its movable neighbours — the
    // matrix entry there being *minus* the weight. An edge to a fixed part
    // contributes to the diagonal and to the right-hand side instead, which is
    // what makes the system non-singular and pins the whole placement to the
    // panel.
    let mut diag = vec![0.0f64; n];
    let mut off: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut bx = vec![0.0f64; n];
    let mut by = vec![0.0f64; n];
    let mut anchored = vec![false; n];
    for (a, b, w) in edges {
        // A non-finite or non-positive weight would make the matrix indefinite —
        // a self-edge just contributes nothing.
        if !w.is_finite() || *w <= 0.0 || a == b {
            continue;
        }
        // The movable end of an edge, and the other end if it is fixed.
        let (i, other) = match (index.get(a.as_str()), index.get(b.as_str())) {
            (Some(&i), Some(&j)) => {
                diag[i] += w;
                diag[j] += w;
                off[i].push((j, *w));
                off[j].push((i, *w));
                continue;
            }
            (Some(&i), None) => (i, b),
            (None, Some(&j)) => (j, a),
            (None, None) => continue,
        };
        if let Some(&(x, y)) = fixed.get(other.as_str()) {
            diag[i] += w;
            bx[i] += w * x;
            by[i] += w * y;
            anchored[i] = true;
        }
    }

    // Pseudo-anchors, proportional to what each part's own circuit already pulls
    // with. `diag[i]` is exactly that total at this point.
    if strength > 0.0 {
        for (i, r) in movable.iter().enumerate() {
            let Some(&(x, y)) = at.get(r.as_str()) else {
                continue;
            };
            let s = strength * diag[i].max(1.0);
            diag[i] += s;
            bx[i] += s * x;
            by[i] += s * y;
            anchored[i] = true;
        }
    }

    // Which parts the panel actually reaches. A component with no fixed part in
    // it is singular; one with a fixed part anywhere in it is positive definite,
    // and since the frontier below never crosses out of a component, the two
    // sets never share an edge — so the reachable ones form a system on their
    // own.
    let mut grounded = anchored.clone();
    let mut frontier: Vec<usize> = (0..n).filter(|&i| grounded[i]).collect();
    while let Some(i) = frontier.pop() {
        for &(j, _) in &off[i] {
            if !grounded[j] {
                grounded[j] = true;
                frontier.push(j);
            }
        }
    }

    // Compact the grounded rows into their own system, so the solver never sees
    // a singular one.
    let rows: Vec<usize> = (0..n).filter(|&i| grounded[i]).collect();
    let slot: HashMap<usize, usize> = rows.iter().enumerate().map(|(k, &i)| (i, k)).collect();
    let sub_diag: Vec<f64> = rows.iter().map(|&i| diag[i]).collect();
    let sub_off: Vec<Vec<(usize, f64)>> = rows
        .iter()
        .map(|&i| off[i].iter().map(|&(j, w)| (slot[&j], w)).collect())
        .collect();
    let sub_bx: Vec<f64> = rows.iter().map(|&i| bx[i]).collect();
    let sub_by: Vec<f64> = rows.iter().map(|&i| by[i]).collect();
    // Start each row from its own home — a guess that is already roughly right
    // costs the solver fewer iterations than one point repeated.
    let start: Vec<(f64, f64)> = rows.iter().map(|&i| home_of(&movable[i])).collect();

    let x = conjugate_gradient(&sub_diag, &sub_off, &sub_bx, &start, |p| p.0);
    let y = conjugate_gradient(&sub_diag, &sub_off, &sub_by, &start, |p| p.1);

    let mut out: HashMap<String, (f64, f64)> =
        movable.iter().map(|r| (r.clone(), home_of(r))).collect();
    for (k, &i) in rows.iter().enumerate() {
        out.insert(movable[i].clone(), (x[k], y[k]));
    }
    out
}

/// Weighted attractor length of a placement, in millimetres — the objective as
/// a *distance* rather than a squared one.
///
/// The spreading loop needs to compare rounds, and squared distance is the wrong
/// ruler for that: it is what makes the solve tractable, not what the board
/// costs. Copper is linear, so this is. Edges naming a part with no position
/// (an unplaced part, or an unrelated circuit's) are skipped.
pub fn wirelength(edges: &[(String, String, f64)], at: &HashMap<String, (f64, f64)>) -> f64 {
    edges
        .iter()
        .filter_map(|(a, b, w)| {
            let (pa, pb) = (at.get(a.as_str())?, at.get(b.as_str())?);
            Some(w * (pa.0 - pb.0).hypot(pa.1 - pb.1))
        })
        .sum()
}

/// Jacobi-preconditioned conjugate gradient on the symmetric positive-definite
/// system in row form, starting from each part's `start` point as read by `axis`.
///
/// CG rather than a factorisation because the matrix is sparse and we only ever
/// need the solution, not the inverse; preconditioned because the weights span
/// two orders of magnitude (a rail edge at 1/20 against a decoupling bond) and
/// dividing by the diagonal takes most of that spread out. Iterations are capped
/// well above the `n` steps CG needs in exact arithmetic — the cap is a backstop
/// against a pathological matrix, not the expected exit.
fn conjugate_gradient(
    diag: &[f64],
    off: &[Vec<(usize, f64)>],
    b: &[f64],
    start: &[(f64, f64)],
    axis: impl Fn(&(f64, f64)) -> f64,
) -> Vec<f64> {
    let n = diag.len();
    let dot = |u: &[f64], v: &[f64]| -> f64 { (0..n).map(|i| u[i] * v[i]).sum() };
    let mul = |v: &[f64]| -> Vec<f64> {
        (0..n)
            .map(|i| diag[i] * v[i] - off[i].iter().map(|&(j, w)| w * v[j]).sum::<f64>())
            .collect()
    };

    let mut x: Vec<f64> = start.iter().map(&axis).collect();
    let ax = mul(&x);
    let mut r: Vec<f64> = (0..n).map(|i| b[i] - ax[i]).collect();
    let mut z: Vec<f64> = (0..n).map(|i| r[i] / diag[i]).collect();
    let mut p = z.clone();
    let mut rz = dot(&r, &z);
    // Scale the residual target by the right-hand side, so the tolerance means
    // the same thing on a 40 mm board as on a 400 mm one.
    let target = CG_TOL * dot(b, b).sqrt().max(1.0);

    for _ in 0..(4 * n + 50) {
        if dot(&r, &r).sqrt() <= target || rz == 0.0 || !rz.is_finite() {
            break;
        }
        let ap = mul(&p);
        let denom = dot(&p, &ap);
        if denom <= 0.0 {
            break;
        }
        let alpha = rz / denom;
        for i in 0..n {
            x[i] += alpha * p[i];
            r[i] -= alpha * ap[i];
            z[i] = r[i] / diag[i];
        }
        let rz_next = dot(&r, &z);
        let beta = rz_next / rz;
        for i in 0..n {
            p[i] = z[i] + beta * p[i];
        }
        rz = rz_next;
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef, RefDes};

    fn fixed(pts: &[(&str, f64, f64)]) -> HashMap<String, (f64, f64)> {
        pts.iter()
            .map(|(r, x, y)| (r.to_string(), (*x, *y)))
            .collect()
    }

    /// One home point for every part in a test — the callers that matter pass an
    /// even spread, but a test with one island only needs one point.
    fn home(x: f64, y: f64) -> HashMap<String, (f64, f64)> {
        ["A", "B", "C", "D", "C1", "R1", "R2", "R9"]
            .iter()
            .map(|r| (r.to_string(), (x, y)))
            .collect()
    }

    fn edge(a: &str, b: &str, w: f64) -> (String, String, f64) {
        (a.to_string(), b.to_string(), w)
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn a_part_between_two_anchors_lands_at_the_weighted_average() {
        let p = solve(
            &["C1".to_string()],
            &fixed(&[("L", 0.0, 0.0), ("R", 10.0, 0.0)]),
            &[edge("C1", "L", 1.0), edge("C1", "R", 3.0)],
            &home(5.0, 5.0),
        );
        // Pulled three times as hard toward R: 3/4 of the way there.
        assert!(near(p["C1"].0, 7.5), "{:?}", p["C1"]);
        assert!(near(p["C1"].1, 0.0), "{:?}", p["C1"]);
    }

    /// The property greedy could not have. A chain of three parts strung between
    /// two anchors settles evenly, because all three positions are solved
    /// together. Placing them one at a time puts the first at the midpoint of
    /// whatever was already down and leaves the rest hanging off it.
    #[test]
    fn a_chain_settles_evenly_between_its_ends() {
        let p = solve(
            &["A".to_string(), "B".to_string(), "C".to_string()],
            &fixed(&[("L", 0.0, 0.0), ("R", 12.0, 0.0)]),
            &[
                edge("L", "A", 1.0),
                edge("A", "B", 1.0),
                edge("B", "C", 1.0),
                edge("C", "R", 1.0),
            ],
            &home(6.0, 0.0),
        );
        for (r, want) in [("A", 3.0), ("B", 6.0), ("C", 9.0)] {
            assert!((p[r].0 - want).abs() < 1e-3, "{r} at {:?}", p[r]);
        }
    }

    /// A bypass cap torn between its IC and a signal net lands on the IC's side
    /// of the board, in proportion to the weights — not somewhere in between,
    /// and not so hard that the cap is the only thing the solve can see.
    #[test]
    fn the_decoupling_bond_outweighs_a_signal_net() {
        let ordinary = solve(
            &["C1".to_string()],
            &fixed(&[("U1", 0.0, 0.0), ("J1", 100.0, 0.0)]),
            &[edge("C1", "U1", 1.0), edge("C1", "J1", 1.0)],
            &home(50.0, 0.0),
        );
        let p = solve(
            &["C1".to_string()],
            &fixed(&[("U1", 0.0, 0.0), ("J1", 100.0, 0.0)]),
            &[edge("C1", "U1", DECOUPLE_PULL), edge("C1", "J1", 1.0)],
            &home(50.0, 0.0),
        );
        assert!(
            near(ordinary["C1"].0, 50.0),
            "ordinary equal pulls should split the difference, got {:?}",
            ordinary["C1"]
        );
        assert!(
            near(p["C1"].0, 25.0) && p["C1"].0 < ordinary["C1"].0,
            "decoupling pull should put the cap on the IC side of an ordinary signal pull, got {:?}",
            p["C1"]
        );
    }

    #[test]
    fn a_part_tied_to_nothing_goes_home() {
        let p = solve(
            &["R9".to_string()],
            &fixed(&[("U1", 0.0, 0.0)]),
            &[],
            &home(20.0, 64.0),
        );
        assert!(
            near(p["R9"].0, 20.0) && near(p["R9"].1, 64.0),
            "{:?}",
            p["R9"]
        );
    }

    /// An island of parts netted only to each other has no path to the panel, so
    /// no position on it beats any other. It must still answer.
    #[test]
    fn an_island_with_no_anchor_still_solves() {
        let p = solve(
            &["R1".to_string(), "R2".to_string()],
            &fixed(&[("U1", 0.0, 0.0)]),
            &[edge("R1", "R2", 1.0)],
            &home(20.0, 64.0),
        );
        for r in ["R1", "R2"] {
            assert!(p[r].0.is_finite() && p[r].1.is_finite());
            assert!(
                (p[r].0 - 20.0).abs() < 1e-3 && (p[r].1 - 64.0).abs() < 1e-3,
                "{r} {:?}",
                p[r]
            );
        }
    }

    /// Nothing can be pulled outside everything that pulls it — so a solved
    /// placement is on the board whenever the anchors are, before legalization
    /// has done anything.
    #[test]
    fn the_solution_stays_inside_the_hull_of_what_is_fixed() {
        let movable: Vec<String> = ["A", "B", "C", "D"].iter().map(|s| s.to_string()).collect();
        let p = solve(
            &movable,
            &fixed(&[("P1", 2.0, 3.0), ("P2", 38.0, 120.0), ("P3", 5.0, 110.0)]),
            &[
                edge("A", "P1", 1.0),
                edge("A", "B", 0.5),
                edge("B", "C", 0.5),
                edge("C", "P2", 1.0),
                edge("D", "P3", 0.25),
                edge("D", "A", 2.0),
            ],
            &home(200.0, -50.0),
        );
        for (r, q) in &p {
            assert!(
                !near(q.0, 200.0) || !near(q.1, -50.0),
                "{r} fell back to the outside home instead of solving: {q:?}"
            );
            assert!(q.0 >= 2.0 - 1e-6 && q.0 <= 38.0 + 1e-6, "{r} {q:?}");
            assert!(q.1 >= 3.0 - 1e-6 && q.1 <= 120.0 + 1e-6, "{r} {q:?}");
        }
    }

    /// A board with nothing anchored — no panel, no power header — has no frame,
    /// and every part coincides at the objective's optimum. Answering that with
    /// one point stacks the whole circuit in the middle of the board, which cost
    /// a 13-part demo 3 unrouted nets. Each part must fall back to its own home.
    #[test]
    fn an_unanchored_board_falls_back_to_the_spread_not_to_one_point() {
        let movable: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        let spread: HashMap<String, (f64, f64)> = movable
            .iter()
            .enumerate()
            .map(|(i, r)| (r.clone(), (20.0, 20.0 + 40.0 * i as f64)))
            .collect();
        let p = solve(
            &movable,
            &HashMap::new(),
            &[edge("A", "B", 1.0), edge("B", "C", 1.0)],
            &spread,
        );
        assert_eq!(p, spread);
    }

    /// Same input, same answer — the loop keeps the best of several attempts and
    /// diffs boards between them, so a placer that wobbles is unusable.
    #[test]
    fn the_solve_is_deterministic() {
        let movable: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        let anchors = fixed(&[("P1", 0.0, 0.0), ("P2", 30.0, 90.0)]);
        let edges = vec![
            edge("A", "P1", 1.0),
            edge("A", "B", 0.7),
            edge("B", "C", 0.3),
            edge("C", "P2", 1.5),
        ];
        let first = solve(&movable, &anchors, &edges, &home(15.0, 45.0));
        let again = solve(&movable, &anchors, &edges, &home(15.0, 45.0));
        assert_eq!(first, again);
    }

    /// An edge naming a part that is neither movable nor fixed (a part filtered
    /// out upstream) is skipped, not panicked on.
    #[test]
    fn an_edge_to_an_unknown_part_is_ignored() {
        let p = solve(
            &["A".to_string()],
            &fixed(&[("P1", 4.0, 4.0)]),
            &[edge("A", "P1", 1.0), edge("A", "GHOST", 99.0)],
            &home(0.0, 0.0),
        );
        assert!(near(p["A"].0, 4.0) && near(p["A"].1, 4.0), "{:?}", p["A"]);
    }

    fn a_circuit() -> Circuit {
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
        Circuit {
            name: "t".into(),
            parts: vec![
                part("U1", "Package_SO:SOIC-8"),
                part("C2", "Capacitor_SMD:C_0603_1608Metric"), // decoupling +12V↔GND
                part("C1", "Capacitor_SMD:C_0603_1608Metric"), // signal SIG↔GND
                part("R1", "Resistor_SMD:R_0603_1608Metric"),
            ],
            nets: vec![
                Net {
                    name: "+12V".into(),
                    pins: vec![node("U1", "8"), node("C2", "1")],
                    net_class: None,
                },
                Net {
                    name: "GND".into(),
                    pins: vec![node("U1", "4"), node("C2", "2"), node("C1", "2")],
                    net_class: None,
                },
                Net {
                    name: "SIG".into(),
                    pins: vec![node("U1", "1"), node("C1", "1")],
                    net_class: None,
                },
                Net {
                    name: "FB".into(),
                    pins: vec![node("U1", "2"), node("R1", "1")],
                    net_class: Some("Critical".into()),
                },
            ],
        }
    }

    #[test]
    fn a_two_pin_net_pulls_harder_than_a_rail() {
        let e = attractors(&a_circuit());
        let weights = |a: &str, b: &str| {
            let mut got: Vec<f64> = e
                .iter()
                .filter(|(x, y, _)| (x == a && y == b) || (x == b && y == a))
                .map(|(_, _, w)| *w)
                .collect();
            got.sort_by(f64::total_cmp);
            got
        };
        // SIG is a 2-pin net: full strength. GND touches three parts: half.
        assert_eq!(weights("C1", "U1"), vec![0.5, 1.0]);
        assert_eq!(weights("C1", "C2"), vec![0.5]);
        // A critical net pulls CRITICAL_PULL× harder than an ordinary 2-pin one.
        assert_eq!(weights("R1", "U1"), vec![CRITICAL_PULL]);
    }

    /// Moved from `board.rs` with the edge model. C2 bridges +12V↔GND so it is
    /// bonded to U1; C1 is a signal cap and gets no bond.
    #[test]
    fn a_bypass_cap_is_bonded_to_its_ic() {
        let e = attractors(&a_circuit());
        let bond = |a: &str, b: &str| {
            e.iter()
                .any(|(x, y, w)| *w == DECOUPLE_PULL && ((x == a && y == b) || (x == b && y == a)))
        };
        assert!(bond("C2", "U1"));
        assert!(!bond("C1", "U1"));
    }

    #[test]
    fn pull_totals_sum_both_ends_of_every_edge() {
        let t = pull_totals(&[edge("A", "B", 1.0), edge("A", "C", 0.5)]);
        assert!(near(t["A"], 1.5));
        assert!(near(t["B"], 1.0));
        assert!(near(t["C"], 0.5));
    }
}
