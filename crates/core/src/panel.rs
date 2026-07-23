//! Panel specification, DXF export, and order tracking.
//!
//! DESIGN.md §6.9, §7.1, §7.5.
//!
//! The [`PanelSpec`] trait is the format-agnostic seam: dimensions in mm,
//! mounting holes, and anchored cutouts. The only v1 implementation is
//! Eurorack; pedal/rack/500-series are deliberately unimplemented.
//!
//! DXF export consumes any `&dyn PanelSpec` — it does not know about HP, U,
//! or enclosure size classes.

use std::path::PathBuf;
use std::process::Command;

use crate::parts::PartsError;
use crate::tools::find_on_path;

// ---------------------------------------------------------------------------
//  PanelSpec trait + geometry types
// ---------------------------------------------------------------------------

/// A mounting hole on a panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MountingHole {
    pub x_mm: f64,
    pub y_mm: f64,
    pub diameter_mm: f64,
}

/// An anchored cutout (jack, pot, switch, LED, etc.).
#[derive(Debug, Clone, PartialEq)]
pub struct Cutout {
    pub x_mm: f64,
    pub y_mm: f64,
    pub rotation_deg: f64,
    /// Footprint name, e.g. `"Thonkiconn"`, `"Alpha9mm"`, `"LED_3mm"`.
    pub footprint: String,
}

/// The shape of a cutout, derived from its footprint name.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CutoutShape {
    /// A round hole (mounting holes, pots, LEDs).
    Circle { diameter_mm: f64 },
    /// A rectangular cutout (jacks, some switches).
    RoundedRect {
        width_mm: f64,
        height_mm: f64,
        corner_radius_mm: f64,
    },
}

/// Return the standard [`CutoutShape`] for a footprint name, or `None` if
/// the footprint is unknown.
pub fn footprint_shape(footprint: &str) -> Option<CutoutShape> {
    // Normalise: strip common prefixes/suffixes and lower-case.
    let name = footprint
        .rsplit_once(':')
        .map(|(_, r)| r)
        .unwrap_or(footprint)
        .to_ascii_lowercase();
    match name.as_str() {
        "thonkiconn" | "pj301m" | "pj301" => Some(CutoutShape::RoundedRect {
            width_mm: 10.0,
            height_mm: 8.5,
            corner_radius_mm: 0.5,
        }),
        "alpha9mm" | "alphapot" | "pot_alpha_9mm" => Some(CutoutShape::Circle { diameter_mm: 7.0 }),
        "led_3mm" | "led3mm" | "led_3" => Some(CutoutShape::Circle { diameter_mm: 3.0 }),
        "led_5mm" | "led5mm" | "led_5" => Some(CutoutShape::Circle { diameter_mm: 5.0 }),
        "toggle" | "toggle_spst" | "toggle_spdt" => Some(CutoutShape::Circle { diameter_mm: 6.5 }),
        "m3" | "mounting_hole_m3" => Some(CutoutShape::Circle { diameter_mm: 3.2 }),
        _ => None,
    }
}

/// The format-agnostic panel specification.
///
/// All dimensions are in millimeters. No format-specific concepts (HP, U,
/// enclosure size class) appear in the trait itself — those are internal to
/// each implementation.
pub trait PanelSpec {
    /// Panel width in mm.
    fn width_mm(&self) -> f64;
    /// Panel height in mm.
    fn height_mm(&self) -> f64;
    /// Panel thickness in mm.
    fn thickness_mm(&self) -> f64;
    /// Mounting holes.
    fn mounting_holes(&self) -> &[MountingHole];
    /// Anchored cutouts (jacks, pots, switches, LEDs, etc.).
    fn cutouts(&self) -> &[Cutout];
}

// ---------------------------------------------------------------------------
//  Eurorack implementation
// ---------------------------------------------------------------------------

const EURORACK_HEIGHT_MM: f64 = 128.5;
const HP_MM: f64 = 5.08;
const EURORACK_HOLE_DIAMETER_MM: f64 = 3.2;
const EURORACK_HOLE_INSET_X_MM: f64 = 7.5;
const EURORACK_HOLE_INSET_Y_MM: f64 = 3.0;

/// A Eurorack panel.
///
/// Constructed in HP (horizontal pitch) internally, but exposes only mm
/// through the [`PanelSpec`] trait.
#[derive(Debug, Clone, PartialEq)]
pub struct EurorackPanel {
    hp: u16,
    thickness_mm: f64,
    extra_holes: Vec<MountingHole>,
    cutouts: Vec<Cutout>,
}

impl EurorackPanel {
    /// Create a new Eurorack panel of the given HP width.
    ///
    /// Standard height (128.5 mm) and thickness (2.0 mm) are applied.
    /// Default mounting holes are added automatically based on HP width.
    pub fn new(hp: u16) -> Self {
        let mut panel = EurorackPanel {
            hp,
            thickness_mm: 2.0,
            extra_holes: Vec::new(),
            cutouts: Vec::new(),
        };
        panel.rebuild_default_holes();
        panel
    }

    /// Override the default thickness (mm).
    pub fn with_thickness(mut self, mm: f64) -> Self {
        self.thickness_mm = mm;
        self
    }

    /// Add a cutout at the given position (mm from bottom-left).
    pub fn with_cutout(mut self, x_mm: f64, y_mm: f64, footprint: impl Into<String>) -> Self {
        self.cutouts.push(Cutout {
            x_mm,
            y_mm,
            rotation_deg: 0.0,
            footprint: footprint.into(),
        });
        self
    }

    /// Add a cutout with explicit rotation.
    pub fn with_cutout_rotated(
        mut self,
        x_mm: f64,
        y_mm: f64,
        rotation_deg: f64,
        footprint: impl Into<String>,
    ) -> Self {
        self.cutouts.push(Cutout {
            x_mm,
            y_mm,
            rotation_deg,
            footprint: footprint.into(),
        });
        self
    }

    /// Width in mm (HP × 5.08).
    pub fn width_mm_value(&self) -> f64 {
        f64::from(self.hp) * HP_MM
    }

    /// The HP width (internal unit, not part of `PanelSpec`).
    pub fn hp(&self) -> u16 {
        self.hp
    }

    fn rebuild_default_holes(&mut self) {
        let w = self.width_mm_value();
        let h = EURORACK_HEIGHT_MM;
        // Left side holes (always present).
        self.extra_holes.push(MountingHole {
            x_mm: EURORACK_HOLE_INSET_X_MM,
            y_mm: h - EURORACK_HOLE_INSET_Y_MM,
            diameter_mm: EURORACK_HOLE_DIAMETER_MM,
        });
        self.extra_holes.push(MountingHole {
            x_mm: EURORACK_HOLE_INSET_X_MM,
            y_mm: EURORACK_HOLE_INSET_Y_MM,
            diameter_mm: EURORACK_HOLE_DIAMETER_MM,
        });
        // Right side holes for panels ≥ 8 HP.
        if self.hp >= 8 {
            self.extra_holes.push(MountingHole {
                x_mm: w - EURORACK_HOLE_INSET_X_MM,
                y_mm: h - EURORACK_HOLE_INSET_Y_MM,
                diameter_mm: EURORACK_HOLE_DIAMETER_MM,
            });
            self.extra_holes.push(MountingHole {
                x_mm: w - EURORACK_HOLE_INSET_X_MM,
                y_mm: EURORACK_HOLE_INSET_Y_MM,
                diameter_mm: EURORACK_HOLE_DIAMETER_MM,
            });
        }
    }
}

impl PanelSpec for EurorackPanel {
    fn width_mm(&self) -> f64 {
        self.width_mm_value()
    }

    fn height_mm(&self) -> f64 {
        EURORACK_HEIGHT_MM
    }

    fn thickness_mm(&self) -> f64 {
        self.thickness_mm
    }

    fn mounting_holes(&self) -> &[MountingHole] {
        &self.extra_holes
    }

    fn cutouts(&self) -> &[Cutout] {
        &self.cutouts
    }
}

// ---------------------------------------------------------------------------
//  TOML file format (interim — until layout loop generates panels)
// ---------------------------------------------------------------------------

/// A panel definition read from a TOML file.
///
/// Example:
/// ```toml
/// format = "eurorack"
/// hp = 8
/// thickness_mm = 2.0
///
/// [[cutouts]]
/// x_mm = 10.0
/// y_mm = 50.0
/// footprint = "Thonkiconn"
/// ```
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct PanelFile {
    pub format: String,
    #[serde(default)]
    pub hp: Option<u16>,
    #[serde(default = "default_thickness")]
    pub thickness_mm: f64,
    #[serde(default)]
    pub cutouts: Vec<CutoutFile>,
}

fn default_thickness() -> f64 {
    2.0
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct CutoutFile {
    pub x_mm: f64,
    pub y_mm: f64,
    #[serde(default)]
    pub rotation_deg: f64,
    pub footprint: String,
}

impl PanelFile {
    /// Parse from TOML bytes.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Convert to a concrete `PanelSpec` implementation.
    ///
    /// Returns `Err` if the format is unknown or required fields are missing.
    pub fn to_spec(&self) -> Result<Box<dyn PanelSpec>, String> {
        match self.format.as_str() {
            "eurorack" => {
                let hp = self.hp.ok_or("eurorack panel requires `hp`")?;
                let mut panel = EurorackPanel::new(hp).with_thickness(self.thickness_mm);
                for c in &self.cutouts {
                    panel = panel.with_cutout_rotated(
                        c.x_mm,
                        c.y_mm,
                        c.rotation_deg,
                        c.footprint.clone(),
                    );
                }
                Ok(Box::new(panel))
            }
            other => Err(format!("unsupported panel format: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
//  DXF export (ASCII, minimal, SendCutSend-compatible)
// ---------------------------------------------------------------------------

/// Write a DXF representation of `panel` to `w`.
///
/// The DXF contains:
/// * a closed `LWPOLYLINE` for the panel outline,
/// * `CIRCLE`s for round cutouts and mounting holes,
/// * `LWPOLYLINE`s for rectangular cutouts.
pub fn write_dxf<W: std::fmt::Write>(w: &mut W, panel: &dyn PanelSpec) -> std::fmt::Result {
    let width = panel.width_mm();
    let height = panel.height_mm();

    // Header ----------------------------------------------------------------
    writeln!(w, "0")?;
    writeln!(w, "SECTION")?;
    writeln!(w, "2")?;
    writeln!(w, "HEADER")?;
    writeln!(w, "9")?;
    writeln!(w, "$ACADVER")?;
    writeln!(w, "1")?;
    writeln!(w, "AC1032")?;
    writeln!(w, "0")?;
    writeln!(w, "ENDSEC")?;

    // Tables ----------------------------------------------------------------
    writeln!(w, "0")?;
    writeln!(w, "SECTION")?;
    writeln!(w, "2")?;
    writeln!(w, "TABLES")?;
    writeln!(w, "0")?;
    writeln!(w, "TABLE")?;
    writeln!(w, "2")?;
    writeln!(w, "LAYER")?;
    writeln!(w, "5")?;
    writeln!(w, "2")?;
    writeln!(w, "100")?;
    writeln!(w, "AcDbSymbolTable")?;
    writeln!(w, "70")?;
    writeln!(w, "1")?;
    writeln!(w, "0")?;
    writeln!(w, "LAYER")?;
    writeln!(w, "5")?;
    writeln!(w, "10")?;
    writeln!(w, "100")?;
    writeln!(w, "AcDbSymbolTableRecord")?;
    writeln!(w, "100")?;
    writeln!(w, "AcDbLayerTableRecord")?;
    writeln!(w, "2")?;
    writeln!(w, "0")?;
    writeln!(w, "70")?;
    writeln!(w, "0")?;
    writeln!(w, "62")?;
    writeln!(w, "7")?;
    writeln!(w, "6")?;
    writeln!(w, "Continuous")?;
    writeln!(w, "0")?;
    writeln!(w, "ENDTAB")?;
    writeln!(w, "0")?;
    writeln!(w, "ENDSEC")?;

    // Entities --------------------------------------------------------------
    writeln!(w, "0")?;
    writeln!(w, "SECTION")?;
    writeln!(w, "2")?;
    writeln!(w, "ENTITIES")?;

    // Panel outline.
    write_lwpolyline_rect(w, 0.0, 0.0, width, height)?;

    // Mounting holes.
    for hole in panel.mounting_holes() {
        write_circle(w, hole.x_mm, hole.y_mm, hole.diameter_mm / 2.0)?;
    }

    // Cutouts.
    for cutout in panel.cutouts() {
        match footprint_shape(&cutout.footprint) {
            Some(CutoutShape::Circle { diameter_mm }) => {
                write_circle(w, cutout.x_mm, cutout.y_mm, diameter_mm / 2.0)?;
            }
            Some(CutoutShape::RoundedRect {
                width_mm,
                height_mm,
                corner_radius_mm: _,
            }) => {
                // For laser/waterjet, a sharp rectangle is fine; radius is
                // handled by the cutter kerf or post-processing.
                let hw = width_mm / 2.0;
                let hh = height_mm / 2.0;
                write_lwpolyline_rect(
                    w,
                    cutout.x_mm - hw,
                    cutout.y_mm - hh,
                    cutout.x_mm + hw,
                    cutout.y_mm + hh,
                )?;
            }
            None => {
                // Unknown footprint — emit a small circle as a visual marker.
                write_circle(w, cutout.x_mm, cutout.y_mm, 1.5)?;
            }
        }
    }

    writeln!(w, "0")?;
    writeln!(w, "ENDSEC")?;
    writeln!(w, "0")?;
    writeln!(w, "EOF")?;
    Ok(())
}

/// Generate a DXF string from a panel.
pub fn panel_to_dxf(panel: &dyn PanelSpec) -> String {
    let mut s = String::with_capacity(4096);
    write_dxf(&mut s, panel).expect("write to String is infallible");
    s
}

/// Generate a **panel PCB** (`.kicad_pcb`): the outline, jack/pot/LED cutouts and
/// mounting holes as `Edge.Cuts` loops (inner loops become board cutouts), plus a
/// silkscreen title. This is the "PCB panel" many Eurorack builders order instead
/// of a milled aluminium one — it runs through the same gerber export as any
/// board. Mechanical only: no copper, no components.
///
/// Panel coordinates are measured from the bottom-left; KiCad's are top-down, so
/// Y is flipped here.
pub fn panel_to_kicad_pcb(panel: &dyn PanelSpec, title: &str) -> String {
    use crate::board::{det_uuid, mm};
    let (w, h) = (panel.width_mm(), panel.height_mm());
    let fy = |y: f64| h - y;
    let edge_rect = |x1: f64, y1: f64, x2: f64, y2: f64, seed: &str| {
        format!(
            "  (gr_rect (start {} {}) (end {} {}) (stroke (width 0.15) (type solid)) \
             (fill no) (layer \"Edge.Cuts\") (uuid \"{}\"))\n",
            mm(x1),
            mm(y1),
            mm(x2),
            mm(y2),
            det_uuid(seed)
        )
    };
    let edge_circle = |cx: f64, cy: f64, r: f64, seed: &str| {
        format!(
            "  (gr_circle (center {} {}) (end {} {}) (stroke (width 0.15) (type solid)) \
             (fill no) (layer \"Edge.Cuts\") (uuid \"{}\"))\n",
            mm(cx),
            mm(cy),
            mm(cx + r),
            mm(cy),
            det_uuid(seed)
        )
    };

    let mut s = String::new();
    s.push_str(
        "(kicad_pcb (version 20241229) (generator \"legion-of-bom\") (generator_version \"9.0\")\n\
         \x20 (general (thickness 1.6))\n  (paper \"A4\")\n\
         \x20 (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal) (5 \"F.SilkS\" user) \
         (7 \"B.SilkS\" user) (1 \"F.Mask\" user) (3 \"B.Mask\" user) (25 \"Edge.Cuts\" user) \
         (35 \"F.Fab\" user) (33 \"B.Fab\" user))\n\
         \x20 (setup (pad_to_mask_clearance 0))\n  (net 0 \"\")\n",
    );
    // Panel outline.
    s.push_str(&edge_rect(0.0, 0.0, w, h, "panel.outline"));
    // Mounting holes.
    for (i, hole) in panel.mounting_holes().iter().enumerate() {
        s.push_str(&edge_circle(
            hole.x_mm,
            fy(hole.y_mm),
            hole.diameter_mm / 2.0,
            &format!("panel.hole.{i}"),
        ));
    }
    // Cutouts (jack rects, pot/LED circles), as inner Edge.Cuts loops.
    for (i, c) in panel.cutouts().iter().enumerate() {
        let (cx, cy) = (c.x_mm, fy(c.y_mm));
        let seed = format!("panel.cut.{i}");
        match footprint_shape(&c.footprint) {
            Some(CutoutShape::Circle { diameter_mm }) => {
                s.push_str(&edge_circle(cx, cy, diameter_mm / 2.0, &seed))
            }
            Some(CutoutShape::RoundedRect {
                width_mm,
                height_mm,
                ..
            }) => s.push_str(&edge_rect(
                cx - width_mm / 2.0,
                cy - height_mm / 2.0,
                cx + width_mm / 2.0,
                cy + height_mm / 2.0,
                &seed,
            )),
            None => s.push_str(&edge_circle(cx, cy, 1.5, &seed)),
        }
    }
    // Title, silkscreen, reading up the panel.
    s.push_str(&format!(
        "  (gr_text \"{}\" (at {} {} 90) (layer \"F.SilkS\") (uuid \"{}\") \
         (effects (font (size 2.5 2.5) (thickness 0.35))))\n",
        title,
        mm(w / 2.0),
        mm(h / 2.0),
        det_uuid("panel.title")
    ));
    s.push_str(")\n");
    s
}

fn write_circle<W: std::fmt::Write>(w: &mut W, cx: f64, cy: f64, r: f64) -> std::fmt::Result {
    writeln!(w, "0")?;
    writeln!(w, "CIRCLE")?;
    writeln!(w, "8")?;
    writeln!(w, "0")?;
    writeln!(w, "10")?;
    writeln!(w, "{cx}")?;
    writeln!(w, "20")?;
    writeln!(w, "{cy}")?;
    writeln!(w, "40")?;
    writeln!(w, "{r}")
}

fn write_lwpolyline_rect<W: std::fmt::Write>(
    w: &mut W,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
) -> std::fmt::Result {
    writeln!(w, "0")?;
    writeln!(w, "LWPOLYLINE")?;
    writeln!(w, "8")?;
    writeln!(w, "0")?;
    writeln!(w, "100")?;
    writeln!(w, "AcDbEntity")?;
    writeln!(w, "100")?;
    writeln!(w, "AcDbPolyline")?;
    writeln!(w, "90")?;
    writeln!(w, "4")?;
    writeln!(w, "70")?;
    writeln!(w, "1")?; // closed
    writeln!(w, "43")?;
    writeln!(w, "0.0")?;

    for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
        writeln!(w, "10")?;
        writeln!(w, "{x}")?;
        writeln!(w, "20")?;
        writeln!(w, "{y}")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
//  Panel order tracking (Dolt)
// ---------------------------------------------------------------------------

const PANEL_ORDERS_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS panel_orders (\
  id INT AUTO_INCREMENT PRIMARY KEY,\
  module VARCHAR(64) NOT NULL,\
  dxf_path TEXT NOT NULL,\
  vendor VARCHAR(32),\
  status VARCHAR(16) DEFAULT 'not_ordered',\
  ordered_at DATETIME,\
  tracking_ref TEXT,\
  notes TEXT);";

/// One row in the panel-orders table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelOrder {
    pub id: i64,
    pub module: String,
    pub dxf_path: String,
    pub vendor: Option<String>,
    pub status: PanelOrderStatus,
    pub ordered_at: Option<String>,
    pub tracking_ref: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelOrderStatus {
    NotOrdered,
    Ordered,
    Shipped,
    Received,
}

impl PanelOrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PanelOrderStatus::NotOrdered => "not_ordered",
            PanelOrderStatus::Ordered => "ordered",
            PanelOrderStatus::Shipped => "shipped",
            PanelOrderStatus::Received => "received",
        }
    }
}

impl std::str::FromStr for PanelOrderStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "not_ordered" => Ok(PanelOrderStatus::NotOrdered),
            "ordered" => Ok(PanelOrderStatus::Ordered),
            "shipped" => Ok(PanelOrderStatus::Shipped),
            "received" => Ok(PanelOrderStatus::Received),
            other => Err(format!("unknown panel order status: {other}")),
        }
    }
}

/// A handle to the Dolt-backed panel-orders store.
#[derive(Debug, Clone)]
pub struct PanelOrders {
    root: PathBuf,
    dolt: PathBuf,
}

impl PanelOrders {
    /// Open (initialising if needed) the panel-orders store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, PartsError> {
        let dolt = find_on_path("dolt").ok_or(PartsError::DoltNotFound)?;
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let store = PanelOrders { root, dolt };
        if !store.root.join(".dolt").is_dir() {
            store.dolt(&["init"], "init")?;
        }
        store.sql(PANEL_ORDERS_SCHEMA)?;
        Ok(store)
    }

    /// Record a new panel order row.
    pub fn create(
        &self,
        module: &str,
        dxf_path: &str,
        vendor: Option<&str>,
        notes: Option<&str>,
    ) -> Result<i64, PartsError> {
        let sql = format!(
            "INSERT INTO panel_orders (module, dxf_path, vendor, status, notes) \
             VALUES ({}, {}, {}, {}, {});",
            sql_str(module),
            sql_str(dxf_path),
            sql_opt(vendor),
            sql_str(PanelOrderStatus::NotOrdered.as_str()),
            sql_opt(notes),
        );
        self.sql(&sql)?;
        // Fetch the auto-increment id back.
        let rows = self.query("SELECT LAST_INSERT_ID() AS id;")?;
        let id = rows
            .into_iter()
            .next()
            .and_then(|r| r.get("id").and_then(serde_json::Value::as_i64))
            .unwrap_or(0);
        Ok(id)
    }

    /// Find the most recent order for a module, if any.
    pub fn latest(&self, module: &str) -> Result<Option<PanelOrder>, PartsError> {
        let rows = self.query(&format!(
            "SELECT * FROM panel_orders WHERE module={} ORDER BY id DESC LIMIT 1",
            sql_str(module)
        ))?;
        Ok(rows.into_iter().next().and_then(parse_order_row))
    }

    /// List all orders for a module, newest first.
    pub fn list(&self, module: &str) -> Result<Vec<PanelOrder>, PartsError> {
        let rows = self.query(&format!(
            "SELECT * FROM panel_orders WHERE module={} ORDER BY id DESC",
            sql_str(module)
        ))?;
        Ok(rows.into_iter().filter_map(parse_order_row).collect())
    }

    /// Update status to `ordered` and record vendor + tracking ref.
    pub fn mark_ordered(
        &self,
        module: &str,
        vendor: &str,
        tracking_ref: Option<&str>,
    ) -> Result<(), PartsError> {
        let sql = format!(
            "UPDATE panel_orders SET status={}, ordered_at=NOW(), vendor={}, tracking_ref={} \
             WHERE module={} AND status={};",
            sql_str(PanelOrderStatus::Ordered.as_str()),
            sql_str(vendor),
            sql_opt(tracking_ref),
            sql_str(module),
            sql_str(PanelOrderStatus::NotOrdered.as_str()),
        );
        self.sql(&sql)
    }

    /// Update status for a given order id.
    pub fn set_status(&self, id: i64, status: PanelOrderStatus) -> Result<(), PartsError> {
        let sql = format!(
            "UPDATE panel_orders SET status={} WHERE id={id};",
            sql_str(status.as_str()),
        );
        self.sql(&sql)
    }

    // ---- dolt plumbing (mirrors PartsLibrary) -----------------------------

    fn dolt(&self, args: &[&str], context: &str) -> Result<String, PartsError> {
        let output = Command::new(&self.dolt)
            .current_dir(&self.root)
            .args(args)
            .output()?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(PartsError::Dolt {
                context: context.to_string(),
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    fn sql(&self, sql: &str) -> Result<(), PartsError> {
        self.dolt(&["sql", "-q", sql], "sql").map(|_| ())
    }

    fn query(&self, sql: &str) -> Result<Vec<serde_json::Value>, PartsError> {
        let stdout = self.dolt(&["sql", "-q", sql, "-r", "json"], "query")?;
        if stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        let value: serde_json::Value = serde_json::from_str(&stdout)?;
        Ok(value
            .get("rows")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default())
    }
}

/// The default panel-orders location (override with `LOB_PANEL_ORDERS_DIR`).
pub fn default_panel_orders_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOB_PANEL_ORDERS_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("legion-of-bom").join("panels")
}

fn parse_order_row(row: serde_json::Value) -> Option<PanelOrder> {
    Some(PanelOrder {
        id: row.get("id").and_then(serde_json::Value::as_i64)?,
        module: row.get("module")?.as_str()?.to_string(),
        dxf_path: row.get("dxf_path")?.as_str()?.to_string(),
        vendor: row
            .get("vendor")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        status: row
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse().ok())?,
        ordered_at: row
            .get("ordered_at")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        tracking_ref: row
            .get("tracking_ref")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        notes: row
            .get("notes")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
    })
}

// ---- SQL helpers (copied from parts.rs; kept private to avoid pub) --------

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn sql_opt(s: Option<&str>) -> String {
    s.map(sql_str).unwrap_or_else(|| "NULL".to_string())
}

// ---------------------------------------------------------------------------
//  Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eurorack_dimensions_in_mm() {
        let panel = EurorackPanel::new(6);
        assert_eq!(panel.width_mm(), 6.0 * 5.08);
        assert_eq!(panel.height_mm(), 128.5);
        assert_eq!(panel.thickness_mm(), 2.0);
    }

    #[test]
    fn panel_pcb_has_outline_cutouts_and_title() {
        let panel = EurorackPanel::new(6)
            .with_cutout(15.24, 100.0, "Alpha9mm") // pot -> circle
            .with_cutout(15.24, 30.0, "Thonkiconn"); // jack -> rect
        let pcb = panel_to_kicad_pcb(&panel, "demo");
        assert!(pcb.starts_with("(kicad_pcb"));
        assert!(pcb.contains(r#"(layer "Edge.Cuts")"#));
        assert!(pcb.contains("gr_circle"), "pot cutout as a circle");
        // Outline rect + the jack cutout rect (≥2 gr_rect on Edge.Cuts).
        assert!(pcb.matches("gr_rect").count() >= 2);
        assert!(pcb.contains(r#"(gr_text "demo""#));
        assert!(pcb.contains(r#"(layer "F.SilkS")"#));
    }

    #[test]
    fn eurorack_small_panel_two_holes() {
        let panel = EurorackPanel::new(4);
        assert_eq!(panel.mounting_holes().len(), 2);
    }

    #[test]
    fn eurorack_large_panel_four_holes() {
        let panel = EurorackPanel::new(10);
        assert_eq!(panel.mounting_holes().len(), 4);
    }

    #[test]
    fn eurorack_cutouts_round_trip() {
        let panel = EurorackPanel::new(8)
            .with_cutout(10.0, 50.0, "Thonkiconn")
            .with_cutout(25.0, 50.0, "Alpha9mm");
        assert_eq!(panel.cutouts().len(), 2);
        assert_eq!(panel.cutouts()[0].footprint, "Thonkiconn");
        assert_eq!(panel.cutouts()[1].footprint, "Alpha9mm");
    }

    #[test]
    fn footprint_shape_lookup() {
        assert!(matches!(
            footprint_shape("Thonkiconn"),
            Some(CutoutShape::RoundedRect { .. })
        ));
        assert!(matches!(
            footprint_shape("Alpha9mm"),
            Some(CutoutShape::Circle { diameter_mm: 7.0 })
        ));
        assert!(matches!(
            footprint_shape("LED_3mm"),
            Some(CutoutShape::Circle { diameter_mm: 3.0 })
        ));
        assert!(footprint_shape("UnknownThing").is_none());
    }

    #[test]
    fn dxf_contains_entities() {
        let panel = EurorackPanel::new(8)
            .with_cutout(10.0, 50.0, "Thonkiconn")
            .with_cutout(25.0, 50.0, "Alpha9mm");
        let dxf = panel_to_dxf(&panel);
        assert!(dxf.contains("LWPOLYLINE"));
        assert!(dxf.contains("CIRCLE"));
        assert!(dxf.contains("EOF"));
        // Mounting holes + Alpha9mm = at least 5 circles (4 holes + 1 cutout).
        assert!(dxf.matches("CIRCLE").count() >= 5);
    }

    #[test]
    fn panel_file_roundtrip() {
        let toml = r#"
format = "eurorack"
hp = 8
thickness_mm = 2.0

[[cutouts]]
x_mm = 10.0
y_mm = 50.0
footprint = "Thonkiconn"

[[cutouts]]
x_mm = 25.0
y_mm = 50.0
footprint = "Alpha9mm"
"#;
        let file = PanelFile::from_toml(toml).unwrap();
        assert_eq!(file.format, "eurorack");
        assert_eq!(file.hp, Some(8));
        assert_eq!(file.cutouts.len(), 2);

        let spec = file.to_spec().unwrap();
        assert_eq!(spec.width_mm(), 8.0 * 5.08);
        assert_eq!(spec.cutouts().len(), 2);
    }

    #[test]
    fn panel_file_rejects_unknown_format() {
        let toml = r#"format = "pedal""#;
        let file = PanelFile::from_toml(toml).unwrap();
        assert!(file.to_spec().is_err());
    }

    #[test]
    fn panel_order_status_roundtrip() {
        assert_eq!(
            "not_ordered".parse::<PanelOrderStatus>().unwrap(),
            PanelOrderStatus::NotOrdered
        );
        assert_eq!(
            "ordered".parse::<PanelOrderStatus>().unwrap(),
            PanelOrderStatus::Ordered
        );
        assert!("bogus".parse::<PanelOrderStatus>().is_err());
    }

    /// Full round-trip against a real Dolt repo. Skipped if `dolt` is absent.
    #[test]
    fn panel_orders_roundtrip_when_dolt_available() {
        if find_on_path("dolt").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("lob-panel-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = PanelOrders::open(&root).expect("open");

        let id = store
            .create(
                "crossfader-v1",
                "/tmp/crossfader.dxf",
                Some("sendcutsend"),
                None,
            )
            .expect("create");
        assert!(id >= 0);

        let latest = store
            .latest("crossfader-v1")
            .expect("latest")
            .expect("present");
        assert_eq!(latest.module, "crossfader-v1");
        assert_eq!(latest.dxf_path, "/tmp/crossfader.dxf");
        assert_eq!(latest.vendor.as_deref(), Some("sendcutsend"));
        assert_eq!(latest.status, PanelOrderStatus::NotOrdered);

        store
            .mark_ordered("crossfader-v1", "sendcutsend", Some("TRK-12345"))
            .expect("mark ordered");

        let ordered = store
            .latest("crossfader-v1")
            .expect("latest")
            .expect("present");
        assert_eq!(ordered.status, PanelOrderStatus::Ordered);
        assert_eq!(ordered.tracking_ref.as_deref(), Some("TRK-12345"));

        let all = store.list("crossfader-v1").expect("list");
        assert_eq!(all.len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }
}
