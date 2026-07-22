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

pub mod model;
pub mod netlist;
pub mod skidl;
pub mod source;
pub mod stage;
pub mod tools;

pub use model::{Circuit, Net, Part, PinRef, RefDes};
pub use netlist::{parse_netlist_file, parse_netlist_str};
pub use skidl::{SkidlRun, SkidlRunner};
pub use source::CircuitSource;
pub use stage::{Finding, PipelineReport, Severity, Stage, StageError, StageOutcome};
pub use tools::{find_on_path, phase0_tools, Tool, ToolStatus};
