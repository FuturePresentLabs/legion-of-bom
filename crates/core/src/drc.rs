//! DRC readback — the layout loop's **check** step (DESIGN.md 6.5, step 3).
//!
//! Runs KiCad's design-rule check headlessly (`kicad-cli pcb drc --format json`)
//! and parses the violations into structured data the iterative layout loop
//! (j54.6) repairs against, and the CLI reports.
//!
//! De-risk note (j54.11): the original plan targeted `kicad-ipc-rs`, but that
//! needs a *running* KiCad with the IPC plugin enabled. `kicad-cli` produces the
//! same DRC data headlessly — the same reason board *generation* moved to direct
//! S-expression emission rather than the IPC API. Live-KiCad IPC stays an option
//! for interactive editing, not the loop's automated check.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::stage::StageError;

/// A parsed `kicad-cli pcb drc --format json` report.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DrcReport {
    /// Design-rule violations (clearance, shorts, silk, holes, …).
    #[serde(default)]
    pub violations: Vec<DrcViolation>,
    /// Ratsnest items with no copper connection.
    #[serde(default)]
    pub unconnected_items: Vec<DrcViolation>,
    /// Board-vs-schematic parity problems.
    #[serde(default)]
    pub schematic_parity: Vec<DrcViolation>,
}

/// One DRC finding.
#[derive(Debug, Clone, Deserialize)]
pub struct DrcViolation {
    /// Rule key, e.g. `clearance`, `unconnected_items`, `hole_to_hole`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// `error`, `warning`, `ignore`, or `exclusion`.
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub description: String,
    /// The items involved (each with a board position, when KiCad gives one).
    #[serde(default)]
    pub items: Vec<DrcItem>,
}

impl DrcViolation {
    /// Whether this is a **silkscreen collision** (DESIGN 6.10) — silk over a
    /// pad/copper, silk over silk, or silk over the board edge — rather than an
    /// electrical rule. Every KiCad silkscreen DRC key contains `silk`
    /// (`silk_over_copper`, `silk_overlap`, `silk_edge_clearance`), so match on
    /// that: a new key variant is caught without a spelling update. These pass
    /// the copper/electrical checks silently, so the layout loop surfaces them
    /// separately (they degrade a hand-assembler's legend, not the circuit).
    pub fn is_silkscreen_collision(&self) -> bool {
        self.kind.contains("silk")
    }
}

/// One item referenced by a violation.
#[derive(Debug, Clone, Deserialize)]
pub struct DrcItem {
    #[serde(default)]
    pub description: String,
    pub pos: Option<Pos>,
    pub uuid: Option<String>,
}

/// A board position in millimetres.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Pos {
    pub x: f64,
    pub y: f64,
}

impl DrcReport {
    /// Parse a `kicad-cli pcb drc --format json` document.
    pub fn from_json(json: &str) -> Result<DrcReport, StageError> {
        serde_json::from_str(json).map_err(|e| StageError::Other(format!("parsing DRC JSON: {e}")))
    }

    /// Every finding across all three categories.
    pub fn all(&self) -> impl Iterator<Item = &DrcViolation> {
        self.violations
            .iter()
            .chain(&self.unconnected_items)
            .chain(&self.schematic_parity)
    }

    /// Findings at `error` severity (what blocks a manufacturable board).
    pub fn errors(&self) -> impl Iterator<Item = &DrcViolation> {
        self.all().filter(|v| v.severity == "error")
    }

    /// Findings at `warning` severity.
    pub fn warnings(&self) -> impl Iterator<Item = &DrcViolation> {
        self.all().filter(|v| v.severity == "warning")
    }

    /// Silkscreen collisions (DESIGN 6.10) — silk over pad/copper, silk-over-
    /// silk, or silk over the board edge. Typically `warning` severity, so they
    /// don't fail [`is_clean`](Self::is_clean); the layout loop's check step
    /// surfaces and repairs them explicitly rather than letting the copper checks
    /// swallow them.
    pub fn silkscreen_collisions(&self) -> impl Iterator<Item = &DrcViolation> {
        self.all().filter(|v| v.is_silkscreen_collision())
    }

    pub fn error_count(&self) -> usize {
        self.errors().count()
    }
    pub fn warning_count(&self) -> usize {
        self.warnings().count()
    }
    pub fn silkscreen_collision_count(&self) -> usize {
        self.silkscreen_collisions().count()
    }
    pub fn unconnected_count(&self) -> usize {
        self.unconnected_items.len()
    }

    /// A board is clean when nothing at `error` severity remains (warnings are
    /// advisory). Unconnected items report as errors, so they count here too.
    pub fn is_clean(&self) -> bool {
        self.error_count() == 0
    }
}

/// Run DRC on a `.kicad_pcb` via `kicad-cli`, returning the parsed report. Refills
/// zones first (`--refill-zones`) so pour-related clearances are checked against
/// the real filled copper.
pub fn run_drc(board: &Path, kicad_cli: &Path) -> Result<DrcReport, StageError> {
    let stem = board
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board");
    let out_path = std::env::temp_dir().join(format!("lob-{stem}-drc.json"));

    let output = Command::new(kicad_cli)
        .args([
            "pcb",
            "drc",
            "--format",
            "json",
            "--severity-all",
            "--refill-zones",
        ])
        .arg("-o")
        .arg(&out_path)
        .arg(board)
        .output()
        .map_err(|e| {
            StageError::ToolNotFound(format!("kicad-cli ({}): {e}", kicad_cli.display()))
        })?;

    // kicad-cli exits non-zero when violations exist; the JSON is still written,
    // so trust the output file, not the exit code.
    if !out_path.is_file() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(StageError::ToolFailed {
            tool: "kicad-cli pcb drc".into(),
            code: output.status.code().unwrap_or(-1),
            stderr: stderr.lines().rev().take(10).collect::<Vec<_>>().join("\n"),
        });
    }
    let json = std::fs::read_to_string(&out_path)?;
    DrcReport::from_json(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "violations": [
            {"type":"clearance","severity":"error","description":"Clearance violation",
             "items":[{"description":"Track [GND]","pos":{"x":100.0,"y":50.0},"uuid":"a"},
                      {"description":"Via [OUT]","pos":{"x":100.1,"y":50.0},"uuid":"b"}]},
            {"type":"silk_over_copper","severity":"warning","description":"Silk over pad","items":[]}
        ],
        "unconnected_items": [
            {"type":"unconnected_items","severity":"error","description":"Missing connection","items":[]}
        ],
        "schematic_parity": []
    }"#;

    #[test]
    fn parses_and_summarises() {
        let r = DrcReport::from_json(SAMPLE).unwrap();
        // 1 clearance error + 1 unconnected error = 2 errors; 1 warning.
        assert_eq!(r.error_count(), 2);
        assert_eq!(r.warning_count(), 1);
        assert_eq!(r.unconnected_count(), 1);
        assert!(!r.is_clean());

        let clearance = r.errors().find(|v| v.kind == "clearance").unwrap();
        assert_eq!(clearance.items.len(), 2);
        assert_eq!(clearance.items[0].pos.unwrap().x, 100.0);
    }

    #[test]
    fn surfaces_silkscreen_collisions() {
        // Real KiCad 10 silk keys: silk_overlap (silk↔silk), silk_over_copper
        // (silk↔pad). Both are warnings — not electrical — so is_clean ignores
        // them, but the loop's check step must be able to enumerate them.
        let json = r#"{"violations":[
            {"type":"silk_overlap","severity":"warning","description":"Silkscreen clearance",
             "items":[{"description":"Text 'C7'","pos":{"x":110.0,"y":95.0}}]},
            {"type":"silk_over_copper","severity":"warning","description":"Silk over pad","items":[]},
            {"type":"clearance","severity":"error","description":"Clearance","items":[]}
        ]}"#;
        let r = DrcReport::from_json(json).unwrap();
        assert_eq!(r.silkscreen_collision_count(), 2);
        assert!(r.silkscreen_collisions().all(|v| v.is_silkscreen_collision()));
        // A real electrical error is still the only thing that makes it unclean.
        assert_eq!(r.error_count(), 1);
        assert!(!r.is_clean());
    }

    #[test]
    fn clean_report_is_clean() {
        let r = DrcReport::from_json(r#"{"violations":[],"unconnected_items":[]}"#).unwrap();
        assert!(r.is_clean());
        assert_eq!(r.error_count(), 0);
    }

    #[test]
    fn tolerates_missing_fields() {
        // A report with only some keys still parses (serde defaults).
        let r = DrcReport::from_json("{}").unwrap();
        assert!(r.is_clean());
    }
}
