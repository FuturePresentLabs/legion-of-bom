//! SPICE deck generation + ngspice AC simulation. DESIGN.md 5.1.
//!
//! Generates a SPICE deck from a [`CircuitSource`], wraps it in a small AC test
//! harness (a 1 V AC source on the input net, an `.ac` sweep), runs `ngspice -b`,
//! and parses the frequency response back out.
//!
//! Phase 0 scope: passive R/C/L parts and the IN/OUT/GND net-name convention.
//! Active devices (op-amps, etc.) and richer harnesses come with later circuits.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use crate::source::CircuitSource;
use crate::stage::StageError;
use crate::tools::find_on_path;

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
/// Phase 0 convention: drive `IN`, probe `OUT`, `GND`/`0` is ground.
#[derive(Debug, Clone)]
pub struct SimConfig {
    pub input_net: String,
    pub output_net: String,
    pub ground_nets: Vec<String>,
    pub ac: AcSweep,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            input_net: "IN".into(),
            output_net: "OUT".into(),
            ground_nets: vec!["GND".into(), "0".into()],
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

/// Format a frequency for a SPICE card without scientific notation.
fn fmt_hz(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// Generate an ngspice AC deck for `circuit`, writing results to `data_path`.
pub fn generate_ac_deck(
    circuit: &dyn CircuitSource,
    config: &SimConfig,
    data_path: &Path,
) -> Result<String, StageError> {
    let net_names: HashSet<&str> = circuit.nets().iter().map(|n| n.name.as_str()).collect();
    for required in [&config.input_net, &config.output_net] {
        if !net_names.contains(required.as_str()) {
            return Err(StageError::Other(format!(
                "simulation needs a net named '{required}' (have: {})",
                circuit
                    .nets()
                    .iter()
                    .map(|n| n.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }

    let pinmap = pin_net_map(circuit);
    let mut lines = vec![format!("* legion-of-bom AC deck for {}", circuit.name())];

    let mut unsupported = Vec::new();
    for part in circuit.parts() {
        let refdes = &part.refdes.0;
        match refdes.chars().next().unwrap_or('?').to_ascii_uppercase() {
            'R' | 'C' | 'L' => {
                let n1 = pinmap.get(&(refdes.clone(), "1".to_string()));
                let n2 = pinmap.get(&(refdes.clone(), "2".to_string()));
                let (Some(n1), Some(n2)) = (n1, n2) else {
                    return Err(StageError::Other(format!(
                        "{refdes}: expected pins 1 and 2 to be connected to nets"
                    )));
                };
                lines.push(format!(
                    "{refdes} {} {} {}",
                    config.node(n1),
                    config.node(n2),
                    part.value
                ));
            }
            _ => unsupported.push(refdes.clone()),
        }
    }
    if !unsupported.is_empty() {
        return Err(StageError::Other(format!(
            "AC deck generation supports only R/C/L at Phase 0; unsupported: {}",
            unsupported.join(", ")
        )));
    }

    let in_node = config.node(&config.input_net);
    if in_node == "0" {
        return Err(StageError::Other(format!(
            "input net '{}' maps to ground",
            config.input_net
        )));
    }
    // 1 V AC source driving the input against ground.
    lines.push(format!("Vlob_src {in_node} 0 DC 0 AC 1"));

    let out_node = config.node(&config.output_net);
    lines.push(".control".into());
    lines.push(format!(
        "ac dec {} {} {}",
        config.ac.points_per_decade,
        fmt_hz(config.ac.start_hz),
        fmt_hz(config.ac.stop_hz)
    ));
    lines.push(format!("wrdata {} vdb({})", data_path.display(), out_node));
    lines.push(".endc".into());
    lines.push(".end".into());

    Ok(lines.join("\n") + "\n")
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

    let deck = generate_ac_deck(circuit, config, &data_path)?;
    std::fs::write(&deck_path, &deck)?;

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
    fn deck_maps_ground_and_harness() {
        let c = rc_lowpass();
        let deck = generate_ac_deck(&c, &SimConfig::default(), Path::new("/tmp/x.dat")).unwrap();
        assert!(deck.contains("R1 IN OUT 1k"), "deck:\n{deck}");
        assert!(
            deck.contains("C1 OUT 0 159n"),
            "GND should map to 0:\n{deck}"
        );
        assert!(deck.contains("Vlob_src IN 0 DC 0 AC 1"), "deck:\n{deck}");
        assert!(deck.contains("ac dec 100 1 1000000"), "deck:\n{deck}");
        assert!(deck.contains("vdb(OUT)"), "deck:\n{deck}");
    }

    #[test]
    fn deck_rejects_missing_output_net() {
        let mut c = rc_lowpass();
        c.nets.retain(|n| n.name != "OUT");
        let err = generate_ac_deck(&c, &SimConfig::default(), Path::new("/tmp/x.dat")).unwrap_err();
        assert!(matches!(err, StageError::Other(_)));
    }

    #[test]
    fn deck_rejects_active_parts_at_phase0() {
        let mut c = rc_lowpass();
        c.parts.push(Part::new("U1", "TL072"));
        let err = generate_ac_deck(&c, &SimConfig::default(), Path::new("/tmp/x.dat")).unwrap_err();
        assert!(matches!(err, StageError::Other(msg) if msg.contains("U1")));
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
}
