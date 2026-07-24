//! SPICE deck generation + ngspice AC simulation. DESIGN.md 5.1.
//!
//! Generates a SPICE deck from a [`CircuitSource`], wraps it in a small AC test
//! harness (a 1 V AC source on the input net, an `.ac` sweep), runs `ngspice -b`,
//! and parses the frequency response back out.
//!
//! Parts are emitted from the [`SpiceModel`] they carry (resolved from the
//! component by [`crate::symbols`]): primitives (R/C/L) directly, modelled
//! devices as subckt instances with `.include` and supply rails. The generator
//! has no per-device knowledge — the model travels with the component.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::source::CircuitSource;
use crate::stage::StageError;
use crate::tools::find_on_path;

/// How a part is instantiated in SPICE. Carried by the component (resolved from
/// its symbol, and later the parts library), never special-cased in the
/// generator. Primitives (R/C/L) have no `SpiceModel` — they're emitted directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpiceModel {
    /// A subcircuit instance: `X<ref> <nodes…> <subckt>`, with `nodes` taken
    /// from the part's pins in `pin_order`, and `.include <include>` emitted once.
    Subckt {
        /// The `.subckt` name to instantiate.
        subckt: String,
        /// SPICE library file to `.include`.
        include: PathBuf,
        /// Part pin numbers in the subckt's terminal order.
        pin_order: Vec<String>,
        /// Optional parameter string (e.g. `GAIN=20k`); `None` uses subckt defaults.
        params: Option<String>,
    },
}

/// A logarithmic AC sweep (points per decade, from `start_hz` to `stop_hz`).
#[derive(Debug, Clone)]
pub struct AcSweep {
    pub points_per_decade: u32,
    pub start_hz: f64,
    pub stop_hz: f64,
}

impl Default for AcSweep {
    fn default() -> Self {
        AcSweep {
            points_per_decade: 100,
            start_hz: 1.0,
            stop_hz: 1e6,
        }
    }
}

/// Which nets form the AC test harness, plus the sweep. The defaults encode the
/// Phase 0 convention: drive `IN`, probe `OUT`, `GND`/`0` is ground, and any
/// `VCC`/`VEE` supply nets get ±15 V rails.
#[derive(Debug, Clone)]
pub struct SimConfig {
    pub input_net: String,
    pub output_net: String,
    pub ground_nets: Vec<String>,
    /// Supply nets and their DC voltage; a source is emitted for each that the
    /// circuit actually uses (e.g. an op-amp's `VCC`/`VEE`).
    pub supplies: Vec<(String, f64)>,
    pub ac: AcSweep,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            input_net: "IN".into(),
            output_net: "OUT".into(),
            ground_nets: vec!["GND".into(), "0".into()],
            supplies: vec![("VCC".into(), 15.0), ("VEE".into(), -15.0)],
            ac: AcSweep::default(),
        }
    }
}

/// Candidate signal-input / signal-output net names, most-specific first.
const INPUT_NETS: &[&str] = &["IN", "SIG_IN", "INPUT", "IN_L", "AUDIO_IN", "AUDIO_IN_L"];
const OUTPUT_NETS: &[&str] = &[
    "OUT",
    "SIG_OUT",
    "OUTPUT",
    "OUT_L",
    "AUDIO_OUT",
    "AUDIO_OUT_L",
];

/// The DC voltage a net name implies if it's a supply rail — `+12V`→+12, `-12V`→
/// −12, `+5V`→5, `+3V3`→3.3, or the named rails VCC/VDD (+12) / VEE/VSS (−12).
/// `None` for anything that isn't rail-shaped (signals, ground, IABC, …).
fn rail_voltage(name: &str) -> Option<f64> {
    let up = name.trim().to_ascii_uppercase();
    match up.as_str() {
        "VCC" | "VDD" => return Some(12.0),
        "VEE" | "VSS" => return Some(-12.0),
        _ => {}
    }
    // Explicit rails start with a sign and carry a voltage: +12V, -12V, +3V3.
    let sign = match up.as_bytes().first() {
        Some(b'+') => 1.0,
        Some(b'-') => -1.0,
        _ => return None,
    };
    let body = &up[1..];
    if !body.contains('V') {
        return None;
    }
    // `12V`→`12`, `3V3`→`3.3`, `3.3V`→`3.3`.
    let num = body.trim_end_matches('V').replace('V', ".");
    num.parse::<f64>().ok().map(|v| sign * v)
}

impl SimConfig {
    /// Infer the I/O and supply nets from the circuit's own net names, so the
    /// harness adapts to a real board (`SIG_IN`/`SIG_OUT`, `+12V`/`-12V`) instead
    /// of assuming `IN`/`OUT`/`±15 V`. Falls back to [`default`](Self::default)
    /// names/supplies when nothing matches. (tus.9)
    pub fn infer(circuit: &dyn CircuitSource) -> SimConfig {
        let names: Vec<&str> = circuit.nets().iter().map(|n| n.name.as_str()).collect();
        let first = |cands: &[&str], fallback: &str| -> String {
            for c in cands {
                if let Some(n) = names.iter().find(|n| n.eq_ignore_ascii_case(c)) {
                    return n.to_string();
                }
            }
            fallback.to_string()
        };
        let supplies: Vec<(String, f64)> = names
            .iter()
            .filter_map(|n| rail_voltage(n).map(|v| (n.to_string(), v)))
            .collect();
        SimConfig {
            input_net: first(INPUT_NETS, "IN"),
            output_net: first(OUTPUT_NETS, "OUT"),
            ground_nets: vec!["GND".into(), "0".into()],
            supplies: if supplies.is_empty() {
                SimConfig::default().supplies
            } else {
                supplies
            },
            ac: AcSweep::default(),
        }
    }
}

impl SimConfig {
    fn is_ground(&self, net: &str) -> bool {
        self.ground_nets.iter().any(|g| g.eq_ignore_ascii_case(net))
    }

    /// SPICE node name for a net: ground nets collapse to `0`.
    fn node(&self, net: &str) -> String {
        if self.is_ground(net) {
            "0".into()
        } else {
            net.to_string()
        }
    }
}

/// One point of an AC frequency response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcPoint {
    pub freq_hz: f64,
    pub mag_db: f64,
}

/// A parsed AC frequency response.
#[derive(Debug, Clone)]
pub struct AcResult {
    pub points: Vec<AcPoint>,
}

/// Half-power point: 20·log10(1/√2) ≈ −3.0103 dB below the passband.
const HALF_POWER_DB: f64 = 3.010_299_956_639_812;

impl AcResult {
    /// Passband gain (dB) — approximated by the lowest-frequency point, which is
    /// the flat region for a low-pass.
    pub fn passband_gain_db(&self) -> Option<f64> {
        self.points.first().map(|p| p.mag_db)
    }

    /// The −3 dB cutoff frequency, interpolated (in log-frequency) at the first
    /// downward crossing of `passband − 3.0103 dB`. Assumes a low-pass shape.
    pub fn cutoff_3db_hz(&self) -> Option<f64> {
        let reference = self.passband_gain_db()?;
        let target = reference - HALF_POWER_DB;
        for w in self.points.windows(2) {
            let (a, b) = (w[0], w[1]);
            // First segment crossing the target on the way down.
            if a.mag_db >= target
                && b.mag_db <= target
                && (a.mag_db - b.mag_db).abs() > f64::EPSILON
            {
                let frac = (target - a.mag_db) / (b.mag_db - a.mag_db);
                let log_f = a.freq_hz.ln() + frac * (b.freq_hz.ln() - a.freq_hz.ln());
                return Some(log_f.exp());
            }
        }
        None
    }
}

/// Build the pin → net-name lookup for the whole circuit.
fn pin_net_map(circuit: &dyn CircuitSource) -> HashMap<(String, String), String> {
    let mut map = HashMap::new();
    for net in circuit.nets() {
        for pin in &net.pins {
            map.insert((pin.refdes.0.clone(), pin.pin.clone()), net.name.clone());
        }
    }
    map
}

/// Format a number for a SPICE card without scientific notation.
fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// Error if a net the harness needs (input/output) isn't in the circuit.
fn require_net(
    circuit: &dyn CircuitSource,
    net_names: &HashSet<&str>,
    required: &str,
) -> Result<(), StageError> {
    if net_names.contains(required) {
        return Ok(());
    }
    Err(StageError::Other(format!(
        "simulation needs a net named '{required}' (have: {})",
        circuit
            .nets()
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Emit the circuit body — `.include`s and component cards — from each part's
/// resolved SPICE model (subckt instance) or, for R/C/L, a primitive. Shared by
/// the AC and transient decks; the generator has no per-device knowledge.
fn netlist_body(
    circuit: &dyn CircuitSource,
    config: &SimConfig,
    models: &HashMap<String, SpiceModel>,
) -> Result<(BTreeSet<PathBuf>, Vec<String>), StageError> {
    let pinmap = pin_net_map(circuit);
    let node_at = |refdes: &str, pin: &str| -> Option<String> {
        pinmap
            .get(&(refdes.to_string(), pin.to_string()))
            .map(|net| config.node(net))
    };

    let mut includes: BTreeSet<PathBuf> = BTreeSet::new();
    let mut components = Vec::new();
    let mut unsupported = Vec::new();

    for part in circuit.parts() {
        let refdes = &part.refdes.0;

        // Connectors and mechanical parts (jacks, headers, mounting holes, test
        // points) carry no SPICE device — their pins are just net junctions. Skip
        // them rather than demanding a model.
        if is_electrical_noop(part) {
            continue;
        }

        // A part that carries a SPICE model is instantiated from that model.
        if let Some(SpiceModel::Subckt {
            subckt,
            include,
            pin_order,
            params,
        }) = models.get(refdes)
        {
            includes.insert(include.clone());
            let mut nodes = Vec::with_capacity(pin_order.len());
            for pin in pin_order {
                let node = node_at(refdes, pin).ok_or_else(|| {
                    StageError::Other(format!("{refdes}: pin {pin} is not connected to a net"))
                })?;
                nodes.push(node);
            }
            let mut line = format!("X{refdes} {} {subckt}", nodes.join(" "));
            if let Some(params) = params {
                line.push(' ');
                line.push_str(params);
            }
            components.push(line);
            continue;
        }

        // A potentiometer (`RV…`, 3 pins: 1–wiper(2)–3) → two resistors split at a
        // nominal 50% wiper. Without this the wiper net floats and the DC operating
        // point can't be found.
        if refdes.to_ascii_uppercase().starts_with("RV") {
            if let (Some(n1), Some(nw), Some(n3)) = (
                node_at(refdes, "1"),
                node_at(refdes, "2"),
                node_at(refdes, "3"),
            ) {
                let half = crate::units::parse_eng_value(&part.value)
                    .map(|v| fmt_num(v / 2.0))
                    .unwrap_or_else(|| part.value.clone());
                components.push(format!("R{refdes}A {n1} {nw} {half}"));
                components.push(format!("R{refdes}B {nw} {n3} {half}"));
                continue;
            }
        }

        // Otherwise a SPICE primitive, by reference-designator letter.
        match refdes.chars().next().unwrap_or('?').to_ascii_uppercase() {
            'R' | 'C' | 'L' => {
                let (Some(n1), Some(n2)) = (node_at(refdes, "1"), node_at(refdes, "2")) else {
                    return Err(StageError::Other(format!(
                        "{refdes}: expected pins 1 and 2 to be connected to nets"
                    )));
                };
                components.push(format!("{refdes} {n1} {n2} {}", part.value));
            }
            _ => unsupported.push(refdes.clone()),
        }
    }
    if !unsupported.is_empty() {
        return Err(StageError::Other(format!(
            "no SPICE model or primitive mapping for: {}",
            unsupported.join(", ")
        )));
    }
    Ok((includes, components))
}

/// Whether a part is an electrical no-op for simulation — a connector or
/// mechanical part (jack, header, mounting hole, test point) that contributes no
/// SPICE device, only net junctions. Recognised by the `J` reference prefix or a
/// connector/mechanical footprint.
fn is_electrical_noop(part: &crate::model::Part) -> bool {
    if part.refdes.0.starts_with('J') {
        return true;
    }
    part.footprint.as_deref().is_some_and(|f| {
        let f = f.to_ascii_lowercase();
        [
            "jack",
            "connector",
            "mountinghole",
            "testpoint",
            "fiducial",
            "socket",
        ]
        .iter()
        .any(|k| f.contains(k))
    })
}

/// Supply-rail sources for each supply net the circuit actually uses.
fn supply_lines(config: &SimConfig, net_names: &HashSet<&str>) -> Vec<String> {
    config
        .supplies
        .iter()
        .filter(|(net, _)| net_names.contains(net.as_str()))
        .map(|(net, volts)| format!("V{net}_supply {} 0 {}", config.node(net), fmt_num(*volts)))
        .collect()
}

/// Generate an ngspice AC deck for `circuit`, writing results to `data_path`.
///
/// `models` maps a reference designator to the SPICE model resolved for that
/// component (see [`crate::symbols::resolve_models`]). Parts with a model are
/// instantiated as subcircuits; parts without one are emitted as SPICE
/// primitives (R/C/L). The generator contains no per-device knowledge.
pub fn generate_ac_deck(
    circuit: &dyn CircuitSource,
    config: &SimConfig,
    models: &HashMap<String, SpiceModel>,
    data_path: &Path,
) -> Result<String, StageError> {
    let net_names: HashSet<&str> = circuit.nets().iter().map(|n| n.name.as_str()).collect();
    require_net(circuit, &net_names, &config.input_net)?;
    require_net(circuit, &net_names, &config.output_net)?;

    let (includes, components) = netlist_body(circuit, config, models)?;

    let in_node = config.node(&config.input_net);
    if in_node == "0" {
        return Err(StageError::Other(format!(
            "input net '{}' maps to ground",
            config.input_net
        )));
    }

    let mut lines = vec![format!("* legion-of-bom AC deck for {}", circuit.name())];
    // A tiny shunt to ground on every node keeps a floating/high-Z node (an op-amp
    // input, an unpatched jack) from making the operating-point matrix singular.
    lines.push(".options rshunt=1e12 gmin=1e-10 itl1=1000".into());
    for include in &includes {
        lines.push(format!(".include {}", include.display()));
    }
    lines.extend(supply_lines(config, &net_names));
    lines.extend(components);
    // 1 V AC source driving the input against ground.
    lines.push(format!("Vlob_src {in_node} 0 DC 0 AC 1"));

    let out_node = config.node(&config.output_net);
    lines.push(".control".into());
    lines.push(format!(
        "ac dec {} {} {}",
        config.ac.points_per_decade,
        fmt_num(config.ac.start_hz),
        fmt_num(config.ac.stop_hz)
    ));
    lines.push(format!("wrdata {} vdb({})", data_path.display(), out_node));
    lines.push(".endc".into());
    lines.push(".end".into());

    Ok(lines.join("\n") + "\n")
}

/// A transient (time-domain) analysis: a voltage step on the input net, run for
/// `stop_s` at `step_s` resolution, probing the output waveform. The basis for
/// slew-rate / step-response checks on time-domain circuits (DESIGN 5, tus.7) —
/// where an AC sweep says nothing (a slew limiter's behaviour *is* the transient).
#[derive(Debug, Clone)]
pub struct TranAnalysis {
    /// `.tran` time step (s).
    pub step_s: f64,
    /// `.tran` stop time (s).
    pub stop_s: f64,
    /// When the input steps (s).
    pub step_at_s: f64,
    /// Input voltage before / after the step.
    pub from_v: f64,
    pub to_v: f64,
}

impl Default for TranAnalysis {
    fn default() -> Self {
        TranAnalysis {
            step_s: 1e-5,
            stop_s: 1e-2,
            step_at_s: 1e-4,
            from_v: 0.0,
            to_v: 5.0,
        }
    }
}

/// One point of a transient waveform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TranPoint {
    pub t_s: f64,
    pub v: f64,
}

/// A parsed transient waveform of the probed output net.
#[derive(Debug, Clone)]
pub struct TranResult {
    pub points: Vec<TranPoint>,
}

impl TranResult {
    /// Peak slew rate |dV/dt| over the waveform (V/s).
    pub fn max_slew_v_per_s(&self) -> Option<f64> {
        self.points
            .windows(2)
            .filter_map(|w| {
                let dt = w[1].t_s - w[0].t_s;
                (dt > 0.0).then(|| ((w[1].v - w[0].v) / dt).abs())
            })
            .fold(None, |m, s| Some(m.map_or(s, |mx: f64| mx.max(s))))
    }

    /// 10%→90% rise time between the initial and final output levels (s), for a
    /// monotonic step response.
    pub fn rise_time_s(&self) -> Option<f64> {
        let v0 = self.points.first()?.v;
        let v1 = self.points.last()?.v;
        let (lo, hi) = (v0 + 0.1 * (v1 - v0), v0 + 0.9 * (v1 - v0));
        let cross = |thr: f64| {
            self.points.windows(2).find_map(|w| {
                let (a, b) = (w[0], w[1]);
                ((a.v - thr) * (b.v - thr) <= 0.0 && (b.v - a.v).abs() > f64::EPSILON).then(|| {
                    let f = (thr - a.v) / (b.v - a.v);
                    a.t_s + f * (b.t_s - a.t_s)
                })
            })
        };
        Some(cross(hi)? - cross(lo)?)
    }
}

/// Generate an ngspice transient deck: the same circuit body, a voltage **step**
/// on the input net, a `.tran` run probing the output waveform.
pub fn generate_tran_deck(
    circuit: &dyn CircuitSource,
    config: &SimConfig,
    tran: &TranAnalysis,
    models: &HashMap<String, SpiceModel>,
    data_path: &Path,
) -> Result<String, StageError> {
    let net_names: HashSet<&str> = circuit.nets().iter().map(|n| n.name.as_str()).collect();
    require_net(circuit, &net_names, &config.input_net)?;
    require_net(circuit, &net_names, &config.output_net)?;

    let (includes, components) = netlist_body(circuit, config, models)?;

    let in_node = config.node(&config.input_net);
    if in_node == "0" {
        return Err(StageError::Other(format!(
            "input net '{}' maps to ground",
            config.input_net
        )));
    }
    let out_node = config.node(&config.output_net);

    let mut lines = vec![format!(
        "* legion-of-bom transient deck for {}",
        circuit.name()
    )];
    // See the AC deck: keep floating/high-Z nodes from making the matrix singular.
    lines.push(".options rshunt=1e12 gmin=1e-10 itl1=1000".into());
    for include in &includes {
        lines.push(format!(".include {}", include.display()));
    }
    lines.extend(supply_lines(config, &net_names));
    lines.extend(components);
    // Input step as a sharp PWL ramp at step_at_s.
    let edge = tran.step_s.clamp(1e-9, 1e-6);
    lines.push(format!(
        "Vlob_src {in_node} 0 PWL(0 {} {} {} {} {} {} {})",
        fmt_num(tran.from_v),
        fmt_num(tran.step_at_s),
        fmt_num(tran.from_v),
        fmt_num(tran.step_at_s + edge),
        fmt_num(tran.to_v),
        fmt_num(tran.stop_s),
        fmt_num(tran.to_v),
    ));
    lines.push(".control".into());
    lines.push(format!(
        "tran {} {}",
        fmt_num(tran.step_s),
        fmt_num(tran.stop_s)
    ));
    lines.push(format!("wrdata {} v({out_node})", data_path.display()));
    lines.push(".endc".into());
    lines.push(".end".into());
    Ok(lines.join("\n") + "\n")
}

/// Parse `wrdata` transient output: whitespace-separated `time voltage` per line.
fn parse_tran_data(text: &str) -> Vec<TranPoint> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let t = it.next()?.parse::<f64>().ok()?;
            let v = it.next()?.parse::<f64>().ok()?;
            Some(TranPoint { t_s: t, v })
        })
        .collect()
}

/// Run a transient simulation: generate a step-response deck, run `ngspice -b`,
/// parse the output waveform. Artifacts are written under `work_dir`.
pub fn simulate_tran(
    circuit: &dyn CircuitSource,
    config: &SimConfig,
    tran: &TranAnalysis,
    work_dir: &Path,
) -> Result<TranResult, StageError> {
    let ngspice =
        find_on_path("ngspice").ok_or_else(|| StageError::ToolNotFound("ngspice".into()))?;

    std::fs::create_dir_all(work_dir)?;
    let work_dir = work_dir.canonicalize()?;
    let name = sanitize(circuit.name());
    let data_path = work_dir.join(format!("{name}_tran.dat"));
    let deck_path = work_dir.join(format!("{name}_tran.cir"));

    let models = match crate::skidl::kicad_symbol_dir() {
        Some(dir) => crate::symbols::resolve_models(circuit, dir.path())?,
        None => HashMap::new(),
    };

    let deck = generate_tran_deck(circuit, config, tran, &models, &data_path)?;
    std::fs::write(&deck_path, &deck)?;
    crate::symbols::write_builtin_lib(&work_dir)?;

    let output = Command::new(&ngspice)
        .arg("-b")
        .arg(&deck_path)
        .current_dir(&work_dir)
        .output()
        .map_err(|e| StageError::ToolNotFound(format!("ngspice {}: {e}", ngspice.display())))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(StageError::ToolFailed {
            tool: "ngspice".into(),
            code: output.status.code().unwrap_or(-1),
            stderr: tail(&stderr, 20),
        });
    }

    let data = std::fs::read_to_string(&data_path).map_err(|e| {
        StageError::Other(format!(
            "ngspice produced no data at {}: {e}",
            data_path.display()
        ))
    })?;
    let points = parse_tran_data(&data);
    if points.is_empty() {
        return Err(StageError::Other(
            "ngspice produced no transient data points".into(),
        ));
    }
    Ok(TranResult { points })
}

/// Run an AC simulation: generate a deck, run `ngspice -b`, parse the response.
/// Artifacts (`.cir`, `.dat`) are written under `work_dir`.
pub fn simulate_ac(
    circuit: &dyn CircuitSource,
    config: &SimConfig,
    work_dir: &Path,
) -> Result<AcResult, StageError> {
    let ngspice =
        find_on_path("ngspice").ok_or_else(|| StageError::ToolNotFound("ngspice".into()))?;

    std::fs::create_dir_all(work_dir)?;
    let work_dir = work_dir.canonicalize()?;
    let name = sanitize(circuit.name());
    let data_path = work_dir.join(format!("{name}_ac.dat"));
    let deck_path = work_dir.join(format!("{name}.cir"));

    // Resolve each modelled component's SPICE model from its symbol (the parts
    // library will be this source later). Without a symbol dir, only primitives
    // (R/C/L) can be simulated.
    let models = match crate::skidl::kicad_symbol_dir() {
        Some(dir) => crate::symbols::resolve_models(circuit, dir.path())?,
        None => HashMap::new(),
    };

    let deck = generate_ac_deck(circuit, config, &models, &data_path)?;
    std::fs::write(&deck_path, &deck)?;
    crate::symbols::write_builtin_lib(&work_dir)?;

    let output = Command::new(&ngspice)
        .arg("-b")
        .arg(&deck_path)
        .current_dir(&work_dir)
        .output()
        .map_err(|e| StageError::ToolNotFound(format!("ngspice {}: {e}", ngspice.display())))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(StageError::ToolFailed {
            tool: "ngspice".into(),
            code: output.status.code().unwrap_or(-1),
            stderr: tail(&stderr, 20),
        });
    }

    let data = std::fs::read_to_string(&data_path).map_err(|e| {
        StageError::Other(format!(
            "ngspice produced no data at {}: {e}",
            data_path.display()
        ))
    })?;
    let points = parse_ac_data(&data);
    if points.is_empty() {
        return Err(StageError::Other(
            "ngspice produced no AC data points".into(),
        ));
    }
    Ok(AcResult { points })
}

/// Parse `wrdata` output: whitespace-separated `frequency magnitude_db` per line.
fn parse_ac_data(text: &str) -> Vec<AcPoint> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let freq = it.next()?.parse::<f64>().ok()?;
            let mag = it.next()?.parse::<f64>().ok()?;
            Some(AcPoint {
                freq_hz: freq,
                mag_db: mag,
            })
        })
        .collect()
}

/// Replace filesystem-unfriendly characters in a circuit name.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The last `n` lines of `s`, joined — keeps error output bounded.
fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, Part, PinRef};

    fn rc_lowpass() -> Circuit {
        Circuit {
            name: "rc_lowpass".into(),
            parts: vec![Part::new("R1", "1k"), Part::new("C1", "159n")],
            nets: vec![
                Net::new("IN", vec![PinRef::new("R1", "1")]),
                Net::new("OUT", vec![PinRef::new("R1", "2"), PinRef::new("C1", "1")]),
                Net::new("GND", vec![PinRef::new("C1", "2")]),
            ],
        }
    }

    #[test]
    fn rail_voltage_parses_supply_names() {
        assert_eq!(rail_voltage("+12V"), Some(12.0));
        assert_eq!(rail_voltage("-12V"), Some(-12.0));
        assert_eq!(rail_voltage("+5V"), Some(5.0));
        assert_eq!(rail_voltage("+3V3"), Some(3.3));
        assert_eq!(rail_voltage("VCC"), Some(12.0));
        assert_eq!(rail_voltage("VEE"), Some(-12.0));
        // Signals / ground / bias nodes are not rails.
        for n in ["GND", "0", "SIG_IN", "IABC", "+CV", "SLEW_NODE"] {
            assert_eq!(rail_voltage(n), None, "{n} must not read as a rail");
        }
    }

    #[test]
    fn infer_adapts_io_and_supplies_to_the_circuit() {
        // A slew-limiter-shaped circuit: SIG_IN/SIG_OUT + ±12V rails.
        let c = Circuit {
            name: "sl".into(),
            parts: vec![Part::new("U1", "op")],
            nets: vec![
                Net::new("SIG_IN", vec![PinRef::new("U1", "3")]),
                Net::new("SIG_OUT", vec![PinRef::new("U1", "1")]),
                Net::new("+12V", vec![PinRef::new("U1", "8")]),
                Net::new("-12V", vec![PinRef::new("U1", "4")]),
                Net::new("GND", vec![PinRef::new("U1", "5")]),
            ],
        };
        let cfg = SimConfig::infer(&c);
        assert_eq!(cfg.input_net, "SIG_IN");
        assert_eq!(cfg.output_net, "SIG_OUT");
        let mut sup = cfg.supplies.clone();
        sup.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(sup, vec![("+12V".into(), 12.0), ("-12V".into(), -12.0)]);
        // A plain RC (IN/OUT, no rails) falls back cleanly.
        let rc = SimConfig::infer(&rc_lowpass());
        assert_eq!(
            (rc.input_net.as_str(), rc.output_net.as_str()),
            ("IN", "OUT")
        );
        assert_eq!(rc.supplies, SimConfig::default().supplies);
    }

    #[test]
    fn deck_maps_ground_and_harness() {
        let c = rc_lowpass();
        let models = HashMap::new();
        let deck =
            generate_ac_deck(&c, &SimConfig::default(), &models, Path::new("/tmp/x.dat")).unwrap();
        assert!(deck.contains("R1 IN OUT 1k"), "deck:\n{deck}");
        assert!(
            deck.contains("C1 OUT 0 159n"),
            "GND should map to 0:\n{deck}"
        );
        assert!(deck.contains("Vlob_src IN 0 DC 0 AC 1"), "deck:\n{deck}");
        assert!(deck.contains("ac dec 100 1 1000000"), "deck:\n{deck}");
        assert!(deck.contains("vdb(OUT)"), "deck:\n{deck}");
        // No supply rails when the circuit uses no supply nets.
        assert!(!deck.contains("_supply"), "deck:\n{deck}");
    }

    #[test]
    fn deck_rejects_missing_output_net() {
        let mut c = rc_lowpass();
        c.nets.retain(|n| n.name != "OUT");
        let err = generate_ac_deck(
            &c,
            &SimConfig::default(),
            &HashMap::new(),
            Path::new("/tmp/x.dat"),
        )
        .unwrap_err();
        assert!(matches!(err, StageError::Other(_)));
    }

    #[test]
    fn deck_rejects_part_with_no_model_or_primitive() {
        let mut c = rc_lowpass();
        c.parts.push(Part::new("U1", "TL072")); // no model resolved, not a primitive
        let err = generate_ac_deck(
            &c,
            &SimConfig::default(),
            &HashMap::new(),
            Path::new("/tmp/x.dat"),
        )
        .unwrap_err();
        assert!(matches!(err, StageError::Other(msg) if msg.contains("U1")));
    }

    #[test]
    fn deck_instantiates_a_subckt_model_with_rails() {
        // An op-amp part carrying a resolved subckt model → X-line + .include +
        // supply rails, no per-device branch in the generator.
        let c = Circuit {
            name: "amp".into(),
            parts: vec![
                Part {
                    refdes: "U1".into(),
                    value: "OPAMP".into(),
                    footprint: None,
                    library_part: Some("Simulation_SPICE:OPAMP".into()),
                    mpn: None,
                    sim: None,
                    side: None,
                },
                Part::new("R1", "9k"),
                Part::new("R2", "1k"),
            ],
            nets: vec![
                Net::new("IN", vec![PinRef::new("U1", "1")]),
                Net::new(
                    "FB",
                    vec![
                        PinRef::new("U1", "2"),
                        PinRef::new("R1", "2"),
                        PinRef::new("R2", "1"),
                    ],
                ),
                Net::new("OUT", vec![PinRef::new("U1", "5"), PinRef::new("R1", "1")]),
                Net::new("VCC", vec![PinRef::new("U1", "3")]),
                Net::new("VEE", vec![PinRef::new("U1", "4")]),
                Net::new("GND", vec![PinRef::new("R2", "2")]),
            ],
        };
        let mut models = HashMap::new();
        models.insert(
            "U1".to_string(),
            SpiceModel::Subckt {
                subckt: "kicad_builtin_opamp".into(),
                include: PathBuf::from("/k/Simulation_SPICE.sp"),
                pin_order: vec!["1".into(), "2".into(), "3".into(), "4".into(), "5".into()],
                params: None,
            },
        );
        let deck =
            generate_ac_deck(&c, &SimConfig::default(), &models, Path::new("/tmp/x.dat")).unwrap();
        // Nodes in subckt terminal order: pins 1,2,3,4,5 → IN FB VCC VEE OUT.
        assert!(
            deck.contains("XU1 IN FB VCC VEE OUT kicad_builtin_opamp"),
            "deck:\n{deck}"
        );
        assert!(
            deck.contains(".include /k/Simulation_SPICE.sp"),
            "deck:\n{deck}"
        );
        assert!(deck.contains("VVCC_supply VCC 0 15"), "deck:\n{deck}");
        assert!(deck.contains("VVEE_supply VEE 0 -15"), "deck:\n{deck}");
        assert!(
            deck.contains("R1 OUT FB 9k") || deck.contains("R1 FB OUT 9k"),
            "deck:\n{deck}"
        );
    }

    #[test]
    fn cutoff_interpolation_finds_crossing() {
        // Synthetic points bracketing −3.0103 dB near 1 kHz.
        let result = AcResult {
            points: vec![
                AcPoint {
                    freq_hz: 1.0,
                    mag_db: 0.0,
                },
                AcPoint {
                    freq_hz: 1000.0,
                    mag_db: -3.0,
                },
                AcPoint {
                    freq_hz: 1023.0,
                    mag_db: -3.2,
                },
            ],
        };
        let fc = result.cutoff_3db_hz().unwrap();
        assert!((1000.0..=1023.0).contains(&fc), "fc = {fc}");
    }

    #[test]
    fn ac_data_parsing_skips_junk() {
        let data = " 1.00e+00 -4.3e-06 \nheader line\n 1.00e+03 -3.01e+00 \n";
        let points = parse_ac_data(data);
        assert_eq!(points.len(), 2);
        assert_eq!(points[1].freq_hz, 1000.0);
    }

    #[test]
    fn tran_deck_has_step_source_and_tran_analysis() {
        let c = rc_lowpass();
        let tran = TranAnalysis {
            step_s: 1e-5,
            stop_s: 1e-2,
            step_at_s: 1e-4,
            from_v: 0.0,
            to_v: 5.0,
        };
        let deck = generate_tran_deck(
            &c,
            &SimConfig::default(),
            &tran,
            &HashMap::new(),
            Path::new("/tmp/x.dat"),
        )
        .unwrap();
        assert!(deck.contains("R1 IN OUT 1k"), "deck:\n{deck}");
        assert!(deck.contains("Vlob_src IN 0 PWL("), "step source:\n{deck}");
        assert!(deck.contains("\ntran "), "tran analysis:\n{deck}");
        assert!(deck.contains("wrdata /tmp/x.dat v(OUT)"), "probe:\n{deck}");
    }

    #[test]
    fn tran_metrics_slew_and_rise() {
        // A 0→10 V ramp over 1 ms, then flat: slew ≈ 1e4 V/s, 10–90% rise ≈ 0.8 ms.
        let mut points: Vec<TranPoint> = (0..=10)
            .map(|i| TranPoint {
                t_s: i as f64 * 1e-4,
                v: i as f64,
            })
            .collect();
        points.push(TranPoint { t_s: 2e-3, v: 10.0 });
        let r = TranResult { points };
        assert!((r.max_slew_v_per_s().unwrap() - 1e4).abs() < 1.0);
        assert!((r.rise_time_s().unwrap() - 0.8e-3).abs() < 1e-5);
    }

    #[test]
    fn tran_data_parsing_reads_time_voltage() {
        let data = " 0.000e+00 0.0 \n 1.000e-04 1.0 \nhdr\n 2.000e-04 2.0 \n";
        let pts = parse_tran_data(data);
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[1].t_s, 1e-4);
        assert_eq!(pts[2].v, 2.0);
    }

    #[test]
    fn tran_step_response_of_rc_when_ngspice_available() {
        // End-to-end pipeline transient run (needs ngspice). An RC low-pass (R=1k,
        // C=159n, τ≈159µs) fed a 0→1 V step charges toward 1 V, steepest right
        // after the step at ≈ Vstep/τ ≈ 6289 V/s.
        if crate::tools::find_on_path("ngspice").is_none() {
            return;
        }
        let c = rc_lowpass();
        let tran = TranAnalysis {
            step_s: 1e-6,
            stop_s: 2e-3,
            step_at_s: 1e-4,
            from_v: 0.0,
            to_v: 1.0,
        };
        let dir = std::env::temp_dir().join("lob-tran-test");
        let r = simulate_tran(&c, &SimConfig::default(), &tran, &dir).unwrap();
        assert!(r.points.first().unwrap().v.abs() < 0.05, "starts near 0");
        assert!(r.points.last().unwrap().v > 0.9, "charges toward 1V");
        let sr = r.max_slew_v_per_s().unwrap();
        assert!((3000.0..12000.0).contains(&sr), "RC step slew {sr} V/s");
    }
}
