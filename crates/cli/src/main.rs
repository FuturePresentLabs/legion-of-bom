//! `lob` â the legion-of-bom command-line interface.
//!
//! A thin wrapper over `legion-of-bom-core`. The `run` subcommand runs the full
//! Phase 0 pipeline â SKiDL run â parse â validate â simulate â verify â BOM â
//! and reports per-stage pass/fail, exiting non-zero on any failure.

mod doctor;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use legion_of_bom_core::family;
use legion_of_bom_core::skidl::{kicad_footprint_dir, kicad_symbol_dir};
use legion_of_bom_core::{
    analytic_check, build_facts, build_guide_with, default_image_cache_dir,
    default_panel_orders_dir, default_parts_dir, derive_panel, derive_panel_for, embed_source,
    eurorack_trial_build, export_board_glb, export_cpl, export_gerbers, fetch_from_jlcpcb,
    fetch_from_kicad, free_outline_template, generate_board_artifacts, generate_bom,
    generate_fuzz_chain, guide, guide_to_html, guide_to_pdf, jlc_assembly_bom, jlcpcb_design_rules,
    kicad_cli_path, min_panel_hp_for, minimum_hp, minimum_routable_hp, package_key,
    panel_from_board, panel_to_dxf, panel_to_kicad_pcb, parse_netlist_file, part_kind_of,
    photo_source, plan_repair, png_to_jpeg, render_board_png, render_spec_text, rules, run_drc,
    run_layout_loop, schematic_to_svg, simulate_ac, simulate_tran, simulate_tran_drive,
    suggest_by_keyword, suggest_mpns, svg_to_pdf_bytes, validate_erc, value_key, zip_dir,
    ArtifactKind, ArtifactStatus, BoardOptions, BoardPng, BomLine, BuildCopy, BuiltinCutouts,
    CircuitSource, EnclosureSize, EurorackPlacer, Finding, FuzzConstraints, GuideOptions, HpSearch,
    JlcpcbClient, KitType, LayoutLoop, LayoutMode, Logo, Manifest, MouserClient, PanelFile,
    PanelFormat, PanelOrders, PartRecord, PartResolution, PartsLibrary, PipelineReport,
    PlacementFile, Populate, ProjectView, Quality, Repair, ResolutionStatus, SeededPlacer,
    Severity, SilkLegend, SimConfig, SkidlRunner, SourcingClients, StageOutcome, TranAnalysis,
    TranDrive,
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
    /// Show each circuit's build state â which artifacts exist and whether they
    /// are stale relative to the source + manifest. No network.
    Status,
    /// Serve the local web dashboard (localhost, no auth) for this repo â the
    /// read-only Forestry-style viewer over the same core.
    Serve {
        /// Address to bind (host:port).
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
    },
    /// Check that the external toolchain (ngspice, kicad-cli, SKiDL) is available.
    Doctor,
    /// Inspect and edit the global, SQLite-backed parts library.
    Parts {
        #[command(subcommand)]
        action: PartsCmd,
    },
    /// The catalog of known-good parts synthesis chooses from (catalog/parts).
    Catalog {
        #[command(subcommand)]
        action: CatalogCmd,
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
        /// Also write a Visual BOM (HTML): a part photo per line (Thonk, then
        /// EasyEDA/LCSC â both keyless), with through-hole resistors shown as
        /// their color code.
        #[arg(long)]
        visual: bool,
        /// Keep surface-mount parts on the sorting sheet. Off by default: on a
        /// mixed kit the fab has already reflowed them, so a cell for one is a
        /// cell the builder never uses.
        #[arg(long)]
        smd: bool,
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
        #[arg(long, default_value = LAYOUT_MODE)]
        mode: String,
        /// Iterative layout attempts over a panel board (0 = one-shot placement).
        #[arg(long, default_value_t = 6)]
        iterations: usize,
        /// Brand logo SVG to render on the back silk (bottom-centre).
        #[arg(long)]
        logo: Option<PathBuf>,
        /// Also write a combined, named, colored 3D scene (.glb) of the
        /// placed board â for an external model viewer, never a rendered
        /// image. See `legion_of_bom_core::export_board_glb`.
        #[arg(long)]
        model: Option<PathBuf>,
    },
    /// Render a readable schematic diagram (symbols + routed nets) from a
    /// circuit â SVG for viewing/iterating, PDF for a shareable final export.
    /// At least one of --svg/--pdf is required.
    Diagram {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
        /// Write the diagram as SVG here.
        #[arg(long)]
        svg: Option<PathBuf>,
        /// Write the diagram as PDF here (one page, sized to the diagram).
        #[arg(long)]
        pdf: Option<PathBuf>,
    },
    /// Drive a sine sweep into a circuit's recognized input net and measure
    /// gain compression / flat-topping â "does this circuit actually clip
    /// like a fuzz" as a measured SPICE claim, not a schematic read. Exits
    /// non-zero only if the measurement itself couldn't run (SKiDL/netlist
    /// failure, or no recognizable input net) â a low/uninteresting
    /// flat-top is still exit 0, since "it doesn't clip at this drive
    /// level" is a valid, successfully-measured answer. Threshold
    /// pass/fail against a task's rubric is the caller's job (see
    /// PCBBench's `Check::SpiceClips`), not this command's.
    ScopeProbe {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
        /// Override which net to drive (default: SimConfig::infer's guess).
        #[arg(long)]
        input_net_hint: Option<String>,
        /// Sweep drive frequency (Hz).
        #[arg(long, default_value_t = 200.0)]
        freq_hz: f64,
        /// Smallest drive amplitude, peak volts.
        #[arg(long, default_value_t = 0.005)]
        min_amplitude: f64,
        /// Largest drive amplitude, peak volts.
        #[arg(long, default_value_t = 1.0)]
        max_amplitude: f64,
        /// Sweep points between min and max amplitude (log-spaced).
        #[arg(long, default_value_t = 8)]
        steps: usize,
        /// Write the measurement report here as JSON.
        #[arg(long)]
        out: PathBuf,
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
        #[arg(long, default_value = LAYOUT_MODE)]
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
        /// mixed. THT-first framing + copy â most DIY kits are through-hole.
        #[arg(long, default_value = "auto")]
        kit: String,
        /// Layout cost-function mode: analog | digital | mixed.
        ///
        /// Must match what `lob fab` is given, or the guide documents a different
        /// board from the one manufactured (`legion-of-bom-p6m`). Before this
        /// existed, `guide` had no way to be told and always laid out `analog`.
        #[arg(long, default_value = LAYOUT_MODE)]
        mode: String,
    },
    /// Import a board from another EDA tool.
    Import {
        #[command(subcommand)]
        action: ImportCmd,
    },
    /// Set up (or tidy) a circuits repo: write the .gitignore for lob's
    /// generated outputs, and report any generated files that are tracked.
    ///
    /// Idempotent â safe to re-run after upgrading lob to pick up new patterns.
    Init {
        /// Report what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Panel design: generate DXF, track orders.
    Panel {
        #[command(subcommand)]
        action: PanelCmd,
    },
    /// Concept -> spec: ask every typed decision for a design brief and write
    /// a spec -- raw text plus a machine-readable JSON, no schematic
    /// (legion-of-bom-utn). Needs OODA_API_KEY (and optionally OODA_BASE_URL
    /// for a local dev endpoint) -- see .env.example. Follow with
    /// `lob schematic` to render the circuit from the written spec.
    Spec {
        /// Circuit family from the curated library: "board" (synthesized from
        /// the parts catalog) or "fuzz-pedal".
        family: String,
        /// Free-text design brief, carried as decision context (not parsed for
        /// control flow -- the decisions, not the brief, choose the circuit).
        #[arg(long)]
        brief: String,
        /// Base output path (no extension): writes "<out>.json" (machine-
        /// readable, input to `lob schematic`) and "<out>.txt" (human-readable).
        #[arg(long)]
        out: PathBuf,
        /// Also write the full decision trace (JSON: key/type/chosen/confidence)
        /// here, for eval scoring (e.g. PCBBench).
        #[arg(long)]
        trace: Option<PathBuf>,
    },
    /// Concept -> spec, DAG variant: assemble a fuzz-pedal circuit as a
    /// chain of gain-stage nodes via sequential typed decisions ("add
    /// another stage, or stop?") instead of picking from one fixed
    /// two-stage template -- "rail -> node -> node -> ... -> output".
    /// Supply voltage and enclosure size are constraints on the request
    /// (specified, not decided); chain length, per-stage voicing, current
    /// budget, and tone-stack presence are real decisions. Needs
    /// OODA_API_KEY, same as `lob spec`.
    SpecChain {
        /// Free-text design brief, carried as decision context.
        #[arg(long)]
        brief: String,
        /// Supply rail voltage -- a constraint, not a decision (9.0 for a
        /// standard battery/adapter pedal, 18.0 for a two-battery/boosted one).
        #[arg(long, default_value_t = 9.0)]
        vcc: f64,
        /// Enclosure size class -- a constraint, not a decision: 1590b | 1590bb | 125b.
        #[arg(long, default_value = "1590b")]
        enclosure: String,
        /// Base output path (no extension): writes "<out>.json" and "<out>.txt".
        #[arg(long)]
        out: PathBuf,
        /// Also write the full decision trace (JSON) here.
        #[arg(long)]
        trace: Option<PathBuf>,
    },
    /// Spec -> design: render a SKiDL schematic from a spec file written by
    /// `lob spec`. A pure function of the spec -- no decision calls, no
    /// network -- so the schematic traces back to exactly what System One
    /// decided, replayably.
    Schematic {
        /// Spec JSON path (from `lob spec ... --out <base>`, i.e. "<base>.json").
        spec: PathBuf,
        /// Write the rendered SKiDL circuit here.
        #[arg(long)]
        out: PathBuf,
        /// Also write a Guitar Pedal panel TOML sized to the spec's enclosure
        /// class, with the Fuzz/Volume pots anchored to the circuit's RV1/RV2
        /// refdes -- a pure function of the spec, same as the schematic
        /// itself, so `lob board --panel <this file>` has real cutouts to
        /// check the circuit against without a further design decision.
        #[arg(long)]
        panel: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum CatalogCmd {
    /// Check a catalog: it loads, every pin name and alternate exists in its
    /// KiCad symbol, and every quote is on its page of the pinned datasheet
    /// (fetched into the datasheet store). Exits non-zero on any problem.
    Check {
        /// Catalog directory (default: this checkout's catalog/parts).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// List the parts and the roles each can fill.
    List {
        #[arg(long)]
        dir: Option<PathBuf>,
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
        /// Fab-package directories, or Eagle `.sch` files/folders, to learn from.
        packages: Vec<PathBuf>,
        /// A BOM naming what was bought, joined to an Eagle source by refdes.
        ///
        /// Eagle records what a board is made of, not what was ordered, so a
        /// schematic alone can only say which parts we need an answer for. Pair
        /// it with the BOM that built the board and both halves are present.
        #[arg(long)]
        bom: Option<PathBuf>,
    },
    /// Attach our own photo to a part we build with.
    ///
    /// A photo of the actual part beats a datasheet render when two jacks look
    /// alike on the bench. Takes a URL or a path; a path inside the repo is
    /// stored relative to it so the library travels with a clone.
    Photo {
        /// Part kind (jack, pot, resistor, ...) â as shown by `lob parts house`.
        kind: String,
        /// Value, or `-` for a part that has none (a jack, an IC).
        value: String,
        /// Package key, as shown by `lob parts house`.
        package: String,
        /// Image URL, or a path to an image file.
        photo: String,
    },
    /// Show the parts we build with, most-used first.
    House {
        /// Only this kind (resistor, capacitor, jack, pot, ic, ...).
        #[arg(long)]
        kind: Option<String>,
        /// Write CSV instead of a table â a reviewable, diffable export, since
        /// the SQLite file itself isn't (the same split as `.beads`).
        #[arg(long)]
        csv: bool,
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
    /// Set a part's Visual-BOM photo by MPN â a URL or a local image file. For
    /// boutique parts a distributor lookup can't cover (Thonkiconn jacks, pots);
    /// `scripts/pull_part_image.py` populates these from Thonk/Tayda/EasyEDA.
    SetImage {
        /// Manufacturer part number to attach the photo to.
        mpn: String,
        /// Image URL (http/https) or a path to a local image file.
        source: String,
    },
    /// Set a part's build-guide assembly notes by MPN â ordered part-specific tips
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
    /// Mouser keyword search + the LCSC/EasyEDA catalog. SUGGEST-ONLY â it prints
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
    /// This is the check `layout` / real BOM ordering enforce (okm.4) â the
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
    /// result is a circuit the pipeline can work on â and can be re-emitted as
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
    /// Generate a panel PCB (.kicad_pcb) + gerbers from a panel spec TOML â the
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
    /// (jacks/pots/switches) â instead of hand-writing cutout coordinates.
    Derive {
        /// Path to the circuit definition (e.g. a SKiDL script).
        circuit: PathBuf,
        /// Panel width in HP. Omit to size to the minimum HP the PCB fits in â
        /// the PCB drives the panel (DESIGN 6.1), which is the default.
        #[arg(long)]
        hp: Option<u16>,
        /// Height class: `eurorack` (3U, default), `intellijel-1u`, or
        /// `pulplogic-1u`. Bare `1u` means Intellijel.
        ///
        /// The two 1U standards are mutually incompatible â a case railed for
        /// one will not take the other â so this is a property of the case the
        /// module goes into, not a preference.
        #[arg(long)]
        format: Option<String>,
        /// Output TOML path (default: <circuit>_panel.toml next to the circuit).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Ignore the built board and lay the controls out from scratch.
        ///
        /// The default reads `out/<name>/<name>.kicad_pcb` when it exists and
        /// puts each cutout where that part actually sits, because the board is
        /// what gets manufactured. Use this for a circuit with no board yet, or
        /// to start a fresh arrangement the board will then follow.
        #[arg(long)]
        idealised: bool,
        /// Overwrite the output spec if it already exists.
        ///
        /// The default output path is the circuit's own `<name>_panel.toml`, which
        /// for anything already built is the tracked, hand-authored spec. Deriving
        /// over it silently is how `legion-of-bom-byh` lost a hand-built layout.
        #[arg(long)]
        force: bool,
    },
    /// Compute the minimum Eurorack HP that fits a circuit â the PCB drives the
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
        Command::Init { dry_run } => init_cmd(dry_run),
        Command::Parts { action } => parts_cmd(action),
        Command::Catalog { action } => catalog_cmd(action),
        Command::Bom {
            circuit,
            price,
            out,
            visual,
            smd,
        } => bom_cmd(circuit, price, out, visual, smd),
        Command::Board {
            circuit,
            out,
            panel,
            mode,
            iterations,
            logo,
            model: model_glb,
        } => board_cmd(circuit, out, panel, mode, iterations, logo, model_glb),
        Command::Diagram { circuit, svg, pdf } => diagram_cmd(circuit, svg, pdf),
        Command::ScopeProbe {
            circuit,
            input_net_hint,
            freq_hz,
            min_amplitude,
            max_amplitude,
            steps,
            out,
        } => scope_probe_cmd(
            circuit,
            input_net_hint,
            freq_hz,
            min_amplitude,
            max_amplitude,
            steps,
            out,
        ),
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
            mode,
        } => guide_cmd(circuit, out, panel, kit, mode),
        Command::Import { action } => import_cmd(action),
        Command::Panel { action } => panel_cmd(action),
        Command::Spec {
            family,
            brief,
            out,
            trace,
        } => spec_cmd(family, brief, out, trace),
        Command::SpecChain {
            brief,
            vcc,
            enclosure,
            out,
            trace,
        } => spec_chain_cmd(brief, vcc, enclosure, out, trace),
        Command::Schematic { spec, out, panel } => schematic_cmd(spec, out, panel),
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
/// Stages: SKiDL run â parse â simulate (ngspice AC) â verify (textbook cutoff).
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

    // Stage: SKiDL â run the script, capture the netlist + ERC report.
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

    // Stage: validate â surface ERC results as structured findings.
    report.push(validate_erc(skidl_run.erc_report.as_deref()));

    // Stage: simulate â generate a SPICE deck and run an ngspice AC sweep. Infer
    // the I/O and supply nets from the circuit (SIG_IN/SIG_OUT, +12V/-12V) rather
    // than assuming IN/OUT/Â±15 V (tus.9).
    //
    // Not every circuit has an audio-style signal path to sweep â a Pierce
    // oscillator's resonator network (crate::oscillator) has no IN/OUT at
    // all, only OSC_IN/OSC_OUT net labels with no consuming circuit. That's
    // not a broken circuit, it's a genuinely different shape (verified
    // analytically, not by SPICE â see that module's docs), and used to
    // hard-fail the *entire* pipeline here the same way a missing footprint
    // used to hard-fail the entire board (see the `not_placed` fix) â fixed
    // the same way: skip what doesn't apply, don't refuse what does.
    let sim_config = SimConfig::infer(&model);
    let net_names: std::collections::HashSet<&str> =
        model.nets().iter().map(|n| n.name.as_str()).collect();
    let has_io = net_names.contains(sim_config.input_net.as_str())
        && net_names.contains(sim_config.output_net.as_str());
    // Input and output joined only through parts the circuit declares out of
    // simulation (an ADC, a processor, a DAC) is a converter board, not an
    // analog signal path: there is no passband gain to measure.
    let converter_only = has_io
        .then(|| {
            legion_of_bom_core::spice::analog_path(
                &model,
                &sim_config.input_net,
                &sim_config.output_net,
            )
            .err()
            .filter(|via| !via.is_empty())
        })
        .flatten();
    let has_signal_path = has_io && converter_only.is_none();

    let ac_result = if has_signal_path {
        let ac = simulate_ac(&model, &sim_config, &work_dir)
            .with_context(|| "simulate stage failed (try `lob doctor`)")?;
        report.push(StageOutcome::passed("simulate").with(Finding::info(format!(
            "AC sweep: {} points, passband {:.2} dB",
            ac.points.len(),
            ac.passband_gain_db().unwrap_or(0.0)
        ))));

        // Stage: transient â a step response, which shows time-domain behaviour (a
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

        // Stage: crosstalk â a multi-channel circuit has a question a single-channel
        // one does not: do the channels stay independent? Drive channel 1 with a step
        // sequence, hold every other channel's input at 0, and probe both outputs
        // (tus.13 / 9wh). Self-selecting: skipped entirely on a single-channel board.
        if let Some(outcome) = crosstalk_stage(&model, &sim_config, &work_dir) {
            report.push(outcome);
        }

        Some(ac)
    } else if let Some(via) = converter_only {
        report.push(StageOutcome::passed("simulate").with(Finding::info(format!(
            "'{}' reaches '{}' only through {} (declared Sim.Enable = 0) â a \
             converter path, not an analog one; AC/transient/crosstalk analysis \
             doesn't apply",
            sim_config.input_net,
            sim_config.output_net,
            via.join(", ")
        ))));
        None
    } else {
        report.push(StageOutcome::passed("simulate").with(Finding::info(format!(
            "no recognizable signal-path net ('{}'/'{}' not present â have: {}) â \
             this circuit has no audio-style I/O; AC/transient/crosstalk analysis \
             doesn't apply, see the circuit's own docs for how it's verified instead",
            sim_config.input_net,
            sim_config.output_net,
            {
                let mut names: Vec<&str> = net_names.iter().copied().collect();
                names.sort_unstable();
                names.join(", ")
            }
        ))));
        None
    };

    // Stage: verify â assert the simulated response against the textbook value
    // for this topology (RC cutoff, op-amp gain, â¦). Only meaningful when
    // there was an AC sweep to check.
    match &ac_result {
        Some(ac) => report.push(analytic_check(&model, ac, 0.02)),
        None => report.push(StageOutcome::passed("verify").with(Finding::info(
            "skipped â no AC sweep to check (see simulate stage)",
        ))),
    }

    // Stage: bom â group parts into a BOM, write CSV, summarize.
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

/// Concept -> spec: ask every typed decision for `family` once, and write the
/// spec -- raw text (`<out>.txt`) plus machine-readable JSON (`<out>.json`) --
/// with NO schematic. Today's curated set has one family ("fuzz-pedal"); an
/// unknown family fails loud rather than guessing at one.
fn spec_cmd(
    family: String,
    brief: String,
    out: PathBuf,
    trace_path: Option<PathBuf>,
) -> Result<()> {
    // Fail on an unknown family before asking for credentials: the typo is
    // the more useful error.
    if !family::FAMILIES.contains(&family.as_str()) {
        return Err(family::FamilyError::Unknown(family).into());
    }

    let client = ooda::CapturingClient::new(
        ooda::HttpClient::from_env().with_context(|| {
            "OODA_API_KEY not set (see .env.example) -- lob spec needs a Jev/System One-compatible endpoint"
        })?,
        ooda::Capture::for_current_binary().context("opening the decision capture log")?,
    );
    let mut trace = ooda::Trace::new();

    let spec = family::generate(&family, &client, &mut trace, &brief)
        .with_context(|| "spec generation failed")?;

    let json_path = with_extension_appended(&out, "json");
    let text_path = with_extension_appended(&out, "txt");

    let json = serde_json::to_string_pretty(&spec).with_context(|| "serializing spec")?;
    std::fs::write(&json_path, json).with_context(|| format!("writing {}", json_path.display()))?;

    let text = match &spec {
        family::Spec::FuzzPedal(s) => render_spec_text(&brief, s, trace.records()),
        family::Spec::Board(d) => {
            let mut t = format!("Brief: {brief}\n\nRequirements (typed decisions):\n");
            for (k, v) in &d.requirements {
                t.push_str(&format!("  {k:<14} {}\n", if *v { "yes" } else { "no" }));
            }
            t.push_str("\nParts (from the catalog):\n");
            for (slot, sel) in &d.parts {
                t.push_str(&format!(
                    "  {slot:<16} {:<28} {}\n",
                    sel.chosen.join(" + "),
                    sel.how
                ));
            }
            t.push_str("\nBindings:\n");
            for (bus, sel) in &d.bindings {
                t.push_str(&format!(
                    "  {bus:<16} {:<28} {}\n",
                    sel.chosen.join(" + "),
                    sel.how
                ));
            }
            t.push_str(&format!("\nCatalog: {}\n", d.catalog));
            t
        }
        other => anyhow::bail!("`lob spec` does not decide {} specs", other.family()),
    };
    std::fs::write(&text_path, text).with_context(|| format!("writing {}", text_path.display()))?;

    println!("lob spec {family}: {}", text_path.display());
    println!("  spec (machine-readable): {}", json_path.display());
    println!("  decisions made: {}", trace.records().len());
    for record in trace.records() {
        println!(
            "    {:<16} {:<8} {} (confidence {:.2})",
            record.key,
            format!("{:?}", record.kind).to_lowercase(),
            record.chosen,
            record.confidence
        );
    }

    if let Some(trace_path) = trace_path {
        let json =
            serde_json::to_string_pretty(&trace).with_context(|| "serializing decision trace")?;
        std::fs::write(&trace_path, json)
            .with_context(|| format!("writing {}", trace_path.display()))?;
        println!("wrote decision trace: {}", trace_path.display());
    }

    println!(
        "\nNext: `lob schematic {} --out <circuit>.py` to render the circuit \
         from exactly these decisions (no further System One calls).",
        json_path.display()
    );
    Ok(())
}

/// Concept -> spec, DAG variant. See [`Command::SpecChain`]'s docs.
fn spec_chain_cmd(
    brief: String,
    vcc: f64,
    enclosure: String,
    out: PathBuf,
    trace_path: Option<PathBuf>,
) -> Result<()> {
    let enclosure_size = parse_enclosure(&enclosure)?;
    let client = ooda::CapturingClient::new(
        ooda::HttpClient::from_env().with_context(|| {
            "OODA_API_KEY not set (see .env.example) -- lob spec-chain needs a Jev/System One-compatible endpoint"
        })?,
        ooda::Capture::for_current_binary().context("opening the decision capture log")?,
    );
    let mut trace = ooda::Trace::new();

    let constraints = FuzzConstraints {
        vcc,
        enclosure_size,
    };
    let chain = generate_fuzz_chain(&client, &mut trace, &brief, constraints)
        .with_context(|| "chain generation failed")?;

    let json_path = with_extension_appended(&out, "json");
    let text_path = with_extension_appended(&out, "txt");

    let json = serde_json::to_string_pretty(&family::Spec::FuzzChain(chain.clone()))
        .with_context(|| "serializing chain")?;
    std::fs::write(&json_path, json).with_context(|| format!("writing {}", json_path.display()))?;

    let mut text = String::new();
    text.push_str("FUZZ PEDAL SPEC (DAG chain)\n===========================\n\n");
    text.push_str(&format!("Brief: {brief}\n\n"));
    text.push_str(&format!(
        "Constraints (not decisions): vcc={vcc}V enclosure={}\n\n",
        enclosure_size.key()
    ));
    text.push_str("Decisions (System One / Jev/Laya-compatible, via ooda):\n");
    for record in trace.records() {
        text.push_str(&format!(
            "  - {:<20} [{}] {} (confidence {:.2})\n",
            record.key,
            format!("{:?}", record.kind).to_lowercase(),
            record.chosen,
            record.confidence
        ));
    }
    text.push_str(&format!(
        "\nResolved design: {} gain stage(s), ic={:.3}mA, tone_stack={}\n",
        chain.stages.len(),
        chain.ic_ma,
        chain.tone_stack
    ));
    text.push_str(
        "\nNext: `lob schematic <this>.json --out <circuit>.py` to render the circuit.\n",
    );
    std::fs::write(&text_path, &text)
        .with_context(|| format!("writing {}", text_path.display()))?;

    println!("lob spec-chain: {}", text_path.display());
    println!("  spec (machine-readable): {}", json_path.display());
    println!("  stages: {}", chain.stages.len());
    println!("  decisions made: {}", trace.records().len());
    for record in trace.records() {
        println!(
            "    {:<20} {:<8} {} (confidence {:.2})",
            record.key,
            format!("{:?}", record.kind).to_lowercase(),
            record.chosen,
            record.confidence
        );
    }

    if let Some(trace_path) = trace_path {
        let json =
            serde_json::to_string_pretty(&trace).with_context(|| "serializing decision trace")?;
        std::fs::write(&trace_path, json)
            .with_context(|| format!("writing {}", trace_path.display()))?;
        println!("wrote decision trace: {}", trace_path.display());
    }

    println!(
        "\nNext: `lob schematic {} --out <circuit>.py` to render the circuit \
         from exactly these decisions (no further System One calls).",
        json_path.display()
    );
    Ok(())
}

fn parse_enclosure(key: &str) -> Result<EnclosureSize> {
    match key.to_ascii_lowercase().as_str() {
        "1590b" => Ok(EnclosureSize::Size1590B),
        "1590bb" => Ok(EnclosureSize::Size1590BB),
        "125b" => Ok(EnclosureSize::Size125B),
        other => anyhow::bail!("unknown enclosure '{other}' (1590b | 1590bb | 125b)"),
    }
}

/// Spec -> design: read a spec JSON and render its SKiDL schematic. A pure
/// function of the spec file's contents -- makes no decision calls, so
/// running it twice on the same spec always produces the same circuit.
/// Dispatches on the spec's `family` tag ([`family::Spec`]), so every curated
/// family renders through this one command.
fn schematic_cmd(spec_path: PathBuf, out: PathBuf, panel: Option<PathBuf>) -> Result<()> {
    let json = std::fs::read_to_string(&spec_path)
        .with_context(|| format!("reading spec {}", spec_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .with_context(|| format!("parsing spec {}", spec_path.display()))?;

    let spec = family::Spec::from_json(value)
        .with_context(|| format!("parsing spec {}", spec_path.display()))?;
    std::fs::write(&out, spec.render_skidl()?)
        .with_context(|| format!("writing {}", out.display()))?;

    println!(
        "lob schematic {} ({}): {}",
        spec_path.display(),
        spec.family(),
        out.display()
    );

    let unconfirmed = spec.unconfirmed_facts();
    if !unconfirmed.is_empty() {
        println!(
            "  ⚠ {} fact(s) await human confirmation before this board is fab-ready:",
            unconfirmed.len()
        );
        for f in &unconfirmed {
            println!("      - {f}");
        }
    }

    // A family with no panel writes none: `lob board` then derives the
    // outline from the parts, and a stale panel file from an earlier run
    // must not be mistaken for this spec's.
    if let Some(panel_path) = panel {
        match spec.panel() {
            Some(panel_file) => {
                let toml = panel_file
                    .to_toml()
                    .with_context(|| "serializing panel spec")?;
                std::fs::write(&panel_path, toml)
                    .with_context(|| format!("writing {}", panel_path.display()))?;
                println!("  panel: {}", panel_path.display());
            }
            None => {
                if panel_path.exists() {
                    std::fs::remove_file(&panel_path).with_context(|| {
                        format!("removing stale panel {}", panel_path.display())
                    })?;
                }
                println!("  panel: none ({} boards have no panel)", spec.family());
            }
        }
    }

    println!(
        "\nNext: `lob run {out}` to validate + simulate; `lob parts gate {out}` \
         before real board/BOM generation (parts not verified_by_human yet).",
        out = out.display()
    );
    Ok(())
}

/// Append `.ext` after whatever extension (if any) `path` already has, so
/// `--out fuzz_v1` produces `fuzz_v1.json`/`fuzz_v1.txt` and `--out a.b`
/// produces `a.b.json`/`a.b.txt` -- never silently drops what the caller wrote.
fn with_extension_appended(path: &std::path::Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Dwell (s) each step of the crosstalk stimulus is held. Long enough for a slew
/// limiter at its mid-rate setting (~40 V/s) to finish a 4 V transition.
const CROSSTALK_DWELL_S: f64 = 0.15;

/// A step *sequence* â a sequencer feeding the module, not a single edge (tus.13).
/// Each level is held for [`CROSSTALK_DWELL_S`] and reached by a 0.1 ms edge, so
/// the stimulus asks for far more slew rate than the circuit can deliver and the
/// output is genuinely rate-limited rather than following the input.
fn step_sequence(levels: &[f64]) -> Vec<(f64, f64)> {
    let mut pwl = vec![(0.0, levels[0])];
    let mut t = CROSSTALK_DWELL_S / 4.0;
    for level in levels {
        pwl.push((t, *pwl.last().map(|(_, v)| v).unwrap()));
        pwl.push((t + 1e-4, *level));
        t += CROSSTALK_DWELL_S;
    }
    pwl.push((t, *levels.last().unwrap()));
    pwl
}

/// The crosstalk stage: on a multi-channel circuit, drive channel 1 and measure
/// how much of it reaches channel 2's output. `None` on a single-channel circuit,
/// which has no such question.
///
/// Two transient runs with an identical, deterministic stimulus â one probing
/// the driven output, one the undriven one â because a driven transient probes a
/// single net. Soft-failing on a simulation error: a circuit whose transient
/// won't converge is surfaced as a warning, matching the step-response stage.
fn crosstalk_stage(
    model: &dyn CircuitSource,
    sim_config: &SimConfig,
    work_dir: &Path,
) -> Option<StageOutcome> {
    let channels = legion_of_bom_core::signal_channels(model);
    if channels.len() < 2 {
        return None;
    }
    let pwl = step_sequence(&[0.0, 2.0, -2.0, 1.0, 0.0]);
    let stop_s = pwl.last().map(|(t, _)| *t).unwrap_or(1.0);
    // Every channel but the first is held at 0 V â nothing patched in, so any
    // movement on its output arrived from the channel that *is* being driven.
    let quiet: Vec<(String, Vec<(f64, f64)>)> = channels[1..]
        .iter()
        .map(|(input, _)| (input.clone(), vec![(0.0, 0.0), (stop_s, 0.0)]))
        .collect();

    let probe = |net: &str| {
        legion_of_bom_core::simulate_tran_drive(
            model,
            sim_config,
            &legion_of_bom_core::TranDrive {
                step_s: 2e-4,
                stop_s,
                pwl: pwl.clone(),
                cv: quiet.clone(),
                probe_net: Some(net.to_string()),
            },
            work_dir,
        )
    };

    let (aggressor, victim) = match (probe(&channels[0].1), probe(&channels[1].1)) {
        (Ok(a), Ok(v)) => (a, v),
        (Err(e), _) | (_, Err(e)) => {
            return Some(
                StageOutcome::passed("crosstalk").with(Finding::warning(format!(
                    "crosstalk run did not complete: {e}"
                ))),
            );
        }
    };
    // 1e-3 = â60 dB. A netlist with ideal supplies should be orders below that;
    // anything near it means the channels share a node.
    legion_of_bom_core::check_channel_crosstalk(model, &aggressor, &victim, 1e-3)
}

/// Handle `lob board <circuit> [--out]` â netlist â .kicad_pcb.
/// Build board options, using panel-anchored Eurorack placement when a panel
/// spec is given: jacks/pots are anchored to the panel's cutouts (Y flipped from
/// the panel's bottom-up frame to KiCad top-down) and the board outline becomes
/// the panel size (vertical 3U). Otherwise the default grid placement.
/// Shared panel geometry both the board outline and the placer need: panel size
/// (mm), the sheet origin that centres it on A4, and refdesâ(x,y) anchors (Y
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
    // Centre the board on KiCad's A4 sheet (297Ã210 landscape) rather than jamming
    // it in the (0,0) corner.
    let ox = ((297.0 - w) / 2.0).max(10.0);
    let oy = ((210.0 - h) / 2.0).max(10.0);
    Ok((w, h, (ox, oy), anchors))
}

/// Handle `lob board <circuit> [--out]` â netlist â .kicad_pcb.
/// Build board options, using panel-anchored Eurorack placement when a panel
/// The panel spec to build against: the one explicitly given (a `--panel` flag or
/// the manifest's `panel` field), otherwise a panel **auto-derived from the
/// circuit at the minimum HP the PCB fits in** (DESIGN 6.1 â the PCB drives the
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
        // A declared panel is the author's file and is never written to.
        //
        // This used to regenerate the declared spec in place. The intent was
        // sound â a panel missing a cutout cannot mate the board, so faithfully
        // rebuilding a wrong panel is worse than useless â but the cost was
        // destroying hand-authored work: comments, a deliberate two-column
        // layout, a chosen HP. A command that reads like a read must not rewrite
        // tracked source (`legion-of-bom-byh`).
        let Some(stale) = declared_panel_staleness(&path, model, footprint_dir)? else {
            return Ok(Some(path)); // fresh â authoritative, use as-is
        };
        // Derive the substitute anyway. It is no longer what we build; it is the
        // worked example the error points at, so "adopt the derived panel" is a
        // file on disk rather than an instruction to go and make one.
        let substitute = write_auto_panel(model, footprint_dir, work_dir, stem)?;
        return Err(panel_mismatch(&path, &stale, substitute.as_deref()));
    }
    let path = write_auto_panel(model, footprint_dir, work_dir, stem)?;
    Ok(path)
}

/// Why a declared panel spec cannot be built against.
enum PanelStaleness {
    /// Controls the circuit has that the panel declares no cutout for.
    Missing(Vec<String>),
    /// Declared narrower than the PCB the circuit lays out into.
    TooNarrow { declared: u16, needed: u16 },
}

impl std::fmt::Display for PanelStaleness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(refs) => write!(f, "no cutout for {}", refs.join(", ")),
            Self::TooNarrow { declared, needed } => {
                write!(f, "{declared} HP, but the PCB needs {needed} HP")
            }
        }
    }
}

/// The build refuses when the declared panel and the PCB disagree.
///
/// This used to warn and build against a derived substitute. That is the one
/// outcome this tool must never produce quietly: panel and PCB are ordered from
/// different vendors weeks apart, so a board built to a width its panel does not
/// have is money spent on two parts that cannot be assembled. `legion-of-bom-unc`
/// is the whole story â an explicit `hp = 4` was accepted, refused, and replaced
/// with 6 behind a warning; the mismatch surfaced as "why is the panel/PCB
/// mismatched?" only after both had been rendered and looked at.
///
/// Pure so the wording is testable without a KiCad footprint library.
fn panel_mismatch(path: &Path, stale: &PanelStaleness, substitute: Option<&Path>) -> anyhow::Error {
    let file = path.display();
    let mut lines = vec![
        format!("declared panel {file} does not fit the PCB: {stale}"),
        "  panel and board are manufactured separately, so a board that does not mate".into(),
        "  its panel is not a warning â it is two parts that cannot be assembled.".into(),
        "  fix one of:".into(),
    ];
    match stale {
        PanelStaleness::TooNarrow { needed, .. } => {
            lines.push(format!(
                "    â¢ set hp = {needed} in that file, then re-check the cutout x positions"
            ));
            lines.push("    â¢ shrink the circuit until it fits the width you declared".into());
        }
        PanelStaleness::Missing(refs) => {
            lines.push(format!(
                "    â¢ add a cutout for {} to that file",
                refs.join(", ")
            ));
            lines.push("    â¢ drop those controls from the circuit".into());
        }
    }
    if let Some(sub) = substitute {
        lines.push(
            "    â¢ adopt the derived panel: drop `panel = â¦` from lob.toml, or pass".into(),
        );
        lines.push(format!("      --panel {}", sub.display()));
    }
    lines.push(format!("  ({file} was left untouched.)"));
    anyhow::anyhow!(lines.join("\n"))
}

/// Derive a panel from the circuit and write it to `<work_dir>/<stem>_auto_panel.toml`.
///
/// The **only** place this tool writes a panel spec. Always a generated path in
/// the work dir, never a path the author declared.
fn write_auto_panel(
    model: &legion_of_bom_core::Circuit,
    footprint_dir: &Path,
    work_dir: &Path,
    stem: &str,
) -> Result<Option<PathBuf>> {
    let facts = build_facts(model, footprint_dir)?;
    let hp = minimum_hp(model, &facts);
    let panel = derive_panel(model, hp, &BuiltinCutouts);
    if panel.cutouts.is_empty() {
        return Ok(None); // no controls â build a plain board, no panel
    }
    std::fs::create_dir_all(work_dir)?;
    let path = work_dir.join(format!("{stem}_auto_panel.toml"));
    let toml = panel
        .to_toml()
        .map_err(|e| anyhow::anyhow!("serialising panel: {e}"))?;
    std::fs::write(&path, toml).with_context(|| format!("writing {}", path.display()))?;
    println!(
        "  auto panel: {hp} HP, {} control(s) â {}",
        panel.cutouts.len(),
        path.display()
    );
    Ok(Some(path))
}

/// Why a declared panel is stale, or `None` if it is fine. **Read-only.**
///
/// Stale two ways: missing a control the circuit now has (nothing for the board
/// to mate), or declared narrower than the PCB fits in (parts will not lay out).
/// An unreadable or unparseable spec is treated as fine and left entirely alone â
/// guessing at a file we cannot read is how you destroy one.
fn declared_panel_staleness(
    path: &Path,
    model: &legion_of_bom_core::Circuit,
    footprint_dir: &Path,
) -> Result<Option<PanelStaleness>> {
    let Some(declared) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| PanelFile::from_toml(&t).ok())
    else {
        return Ok(None);
    };
    let facts = build_facts(model, footprint_dir)?;
    let min_hp = minimum_hp(model, &facts);
    let expected = derive_panel(model, declared.hp.unwrap_or(min_hp), &BuiltinCutouts);
    let have: std::collections::HashSet<&str> = declared
        .cutouts
        .iter()
        .filter_map(|c| c.refdes.as_deref())
        .collect();
    let missing: Vec<String> = expected
        .cutouts
        .iter()
        .filter_map(|c| c.refdes.as_deref())
        .filter(|r| !have.contains(r))
        .map(String::from)
        .collect();
    if !missing.is_empty() {
        return Ok(Some(PanelStaleness::Missing(missing)));
    }
    if let Some(declared_hp) = declared.hp.filter(|h| *h < min_hp) {
        return Ok(Some(PanelStaleness::TooNarrow {
            declared: declared_hp,
            needed: min_hp,
        }));
    }
    Ok(None)
}

/// spec is given: jacks/pots are anchored to the panel's cutouts and the board
/// outline becomes the panel size (vertical 3U). Otherwise the default grid
/// placement. The placer here is the one-shot [`EurorackPlacer`]; the iterative
/// loop swaps in a [`SeededPlacer`] per attempt.
/// Board options, anchoring panel controls to a hand-authored placement when one
/// exists and to the panel spec otherwise.
///
/// A `<circuit>.placement.toml` wins over the panel's cutouts. Both produce the
/// same thing â a refdesâpoint map in the board's frame â but only one of them
/// is a decision somebody made: the panel spec's positions come from
/// `derive_panel`'s idealised column, and letting that override a layout you
/// authored by hand would silently undo it.
fn board_options_with_panel_and_placement(
    footprint_dir: PathBuf,
    panel: &Option<PathBuf>,
    placement: Option<&Path>,
) -> Result<BoardOptions> {
    let mut opts = BoardOptions::new(footprint_dir);
    let Some(spec_path) = panel else {
        return Ok(opts);
    };
    let (w, h, origin, mut anchors) = panel_geometry(spec_path)?;
    if let Some(path) = placement.filter(|p| p.is_file()) {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let file = PlacementFile::from_toml(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        let hand = file
            .anchors(h)
            .with_context(|| format!("expanding {}", path.display()))?;
        println!(
            "  placement: {} hand-placed control(s) from {}",
            hand.len(),
            path.display()
        );
        anchors.extend(hand);
    }
    opts.placer = Box::new(EurorackPlacer {
        width_mm: w,
        height_mm: h,
        origin_mm: origin,
        anchors,
    });
    opts.fixed_outline = Some((origin.0, origin.1, origin.0 + w, origin.1 + h));
    Ok(opts)
}

/// The hand-placement file that goes with a circuit, if the author wrote one:
/// `<circuit-dir>/<stem>.placement.toml`.
fn placement_path(circuit: &Path, stem: &str) -> PathBuf {
    circuit
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{stem}.placement.toml"))
}

/// Handle `lob init` â make a repo track inputs and ignore outputs.
///
/// Two jobs, both idempotent. It merges lob's generated-output patterns into
/// `.gitignore`, leaving anything the author wrote untouched; and it reports
/// files that are *already tracked* but generated, because .gitignore does not
/// retroactively untrack anything and those are the ones churning the diff.
///
/// It deliberately does NOT run `git rm --cached` itself: untracking is a change
/// to somebody's index and belongs to them. It prints the exact command.
fn init_cmd(dry_run: bool) -> Result<()> {
    let path = PathBuf::from(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let merged = legion_of_bom_core::merge_gitignore(&existing);
    let changed = merged != existing;

    if changed && !dry_run {
        std::fs::write(&path, &merged).with_context(|| format!("writing {}", path.display()))?;
    }
    println!(
        "{} {}",
        if !changed {
            "  .gitignore already current:"
        } else if dry_run {
            "  would update"
        } else {
            "  wrote"
        },
        path.display()
    );

    // Tracked-but-generated: the files .gitignore cannot help with.
    let tracked = std::process::Command::new("git")
        .args(["ls-files"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let stale: Vec<&str> = tracked
        .lines()
        .filter(|l| legion_of_bom_core::is_generated(l))
        .collect();
    if stale.is_empty() {
        println!("  no generated files are tracked â the repo tracks inputs only");
        return Ok(());
    }
    println!(
        "\n  {} tracked file(s) are generated by lob and should not be:",
        stale.len()
    );
    let mut shown = 0;
    for f in &stale {
        if shown < 12 {
            println!("    {f}");
            shown += 1;
        }
    }
    if stale.len() > shown {
        println!("    â¦ and {} more", stale.len() - shown);
    }
    println!("\n  Untracking is a change to your index, so lob will not do it for you:");
    println!("    git rm -r --cached <paths above> && git commit -m 'untrack generated outputs'");
    println!("  The files stay on disk; they simply stop being reviewed.");
    Ok(())
}

/// Resolve a `lob panel` argument to the spec that should actually be built.
///
/// An existing file is used as given â an ad-hoc spec has no circuit to check
/// against. Otherwise the argument is a circuit **name**: the manifest supplies
/// its declared panel, and the *cached* netlist (no SKiDL run, no network) says
/// whether that panel still matches the circuit. A stale one is reported and the
/// derived substitute is built instead, exactly as the board build does â so the
/// panel and the board can no longer disagree about how wide the module is.
fn resolve_panel_spec(arg: &Path) -> Result<PathBuf> {
    if arg.is_file() {
        return Ok(arg.to_path_buf());
    }
    let resolved = resolve_circuit(arg)?;
    let declared = resolved.panel.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "'{}' declares no panel â pass a spec file instead",
            resolved.name
        )
    })?;
    let stem = resolved.name.as_str();
    let work_dir = PathBuf::from("out").join(stem);
    // The cached netlist keeps this offline and instant; a panel is plotted far
    // more often than a circuit changes.
    let net = work_dir.join(format!("{stem}.net"));
    let Ok(model) = parse_netlist_file(&net) else {
        println!(
            "  â  no cached netlist for '{stem}' â plotting the declared panel unchecked\n    run `lob build {stem}` first to have it verified against the circuit"
        );
        return Ok(declared);
    };
    let footprint_dir = kicad_footprint_dir()
        .context("no KiCad footprint library found (set KICAD9_FOOTPRINT_DIR)")?;
    match effective_panel(
        Some(declared.clone()),
        &model,
        &footprint_dir,
        &work_dir,
        stem,
    )? {
        Some(p) => Ok(p),
        None => Ok(declared),
    }
}

/// Placement attempts every command gives a board. One number, because the whole
/// point is that `lob guide` and `lob fab` produce the *same* board.
const LAYOUT_ITERS: usize = 6;

/// Default `--mode` for every command that lays out a board. Shared so the three
/// cannot drift apart by each declaring their own default.
const LAYOUT_MODE: &str = "analog";

/// One layout, and everything a caller needs to report on it.
struct Layout {
    board: String,
    /// Connections the router could not make.
    conflicts: Vec<String>,
    /// Mechanical clearance problems (DESIGN 6.7). Empty on the one-shot path.
    collisions: Vec<String>,
    /// Parts with no footprint at all â off-board hardware, absent from the board.
    not_placed: Vec<String>,
}

/// Generate a board the way every command **must**: the iterative layout loop when
/// there is a panel to anchor to, one-shot placement when there is not.
///
/// Exists so no command can quietly emit a *different* board from the one the fab
/// package contains. If you are about to call `generate_board_report` or
/// `generate_board_artifacts` directly, you want this instead.
///
/// It did not do that job. `legion-of-bom-p6m`: for months this had exactly ONE
/// caller â `guide_cmd` â while `board_cmd` and `fab_cmd` each inlined their own
/// copy of the match, and this copy hardcoded `LayoutLoop::default()` so it
/// ignored `--mode` entirely. `lob fab --mode digital` and `lob guide` therefore
/// laid out two different boards, and guide's is the one written to
/// `out/<n>/<n>.kicad_pcb` â what the dashboard renders and what `lob panel
/// derive` reads back. Taking the whole `LayoutLoop` is what stops that: there is
/// no longer a knob a caller can hold that this function ignores.
fn build_layout(
    model: &legion_of_bom_core::Circuit,
    mut options: BoardOptions,
    panel: &Option<PathBuf>,
    cfg: &LayoutLoop,
) -> Result<Layout> {
    let template = match seeded_template(panel)? {
        Some(template) => template,
        None => {
            let template = free_outline_template(model, &mut options)?;
            println!(
                "  outline: {:.0} x {:.0} mm (no panel â sized to the parts)",
                template.width_mm, template.height_mm
            );
            template
        }
    };
    match cfg.max_iters {
        n if n > 0 => {
            let report = run_layout_loop(model, options, template, cfg)?;
            println!(
                "  seeded layout ({}): {} attempt(s), signal HPWL {:.0}mm, critical {:.0}mm, {} via(s)",
                cfg.mode.as_str(),
                report.iterations,
                report.metrics.signal_hpwl_mm,
                report.metrics.critical_hpwl_mm,
                report.metrics.via_count,
            );
            Ok(Layout {
                board: report.board,
                conflicts: report.unresolved,
                collisions: report.collisions,
                not_placed: report.not_placed,
            })
        }
        _ => {
            let art = generate_board_artifacts(model, &options)?;
            Ok(Layout {
                board: art.pcb,
                conflicts: art.route.conflicts,
                collisions: art.collisions,
                not_placed: art.not_placed,
            })
        }
    }
}

/// The seeded-placer template for the iterative layout loop, when a panel is
/// given; `None` without one ([`free_outline_template`] sizes that board).
fn seeded_template(panel: &Option<PathBuf>) -> Result<Option<SeededPlacer>> {
    match panel {
        Some(spec_path) => {
            let (w, h, origin, anchors) = panel_geometry(spec_path)?;
            Ok(Some(SeededPlacer::new(w, h, origin, anchors)))
        }
        None => Ok(None),
    }
}

/// A human-readable board title from a file stem: `slew_limiter` â `Slew Limiter`.
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

/// The silkscreen legend for a circuit, read from the repo's manifest when one
/// is discoverable. A bare-path build outside a circuits repo simply gets no
/// legend rather than an error â the board is still a board.
fn legend_for(stem: &str) -> SilkLegend {
    let Ok(cwd) = std::env::current_dir() else {
        return SilkLegend::default();
    };
    let Some((_root, manifest)) = Manifest::discover(&cwd).ok() else {
        return SilkLegend::default();
    };
    let entry = manifest.circuit(stem);
    SilkLegend {
        brand: manifest.repo.brand.clone(),
        rev: entry.and_then(|c| c.rev.clone()),
        note: entry.and_then(|c| c.silk_note.clone()),
    }
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
    model_glb: Option<PathBuf>,
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
    // Default: the PCB drives the panel â auto-derive one at minimum HP.
    let panel = effective_panel(panel, &model, &footprint_dir, &work_dir, stem)?;
    let placement = placement_path(&circuit, stem);
    let mut options =
        board_options_with_panel_and_placement(footprint_dir.clone(), &panel, Some(&placement))?;
    options.title = Some(pretty_title(stem));
    // `lob board` takes a bare path, so there is no resolved manifest entry â
    // look one up by stem when this repo has a lob.toml, else print no legend.
    options.legend = legend_for(stem);
    options.logo = load_logo(&logo)?;
    let path = out.unwrap_or_else(|| work_dir.join(format!("{stem}.kicad_pcb")));

    // Iterative, connectivity-aware layout when a panel is given (needs anchors);
    // otherwise the one-shot placement path.
    let cfg = LayoutLoop {
        mode: parse_mode(&mode)?,
        max_iters: iterations,
        kicad_cli: None,
        drc_every_iter: false,
    };
    let Layout {
        board,
        conflicts,
        collisions,
        not_placed,
    } = build_layout(&model, options, &panel, &cfg)?;

    std::fs::write(&path, &board).with_context(|| format!("writing {}", path.display()))?;
    let tracks = board.matches("(segment").count();
    let vias = board.matches("(via").count();
    println!("wrote {}", path.display());
    println!("  placed + routed: {tracks} tracks, {vias} vias, outline + GND pour");
    if let Some(model_path) = model_glb {
        let parts = guide::parse_board(&board).map_err(|e| anyhow::anyhow!(e))?;
        let outline = guide::board_outline(&board)
            .ok_or_else(|| anyhow::anyhow!("board has no Edge.Cuts outline"))?;
        let glb = export_board_glb(stem, &parts, outline).context("exporting 3D model")?;
        std::fs::write(&model_path, &glb)
            .with_context(|| format!("writing {}", model_path.display()))?;
        println!(
            "  wrote {} ({} part(s), {} bytes) â named/colored, for a model viewer, not a render",
            model_path.display(),
            parts.len(),
            glb.len()
        );
    }
    if !conflicts.is_empty() {
        eprintln!(
            "  â  {} connection(s) left unrouted (for manual/iterative routing):",
            conflicts.len()
        );
        for c in &conflicts {
            eprintln!("      - {c}");
        }
    }
    if !collisions.is_empty() {
        eprintln!(
            "  â  {} mechanical clearance issue(s) under a stacked sub-board:",
            collisions.len()
        );
        for c in &collisions {
            eprintln!("      - {c}");
        }
    }
    if !not_placed.is_empty() {
        println!(
            "  not placed (no footprint â off-board hardware, see BOM for MPN): {}",
            not_placed.join(", ")
        );
    }
    println!("  validate: lob drc {}", path.display());
    println!("  export:   kicad-cli pcb export gerbers --check-zones (fills the pour) | export pos (CPL)");
    Ok(())
}

/// Handle `lob diagram <circuit> [--svg <path>] [--pdf <path>]` â a readable
/// schematic diagram (symbols + routed nets), straight from the circuit model,
/// no board/layout step involved. SVG is the fast, iterate-on-it format; PDF
/// is a one-shot render of that same diagram for sharing.
fn diagram_cmd(circuit: PathBuf, svg_out: Option<PathBuf>, pdf_out: Option<PathBuf>) -> Result<()> {
    if svg_out.is_none() && pdf_out.is_none() {
        anyhow::bail!("nothing to write -- pass --svg <path>, --pdf <path>, or both");
    }
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

    let svg = schematic_to_svg(&model);
    if let Some(path) = &svg_out {
        std::fs::write(path, &svg).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    if let Some(path) = &pdf_out {
        let pdf = svg_to_pdf_bytes(&svg).with_context(|| "rendering schematic PDF")?;
        std::fs::write(path, &pdf).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// `count` breakpoints of a `amplitude`-peak sine at `freq_hz`, sampled
/// `points_per_cycle` times per cycle over `cycles` â dense enough that
/// ngspice's PWL source reads as sinusoidal rather than faceted.
fn sine_pwl(freq_hz: f64, amplitude: f64, cycles: u32, points_per_cycle: u32) -> Vec<(f64, f64)> {
    let period = 1.0 / freq_hz;
    let n = cycles * points_per_cycle;
    (0..=n)
        .map(|i| {
            let t = period * f64::from(i) / f64::from(points_per_cycle);
            let v = amplitude * (2.0 * std::f64::consts::PI * freq_hz * t).sin();
            (t, v)
        })
        .collect()
}

/// Peak-to-peak of the last cycle only, so a settling transient at turn-on
/// isn't counted as part of the steady-state signal.
fn steady_state_pp(points: &[legion_of_bom_core::TranPoint], period_s: f64, stop_s: f64) -> f64 {
    let window_start = (stop_s - period_s).max(0.0);
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for p in points.iter().filter(|p| p.t_s >= window_start) {
        lo = lo.min(p.v);
        hi = hi.max(p.v);
    }
    hi - lo
}

/// Fraction of steady-state samples whose slope is under `flat_frac` of this
/// waveform's own peak slope â scale-invariant (unlike comparing raw
/// sample-to-sample deltas against peak-to-peak, which conflates "the
/// timestep is fine" with "the signal is flat"). A clean sine's derivative
/// is near-zero only for an instant at each peak; a clipped waveform's
/// derivative is pinned near zero for a real chunk of each half-cycle.
fn flat_top_fraction(
    points: &[legion_of_bom_core::TranPoint],
    period_s: f64,
    stop_s: f64,
    flat_frac: f64,
) -> f64 {
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
        return 1.0;
    }
    let threshold = peak_slope * flat_frac;
    let flat = slopes.iter().filter(|s| s.abs() < threshold).count();
    flat as f64 / slopes.len() as f64
}

/// Handle `lob scope-probe` â drive a sine sweep into a circuit's recognized
/// input net and measure gain compression / flat-topping. Built for
/// PCBBench's `Check::SpiceClips` (see legion-of-bom's own
/// `crates/core/examples/scope_probe.rs`, this is that mechanism promoted to
/// a real subcommand with a machine-readable report instead of a println).
fn scope_probe_cmd(
    circuit: PathBuf,
    input_net_hint: Option<String>,
    freq_hz: f64,
    min_amplitude: f64,
    max_amplitude: f64,
    steps: usize,
    out: PathBuf,
) -> Result<()> {
    if steps < 2 {
        anyhow::bail!("--steps must be at least 2 (need a min and a max point)");
    }
    if min_amplitude <= 0.0 || max_amplitude <= min_amplitude {
        anyhow::bail!("need 0 < --min-amplitude < --max-amplitude");
    }
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

    let mut config = SimConfig::infer(&model);
    if let Some(hint) = &input_net_hint {
        config.input_net = hint.clone();
    }

    let cycles = 4;
    let points_per_cycle = 80;
    let period = 1.0 / freq_hz;

    // Log-spaced amplitudes: clipping onset is a decades-wide phenomenon
    // (mV to V), a linear sweep would waste most of its points above onset.
    let log_min = min_amplitude.ln();
    let log_max = max_amplitude.ln();
    let amplitudes: Vec<f64> = (0..steps)
        .map(|i| (log_min + (log_max - log_min) * i as f64 / (steps - 1) as f64).exp())
        .collect();

    let mut points_json = Vec::new();
    let mut small_gain = None;
    let mut large: Option<(f64, f64)> = None; // (gain, flat_top) at the largest amplitude
    for &amp in &amplitudes {
        let pwl = sine_pwl(freq_hz, amp, cycles, points_per_cycle);
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
        // Fails loud (non-zero exit) on a genuine measurement failure -- a
        // missing input net, a SPICE deck ngspice can't run -- exactly what
        // "crashed / no recognizable input net = nonzero exit" means: this
        // command only ever reports a clean 0 for a measurement that
        // actually ran, never for one it gave up partway through.
        let result = simulate_tran_drive(&model, &config, &drive, &work_dir)
            .with_context(|| format!("scope-probe at {amp}V peak"))?;
        let in_pp = 2.0 * amp;
        let out_pp = steady_state_pp(&result.points, period, stop_s);
        let gain = out_pp / in_pp;
        let flat_top = flat_top_fraction(&result.points, period, stop_s, 0.1);
        if small_gain.is_none() {
            small_gain = Some(gain);
        }
        large = Some((gain, flat_top));
        points_json.push(serde_json::json!({
            "amplitude_v": amp,
            "gain": gain,
            "gain_db": 20.0 * gain.log10(),
            "flat_top": flat_top,
        }));
    }

    let (gain_at_max, flat_top_at_max) = large.expect("steps >= 2, loop ran");
    let gain_at_min = small_gain.expect("steps >= 2, loop ran");
    let compression_db = 20.0 * (gain_at_max / gain_at_min).log10();

    let report = serde_json::json!({
        "circuit": stem,
        "input_net": config.input_net,
        "output_net": config.output_net,
        "freq_hz": freq_hz,
        "flat_top_at_max": flat_top_at_max,
        "gain_at_max": gain_at_max,
        "gain_at_min": gain_at_min,
        "compression_db": compression_db,
        "points": points_json,
    });
    let json = serde_json::to_string_pretty(&report).with_context(|| "serializing report")?;
    std::fs::write(&out, &json).with_context(|| format!("writing {}", out.display()))?;

    println!(
        "scope-probe: {} ({} -> {}, {:.0}Hz): flat-top {:.1}% at {:.3}V peak, {:.2}dB compression vs {:.3}V peak",
        stem,
        config.input_net,
        config.output_net,
        freq_hz,
        flat_top_at_max * 100.0,
        max_amplitude,
        compression_db,
        min_amplitude,
    );
    println!("wrote {}", out.display());
    Ok(())
}

/// Handle `lob drc <board>` â run DRC and report violations (the layout loop's
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
        println!("  â [{}] {}", v.kind, v.description);
        for it in &v.items {
            println!("      - {}", it.description);
        }
    }
    // Non-silk warnings; silkscreen collisions get their own section below.
    for v in report.warnings().filter(|v| !v.is_silkscreen_collision()) {
        println!("  â  [{}] {}", v.kind, v.description);
    }
    // Silkscreen collisions (DESIGN 6.10): not electrical, but they garble the
    // refdes/polarity legend a hand-assembler reads â surface them explicitly.
    if silk > 0 {
        println!("  silkscreen ({silk}): refdes/marks over pads or overlapping â");
        for v in report.silkscreen_collisions() {
            let loc = v
                .items
                .iter()
                .find_map(|it| it.pos)
                .map(|p| format!(" @ ({:.1}, {:.1})", p.x, p.y))
                .unwrap_or_default();
            println!("      âª [{}] {}{}", v.kind, v.description, loc);
        }
    }
    if report.is_clean() {
        println!("  â no errors");
        Ok(())
    } else {
        anyhow::bail!("{} DRC error(s)", report.error_count());
    }
}

/// Handle `lob fab <circuit> [--out]` â generate a board, gate it on DRC, and
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
    // Default: the PCB drives the panel â auto-derive one at minimum HP.
    let panel = effective_panel(panel, &model, &footprint_dir, &work_dir, stem)?;
    let placement = placement_path(&circuit, stem);
    let mut options =
        board_options_with_panel_and_placement(footprint_dir.clone(), &panel, Some(&placement))?;
    options.title = Some(pretty_title(stem));
    options.legend = SilkLegend {
        brand: resolved.brand.clone(),
        rev: resolved.rev.clone(),
        note: resolved.silk_note.clone(),
    };
    options.logo = load_logo(&logo)?;
    let kicad = kicad_cli_path().context("kicad-cli not found (install KiCad or set PATH)")?;

    // Iterative, connectivity-aware layout when a panel is given; else one-shot.
    // The DRC gate below is the loop's final verification (Â§6.5), so the loop
    // scores in-process unless `--drc-every-iter` is set.
    let cfg = LayoutLoop {
        mode: parse_mode(&mode)?,
        max_iters: iterations,
        kicad_cli: drc_every_iter.then(|| kicad.clone()),
        drc_every_iter,
    };
    let Layout {
        board, conflicts, ..
    } = build_layout(&model, options, &panel, &cfg)?;

    let pkg = out.unwrap_or_else(|| work_dir.join("fab"));
    std::fs::create_dir_all(&pkg)?;
    let board_path = pkg.join(format!("{stem}.kicad_pcb"));
    std::fs::write(&board_path, &board)
        .with_context(|| format!("writing {}", board_path.display()))?;
    if !conflicts.is_empty() {
        eprintln!("  â  {} connection(s) left unrouted:", conflicts.len());
        for c in &conflicts {
            eprintln!("      - {c}");
        }
    }

    // Fab capability, beside the board: kicad-cli reads <board>.kicad_dru from
    // the board's own directory, so writing it here is what makes the DRC gate
    // below judge against what JLCPCB can make rather than KiCad's defaults.
    let dru_path = pkg.join(format!("{stem}.kicad_dru"));
    std::fs::write(&dru_path, jlcpcb_design_rules())
        .with_context(|| format!("writing {}", dru_path.display()))?;

    // Physical-rule gate, ahead of DRC because KiCad cannot do this one.
    //
    // KiCad has no "footprint outside the board outline" rule. Measured: a part
    // moved 15.8mm clear of the edge produces ZERO geometric DRC violations â
    // the only errors are the unconnected nets it drags with it, and a part
    // with no connections (a mounting hole, an unpopulated position) drags
    // none. So a board with a component floating in space can be DRC-clean, and
    // this gate used to ship it: gerbers plotted, CPL written, part placed at a
    // coordinate off the board.
    //
    // `crate::rules` Tier::Physical does catch it, exactly and with the
    // magnitude â it is what the placer's own overflow lane is measured against.
    // It just was not consulted here. It is now.
    if let Ok(facts) = build_facts(&model, &footprint_dir) {
        let derived = rules::derive_in(
            &model,
            &rules::Context {
                facts: Some(&facts),
                outline: guide::board_outline(&board),
            },
        );
        let placed = guide::placements_from_board(&board)
            .map_err(|e| anyhow::anyhow!("reading placements back from the board: {e}"))?;
        let broken = rules::evaluate(&derived, &placed);
        let physical: Vec<_> = broken
            .iter()
            .filter(|v| v.tier == rules::Tier::Physical)
            .collect();
        if !physical.is_empty() {
            for v in &physical {
                eprintln!("  â [physical] {}", v.what);
            }
            anyhow::bail!(
                "board breaks {} physical rule(s) KiCad DRC does not check â refusing to build a fab package",
                physical.len()
            );
        }
    }

    // DRC gate â do not ship a package for a board with errors.
    let report = run_drc(&board_path, &kicad)?;
    println!(
        "DRC: {} error(s), {} warning(s)",
        report.error_count(),
        report.warning_count()
    );
    if !report.is_clean() {
        for v in report.errors() {
            eprintln!("  â [{}] {}", v.kind, v.description);
        }
        anyhow::bail!(
            "board has {} DRC error(s) â refusing to build a fab package",
            report.error_count()
        );
    }

    // Manufacturing outputs.
    let gerber_dir = pkg.join("gerbers");
    export_gerbers(&board_path, &gerber_dir, &kicad)?;
    let zip_path = pkg.join(format!("{stem}-gerbers.zip"));
    let zipped = zip_dir(&gerber_dir, &zip_path)?;
    // Which parts the fab will NOT place, read off the BOARD's real pads rather
    // than guessed from footprint names â a part is through-hole if it has a
    // through-hole pad, and that is a fact about the geometry, not the string.
    //
    // Computed BEFORE the CPL, because both halves of the upload have to be
    // filtered by the same set. It used to be computed after, so only the BOM
    // ever saw it (`legion-of-bom-g5a`).
    //
    // A parse failure here used to `unwrap_or_default()` into an EMPTY set,
    // which silently puts every through-hole part back into the assembly BOM â
    // the exact regression the kit-split exists to prevent, arriving quietly.
    // The board was just written and DRC'd, so failing to parse it is a real
    // fault and worth stopping for.
    let hand_soldered: std::collections::HashSet<String> = guide::parse_board(&board)
        .map(|parts| {
            parts
                .into_iter()
                .filter(|p| p.through_hole)
                .map(|p| p.refdes)
                .collect()
        })
        .map_err(|e| {
            anyhow::anyhow!("reading placements back from the board we just wrote: {e}")
        })?;
    let cpl_path = pkg.join(format!("{stem}-cpl.csv"));
    let placed = export_cpl(&board_path, &cpl_path, &kicad, &hand_soldered)?;
    let bom = generate_bom(&model);
    let bom_path = pkg.join(format!("{stem}-bom.csv"));
    if !hand_soldered.is_empty() {
        let mut hs: Vec<&String> = hand_soldered.iter().collect();
        hs.sort();
        println!(
            "  hand-soldered ({}, withheld from the assembly BOM): {}",
            hs.len(),
            hs.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")
        );
    }
    let assembly = jlc_assembly_bom(&bom, &hand_soldered);
    std::fs::write(&bom_path, &assembly.csv)
        .with_context(|| format!("writing {}", bom_path.display()))?;

    println!("fab package: {}", pkg.display());
    if zipped {
        println!("  PCB (upload this):   {}", zip_path.display());
    } else {
        println!(
            "  gerbers + drill:     {}/  (zip it â system `zip` unavailable)",
            gerber_dir.display()
        );
    }
    println!("  CPL ({placed} placements): {}", cpl_path.display());
    println!(
        "  BOM ({} line items): {}",
        assembly.lines,
        bom_path.display()
    );
    // The PCB half of this package is orderable on its own. The ASSEMBLY half is
    // not, unless every line the fab is asked to place carries a part number â
    // and saying "upload the CPL + BOM for assembly" over a BOM with none is how
    // a package that cannot be quoted looks finished (`legion-of-bom-g5a`).
    if assembly.unsourceable.is_empty() {
        println!(
            "  â JLCPCB: upload the gerber zip for the PCB, then the CPL + BOM for assembly"
        );
    } else {
        println!("  â JLCPCB: upload the gerber zip â the PCB is ready to order.");
        println!(
            "  â  NOT an assembly order yet: {} part(s) on the BOM have no LCSC number, so the\n    \
             fab cannot source them â {}",
            assembly.unsourceable.len(),
            assembly.unsourceable.join(" ")
        );
        println!(
            "    resolve them (`lob parts suggest {stem}` â `lob parts fetch` â `lob parts verify`)\n    \
             or match them by hand in JLCPCB's BOM step."
        );
    }
    Ok(())
}

/// A circuit input resolved from either a direct file path or a manifest circuit.
struct ResolvedCircuit {
    /// Circuit id â the manifest name, or the source file stem for a path arg.
    /// Drives the `out/<name>/` output tree.
    name: String,
    source: PathBuf,
    panel: Option<PathBuf>,
    kit: Option<String>,
    /// Whether the build guide steps through surface-mount parts (manifest
    /// `guide_smd`, circuit override first). A bare path argument gets the
    /// default, since there is no manifest to read it from.
    guide_smd: bool,
    build: Option<BuildCopy>,
    brand: Option<String>,
    /// Silkscreen legend: revision + design note, from the manifest.
    rev: Option<String>,
    silk_note: Option<String>,
}

/// Resolve a `lob <cmd> <arg>` circuit argument. An existing file is used
/// directly (flags supply panel/kit as before). Otherwise `arg` is treated as a
/// circuit **name** in the nearest `lob.toml` (walking up from the working dir),
/// pulling its source/panel/kit/build + the repo brand â so `lob guide
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
            guide_smd: false,
            build: None,
            brand: None,
            rev: None,
            silk_note: None,
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
            "'{}' is an imported circuit (fab package only) â it has no source to build from",
            entry.name
        )
    })?;
    Ok(ResolvedCircuit {
        name: entry.name.clone(),
        source,
        panel: entry.panel_path(&root),
        kit: entry.effective_kit(&manifest.defaults).map(str::to_string),
        guide_smd: entry.effective_guide_smd(&manifest.defaults),
        build: entry.build.clone(),
        brand: manifest.repo.brand.clone(),
        rev: entry.rev.clone(),
        silk_note: entry.silk_note.clone(),
    })
}

/// Handle `lob circuits` â list the circuits declared in the nearest `lob.toml`.
fn circuits_cmd() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let view = ProjectView::discover(&cwd)
        .with_context(|| "no lob.toml found (run inside a circuits repo)")?;
    let repo = view.repo.name.as_deref().unwrap_or("(unnamed)");
    let brand = view
        .repo
        .brand
        .as_deref()
        .map(|b| format!(" Â· {b}"))
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
            .map(|p| format!(" Â· panel {p}"))
            .unwrap_or_default();
        let copy = if c.has_build_copy {
            " Â· build-copy"
        } else {
            ""
        };
        // An imported circuit shows where its package came from instead.
        let origin = match (&c.source, &c.import) {
            (Some(src), _) => src.clone(),
            (None, Some(pkg)) => format!("{pkg}  [imported]"),
            _ => "â".to_string(),
        };
        println!("  {:<20} {origin}  ({kit}{panel}{copy})", c.name);
    }
    Ok(())
}

/// Handle `lob build [circuit]` â produce a circuit's full artifact set (guide +
/// Visual BOM + fab package), or every circuit in the repo. Each artifact is
/// independent, so one failing (e.g. a DRC-blocked fab) still leaves the others.
/// Build the artifacts an imported board *can* have.
///
/// There is no source to lay out or fabricate, but the fab package already
/// states what goes where, which is everything the guide and the Visual BOM
/// need. Writing them under `out/<name>/` is what puts an imported board on
/// the dashboard beside the ones we designed.
fn build_imported(
    root: &Path,
    name: &str,
    package: &Path,
    opts: GuideOptions,
) -> Result<Vec<&'static str>> {
    let board = legion_of_bom_core::read_package(package)
        .with_context(|| format!("reading {}", package.display()))?;
    let dir = root.join("out").join(name);
    std::fs::create_dir_all(&dir)?;

    // No photoreal render: an imported board has gerbers, not a KiCad board we
    // can ask kicad-cli to draw.
    let guide = board.to_guide_with(name, opts);
    let gpath = dir.join(format!("{name}-guide.html"));
    std::fs::write(&gpath, guide_to_html(&guide, None, None))
        .with_context(|| format!("writing {}", gpath.display()))?;

    let mut bom = board.to_bom();
    // An imported package usually names its parts in the comment column rather
    // than a dedicated one, so a Visual BOM built straight from it has no part
    // numbers and cannot be ordered from. Recover them the same way `lob import
    // repair` does, including from the parts we already build with.
    let lib = PartsLibrary::open(default_parts_dir()).ok();
    let filled = legion_of_bom_core::fill_mpns(&mut bom, lib.as_ref());
    if filled.from_comment + filled.from_library > 0 {
        println!(
            "  {name}: {} part number(s) from the comment, {} from parts we use, {} still unknown",
            filled.from_comment, filled.from_library, filled.unresolved
        );
    }
    let cache = default_image_cache_dir();
    // The sorting sheet is a builder's worklist: it carries the loose hardware
    // the netlist cannot know about, and drops the surface-mount parts the fab
    // already soldered.
    let bom = bom.without_smd().with_hardware();
    let thumbs: Vec<Option<String>> = bom.lines.iter().map(|l| resolve_photo(l, &cache)).collect();
    let vpath = dir.join(format!("{name}-vbom.html"));
    std::fs::write(&vpath, bom.to_visual_html(name, &thumbs))
        .with_context(|| format!("writing {}", vpath.display()))?;

    let cpath = dir.join(format!("{name}_bom.csv"));
    std::fs::write(&cpath, bom.to_csv()).with_context(|| format!("writing {}", cpath.display()))?;

    Ok(vec!["guide", "vbom", "bom"])
}

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

    let root = _root;
    let mut failures: Vec<String> = Vec::new();
    for name in &targets {
        println!("\nââââââââââ  build {name}  ââââââââââ");

        // An imported board has no source, but it does have a fab package â
        // enough for a guide and a Visual BOM, which is what a builder needs.
        if let Some(pkg) = manifest
            .circuit(name)
            .filter(|c| c.is_imported())
            .and_then(|c| c.import_path(&root))
        {
            let opts = GuideOptions {
                include_smd: manifest
                    .circuit(name)
                    .is_some_and(|c| c.effective_guide_smd(&manifest.defaults)),
            };
            match build_imported(&root, name, &pkg, opts) {
                Ok(done) => println!("â {name}: {} (imported)", done.join(" + ")),
                Err(e) => {
                    eprintln!("  â {e:#}");
                    failures.push(name.clone());
                }
            }
            continue;
        }

        let arg = || PathBuf::from(name);
        // ONE mode and ONE iteration count, passed to both. They agreed before
        // only because three separately-chosen defaults happened to match â
        // guide's hardcoded `LayoutLoop::default()`, fab's `"analog"` literal, and
        // `GUIDE_LAYOUT_ITERS` vs fab's `6`. Any one of them moving split the
        // guide's board from the one in the fab package (`legion-of-bom-p6m`).
        let steps: [(&str, Result<()>); 3] = [
            (
                "guide",
                guide_cmd(arg(), None, None, "auto".into(), LAYOUT_MODE.into()),
            ),
            ("bom", bom_cmd(arg(), false, None, true, false)),
            (
                "fab",
                fab_cmd(
                    arg(),
                    None,
                    None,
                    LAYOUT_MODE.into(),
                    LAYOUT_ITERS,
                    false,
                    None,
                ),
            ),
        ];
        let mut done = Vec::new();
        let mut circuit_ok = true;
        for (label, res) in steps {
            match res {
                Ok(()) => done.push(label),
                Err(e) => {
                    eprintln!("  â {label}: {e:#}");
                    circuit_ok = false;
                }
            }
        }
        if circuit_ok {
            println!("â {name}: {}", done.join(" + "));
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

/// Handle `lob status` â per-circuit build freshness, no network. An artifact is
/// "stale" when the source (or panel, or the manifest) changed after it was
/// written; "â" when it was never built. Reads the shared [`ProjectView`] model
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
                    Some(ArtifactStatus::Fresh) => "â",
                    Some(ArtifactStatus::Stale) => "stale",
                    _ => "â",
                };
                format!("{} {mark}", kind.label())
            })
            .collect();
        println!("  {:<18} {}", c.name, cols.join("   "));
    }
    Ok(())
}

/// Handle `lob serve [--bind]` â start the localhost dashboard backend over the
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

/// Handle `lob guide <circuit> [--out]` â generate a board and render a
/// step-by-step visual assembly guide (HTML).
fn guide_cmd(
    circuit: PathBuf,
    out: Option<PathBuf>,
    panel: Option<PathBuf>,
    kit: String,
    mode: String,
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
    // Default: the PCB drives the panel â auto-derive one at minimum HP.
    let panel = effective_panel(panel, &model, &footprint_dir, &work_dir, stem)?;
    let placement = placement_path(&circuit, stem);
    let options = board_options_with_panel_and_placement(footprint_dir, &panel, Some(&placement))?;
    // The SAME layout the fab package gets. This used to be a single one-shot
    // `generate_board_report` â no iteration, no scoring, no best-of â so the
    // guide's board was a *different, worse* board than the one that ships:
    // measured on the slew limiter, the guide's had an IC hanging 9mm off the
    // edge and a through-hole cap colliding with an IC through the board, while
    // the fab package was clean. Worse, out/<name>/<name>.kicad_pcb is what the
    // dashboard renders, so the picture everyone looks at was the bad one.
    // Two boards from one circuit is not a layout problem, it is a trust problem.
    let cfg = LayoutLoop {
        mode: parse_mode(&mode)?,
        max_iters: LAYOUT_ITERS,
        kicad_cli: None,
        drc_every_iter: false,
    };
    let board = build_layout(&model, options, &panel, &cfg)?.board;

    let guide_opts = GuideOptions {
        include_smd: resolved.guide_smd,
    };
    let mut guide = build_guide_with(&model, &board, guide_opts).map_err(|e| anyhow::anyhow!(e))?;
    if !guide_opts.include_smd {
        println!("  guide: through-hole parts only (set guide_smd = true to include SMD)");
    }
    if let Some(kit) = kit_override {
        guide.kit = kit;
    }
    println!("  kit: {:?} (assembly copy + framing)", guide.kit);

    // Per-part assembly notes from the parts library (best-effort; keyed by MPN
    // via resolve_circuit). Skips silently when the library is unavailable.
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
    // populates) when kicad-cli is available â top always, plus the bottom side
    // when any part mounts on the back; fall back to the schematic top-down.
    std::fs::create_dir_all(&work_dir)?;
    let board_file = work_dir.join(format!("{stem}.kicad_pcb"));
    std::fs::write(&board_file, &board)?;
    let kicad_cli = kicad_cli_path();
    let any_back = guide.steps.iter().any(|s| s.parts.iter().any(|p| p.back));
    let top = kicad_cli.as_ref().and_then(|k| {
        render_board_png(&board_file, k, Populate::SmdOnly, false, Quality::High).ok()
    });
    let bottom = if any_back {
        kicad_cli.as_ref().and_then(|k| {
            render_board_png(&board_file, k, Populate::SmdOnly, true, Quality::High).ok()
        })
    } else {
        None
    };
    match &top {
        Some((_, w, h)) => println!(
            "  diagram: photoreal bare-board render ({w}Ã{h}){}",
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
    // Embeds the same photoreal renders (PNGâJPEG for DCTDecode), else schematic.
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

/// Handle `lob bom <circuit> [--price] [--out] [--visual] [--smd]`.
fn bom_cmd(
    circuit: PathBuf,
    price: bool,
    out: Option<PathBuf>,
    visual: bool,
    smd: bool,
) -> Result<()> {
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
                                "  priced {mpn} â {} @ ${unit:.4} ({stock} in stock)",
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
        // per-MPN images â the durable source for boutique parts). Best-effort:
        // skip silently if the library isn't available.
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
        // The sorting sheet is a builder's worklist: it carries the loose
        // hardware the netlist cannot know about, and by default drops the
        // surface-mount parts the fab already soldered.
        let bom = if smd {
            bom.clone().with_hardware()
        } else {
            bom.clone().without_smd().with_hardware()
        };
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

/// The Visual BOM's photo for a line: the source [`photo_source`] chooses,
/// embedded as a `data:` URI with any crop applied. `None` â the Visual BOM
/// falls back to a life-size swatch / package silhouette / blank.
fn resolve_photo(line: &BomLine, cache: &Path) -> Option<String> {
    embed_source(&photo_source(line, cache)?, cache)
}

/// One CSV cell, quoted only when it has to be.
fn csv_cell(v: &str) -> String {
    if v.contains([',', '"', '\n']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

/// The repo a path sits in â the nearest ancestor holding `lob.toml` or `.git`.
fn find_repo_root(from: &std::path::Path) -> Option<PathBuf> {
    let mut dir = from.to_path_buf();
    loop {
        if dir.join("lob.toml").is_file() || dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Eagle schematics at a path: the file itself, or any directly inside a folder.
///
/// An empty result means "not an Eagle source", which is how `learn` decides
/// whether to read a fab package instead.
fn eagle_paths(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let is_sch = |p: &std::path::Path| {
        p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("sch"))
    };
    if path.is_file() {
        return if is_sch(path) {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let mut found: Vec<_> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_sch(p))
        .collect();
    found.sort();
    found
}

/// Handle `lob parts â¦` against the global parts library.
fn parts_cmd(action: PartsCmd) -> Result<()> {
    // `suggest` is about parts NOT yet in the library (generic, no MPN), so it
    // doesn't need the library open yet â handle it before opening it.
    if let PartsCmd::Suggest { circuit, limit } = action {
        return suggest_cmd(circuit, limit);
    }
    let lib =
        PartsLibrary::open(default_parts_dir()).with_context(|| "opening the parts library")?;
    match action {
        PartsCmd::Learn { packages, bom } => {
            // A BOM states what was bought, keyed by refdes â the half an Eagle
            // schematic never carries. Joining them recovers the part choice.
            let mut bought: std::collections::HashMap<String, (String, String)> =
                Default::default();
            if let Some(path) = &bom {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                for line in legion_of_bom_core::parse_imported_bom(&text) {
                    let Some(mpn) = line.part_number.filter(|m| !m.is_empty()).or_else(|| {
                        match plan_repair(&line.value, &line.footprint) {
                            Repair::UsePartNumber(m) => Some(m),
                            _ => None,
                        }
                    }) else {
                        continue;
                    };
                    let stated = value_key(&line.value, &line.footprint);
                    for refdes in line.refdes {
                        bought.insert(refdes.to_ascii_uppercase(), (mpn.clone(), stated.clone()));
                    }
                }
                println!("  {}: {} refdes named a part", path.display(), bought.len());
            }

            let (mut learned, mut skipped, mut wanted) = (0usize, 0usize, Vec::new());
            let mut mismatched = 0usize;
            for path in &packages {
                let source = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();

                // An Eagle schematic states what a board is built from but not
                // what was bought â Mutable's carry `value` and `device` and no
                // part number at all. Those still tell us which parts we need an
                // answer for, so they are recorded as demand, not as a choice.
                if eagle_paths(path).is_empty() {
                    let board = legion_of_bom_core::read_package(path)
                        .with_context(|| format!("reading {}", path.display()))?;
                    for part in &board.parts {
                        // A part-number column is authoritative. Only fall back
                        // to reading the comment when the board gives us no
                        // column â a comment like `100nf50V0603` merely *looks*
                        // like a part number and must not outrank the real one.
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
                    continue;
                }

                for sch in eagle_paths(path) {
                    let name = sch.file_stem().and_then(|s| s.to_str()).unwrap_or(&source);
                    let xml = std::fs::read_to_string(&sch)
                        .with_context(|| format!("reading {}", sch.display()))?;
                    let imp = legion_of_bom_core::eagle::parse_schematic(&xml, name);
                    let (mut here, mut gaps) = (0usize, 0usize);
                    for part in &imp.circuit.parts {
                        let package = part.footprint.clone().unwrap_or_default();
                        let kind = part_kind_of(&part.refdes.0, &package);
                        let value = value_key(&part.value, &package);
                        let pkg = package_key(&package);
                        let mut mpn = part.mpn.clone().filter(|m| !m.is_empty());
                        if mpn.is_none() {
                            if let Some((m, stated)) =
                                bought.get(&part.refdes.0.to_ascii_uppercase())
                            {
                                // A BOM and a schematic that disagree about what
                                // a refdes is cannot both be right, and guessing
                                // teaches the library a part number for the wrong
                                // part. Refuse the pairing and say so.
                                if !stated.is_empty() && !value.is_empty() && *stated != value {
                                    eprintln!(
                                        "  ! {} is {value} on the schematic but {stated} in the BOM \
                                         â not learning {m}",
                                        part.refdes.0
                                    );
                                    mismatched += 1;
                                    continue;
                                }
                                mpn = Some(m.clone());
                            }
                        }
                        match &mpn {
                            Some(m) if !m.is_empty() => {
                                lib.learn_house_part(kind, &value, &pkg, m, name)?;
                                learned += 1;
                                here += 1;
                            }
                            _ => {
                                skipped += 1;
                                if lib.house_part(kind, &value, &pkg)?.is_none() {
                                    gaps += 1;
                                    wanted.push((kind.to_string(), value, pkg, name.to_string()));
                                }
                            }
                        }
                    }
                    println!(
                        "  {name}: {} part(s), {here} named a part number, {gaps} we have no answer for",
                        imp.circuit.parts.len()
                    );
                }
            }
            println!(
                "learned {learned} part choice(s) from {} source(s); {skipped} line(s) named no part",
                packages.len()
            );
            if mismatched > 0 {
                println!(
                    "{mismatched} refdes disagreed between the BOM and the schematic â \
                     check the BOM is the one that built this revision"
                );
            }
            if !wanted.is_empty() {
                // Deduplicate: one line per distinct part we cannot answer for,
                // not one per instance, or a board of 40 resistors buries it.
                let mut seen = std::collections::BTreeMap::new();
                for (kind, value, pkg, board) in wanted {
                    seen.entry((kind, value, pkg)).or_insert(board);
                }
                println!(
                    "\n{} part(s) used but not in the library â these need sourcing:",
                    seen.len()
                );
                for ((kind, value, pkg), board) in seen.iter().take(20) {
                    let v = if value.is_empty() { "â" } else { value };
                    println!("  {kind:<11} {v:<10} {pkg:<20} (on {board})");
                }
                if seen.len() > 20 {
                    println!("  â¦ and {} more", seen.len() - 20);
                }
            }
        }

        PartsCmd::House { kind, csv } => {
            let all = lib.house_parts()?;
            let shown: Vec<_> = all
                .iter()
                .filter(|h| kind.as_deref().is_none_or(|k| h.kind == k))
                .collect();
            if csv {
                println!("kind,value,package,mpn,uses,seen_on,photo");
                for h in &shown {
                    println!(
                        "{},{},{},{},{},{},{}",
                        csv_cell(&h.kind),
                        csv_cell(&h.value),
                        csv_cell(&h.package),
                        csv_cell(&h.mpn),
                        h.uses,
                        csv_cell(&h.seen_on),
                        csv_cell(h.photo.as_deref().unwrap_or(""))
                    );
                }
                return Ok(());
            }
            if shown.is_empty() {
                println!("nothing learned yet â try `lob parts learn <package>...`");
                return Ok(());
            }
            for h in &shown {
                let val = if h.value.is_empty() {
                    "â"
                } else {
                    &h.value
                };
                let photo = if h.photo.is_some() { " ð·" } else { "" };
                println!(
                    "  {:<10} {:<10} {:<10} {:<22} x{}  ({}){photo}",
                    h.kind, val, h.package, h.mpn, h.uses, h.seen_on
                );
            }
            let with_photo = shown.iter().filter(|h| h.photo.is_some()).count();
            println!("{} part choice(s), {with_photo} with a photo", shown.len());
        }

        PartsCmd::Photo {
            kind,
            value,
            package,
            photo,
        } => {
            // `-` reads better than an empty argument for the parts that have
            // no value, which is how `lob parts house` displays them too.
            let value = if value == "-" { String::new() } else { value };
            // A path inside the repo is stored relative to it, so the library
            // and its photos survive a clone or a move. Anything else (a URL,
            // a path outside) is stored as written.
            let stored = match std::fs::canonicalize(&photo) {
                Ok(abs) => {
                    let repo = std::env::current_dir()
                        .ok()
                        .and_then(|d| find_repo_root(&d))
                        .unwrap_or_default();
                    abs.strip_prefix(&repo)
                        .map(|r| r.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| abs.to_string_lossy().into_owned())
                }
                Err(_) => photo.clone(),
            };
            if lib.set_house_photo(&kind, &value, &package, &stored)? {
                println!("photo set for {kind} {package}: {stored}");
            } else {
                let val = if value.is_empty() { "â" } else { &value };
                println!(
                    "no such part in the library: {kind} {val} {package}\n\
                     check `lob parts house --kind {kind}` for the exact key"
                );
            }
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
            println!("saved {mpn}");
        }
        PartsCmd::Verify { mpn, by } => {
            if lib.get_part(&mpn)?.is_none() {
                anyhow::bail!("no such part: {mpn}");
            }
            lib.mark_verified(&mpn, &by)?;
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
            println!("set image for {mpn}: {stored}");
        }
        PartsCmd::SetAssembly { mpn, notes } => {
            lib.set_assembly_steps(&mpn, &notes)?;
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
        // Handled before the library is opened (it doesn't need one).
        PartsCmd::Suggest { .. } => unreachable!("suggest is dispatched before lib open"),
        PartsCmd::Gate { circuit } => {
            let resolutions = resolve_circuit_file(&lib, circuit)?;
            let blockers: Vec<_> = resolutions
                .iter()
                .filter(|r| r.blocks_verified_use())
                .collect();
            if blockers.is_empty() {
                let n = resolutions.iter().filter(|r| r.mpn.is_some()).count();
                println!("â verification gate passed â {n} MPN-bearing part(s), all verified");
            } else {
                for b in &blockers {
                    let why = match b.status {
                        ResolutionStatus::Unknown => "not in library",
                        ResolutionStatus::Unverified => "in library, unverified",
                        _ => "",
                    };
                    println!(
                        "  â {:<6} {:<18} {why}",
                        b.refdes,
                        b.mpn.as_deref().unwrap_or("-")
                    );
                }
                anyhow::bail!(
                    "verification gate FAILED: {} part(s) not verified â layout / BOM ordering refuse to run",
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
            println!("fetched {mpn} from {source}:");
            print_part(&part);
            println!("\n(unverified â run `lob parts verify {mpn}` after confirming)");
        }
    }
    Ok(())
}

/// Handle `lob panel ...` commands.
/// `lob import eagle` â read an Eagle design into a circuit, and write SKiDL.
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
                    "  {} schematic symbol(s) skipped (frames, GND/supply markers â not components)",
                    imp.symbols_skipped.len()
                );
            }
            if !imp.unmapped_footprints.is_empty() {
                println!(
                    "  â  {} package(s) have no KiCad footprint and are marked `eagle:` â \
                     resolve before laying this out:",
                    imp.unmapped_footprints.len()
                );
                for p in imp.unmapped_footprints.iter().take(12) {
                    println!("      {p}");
                }
                if imp.unmapped_footprints.len() > 12 {
                    println!("      â¦ and {} more", imp.unmapped_footprints.len() - 12);
                }
            }

            if let Some(brd) = &board {
                let btext = std::fs::read_to_string(brd)
                    .with_context(|| format!("reading {}", brd.display()))?;
                let places = legion_of_bom_core::parse_eagle_board(&btext);
                let back = places.iter().filter(|p| p.back).count();
                println!(
                    "  board: {} placement(s), {back} on the back â the existing layout, reusable as-is",
                    places.len()
                );
            }

            let out = skidl.unwrap_or_else(|| schematic.with_extension("py"));
            std::fs::write(&out, legion_of_bom_core::to_skidl(&imp))
                .with_context(|| format!("writing {}", out.display()))?;
            println!("  SKiDL: {}", out.display());
            println!(
                "  NOTE: symbol libraries are inferred from each reference designator â \
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
                println!("(no distributor key found â set MOUSER_API_KEY to search)");
            }

            // The parts we already buy are the best answer available, and they
            // cost nothing to consult â a jack or a pot we have shipped before
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
                        println!("  {refs:<24} â {what}");
                    }
                }
            }
            println!(
                "\n{have} already had a part number Â· {found} recovered from the comment Â· \
                 {known} known from parts we use Â· {todo} need a search Â· \
                 {skip} not sourceable from this BOM"
            );
            if found > 0 {
                println!(
                    "Recovered numbers come from what the author wrote, not from a \
                     distributor â confirm them before ordering."
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

            let bom = board.to_bom().without_smd().with_hardware();
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
                    "  NOTE: {}/{} BOM lines carry no part number â this package left the \
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
                "  panel: {:.2} mm Ã {:.2} mm, {} hole(s), {} cutout(s)",
                panel.width_mm(),
                panel.height_mm(),
                panel.mounting_holes().len(),
                panel.cutouts().len(),
            );
        }
        PanelCmd::Pcb { spec, out, logo } => {
            // A panel that does not match its board is a module that cannot be
            // assembled, and this command used to have no way of noticing: it
            // took a spec path and plotted it verbatim, while `lob fab` quietly
            // substituted a DERIVED panel whenever the declared one had gone
            // stale. On the slew limiter that shipped a 5 HP panel for a 6 HP
            // board (legion-of-bom-m1b). Same inputs, two answers, no complaint.
            //
            // So resolve the same way every other command does: a path is used
            // as given, a NAME goes through the manifest and gets the same
            // staleness check the board build applies.
            let spec = resolve_panel_spec(&spec)?;
            let toml = std::fs::read_to_string(&spec)
                .with_context(|| format!("reading {}", spec.display()))?;
            let file = PanelFile::from_toml(&toml)
                .with_context(|| format!("parsing {}", spec.display()))?;
            let panel = file
                .to_spec()
                .map_err(|e| anyhow::anyhow!("invalid panel spec: {e}"))?;
            let stem = spec.file_stem().and_then(|s| s.to_str()).unwrap_or("panel");
            // Drop a trailing "_panel"/"-panel" and prettify: "slew_limiter_panel"
            // â "Slew Limiter".
            let title = pretty_title(stem.trim_end_matches("_panel").trim_end_matches("-panel"));
            let logo = load_logo(&logo)?;
            let pcb = panel_to_kicad_pcb(panel.as_ref(), &title, logo.as_ref());
            let out_path = out.unwrap_or_else(|| spec.with_extension("kicad_pcb"));
            std::fs::write(&out_path, pcb)
                .with_context(|| format!("writing {}", out_path.display()))?;
            println!("wrote {}", out_path.display());
            println!(
                "  panel PCB: {:.2} mm Ã {:.2} mm ({} HP), {} hole(s), {} cutout(s)",
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
        PanelCmd::Derive {
            circuit,
            hp,
            format,
            out,
            idealised,
            force,
        } => {
            let format = match format.as_deref() {
                Some(f) => PanelFormat::parse(f).with_context(|| {
                    format!("unknown panel format '{f}' (eurorack | intellijel-1u | pulplogic-1u)")
                })?,
                None => PanelFormat::Eurorack3U,
            };
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
            // An explicit --hp asks to design at a width the built board may not
            // have. Remember it before the auto default shadows it.
            let asked_hp = hp;
            // Default: let the PCB drive the width (minimum HP it fits in).
            let hp = match hp {
                Some(h) => h,
                None => {
                    let footprint_dir = kicad_footprint_dir()
                        .context("no KiCad footprint library found (set KICAD9_FOOTPRINT_DIR)")?;
                    let facts = build_facts(&model, &footprint_dir)?;
                    // A tile lays its controls in a row, so the panel-side
                    // minimum can exceed what the PCB needs by a long way.
                    let min = minimum_hp(&model, &facts).max(min_panel_hp_for(
                        &model,
                        format,
                        &BuiltinCutouts,
                    ));
                    println!("auto width: minimum {min} HP ({})", format.as_str());
                    min
                }
            };
            // Cutout dims resolve through the CutoutSource seam; BuiltinCutouts is
            // the fallback until the parts library carries verified mechanical data.
            let requested_hp = hp;
            // The board is master. A panel derived from an idealised column
            // matches the board only when the board was placed FROM that panel;
            // for an imported board, or one whose layout moved, it is fiction
            // that will not fit the hardware soldered to it. So when a built
            // board exists, read the cutouts off where the parts actually are.
            let board_path = work_dir.join(format!("{stem}.kicad_pcb"));
            let built = (!idealised)
                .then(|| std::fs::read_to_string(&board_path).ok())
                .flatten()
                .and_then(|pcb| panel_from_board(&pcb, &model, &BuiltinCutouts).ok());
            // ...unless you asked for a width that board does not have. Its cutout
            // positions are then evidence about a DIFFERENT panel, and using them
            // is the circular step in `legion-of-bom-unc`: the controls sit at
            // x=15.24 because the board is 6 HP, 15.24 + 7 overflows 4 HP, therefore
            // "4 HP can't fit the control hardware" â the 6 HP board offered as
            // proof that 6 HP is needed. Worse, `panel_from_board` takes its HP from
            // the board outline, so the requested width was discarded and that
            // verdict printed without 4 HP ever being tried.
            let width_clash = built
                .as_ref()
                .and_then(|p| asked_hp.zip(p.hp))
                .filter(|(want, have)| want != have);
            if let Some((want, have)) = width_clash {
                println!(
                    "  built board is {have} HP but you asked for {want} â laying out from \
                     scratch; positions from a board of another width prove nothing here"
                );
            }
            let from_board = built.filter(|_| width_clash.is_none());
            let mut panel = match from_board {
                Some(p) => {
                    println!(
                        "  from the built board ({}) â {} cutout(s) at their real positions",
                        board_path.display(),
                        p.cutouts.len()
                    );
                    p
                }
                None => {
                    // Not when the board was deliberately set aside just above â
                    // "no built board" would be a plain untruth.
                    if !idealised && width_clash.is_none() {
                        println!(
                            "  no built board at {} â laying out from scratch; \
                             the board will follow this panel",
                            board_path.display()
                        );
                    }
                    derive_panel_for(&model, format, hp, &BuiltinCutouts)
                }
            };
            // A derived panel widens itself when too narrow for its own hardware,
            // so report what was emitted, not what was asked for.
            let hp = panel.hp.unwrap_or(hp);
            if hp > requested_hp {
                println!(
                    "  widened to {hp} HP â {requested_hp} HP can't fit the control hardware"
                );
            }
            let out_path =
                out.unwrap_or_else(|| circuit.with_file_name(format!("{stem}_panel.toml")));
            // The default output path IS the declared spec for any circuit that has
            // one â comments, the control order somebody chose, edited labels. A
            // derived panel keeps none of that. `effective_panel` was carefully
            // taught never to write to a declared spec (`legion-of-bom-byh`); this
            // command still could, and did.
            if out_path.exists() && !force {
                anyhow::bail!(
                    "{} already exists â refusing to overwrite it\n  \
                     a derived spec keeps none of what a person put there: comments, the\n  \
                     control order they chose, labels they edited.\n  \
                     â¢ --out <path>  write it elsewhere and diff the two\n  \
                     â¢ --force       overwrite this one anyway",
                    out_path.display()
                );
            }
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
            let floor = minimum_hp(&model, &facts);
            println!(
                "parts fit from: {floor} HP ({:.1} mm) for {stem}",
                f64::from(floor) * 5.08
            );
            println!(
                "  {} panel-facing control(s), {} part(s) total",
                derive_panel(&model, floor, &BuiltinCutouts).cutouts.len(),
                model.parts().len()
            );
            // Fitting is not building. Route each width for real and gate it on
            // KiCad DRC, because a board can fit and still be unroutable â the
            // failure that shipped a 3 HP answer for a board that needed more.
            let cfg = LayoutLoop {
                kicad_cli: kicad_cli_path(),
                max_iters: 2,
                ..LayoutLoop::default()
            };
            let search = HpSearch::default();
            if cfg.kicad_cli.is_some() {
                println!("  trialling widths (place â route â DRC), {floor} HP upâ¦");
            }
            let found = minimum_routable_hp(&model, floor, &search, &cfg, |hp| {
                eurorack_trial_build(&model, &footprint_dir, hp)
            });
            for t in &found.tried {
                match t.errors {
                    None => println!("    {:>2} HP Â· could not be built", t.hp),
                    Some(0) => println!("    {:>2} HP Â· DRC clean", t.hp),
                    Some(n) => {
                        let why: Vec<String> =
                            t.kinds.iter().map(|(k, c)| format!("{c}Ã {k}")).collect();
                        println!("    {:>2} HP Â· {n} DRC error(s): {}", t.hp, why.join(", "));
                    }
                }
            }
            match (found.unproven, found.hp) {
                (true, _) => println!(
                    "  routability unchecked (no kicad-cli) â {floor} HP is a floor, not a proven width"
                ),
                (_, Some(hp)) => println!(
                    "minimum BUILDABLE width: {hp} HP ({:.1} mm)",
                    f64::from(hp) * 5.08
                ),
                (_, None) => println!(
                    "  no width from {floor} to {} HP routed DRC-clean â this is a layout problem, not a width problem",
                    floor + search.max_widths - 1
                ),
            }
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

/// Handle `lob parts suggest <circuit> [--limit N]` â run SKiDL â netlist â model,
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
    // Tell the user what's active â never panic on a missing key.
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
            "every part already declares an MPN ({resolved_count} part(s)) â nothing to suggest"
        );
        return Ok(());
    }

    for ((value, footprint), (part, mut refdes)) in generics {
        refdes.sort();
        let fp = footprint.as_deref().unwrap_or("(no footprint)");
        println!("â {}  {value}  [{fp}]", refdes.join(", "));
        let query = legion_of_bom_core::build_query(&part);
        if let Some(q) = &query {
            println!("    query: \"{q}\"");
        }
        let candidates = suggest_mpns(&part, &clients, limit);
        if candidates.is_empty() {
            println!("    (no candidates â try a more specific value/footprint, or add by hand)");
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
                .map(|p| format!(" Â· {p}"))
                .unwrap_or_default();
            let lcsc = c
                .lcsc_code
                .as_deref()
                .map(|l| format!(" Â· {l}"))
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
        // The confirm path â never silent-assign (okm gate).
        if let Some(top) = candidates.first() {
            let fetch_hint = match top.lcsc_code.as_deref() {
                Some(code) => format!("lob parts fetch {code} --source jlcpcb"),
                None => format!("lob parts add {} --manufacturer '{}'", top.mpn, {
                    top.manufacturer.as_deref().unwrap_or("")
                }),
            };
            println!(
                "    â confirm: {fetch_hint}  then  lob parts verify {}",
                top.mpn
            );
        }
        println!();
    }

    println!(
        "{} generic part group(s) need an MPN; {resolved_count} part(s) already resolved.\n\
         Suggestions are NOT auto-assigned â confirm one, then `lob parts verify` (okm gate).",
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
        println!("no parts declare an MPN (generic/ideal parts) â nothing to resolve");
        return;
    }
    for r in &with_mpn {
        let (mark, label) = match r.status {
            ResolutionStatus::Verified => ("â", "verified"),
            ResolutionStatus::Unverified => ("â ", "in library, unverified"),
            ResolutionStatus::Unknown => ("â", "not in library"),
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
            if outcome.passed { "â" } else { "â" },
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
        println!("â pipeline passed ({} stages)", report.outcomes.len());
    } else {
        let failed = report.outcomes.iter().filter(|o| !o.passed).count();
        println!(
            "â pipeline failed ({failed} of {} stages)",
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
/// while the global file supplies the keys from *any* working directory â so
/// `lob bom --price` and `lob serve` find `MOUSER_API_KEY` when run from inside
/// a circuits repo, not only from this checkout.
fn load_credentials() {
    // Repo-local .env (dev override).
    let _ = dotenvy::dotenv();
    // User-global credentials â the durable home for API keys.
    if let Some(home) = std::env::var_os("HOME") {
        let global = Path::new(&home).join(".lob").join("credentials");
        let _ = dotenvy::from_path(&global);
    }
}

/// Handle `lob catalog`.
fn catalog_cmd(action: CatalogCmd) -> Result<()> {
    use legion_of_bom_core::catalog::{self, Catalog};
    let dir_or_default = |d: Option<PathBuf>| d.unwrap_or_else(catalog::default_catalog_dir);
    match action {
        CatalogCmd::List { dir } => {
            let cat = Catalog::load(&dir_or_default(dir))?;
            for p in &cat.parts {
                println!("{:<28} {}", p.mpn, p.provides.join(", "));
            }
        }
        CatalogCmd::Check { dir } => {
            let dir = dir_or_default(dir);
            let cat = Catalog::load(&dir)?;
            let symbols = kicad_symbol_dir()
                .context("no KiCad symbol library found (set KICAD9_SYMBOL_DIR)")?;
            let mut problems = catalog::check_symbols(&cat, symbols.path())?;
            problems.extend(catalog::check_quotes(
                &cat,
                &legion_of_bom_core::datasheet::default_cache_dir(),
            )?);
            let unconfirmed: usize = cat.parts.iter().map(|p| p.unconfirmed().len()).sum();
            for p in &problems {
                println!("  ✗ {p}");
            }
            println!(
                "{} part(s), {} quote(s) checked, {} reading(s) awaiting confirmation, {} problem(s)",
                cat.parts.len(),
                cat.parts.iter().map(|p| p.quotes().len()).sum::<usize>(),
                unconfirmed,
                problems.len()
            );
            if !problems.is_empty() {
                anyhow::bail!("catalog check failed");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A declared panel is the author's file. Checking whether it has gone stale
    /// must never write to it â the whole of `legion-of-bom-byh` was that check
    /// regenerating the spec in place, destroying comments, a chosen HP and a
    /// hand-built layout.
    #[test]
    fn checking_a_declared_panel_for_staleness_never_writes_to_it() {
        let dir = std::env::temp_dir().join(format!("lob-byh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hand_panel.toml");
        // A deliberately hand-shaped spec: a comment, and only one control
        // declared while the circuit below has two â i.e. stale.
        let original = "# HAND-AUTHORED â a comment a generator would drop.\n\
                        format = \"eurorack\"\n\
                        hp = 8\n\
                        thickness_mm = 1.6\n\n\
                        [[cutouts]]\n\
                        x_mm = 10.0\n\
                        y_mm = 100.0\n\
                        rotation_deg = 0.0\n\
                        footprint = \"Thonkiconn\"\n\
                        refdes = \"J1\"\n";
        std::fs::write(&path, original).unwrap();

        let mut circuit = legion_of_bom_core::Circuit::new("t");
        circuit.parts = vec![
            legion_of_bom_core::Part::new("J1", "in").with_footprint(
                "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical_CircularHoles",
            ),
            legion_of_bom_core::Part::new("RV1", "100k").with_footprint(
                "Potentiometer_THT:Potentiometer_Alpha_RD901F-40-00D_Single_Vertical",
            ),
        ];

        let Some(fp_dir) = kicad_footprint_dir() else {
            return; // no KiCad here; the write-freedom assertion below needs it
        };
        let reason = declared_panel_staleness(&path, &circuit, &fp_dir).unwrap();
        assert!(
            reason.is_some_and(|r| r.to_string().contains("RV1")),
            "the missing control is reported"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the author's file is byte-identical after the check"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `board`, `fab` and `guide` must lay out the same board by DEFAULT.
    ///
    /// `legion-of-bom-p6m`: they agreed only because three separately-chosen
    /// defaults happened to match â `guide` hardcoded `LayoutLoop::default()`
    /// (mode Analog), `fab` and `board` each spelled `"analog"` as a literal, and
    /// `GUIDE_LAYOUT_ITERS` was a different constant from fab's `6`. Any one of
    /// them moving split the guide's board from the one in the fab package, and
    /// the guide's is what the dashboard renders.
    ///
    /// The fix is that they now share `LAYOUT_MODE`/`LAYOUT_ITERS`; this is the
    /// assertion that keeps them sharing it.
    #[test]
    fn board_fab_and_guide_default_to_the_same_layout() {
        use clap::Parser;
        let mode_of = |argv: &[&str]| -> String {
            match Cli::parse_from(argv).command {
                Command::Board { mode, .. } => mode,
                Command::Fab { mode, .. } => mode,
                Command::Guide { mode, .. } => mode,
                _ => unreachable!("only the three layout commands belong here"),
            }
        };
        let board = mode_of(&["lob", "board", "c.py"]);
        let fab = mode_of(&["lob", "fab", "c.py"]);
        let guide = mode_of(&["lob", "guide", "c.py"]);
        assert_eq!(board, fab, "board and fab default to different modes");
        assert_eq!(fab, guide, "fab and guide default to different modes");
        // And the shared default has to be a mode that actually parses, or every
        // one of them fails identically at run time instead of here.
        assert!(
            parse_mode(&board).is_ok(),
            "the shared default mode {board:?} does not parse"
        );
    }

    /// The refusal MESSAGE carries both numbers and a way out.
    ///
    /// Pure formatter only â deliberately not the proof that anything refuses.
    /// See `a_panel_narrower_than_the_pcb_stops_the_build` for that; an earlier
    /// version of this file had only this test and called it the guard, which
    /// would have stayed green through the exact `legion-of-bom-unc` regression
    /// it was named after.
    #[test]
    fn the_refusal_says_both_widths_and_how_to_get_out() {
        let stale = PanelStaleness::TooNarrow {
            declared: 5,
            needed: 6,
        };
        let err = panel_mismatch(
            Path::new("circuits/slew/slew_panel.toml"),
            &stale,
            Some(Path::new("out/slew/slew_auto_panel.toml")),
        )
        .to_string();
        // Both numbers, because "does not fit" without them is unactionable.
        assert!(err.contains("5 HP"), "{err}");
        assert!(err.contains("needs 6 HP"), "{err}");
        // A way out that exists on disk, and the promise we keep about their file.
        assert!(err.contains("out/slew/slew_auto_panel.toml"), "{err}");
        assert!(err.contains("left untouched"), "{err}");
    }

    /// A panel narrower than the PCB is a REFUSAL, not a warning.
    ///
    /// `legion-of-bom-unc`: `hp = 5` was accepted, rejected and silently rebuilt
    /// at 6, and the two were rendered and ordered as if they matched. The panel
    /// and the board come from different vendors, so nothing downstream ever gets
    /// the chance to notice they disagree â this is the only place that can.
    ///
    /// So this drives the real thing: DETECTION (`declared_panel_staleness` must
    /// classify it `TooNarrow`) and REFUSAL (`effective_panel` must return `Err`).
    /// Asserting on `panel_mismatch`'s wording proves neither â a build that
    /// logged the error and carried on would satisfy it.
    #[test]
    fn a_panel_narrower_than_the_pcb_stops_the_build() {
        let Some(fp_dir) = kicad_footprint_dir() else {
            return; // needs real footprints to know how wide the PCB must be
        };
        let dir = std::env::temp_dir().join(format!("lob-unc-narrow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("narrow_panel.toml");
        // 1 HP is narrower than any real circuit's PCB, so this does not depend on
        // the packer's exact answer â only that it is more than one.
        std::fs::write(
            &path,
            "format = \"eurorack\"\nhp = 1\nthickness_mm = 1.6\n\n\
             [[cutouts]]\nx_mm = 2.5\ny_mm = 100.0\nrotation_deg = 0.0\n\
             footprint = \"Thonkiconn\"\nrefdes = \"J1\"\n",
        )
        .unwrap();

        let mut circuit = legion_of_bom_core::Circuit::new("t");
        circuit.parts = vec![
            legion_of_bom_core::Part::new("J1", "in").with_footprint(
                "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical_CircularHoles",
            ),
            legion_of_bom_core::Part::new("U1", "TL074")
                .with_footprint("Package_SO:SOIC-14_3.9x8.7mm_P1.27mm"),
        ];

        // DETECTION: narrower than the PCB, and classified as such â not as a
        // missing cutout, which takes a different branch and a different message.
        let stale = declared_panel_staleness(&path, &circuit, &fp_dir).unwrap();
        let Some(PanelStaleness::TooNarrow { declared, needed }) = stale else {
            panic!("expected TooNarrow, got {:?}", stale.map(|s| s.to_string()));
        };
        assert_eq!(declared, 1);
        assert!(needed > 1, "the PCB needs more than 1 HP, got {needed}");

        // REFUSAL: the build stops. This is the assertion `unc` was filed about â
        // the old code returned Ok(Some(derived_substitute)) here and built on.
        let err = effective_panel(Some(path.clone()), &circuit, &fp_dir, &dir, "t")
            .expect_err("a panel narrower than the PCB must stop the build");
        assert!(err.to_string().contains("does not fit the PCB"), "{err}");
        // And the author's file survived being refused.
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("hp = 1"),
            "the declared spec must not be rewritten"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The width check fires only when the declared panel is *narrower*. A wider
    /// one is a deliberate choice (rack symmetry, a blank strip) and must build.
    #[test]
    fn a_panel_wider_than_the_pcb_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("lob-unc-wide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide_panel.toml");
        // 42 HP for a single jack â absurd, and entirely the author's business.
        std::fs::write(
            &path,
            "format = \"eurorack\"\nhp = 42\nthickness_mm = 1.6\n\n\
             [[cutouts]]\nx_mm = 10.0\ny_mm = 100.0\nrotation_deg = 0.0\n\
             footprint = \"Thonkiconn\"\nrefdes = \"J1\"\n",
        )
        .unwrap();

        let mut circuit = legion_of_bom_core::Circuit::new("t");
        circuit.parts = vec![legion_of_bom_core::Part::new("J1", "in").with_footprint(
            "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical_CircularHoles",
        )];

        let Some(fp_dir) = kicad_footprint_dir() else {
            return; // no KiCad here
        };
        let reason = declared_panel_staleness(&path, &circuit, &fp_dir).unwrap();
        assert!(
            reason.is_none(),
            "a wider-than-needed panel is not stale, got {:?}",
            reason.map(|r| r.to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
