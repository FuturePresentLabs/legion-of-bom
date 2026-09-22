//! Does this circuit actually distort, or does it just pass a sine through
//! unchanged? An AC sweep is a small-signal, linearized answer — it can't see
//! clipping, because clipping is a large-signal, nonlinear effect. This drives
//! a sine burst at the circuit's input net across an amplitude sweep (a quiet
//! pluck up through a hot guitar pickup level) through [`simulate_tran_drive`]
//! and reports gain compression + a flat-topping fraction per amplitude, so
//! "it fuzzes" is a measured claim against the real SPICE waveform — and the
//! onset level is a number, not a guess — rather than a look at the schematic.
//!
//! ```text
//! cargo run -p legion-of-bom-core --example scope_probe -- <circuit.net> [freq_hz] [amp1,amp2,...]
//! ```

use legion_of_bom_core::{
    parse_netlist_file, simulate_tran_drive, Circuit, SimConfig, TranDrive, TranPoint, TranResult,
};

/// Breakpoints for `cycles` sines of `amplitude_v` peak at `freq_hz`, sampled
/// `points_per_cycle` times per cycle — dense enough for ngspice's PWL source
/// to look sinusoidal rather than faceted.
fn sine_pwl(freq_hz: f64, amplitude_v: f64, cycles: u32, points_per_cycle: u32) -> Vec<(f64, f64)> {
    let period = 1.0 / freq_hz;
    let n = cycles * points_per_cycle;
    (0..=n)
        .map(|i| {
            let t = period * f64::from(i) / f64::from(points_per_cycle);
            let v = amplitude_v * (2.0 * std::f64::consts::PI * freq_hz * t).sin();
            (t, v)
        })
        .collect()
}

/// Peak-to-peak of the *last* cycle only — skips the first cycle so a filter's
/// turn-on transient doesn't get counted as part of the steady-state signal.
fn steady_state_pp(points: &[TranPoint], period_s: f64, stop_s: f64) -> f64 {
    let window_start = (stop_s - period_s).max(0.0);
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for p in points.iter().filter(|p| p.t_s >= window_start) {
        lo = lo.min(p.v);
        hi = hi.max(p.v);
    }
    hi - lo
}

/// Fraction of steady-state samples whose slope |dV/dt| is under `flat_frac` of
/// *this waveform's own* peak slope — scale- and sample-density-invariant,
/// unlike comparing raw sample-to-sample deltas against peak-to-peak (which
/// conflates "the timestep is fine" with "the signal is flat"). A clean sine's
/// derivative is a cosine: it's only near-zero for an instant at each peak, so
/// a small fraction of the cycle reads "flat" (~(2/pi)*flat_frac per peak,
/// twice a cycle). A clipped waveform's derivative is pinned near zero for a
/// real chunk of each half-cycle — that's what "sitting at the rail" means.
fn flat_top_fraction(points: &[TranPoint], period_s: f64, stop_s: f64, flat_frac: f64) -> f64 {
    let window_start = (stop_s - period_s).max(0.0);
    let window: Vec<_> = points.iter().filter(|p| p.t_s >= window_start).collect();
    if window.len() < 3 {
        return 0.0;
    }
    let slopes: Vec<f64> = window
        .windows(2)
        .filter_map(|w| {
            let dt = w[1].t_s - w[0].t_s;
            (dt > 0.0).then(|| (w[1].v - w[0].v) / dt)
        })
        .collect();
    let peak_slope = slopes.iter().fold(0.0_f64, |m, s| m.max(s.abs()));
    if peak_slope <= 0.0 {
        return 1.0; // truly dead flat throughout — maximally "flat"
    }
    let threshold = peak_slope * flat_frac;
    let flat = slopes.iter().filter(|s| s.abs() < threshold).count();
    flat as f64 / slopes.len() as f64
}

struct ProbeResult {
    amplitude_v: f64,
    gain: f64,
    flat_top: f64,
}

fn run_probe(
    circuit: &Circuit,
    config: &SimConfig,
    work_dir: &std::path::Path,
    freq_hz: f64,
    amplitude_v: f64,
) -> Result<ProbeResult, Box<dyn std::error::Error>> {
    let cycles = 4;
    let points_per_cycle = 80;
    let period = 1.0 / freq_hz;
    let pwl = sine_pwl(freq_hz, amplitude_v, cycles, points_per_cycle);
    let stop_s = pwl
        .last()
        .map(|(t, _)| *t)
        .unwrap_or(cycles as f64 * period);
    let drive = TranDrive {
        step_s: period / 1000.0,
        stop_s,
        pwl,
        cv: Vec::new(),
        probe_net: None,
    };
    let result: TranResult = simulate_tran_drive(circuit, config, &drive, work_dir)?;

    let in_pp = 2.0 * amplitude_v;
    let out_pp = steady_state_pp(&result.points, period, stop_s);
    let gain = out_pp / in_pp;
    let flat_top = flat_top_fraction(&result.points, period, stop_s, 0.1);
    Ok(ProbeResult {
        amplitude_v,
        gain,
        flat_top,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let net = a
        .next()
        .ok_or("usage: scope_probe <circuit.net> [freq_hz] [amp1,amp2,...]")?;
    let freq_hz: f64 = a.next().and_then(|s| s.parse().ok()).unwrap_or(200.0);
    let amps: Vec<f64> = a
        .next()
        .map(|s| s.split(',').filter_map(|v| v.parse().ok()).collect())
        .filter(|v: &Vec<f64>| !v.is_empty())
        .unwrap_or_else(|| vec![0.005, 0.02, 0.05, 0.1, 0.15, 0.3, 0.6, 1.0, 2.0]);

    let circuit = parse_netlist_file(std::path::Path::new(&net))?;
    let config = SimConfig::infer(&circuit);
    let work_dir = std::path::Path::new(&net)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    println!("scope_probe: {net}  ({freq_hz} Hz sine, sweeping input amplitude)");
    println!("  input net:  {}", config.input_net);
    println!("  output net: {}\n", config.output_net);

    let mut results = Vec::new();
    for amp in amps {
        let r = run_probe(&circuit, &config, &work_dir, freq_hz, amp)?;
        println!(
            "  in_pk={:7.4}V  in_pp={:7.4}V  gain={:7.3} ({:6.2} dB)  flat-top={:5.1}%",
            r.amplitude_v,
            r.amplitude_v * 2.0,
            r.gain,
            20.0 * r.gain.log10(),
            r.flat_top * 100.0
        );
        results.push(r);
    }

    let small_gain = results.first().map(|r| r.gain).unwrap_or(f64::NAN);
    if let Some(largest) = results.last() {
        let compression_db = 20.0 * (largest.gain / small_gain).log10();
        println!(
            "\n  gain compression at largest drive vs smallest: {compression_db:.2} dB, flat-top {:.1}%{}",
            largest.flat_top * 100.0,
            if compression_db < -3.0 || largest.flat_top > 0.15 {
                "  <<< compressing/clipping — this is what makes it a fuzz, not a clean buffer"
            } else {
                "  (still roughly linear even at this drive level — try a larger amplitude)"
            }
        );
    }

    Ok(())
}
