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
pub mod fetch;
pub mod jlcpcb;
pub mod model;
pub mod mouser;
pub mod netlist;
pub mod parts;
pub mod route;
mod sexpr;
pub mod skidl;
pub mod source;
pub mod spice;
pub mod stage;
mod symbols;
pub mod tools;
pub mod units;
pub mod validate;
pub mod verify;

pub use board::{
    generate_board, generate_board_report, BoardError, BoardOptions, GridPlacer, PartFacts,
    Placement, Placer, Side,
};
pub use bom::{generate_bom, Bom, BomLine};
pub use fetch::{fetch_from_jlcpcb, fetch_from_kicad};
pub use jlcpcb::{JlcpcbClient, JlcpcbComponent, JlcpcbError};
pub use model::{Circuit, Net, Part, PinRef, RefDes, SimModel};
pub use mouser::{MouserClient, MouserError, PartPrice, PriceBreak};
pub use netlist::{parse_netlist_file, parse_netlist_str};
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
pub use spice::{simulate_ac, AcPoint, AcResult, SimConfig};
pub use stage::{Finding, PipelineReport, Severity, Stage, StageError, StageOutcome};
pub use tools::{find_on_path, phase0_tools, Tool, ToolStatus};
pub use units::parse_eng_value;
pub use validate::validate_erc;
pub use verify::{analytic_check, check_noninverting_gain, check_rc_cutoff};
