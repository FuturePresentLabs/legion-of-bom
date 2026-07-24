//! `legion-of-bom-core` — the pipeline library.
//!
//! Everything downstream of circuit definition (validation, simulation, layout,
//! BOM) is expressed here as composable [`Stage`]s that read a circuit through
//! the [`CircuitSource`] trait. Per DESIGN.md 2.3/3.3 the model is deliberately
//! DSL-agnostic: today the only producer is a parsed SKiDL-generated KiCad
//! netlist, but no stage may depend on that fact, so a future native DSL or an
//! extracted IR is one new [`CircuitSource`] impl rather than a rewrite.
//!
//! The CLI (`lob`) and the eventual web backend are thin wrappers over this
//! library — anything one surface can do, the other can too.

pub mod board;
pub mod bom;
pub mod drc;
pub mod fab;
pub mod fetch;
pub mod guide;
pub mod jlcpcb;
pub mod layout;
pub mod logo;
pub mod model;
pub mod mouser;
pub mod netlist;
pub mod panel;
pub mod parts;
pub mod pdf;
pub mod route;
mod sexpr;
pub mod skidl;
pub mod source;
pub mod spice;
pub mod stage;
pub mod subboard;
mod symbols;
pub mod tools;
pub mod units;
pub mod validate;
pub mod verify;

pub use board::{
    generate_board, generate_board_artifacts, generate_board_report, BoardArtifacts, BoardError,
    BoardOptions, EurorackPlacer, GridPlacer, PartFacts, Placement, Placer, SeededPlacer,
};
pub use bom::{generate_bom, Bom, BomLine};
pub use drc::{run_drc, DrcItem, DrcReport, DrcViolation};
pub use fab::{
    export_board_svg, export_cpl, export_gerbers, jlc_bom_csv, jlc_cpl_from_kicad_pos, png_to_jpeg,
    render_board_jpeg, render_board_png, zip_dir,
};
pub use fetch::{fetch_from_jlcpcb, fetch_from_kicad};
pub use guide::{
    build_guide, guide_to_html, guide_to_pdf, BoardPng, BuildGuide, BuildStep, PlacedPart,
};
pub use jlcpcb::{JlcpcbClient, JlcpcbComponent, JlcpcbError};
pub use layout::{
    measure, run_layout_loop, score, CostWeights, LayoutLoop, LayoutMode, LayoutReport,
    PlacementMetrics,
};
pub use logo::Logo;
pub use model::{Circuit, Net, Part, PinRef, RefDes, Side, SimModel};
pub use mouser::{MouserClient, MouserError, PartPrice, PriceBreak};
pub use netlist::{parse_netlist_file, parse_netlist_str};
pub use panel::{
    default_panel_orders_dir, derive_panel, footprint_shape, panel_to_dxf, panel_to_kicad_pcb,
    BuiltinCutouts, ControlKind, Cutout, CutoutShape, CutoutSource, CutoutSpec, EurorackPanel,
    MountingHole, PanelFile, PanelOrder, PanelOrderStatus, PanelOrders, PanelSpec,
};
pub use parts::{
    default_parts_dir, PartRecord, PartResolution, PartsError, PartsLibrary, PinRecord,
    RatingRecord, ResolutionStatus,
};
pub use route::{
    GridRouter, MstRouter, PadLayer, PadPoint, RouteNet, RouteOptions, RouteOutput, Router, Track,
    Via,
};
pub use skidl::{SkidlRun, SkidlRunner};
pub use source::CircuitSource;
pub use spice::{
    simulate_ac, simulate_tran, AcPoint, AcResult, SimConfig, TranAnalysis, TranPoint, TranResult,
};
pub use stage::{Finding, PipelineReport, Severity, Stage, StageError, StageOutcome};
pub use tools::{find_on_path, kicad_cli_path, phase0_tools, Tool, ToolStatus};
pub use units::parse_eng_value;
pub use validate::validate_erc;
pub use verify::{analytic_check, check_noninverting_gain, check_rc_cutoff};
