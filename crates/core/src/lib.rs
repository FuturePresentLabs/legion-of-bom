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

pub mod analytical;
pub mod board;
pub mod bom;
pub mod bom_repair;
pub mod carrier;
pub mod decouple;
pub mod drc;
pub mod eagle;
pub mod easyeda;
pub mod edit;
pub mod fab;
pub mod fetch;
pub mod gerber;
pub mod git;
pub mod guide;
pub mod hardware;
pub mod images;
pub mod import;
pub mod jlcpcb;
pub mod layout;
pub mod legalize;
pub mod logo;
pub mod manifest;
pub mod model;
pub mod mouser;
pub mod netlist;
pub mod package;
pub mod panel;
pub mod panel_edit;
pub mod parts;
pub mod pdf;
pub mod photo;
pub mod placement;
pub mod placement_edit;
pub mod project;
pub mod resistor;
pub mod route;
pub mod rules;
pub mod scaffold;
pub mod schematic;
mod sexpr;
pub mod skidl;
pub mod source;
pub mod sourcing;
pub mod spice;
pub mod stage;
pub mod subboard;
pub mod summing;
pub mod symbols;
pub mod theme;
pub mod thonk;
pub mod tools;
pub mod units;
pub mod validate;
pub mod verify;

pub use board::{
    build_facts, decoupling_pairs, generate_board, generate_board_artifacts, generate_board_report,
    minimum_hp, BoardArtifacts, BoardError, BoardOptions, EurorackPlacer, GridPlacer, PartFacts,
    Placement, Placer, SeededPlacer, SilkLegend, SilkValues,
};
pub use bom::{generate_bom, Bom, BomLine, LineKind};
pub use bom_repair::{
    classify as classify_comment, fill_mpns, package_key, part_kind_of, plan as plan_repair,
    search_keyword, value_key, Comment, FillReport, Repair,
};
pub use carrier::{AudioChannel, CarrierBuilder, CarrierError, CarrierPlatform};
pub use decouple::{snap as snap_decoupling, Report as DecoupleReport};
pub use drc::{run_drc, DrcItem, DrcReport, DrcViolation};
pub use eagle::{
    map_footprint, map_footprint_for, parse_board as parse_eagle_board,
    parse_schematic as parse_eagle_schematic, to_skidl, EagleImport, EaglePlacement,
};
pub use easyeda::{product_image_url, search_parts as lcsc_search_parts, LcscCandidate};
pub use edit::{apply_edit_str, edit_manifest, CircuitEdit, EditError, ManifestEdit};
pub use fab::{
    board_sides, export_board_svg, export_cpl, export_gerbers, jlc_assembly_bom, jlc_bom_csv,
    jlc_cpl_from_kicad_pos, jlcpcb_design_rules, png_to_jpeg, render_board_jpeg, render_board_png,
    strip_smd, zip_dir, BoardSides, MountCounts, Populate, Quality,
};
pub use fetch::{fetch_from_jlcpcb, fetch_from_kicad};
pub use gerber::{layers_to_svg, read_layers, Layer as GerberLayer, LayerKind};
pub use git::{stage as git_stage, staged_paths, GitError};
pub use guide::{
    build_guide, build_guide_with, guide_from_parts, guide_from_parts_with, guide_to_html,
    guide_to_pdf, BoardPng, BuildGuide, BuildStep, GuideOptions, KitType, PartNote, PlacedPart,
};
pub use hardware::{for_footprint as hardware_for_footprint, HardwareItem};
pub use images::{
    cached_source_bytes, cropped_bytes, default_cache_dir as default_image_cache_dir, embed_source,
    fetch_data_uri, read_crop, source_bytes, source_mime, write_crop, Crop,
};
pub use import::{
    package_is_through_hole, package_size_mm, parse_bom as parse_imported_bom,
    parse_cpl as parse_imported_cpl, read_package, ImportedBoard, ImportedPart, ImportedPlacement,
};
pub use jlcpcb::{JlcpcbClient, JlcpcbComponent, JlcpcbError};
pub use layout::{
    eurorack_trial_build, measure, minimum_routable_hp, run_layout_loop, score, CostWeights,
    HpSearch, HpTrial, LayoutLoop, LayoutMode, LayoutReport, PlacementMetrics, RoutableHp,
};
pub use legalize::{legalize, Report as LegalizeReport};
pub use logo::Logo;
pub use manifest::{BuildCopy, CircuitEntry, Defaults, Manifest, ManifestError, RepoMeta};
pub use model::{Circuit, Net, Part, PinRef, RefDes, Side, SimModel};
pub use mouser::{MouserClient, MouserError, PartPrice, PriceBreak};
pub use netlist::{parse_netlist_file, parse_netlist_str};
pub use package::{body_mm as package_body_mm, short_name as package_short_name};
pub use panel::{
    default_panel_orders_dir, derive_panel, derive_panel_for, footprint_shape, min_panel_hp,
    min_panel_hp_for, panel_from_board, panel_to_dxf, panel_to_kicad_pcb, panel_to_svg,
    BuiltinCutouts, ControlKind, Cutout, CutoutRole, CutoutShape, CutoutSource, CutoutSpec,
    EurorackPanel, MountingHole, PanelFile, PanelFinish, PanelFormat, PanelOrder, PanelOrderStatus,
    PanelOrders, PanelSpec,
};
pub use panel_edit::{apply_panel_edit_str, edit_panel, PanelEdit, PanelEditError};
pub use parts::{
    default_parts_dir, HousePart, PartRecord, PartResolution, PartsError, PartsLibrary, PinRecord,
    RatingRecord, ResolutionStatus,
};
pub use photo::{photo_keyword, photo_source, thonk_keyword};
pub use placement::{
    Column as PlacementColumn, Grid as PlacementGrid, PlacementError, PlacementFile, Point,
    Row as PlacementRow,
};
pub use placement_edit::{
    apply_placement_edit_str, edit_placement, ops_for_targets, ops_to_reach, Moved,
    PlacementEditError, PlacementEditResult, PlacementOp,
};
pub use project::{ArtifactKind, ArtifactStatus, ArtifactView, CircuitView, ProjectView, RepoView};
pub use resistor::{color_code, parse_ohms, Band, ColorCode};
pub use route::{
    unroutable_by_placement, GridRouter, MstRouter, PadLayer, PadPoint, PathfinderRouter, RouteNet,
    RouteOptions, RouteOutput, Router, Track, Via,
};
pub use rules::{
    derive as derive_rules, evaluate as evaluate_rules, penalty as rule_penalty, Rule, Tier,
    Violation,
};
pub use scaffold::{ignore_block, is_generated, merge_gitignore, IgnoreRule};
pub use schematic::schematic_to_svg;
pub use skidl::{SkidlRun, SkidlRunner};
pub use source::CircuitSource;
pub use sourcing::{
    build_query, part_kind, suggest_by_keyword, suggest_mpns, MpnCandidate, PartKind,
    SourcingClients,
};
pub use spice::{
    signal_channels, simulate_ac, simulate_tran, simulate_tran_drive, AcPoint, AcResult, SimConfig,
    TranAnalysis, TranDrive, TranPoint, TranResult,
};
pub use stage::{Finding, PipelineReport, Severity, Stage, StageError, StageOutcome};
pub use thonk::{product_image_url as thonk_image_url, search as thonk_search, ThonkProduct};
pub use tools::{find_on_path, kicad_cli_path, phase0_tools, Tool, ToolStatus};
pub use units::parse_eng_value;
pub use validate::validate_erc;
pub use verify::{
    analytic_check, check_channel_crosstalk, check_noninverting_gain, check_rc_cutoff,
};
