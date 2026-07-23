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
use legion_of_bom_core::skidl::{kicad_footprint_dir, kicad_symbol_dir};
use legion_of_bom_core::{
    analytic_check, build_guide, default_panel_orders_dir, default_parts_dir, export_cpl,
    export_gerbers, fetch_from_jlcpcb, fetch_from_kicad, generate_board_report, generate_bom,
    guide_to_html, guide_to_pdf, jlc_bom_csv, kicad_cli_path, panel_to_dxf, parse_netlist_file,
    png_to_jpeg, render_board_png, run_drc, simulate_ac, validate_erc, zip_dir, BoardOptions,
    BoardPng, CircuitSource, Finding, JlcpcbClient, MouserClient, PanelFile, PanelOrders,
    PartRecord, PartResolution, PartsLibrary, PipelineReport, ResolutionStatus, Severity,
    SimConfig, SkidlRunner, StageOutcome,
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
    /// Generate a BOM for a circuit, optionally priced live from Mouser.
    Bom {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
        /// Look up live unit price + stock from Mouser (needs MOUSER_API_KEY).
        #[arg(long)]
        price: bool,
        /// Also write the BOM CSV to this path.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Generate a .kicad_pcb board file (footprints placed + routed) from a circuit.
    Board {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
        /// Write the board here (default: out/<name>/<name>.kicad_pcb).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Run DRC on a .kicad_pcb and report violations (the layout loop's check step).
    Drc {
        /// Path to the board file to check.
        board: PathBuf,
    },
    /// Build a DRC-gated manufacturing package (Gerbers + drill + JLCPCB CPL + BOM).
    Fab {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
        /// Package directory (default: out/<name>/fab).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Generate a step-by-step visual assembly guide (HTML) from a circuit.
    Guide {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
        /// Write the guide here (default: out/<name>/<name>-guide.html).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Panel design: generate DXF, track orders.
    Panel {
        #[command(subcommand)]
        action: PanelCmd,
    },
}

#[derive(Debug, Subcommand)]
enum PartsCmd {
    List,
    /// Show a part (pins, ratings, verification status) by MPN.
    Show {
        mpn: String,
    },
    /// Add or update a part's metadata (unverified; pins/ratings come from fetch).
    Add {
        mpn: String,
        #[arg(long)]
        manufacturer: Option<String>,
        #[arg(long)]
        datasheet: Option<String>,
    },
    /// Fetch a part into the library (unverified) from a source.
    ///
    /// `--source kicad` (default): pins + datasheet from the installed KiCad
    /// library, keyed by symbol/MPN. `--source jlcpcb`: authoritative datasheet +
    /// parameters (ratings) from JLCPCB, keyed by LCSC code (`C1002`).
    Fetch {
        /// MPN (kicad source) or LCSC component code (jlcpcb source).
        id: String,
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
    Resolve {
        circuit: PathBuf,
    },
    /// Verification gate: fail if any MPN-bearing part isn't human-verified.
    ///
    /// This is the check `layout` / real BOM ordering enforce (okm.4) — the
    /// structural block against unverified part data.
    Gate {
        circuit: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum PanelCmd {
    /// Generate a DXF from a panel spec TOML file.
    Dxf {
        /// Path to the panel spec TOML.
        spec: PathBuf,
        /// Output DXF path (default: same name with .dxf extension).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Show the current order status for a module.
    Status {
        /// Module name (e.g. "crossfader-v1").
        module: String,
    },
    /// Mark a panel as manually ordered.
    MarkOrdered {
        /// Module name.
        module: String,
        /// Vendor (e.g. "sendcutsend", "oshcut").
        #[arg(long)]
        vendor: String,
        /// Vendor tracking / order reference.
        #[arg(long)]
        tracking: Option<String>,
    },
}

fn main() -> ExitCode {
    // Load .env (API keys) before anything reads the environment.
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let result = match cli.command {
        Command::Run { circuit } => run(circuit),
        Command::Doctor => doctor::run(),
        Command::Parts { action } => parts_cmd(action),
        Command::Bom {
            circuit,
            price,
            out,
        } => bom_cmd(circuit, price, out),
        Command::Board { circuit, out } => board_cmd(circuit, out),
        Command::Drc { board } => drc_cmd(board),
        Command::Fab { circuit, out } => fab_cmd(circuit, out),
        Command::Guide { circuit, out } => guide_cmd(circuit, out),
        Command::Panel { action } => panel_cmd(action),
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

/// Handle `lob board <circuit> [--out]` — netlist → .kicad_pcb.
fn board_cmd(circuit: PathBuf, out: Option<PathBuf>) -> Result<()> {
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

    let footprint_dir = kicad_footprint_dir()
        .context("no KiCad footprint library found (set KICAD9_FOOTPRINT_DIR)")?;
    let (board, conflicts) = generate_board_report(&model, &BoardOptions::new(footprint_dir))?;

    let path = out.unwrap_or_else(|| work_dir.join(format!("{stem}.kicad_pcb")));
    std::fs::write(&path, &board).with_context(|| format!("writing {}", path.display()))?;
    let tracks = board.matches("(segment").count();
    let vias = board.matches("(via").count();
    println!("wrote {}", path.display());
    println!("  placed (grid) + routed: {tracks} tracks, {vias} vias, outline + GND pour");
    if !conflicts.is_empty() {
        eprintln!(
            "  ⚠ {} connection(s) left unrouted (for manual/iterative routing):",
            conflicts.len()
        );
        for c in &conflicts {
            eprintln!("      - {c}");
        }
    }
    println!("  validate: lob drc {}", path.display());
    println!("  export:   kicad-cli pcb export gerbers --check-zones (fills the pour) | export pos (CPL)");
    Ok(())
}

/// Handle `lob drc <board>` — run DRC and report violations (the layout loop's
/// check step). Exits non-zero if any error-severity violation remains.
fn drc_cmd(board: PathBuf) -> Result<()> {
    let board = board
        .canonicalize()
        .with_context(|| format!("board not found: {}", board.display()))?;
    let kicad = kicad_cli_path().context("kicad-cli not found (install KiCad or set PATH)")?;

    let report = run_drc(&board, &kicad)?;
    let silk = report.silkscreen_collision_count();
    println!(
        "DRC {}: {} error(s), {} warning(s) ({} silkscreen), {} unconnected",
        board.display(),
        report.error_count(),
        report.warning_count(),
        silk,
        report.unconnected_count()
    );
    for v in report.errors() {
        println!("  ✗ [{}] {}", v.kind, v.description);
        for it in &v.items {
            println!("      - {}", it.description);
        }
    }
    // Non-silk warnings; silkscreen collisions get their own section below.
    for v in report.warnings().filter(|v| !v.is_silkscreen_collision()) {
        println!("  ⚠ [{}] {}", v.kind, v.description);
    }
    // Silkscreen collisions (DESIGN 6.10): not electrical, but they garble the
    // refdes/polarity legend a hand-assembler reads — surface them explicitly.
    if silk > 0 {
        println!("  silkscreen ({silk}): refdes/marks over pads or overlapping —");
        for v in report.silkscreen_collisions() {
            let loc = v
                .items
                .iter()
                .find_map(|it| it.pos)
                .map(|p| format!(" @ ({:.1}, {:.1})", p.x, p.y))
                .unwrap_or_default();
            println!("      ▪ [{}] {}{}", v.kind, v.description, loc);
        }
    }
    if report.is_clean() {
        println!("  ✓ no errors");
        Ok(())
    } else {
        anyhow::bail!("{} DRC error(s)", report.error_count());
    }
}

/// Handle `lob fab <circuit> [--out]` — generate a board, gate it on DRC, and
/// write the JLCPCB-ready manufacturing package (Gerbers + drill + CPL + BOM).
fn fab_cmd(circuit: PathBuf, out: Option<PathBuf>) -> Result<()> {
    let circuit = circuit
        .canonicalize()
        .with_context(|| format!("circuit not found: {}", circuit.display()))?;
    let stem = circuit
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("circuit");
    let work_dir = PathBuf::from("out").join(stem);

    // Generate the board.
    let run = SkidlRunner::discover(&work_dir)
        .run(&circuit)
        .with_context(|| "SKiDL failed (try `lob doctor`)")?;
    let model = parse_netlist_file(&run.netlist_path)?;
    let footprint_dir = kicad_footprint_dir()
        .context("no KiCad footprint library found (set KICAD9_FOOTPRINT_DIR)")?;
    let (board, conflicts) = generate_board_report(&model, &BoardOptions::new(footprint_dir))?;

    let pkg = out.unwrap_or_else(|| work_dir.join("fab"));
    std::fs::create_dir_all(&pkg)?;
    let board_path = pkg.join(format!("{stem}.kicad_pcb"));
    std::fs::write(&board_path, &board)
        .with_context(|| format!("writing {}", board_path.display()))?;
    if !conflicts.is_empty() {
        eprintln!("  ⚠ {} connection(s) left unrouted:", conflicts.len());
        for c in &conflicts {
            eprintln!("      - {c}");
        }
    }

    let kicad = kicad_cli_path().context("kicad-cli not found (install KiCad or set PATH)")?;

    // DRC gate — do not ship a package for a board with errors.
    let report = run_drc(&board_path, &kicad)?;
    println!(
        "DRC: {} error(s), {} warning(s)",
        report.error_count(),
        report.warning_count()
    );
    if !report.is_clean() {
        for v in report.errors() {
            eprintln!("  ✗ [{}] {}", v.kind, v.description);
        }
        anyhow::bail!(
            "board has {} DRC error(s) — refusing to build a fab package",
            report.error_count()
        );
    }

    // Manufacturing outputs.
    let gerber_dir = pkg.join("gerbers");
    export_gerbers(&board_path, &gerber_dir, &kicad)?;
    let zip_path = pkg.join(format!("{stem}-gerbers.zip"));
    let zipped = zip_dir(&gerber_dir, &zip_path)?;
    let cpl_path = pkg.join(format!("{stem}-cpl.csv"));
    let placed = export_cpl(&board_path, &cpl_path, &kicad)?;
    let bom = generate_bom(&model);
    let bom_path = pkg.join(format!("{stem}-bom.csv"));
    std::fs::write(&bom_path, jlc_bom_csv(&bom))
        .with_context(|| format!("writing {}", bom_path.display()))?;

    println!("fab package: {}", pkg.display());
    if zipped {
        println!("  PCB (upload this):   {}", zip_path.display());
    } else {
        println!(
            "  gerbers + drill:     {}/  (zip it — system `zip` unavailable)",
            gerber_dir.display()
        );
    }
    println!("  CPL ({placed} placements): {}", cpl_path.display());
    println!(
        "  BOM ({} line items): {}",
        bom.lines.len(),
        bom_path.display()
    );
    println!("  → JLCPCB: upload the gerber zip for the PCB, then the CPL + BOM for assembly");
    Ok(())
}

/// Handle `lob guide <circuit> [--out]` — generate a board and render a
/// step-by-step visual assembly guide (HTML).
fn guide_cmd(circuit: PathBuf, out: Option<PathBuf>) -> Result<()> {
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
    let footprint_dir = kicad_footprint_dir()
        .context("no KiCad footprint library found (set KICAD9_FOOTPRINT_DIR)")?;
    let (board, _) = generate_board_report(&model, &BoardOptions::new(footprint_dir))?;

    let guide = build_guide(&model, &board).map_err(|e| anyhow::anyhow!(e))?;

    // Diagram: photorealistic UNPOPULATED board renders (bare pads a builder
    // populates) when kicad-cli is available — top always, plus the bottom side
    // when any part mounts on the back; fall back to the schematic top-down.
    std::fs::create_dir_all(&work_dir)?;
    let board_file = work_dir.join(format!("{stem}.kicad_pcb"));
    std::fs::write(&board_file, &board)?;
    let kicad_cli = kicad_cli_path();
    let any_back = guide.steps.iter().any(|s| s.parts.iter().any(|p| p.back));
    let top = kicad_cli
        .as_ref()
        .and_then(|k| render_board_png(&board_file, k, true, false).ok());
    let bottom = if any_back {
        kicad_cli
            .as_ref()
            .and_then(|k| render_board_png(&board_file, k, true, true).ok())
    } else {
        None
    };
    match &top {
        Some((_, w, h)) => println!(
            "  diagram: photoreal bare-board render ({w}×{h}){}",
            if bottom.is_some() {
                " + bottom side"
            } else {
                ""
            }
        ),
        None => println!("  diagram: schematic top-down (kicad-cli not found)"),
    }
    let top_png = top.as_ref().map(|(png, w, h)| BoardPng {
        png,
        width: *w,
        height: *h,
    });
    let bottom_png = bottom.as_ref().map(|(png, w, h)| BoardPng {
        png,
        width: *w,
        height: *h,
    });
    let html = guide_to_html(&guide, top_png, bottom_png);

    let path = out.unwrap_or_else(|| work_dir.join(format!("{stem}-guide.html")));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, html).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    println!("  {} build steps (low-profile first)", guide.steps.len());
    for (i, step) in guide.steps.iter().enumerate() {
        println!("    {}. {} ({} parts)", i + 1, step.title, step.parts.len());
    }

    // Native print-ready PDF, one step per page (self-contained, no browser).
    // Embeds the same photoreal renders (PNG→JPEG for DCTDecode), else schematic.
    let top_jpeg = top.as_ref().and_then(|(png, _, _)| png_to_jpeg(png));
    let bottom_jpeg = bottom.as_ref().and_then(|(png, _, _)| png_to_jpeg(png));
    let pdf_path = path.with_extension("pdf");
    std::fs::write(
        &pdf_path,
        guide_to_pdf(&guide, top_jpeg.as_deref(), bottom_jpeg.as_deref()),
    )
    .with_context(|| format!("writing {}", pdf_path.display()))?;
    println!(
        "wrote {} (print-ready, one step per page)",
        pdf_path.display()
    );
    Ok(())
}

/// Handle `lob bom <circuit> [--price] [--out]`.
fn bom_cmd(circuit: PathBuf, price: bool, out: Option<PathBuf>) -> Result<()> {
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
    let mut bom = generate_bom(&parse_netlist_file(&run.netlist_path)?);

    if price {
        let client = MouserClient::from_env()
            .with_context(|| "live pricing needs MOUSER_API_KEY (put it in .env)")?;
        let mut priced = 0usize;
        for line in &mut bom.lines {
            let Some(mpn) = line.mpn.clone() else {
                continue;
            };
            match client.search_mpn(&mpn) {
                Ok(Some(pp)) => match pp.unit_price_at(line.qty() as u64) {
                    Some(unit) => {
                        line.set_unit_price(unit);
                        priced += 1;
                        let stock = pp
                            .in_stock
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "?".into());
                        eprintln!(
                            "  priced {mpn} → {} @ ${unit:.4} ({stock} in stock)",
                            pp.mpn
                        );
                    }
                    None => eprintln!("  {mpn}: matched {} but no price breaks", pp.mpn),
                },
                Ok(None) => eprintln!("  no Mouser match: {mpn}"),
                Err(e) => eprintln!("  pricing {mpn}: {e}"),
            }
        }
        eprintln!("priced {priced} line(s)\n");
    }

    print!("{}", bom.to_table());
    if let Some(total) = bom.total() {
        println!("\nTotal: ${total:.2}");
    }
    if let Some(out) = out {
        std::fs::write(&out, bom.to_csv()).with_context(|| format!("writing {}", out.display()))?;
        println!("wrote {}", out.display());
    }
    Ok(())
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
            print_resolutions(&resolve_circuit_file(&lib, circuit)?);
        }
        PartsCmd::Gate { circuit } => {
            let resolutions = resolve_circuit_file(&lib, circuit)?;
            let blockers: Vec<_> = resolutions
                .iter()
                .filter(|r| r.blocks_verified_use())
                .collect();
            if blockers.is_empty() {
                let n = resolutions.iter().filter(|r| r.mpn.is_some()).count();
                println!("✓ verification gate passed — {n} MPN-bearing part(s), all verified");
            } else {
                for b in &blockers {
                    let why = match b.status {
                        ResolutionStatus::Unknown => "not in library",
                        ResolutionStatus::Unverified => "in library, unverified",
                        _ => "",
                    };
                    println!(
                        "  ✗ {:<6} {:<18} {why}",
                        b.refdes,
                        b.mpn.as_deref().unwrap_or("-")
                    );
                }
                anyhow::bail!(
                    "verification gate FAILED: {} part(s) not verified — layout / BOM ordering refuse to run",
                    blockers.len()
                );
            }
        }
        PartsCmd::Fetch { id, source } => {
            let fetched = match source.as_str() {
                "kicad" => {
                    let dir = kicad_symbol_dir()
                        .context("no KiCad symbol library found (set KICAD9_SYMBOL_DIR)")?;
                    fetch_from_kicad(&id, dir.path())?
                }
                "jlcpcb" => {
                    let client = JlcpcbClient::from_env().context(
                        "JLCPCB fetch needs JLCPCB_APP_ID/ACCESS_KEY/SECRET_KEY in .env",
                    )?;
                    fetch_from_jlcpcb(&id, &client)?
                }
                other => anyhow::bail!("unknown source '{other}' (use `kicad` or `jlcpcb`)"),
            };
            let part = merge_fetched(lib.get_part(&fetched.mpn)?, fetched);
            let mpn = part.mpn.clone();
            lib.upsert_part(&part)?;
            lib.commit(&format!("parts: fetch {mpn} from {source}"))?;
            println!("fetched {mpn} from {source}:");
            print_part(&part);
            println!("\n(unverified — run `lob parts verify {mpn}` after confirming)");
        }
    }
    Ok(())
}

/// Handle `lob panel ...` commands.
fn panel_cmd(action: PanelCmd) -> Result<()> {
    match action {
        PanelCmd::Dxf { spec, out } => {
            let toml = std::fs::read_to_string(&spec)
                .with_context(|| format!("reading {}", spec.display()))?;
            let file = PanelFile::from_toml(&toml)
                .with_context(|| format!("parsing {}", spec.display()))?;
            let panel = file
                .to_spec()
                .map_err(|e| anyhow::anyhow!("invalid panel spec: {e}"))?;
            let dxf = panel_to_dxf(panel.as_ref());
            let out_path = out.unwrap_or_else(|| spec.with_extension("dxf"));
            std::fs::write(&out_path, dxf)
                .with_context(|| format!("writing {}", out_path.display()))?;
            println!("wrote {}", out_path.display());
            println!(
                "  panel: {:.2} mm × {:.2} mm, {} hole(s), {} cutout(s)",
                panel.width_mm(),
                panel.height_mm(),
                panel.mounting_holes().len(),
                panel.cutouts().len(),
            );
        }
        PanelCmd::Status { module } => {
            let store = PanelOrders::open(default_panel_orders_dir())
                .with_context(|| "opening panel orders (is `dolt` installed?)")?;
            match store.latest(&module)? {
                Some(order) => {
                    println!("{}: {}", order.module, order.status.as_str());
                    println!("  dxf:   {}", order.dxf_path);
                    if let Some(v) = order.vendor {
                        println!("  vendor: {v}");
                    }
                    if let Some(t) = order.tracking_ref {
                        println!("  tracking: {t}");
                    }
                    if let Some(n) = order.notes {
                        println!("  notes: {n}");
                    }
                }
                None => println!("{module}: no orders on record"),
            }
        }
        PanelCmd::MarkOrdered {
            module,
            vendor,
            tracking,
        } => {
            let store = PanelOrders::open(default_panel_orders_dir())
                .with_context(|| "opening panel orders (is `dolt` installed?)")?;
            store.mark_ordered(&module, &vendor, tracking.as_deref())?;
            println!("marked {module} as ordered via {vendor}");
            if let Some(t) = tracking {
                println!("  tracking: {t}");
            }
        }
    }
    Ok(())
}

/// Run SKiDL + parse a circuit, then resolve its parts against the library.
fn resolve_circuit_file(
    lib: &PartsLibrary,
    circuit: PathBuf,
) -> Result<Vec<legion_of_bom_core::PartResolution>> {
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
    Ok(lib.resolve_circuit(&model)?)
}

/// Merge a freshly-fetched part into any existing record: overlay non-empty
/// fields so different sources compose (JLCPCB datasheet/ratings + KiCad pins)
/// rather than overwrite. Verification status is preserved.
fn merge_fetched(existing: Option<PartRecord>, fetched: PartRecord) -> PartRecord {
    let Some(mut merged) = existing else {
        return fetched;
    };
    if fetched.manufacturer.is_some() {
        merged.manufacturer = fetched.manufacturer;
    }
    if fetched.datasheet_url.is_some() {
        merged.datasheet_url = fetched.datasheet_url;
    }
    if !fetched.pins.is_empty() {
        merged.pins = fetched.pins;
    }
    if !fetched.ratings.is_empty() {
        merged.ratings = fetched.ratings;
    }
    merged
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
