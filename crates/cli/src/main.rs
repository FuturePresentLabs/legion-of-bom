//! `lob` — the legion-of-bom command-line interface.
//!
//! A thin wrapper over `legion-of-bom-core`. The `run` subcommand runs the full
//! Phase 0 pipeline — SKiDL run → parse → validate → simulate → verify → BOM —
//! and reports per-stage pass/fail, exiting non-zero on any failure.

mod doctor;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use legion_of_bom_core::skidl::{kicad_footprint_dir, kicad_symbol_dir};
use legion_of_bom_core::{
    analytic_check, build_facts, build_guide, default_image_cache_dir, default_panel_orders_dir,
    default_parts_dir, derive_panel, embed_source, export_cpl, export_gerbers, fetch_data_uri,
    fetch_from_jlcpcb, fetch_from_kicad, generate_board_artifacts, generate_board_report,
    generate_bom, guide_to_html, guide_to_pdf, jlc_bom_csv, kicad_cli_path, minimum_hp,
    package_key, panel_to_dxf, panel_to_kicad_pcb, parse_netlist_file, part_kind_of, plan_repair,
    png_to_jpeg, product_image_url, render_board_png, run_drc, run_layout_loop, simulate_ac,
    simulate_tran, suggest_by_keyword, suggest_mpns, validate_erc, value_key, zip_dir,
    ArtifactKind, ArtifactStatus, BoardOptions, BoardPng, BomLine, BuildCopy, BuiltinCutouts,
    CircuitSource, EurorackPlacer, Finding, JlcpcbClient, KitType, LayoutLoop, LayoutMode, Logo,
    Manifest, MouserClient, PanelFile, PanelOrders, PartRecord, PartResolution, PartsLibrary,
    PipelineReport, ProjectView, Repair, ResolutionStatus, SeededPlacer, Severity, SimConfig,
    SkidlRunner, SourcingClients, StageOutcome, TranAnalysis,
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
    /// List the circuits declared in this repo's lob.toml.
    Circuits,
    /// Build a circuit's full artifact set (guide + Visual BOM + fab package), or
    /// every circuit in the repo when no name is given.
    Build {
        /// Circuit name from lob.toml; omit to build all circuits.
        circuit: Option<String>,
    },
    /// Show each circuit's build state — which artifacts exist and whether they
    /// are stale relative to the source + manifest. No network.
    Status,
    /// Serve the local web dashboard (localhost, no auth) for this repo — the
    /// read-only Forestry-style viewer over the same core.
    Serve {
        /// Address to bind (host:port).
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
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
        /// Also write a Visual BOM (HTML): a part photo per line (EasyEDA/LCSC,
        /// keyless), with through-hole resistors shown as their color code.
        #[arg(long)]
        visual: bool,
    },
    /// Generate a .kicad_pcb board file (footprints placed + routed) from a circuit.
    Board {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
        /// Write the board here (default: out/<name>/<name>.kicad_pcb).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Eurorack panel spec (TOML): anchor jacks/pots to its cutouts and size
        /// the board to the panel (vertical 3U) instead of a bounding-box strip.
        #[arg(long)]
        panel: Option<PathBuf>,
        /// Layout cost-function mode (only affects the panel-anchored loop):
        /// analog | digital | mixed.
        #[arg(long, default_value = "analog")]
        mode: String,
        /// Iterative layout attempts over a panel board (0 = one-shot placement).
        #[arg(long, default_value_t = 6)]
        iterations: usize,
        /// Brand logo SVG to render on the back silk (bottom-centre).
        #[arg(long)]
        logo: Option<PathBuf>,
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
        /// Eurorack panel spec (TOML): build the vertical panel-anchored board.
        #[arg(long)]
        panel: Option<PathBuf>,
        /// Layout cost-function mode: analog | digital | mixed.
        #[arg(long, default_value = "analog")]
        mode: String,
        /// Iterative layout attempts over a panel board (0 = one-shot placement).
        #[arg(long, default_value_t = 6)]
        iterations: usize,
        /// Run full KiCad DRC on every layout attempt (slow; default is a single
        /// final-gate DRC).
        #[arg(long)]
        drc_every_iter: bool,
        /// Brand logo SVG to render on the back silk (bottom-centre).
        #[arg(long)]
        logo: Option<PathBuf>,
    },
    /// Generate a step-by-step visual assembly guide (HTML) from a circuit.
    Guide {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
        /// Write the guide here (default: out/<name>/<name>-guide.html).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Eurorack panel spec (TOML): build the vertical panel-anchored board.
        #[arg(long)]
        panel: Option<PathBuf>,
        /// Assembly kit type: auto (default; detects from pad types) | tht | smd |
        /// mixed. THT-first framing + copy — most DIY kits are through-hole.
        #[arg(long, default_value = "auto")]
        kit: String,
    },
    /// Import a board from another EDA tool.
    Import {
        #[command(subcommand)]
        action: ImportCmd,
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
    /// Learn the parts we build with from boards that were really manufactured.
    ///
    /// Reads each fab package's BOM and records what was actually used for every
    /// kind/value/package, so a later board can resolve "a 10k 0603" or "a jack"
    /// from what shipped rather than from a distributor search.
    Learn {
        /// Package directories to learn from.
        packages: Vec<PathBuf>,
    },
    /// Show the parts we build with, most-used first.
    House {
        /// Only this kind (resistor, capacitor, jack, pot, ic, ...).
        #[arg(long)]
        kind: Option<String>,
    },
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
    /// Set a part's Visual-BOM photo by MPN — a URL or a local image file. For
    /// boutique parts a distributor lookup can't cover (Thonkiconn jacks, pots);
    /// `scripts/pull_part_image.py` populates these from Thonk/Tayda/EasyEDA.
    SetImage {
        /// Manufacturer part number to attach the photo to.
        mpn: String,
        /// Image URL (http/https) or a path to a local image file.
        source: String,
    },
    /// Set a part's build-guide assembly notes by MPN — ordered part-specific tips
    /// (e.g. "snap off the locating tab if unused") that augment the generic
    /// per-kind copy. Pass no notes to clear them.
    SetAssembly {
        /// Manufacturer part number to attach the notes to.
        mpn: String,
        /// Ordered note lines; each argument is one step.
        notes: Vec<String>,
    },
    /// Resolve a circuit's parts against the library by MPN.
    Resolve {
        circuit: PathBuf,
    },
    /// Suggest real MPNs for a circuit's *generic* parts (no MPN yet), using
    /// Mouser keyword search + the LCSC/EasyEDA catalog. SUGGEST-ONLY — it prints
    /// ranked candidates for a human to confirm (`lob parts fetch` + `verify`);
    /// it never assigns or orders. Degrades gracefully when a distributor key is
    /// absent (LCSC is keyless; Mouser needs MOUSER_API_KEY).
    Suggest {
        /// Path to the circuit definition (e.g. a SKiDL script), or a circuit name
        /// from lob.toml.
        circuit: PathBuf,
        /// Max candidates to show per part.
        #[arg(long, default_value_t = 3)]
        limit: usize,
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
enum ImportCmd {
    /// Read an Eagle schematic (and optionally its board) into a circuit.
    ///
    /// Unlike a fab package, an Eagle schematic carries the netlist, so the
    /// result is a circuit the pipeline can work on — and can be re-emitted as
    /// SKiDL to become a definition you edit and re-run.
    Eagle {
        /// The `.sch` file.
        schematic: PathBuf,
        /// The matching `.brd`, to report the existing placement.
        #[arg(long)]
        board: Option<PathBuf>,
        /// Write a SKiDL script here (default: <schematic>.py next to it).
        #[arg(long)]
        skidl: Option<PathBuf>,
    },
    /// Recover part numbers for an imported BOM that has none.
    Repair {
        /// The package directory.
        package: PathBuf,
        /// Also search a distributor for the lines that need one (needs
        /// MOUSER_API_KEY); otherwise just report the plan.
        #[arg(long)]
        search: bool,
        /// Candidates to show per searched line.
        #[arg(long, default_value_t = 3)]
        limit: usize,
    },
    /// Build a DIY assembly guide and Visual BOM for an imported fab package.
    Guide {
        /// The package directory (BOM + pick-and-place + gerbers).
        package: PathBuf,
        /// Name for the guide (default: the directory's name).
        #[arg(long)]
        name: Option<String>,
        /// Where to write; defaults to the package directory.
        #[arg(long)]
        out: Option<PathBuf>,
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
    /// Generate a panel PCB (.kicad_pcb) + gerbers from a panel spec TOML — the
    /// "PCB panel" many Eurorack builders order instead of milled aluminium.
    Pcb {
        /// Path to the panel spec TOML.
        spec: PathBuf,
        /// Output .kicad_pcb path (default: same name with .kicad_pcb extension).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Brand logo SVG to render on the front silk (bottom-centre).
        #[arg(long)]
        logo: Option<PathBuf>,
    },
    /// Derive an editable panel spec (TOML) from a circuit's panel-facing parts
    /// (jacks/pots/switches) — instead of hand-writing cutout coordinates.
    Derive {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
        /// Panel width in HP. Omit to size to the minimum HP the PCB fits in —
        /// the PCB drives the panel (DESIGN 6.1), which is the default.
        #[arg(long)]
        hp: Option<u16>,
        /// Output TOML path (default: <circuit>_panel.toml next to the circuit).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Compute the minimum Eurorack HP that fits a circuit — the PCB drives the
    /// panel width (DESIGN 6.1).
    Fit {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
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
    // Load API keys before anything reads the environment.
    load_credentials();
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let result = match cli.command {
        Command::Run { circuit } => run(circuit),
        Command::Circuits => circuits_cmd(),
        Command::Build { circuit } => build_cmd(circuit),
        Command::Status => status_cmd(),
        Command::Serve { bind } => serve_cmd(bind),
        Command::Doctor => doctor::run(),
        Command::Parts { action } => parts_cmd(action),
        Command::Bom {
            circuit,
            price,
            out,
            visual,
        } => bom_cmd(circuit, price, out, visual),
        Command::Board {
            circuit,
            out,
            panel,
            mode,
            iterations,
            logo,
        } => board_cmd(circuit, out, panel, mode, iterations, logo),
        Command::Drc { board } => drc_cmd(board),
        Command::Fab {
            circuit,
            out,
            panel,
            mode,
            iterations,
            drc_every_iter,
            logo,
        } => fab_cmd(circuit, out, panel, mode, iterations, drc_every_iter, logo),
        Command::Guide {
            circuit,
            out,
            panel,
            kit,
        } => guide_cmd(circuit, out, panel, kit),
        Command::Import { action } => import_cmd(action),
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

    // Stage: simulate — generate a SPICE deck and run an ngspice AC sweep. Infer
    // the I/O and supply nets from the circuit (SIG_IN/SIG_OUT, +12V/-12V) rather
    // than assuming IN/OUT/±15 V (tus.9).
    let sim_config = SimConfig::infer(&model);
    let ac = simulate_ac(&model, &sim_config, &work_dir)
        .with_context(|| "simulate stage failed (try `lob doctor`)")?;
    report.push(StageOutcome::passed("simulate").with(Finding::info(format!(
        "AC sweep: {} points, passband {:.2} dB",
        ac.points.len(),
        ac.passband_gain_db().unwrap_or(0.0)
    ))));

    // Stage: transient — a step response, which shows time-domain behaviour (a
    // slew limiter's peak slew rate) that an AC sweep can't (tus.10). Soft: a
    // circuit whose step response won't converge is surfaced, not fatal.
    match simulate_tran(&model, &sim_config, &TranAnalysis::default(), &work_dir) {
        Ok(t) => {
            let slew = t.max_slew_v_per_s().map_or_else(
                || "n/a".to_string(),
                |s| format!("{:.0} V/s ({:.2} V/ms)", s, s / 1e3),
            );
            report.push(
                StageOutcome::passed("transient")
                    .with(Finding::info(format!("step response: peak slew {slew}"))),
            );
        }
        Err(e) => report.push(
            StageOutcome::passed("transient")
                .with(Finding::warning(format!("step response did not run: {e}"))),
        ),
    }

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
/// Build board options, using panel-anchored Eurorack placement when a panel
/// spec is given: jacks/pots are anchored to the panel's cutouts (Y flipped from
/// the panel's bottom-up frame to KiCad top-down) and the board outline becomes
/// the panel size (vertical 3U). Otherwise the default grid placement.
/// Shared panel geometry both the board outline and the placer need: panel size
/// (mm), the sheet origin that centres it on A4, and refdes→(x,y) anchors (Y
/// flipped from the panel's bottom-up cutouts to KiCad top-down).
type PanelGeometry = (
    f64,
    f64,
    (f64, f64),
    std::collections::HashMap<String, (f64, f64)>,
);

fn panel_geometry(spec_path: &std::path::Path) -> Result<PanelGeometry> {
    let toml = std::fs::read_to_string(spec_path)
        .with_context(|| format!("reading {}", spec_path.display()))?;
    let file =
        PanelFile::from_toml(&toml).with_context(|| format!("parsing {}", spec_path.display()))?;
    let spec = file
        .to_spec()
        .map_err(|e| anyhow::anyhow!("invalid panel spec: {e}"))?;
    let (w, h) = (spec.width_mm(), spec.height_mm());
    let mut anchors = std::collections::HashMap::new();
    for c in spec.cutouts() {
        if let Some(refdes) = &c.refdes {
            anchors.insert(refdes.clone(), (c.x_mm, h - c.y_mm));
        }
    }
    // Centre the board on KiCad's A4 sheet (297×210 landscape) rather than jamming
    // it in the (0,0) corner.
    let ox = ((297.0 - w) / 2.0).max(10.0);
    let oy = ((210.0 - h) / 2.0).max(10.0);
    Ok((w, h, (ox, oy), anchors))
}

/// Handle `lob board <circuit> [--out]` — netlist → .kicad_pcb.
/// Build board options, using panel-anchored Eurorack placement when a panel
/// The panel spec to build against: the one explicitly given (a `--panel` flag or
/// the manifest's `panel` field), otherwise a panel **auto-derived from the
/// circuit at the minimum HP the PCB fits in** (DESIGN 6.1 — the PCB drives the
/// panel). The derived spec is written into the work dir as `<stem>_auto_panel.toml`
/// so downstream reads it like any other. `None` only when the circuit has no
/// panel-facing controls (a plain board).
fn effective_panel(
    explicit: Option<PathBuf>,
    model: &legion_of_bom_core::Circuit,
    footprint_dir: &Path,
    work_dir: &Path,
    stem: &str,
) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        // A declared panel is authoritative — EXCEPT when it's stale (missing
        // controls the circuit now has). The board can't mate a jack/pot with no
        // cutout, so a stale spec silently produces the wrong board ("panel doesn't
        // match the board / no holes for the pots / 8 HP keeps coming back"). Rather
        // than faithfully rebuild a wrong panel, self-heal it: regenerate in place
        // from the circuit at minimum HP, keeping the builder's finish/thickness.
        refresh_declared_panel_if_stale(&path, model, footprint_dir)?;
        return Ok(Some(path));
    }
    let facts = build_facts(model, footprint_dir)?;
    let hp = minimum_hp(model, &facts);
    let panel = derive_panel(model, hp, &BuiltinCutouts);
    if panel.cutouts.is_empty() {
        return Ok(None); // no controls — build a plain board, no panel
    }
    std::fs::create_dir_all(work_dir)?;
    let path = work_dir.join(format!("{stem}_auto_panel.toml"));
    let toml = panel
        .to_toml()
        .map_err(|e| anyhow::anyhow!("serialising panel: {e}"))?;
    std::fs::write(&path, toml).with_context(|| format!("writing {}", path.display()))?;
    println!(
        "  auto panel: {hp} HP, {} control(s) → {}",
        panel.cutouts.len(),
        path.display()
    );
    Ok(Some(path))
}

/// Self-heal a declared panel that has gone stale — missing controls the circuit
/// now has (compared by refdes: what `derive_panel` would place vs what the spec
/// declares). When stale, regenerate the spec in place from the circuit at minimum
/// HP, preserving the builder-owned finish/thickness. A fresh panel is left
/// untouched. Best-effort: an unreadable/unparseable spec is left alone.
fn refresh_declared_panel_if_stale(
    path: &Path,
    model: &legion_of_bom_core::Circuit,
    footprint_dir: &Path,
) -> Result<()> {
    let Some(declared) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| PanelFile::from_toml(&t).ok())
    else {
        return Ok(());
    };
    let facts = build_facts(model, footprint_dir)?;
    let min_hp = minimum_hp(model, &facts);
    let expected = derive_panel(model, declared.hp.unwrap_or(min_hp), &BuiltinCutouts);
    let have: std::collections::HashSet<&str> = declared
        .cutouts
        .iter()
        .filter_map(|c| c.refdes.as_deref())
        .collect();
    let missing: Vec<&str> = expected
        .cutouts
        .iter()
        .filter_map(|c| c.refdes.as_deref())
        .filter(|r| !have.contains(r))
        .collect();
    // Stale two ways: missing a control the circuit now has (no cutout to mate), or
    // declared narrower than the PCB actually fits in (parts won't lay out → DRC).
    let too_small = declared.hp.is_some_and(|h| h < min_hp);
    if missing.is_empty() && !too_small {
        return Ok(()); // fresh — the declared panel is authoritative, use as-is
    }
    // Rebuild it from the circuit at the minimum HP the PCB fits in.
    let mut fresh = derive_panel(model, min_hp, &BuiltinCutouts);
    fresh.finish = declared.finish;
    fresh.thickness_mm = declared.thickness_mm;
    let toml = fresh
        .to_toml()
        .map_err(|e| anyhow::anyhow!("serialising regenerated panel: {e}"))?;
    std::fs::write(path, &toml).with_context(|| format!("writing {}", path.display()))?;
    let reason = if !missing.is_empty() {
        format!("missing {}", missing.join(", "))
    } else {
        format!("{} HP too small for the PCB", declared.hp.unwrap_or(0))
    };
    println!(
        "  ⚠ declared panel {} was stale ({reason}) → regenerated: {min_hp} HP, {} control(s)",
        path.display(),
        fresh.cutouts.len()
    );
    Ok(())
}

/// spec is given: jacks/pots are anchored to the panel's cutouts and the board
/// outline becomes the panel size (vertical 3U). Otherwise the default grid
/// placement. The placer here is the one-shot [`EurorackPlacer`]; the iterative
/// loop swaps in a [`SeededPlacer`] per attempt.
fn board_options_with_panel(
    footprint_dir: PathBuf,
    panel: &Option<PathBuf>,
) -> Result<BoardOptions> {
    let mut opts = BoardOptions::new(footprint_dir);
    if let Some(spec_path) = panel {
        let (w, h, origin, anchors) = panel_geometry(spec_path)?;
        opts.placer = Box::new(EurorackPlacer {
            width_mm: w,
            height_mm: h,
            origin_mm: origin,
            anchors,
        });
        opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));
    }
    Ok(opts)
}

/// The seeded-placer template for the iterative layout loop, when a panel is
/// given. `None` (no panel) means there's nothing to anchor to, so the loop is
/// skipped and one-shot placement stands.
fn seeded_template(panel: &Option<PathBuf>) -> Result<Option<SeededPlacer>> {
    match panel {
        Some(spec_path) => {
            let (w, h, origin, anchors) = panel_geometry(spec_path)?;
            Ok(Some(SeededPlacer::new(w, h, origin, anchors)))
        }
        None => Ok(None),
    }
}

/// A human-readable board title from a file stem: `slew_limiter` → `Slew Limiter`.
fn pretty_title(stem: &str) -> String {
    stem.split(['_', '-'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Load a brand logo SVG, if a path is given.
fn load_logo(path: &Option<PathBuf>) -> Result<Option<Logo>> {
    match path {
        Some(p) => {
            let svg = std::fs::read_to_string(p)
                .with_context(|| format!("reading logo {}", p.display()))?;
            let logo = Logo::from_svg(&svg).map_err(|e| anyhow::anyhow!("parsing logo: {e}"))?;
            Ok(Some(logo))
        }
        None => Ok(None),
    }
}

/// Parse `--mode`, erroring clearly on an unknown value.
fn parse_mode(mode: &str) -> Result<LayoutMode> {
    LayoutMode::parse(mode)
        .ok_or_else(|| anyhow::anyhow!("unknown --mode '{mode}' (analog | digital | mixed)"))
}

fn board_cmd(
    circuit: PathBuf,
    out: Option<PathBuf>,
    panel: Option<PathBuf>,
    mode: String,
    iterations: usize,
    logo: Option<PathBuf>,
) -> Result<()> {
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
    // Default: the PCB drives the panel — auto-derive one at minimum HP.
    let panel = effective_panel(panel, &model, &footprint_dir, &work_dir, stem)?;
    let mut options = board_options_with_panel(footprint_dir, &panel)?;
    options.title = Some(pretty_title(stem));
    options.logo = load_logo(&logo)?;
    let path = out.unwrap_or_else(|| work_dir.join(format!("{stem}.kicad_pcb")));

    // Iterative, connectivity-aware layout when a panel is given (needs anchors);
    // otherwise the one-shot placement path.
    let (board, conflicts, collisions) = match (seeded_template(&panel)?, iterations) {
        (Some(template), iters) if iters > 0 => {
            let cfg = LayoutLoop {
                mode: parse_mode(&mode)?,
                max_iters: iters,
                kicad_cli: None,
                drc_every_iter: false,
            };
            let report = run_layout_loop(&model, options, template, &cfg)?;
            println!(
                "  seeded layout ({mode}): {} attempt(s), signal HPWL {:.0}mm, critical {:.0}mm, {} via(s)",
                report.iterations,
                report.metrics.signal_hpwl_mm,
                report.metrics.critical_hpwl_mm,
                report.metrics.via_count,
            );
            (report.board, report.unresolved, report.collisions)
        }
        _ => {
            let art = generate_board_artifacts(&model, &options)?;
            (art.pcb, art.route.conflicts, art.collisions)
        }
    };

    std::fs::write(&path, &board).with_context(|| format!("writing {}", path.display()))?;
    let tracks = board.matches("(segment").count();
    let vias = board.matches("(via").count();
    println!("wrote {}", path.display());
    println!("  placed + routed: {tracks} tracks, {vias} vias, outline + GND pour");
    if !conflicts.is_empty() {
        eprintln!(
            "  ⚠ {} connection(s) left unrouted (for manual/iterative routing):",
            conflicts.len()
        );
        for c in &conflicts {
            eprintln!("      - {c}");
        }
    }
    if !collisions.is_empty() {
        eprintln!(
            "  ⚠ {} mechanical clearance issue(s) under a stacked sub-board:",
            collisions.len()
        );
        for c in &collisions {
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
fn fab_cmd(
    circuit: PathBuf,
    out: Option<PathBuf>,
    panel: Option<PathBuf>,
    mode: String,
    iterations: usize,
    drc_every_iter: bool,
    logo: Option<PathBuf>,
) -> Result<()> {
    let resolved = resolve_circuit(&circuit)?;
    let panel = panel.or_else(|| resolved.panel.clone());
    let circuit = resolved
        .source
        .canonicalize()
        .with_context(|| format!("circuit not found: {}", resolved.source.display()))?;
    let stem = resolved.name.as_str();
    let work_dir = PathBuf::from("out").join(stem);

    // Generate the board.
    let run = SkidlRunner::discover(&work_dir)
        .run(&circuit)
        .with_context(|| "SKiDL failed (try `lob doctor`)")?;
    let model = parse_netlist_file(&run.netlist_path)?;
    let footprint_dir = kicad_footprint_dir()
        .context("no KiCad footprint library found (set KICAD9_FOOTPRINT_DIR)")?;
    // Default: the PCB drives the panel — auto-derive one at minimum HP.
    let panel = effective_panel(panel, &model, &footprint_dir, &work_dir, stem)?;
    let mut options = board_options_with_panel(footprint_dir, &panel)?;
    options.title = Some(pretty_title(stem));
    options.logo = load_logo(&logo)?;
    let kicad = kicad_cli_path().context("kicad-cli not found (install KiCad or set PATH)")?;

    // Iterative, connectivity-aware layout when a panel is given; else one-shot.
    // The DRC gate below is the loop's final verification (§6.5), so the loop
    // scores in-process unless `--drc-every-iter` is set.
    let (board, conflicts) = match (seeded_template(&panel)?, iterations) {
        (Some(template), iters) if iters > 0 => {
            let cfg = LayoutLoop {
                mode: parse_mode(&mode)?,
                max_iters: iters,
                kicad_cli: drc_every_iter.then(|| kicad.clone()),
                drc_every_iter,
            };
            let report = run_layout_loop(&model, options, template, &cfg)?;
            println!(
                "  seeded layout ({mode}): {} attempt(s), signal HPWL {:.0}mm, critical {:.0}mm, {} via(s)",
                report.iterations,
                report.metrics.signal_hpwl_mm,
                report.metrics.critical_hpwl_mm,
                report.metrics.via_count,
            );
            (report.board, report.unresolved)
        }
        _ => generate_board_report(&model, &options)?,
    };

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

/// A circuit input resolved from either a direct file path or a manifest circuit.
struct ResolvedCircuit {
    /// Circuit id — the manifest name, or the source file stem for a path arg.
    /// Drives the `out/<name>/` output tree.
    name: String,
    source: PathBuf,
    panel: Option<PathBuf>,
    kit: Option<String>,
    build: Option<BuildCopy>,
    brand: Option<String>,
}

/// Resolve a `lob <cmd> <arg>` circuit argument. An existing file is used
/// directly (flags supply panel/kit as before). Otherwise `arg` is treated as a
/// circuit **name** in the nearest `lob.toml` (walking up from the working dir),
/// pulling its source/panel/kit/build + the repo brand — so `lob guide
/// slew_limiter` works by name from inside a circuits repo.
fn resolve_circuit(arg: &Path) -> Result<ResolvedCircuit> {
    if arg.is_file() {
        return Ok(ResolvedCircuit {
            name: arg
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("circuit")
                .to_string(),
            source: arg.to_path_buf(),
            panel: None,
            kit: None,
            build: None,
            brand: None,
        });
    }
    let cwd = std::env::current_dir()?;
    let (root, manifest) = Manifest::discover(&cwd).map_err(|e| {
        anyhow::anyhow!(
            "'{}' is not a file, and no circuits repo was found: {e}",
            arg.display()
        )
    })?;
    let name = arg.to_str().unwrap_or_default();
    let entry = manifest.circuit(name).ok_or_else(|| {
        let have: Vec<&str> = manifest.circuits.iter().map(|c| c.name.as_str()).collect();
        anyhow::anyhow!(
            "no circuit '{name}' in {}/lob.toml (have: {})",
            root.display(),
            if have.is_empty() {
                "none".into()
            } else {
                have.join(", ")
            }
        )
    })?;
    // Commands that build need a definition; an imported circuit has none.
    let source = entry.source_path(&root).ok_or_else(|| {
        anyhow::anyhow!(
            "'{}' is an imported circuit (fab package only) — it has no source to build from",
            entry.name
        )
    })?;
    Ok(ResolvedCircuit {
        name: entry.name.clone(),
        source,
        panel: entry.panel_path(&root),
        kit: entry.effective_kit(&manifest.defaults).map(str::to_string),
        build: entry.build.clone(),
        brand: manifest.repo.brand.clone(),
    })
}

/// Handle `lob circuits` — list the circuits declared in the nearest `lob.toml`.
fn circuits_cmd() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let view = ProjectView::discover(&cwd)
        .with_context(|| "no lob.toml found (run inside a circuits repo)")?;
    let repo = view.repo.name.as_deref().unwrap_or("(unnamed)");
    let brand = view
        .repo
        .brand
        .as_deref()
        .map(|b| format!(" · {b}"))
        .unwrap_or_default();
    println!("{repo}{brand}  [{}]", view.root.display());
    if view.circuits.is_empty() {
        println!("  (no circuits declared)");
    }
    for c in &view.circuits {
        let kit = c.kit.as_deref().unwrap_or("auto");
        let panel = c
            .panel
            .as_deref()
            .map(|p| format!(" · panel {p}"))
            .unwrap_or_default();
        let copy = if c.has_build_copy {
            " · build-copy"
        } else {
            ""
        };
        // An imported circuit shows where its package came from instead.
        let origin = match (&c.source, &c.import) {
            (Some(src), _) => src.clone(),
            (None, Some(pkg)) => format!("{pkg}  [imported]"),
            _ => "—".to_string(),
        };
        println!("  {:<20} {origin}  ({kit}{panel}{copy})", c.name);
    }
    Ok(())
}

/// Handle `lob build [circuit]` — produce a circuit's full artifact set (guide +
/// Visual BOM + fab package), or every circuit in the repo. Each artifact is
/// independent, so one failing (e.g. a DRC-blocked fab) still leaves the others.
fn build_cmd(name: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (_root, manifest) = Manifest::discover(&cwd)
        .with_context(|| "no lob.toml found (run inside a circuits repo)")?;
    let targets: Vec<String> = match name {
        Some(n) => {
            manifest
                .circuit(&n)
                .ok_or_else(|| anyhow::anyhow!("no circuit '{n}' in lob.toml"))?;
            vec![n]
        }
        None => manifest.circuits.iter().map(|c| c.name.clone()).collect(),
    };
    if targets.is_empty() {
        println!("no circuits declared in lob.toml");
        return Ok(());
    }

    let mut failures: Vec<String> = Vec::new();
    for name in &targets {
        println!("\n━━━━━━━━━━  build {name}  ━━━━━━━━━━");
        let arg = || PathBuf::from(name);
        let steps: [(&str, Result<()>); 3] = [
            ("guide", guide_cmd(arg(), None, None, "auto".into())),
            ("bom", bom_cmd(arg(), false, None, true)),
            (
                "fab",
                fab_cmd(arg(), None, None, "analog".into(), 6, false, None),
            ),
        ];
        let mut done = Vec::new();
        let mut circuit_ok = true;
        for (label, res) in steps {
            match res {
                Ok(()) => done.push(label),
                Err(e) => {
                    eprintln!("  ✗ {label}: {e:#}");
                    circuit_ok = false;
                }
            }
        }
        if circuit_ok {
            println!("✓ {name}: {}", done.join(" + "));
        } else {
            failures.push(name.clone());
        }
    }

    println!(
        "\nbuilt {}/{} circuit(s)",
        targets.len() - failures.len(),
        targets.len()
    );
    if !failures.is_empty() {
        anyhow::bail!("incomplete: {}", failures.join(", "));
    }
    Ok(())
}

/// Handle `lob status` — per-circuit build freshness, no network. An artifact is
/// "stale" when the source (or panel, or the manifest) changed after it was
/// written; "—" when it was never built. Reads the shared [`ProjectView`] model
/// so the dashboard reports identical state (DESIGN 2.2).
fn status_cmd() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let view = ProjectView::discover(&cwd)
        .with_context(|| "no lob.toml found (run inside a circuits repo)")?;
    println!(
        "{}  [{}]",
        view.repo.name.as_deref().unwrap_or("(unnamed)"),
        view.root.display()
    );

    // The three artifacts `lob status` has always reported, in column order.
    let columns = [ArtifactKind::Guide, ArtifactKind::Vbom, ArtifactKind::Fab];
    for c in &view.circuits {
        let cols: Vec<String> = columns
            .iter()
            .map(|&kind| {
                let mark = match c.artifact(kind).map(|a| a.status) {
                    Some(ArtifactStatus::Fresh) => "✓",
                    Some(ArtifactStatus::Stale) => "stale",
                    _ => "—",
                };
                format!("{} {mark}", kind.label())
            })
            .collect();
        println!("  {:<18} {}", c.name, cols.join("   "));
    }
    Ok(())
}

/// Handle `lob serve [--bind]` — start the localhost dashboard backend over the
/// nearest circuits repo. The third head (DESIGN 2.2), no auth (DESIGN 2.5).
fn serve_cmd(bind: String) -> Result<()> {
    let addr: std::net::SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid --bind address '{bind}' (expected host:port)"))?;
    let cwd = std::env::current_dir()?;
    let root = Manifest::find_repo_root(&cwd)
        .ok_or_else(|| anyhow::anyhow!("no lob.toml found (run inside a circuits repo)"))?;
    println!("serving {} at http://{addr}", root.display());
    println!("  (Ctrl-C to stop)");
    legion_of_bom_web::serve_blocking(root, addr)
}

/// Handle `lob guide <circuit> [--out]` — generate a board and render a
/// step-by-step visual assembly guide (HTML).
fn guide_cmd(
    circuit: PathBuf,
    out: Option<PathBuf>,
    panel: Option<PathBuf>,
    kit: String,
) -> Result<()> {
    // A path is used directly; a bare name resolves via the repo's lob.toml
    // (source + panel + kit + build copy + brand). Explicit flags still win.
    let resolved = resolve_circuit(&circuit)?;
    let panel = panel.or_else(|| resolved.panel.clone());
    // Effective kit: an explicit --kit wins; else the manifest's; else auto.
    let kit = if kit != "auto" {
        kit
    } else {
        resolved.kit.clone().unwrap_or_else(|| "auto".into())
    };
    // Resolve the kit override up front so a bad value fails before the SKiDL run.
    let kit_override = match kit.as_str() {
        "auto" => None,
        other => Some(KitType::parse(other).ok_or_else(|| {
            anyhow::anyhow!("unknown kit '{other}' (expected auto | tht | smd | mixed)")
        })?),
    };
    let circuit = resolved
        .source
        .canonicalize()
        .with_context(|| format!("circuit not found: {}", resolved.source.display()))?;
    let stem = resolved.name.as_str();
    let work_dir = PathBuf::from("out").join(stem);

    let run = SkidlRunner::discover(&work_dir)
        .run(&circuit)
        .with_context(|| "SKiDL failed (try `lob doctor`)")?;
    let model = parse_netlist_file(&run.netlist_path)?;
    let footprint_dir = kicad_footprint_dir()
        .context("no KiCad footprint library found (set KICAD9_FOOTPRINT_DIR)")?;
    // Default: the PCB drives the panel — auto-derive one at minimum HP.
    let panel = effective_panel(panel, &model, &footprint_dir, &work_dir, stem)?;
    let options = board_options_with_panel(footprint_dir, &panel)?;
    let (board, _) = generate_board_report(&model, &options)?;

    let mut guide = build_guide(&model, &board).map_err(|e| anyhow::anyhow!(e))?;
    if let Some(kit) = kit_override {
        guide.kit = kit;
    }
    println!("  kit: {:?} (assembly copy + framing)", guide.kit);

    // Per-part assembly notes from the parts library (best-effort; keyed by MPN
    // via resolve_circuit). Skips silently when the library (dolt) is unavailable.
    if let Ok(lib) = PartsLibrary::open(default_parts_dir()) {
        if let Ok(resolutions) = lib.resolve_circuit(&model) {
            let notes: std::collections::BTreeMap<String, Vec<String>> = resolutions
                .into_iter()
                .filter_map(|r| {
                    let steps = r.record?.assembly_steps;
                    (!steps.is_empty()).then_some((r.refdes, steps))
                })
                .collect();
            if !notes.is_empty() {
                println!("  part notes: {} part(s) from the library", notes.len());
                guide.attach_part_notes(&notes);
            }
        }
    }

    // Per-circuit build copy + brand from the manifest (5uj.5).
    if resolved.build.is_some() || resolved.brand.is_some() {
        let b = resolved.build.clone().unwrap_or_default();
        guide.set_build_copy(resolved.brand.clone(), b.intro, b.tools, b.cautions);
        println!("  build copy: from lob.toml");
    }

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

/// Handle `lob bom <circuit> [--price] [--out] [--visual]`.
fn bom_cmd(circuit: PathBuf, price: bool, out: Option<PathBuf>, visual: bool) -> Result<()> {
    let resolved = resolve_circuit(&circuit)?;
    let stem = resolved.name.clone();
    let circuit = resolved
        .source
        .canonicalize()
        .with_context(|| "circuit not found")?;
    let stem = stem.as_str();
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
                Ok(Some(pp)) => {
                    // Keep the product photo for the Visual BOM, even if no price break.
                    line.image_url = pp.image_url.clone();
                    match pp.unit_price_at(line.qty() as u64) {
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
                    }
                }
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

    if visual {
        // Hydrate line photos from the parts library first (curated/scripted
        // per-MPN images — the durable source for boutique parts). Best-effort:
        // skip silently if the library (dolt) isn't available.
        if let Ok(lib) = PartsLibrary::open(default_parts_dir()) {
            for line in &mut bom.lines {
                if line.image_url.is_some() {
                    continue;
                }
                if let Some(mpn) = &line.mpn {
                    if let Ok(Some(rec)) = lib.get_part(mpn) {
                        line.image_url = rec.image_url;
                    }
                }
            }
        }

        // Resolve + cache + embed a photo per line: the library/curated image
        // first, else an EasyEDA/LCSC auto-lookup; lines with none fall back to a
        // color swatch (THT resistors) or a blank cell.
        let cache = default_image_cache_dir();
        let mut fetched = 0usize;
        let thumbs: Vec<Option<String>> = bom
            .lines
            .iter()
            .map(|l| {
                let t = resolve_photo(l, &cache);
                fetched += t.is_some() as usize;
                t
            })
            .collect();
        std::fs::create_dir_all(&work_dir)?;
        let vpath = work_dir.join(format!("{stem}-vbom.html"));
        std::fs::write(&vpath, bom.to_visual_html(stem, &thumbs))
            .with_context(|| format!("writing {}", vpath.display()))?;
        println!(
            "wrote {} (Visual BOM, {fetched}/{} photo(s))",
            vpath.display(),
            bom.lines.len()
        );
    }
    Ok(())
}

/// Resolve an embeddable thumbnail (`data:` URI) for a BOM line: a curated or
/// priced `image_url` first, else an EasyEDA/LCSC product photo looked up by MPN
/// or distinctive value. `None` → the Visual BOM falls back to a swatch / blank.
fn resolve_photo(line: &BomLine, cache: &Path) -> Option<String> {
    // A curated/library image (may be a `file://` local photo) wins.
    if let Some(src) = &line.image_url {
        if let Some(thumb) = embed_source(src, cache) {
            return Some(thumb);
        }
    }
    let keyword = photo_keyword(line)?;
    let url = product_image_url(&keyword)?;
    fetch_data_uri(&url, cache)
}

/// The photo-search keyword for a line, or `None` when a photo isn't wanted:
/// passives (R/C) get a swatch/blank, and a generic value with no MPN (e.g.
/// `"100k"`) would only return noise — so require a part-number-like token.
fn photo_keyword(line: &BomLine) -> Option<String> {
    let prefix: String = line
        .refdes
        .first()?
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    if matches!(prefix.as_str(), "R" | "C") {
        return None;
    }
    let keyword = line
        .mpn
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(line.value.as_str());
    if keyword.is_empty() || (line.mpn.is_none() && !has_letter_run(keyword, 2)) {
        return None;
    }
    Some(keyword.to_string())
}

/// Whether `s` contains a run of at least `n` consecutive ASCII letters — a cheap
/// "looks like a part number, not a bare value" test (`"LM13700"`/`"TL072"` yes,
/// `"100k"`/`"4.7k"` no — a passive's unit suffix is a lone letter).
fn has_letter_run(s: &str, n: usize) -> bool {
    let mut run = 0usize;
    for c in s.chars() {
        run = if c.is_ascii_alphabetic() { run + 1 } else { 0 };
        if run >= n {
            return true;
        }
    }
    false
}

/// Handle `lob parts …` against the global parts library.
fn parts_cmd(action: PartsCmd) -> Result<()> {
    // `suggest` is about parts NOT yet in the library (generic, no MPN), so it
    // doesn't need Dolt — handle it before opening the library.
    if let PartsCmd::Suggest { circuit, limit } = action {
        return suggest_cmd(circuit, limit);
    }
    let lib = PartsLibrary::open(default_parts_dir())
        .with_context(|| "opening the parts library (is `dolt` installed?)")?;
    match action {
        PartsCmd::Learn { packages } => {
            let (mut learned, mut skipped) = (0usize, 0usize);
            for dir in &packages {
                let board = legion_of_bom_core::read_package(dir)
                    .with_context(|| format!("reading {}", dir.display()))?;
                let source = dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                for part in &board.parts {
                    // Only learn from a line that names a real part. A value or a
                    // signal label tells us nothing about what to buy.
                    // A part-number column is authoritative. Only fall back to
                    // reading the comment when the board gives us no column —
                    // a comment like `100nf50V0603` merely *looks* like a part
                    // number and must not outrank the real one.
                    let mpn = match &part.part_number {
                        Some(m) if !m.is_empty() => m.clone(),
                        _ => match plan_repair(&part.value, &part.footprint) {
                            Repair::UsePartNumber(m) => m,
                            _ => {
                                skipped += 1;
                                continue;
                            }
                        },
                    };
                    let refdes = part.refdes.first().map(String::as_str).unwrap_or("");
                    lib.learn_house_part(
                        part_kind_of(refdes, &part.footprint),
                        &value_key(&part.value, &part.footprint),
                        &package_key(&part.footprint),
                        &mpn,
                        &source,
                    )?;
                    learned += 1;
                }
                println!("  {source}: {} BOM line(s)", board.parts.len());
            }
            println!(
                "learned {learned} part choice(s) from {} board(s); {skipped} line(s) named no part",
                packages.len()
            );
        }

        PartsCmd::House { kind } => {
            let all = lib.house_parts()?;
            let shown: Vec<_> = all
                .iter()
                .filter(|h| kind.as_deref().is_none_or(|k| h.kind == k))
                .collect();
            if shown.is_empty() {
                println!("nothing learned yet — try `lob parts learn <package>...`");
                return Ok(());
            }
            for h in &shown {
                let val = if h.value.is_empty() { "—" } else { &h.value };
                println!(
                    "  {:<10} {:<10} {:<10} {:<22} x{}  ({})",
                    h.kind, val, h.package, h.mpn, h.uses, h.seen_on
                );
            }
            println!("{} part choice(s)", shown.len());
        }

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
        PartsCmd::SetImage { mpn, source } => {
            // A local file is stored as an absolute `file://` URL (durable across
            // working directories); anything else is taken as an http(s) URL.
            let stored = if Path::new(&source).is_file() {
                let abs = std::fs::canonicalize(&source)
                    .with_context(|| format!("resolving image path {source}"))?;
                format!("file://{}", abs.display())
            } else {
                source.clone()
            };
            lib.set_image_url(&mpn, Some(&stored))?;
            lib.commit(&format!("parts: set image {mpn}"))?;
            println!("set image for {mpn}: {stored}");
        }
        PartsCmd::SetAssembly { mpn, notes } => {
            lib.set_assembly_steps(&mpn, &notes)?;
            lib.commit(&format!("parts: set assembly {mpn}"))?;
            if notes.is_empty() {
                println!("cleared assembly notes for {mpn}");
            } else {
                println!("set {} assembly note(s) for {mpn}:", notes.len());
                for (i, n) in notes.iter().enumerate() {
                    println!("  {}. {n}", i + 1);
                }
            }
        }
        PartsCmd::Resolve { circuit } => {
            print_resolutions(&resolve_circuit_file(&lib, circuit)?);
        }
        // Handled before the library is opened (it needs no Dolt).
        PartsCmd::Suggest { .. } => unreachable!("suggest is dispatched before lib open"),
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
/// `lob import eagle` — read an Eagle design into a circuit, and write SKiDL.
fn import_cmd(action: ImportCmd) -> Result<()> {
    match action {
        ImportCmd::Eagle {
            schematic,
            board,
            skidl,
        } => {
            let xml = std::fs::read_to_string(&schematic)
                .with_context(|| format!("reading {}", schematic.display()))?;
            let name = schematic
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("imported");
            let imp = legion_of_bom_core::parse_eagle_schematic(&xml, name);

            println!(
                "{name}: {} part(s), {} net(s)",
                imp.circuit.parts.len(),
                imp.circuit.nets.len()
            );
            if !imp.symbols_skipped.is_empty() {
                println!(
                    "  {} schematic symbol(s) skipped (frames, GND/supply markers — not components)",
                    imp.symbols_skipped.len()
                );
            }
            if !imp.unmapped_footprints.is_empty() {
                println!(
                    "  ⚠ {} package(s) have no KiCad footprint and are marked `eagle:` — \
                     resolve before laying this out:",
                    imp.unmapped_footprints.len()
                );
                for p in imp.unmapped_footprints.iter().take(12) {
                    println!("      {p}");
                }
                if imp.unmapped_footprints.len() > 12 {
                    println!("      … and {} more", imp.unmapped_footprints.len() - 12);
                }
            }

            if let Some(brd) = &board {
                let btext = std::fs::read_to_string(brd)
                    .with_context(|| format!("reading {}", brd.display()))?;
                let places = legion_of_bom_core::parse_eagle_board(&btext);
                let back = places.iter().filter(|p| p.back).count();
                println!(
                    "  board: {} placement(s), {back} on the back — the existing layout, reusable as-is",
                    places.len()
                );
            }

            let out = skidl.unwrap_or_else(|| schematic.with_extension("py"));
            std::fs::write(&out, legion_of_bom_core::to_skidl(&imp))
                .with_context(|| format!("writing {}", out.display()))?;
            println!("  SKiDL: {}", out.display());
            println!(
                "  NOTE: symbol libraries are inferred from each reference designator — \
                 review the Part(...) lines before running it."
            );
            Ok(())
        }

        ImportCmd::Repair {
            package,
            search,
            limit,
        } => {
            let board = legion_of_bom_core::read_package(&package)
                .with_context(|| format!("reading {}", package.display()))?;
            let clients = search.then(SourcingClients::from_env).unwrap_or_default();
            if search && !clients.any() {
                println!("(no distributor key found — set MOUSER_API_KEY to search)");
            }

            // The parts we already buy are the best answer available, and they
            // cost nothing to consult — a jack or a pot we have shipped before
            // is known good, so it should never reach a distributor search.
            let lib = PartsLibrary::open(default_parts_dir()).ok();

            let (mut have, mut found, mut todo, mut skip) = (0usize, 0usize, 0usize, 0usize);
            let mut known = 0usize;
            for part in &board.parts {
                let refs = part.refdes.join(", ");
                if part.part_number.is_some() {
                    have += 1;
                    continue;
                }
                match plan_repair(&part.value, &part.footprint) {
                    Repair::UsePartNumber(mpn) => {
                        found += 1;
                        println!("  {refs:<24} {mpn}   (from the comment)");
                    }
                    Repair::Search(keyword) => {
                        let refdes = part.refdes.first().map(String::as_str).unwrap_or("");
                        let hit = lib.as_ref().and_then(|l| {
                            l.house_part(
                                part_kind_of(refdes, &part.footprint),
                                &value_key(&part.value, &part.footprint),
                                &package_key(&part.footprint),
                            )
                            .ok()
                            .flatten()
                        });
                        if let Some(h) = hit {
                            known += 1;
                            let how = if h.exact { "we use" } else { "closest we use" };
                            println!(
                                "  {refs:<24} {}   ({how}, x{} on {})",
                                h.mpn, h.uses, h.seen_on
                            );
                            continue;
                        }
                        todo += 1;
                        println!("  {refs:<24} ? {keyword}");
                        if search && clients.any() {
                            for c in suggest_by_keyword(&keyword, &clients, limit) {
                                let stock = c
                                    .in_stock
                                    .map(|n| format!(", {n} in stock"))
                                    .unwrap_or_default();
                                let price = c
                                    .unit_price
                                    .map(|p| format!(", ${p:.3}"))
                                    .unwrap_or_default();
                                println!(
                                    "        {} [{}{stock}{price}]",
                                    c.mpn,
                                    c.manufacturer.as_deref().unwrap_or(c.source)
                                );
                            }
                        }
                    }
                    Repair::NotSourceable(what) => {
                        skip += 1;
                        println!("  {refs:<24} — {what}");
                    }
                }
            }
            println!(
                "\n{have} already had a part number · {found} recovered from the comment · \
                 {known} known from parts we use · {todo} need a search · \
                 {skip} not sourceable from this BOM"
            );
            if found > 0 {
                println!(
                    "Recovered numbers come from what the author wrote, not from a \
                     distributor — confirm them before ordering."
                );
            }
            Ok(())
        }

        ImportCmd::Guide { package, name, out } => {
            let board = legion_of_bom_core::read_package(&package)
                .with_context(|| format!("reading {}", package.display()))?;
            let stem = name.unwrap_or_else(|| {
                package
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("imported")
                    .to_string()
            });
            let dir = out.unwrap_or_else(|| package.clone());
            std::fs::create_dir_all(&dir)?;

            let guide = board.to_guide(&stem);
            let steps = guide.steps.len();
            let placed: usize = guide.steps.iter().map(|s| s.parts.len()).sum();
            // No photoreal render: an imported board has gerbers, not a KiCad
            // board we can ask kicad-cli to draw.
            let html = guide_to_html(&guide, None, None);
            let gpath = dir.join(format!("{stem}-guide.html"));
            std::fs::write(&gpath, html).with_context(|| format!("writing {}", gpath.display()))?;
            println!("{stem}: {steps} step(s), {placed} part(s)");
            println!("  guide: {}", gpath.display());

            let bom = board.to_bom();
            let cache = default_image_cache_dir();
            let mut fetched = 0usize;
            let thumbs: Vec<Option<String>> = bom
                .lines
                .iter()
                .map(|l| {
                    let t = resolve_photo(l, &cache);
                    fetched += t.is_some() as usize;
                    t
                })
                .collect();
            let vpath = dir.join(format!("{stem}-vbom.html"));
            std::fs::write(&vpath, bom.to_visual_html(&stem, &thumbs))
                .with_context(|| format!("writing {}", vpath.display()))?;
            println!(
                "  vbom:  {} ({fetched}/{} photo(s))",
                vpath.display(),
                bom.lines.len()
            );

            let with_mpn = bom.lines.iter().filter(|l| l.mpn.is_some()).count();
            if with_mpn < bom.lines.len() {
                println!(
                    "  NOTE: {}/{} BOM lines carry no part number — this package left the \
                     distributor column blank, so those cannot be priced or ordered directly. \
                     `lob parts suggest` can propose candidates from value + package.",
                    bom.lines.len() - with_mpn,
                    bom.lines.len()
                );
            }
            Ok(())
        }
    }
}

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
        PanelCmd::Pcb { spec, out, logo } => {
            let toml = std::fs::read_to_string(&spec)
                .with_context(|| format!("reading {}", spec.display()))?;
            let file = PanelFile::from_toml(&toml)
                .with_context(|| format!("parsing {}", spec.display()))?;
            let panel = file
                .to_spec()
                .map_err(|e| anyhow::anyhow!("invalid panel spec: {e}"))?;
            let stem = spec.file_stem().and_then(|s| s.to_str()).unwrap_or("panel");
            // Drop a trailing "_panel"/"-panel" and prettify: "slew_limiter_panel"
            // → "Slew Limiter".
            let title = pretty_title(stem.trim_end_matches("_panel").trim_end_matches("-panel"));
            let logo = load_logo(&logo)?;
            let pcb = panel_to_kicad_pcb(panel.as_ref(), &title, logo.as_ref());
            let out_path = out.unwrap_or_else(|| spec.with_extension("kicad_pcb"));
            std::fs::write(&out_path, pcb)
                .with_context(|| format!("writing {}", out_path.display()))?;
            println!("wrote {}", out_path.display());
            println!(
                "  panel PCB: {:.2} mm × {:.2} mm ({} HP), {} hole(s), {} cutout(s)",
                panel.width_mm(),
                panel.height_mm(),
                (panel.width_mm() / 5.08).round() as i64,
                panel.mounting_holes().len(),
                panel.cutouts().len(),
            );
            // Gerbers, if kicad-cli is available (panels are mechanical: Edge.Cuts + silk).
            if let Some(kicad) = kicad_cli_path() {
                let gdir = out_path
                    .with_extension("")
                    .with_file_name(format!("{stem}-panel-gerbers"));
                match export_gerbers(&out_path, &gdir, &kicad) {
                    Ok(()) => {
                        let zip = gdir.with_extension("zip");
                        let zipped = zip_dir(&gdir, &zip).unwrap_or(false);
                        println!(
                            "  gerbers: {}",
                            if zipped {
                                zip.display().to_string()
                            } else {
                                gdir.display().to_string()
                            }
                        );
                    }
                    Err(e) => println!("  gerbers: skipped ({e})"),
                }
            }
        }
        PanelCmd::Derive { circuit, hp, out } => {
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
            // Default: let the PCB drive the width (minimum HP it fits in).
            let hp = match hp {
                Some(h) => h,
                None => {
                    let footprint_dir = kicad_footprint_dir()
                        .context("no KiCad footprint library found (set KICAD9_FOOTPRINT_DIR)")?;
                    let facts = build_facts(&model, &footprint_dir)?;
                    let min = minimum_hp(&model, &facts);
                    println!("auto width: minimum {min} HP");
                    min
                }
            };
            // Cutout dims resolve through the CutoutSource seam; BuiltinCutouts is
            // the fallback until the parts library carries verified mechanical data.
            let requested_hp = hp;
            let mut panel = derive_panel(&model, hp, &BuiltinCutouts);
            // derive_panel widens a panel too narrow for its own hardware, so report
            // what was actually emitted, not what was asked for.
            let hp = panel.hp.unwrap_or(hp);
            if hp > requested_hp {
                println!("  widened to {hp} HP — {requested_hp} HP can't fit the control hardware");
            }
            let out_path =
                out.unwrap_or_else(|| circuit.with_file_name(format!("{stem}_panel.toml")));
            // Re-deriving must not wipe the builder-owned finish / thickness they set
            // on the existing spec (cutout topology is what we're regenerating).
            if let Some(prev) = std::fs::read_to_string(&out_path)
                .ok()
                .and_then(|t| PanelFile::from_toml(&t).ok())
            {
                panel.finish = prev.finish;
                panel.thickness_mm = prev.thickness_mm;
            }
            let toml = panel
                .to_toml()
                .map_err(|e| anyhow::anyhow!("serialising panel: {e}"))?;
            std::fs::write(&out_path, &toml)
                .with_context(|| format!("writing {}", out_path.display()))?;
            println!(
                "derived panel: {} ({} control(s), {} HP)",
                out_path.display(),
                panel.cutouts.len(),
                hp
            );
            for c in &panel.cutouts {
                println!(
                    "  {:<4} {:<10} @ ({:5.1}, {:5.1})  {}",
                    c.refdes.as_deref().unwrap_or("?"),
                    c.footprint,
                    c.x_mm,
                    c.y_mm,
                    c.label.as_deref().unwrap_or("")
                );
            }
        }
        PanelCmd::Fit { circuit } => {
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
            let facts = build_facts(&model, &footprint_dir)?;
            let hp = minimum_hp(&model, &facts);
            let mm = f64::from(hp) * 5.08;
            println!("minimum panel width: {hp} HP ({mm:.1} mm) for {stem}");
            println!(
                "  {} panel-facing control(s), {} part(s) total",
                derive_panel(&model, hp, &BuiltinCutouts).cutouts.len(),
                model.parts().len()
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

/// Handle `lob parts suggest <circuit> [--limit N]` — run SKiDL → netlist → model,
/// then for each part with NO MPN, print ranked real-MPN candidates from the
/// distributors we have access to. SUGGEST-ONLY: prints only; a human confirms one
/// via `lob parts fetch`/`verify` (the okm gate stays intact). Degrades gracefully
/// when a distributor key is absent.
fn suggest_cmd(circuit: PathBuf, limit: usize) -> Result<()> {
    let resolved = resolve_circuit(&circuit)?;
    let stem = resolved.name.clone();
    let circuit = resolved
        .source
        .canonicalize()
        .with_context(|| "circuit not found")?;
    let work_dir = PathBuf::from("out").join(&stem);
    let run = SkidlRunner::discover(&work_dir)
        .run(&circuit)
        .with_context(|| "SKiDL failed (try `lob doctor`)")?;
    let model = parse_netlist_file(&run.netlist_path)?;

    let clients = SourcingClients::from_env();
    // Tell the user what's active — never panic on a missing key.
    let mut sources = Vec::new();
    if clients.mouser.is_some() {
        sources.push("Mouser");
    } else {
        eprintln!("  note: set MOUSER_API_KEY to enable Mouser keyword search");
    }
    if clients.use_lcsc {
        sources.push("LCSC/EasyEDA (keyless)");
    }
    if !clients.any() {
        anyhow::bail!("no distributor sources available (set MOUSER_API_KEY, or enable LCSC)");
    }
    println!("suggesting MPNs via: {}\n", sources.join(" + "));

    // Only parts with no MPN need a suggestion; group by (value, footprint) so we
    // search each distinct generic part once, not per reference designator.
    use std::collections::BTreeMap;
    let mut generics: BTreeMap<(String, Option<String>), (legion_of_bom_core::Part, Vec<String>)> =
        BTreeMap::new();
    let mut resolved_count = 0usize;
    for part in model.parts() {
        if part.mpn.is_some() {
            resolved_count += 1;
            continue;
        }
        let key = (part.value.clone(), part.footprint.clone());
        generics
            .entry(key)
            .or_insert_with(|| (part.clone(), Vec::new()))
            .1
            .push(part.refdes.0.clone());
    }

    if generics.is_empty() {
        println!(
            "every part already declares an MPN ({resolved_count} part(s)) — nothing to suggest"
        );
        return Ok(());
    }

    for ((value, footprint), (part, mut refdes)) in generics {
        refdes.sort();
        let fp = footprint.as_deref().unwrap_or("(no footprint)");
        println!("● {}  {value}  [{fp}]", refdes.join(", "));
        let query = legion_of_bom_core::build_query(&part);
        if let Some(q) = &query {
            println!("    query: \"{q}\"");
        }
        let candidates = suggest_mpns(&part, &clients, limit);
        if candidates.is_empty() {
            println!("    (no candidates — try a more specific value/footprint, or add by hand)");
        }
        for (i, c) in candidates.iter().enumerate() {
            let mfr = c.manufacturer.as_deref().unwrap_or("?");
            let stock = c
                .in_stock
                .map(|s| format!("{s} in stock"))
                .unwrap_or_else(|| "stock ?".into());
            let price = c
                .unit_price
                .map(|p| format!("${p:.4}"))
                .unwrap_or_else(|| "$ ?".into());
            let pkg = c
                .package
                .as_deref()
                .map(|p| format!(" · {p}"))
                .unwrap_or_default();
            let lcsc = c
                .lcsc_code
                .as_deref()
                .map(|l| format!(" · {l}"))
                .unwrap_or_default();
            println!(
                "    {}. {:<22} {mfr:<16} {stock:<16} {price:<9}{pkg}{lcsc}  [{}]",
                i + 1,
                c.mpn,
                c.source,
            );
            if let Some(ds) = &c.datasheet_url {
                println!("       datasheet: {ds}");
            }
        }
        // The confirm path — never silent-assign (okm gate).
        if let Some(top) = candidates.first() {
            let fetch_hint = match top.lcsc_code.as_deref() {
                Some(code) => format!("lob parts fetch {code} --source jlcpcb"),
                None => format!("lob parts add {} --manufacturer '{}'", top.mpn, {
                    top.manufacturer.as_deref().unwrap_or("")
                }),
            };
            println!(
                "    → confirm: {fetch_hint}  then  lob parts verify {}",
                top.mpn
            );
        }
        println!();
    }

    println!(
        "{} generic part group(s) need an MPN; {resolved_count} part(s) already resolved.\n\
         Suggestions are NOT auto-assigned — confirm one, then `lob parts verify` (okm gate).",
        // recompute count of groups printed
        model
            .parts()
            .iter()
            .filter(|p| p.mpn.is_none())
            .map(|p| (p.value.clone(), p.footprint.clone()))
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
    Ok(())
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
    println!("image:        {}", part.image_url.as_deref().unwrap_or("-"));
    if !part.assembly_steps.is_empty() {
        println!("assembly:");
        for (i, step) in part.assembly_steps.iter().enumerate() {
            println!("  {}. {step}", i + 1);
        }
    }
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

/// Load API credentials into the process environment before anything reads it.
///
/// Precedence, highest first: variables already set in the real environment,
/// then a repo-local `.env` (a dev convenience, searched from the cwd upward),
/// then the user-global `~/.lob/credentials`. `dotenvy` never overrides an
/// already-set variable, so loading local before global gives local precedence
/// while the global file supplies the keys from *any* working directory — so
/// `lob bom --price` and `lob serve` find `MOUSER_API_KEY` when run from inside
/// a circuits repo, not only from this checkout.
fn load_credentials() {
    // Repo-local .env (dev override).
    let _ = dotenvy::dotenv();
    // User-global credentials — the durable home for API keys.
    if let Some(home) = std::env::var_os("HOME") {
        let global = Path::new(&home).join(".lob").join("credentials");
        let _ = dotenvy::from_path(&global);
    }
}
