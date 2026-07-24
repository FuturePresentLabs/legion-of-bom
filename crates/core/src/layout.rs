//! Iterative layout loop — place → route → check → repair (DESIGN.md §6.5).
//!
//! Not general autorouting: guided repair over the *free* (non-anchored) parts
//! only. Placement is the lever. [`SeededPlacer`](crate::board::SeededPlacer)
//! seeds each free part at the centroid of what it's netted to, so signal traces
//! stay short; this loop wraps that with a mode-weighted cost function, keeps the
//! best-scoring attempt, and — when the router still can't connect everything —
//! **repairs** by perturbing the implicated parts and trying again. It exits by
//! resolving what it can and *surfacing the rest for manual routing* (§6.8),
//! never silently forcing a net through.
//!
//! The per-iteration "check" is in-process (wirelength, via count, unrouted
//! conflicts, critical-net tightness) because a full KiCad DRC costs seconds per
//! run — too slow to score every attempt. KiCad DRC runs once, on the winning
//! board, as a final gate (`kicad_cli`), with an opt-in (`drc_every_iter`) to
//! fold it into every iteration when the caller accepts the cost.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::board::{generate_board_artifacts, BoardError, BoardOptions, Placement, SeededPlacer};
use crate::drc::{run_drc, DrcReport};
use crate::route::RouteOutput;
use crate::source::CircuitSource;
use crate::stage::Finding;

/// Project layout mode (DESIGN §6.3) — selects the cost-function weights. Manual,
/// not auto-detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    /// Weights critical-net tightness and short signal wiring — audio boards.
    #[default]
    Analog,
    /// Weights via count and total routed length.
    Digital,
    /// Both, leaning on per-net `critical()` tags to disambiguate.
    Mixed,
}

impl LayoutMode {
    /// Parse `analog` | `digital` | `mixed` (case-insensitive).
    pub fn parse(s: &str) -> Option<LayoutMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "analog" => Some(LayoutMode::Analog),
            "digital" => Some(LayoutMode::Digital),
            "mixed" => Some(LayoutMode::Mixed),
            _ => None,
        }
    }

    /// The cost weights this mode scores placements by.
    pub fn weights(self) -> CostWeights {
        match self {
            // Analog: keep critical + signal nets short; vias matter little.
            LayoutMode::Analog => CostWeights {
                wirelength: 1.0,
                critical: 3.0,
                via: 0.5,
                routed_len: 0.2,
                unrouted: 50.0,
            },
            // Digital: fewer vias, shorter copper; criticality less special.
            LayoutMode::Digital => CostWeights {
                wirelength: 1.0,
                critical: 1.0,
                via: 2.0,
                routed_len: 1.0,
                unrouted: 50.0,
            },
            LayoutMode::Mixed => CostWeights {
                wirelength: 1.0,
                critical: 2.0,
                via: 1.0,
                routed_len: 0.5,
                unrouted: 50.0,
            },
        }
    }
}

/// Violation-scoring weights (DESIGN §6.3). Higher = the loop tries harder to
/// drive that term down. `unrouted` dominates so a routable board always beats a
/// tighter-but-broken one.
#[derive(Debug, Clone, Copy)]
pub struct CostWeights {
    /// Per-mm of fan-out-weighted signal wirelength.
    pub wirelength: f64,
    /// Per-mm of critical-net span (on top of `wirelength`).
    pub critical: f64,
    /// Per via.
    pub via: f64,
    /// Per-mm of total routed copper.
    pub routed_len: f64,
    /// Per net the router left unconnected.
    pub unrouted: f64,
}

/// What one placement+route attempt measured — all in-process, no KiCad. Lower is
/// better on every field.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlacementMetrics {
    /// Raw total half-perimeter wirelength across all multi-pin nets (reporting).
    pub hpwl_mm: f64,
    /// Fan-out-weighted HPWL: a 2-pin net counts full, an N-part rail at
    /// `1/(N-1)` — so signal nets, not unavoidable rails, drive the score.
    pub signal_hpwl_mm: f64,
    /// HPWL summed over `critical()`-tagged nets only.
    pub critical_hpwl_mm: f64,
    /// Total routed copper length (mm).
    pub routed_len_mm: f64,
    /// Vias placed.
    pub via_count: usize,
    /// Connections the router could not complete.
    pub unrouted: usize,
}

/// Measure a placement+route attempt. `placements` are part centres (board
/// coordinates); `route` is what the router produced.
pub fn measure(
    circuit: &dyn CircuitSource,
    placements: &HashMap<String, Placement>,
    route: &RouteOutput,
) -> PlacementMetrics {
    let mut m = PlacementMetrics::default();
    for net in circuit.nets() {
        // Distinct placed parts on this net, by centre.
        let mut seen: Vec<&str> = Vec::new();
        let mut pts: Vec<(f64, f64)> = Vec::new();
        for pin in &net.pins {
            let r = pin.refdes.0.as_str();
            if seen.contains(&r) {
                continue;
            }
            if let Some(p) = placements.get(r) {
                seen.push(r);
                pts.push((p.x_mm, p.y_mm));
            }
        }
        if pts.len() < 2 {
            continue;
        }
        let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for &(x, y) in &pts {
            minx = minx.min(x);
            miny = miny.min(y);
            maxx = maxx.max(x);
            maxy = maxy.max(y);
        }
        let hpwl = (maxx - minx) + (maxy - miny);
        m.hpwl_mm += hpwl;
        m.signal_hpwl_mm += hpwl / (pts.len() as f64 - 1.0);
        if net.is_critical() {
            m.critical_hpwl_mm += hpwl;
        }
    }
    m.routed_len_mm = route
        .tracks
        .iter()
        .map(|t| (t.end.0 - t.start.0).hypot(t.end.1 - t.start.1))
        .sum();
    m.via_count = route.vias.len();
    m.unrouted = route.conflicts.len();
    m
}

/// The mode-weighted cost of a placement — lower is better.
pub fn score(m: &PlacementMetrics, w: &CostWeights) -> f64 {
    w.wirelength * m.signal_hpwl_mm
        + w.critical * m.critical_hpwl_mm
        + w.via * m.via_count as f64
        + w.routed_len * m.routed_len_mm
        + w.unrouted * m.unrouted as f64
}

/// Loop configuration.
#[derive(Debug, Clone)]
pub struct LayoutLoop {
    pub mode: LayoutMode,
    /// Placement attempts to try (>=1). The first is the pure seeded placement;
    /// the rest are repair perturbations, kept only if they score better.
    pub max_iters: usize,
    /// `kicad-cli` for the final DRC gate; `None` skips DRC (graceful degrade).
    pub kicad_cli: Option<PathBuf>,
    /// Fold a full KiCad DRC into *every* iteration's score. Off by default —
    /// KiCad DRC costs seconds per run (measured ~3.5 s on the slew board).
    pub drc_every_iter: bool,
}

impl Default for LayoutLoop {
    fn default() -> Self {
        LayoutLoop {
            mode: LayoutMode::Analog,
            max_iters: 6,
            kicad_cli: None,
            drc_every_iter: false,
        }
    }
}

/// The loop's result: the best board and why it stopped.
#[derive(Debug)]
pub struct LayoutReport {
    /// The best-scoring `.kicad_pcb` text.
    pub board: String,
    /// Attempts actually run.
    pub iterations: usize,
    /// The winning attempt's cost.
    pub score: f64,
    /// The winning attempt's metrics.
    pub metrics: PlacementMetrics,
    /// Connections surfaced for manual routing (§6.8) — the router's conflicts.
    pub unresolved: Vec<String>,
    /// Mechanical clearance problems: parts under a stacked sub-board taller than
    /// its standoff (DESIGN 6.7). Surfaced, not auto-fixed.
    pub collisions: Vec<String>,
    /// Final-gate DRC report, when `kicad_cli` was provided.
    pub drc: Option<DrcReport>,
    /// Human-facing observations (info/warning/error), incl. unresolved criticals.
    pub findings: Vec<Finding>,
}

/// Run the iterative layout loop. `template` carries the panel dimensions and
/// anchored cutouts; each iteration rebuilds a [`SeededPlacer`] from it (with
/// repair nudges) and installs it into `options` before generating the board.
/// `options`' router, route settings, ground pour, and outline are used as-is.
pub fn run_layout_loop(
    circuit: &dyn CircuitSource,
    mut options: BoardOptions,
    template: SeededPlacer,
    cfg: &LayoutLoop,
) -> Result<LayoutReport, BoardError> {
    // One placement attempt's result, so the loop can keep the best by score.
    struct Attempt {
        score: f64,
        board: String,
        metrics: PlacementMetrics,
        unresolved: Vec<String>,
        collisions: Vec<String>,
        drc: Option<DrcReport>,
    }

    let weights = cfg.mode.weights();
    let iters = cfg.max_iters.max(1);

    // Free parts (everything not anchored), sorted — the repair perturbation set.
    let mut free: Vec<String> = circuit
        .parts()
        .iter()
        .map(|p| p.refdes.0.clone())
        .filter(|r| !template.anchors.contains_key(r))
        .collect();
    free.sort();

    let mut best: Option<Attempt> = None;
    let mut nudges: HashMap<String, (f64, f64)> = HashMap::new();
    let mut ran = 0;

    for i in 0..iters {
        ran += 1;
        let mut placer = template.clone();
        placer.nudges = nudges.clone();
        options.placer = Box::new(placer);

        let art = generate_board_artifacts(circuit, &options)?;
        let metrics = measure(circuit, &art.placements, &art.route);
        let mut sc = score(&metrics, &weights);

        // Optional per-iteration DRC (opt-in; slow). Errors add a large penalty.
        let mut drc = None;
        if cfg.drc_every_iter {
            if let Some(cli) = &cfg.kicad_cli {
                if let Ok(report) = drc_on(circuit, &art.pcb, cli) {
                    sc += 100.0 * report.error_count() as f64;
                    drc = Some(report);
                }
            }
        }

        let improved = best.as_ref().is_none_or(|b| sc < b.score - 1e-6);
        if improved {
            best = Some(Attempt {
                score: sc,
                board: art.pcb,
                metrics,
                unresolved: art.route.conflicts.clone(),
                collisions: art.collisions.clone(),
                drc,
            });
        }

        // Nothing left unrouted → the seeded placement is already clean; stop.
        if metrics.unrouted == 0 {
            break;
        }
        // Last iteration — no point planning another repair.
        if i + 1 == iters {
            break;
        }
        // Repair: perturb the free parts so the next attempt explores a different
        // arrangement the router may find easier (DESIGN §6.5 step 4). Deterministic
        // shake — no RNG — so each attempt is a clean, reproducible git diff.
        nudges = repair_nudges(&free, i + 1);
    }

    let Attempt {
        score,
        board,
        metrics,
        unresolved,
        collisions,
        mut drc,
    } = best.expect("loop runs at least once");

    // Final DRC gate on the winning board, if not already done per-iteration.
    if drc.is_none() {
        if let Some(cli) = &cfg.kicad_cli {
            drc = drc_on(circuit, &board, cli).ok();
        }
    }

    // Surface findings: unresolved nets (critical ones as errors — §6.8), DRC.
    let mut findings = Vec::new();
    if unresolved.is_empty() {
        findings.push(Finding::info("all nets routed"));
    } else {
        let criticals: Vec<&str> = circuit
            .nets()
            .iter()
            .filter(|n| n.is_critical())
            .map(|n| n.name.as_str())
            .collect();
        for c in &unresolved {
            // A critical net left unrouted must be routed by hand — never forced.
            let is_critical = criticals
                .iter()
                .any(|name| c.contains(&format!("({name}):")));
            if is_critical {
                findings.push(Finding::error(format!("critical net unresolved: {c}")));
            } else {
                findings.push(Finding::warning(format!(
                    "unresolved (route manually): {c}"
                )));
            }
        }
    }
    for c in &collisions {
        findings.push(Finding::warning(format!("mechanical clearance: {c}")));
    }
    if let Some(report) = &drc {
        if report.error_count() > 0 {
            findings.push(Finding::error(format!(
                "KiCad DRC: {} error(s)",
                report.error_count()
            )));
        } else {
            findings.push(Finding::info("KiCad DRC clean"));
        }
    }

    Ok(LayoutReport {
        board,
        iterations: ran,
        score,
        metrics,
        unresolved,
        collisions,
        drc,
        findings,
    })
}

/// Deterministic repair perturbation: nudge each free part by a golden-angle
/// offset that varies with the attempt, so successive attempts explore different
/// arrangements without any RNG. Magnitude grows with the attempt number.
fn repair_nudges(free: &[String], attempt: usize) -> HashMap<String, (f64, f64)> {
    const GOLDEN_ANGLE: f64 = 2.399_963_229_728_653; // radians
    let mag = 2.0 + 1.5 * attempt as f64;
    free.iter()
        .enumerate()
        .map(|(k, r)| {
            let ang = GOLDEN_ANGLE * (k + attempt) as f64;
            (r.clone(), (mag * ang.cos(), mag * ang.sin()))
        })
        .collect()
}

/// Run KiCad DRC on a board string by writing it to a temp file first (KiCad
/// needs a path). Best-effort — any failure (no kicad-cli, write error) is an
/// `Err` the caller treats as "no DRC this run".
fn drc_on(
    circuit: &dyn CircuitSource,
    board: &str,
    kicad_cli: &std::path::Path,
) -> Result<DrcReport, BoardError> {
    let path = std::env::temp_dir().join(format!("lob_layout_{}.kicad_pcb", circuit.name()));
    std::fs::write(&path, board)?;
    run_drc(&path, kicad_cli).map_err(|e| BoardError::Other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{PartFacts, Placer};
    use crate::model::{Net, Part, PinRef};
    use crate::route::RouteOutput;
    use crate::source::CircuitSource;

    /// A tiny circuit: two 2-pin nets, one of them tagged critical.
    struct Toy {
        parts: Vec<Part>,
        nets: Vec<Net>,
    }
    impl CircuitSource for Toy {
        fn name(&self) -> &str {
            "toy"
        }
        fn parts(&self) -> &[Part] {
            &self.parts
        }
        fn nets(&self) -> &[Net] {
            &self.nets
        }
    }

    fn toy() -> Toy {
        Toy {
            parts: vec![
                Part::new("U1", "opamp"),
                Part::new("C1", "47n"),
                Part::new("R1", "10k"),
            ],
            nets: vec![
                Net::new("SLEW", vec![PinRef::new("U1", "5"), PinRef::new("C1", "1")])
                    .with_class("Critical"),
                Net::new("OUT", vec![PinRef::new("U1", "1"), PinRef::new("R1", "2")]),
            ],
        }
    }

    #[test]
    fn score_rewards_shorter_and_routed() {
        let w = LayoutMode::Analog.weights();
        let tight = PlacementMetrics {
            signal_hpwl_mm: 20.0,
            critical_hpwl_mm: 5.0,
            ..Default::default()
        };
        let loose = PlacementMetrics {
            signal_hpwl_mm: 80.0,
            critical_hpwl_mm: 40.0,
            ..Default::default()
        };
        assert!(score(&tight, &w) < score(&loose, &w));

        // An unrouted net dominates a modest wirelength win: the same tight board
        // with one connection left open must score worse than fully routed.
        let broken = PlacementMetrics {
            unrouted: 1,
            ..tight
        };
        assert!(score(&broken, &w) > score(&tight, &w));
    }

    #[test]
    fn analog_mode_weights_criticals_harder_than_digital() {
        let m = PlacementMetrics {
            critical_hpwl_mm: 10.0,
            ..Default::default()
        };
        assert!(
            score(&m, &LayoutMode::Analog.weights()) > score(&m, &LayoutMode::Digital.weights())
        );
    }

    #[test]
    fn measure_counts_hpwl_and_criticals() {
        let c = toy();
        let mut placements = HashMap::new();
        placements.insert("U1".into(), place(0.0, 0.0));
        placements.insert("C1".into(), place(3.0, 4.0)); // SLEW span = 3+4 = 7
        placements.insert("R1".into(), place(10.0, 0.0)); // OUT span = 10+0 = 10
        let m = measure(&c, &placements, &RouteOutput::default());
        assert!((m.hpwl_mm - 17.0).abs() < 1e-9);
        assert!((m.critical_hpwl_mm - 7.0).abs() < 1e-9); // only SLEW is critical
    }

    fn place(x: f64, y: f64) -> Placement {
        Placement {
            x_mm: x,
            y_mm: y,
            rotation_deg: 0.0,
            back: false,
        }
    }

    #[test]
    fn seeded_placer_puts_connected_parts_closer_than_alphabetical() {
        // Anchor U1 at one corner; C1 (critical net to U1) and a decoy far part.
        let c = Toy {
            parts: vec![Part::new("C1", "47n"), Part::new("U1", "op")],
            nets: vec![
                Net::new("SLEW", vec![PinRef::new("U1", "5"), PinRef::new("C1", "1")])
                    .with_class("Critical"),
            ],
        };
        let mut anchors = HashMap::new();
        anchors.insert("U1".to_string(), (5.0, 90.0)); // near the bottom
        let placer = SeededPlacer::new(40.0, 100.0, (0.0, 0.0), anchors);
        let mut facts = HashMap::new();
        let f = |w, h| PartFacts {
            extent: (w, h),
            origin_offset: (0.0, 0.0),
            side: crate::model::Side::Front,
        };
        facts.insert("C1".to_string(), f(5.0, 5.0));
        facts.insert("U1".to_string(), f(8.0, 8.0));
        let placements = placer.place(&c, &facts);
        let u1 = placements["U1"];
        let c1 = placements["C1"];
        // C1 seeds next to U1 (its only neighbour), not sprayed to the top row.
        let dist = (u1.x_mm - c1.x_mm).hypot(u1.y_mm - c1.y_mm);
        assert!(dist < 20.0, "C1 should seed near U1, got {dist}mm");
    }

    #[test]
    fn repair_nudges_are_deterministic_and_grow() {
        let free = vec!["C1".to_string(), "R1".to_string()];
        assert_eq!(repair_nudges(&free, 1), repair_nudges(&free, 1));
        let mag1 = mag(&repair_nudges(&free, 1)["C1"]);
        let mag3 = mag(&repair_nudges(&free, 3)["C1"]);
        assert!(mag3 > mag1, "later attempts perturb further");
    }

    fn mag((x, y): &(f64, f64)) -> f64 {
        x.hypot(*y)
    }
}
