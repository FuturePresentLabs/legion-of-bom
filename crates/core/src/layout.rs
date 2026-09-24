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

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::board::{generate_board_artifacts, BoardError, BoardOptions, Placement, SeededPlacer};
use crate::drc::{run_drc, DrcReport};
use crate::layout_repair::{decide_repair, RepairAction, RepairEvidence, RuleEvidence};
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

    /// The name this mode parses from — so a command can report the mode it
    /// actually used rather than echoing back the string it was given.
    pub fn as_str(self) -> &'static str {
        match self {
            LayoutMode::Analog => "analog",
            LayoutMode::Digital => "digital",
            LayoutMode::Mixed => "mixed",
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
#[derive(Debug, Clone, Default)]
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
    /// Cost of the design rules this placement broke ([`crate::rules`]), tiered
    /// so a higher tier cannot be traded away for a lower one.
    pub rule_penalty: f64,
    /// What was broken, worst first — surfaced in the report rather than
    /// silently priced in.
    pub violations: Vec<crate::rules::Violation>,
}

/// Measure a placement+route attempt. `placements` are part centres (board
/// coordinates); `route` is what the router produced.
pub fn measure(
    circuit: &dyn CircuitSource,
    placements: &HashMap<String, Placement>,
    route: &RouteOutput,
) -> PlacementMetrics {
    measure_against(circuit, placements, route, &crate::rules::derive(circuit))
}

/// [`measure`], against a rule set derived once by the caller — the loop
/// evaluates the same rules on every attempt and should not re-derive them.
pub fn measure_against(
    circuit: &dyn CircuitSource,
    placements: &HashMap<String, Placement>,
    route: &RouteOutput,
    rules: &[crate::rules::Rule],
) -> PlacementMetrics {
    let mut m = PlacementMetrics::default();
    m.violations = crate::rules::evaluate(rules, placements);
    m.rule_penalty = crate::rules::penalty(&m.violations);
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
    // Rule violations come first and are priced decades above the preference
    // terms. Without this the loop happily trades a decoupling cap across the
    // board for a few millimetres of copper — which is exactly what shipped.
    m.rule_penalty
        + w.wirelength * m.signal_hpwl_mm
        + w.critical * m.critical_hpwl_mm
        + w.via * m.via_count as f64
        + w.routed_len * m.routed_len_mm
        + w.unrouted * m.unrouted as f64
}

/// Consecutive non-improving attempts before the loop calls it done.
///
/// The cost of raising this is a full place+route per extra attempt; the cost of
/// lowering it to 1 is stopping one short of a candidate that would have helped,
/// since the spreading trajectory a placer offers is not monotonic — on
/// `daisy_panel_demo` the useful arrangement sits *after* a worse one.
const PATIENCE: usize = 2;

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

/// One deterministic point in the bounded layout/router policy sweep.
///
/// The values are multipliers over the caller's board options, not another
/// configuration surface. This keeps manufacturing geometry authoritative while
/// still trying a small, reproducible set of router trade-offs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutPolicy {
    pub name: &'static str,
    pub grid_scale: f64,
    pub via_cost_scale: f64,
    pub back_penalty_scale: f64,
    /// Only applies to automatically-sized outlines. Fixed/panel outlines are
    /// never silently enlarged.
    pub outline_margin_delta_mm: f64,
}

/// The stable policy ladder. Iterations beyond the ladder repeat it while the
/// placement repair sequence continues to explore new arrangements.
pub const LAYOUT_POLICIES: [LayoutPolicy; 4] = [
    LayoutPolicy {
        name: "balanced",
        grid_scale: 1.0,
        via_cost_scale: 1.0,
        back_penalty_scale: 1.0,
        outline_margin_delta_mm: 0.0,
    },
    LayoutPolicy {
        name: "fine_grid",
        grid_scale: 0.75,
        via_cost_scale: 1.0,
        back_penalty_scale: 1.0,
        outline_margin_delta_mm: 0.0,
    },
    LayoutPolicy {
        name: "via_friendly",
        grid_scale: 1.0,
        via_cost_scale: 0.7,
        back_penalty_scale: 0.5,
        outline_margin_delta_mm: 0.0,
    },
    LayoutPolicy {
        name: "roomy_coarse",
        grid_scale: 1.25,
        via_cost_scale: 1.2,
        back_penalty_scale: 1.0,
        outline_margin_delta_mm: 1.0,
    },
];

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
    /// Router budget/progress evidence from the winning attempt.
    pub routing: Option<crate::route::RoutingReport>,
    /// Mechanical clearance problems: parts under a stacked sub-board taller than
    /// its standoff (DESIGN 6.7). Surfaced, not auto-fixed.
    pub collisions: Vec<String>,
    /// Parts with no footprint at all — off-board hardware, absent from
    /// every attempt this loop ran (see [`crate::board::BoardArtifacts::not_placed`]).
    pub not_placed: Vec<String>,
    /// Final-gate DRC report, when `kicad_cli` was provided.
    pub drc: Option<DrcReport>,
    /// Human-facing observations (info/warning/error), incl. unresolved criticals.
    pub findings: Vec<Finding>,
    /// Bounded repair strategies selected during this run, in attempt order.
    pub repair_actions: Vec<RepairAction>,
    /// Policy which produced the winning candidate.
    pub policy: LayoutPolicy,
}

#[derive(Debug, Clone, Copy)]
struct CandidateRank {
    unrouted: usize,
    drc_errors: usize,
    broken: [f64; 3],
    area_mm2: f64,
    /// Deterministic router work proxy. Wall time remains report evidence, but
    /// cannot select a winner because scheduler noise would change the board.
    runtime_work: u64,
    preference: f64,
    policy_index: usize,
}

impl CandidateRank {
    fn cmp(&self, other: &Self) -> Ordering {
        self.unrouted
            .cmp(&other.unrouted)
            .then_with(|| self.drc_errors.cmp(&other.drc_errors))
            .then_with(|| cmp_f64_array(self.broken, other.broken))
            .then_with(|| self.area_mm2.total_cmp(&other.area_mm2))
            .then_with(|| self.runtime_work.cmp(&other.runtime_work))
            .then_with(|| self.preference.total_cmp(&other.preference))
            .then_with(|| self.policy_index.cmp(&other.policy_index))
    }
}

fn cmp_f64_array(left: [f64; 3], right: [f64; 3]) -> Ordering {
    left[0]
        .total_cmp(&right[0])
        .then_with(|| left[1].total_cmp(&right[1]))
        .then_with(|| left[2].total_cmp(&right[2]))
}

fn budget_share(total: Option<u64>, candidates: usize, index: usize) -> Option<u64> {
    total.map(|total| {
        let candidates = candidates.max(1) as u64;
        total / candidates + u64::from((index as u64) < total % candidates)
    })
}

/// Optional RLCD seam for repair strategy selection. The caller owns the
/// client and trace; the loop owns when (and how often) a decision is allowed.
pub struct RepairDecider<'a> {
    pub client: &'a dyn ooda::Client,
    pub trace: &'a mut ooda::Trace,
}

/// Run the iterative layout loop. `template` carries the panel dimensions and
/// anchored cutouts; each iteration rebuilds a [`SeededPlacer`] from it (with
/// repair nudges) and installs it into `options` before generating the board.
/// `options`' router, route settings, ground pour, and outline are used as-is.
pub fn run_layout_loop(
    circuit: &dyn CircuitSource,
    options: BoardOptions,
    template: SeededPlacer,
    cfg: &LayoutLoop,
) -> Result<LayoutReport, BoardError> {
    run_layout_loop_with_decider(circuit, options, template, cfg, None)
}

/// [`run_layout_loop`] with bounded RLCD repair selection enabled.
pub fn run_layout_loop_with_decider(
    circuit: &dyn CircuitSource,
    mut options: BoardOptions,
    template: SeededPlacer,
    cfg: &LayoutLoop,
    mut decider: Option<RepairDecider<'_>>,
) -> Result<LayoutReport, BoardError> {
    // One placement attempt's result, so the loop can keep the best by score.
    struct Attempt {
        /// Millimetres broken per tier, worst tier first — the relaxation key.
        broken: [f64; 3],
        /// Connections the router could not make. Ranks above every preference.
        unrouted: usize,
        score: f64,
        board: String,
        metrics: PlacementMetrics,
        unresolved: Vec<String>,
        routing: Option<crate::route::RoutingReport>,
        collisions: Vec<String>,
        not_placed: Vec<String>,
        drc: Option<DrcReport>,
        rank: CandidateRank,
        policy: LayoutPolicy,
    }

    let weights = cfg.mode.weights();
    // Size-aware rules: how close a cap *can* get to its chip depends on how big
    // both are, and a limit smaller than that floor can never be met.
    let facts = crate::board::build_facts(circuit, &options.footprint_dir).ok();
    let rules = crate::rules::derive_in(
        circuit,
        &crate::rules::Context {
            facts: facts.as_ref(),
            outline: options.fixed_outline,
        },
    );
    let iters = cfg.max_iters.max(1);
    let base_grid_mm = options.route_options.grid_mm;
    let base_via_cost_mm = options.route_options.via_cost_mm;
    let base_back_penalty_mm = options.route_options.back_penalty_mm;
    let base_outline_margin_mm = options.outline_margin_mm;
    let total_expansions = options.route_options.max_expansions;
    let total_wall_time_ms = options.route_options.max_wall_time_ms;

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
    // Consecutive attempts that did not improve on the best so far.
    let mut stale = 0usize;
    let mut repair_actions = Vec::new();

    for i in 0..iters {
        ran += 1;
        let policy_index = i % LAYOUT_POLICIES.len();
        let policy = LAYOUT_POLICIES[policy_index];
        options.route_options.grid_mm = base_grid_mm * policy.grid_scale;
        options.route_options.via_cost_mm = base_via_cost_mm * policy.via_cost_scale;
        options.route_options.back_penalty_mm = base_back_penalty_mm * policy.back_penalty_scale;
        options.outline_margin_mm = if options.fixed_outline.is_none() {
            base_outline_margin_mm + policy.outline_margin_delta_mm
        } else {
            base_outline_margin_mm
        };
        // The CLI budgets the complete layout search, not every candidate. A
        // hard board therefore cannot multiply its allowance by `max_iters`.
        options.route_options.max_expansions = budget_share(total_expansions, iters, i);
        options.route_options.max_wall_time_ms = budget_share(total_wall_time_ms, iters, i);
        let mut placer = template.clone();
        placer.nudges = nudges.clone();
        options.placer = Box::new(placer);

        let art = generate_board_artifacts(circuit, &options)?;
        let metrics = measure_against(circuit, &art.placements, &art.route, &rules);
        // Snapshot before `metrics` potentially moves into `Attempt` below —
        // repair_nudges needs this attempt's violations after that point.
        let violations_this_attempt = metrics.violations.clone();
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

        // Snapshot the complete decision observation before the winning-attempt
        // branch moves `metrics`/`drc` into storage.
        let repair_evidence = RepairEvidence {
            attempt: i + 1,
            attempts_remaining: iters - i - 1,
            unrouted_connections: art.route.conflicts.len(),
            route_conflicts: art.route.conflicts.clone(),
            rule_violations: violations_this_attempt
                .iter()
                .map(|v| RuleEvidence {
                    tier: format!("{:?}", v.tier).to_ascii_lowercase(),
                    by_mm: v.by_mm,
                    description: v.what.clone(),
                    repairable: v.repair.is_some(),
                })
                .collect(),
            drc_errors: drc.as_ref().map_or(0, DrcReport::error_count),
            drc_error_kinds: drc
                .as_ref()
                .map(|r| r.errors().map(|v| v.kind.clone()).collect())
                .unwrap_or_default(),
            signal_hpwl_mm: metrics.signal_hpwl_mm,
            critical_hpwl_mm: metrics.critical_hpwl_mm,
            via_count: metrics.via_count,
        };

        let (unrouted, penalty) = (metrics.unrouted, metrics.rule_penalty);
        let broken = crate::rules::by_tier(&metrics.violations);
        let preference = sc - penalty;
        // Fabrication ordering: connectivity and actual DRC decide first. When
        // DRC is unavailable or tied, the black-book rule tiers prevent a
        // candidate buying compactness with a known physical/electrical defect.
        let drc_errors = drc.as_ref().map_or(0, DrcReport::error_count);
        let area_mm2 = options.fixed_outline.map_or_else(
            || {
                (template.width_mm + 2.0 * options.outline_margin_mm)
                    * (template.height_mm + 2.0 * options.outline_margin_mm)
            },
            |(x0, y0, x1, y1)| (x1 - x0).abs() * (y1 - y0).abs(),
        );
        let rank = CandidateRank {
            unrouted,
            drc_errors,
            broken,
            area_mm2,
            runtime_work: art.route.report.as_ref().map_or(0, |r| r.expansions),
            preference,
            policy_index,
        };
        let improved = best.as_ref().is_none_or(|b| rank.cmp(&b.rank).is_lt());
        stale = if improved { 0 } else { stale + 1 };
        if improved {
            best = Some(Attempt {
                broken,
                unrouted,
                score: sc,
                board: art.pcb,
                metrics,
                unresolved: art.route.conflicts.clone(),
                routing: art.route.report.clone(),
                collisions: art.collisions.clone(),
                not_placed: art.not_placed,
                drc,
                rank,
                policy,
            });
        }

        // Stop when attempts stop helping — not when one merely comes out clean.
        //
        // This used to exit the moment `unrouted == 0 && penalty <= 0.0`, on the
        // reasoning that a clean board is a finished board. It is not: it is the
        // *first* acceptable board, and the loop's whole purpose is to be a
        // fine-tuning stage. Two ways that bit. Under analytical placement the
        // first attempt is usually already clean, so fine-tuning never ran at
        // all. And on `daisy_panel_demo` the first *routable* attempt was a badly
        // spread one scoring 739.6, which the loop then returned while a 307.8
        // was two candidates further down the list (`legion-of-bom-lso`).
        //
        // Clean is now the floor, not the finish line: keep going while attempts
        // improve, and give up after [`PATIENCE`] consecutive ones that do not.
        //
        // Giving up early is only allowed once there is something worth keeping.
        // Patience is a stop rule for *polishing*, and applying it to a board
        // that is still broken is just quitting: on the real 5 HP slew limiter it
        // ended the search after 3 attempts holding a board with a physical rule
        // violation, where spending the full budget finds a clean one. While the
        // best attempt so far still breaks a rule or leaves a net unrouted, the
        // whole iteration budget is on the table.
        let acceptable = best
            .as_ref()
            .is_some_and(|b| b.unrouted == 0 && b.broken.iter().all(|&mm| mm <= 0.0));
        if acceptable && stale >= PATIENCE {
            break;
        }
        // Last iteration — no point planning another repair.
        if i + 1 == iters {
            break;
        }
        // Repair: perturb the free parts so the next attempt explores a different
        // arrangement the router may find easier (DESIGN §6.5 step 4). Deterministic
        // shake — no RNG — so each attempt is a clean, reproducible git diff.
        let action = match decider.as_mut() {
            Some(d) => decide_repair(d.client, d.trace, &repair_evidence)
                .map_err(|e| BoardError::Other(format!("layout repair decision failed: {e}")))?,
            None if repair_evidence.rule_violations.iter().any(|v| v.repairable) => {
                RepairAction::FollowRuleHints
            }
            None => RepairAction::ExploreLocal,
        };
        repair_actions.push(action);
        nudges = repair_nudges(
            &free,
            i + 1,
            &art.placements,
            &violations_this_attempt,
            action,
        );
    }

    let Attempt {
        broken: _,
        unrouted: _,
        score,
        board,
        metrics,
        unresolved,
        routing,
        collisions,
        not_placed,
        mut drc,
        rank: _,
        policy,
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
    if !not_placed.is_empty() {
        findings.push(Finding::info(format!(
            "not placed (no footprint — off-board hardware): {}",
            not_placed.join(", ")
        )));
    }
    // What the winning layout had to break to fit, and by how much. Reported at
    // the severity of the tier it broke: a physical rule means the board cannot
    // be built, an electrical one means it will work worse than intended. This
    // used to be absorbed silently into the score, which is how a decoupling cap
    // shipped 89mm from its chip without anything saying so.
    if metrics.violations.is_empty() {
        findings.push(Finding::info("all design rules met"));
    } else {
        for v in &metrics.violations {
            let msg = format!("relaxed by {:.1}mm — {}", v.by_mm, v.what);
            findings.push(match v.tier {
                crate::rules::Tier::Physical => Finding::error(msg),
                crate::rules::Tier::Electrical => Finding::warning(msg),
                crate::rules::Tier::Preference => Finding::info(msg),
            });
        }
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
        routing,
        collisions,
        not_placed,
        drc,
        findings,
        repair_actions,
        policy,
    })
}

/// Deterministic repair perturbation: nudge each free part by a golden-angle
/// offset that varies with the attempt, so successive attempts explore different
/// arrangements without any RNG. Magnitude grows with the attempt number.
/// A part a rule violation names (`Violation.repair`, already computed by
/// `rules::assess` — see e.g. `Rule::Proximity`'s "move the cap to its IC"
/// hint) steps partway toward that real, targeted destination instead of
/// guessing. Every other free part still gets the golden-angle exploration
/// nudge — the loop's purpose is broader than fixing violations (DESIGN
/// §6.5 step 4: finding a better *arrangement*, wirelength included, even
/// where nothing is strictly broken), so an un-implicated part keeps
/// exploring rather than sitting still.
///
/// The guided step is a fixed fraction ([`REPAIR_STEP`]) of the hinted
/// distance, not the whole way — a hint is computed against *one* current
/// violation, and the board moves when other parts move too, so jumping
/// straight to it risks overshooting into a new violation next attempt.
/// Magnitude for the unguided golden-angle parts still grows with the
/// attempt number, exactly as before.
fn repair_nudges(
    free: &[String],
    attempt: usize,
    placements: &HashMap<String, Placement>,
    violations: &[crate::rules::Violation],
    action: RepairAction,
) -> HashMap<String, (f64, f64)> {
    const GOLDEN_ANGLE: f64 = 2.399_963_229_728_653; // radians
    /// Fraction of a repair hint's distance to actually move each attempt.
    const REPAIR_STEP: f64 = 0.6;
    let mag = match action {
        RepairAction::FollowRuleHints | RepairAction::ExploreLocal => 2.0 + 1.5 * attempt as f64,
        RepairAction::ExploreWide => 2.0 * (2.0 + 1.5 * attempt as f64),
    };

    // Last violation naming a part wins if several do — one guided step per
    // part per attempt, same as everything else in this loop.
    let mut targeted: HashMap<&str, (f64, f64)> = HashMap::new();
    for v in violations {
        if let Some(r) = &v.repair {
            targeted.insert(r.refdes.as_str(), r.toward_mm);
        }
    }

    free.iter()
        .enumerate()
        .map(|(k, r)| {
            if action == RepairAction::FollowRuleHints {
                if let (Some(&(tx, ty)), Some(p)) = (targeted.get(r.as_str()), placements.get(r)) {
                    return (
                        r.clone(),
                        ((tx - p.x_mm) * REPAIR_STEP, (ty - p.y_mm) * REPAIR_STEP),
                    );
                }
            }
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
    // Unique per board *content*, not just per circuit: the HP search writes a
    // different board for every candidate width, and a shared path means one
    // trial can be read as another's.
    let stamp: String = {
        use sha2::{Digest, Sha256};
        Sha256::digest(board.as_bytes())
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let path =
        std::env::temp_dir().join(format!("lob_layout_{}_{stamp}.kicad_pcb", circuit.name()));
    std::fs::write(&path, board)?;
    run_drc(&path, kicad_cli).map_err(|e| BoardError::Other(e.to_string()))
}

/// Options + seeded template for a **trial build** of a Eurorack module at `hp`,
/// set up exactly as a real build is at that width: panel derived from the
/// circuit, its cutouts anchored, outline fixed to the panel, everything else
/// left at [`BoardOptions::new`]'s defaults.
///
/// This lives here, rather than in each caller, because "exactly as" is the
/// load-bearing part and a second copy is a second chance to drift from it. A
/// sizing trial configured differently from the build measures a board nobody
/// ships: `examples/board_preview` overrode the router and reported ~69 DRC
/// errors against a shipped board's 5, misleading this work twice
/// (`legion-of-bom-nz1`).
///
/// A caller with a *hand-authored* panel or placement should pass its own
/// closure to [`minimum_routable_hp`] instead — this one derives the panel, which
/// is only right when nobody has laid the module out by hand.
pub fn eurorack_trial_build(
    circuit: &dyn CircuitSource,
    footprint_dir: &std::path::Path,
    hp: u16,
) -> Result<(BoardOptions, SeededPlacer), BoardError> {
    use crate::panel::{BuiltinCutouts, EurorackPanel, PanelSpec};
    let dims = EurorackPanel::new(hp);
    let (w, h) = (dims.width_mm(), dims.height_mm());
    let anchors: HashMap<String, (f64, f64)> =
        crate::panel::derive_panel(circuit, hp, &BuiltinCutouts)
            .cutouts
            .iter()
            .filter_map(|c| c.refdes.clone().map(|r| (r, (c.x_mm, h - c.y_mm))))
            .collect();
    // Centre on KiCad's A4 sheet, as the CLI does, rather than the (0,0) corner.
    let origin = (((297.0 - w) / 2.0).max(10.0), ((210.0 - h) / 2.0).max(10.0));
    let mut opts = BoardOptions::new(footprint_dir);
    opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));
    opts.placer = Box::new(crate::board::EurorackPlacer {
        width_mm: w,
        height_mm: h,
        origin_mm: origin,
        anchors: anchors.clone(),
    });
    Ok((opts, SeededPlacer::new(w, h, origin, anchors)))
}

/// The panel widths a Eurorack module is actually sold in, from `floor` upward.
///
/// Even HP, plus 3 — that is the convention, and 5, 7 or 9 HP reads as a mistake
/// to anyone buying a module. There is no technical reason a 7 HP panel cannot
/// be cut; it just is not a width the format uses, so offering one as "the
/// minimum buildable width" is offering something nobody wants.
fn conventional_widths(floor: u16) -> impl Iterator<Item = u16> {
    (floor..=u16::MAX).filter(|hp| *hp == 2 || *hp == 3 || hp % 2 == 0)
}

/// How far above the geometric floor to look for a width that actually builds.
#[derive(Debug, Clone)]
pub struct HpSearch {
    /// Widths to trial, starting at the floor, before giving up. Each one costs a
    /// full place → route → DRC, so this is deliberately small: if a board needs
    /// four more HP than its parts occupy, the answer is a layout problem, not a
    /// wider search.
    pub max_widths: u16,
}

impl Default for HpSearch {
    fn default() -> Self {
        HpSearch { max_widths: 4 }
    }
}

/// What one candidate width did when actually built.
#[derive(Debug, Clone)]
pub struct HpTrial {
    pub hp: u16,
    /// DRC errors at this width; `None` if the board could not be generated.
    pub errors: Option<usize>,
    /// Error counts per DRC rule, so a rejection says *what* was wrong.
    pub kinds: Vec<(String, usize)>,
}

/// The narrowest width proven buildable, and the evidence for it.
#[derive(Debug, Clone)]
pub struct RoutableHp {
    /// The narrowest width that routed DRC-clean. `None` means no width in range
    /// did — which is a real answer, not a failure to compute one.
    pub hp: Option<u16>,
    /// The geometric floor the search started from: the width the parts *fit* in.
    pub floor_hp: u16,
    /// Each width tried, narrowest first.
    pub tried: Vec<HpTrial>,
    /// DRC never ran (no `kicad-cli`), so nothing here is proven.
    pub unproven: bool,
}

/// The narrowest width the circuit actually **builds** in — placed, routed, and
/// gated on real KiCad DRC — searching upward from a geometric floor.
///
/// [`minimum_hp`](crate::board::minimum_hp) answers a different and weaker
/// question: does the parts' copper *fit* between the edges. That is a genuine
/// lower bound and a fast one, but a board can fit and still be unbuildable
/// because the router cannot complete every net in the space left over. Reporting
/// the fit answer as the minimum width is what produced a 3 HP slew limiter with
/// parts hanging off the edge (`legion-of-bom-t5t`).
///
/// `configure` supplies the options and placer template for a given width, and it
/// must be **the same configuration the caller will really build with**. This is
/// the whole point of the seam: the sizing harness in `examples/board_preview`
/// spent two rounds of this work drawing DRC conclusions from a router the CLI
/// never uses (`legion-of-bom-nz1`), and a trial that does not match the build
/// proves nothing about the build.
///
/// Degrades gracefully: with no `kicad_cli` in `cfg`, routability cannot be
/// checked at all, so this returns `unproven` rather than guessing.
pub fn minimum_routable_hp<F>(
    circuit: &dyn CircuitSource,
    floor_hp: u16,
    search: &HpSearch,
    cfg: &LayoutLoop,
    mut configure: F,
) -> RoutableHp
where
    F: FnMut(u16) -> Result<(BoardOptions, SeededPlacer), BoardError>,
{
    let mut out = RoutableHp {
        hp: None,
        floor_hp,
        tried: Vec::new(),
        unproven: cfg.kicad_cli.is_none(),
    };
    if out.unproven {
        return out;
    }
    for hp in conventional_widths(floor_hp).take(search.max_widths.max(1) as usize) {
        let built = configure(hp)
            .and_then(|(options, template)| run_layout_loop(circuit, options, template, cfg));
        // A width that cannot even be generated is recorded and stepped past: the
        // next one up may well work, and that is the question being asked.
        let Ok(report) = built else {
            out.tried.push(HpTrial {
                hp,
                errors: None,
                kinds: Vec::new(),
            });
            continue;
        };
        // No DRC report despite a kicad-cli means the gate could not run. Treat it
        // as unproven rather than silently accepting the width.
        let Some(drc) = report.drc else {
            out.tried.push(HpTrial {
                hp,
                errors: None,
                kinds: Vec::new(),
            });
            continue;
        };
        let mut by_kind: std::collections::BTreeMap<&str, usize> = Default::default();
        for v in drc
            .violations
            .iter()
            .chain(&drc.unconnected_items)
            .filter(|v| v.severity == "error")
        {
            *by_kind.entry(v.kind.as_str()).or_default() += 1;
        }
        let mut kinds: Vec<(String, usize)> = by_kind
            .into_iter()
            .map(|(k, n)| (k.to_string(), n))
            .collect();
        kinds.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let errors = drc.error_count();
        out.tried.push(HpTrial {
            hp,
            errors: Some(errors),
            kinds,
        });
        if errors == 0 {
            out.hp = Some(hp);
            break;
        }
    }
    out
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

    /// Without `kicad-cli` there is no way to check routability, so the search
    /// must say it could not prove anything rather than hand back the floor as if
    /// it had — quoting an unproven width as buildable is the original bug.
    #[test]
    fn with_no_kicad_cli_the_search_proves_nothing() {
        let cfg = LayoutLoop {
            kicad_cli: None,
            ..LayoutLoop::default()
        };
        let mut called = 0;
        let out = minimum_routable_hp(&toy(), 4, &HpSearch::default(), &cfg, |_| {
            called += 1;
            Err(BoardError::Other("should not be reached".into()))
        });
        assert!(out.unproven);
        assert_eq!(out.hp, None);
        assert_eq!(out.floor_hp, 4);
        assert!(out.tried.is_empty());
        assert_eq!(called, 0, "no width should be built when DRC cannot run");
    }

    /// A width that cannot even be configured is recorded and stepped past — the
    /// next one up may well build, and that is the question being asked.
    #[test]
    fn a_width_that_cannot_be_built_is_recorded_and_the_search_continues() {
        let cfg = LayoutLoop {
            kicad_cli: Some(PathBuf::from("/nonexistent/kicad-cli")),
            ..LayoutLoop::default()
        };
        let search = HpSearch { max_widths: 3 };
        let out = minimum_routable_hp(&toy(), 6, &search, &cfg, |_| {
            Err(BoardError::Other("no footprints".into()))
        });
        assert!(!out.unproven);
        assert_eq!(out.hp, None, "nothing built, so nothing is proven");
        let widths: Vec<u16> = out.tried.iter().map(|t| t.hp).collect();
        // 7 HP is skipped: even HP plus 3 is what modules are sold in.
        assert_eq!(widths, vec![6, 8, 10], "every conventional width was tried");
        assert!(out.tried.iter().all(|t| t.errors.is_none()));
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
            ..tight.clone()
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
            body_extent: (w, h),
            origin_offset: (0.0, 0.0),
            side: crate::model::Side::Front,
            height_mm: 2.0,
            standoff_mm: None,
            tht_pads: Vec::new(),
            pin_offsets: HashMap::new(),
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

    fn candidate(
        unrouted: usize,
        drc_errors: usize,
        area_mm2: f64,
        runtime_work: u64,
        preference: f64,
    ) -> CandidateRank {
        CandidateRank {
            unrouted,
            drc_errors,
            broken: [0.0; 3],
            area_mm2,
            runtime_work,
            preference,
            policy_index: 0,
        }
    }

    #[test]
    fn candidate_order_is_connectivity_then_drc_then_area_work_preference() {
        let routed = candidate(0, 99, 10_000.0, 99_000, 99_000.0);
        let unrouted = candidate(1, 0, 1.0, 1, 1.0);
        assert!(routed.cmp(&unrouted).is_lt());

        let drc_clean = candidate(0, 0, 10_000.0, 99_000, 99_000.0);
        let drc_broken = candidate(0, 1, 1.0, 1, 1.0);
        assert!(drc_clean.cmp(&drc_broken).is_lt());

        let rules_clean = candidate(0, 0, 10_000.0, 99_000, 99_000.0);
        let mut rules_broken = candidate(0, 0, 1.0, 1, 1.0);
        rules_broken.broken[0] = 0.1;
        assert!(rules_clean.cmp(&rules_broken).is_lt());

        assert!(candidate(0, 0, 99.0, 999, 999.0)
            .cmp(&candidate(0, 0, 100.0, 1, 1.0))
            .is_lt());
        assert!(candidate(0, 0, 100.0, 9, 999.0)
            .cmp(&candidate(0, 0, 100.0, 10, 1.0))
            .is_lt());
        assert!(candidate(0, 0, 100.0, 10, 1.0)
            .cmp(&candidate(0, 0, 100.0, 10, 2.0))
            .is_lt());
    }

    #[test]
    fn policy_ladder_and_budget_split_are_deterministic_and_bounded() {
        assert_eq!(
            LAYOUT_POLICIES.map(|policy| policy.name),
            ["balanced", "fine_grid", "via_friendly", "roomy_coarse"]
        );
        let shares: Vec<u64> = (0..6)
            .map(|index| budget_share(Some(10), 6, index).unwrap())
            .collect();
        assert_eq!(shares, [2, 2, 2, 2, 1, 1]);
        assert_eq!(shares.iter().sum::<u64>(), 10);
        assert_eq!(budget_share(None, 6, 0), None);
    }

    #[test]
    fn violations_total_by_tier() {
        use crate::rules::{by_tier, Tier, Violation};
        let v = |tier, by_mm| Violation {
            tier,
            by_mm,
            what: String::new(),
            repair: None,
        };
        let got = by_tier(&[
            v(Tier::Electrical, 1.5),
            v(Tier::Electrical, 2.0),
            v(Tier::Physical, 0.25),
        ]);
        assert_eq!(got, [0.25, 3.5, 0.0]);
        assert_eq!(by_tier(&[]), [0.0; 3]);
    }

    #[test]
    fn repair_nudges_are_deterministic_and_grow() {
        let free = vec!["C1".to_string(), "R1".to_string()];
        let placements = HashMap::new();
        let violations = Vec::new();
        assert_eq!(
            repair_nudges(
                &free,
                1,
                &placements,
                &violations,
                RepairAction::ExploreLocal
            ),
            repair_nudges(
                &free,
                1,
                &placements,
                &violations,
                RepairAction::ExploreLocal
            )
        );
        let mag1 = mag(&repair_nudges(
            &free,
            1,
            &placements,
            &violations,
            RepairAction::ExploreLocal,
        )["C1"]);
        let mag3 = mag(&repair_nudges(
            &free,
            3,
            &placements,
            &violations,
            RepairAction::ExploreLocal,
        )["C1"]);
        assert!(mag3 > mag1, "later attempts perturb further");
    }

    #[test]
    fn repair_nudges_steps_a_named_part_toward_its_real_hint_not_a_blind_spiral() {
        // C1 sits at (0,0); a violation says it should head to (10,0) — same
        // shape as Rule::Proximity's real "move the cap to its IC" repair.
        // R1 has no violation naming it at all, so it must still get the
        // ordinary golden-angle exploration nudge, unaffected.
        let free = vec!["C1".to_string(), "R1".to_string()];
        let mut placements = HashMap::new();
        placements.insert(
            "C1".to_string(),
            Placement {
                x_mm: 0.0,
                y_mm: 0.0,
                rotation_deg: 0.0,
                back: false,
            },
        );
        let violations = vec![crate::rules::Violation {
            tier: crate::rules::Tier::Electrical,
            by_mm: 3.0,
            what: "C1 too far from its IC".into(),
            repair: Some(crate::rules::Repair {
                refdes: "C1".to_string(),
                toward_mm: (10.0, 0.0),
            }),
        }];

        let guided = repair_nudges(
            &free,
            1,
            &placements,
            &violations,
            RepairAction::FollowRuleHints,
        );
        // 0.6 (REPAIR_STEP) of the 10mm gap toward the hint, straight along X.
        assert!(
            (guided["C1"].0 - 6.0).abs() < 1e-9,
            "got {:?}",
            guided["C1"]
        );
        assert!(
            (guided["C1"].1 - 0.0).abs() < 1e-9,
            "got {:?}",
            guided["C1"]
        );

        // R1 (unmentioned by any violation, and absent from placements too)
        // matches the plain golden-angle nudge exactly, as if there were no
        // violations at all.
        let unguided = repair_nudges(
            &free,
            1,
            &HashMap::new(),
            &Vec::new(),
            RepairAction::FollowRuleHints,
        );
        assert_eq!(guided["R1"], unguided["R1"]);
    }

    fn mag((x, y): &(f64, f64)) -> f64 {
        x.hypot(*y)
    }
}
