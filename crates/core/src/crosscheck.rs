//! Cross-engine validation harness: our ngspice transient vs pedalkernel's
//! process (ef4.4). Both engines compile the same circuit — ours via the SPICE
//! deck, pedalkernel via the .pedal emitter (ef4.3) — run the same step
//! stimulus, and the harness measures each engine's step response and
//! passes/fails the agreement.
//!
//! pedalkernel is a *tool* here, not a dependency (ef4.1): the binary is
//! discovered via `PEDALKERNEL_BIN`/`PATH` (`crate::tools::pedalkernel_path`)
//! and invoked as `pedalkernel process <pedal> <in.wav> <out.wav>`.
//! A missing binary is a warning, not a failure — ngspice remains the sim
//! authority; the cross-engine check is a bonus, so it degrades gracefully.

use crate::source::CircuitSource;
use crate::spice::{simulate_tran, SimConfig, TranAnalysis, TranPoint};
use crate::stage::{Finding, Severity, StageError};
use std::io::Write as _;
use std::path::Path;
use std::process::Command;

/// Sample rate for the cross-engine stimulus WAV (pedalkernel's native rate).
pub const PK_SAMPLE_RATE: u32 = 48_000;

/// Env var pointing at the `pedalkernel` binary (`PATH` is checked first).
/// Re-exported here for the CLI's error messages.
pub use crate::tools::PEDALKERNEL_BIN_ENV;

/// One engine's step-response metrics, comparable across engines.
#[derive(Debug, Clone, PartialEq)]
pub struct StepMetrics {
    /// Settled output value (V).
    pub final_v: f64,
    /// Peak slew rate |dV/dt| (V/s).
    pub max_slew_v_per_s: f64,
    /// 10%→90% rise time (s).
    pub rise_time_s: f64,
}

/// The cross-engine comparison result.
#[derive(Debug, Clone, PartialEq)]
pub struct CrosscheckReport {
    /// ngspice's metrics (the sim authority).
    pub ngspice: StepMetrics,
    /// pedalkernel's metrics; `None` when the binary was unavailable.
    pub pedalkernel: Option<StepMetrics>,
    /// Info for each engine's numbers; warnings/errors for disagreements.
    pub findings: Vec<Finding>,
}

impl CrosscheckReport {
    /// Whether both engines ran and agreed within tolerance.
    pub fn passed(&self) -> bool {
        self.pedalkernel.is_some() && !self.findings.iter().any(|f| f.severity == Severity::Error)
    }
}

// ---------------------------------------------------------------------------
// WAV I/O — hand-rolled PCM16 mono RIFF, no new dependencies.
// ---------------------------------------------------------------------------

/// Write a mono 32-bit float WAV holding a voltage step: `from_v` until
/// `step_at_s`, then `to_v`, until `stop_s`. Samples are native volts — the
/// same unit convention ngspice's transient uses — so both engines' outputs
/// compare directly.
pub fn write_step_wav(path: &Path, sample_rate: u32, tran: &TranAnalysis) -> std::io::Result<()> {
    let n = (tran.stop_s * sample_rate as f64).round() as usize;
    let step_sample = (tran.step_at_s * sample_rate as f64).round() as usize;
    let samples: Vec<f32> = (0..n)
        .map(|i| {
            if i < step_sample {
                tran.from_v as f32
            } else {
                tran.to_v as f32
            }
        })
        .collect();
    write_wav(path, sample_rate, &samples)
}

/// Write mono 32-bit float WAV samples to `path` (pedalkernel's WAV format —
/// hound's `SampleFormat::Float`, WAVE_FORMAT_EXTENSIBLE on disk).
pub fn write_wav(path: &Path, sample_rate: u32, samples: &[f32]) -> std::io::Result<()> {
    use std::io::BufWriter;

    let data_len = samples.len() * 4;
    let file = std::fs::File::create(path)?;
    let mut w = BufWriter::new(file);
    // WAVE_FORMAT_EXTENSIBLE (0xFFFE) with an IEEE-float subformat GUID — the
    // container hound writes and pedalkernel reads.
    w.write_all(b"RIFF")?;
    w.write_all(&(60 + data_len as u32).to_le_bytes())?; // 4 + fmt(48) + data hdr
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&40u32.to_le_bytes())?;
    w.write_all(&0xFFFEu16.to_le_bytes())?; // extensible
    w.write_all(&1u16.to_le_bytes())?; // mono
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&(sample_rate * 4).to_le_bytes())?; // byte rate
    w.write_all(&4u16.to_le_bytes())?; // block align
    w.write_all(&32u16.to_le_bytes())?; // bits per sample
    w.write_all(&22u16.to_le_bytes())?; // cbSize
    w.write_all(&32u16.to_le_bytes())?; // valid bits
    w.write_all(&0u32.to_le_bytes())?; // channel mask (mono, unspecified)
                                       // Sub-format GUID: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT (00000003-0000-0010-
                                       // 8000-00aa00389b71).
    w.write_all(&[
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ])?;
    w.write_all(b"data")?;
    w.write_all(&(data_len as u32).to_le_bytes())?;
    for s in samples {
        w.write_all(&s.to_le_bytes())?;
    }
    w.flush()
}

/// Read a mono WAV into (time, volts) points. Accepts 32-bit float (what
/// pedalkernel writes and what [`write_step_wav`] produces) and 16-bit PCM;
/// unsupported formats are loud errors.
pub fn read_wav(path: &Path, sample_rate: u32) -> Result<Vec<TranPoint>, StageError> {
    let bytes = std::fs::read(path)
        .map_err(|e| StageError::Other(format!("reading {}: {e}", path.display())))?;
    if bytes.get(0..4) != Some(&b"RIFF"[..]) {
        return Err(StageError::Other(format!(
            "{}: not a RIFF/WAV file",
            path.display()
        )));
    }
    // Walk chunks after the 12-byte RIFF header; find `fmt ` and `data`.
    let mut i = 12usize;
    let mut fmt: Option<(u16, u16, u16)> = None; // audio format, channels, bits
    let mut data: Option<&[u8]> = None;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes(
            bytes[i + 4..i + 8]
                .try_into()
                .map_err(|_| StageError::Other("truncated wav chunk".into()))?,
        ) as usize;
        let Some(body) = bytes.get(i + 8..i + 8 + len) else {
            return Err(StageError::Other("truncated wav chunk body".into()));
        };
        match id {
            b"fmt " => {
                let audio_format = u16::from_le_bytes(body[0..2].try_into().unwrap());
                let channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
                let bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
                fmt = Some((audio_format, channels, bits));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        i += 8 + len + (len & 1); // chunks are word-aligned
    }
    match fmt {
        Some((1, 1, 16)) => {}
        Some((0xFFFE | 3, 1, 32)) => {}
        _ => {
            return Err(StageError::Other(format!(
                "{}: unsupported WAV format (want mono 16-bit PCM or 32-bit float, got {fmt:?})",
                path.display()
            )));
        }
    }
    let Some(data) = data else {
        return Err(StageError::Other(format!(
            "{}: no data chunk",
            path.display()
        )));
    };
    let dt = 1.0 / sample_rate as f64;
    Ok(match fmt {
        Some((1, 1, 16)) => data
            .chunks_exact(2)
            .enumerate()
            .map(|(n, b)| TranPoint {
                t_s: n as f64 * dt,
                v: i16::from_le_bytes([b[0], b[1]]) as f64 / i16::MAX as f64 * 2.0,
            })
            .collect(),
        _ => data
            .chunks_exact(4)
            .enumerate()
            .map(|(n, b)| TranPoint {
                t_s: n as f64 * dt,
                v: f32::from_le_bytes(b.try_into().unwrap()) as f64,
            })
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Tolerances for the engine agreement check.
pub struct Tolerances {
    /// Max relative difference in peak slew rate (0.30 = 30%).
    pub slew_rel: f64,
    /// Max absolute difference in the settled output value (V).
    pub final_v_abs: f64,
    /// Max relative difference in 10%→90% rise time.
    pub rise_rel: f64,
}

impl Default for Tolerances {
    fn default() -> Self {
        Tolerances {
            slew_rel: 0.30,
            final_v_abs: 0.05,
            rise_rel: 0.30,
        }
    }
}

/// Run the differential validation: ngspice `simulate_tran` vs pedalkernel
/// `process` on the same step stimulus. A missing pedalkernel binary is a
/// warning (cross-engine check skipped), never an error.
pub fn crosscheck_tran(
    circuit: &dyn CircuitSource,
    config: &SimConfig,
    tran: &TranAnalysis,
    work_dir: &Path,
    tol: &Tolerances,
) -> Result<CrosscheckReport, StageError> {
    let mut findings = Vec::new();

    // Engine 1: ngspice (validated in spice.rs).
    let spice = simulate_tran(circuit, config, tran, work_dir)?;
    let spice_metrics = StepMetrics {
        final_v: spice.points.last().map(|p| p.v).unwrap_or(f64::NAN),
        max_slew_v_per_s: spice.max_slew_v_per_s().unwrap_or(f64::NAN),
        rise_time_s: spice.rise_time_s().unwrap_or(f64::NAN),
    };
    findings.push(Finding::info(format!(
        "ngspice: final {:.4} V, peak slew {:.0} V/s, rise {:.2} µs",
        spice_metrics.final_v,
        spice_metrics.max_slew_v_per_s,
        spice_metrics.rise_time_s * 1e6
    )));

    // Engine 2: pedalkernel, if the binary is available.
    let Some(pk) = crate::tools::pedalkernel_path() else {
        findings.push(Finding::warning(format!(
            "pedalkernel binary not found (set {PEDALKERNEL_BIN_ENV} or put it on PATH) — cross-engine check skipped"
        )));
        return Ok(CrosscheckReport {
            ngspice: spice_metrics,
            pedalkernel: None,
            findings,
        });
    };

    // Emit the .pedal + stimulus WAV, run `pedalkernel process`, read back.
    let pedal_path = work_dir.join("crosscheck.pedal");
    let pedal_text = crate::pedal::emit_pedal(circuit, config)?;
    std::fs::write(&pedal_path, &pedal_text)?;
    let in_wav = work_dir.join("crosscheck_in.wav");
    let out_wav = work_dir.join("crosscheck_out.wav");
    write_step_wav(&in_wav, PK_SAMPLE_RATE, tran)?;
    run_pk_process(&pk, &pedal_path, &in_wav, &out_wav)?;
    let points = read_wav(&out_wav, PK_SAMPLE_RATE)?;
    let pk_metrics = measure(&points);
    findings.push(Finding::info(format!(
        "pedalkernel: final {:.4} V, peak slew {:.0} V/s, rise {:.2} µs",
        pk_metrics.final_v,
        pk_metrics.max_slew_v_per_s,
        pk_metrics.rise_time_s * 1e6
    )));

    // Compare, failing loud on any disagreement beyond tolerance.
    let slew_rel = rel_diff(pk_metrics.max_slew_v_per_s, spice_metrics.max_slew_v_per_s);
    if slew_rel > tol.slew_rel {
        findings.push(Finding::error(format!(
            "peak slew disagrees by {:.1}% (tol {:.0}%): ngspice {:.0} vs pedalkernel {:.0} V/s",
            slew_rel * 100.0,
            tol.slew_rel * 100.0,
            spice_metrics.max_slew_v_per_s,
            pk_metrics.max_slew_v_per_s
        )));
    }
    let dv = (pk_metrics.final_v - spice_metrics.final_v).abs();
    if dv > tol.final_v_abs {
        findings.push(Finding::error(format!(
            "final value disagrees by {dv:.4} V (tol {:.2} V): ngspice {:.4} vs pedalkernel {:.4}",
            tol.final_v_abs, spice_metrics.final_v, pk_metrics.final_v
        )));
    }
    if spice_metrics.rise_time_s.is_finite() && pk_metrics.rise_time_s.is_finite() {
        let rise_rel = rel_diff(pk_metrics.rise_time_s, spice_metrics.rise_time_s);
        if rise_rel > tol.rise_rel {
            findings.push(Finding::error(format!(
                "rise time disagrees by {:.1}% (tol {:.0}%): ngspice {:.2} µs vs pedalkernel {:.2} µs",
                rise_rel * 100.0,
                tol.rise_rel * 100.0,
                spice_metrics.rise_time_s * 1e6,
                pk_metrics.rise_time_s * 1e6
            )));
        }
    } else {
        findings.push(Finding::warning(
            "rise time not measurable on one engine — check the step response shape",
        ));
    }

    Ok(CrosscheckReport {
        ngspice: spice_metrics,
        pedalkernel: Some(pk_metrics),
        findings,
    })
}

/// Relative difference |a - b| / |b|, guarded against zero denominators.
fn rel_diff(a: f64, b: f64) -> f64 {
    (a - b).abs() / b.abs().max(f64::EPSILON)
}

/// Measure step metrics off a raw (time, volts) point vector.
fn measure(points: &[TranPoint]) -> StepMetrics {
    let final_v = points.last().map(|p| p.v).unwrap_or(f64::NAN);
    let max_slew = points
        .windows(2)
        .filter_map(|w| {
            let dt = w[1].t_s - w[0].t_s;
            (dt > 0.0).then(|| ((w[1].v - w[0].v) / dt).abs())
        })
        .fold(None, |m, s| Some(m.map_or(s, |mx: f64| mx.max(s))))
        .unwrap_or(f64::NAN);
    // 10%→90% rise time between the initial and final levels.
    let rise_time = (|| -> Option<f64> {
        let v0 = points.first()?.v;
        let v1 = points.last()?.v;
        let (lo, hi) = (v0 + 0.1 * (v1 - v0), v0 + 0.9 * (v1 - v0));
        let cross = |thr: f64| {
            points.windows(2).find_map(|w| {
                let (a, b) = (w[0], w[1]);
                ((a.v - thr) * (b.v - thr) <= 0.0 && (b.v - a.v).abs() > f64::EPSILON).then(|| {
                    let f = (thr - a.v) / (b.v - a.v);
                    a.t_s + f * (b.t_s - a.t_s)
                })
            })
        };
        Some(cross(hi)? - cross(lo)?)
    })()
    .unwrap_or(f64::NAN);
    StepMetrics {
        final_v,
        max_slew_v_per_s: max_slew,
        rise_time_s: rise_time,
    }
}

/// Run `pedalkernel process <pedal> <in.wav> <out.wav>` with output
/// normalization disabled so the engine's raw response is what gets compared.
fn run_pk_process(
    bin: &Path,
    pedal: &Path,
    in_wav: &Path,
    out_wav: &Path,
) -> Result<(), StageError> {
    let _ = std::fs::remove_file(out_wav);
    let output = Command::new(bin)
        .arg("process")
        .arg(pedal)
        .arg(in_wav)
        .arg(out_wav)
        .arg("--no-calibrate")
        .output()
        .map_err(|e| StageError::ToolNotFound(format!("pedalkernel {}: {e}", bin.display())))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(StageError::ToolFailed {
            tool: "pedalkernel".into(),
            code: output.status.code().unwrap_or(-1),
            stderr: tail_lines(&stderr, 20),
        });
    }
    if !out_wav.is_file() {
        return Err(StageError::Other(format!(
            "pedalkernel produced no output at {}",
            out_wav.display()
        )));
    }
    Ok(())
}

/// The last `n` lines of `s`, joined — keeps error output bounded.
fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join("lob-wav-test");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn wav_round_trip_preserves_samples() {
        let path = dir().join("rt.wav");
        let samples: Vec<f32> = (0..1000).map(|i| (i % 100) as f32 / 100.0).collect();
        write_wav(&path, 48_000, &samples).unwrap();
        let pts = read_wav(&path, 48_000).unwrap();
        assert_eq!(pts.len(), samples.len());
        for (p, s) in pts.iter().zip(&samples) {
            assert!((p.v - *s as f64).abs() < 1e-6, "{} vs {s}", p.v);
        }
        assert!((pts[1].t_s - 1.0 / 48_000.0).abs() < 1e-9);
    }

    #[test]
    fn reads_hound_float_wav() {
        // pedalkernel writes 32-bit float WAVs via hound (extensible format) —
        // verify our reader handles a hound-produced file, not just our own.
        use hound::{SampleFormat, WavSpec, WavWriter};
        let path = dir().join("hound.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut w = WavWriter::create(&path, spec).unwrap();
        for v in [0.0f32, 0.25, -0.5, 1.0] {
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        let pts = read_wav(&path, 48_000).unwrap();
        assert_eq!(pts.len(), 4);
        for (p, want) in pts.iter().zip([0.0f64, 0.25, -0.5, 1.0]) {
            assert!((p.v - want).abs() < 1e-6, "{} vs {want}", p.v);
        }
    }

    #[test]
    fn step_wav_holds_levels_and_transitions() {
        let path = dir().join("step.wav");
        let tran = TranAnalysis {
            step_s: 1e-6,
            stop_s: 1e-3,
            step_at_s: 5e-4,
            from_v: 0.0,
            to_v: 1.0,
            sequence: Vec::new(),
            cv: None,
        };
        write_step_wav(&path, 48_000, &tran).unwrap();
        let pts = read_wav(&path, 48_000).unwrap();
        assert_eq!(pts.len(), 48);
        assert!(pts[0].v.abs() < 1e-6, "starts at from_v");
        assert!(pts[23].v.abs() < 1e-6, "still low before the step");
        assert!((pts[25].v - 1.0).abs() < 1e-3, "high after the step");
    }

    #[test]
    fn read_wav_rejects_non_wav() {
        let path = dir().join("not.wav");
        std::fs::write(&path, b"NOPE-not-a-wav-at-all........").unwrap();
        let err = read_wav(&path, 48_000).unwrap_err();
        assert!(matches!(err, StageError::Other(ref m) if m.contains("RIFF")));
    }
}
