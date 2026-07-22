//! `lob` — the legion-of-bom command-line interface.
//!
//! A thin wrapper over `legion-of-bom-core`. The `run` subcommand runs the full
//! Phase 0 pipeline — SKiDL run → parse → validate → simulate → verify → BOM —
//! and reports per-stage pass/fail, exiting non-zero on any failure.

mod doctor;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use legion_of_bom_core::skidl::kicad_symbol_dir;
use legion_of_bom_core::{
    analytic_check, default_parts_dir, fetch_from_kicad, generate_bom, parse_netlist_file,
    simulate_ac, validate_erc, CircuitSource, Finding, PartRecord, PartResolution, PartsLibrary,
    PipelineReport, ResolutionStatus, Severity, SimConfig, SkidlRunner, StageOutcome,
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
    /// Inspect and edit the global, Dolt-backed parts library.
    Parts {
        #[command(subcommand)]
        action: PartsCmd,
    },
}

#[derive(Debug, Subcommand)]
enum PartsCmd {
    /// List all MPNs in the library.
    List,
    /// Show a part (pins, ratings, verification status) by MPN.
    Show { mpn: String },
    /// Add or update a part's metadata (unverified; pins/ratings come from fetch).
    Add {
        mpn: String,
        #[arg(long)]
        manufacturer: Option<String>,
        #[arg(long)]
        datasheet: Option<String>,
    },
    /// Fetch a part's pins + datasheet from a source into the library (unverified).
    Fetch {
        mpn: String,
        /// Source: `kicad` (installed KiCad library). Distributor APIs come next.
        #[arg(long, default_value = "kicad")]
        source: String,
    },
    /// Mark a part human-verified (the gate real ordering/layout checks).
    Verify {
        mpn: String,
        #[arg(long, default_value = "cli-user")]
        by: String,
    },
    /// Resolve a circuit's parts against the library by MPN.
    Resolve { circuit: PathBuf },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let result = match cli.command {
        Command::Run { circuit } => run(circuit),
        Command::Doctor => doctor::run(),
        Command::Parts { action } => parts_cmd(action),
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
    report.push(StageOutcome::passed("skidl").with(Finding::info(format!(
        "netlist: {}",
        skidl_run.netlist_path.display()
    ))));

    // Stage: parse the netlist into the internal Circuit model.
    let model =
        parse_netlist_file(&skidl_run.netlist_path).with_context(|| "parse stage failed")?;
    report.push(StageOutcome::passed("parse").with(Finding::info(format!(
        "{} parts, {} nets",
        model.parts().len(),
        model.nets().len()
    ))));

    // Stage: validate — surface ERC results as structured findings.
    report.push(validate_erc(skidl_run.erc_report.as_deref()));

    // Stage: simulate — generate a SPICE deck and run an ngspice AC sweep.
    let ac = simulate_ac(&model, &SimConfig::default(), &work_dir)
        .with_context(|| "simulate stage failed (try `lob doctor`)")?;
    report.push(StageOutcome::passed("simulate").with(Finding::info(format!(
        "AC sweep: {} points, passband {:.2} dB",
        ac.points.len(),
        ac.passband_gain_db().unwrap_or(0.0)
    ))));

    // Stage: verify — assert the simulated response against the textbook value
    // for this topology (RC cutoff, op-amp gain, …).
    report.push(analytic_check(&model, &ac, 0.02));

    // Stage: bom — group parts into a BOM, write CSV, summarize.
    let bom = generate_bom(&model);
    let csv_path = work_dir.join(format!("{stem}_bom.csv"));
    std::fs::write(&csv_path, bom.to_csv()).with_context(|| "writing BOM CSV")?;
    let mut bom_outcome = StageOutcome::passed("bom").with(Finding::info(format!(
        "{} line(s), {} component(s); wrote {}",
        bom.lines.len(),
        bom.component_count(),
        csv_path.display()
    )));
    let missing = bom.parts_without_footprint();
    if !missing.is_empty() {
        bom_outcome = bom_outcome.with(Finding::warning(format!(
            "no footprint: {}",
            missing.join(", ")
        )));
    }
    report.push(bom_outcome);

    print_report(&report);
    println!("\nBOM\n{}", bom.to_table());

    if report.passed() {
        Ok(())
    } else {
        anyhow::bail!("pipeline reported stage failures")
    }
}

/// Handle `lob parts …` against the global parts library.
fn parts_cmd(action: PartsCmd) -> Result<()> {
    let lib = PartsLibrary::open(default_parts_dir())
        .with_context(|| "opening the parts library (is `dolt` installed?)")?;
    match action {
        PartsCmd::List => {
            let mpns = lib.list_mpns()?;
            if mpns.is_empty() {
                println!("(parts library is empty)");
            }
            for mpn in mpns {
                println!("{mpn}");
            }
        }
        PartsCmd::Show { mpn } => match lib.get_part(&mpn)? {
            None => println!("not found: {mpn}"),
            Some(part) => print_part(&part),
        },
        PartsCmd::Add {
            mpn,
            manufacturer,
            datasheet,
        } => {
            // Preserve existing pins/ratings/verification; update metadata only.
            let mut part = lib.get_part(&mpn)?.unwrap_or_else(|| PartRecord::new(&mpn));
            part.manufacturer = manufacturer.or(part.manufacturer);
            part.datasheet_url = datasheet.or(part.datasheet_url);
            lib.upsert_part(&part)?;
            lib.commit(&format!("parts: add/update {mpn}"))?;
            println!("saved {mpn}");
        }
        PartsCmd::Verify { mpn, by } => {
            if lib.get_part(&mpn)?.is_none() {
                anyhow::bail!("no such part: {mpn}");
            }
            lib.mark_verified(&mpn, &by)?;
            lib.commit(&format!("parts: verify {mpn}"))?;
            println!("verified {mpn} (by {by})");
        }
        PartsCmd::Resolve { circuit } => {
            let circuit = circuit
                .canonicalize()
                .with_context(|| format!("circuit not found: {}", circuit.display()))?;
            let stem = circuit
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("circuit");
            let work_dir = PathBuf::from("out").join(stem);
            let run = SkidlRunner::discover(&work_dir)
                .run(&circuit)
                .with_context(|| "SKiDL failed (try `lob doctor`)")?;
            let model = parse_netlist_file(&run.netlist_path)?;
            print_resolutions(&lib.resolve_circuit(&model)?);
        }
        PartsCmd::Fetch { mpn, source } => {
            let part = match source.as_str() {
                "kicad" => {
                    let dir = kicad_symbol_dir()
                        .context("no KiCad symbol library found (set KICAD9_SYMBOL_DIR)")?;
                    fetch_from_kicad(&mpn, dir.path())?
                }
                other => anyhow::bail!(
                    "unknown source '{other}' (only `kicad` so far; JLCPCB/Mouser next)"
                ),
            };
            lib.upsert_part(&part)?;
            lib.commit(&format!("parts: fetch {mpn} from {source}"))?;
            println!("fetched {mpn} from {source}:");
            print_part(&part);
            println!("\n(unverified — run `lob parts verify {mpn}` after confirming)");
        }
    }
    Ok(())
}

fn print_part(part: &PartRecord) {
    println!("MPN:          {}", part.mpn);
    println!(
        "manufacturer: {}",
        part.manufacturer.as_deref().unwrap_or("-")
    );
    println!(
        "datasheet:    {}",
        part.datasheet_url.as_deref().unwrap_or("-")
    );
    let verified = if part.verified_by_human {
        format!(
            "yes{}",
            part.verified_by
                .as_deref()
                .map(|b| format!(" (by {b})"))
                .unwrap_or_default()
        )
    } else {
        "no".to_string()
    };
    println!("verified:     {verified}");
    if !part.pins.is_empty() {
        println!("pins:");
        for pin in &part.pins {
            let cite = pin
                .cited_page
                .map(|p| format!("  [p.{p}]"))
                .unwrap_or_default();
            println!("  {:>3} = {}{cite}", pin.pin_number, pin.pin_name);
        }
    }
    if !part.ratings.is_empty() {
        println!("ratings:");
        for r in &part.ratings {
            let unit = r
                .unit
                .as_deref()
                .map(|u| format!(" {u}"))
                .unwrap_or_default();
            let cite = r
                .cited_page
                .map(|p| format!("  [p.{p}]"))
                .unwrap_or_default();
            println!("  {} = {}{unit}{cite}", r.name, r.value);
        }
    }
}

fn print_resolutions(resolutions: &[PartResolution]) {
    let with_mpn: Vec<_> = resolutions.iter().filter(|r| r.mpn.is_some()).collect();
    if with_mpn.is_empty() {
        println!("no parts declare an MPN (generic/ideal parts) — nothing to resolve");
        return;
    }
    for r in &with_mpn {
        let (mark, label) = match r.status {
            ResolutionStatus::Verified => ("✓", "verified"),
            ResolutionStatus::Unverified => ("⚠", "in library, unverified"),
            ResolutionStatus::Unknown => ("✗", "not in library"),
            ResolutionStatus::NoMpn => continue,
        };
        println!(
            "  {mark} {:<6} {:<18} {label}",
            r.refdes,
            r.mpn.as_deref().unwrap_or("-")
        );
    }
    let verified = with_mpn
        .iter()
        .filter(|r| r.status == ResolutionStatus::Verified)
        .count();
    println!(
        "\n{verified}/{} MPN-bearing part(s) verified",
        with_mpn.len()
    );
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
