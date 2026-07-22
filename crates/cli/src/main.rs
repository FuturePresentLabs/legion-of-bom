//! `lob` — the legion-of-bom command-line interface.
//!
//! A thin wrapper over `legion-of-bom-core`. The `run` subcommand is where the
//! Phase 0 pipeline (SKiDL run -> parse -> validate -> simulate -> verify ->
//! BOM) gets wired in; see the beads "Phase 0" epic. Today it only resolves the
//! circuit path and prints the planned stages.

mod doctor;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use legion_of_bom_core::SkidlRunner;

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

/// Run the pipeline against a circuit.
///
/// Stage 1 (SKiDL runner) is wired; parse -> validate -> simulate -> verify ->
/// bom are the remaining Phase 0 tasks and plug in below.
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
    println!("lob run: {}", circuit.display());

    // Stage 1: SKiDL — run the script, capture the netlist + ERC report.
    let runner = SkidlRunner::discover(&work_dir);
    let skidl_run = runner
        .run(&circuit)
        .with_context(|| "SKiDL stage failed (try `lob doctor`)")?;

    println!(
        "  ✓ skidl     netlist: {}",
        skidl_run.netlist_path.display()
    );
    match &skidl_run.erc_report {
        Some(report) => {
            let summary = erc_summary(report);
            println!("             ERC: {summary}");
        }
        None => println!("             ERC: (no report emitted)"),
    }

    println!("  … parse -> validate -> simulate -> verify -> bom: not yet implemented");
    println!("    (see the beads 'Phase 0' epic)");
    Ok(())
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
