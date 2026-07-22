//! `lob` — the legion-of-bom command-line interface.
//!
//! A thin wrapper over `legion-of-bom-core`. The `run` subcommand is where the
//! Phase 0 pipeline (SKiDL run -> parse -> validate -> simulate -> verify ->
//! BOM) gets wired in; see the beads "Phase 0" epic. Stages 1-2 (SKiDL runner,
//! netlist parse) are live; validate/simulate/verify/bom are still to come.

mod doctor;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use legion_of_bom_core::{
    check_rc_cutoff, parse_netlist_file, simulate_ac, CircuitSource, Finding, PipelineReport,
    Severity, SimConfig, SkidlRunner, StageOutcome,
};

/// legion-of-bom: circuit-as-code in, manufacturing-ready outputs out.
#[derive(Debug, Parser)]
#[command(name = "lob", version, about)]
struct Cli {
    /// Increase log verbosity: -v = debug, -vv = trace.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the pipeline on a circuit definition.
    Run {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
    },
    /// Check that the external toolchain (ngspice, kicad-cli, SKiDL) is available.
    Doctor,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let result = match cli.command {
        Command::Run { circuit } => run(circuit),
        Command::Doctor => doctor::run(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Print the full error chain, then fail with a non-zero code so the
            // pipeline is scriptable / CI-friendly.
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Run the pipeline against a circuit and report per-stage pass/fail.
///
/// Stages: SKiDL run → parse → simulate (ngspice AC) → verify (textbook cutoff).
/// Validate (ERC as structured findings) and BOM are the remaining Phase 0 tasks
/// and slot into the same report. Exits non-zero if any stage fails.
fn run(circuit: PathBuf) -> Result<()> {
    let circuit = circuit
        .canonicalize()
        .with_context(|| format!("circuit not found: {}", circuit.display()))?;

    let stem = circuit
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("circuit");
    let work_dir = PathBuf::from("out").join(stem);

    tracing::info!(circuit = %circuit.display(), "lob run");
    println!("lob run: {}\n", circuit.display());

    let mut report = PipelineReport::new();

    // Stage: SKiDL — run the script, capture the netlist + ERC report.
    let runner = SkidlRunner::discover(&work_dir);
    let skidl_run = runner
        .run(&circuit)
        .with_context(|| "SKiDL stage failed (try `lob doctor`)")?;
    let mut skidl_outcome = StageOutcome::passed("skidl").with(Finding::info(format!(
        "netlist: {}",
        skidl_run.netlist_path.display()
    )));
    if let Some(erc) = &skidl_run.erc_report {
        skidl_outcome = skidl_outcome.with(Finding::info(format!("ERC: {}", erc_summary(erc))));
    }
    report.push(skidl_outcome);

    // Stage: parse the netlist into the internal Circuit model.
    let model =
        parse_netlist_file(&skidl_run.netlist_path).with_context(|| "parse stage failed")?;
    report.push(StageOutcome::passed("parse").with(Finding::info(format!(
        "{} parts, {} nets",
        model.parts().len(),
        model.nets().len()
    ))));

    // Stage: simulate — generate a SPICE deck and run an ngspice AC sweep.
    let ac = simulate_ac(&model, &SimConfig::default(), &work_dir)
        .with_context(|| "simulate stage failed (try `lob doctor`)")?;
    report.push(StageOutcome::passed("simulate").with(Finding::info(format!(
        "AC sweep: {} points, passband {:.2} dB",
        ac.points.len(),
        ac.passband_gain_db().unwrap_or(0.0)
    ))));

    // Stage: verify — assert the simulated cutoff against the textbook value.
    report.push(check_rc_cutoff(&model, &ac, 0.02));

    print_report(&report);
    if report.passed() {
        Ok(())
    } else {
        anyhow::bail!("pipeline reported stage failures")
    }
}

/// Print each stage's pass/fail mark and findings, then an overall summary.
fn print_report(report: &PipelineReport) {
    for outcome in &report.outcomes {
        println!(
            "  {} {}",
            if outcome.passed { "✓" } else { "✗" },
            outcome.stage
        );
        for finding in &outcome.findings {
            let prefix = match finding.severity {
                Severity::Info => "",
                Severity::Warning => "warning: ",
                Severity::Error => "error: ",
            };
            println!("      {prefix}{}", finding.message);
        }
    }
    println!();
    if report.passed() {
        println!("✓ pipeline passed ({} stages)", report.outcomes.len());
    } else {
        let failed = report.outcomes.iter().filter(|o| !o.passed).count();
        println!(
            "✗ pipeline failed ({failed} of {} stages)",
            report.outcomes.len()
        );
    }
}

/// One-line summary from a SKiDL ERC report: the counts line if present, else
/// the first non-empty line.
fn erc_summary(report: &str) -> String {
    report
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .find(|l| l.contains("warnings found") || l.contains("errors found"))
        .or_else(|| report.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("(empty)")
        .to_string()
}

/// Initialize tracing. `RUST_LOG` wins if set; otherwise `-v` picks the level.
fn init_tracing(verbose: u8) {
    use tracing_subscriber::EnvFilter;

    let default_level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "lob={default_level},legion_of_bom_core={default_level}"
        ))
    });

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
