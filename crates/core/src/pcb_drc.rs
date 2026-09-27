//! First-party copper DRC: clearance, board-edge, and connectivity — the rules
//! the layout loop and router can be planned against instead of discovering via a
//! `kicad-cli` subprocess.
//!
//! This is a **scoped** checker, not a KiCad DRC replacement. Rule coverage is
//! declared in [`IMPLEMENTED_CHECKS`] / [`DEFERRED_CHECKS`]; anything deferred is
//! not silently passed — it is reported as out of scope so an unsupported board
//! is never called clean. KiCad's own DRC stays available as
//! [`crate::drc::run_drc`] for the remainder and as a cross-check oracle.
//!
//! @derives-from url:https://www.ipc.org/TOC/IPC-2221.pdf — generic conductor
//!   spacing (clearance) and board-edge rules; exact clause/table to be filled
//!   from the licensed edition (see design_rules.rs for the citation registry)

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::drc::{DrcItem, DrcReport, DrcViolation, Pos};
use crate::route::{pt_seg, seg_pad, seg_seg, PadPoint, RouteNet, Track, Via};

/// The rules this checker actually evaluates.
pub const IMPLEMENTED_CHECKS: &[&str] = &["clearance", "edge_clearance", "unconnected_items"];

/// A compact, float-free digest of a first-party [`DrcReport`], safe to embed
/// in the serializable routing evidence (`RoutingReport`) and the CLI report.
/// It states what the checker *found and did not find*, so a green board says
/// so on its face and a deferred kind is never mistaken for a passed one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirstPartyDrcSummary {
    pub error_count: usize,
    pub warning_count: usize,
    pub unconnected_count: usize,
    /// Violated implemented kinds, sorted, with counts.
    pub error_kinds: Vec<(String, usize)>,
    /// The kinds this checker evaluates at all — the coverage contract.
    pub implemented_checks: Vec<String>,
}

impl FirstPartyDrcSummary {
    pub fn of(report: &DrcReport) -> Self {
        let mut by_kind: std::collections::BTreeMap<String, usize> = Default::default();
        for v in report.errors() {
            *by_kind.entry(v.kind.clone()).or_default() += 1;
        }
        Self {
            error_count: report.error_count(),
            warning_count: report.warning_count(),
            unconnected_count: report.unconnected_count(),
            error_kinds: by_kind.into_iter().collect(),
            implemented_checks: IMPLEMENTED_CHECKS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// KiCad DRC keys this checker does **not** evaluate. Retained so a caller can
/// see the gap rather than trust a green result that never covered them.
pub const DEFERRED_CHECKS: &[&str] = &[
    "track_width",
    "via_diameter",
    "via_drill",
    "hole_size",
    "annular_width",
    "hole_to_hole",
    "silk_over_copper",
    "silk_overlap",
    "silk_edge_clearance",
    "solder_mask_bridge",
    "courtyard_overlap",
    "lib_footprint_mismatch",
];

const TOL: f64 = 1e-6;
const FRONT: usize = 0;
const BACK: usize = 1;

/// The copper a board generation pass produced, plus the context the checker
/// needs. Pads come from the routed nets (net membership included); tracks and
/// vias from the router output.
pub struct CopperGeometry<'a> {
    pub nets: &'a [RouteNet],
    pub tracks: &'a [Track],
    pub vias: &'a [Via],
    pub outline: Option<(f64, f64, f64, f64)>,
    pub edge_clearance_mm: f64,
    /// Nets connected by a copper pour. The pour is not in `tracks`, so their
    /// pads are treated as connected (the plane does the work).
    pub poured_nets: &'a [usize],
}

/// Run the implemented checks over `geo` at a constant `clearance_mm`.
pub fn check_copper(geo: &CopperGeometry, clearance_mm: f64) -> DrcReport {
    check_copper_with(geo, &|_, _| clearance_mm)
}

/// Run the implemented checks with **per-pair** clearance: `pair_clearance`
/// answers the required spacing between two nets by index, which is how a
/// net-class ruleset is honored — the checker, the router, and the emitted
/// `.kicad_dru` resolve the same `DesignRules` object.
pub fn check_copper_with(
    geo: &CopperGeometry,
    pair_clearance: &dyn Fn(usize, usize) -> f64,
) -> DrcReport {
    let mut report = DrcReport::default();
    clearance(geo, pair_clearance, &mut report);
    edge_clearance(geo, &mut report);
    connectivity(geo, &mut report);
    report
}

fn track_layer(track: &Track) -> usize {
    if track.layer.eq_ignore_ascii_case("B.Cu") {
        BACK
    } else {
        FRONT
    }
}

fn item(description: impl Into<String>, at: (f64, f64)) -> DrcItem {
    DrcItem {
        description: description.into(),
        pos: Some(Pos { x: at.0, y: at.1 }),
        uuid: None,
    }
}

fn violation(kind: &str, description: impl Into<String>, items: Vec<DrcItem>) -> DrcViolation {
    DrcViolation {
        kind: kind.into(),
        severity: "error".into(),
        description: description.into(),
        items,
    }
}

fn clearance(
    geo: &CopperGeometry,
    pair_clearance: &dyn Fn(usize, usize) -> f64,
    report: &mut DrcReport,
) {
    for (i, t) in geo.tracks.iter().enumerate() {
        let tl = track_layer(t);
        for u in &geo.tracks[i + 1..] {
            if u.net_idx == t.net_idx || track_layer(u) != tl {
                continue;
            }
            let limit = pair_clearance(t.net_idx, u.net_idx) - TOL;
            let gap = seg_seg(t.start, t.end, u.start, u.end) - (t.width_mm + u.width_mm) / 2.0;
            if gap < limit {
                report.violations.push(violation(
                    "clearance",
                    format!(
                        "tracks of nets {} and {} are {gap:.3}mm apart",
                        t.net_idx, u.net_idx
                    ),
                    vec![item("track", t.start), item("track", u.start)],
                ));
            }
        }
        for v in geo.vias.iter().filter(|v| v.net_idx != t.net_idx) {
            let limit = pair_clearance(t.net_idx, v.net_idx) - TOL;
            let gap = pt_seg(v.at, t.start, t.end) - (t.width_mm + v.size_mm) / 2.0;
            if gap < limit {
                report.violations.push(violation(
                    "clearance",
                    format!(
                        "track net {} and via net {} are {gap:.3}mm apart",
                        t.net_idx, v.net_idx
                    ),
                    vec![item("track", t.start), item("via", v.at)],
                ));
            }
        }
        for net in geo.nets.iter().filter(|n| n.net_idx != t.net_idx) {
            let limit = pair_clearance(t.net_idx, net.net_idx) - TOL;
            for p in net.pads.iter().filter(|p| p.layer.on(tl)) {
                let gap = seg_pad(t.start, t.end, p) - t.width_mm / 2.0;
                if gap < limit {
                    report.violations.push(violation(
                        "clearance",
                        format!(
                            "track net {} is {gap:.3}mm from pad {}.{} (net {})",
                            t.net_idx, p.refdes, p.pad, net.net_idx
                        ),
                        vec![item("track", t.start), item("pad", (p.x_mm, p.y_mm))],
                    ));
                }
            }
        }
    }
    for (i, v) in geo.vias.iter().enumerate() {
        for u in &geo.vias[i + 1..] {
            if u.net_idx == v.net_idx {
                continue;
            }
            let limit = pair_clearance(v.net_idx, u.net_idx) - TOL;
            let gap = (v.at.0 - u.at.0).hypot(v.at.1 - u.at.1) - (v.size_mm + u.size_mm) / 2.0;
            if gap < limit {
                report.violations.push(violation(
                    "clearance",
                    format!(
                        "vias of nets {} and {} are {gap:.3}mm apart",
                        v.net_idx, u.net_idx
                    ),
                    vec![item("via", v.at), item("via", u.at)],
                ));
            }
        }
        for net in geo.nets.iter().filter(|n| n.net_idx != v.net_idx) {
            let limit = pair_clearance(v.net_idx, net.net_idx) - TOL;
            for p in &net.pads {
                let gap = seg_pad(v.at, v.at, p) - v.size_mm / 2.0;
                if gap < limit {
                    report.violations.push(violation(
                        "clearance",
                        format!(
                            "via net {} is {gap:.3}mm from pad {}.{} (net {})",
                            v.net_idx, p.refdes, p.pad, net.net_idx
                        ),
                        vec![item("via", v.at), item("pad", (p.x_mm, p.y_mm))],
                    ));
                }
            }
        }
    }
}

fn edge_clearance(geo: &CopperGeometry, report: &mut DrcReport) {
    let Some((x0, y0, x1, y1)) = geo.outline else {
        return;
    };
    let band = geo.edge_clearance_mm - TOL;
    let outside = |x: f64, y: f64, half: f64| {
        x - half < x0 + band || x + half > x1 - band || y - half < y0 + band || y + half > y1 - band
    };
    for t in geo.tracks {
        let half = t.width_mm / 2.0;
        for end in [t.start, t.end] {
            if outside(end.0, end.1, half) {
                report.violations.push(violation(
                    "edge_clearance",
                    format!(
                        "track net {} is inside the {:.2}mm board-edge band",
                        t.net_idx, geo.edge_clearance_mm
                    ),
                    vec![item("track", end)],
                ));
            }
        }
    }
    for v in geo.vias {
        if outside(v.at.0, v.at.1, v.size_mm / 2.0) {
            report.violations.push(violation(
                "edge_clearance",
                format!(
                    "via net {} is inside the {:.2}mm board-edge band",
                    v.net_idx, geo.edge_clearance_mm
                ),
                vec![item("via", v.at)],
            ));
        }
    }
}

/// One net's copper, for the connectivity graph.
enum Cu<'a> {
    Pad(&'a PadPoint),
    Track(&'a Track),
    Via(&'a Via),
}

fn touches(a: &Cu, b: &Cu) -> bool {
    match (a, b) {
        (Cu::Pad(p), Cu::Pad(q)) => rects_touch(p, q),
        (Cu::Pad(p), Cu::Track(t)) | (Cu::Track(t), Cu::Pad(p)) => {
            p.layer.on(track_layer(t)) && seg_pad(t.start, t.end, p) <= TOL
        }
        (Cu::Pad(p), Cu::Via(v)) | (Cu::Via(v), Cu::Pad(p)) => {
            seg_pad(v.at, v.at, p) <= v.size_mm / 2.0 + TOL
        }
        (Cu::Track(t), Cu::Track(u)) => {
            track_layer(t) == track_layer(u) && seg_seg(t.start, t.end, u.start, u.end) <= TOL
        }
        (Cu::Track(t), Cu::Via(v)) | (Cu::Via(v), Cu::Track(t)) => {
            pt_seg(v.at, t.start, t.end) <= (t.width_mm + v.size_mm) / 2.0 + TOL
        }
        (Cu::Via(v), Cu::Via(u)) => {
            (v.at.0 - u.at.0).hypot(v.at.1 - u.at.1) <= (v.size_mm + u.size_mm) / 2.0 + TOL
        }
    }
}

/// Per-net connectivity: every pad of a routed net must be joined to the rest
/// through copper that touches. The router reports its own conflicts; this is
/// the independent geometric check that its copper actually joins what it claims.
fn connectivity(geo: &CopperGeometry, report: &mut DrcReport) {
    let poured: HashSet<usize> = geo.poured_nets.iter().copied().collect();
    for net in geo
        .nets
        .iter()
        .filter(|n| n.net_idx != 0 && n.pads.len() >= 2)
    {
        if poured.contains(&net.net_idx) {
            continue; // connected by the plane, not by tracks
        }
        let mut items: Vec<Cu> = Vec::new();
        for p in &net.pads {
            items.push(Cu::Pad(p));
        }
        for t in geo.tracks.iter().filter(|t| t.net_idx == net.net_idx) {
            items.push(Cu::Track(t));
        }
        for v in geo.vias.iter().filter(|v| v.net_idx == net.net_idx) {
            items.push(Cu::Via(v));
        }
        let mut uf = UnionFind::new(items.len());
        for a in 0..items.len() {
            for b in a + 1..items.len() {
                if touches(&items[a], &items[b]) {
                    uf.union(a, b);
                }
            }
        }
        let roots: HashSet<usize> = (0..net.pads.len()).map(|i| uf.find(i)).collect();
        if roots.len() > 1 {
            report.unconnected_items.push(violation(
                "unconnected_items",
                format!(
                    "net {} ({}) has {} separate copper islands across {} pads",
                    net.net_idx,
                    net.name,
                    roots.len(),
                    net.pads.len()
                ),
                net.pads
                    .iter()
                    .take(6)
                    .map(|p| item(format!("pad {}.{}", p.refdes, p.pad), (p.x_mm, p.y_mm)))
                    .collect(),
            ));
        }
    }
}

fn rects_touch(a: &PadPoint, b: &PadPoint) -> bool {
    let (ax0, ay0) = (a.x_mm - a.w_mm / 2.0, a.y_mm - a.h_mm / 2.0);
    let (ax1, ay1) = (a.x_mm + a.w_mm / 2.0, a.y_mm + a.h_mm / 2.0);
    let (bx0, by0) = (b.x_mm - b.w_mm / 2.0, b.y_mm - b.h_mm / 2.0);
    let (bx1, by1) = (b.x_mm + b.w_mm / 2.0, b.y_mm + b.h_mm / 2.0);
    ax0 - TOL < bx1 && bx0 - TOL < ax1 && ay0 - TOL < by1 && by0 - TOL < ay1
}

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::PadLayer;

    fn pad(refdes: &str, num: &str, x: f64, y: f64, layer: PadLayer) -> PadPoint {
        PadPoint {
            refdes: refdes.into(),
            pad: num.into(),
            x_mm: x,
            y_mm: y,
            w_mm: 0.5,
            h_mm: 0.5,
            layer,
        }
    }

    fn track(start: (f64, f64), end: (f64, f64), net_idx: usize) -> Track {
        Track {
            start,
            end,
            width_mm: 0.25,
            layer: "F.Cu".into(),
            net_idx,
        }
    }

    fn geo<'a>(
        nets: &'a [RouteNet],
        tracks: &'a [Track],
        vias: &'a [Via],
        poured: &'a [usize],
    ) -> CopperGeometry<'a> {
        CopperGeometry {
            nets,
            tracks,
            vias,
            outline: None,
            edge_clearance_mm: 0.5,
            poured_nets: poured,
        }
    }

    #[test]
    fn implemented_and_deferred_checks_are_disjoint() {
        // A check must be exactly one of implemented or deferred: the coverage
        // manifest is how a caller knows what a green result did not cover.
        for c in IMPLEMENTED_CHECKS {
            assert!(
                !DEFERRED_CHECKS.contains(c),
                "{c} is listed as both implemented and deferred"
            );
        }
        for c in ["clearance", "edge_clearance", "unconnected_items"] {
            assert!(
                IMPLEMENTED_CHECKS.contains(&c),
                "{c} missing from implemented"
            );
        }
        // Every violation kind the checker can emit must be a declared check.
        let nets = vec![RouteNet {
            net_idx: 1,
            name: "SIG".into(),
            pads: vec![
                pad("U1", "1", 0.0, 0.0, PadLayer::Front),
                pad("U2", "1", 5.0, 0.0, PadLayer::Front),
            ],
        }];
        let mut g = geo(&nets, &[], &[], &[]);
        g.outline = Some((0.0, 0.0, 1.0, 1.0));
        for v in check_copper(&g, 0.2).all() {
            assert!(
                IMPLEMENTED_CHECKS.contains(&v.kind.as_str()),
                "emitted an undeclared check kind: {}",
                v.kind
            );
        }
    }

    #[test]
    fn flags_a_clearance_violation_and_passes_at_clearance() {
        let nets = vec![
            RouteNet {
                net_idx: 1,
                name: "A".into(),
                pads: vec![pad("U1", "1", 0.0, 5.0, PadLayer::Front)],
            },
            RouteNet {
                net_idx: 2,
                name: "B".into(),
                pads: vec![pad("U2", "1", 10.0, 5.0, PadLayer::Front)],
            },
        ];
        // Two parallel tracks 0.3mm apart centre-to-centre, 0.25mm wide: 0.05mm
        // of air, below 0.2mm clearance.
        let tight = [
            track((0.0, 0.0), (5.0, 0.0), 1),
            track((0.0, 0.3), (5.0, 0.3), 2),
        ];
        let r = check_copper(&geo(&nets, &tight, &[], &[]), 0.2);
        assert_eq!(
            r.violations
                .iter()
                .filter(|v| v.kind == "clearance")
                .count(),
            1
        );
        // At 0.45mm apart there is exactly legal clearance.
        let loose = [
            track((0.0, 0.0), (5.0, 0.0), 1),
            track((0.0, 0.45), (5.0, 0.45), 2),
        ];
        let r = check_copper(&geo(&nets, &loose, &[], &[]), 0.2);
        assert!(r.violations.iter().all(|v| v.kind != "clearance"));
    }

    #[test]
    fn flags_a_board_edge_violation() {
        let nets = vec![RouteNet {
            net_idx: 1,
            name: "A".into(),
            pads: vec![pad("U1", "1", 1.0, 1.0, PadLayer::Front)],
        }];
        let tracks = [track((0.2, 5.0), (5.0, 5.0), 1)];
        let mut g = geo(&nets, &tracks, &[], &[]);
        g.outline = Some((0.0, 0.0, 10.0, 10.0));
        g.edge_clearance_mm = 0.5;
        let r = check_copper(&g, 0.2);
        assert_eq!(
            r.violations
                .iter()
                .filter(|v| v.kind == "edge_clearance")
                .count(),
            1
        );
    }

    #[test]
    fn connectivity_flags_a_disconnected_pad_and_passes_when_joined() {
        let nets = vec![RouteNet {
            net_idx: 1,
            name: "SIG".into(),
            pads: vec![
                pad("U1", "1", 0.0, 0.0, PadLayer::Front),
                pad("U2", "1", 5.0, 0.0, PadLayer::Front),
            ],
        }];
        // No copper joining the pads.
        let r = check_copper(&geo(&nets, &[], &[], &[]), 0.2);
        assert_eq!(r.unconnected_items.len(), 1);
        // A track from pad to pad joins them.
        let tracks = [track((0.0, 0.0), (5.0, 0.0), 1)];
        let r = check_copper(&geo(&nets, &tracks, &[], &[]), 0.2);
        assert_eq!(r.unconnected_items.len(), 0);
    }

    #[test]
    fn a_poured_net_is_not_flagged_unconnected() {
        let nets = vec![RouteNet {
            net_idx: 3,
            name: "GND".into(),
            pads: vec![
                pad("U1", "2", 0.0, 0.0, PadLayer::Front),
                pad("U2", "2", 9.0, 0.0, PadLayer::Front),
            ],
        }];
        let r = check_copper(&geo(&nets, &[], &[], &[3]), 0.2);
        assert!(r.unconnected_items.is_empty());
    }

    #[test]
    fn pairwise_clearance_uses_the_class_of_each_net() {
        // Two parallel 0.25mm tracks with 0.25mm of air (0.5mm centre-to-
        // centre): legal at a 0.2 default, illegal once either net's class
        // demands 0.3. This is the single-source assertion: the checker's
        // verdict moves when the rules do, with no recompile of the copper.
        let nets = vec![
            RouteNet {
                net_idx: 1,
                name: "SIG".into(),
                pads: vec![pad("U1", "1", 0.0, 5.0, PadLayer::Front)],
            },
            RouteNet {
                net_idx: 2,
                name: "+3V3".into(),
                pads: vec![pad("U2", "1", 10.0, 5.0, PadLayer::Front)],
            },
        ];
        let tracks = [
            track((0.0, 0.0), (5.0, 0.0), 1),
            track((0.0, 0.5), (5.0, 0.5), 2),
        ];
        let g = geo(&nets, &tracks, &[], &[]);
        let loose = check_copper(&g, 0.2);
        assert!(
            loose.violations.iter().all(|v| v.kind != "clearance"),
            "0.25mm air must be legal at 0.2: {loose:?}"
        );
        // Same copper, same constant check at 0.3 flags it — and a pair model
        // that only raises the Power net's class must agree.
        let strict = check_copper_with(&g, &|a, b| {
            let power = a == 2 || b == 2;
            if power {
                0.3
            } else {
                0.2
            }
        });
        assert_eq!(
            strict
                .violations
                .iter()
                .filter(|v| v.kind == "clearance")
                .count(),
            1,
            "the power pair must flag at 0.3"
        );
        // A pair model that raises an *involved* net only fires for involved
        // pairs: SIG-to-SIG geometry (none here) would stay clean, and the
        // constant-0.2 answer must genuinely differ from the pairwise one.
        let flat_strict = check_copper(&g, 0.3);
        assert_eq!(
            flat_strict
                .violations
                .iter()
                .filter(|v| v.kind == "clearance")
                .count(),
            1
        );
    }
}
