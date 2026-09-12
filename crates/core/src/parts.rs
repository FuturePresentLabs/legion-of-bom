//! The global, Dolt-backed parts library — verified part definitions keyed by
//! MPN. DESIGN.md 2.6, 3.5; MCP.md 1.2.
//!
//! This answers "is this part definition trustworthy" (pinout, ratings, and —
//! via [`crate::spice`]/later tasks — SPICE models), never pricing/stock/qty
//! (that's BOM's job, layered on top). It is *global* and cross-project on
//! purpose: once a part is verified it stays verified for every future project.
//!
//! Storage is a Dolt repository, version-controlled like git (each write can be
//! committed, diffed, reverted). We drive the `dolt` CLI directly (shelling out,
//! the same pattern as the SKiDL/ngspice/KiCad stages) rather than running a SQL
//! server — simplest correct thing for a local-first tool; a `dolt sql-server` +
//! prepared statements is the upgrade path if throughput ever demands it.

use std::path::PathBuf;
use std::process::Command;

use crate::source::CircuitSource;
use crate::tools::find_on_path;

/// The parts-library schema. `mpn` is the natural key across all three tables
/// (MCP.md 1.2 uses a surrogate `id`; the MPN is the real identity and keeps the
/// shell-out layer simple — no id juggling).
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS parts (\
  mpn VARCHAR(64) PRIMARY KEY,\
  manufacturer VARCHAR(128),\
  datasheet_url TEXT,\
  fetched_at DATETIME,\
  verified_by_human BOOLEAN NOT NULL DEFAULT FALSE,\
  verified_at DATETIME,\
  verified_by VARCHAR(64),\
  image_url TEXT);\
CREATE TABLE IF NOT EXISTS part_pins (\
  mpn VARCHAR(64) NOT NULL,\
  pin_number VARCHAR(8) NOT NULL,\
  pin_name VARCHAR(64),\
  cited_page INT,\
  PRIMARY KEY (mpn, pin_number));\
CREATE TABLE IF NOT EXISTS part_ratings (\
  mpn VARCHAR(64) NOT NULL,\
  rating_name VARCHAR(64) NOT NULL,\
  value TEXT,\
  unit VARCHAR(16),\
  cited_page INT,\
  PRIMARY KEY (mpn, rating_name));\
CREATE TABLE IF NOT EXISTS part_assembly_steps (\
  mpn VARCHAR(64) NOT NULL,\
  step_order INT NOT NULL,\
  text TEXT,\
  PRIMARY KEY (mpn, step_order));\
CREATE TABLE IF NOT EXISTS part_cutouts (\
  mpn VARCHAR(64) PRIMARY KEY,\
  kind VARCHAR(16) NOT NULL,\
  shape VARCHAR(16) NOT NULL,\
  bore_diameter_mm REAL,\
  rect_w_mm REAL,\
  rect_h_mm REAL,\
  corner_radius_mm REAL,\
  anti_rotation VARCHAR(16),\
  body_diameter_mm REAL,\
  body_depth_mm REAL,\
  cited_page INT,\
  cited_source TEXT);";

/// How a part mounts to a panel (okm.14) — mirrors [`crate::panel::ControlKind`]
/// without a cross-module type dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutoutKind {
    Jack,
    Pot,
    Switch,
    Led,
}

impl CutoutKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CutoutKind::Jack => "jack",
            CutoutKind::Pot => "pot",
            CutoutKind::Switch => "switch",
            CutoutKind::Led => "led",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "jack" => CutoutKind::Jack,
            "pot" => CutoutKind::Pot,
            "switch" => CutoutKind::Switch,
            "led" => CutoutKind::Led,
            _ => return None,
        })
    }
}

/// The panel opening a part's cutout shape needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CutoutGeometry {
    /// A round bore (LED, pot bushing, jack barrel).
    Circle { diameter_mm: f64 },
    /// A rounded rectangle (rectangular jacks, some switches).
    RoundedRect {
        width_mm: f64,
        height_mm: f64,
        corner_radius_mm: f64,
    },
}

/// How a part is kept from rotating in its panel hole (pots mainly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntiRotation {
    FlatShaft,
    NotchedShaft,
    DShaft,
}

impl AntiRotation {
    pub fn as_str(self) -> &'static str {
        match self {
            AntiRotation::FlatShaft => "flat",
            AntiRotation::NotchedShaft => "notch",
            AntiRotation::DShaft => "dshaft",
        }
    }
}

/// A part's panel/enclosure mechanical data (okm.14): the opening its panel
/// mount needs, the body envelope for crowding checks, and the mounting depth.
/// Travels with the part like its pinout — the generator never special-cases
/// per part. Consumed through [`crate::panel::CutoutSource`].
#[derive(Debug, Clone, PartialEq)]
pub struct CutoutRecord {
    pub mpn: String,
    pub kind: CutoutKind,
    pub shape: CutoutGeometry,
    pub anti_rotation: Option<AntiRotation>,
    /// Body/knob envelope diameter (mm) — spacing/crowding checks.
    pub body_diameter_mm: Option<f64>,
    /// How deep below the panel surface the body extends (mm).
    pub body_depth_mm: Option<f64>,
    /// Citation: where this geometry came from (datasheet page / measured).
    pub cited_page: Option<i64>,
    pub cited_source: Option<String>,
}

/// One pin of a part, with a citation back to the source page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinRecord {
    pub pin_number: String,
    pub pin_name: String,
    pub cited_page: Option<i64>,
}

/// One absolute-max / parametric rating, with a citation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RatingRecord {
    pub name: String,
    pub value: String,
    pub unit: Option<String>,
    pub cited_page: Option<i64>,
}

/// A part record in the library, keyed by MPN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartRecord {
    pub mpn: String,
    pub manufacturer: Option<String>,
    /// Datasheet URL — from a distributor API, never a free-form web search.
    pub datasheet_url: Option<String>,
    /// Gates real use: `layout`/`generate_bom` refuse unverified parts.
    pub verified_by_human: bool,
    pub verified_by: Option<String>,
    /// Product-photo URL (or `file://` local path) for the Visual BOM — the
    /// durable per-MPN cache for parts a distributor auto-lookup can't cover
    /// (boutique DIY jacks/pots). Set via `lob parts set-image`.
    pub image_url: Option<String>,
    /// Ordered part-specific assembly notes shown in the build guide, overriding
    /// the generic per-kind copy — e.g. "snap off the locating tab if unused" for
    /// a particular pot. Travels with the part like its pinout. Set via
    /// `lob parts set-assembly`.
    pub assembly_steps: Vec<String>,
    pub pins: Vec<PinRecord>,
    pub ratings: Vec<RatingRecord>,
}

impl PartRecord {
    /// A fresh, unverified part with just an MPN.
    pub fn new(mpn: impl Into<String>) -> Self {
        PartRecord {
            mpn: mpn.into(),
            manufacturer: None,
            datasheet_url: None,
            verified_by_human: false,
            verified_by: None,
            image_url: None,
            assembly_steps: Vec::new(),
            pins: Vec::new(),
            ratings: Vec::new(),
        }
    }
}

/// Errors from parts-library operations.
#[derive(Debug, thiserror::Error)]
pub enum PartsError {
    #[error("`dolt` executable not found on PATH")]
    DoltNotFound,
    #[error("dolt {context} failed (exit {code}): {stderr}")]
    Dolt {
        context: String,
        code: i32,
        stderr: String,
    },
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parsing dolt JSON output: {0}")]
    Json(#[from] serde_json::Error),
}

/// A handle to the Dolt-backed parts library at a given directory.
#[derive(Debug, Clone)]
pub struct PartsLibrary {
    root: PathBuf,
    dolt: PathBuf,
}

impl PartsLibrary {
    /// Open (initialising if needed) the parts library at `root`, ensuring the
    /// schema exists.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, PartsError> {
        let dolt = find_on_path("dolt").ok_or(PartsError::DoltNotFound)?;
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let lib = PartsLibrary { root, dolt };
        if !lib.root.join(".dolt").is_dir() {
            lib.dolt(&["init"], "init")?;
        }
        lib.sql(SCHEMA)?;
        lib.ensure_image_column()?;
        Ok(lib)
    }

    /// Migrate a pre-existing library that predates the `image_url` column. New
    /// DBs get it from `SCHEMA`; older ones are missing it, so add it when a probe
    /// select fails. Version-safe (no reliance on `ADD COLUMN IF NOT EXISTS`).
    fn ensure_image_column(&self) -> Result<(), PartsError> {
        if self.query("SELECT image_url FROM parts LIMIT 1").is_err() {
            self.sql("ALTER TABLE parts ADD COLUMN image_url TEXT")?;
        }
        Ok(())
    }

    /// Insert or fully replace a part (and its pins/ratings) atomically.
    pub fn upsert_part(&self, part: &PartRecord) -> Result<(), PartsError> {
        let mpn = sql_str(&part.mpn);
        let mut stmts = vec![
            "START TRANSACTION;".to_string(),
            format!("DELETE FROM parts WHERE mpn={mpn};"),
            format!("DELETE FROM part_pins WHERE mpn={mpn};"),
            format!("DELETE FROM part_ratings WHERE mpn={mpn};"),
            format!("DELETE FROM part_assembly_steps WHERE mpn={mpn};"),
            format!(
                "INSERT INTO parts (mpn, manufacturer, datasheet_url, verified_by_human, verified_by, image_url) \
                 VALUES ({mpn}, {}, {}, {}, {}, {});",
                sql_opt(part.manufacturer.as_deref()),
                sql_opt(part.datasheet_url.as_deref()),
                sql_bool(part.verified_by_human),
                sql_opt(part.verified_by.as_deref()),
                sql_opt(part.image_url.as_deref()),
            ),
        ];
        for pin in &part.pins {
            stmts.push(format!(
                "INSERT INTO part_pins (mpn, pin_number, pin_name, cited_page) VALUES ({mpn}, {}, {}, {});",
                sql_str(&pin.pin_number),
                sql_str(&pin.pin_name),
                sql_int(pin.cited_page),
            ));
        }
        for rating in &part.ratings {
            stmts.push(format!(
                "INSERT INTO part_ratings (mpn, rating_name, value, unit, cited_page) VALUES ({mpn}, {}, {}, {}, {});",
                sql_str(&rating.name),
                sql_str(&rating.value),
                sql_opt(rating.unit.as_deref()),
                sql_int(rating.cited_page),
            ));
        }
        for (i, step) in part.assembly_steps.iter().enumerate() {
            stmts.push(format!(
                "INSERT INTO part_assembly_steps (mpn, step_order, text) VALUES ({mpn}, {}, {});",
                i as i64,
                sql_str(step),
            ));
        }
        stmts.push("COMMIT;".to_string());
        self.sql(&stmts.join("\n"))
    }

    /// Fetch a part by MPN, with its pins and ratings, or `None` if absent.
    pub fn get_part(&self, mpn: &str) -> Result<Option<PartRecord>, PartsError> {
        let key = sql_str(mpn);
        let rows = self.query(&format!(
            "SELECT mpn, manufacturer, datasheet_url, verified_by_human, verified_by, image_url FROM parts WHERE mpn={key}"
        ))?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };

        let pins = self
            .query(&format!(
                "SELECT pin_number, pin_name, cited_page FROM part_pins WHERE mpn={key} ORDER BY pin_number"
            ))?
            .into_iter()
            .map(|r| PinRecord {
                pin_number: str_field(&r, "pin_number").unwrap_or_default(),
                pin_name: str_field(&r, "pin_name").unwrap_or_default(),
                cited_page: int_field(&r, "cited_page"),
            })
            .collect();

        let ratings = self
            .query(&format!(
                "SELECT rating_name, value, unit, cited_page FROM part_ratings WHERE mpn={key} ORDER BY rating_name"
            ))?
            .into_iter()
            .map(|r| RatingRecord {
                name: str_field(&r, "rating_name").unwrap_or_default(),
                value: str_field(&r, "value").unwrap_or_default(),
                unit: str_field(&r, "unit"),
                cited_page: int_field(&r, "cited_page"),
            })
            .collect();

        let assembly_steps = self
            .query(&format!(
                "SELECT text FROM part_assembly_steps WHERE mpn={key} ORDER BY step_order"
            ))?
            .into_iter()
            .filter_map(|r| str_field(&r, "text"))
            .collect();

        Ok(Some(PartRecord {
            mpn: str_field(&row, "mpn").unwrap_or_else(|| mpn.to_string()),
            manufacturer: str_field(&row, "manufacturer"),
            datasheet_url: str_field(&row, "datasheet_url"),
            verified_by_human: bool_field(&row, "verified_by_human"),
            verified_by: str_field(&row, "verified_by"),
            image_url: str_field(&row, "image_url"),
            assembly_steps,
            pins,
            ratings,
        }))
    }

    /// All MPNs in the library, sorted.
    pub fn list_mpns(&self) -> Result<Vec<String>, PartsError> {
        Ok(self
            .query("SELECT mpn FROM parts ORDER BY mpn")?
            .into_iter()
            .filter_map(|r| str_field(&r, "mpn"))
            .collect())
    }

    /// Set (or clear, with `None`) a part's product-photo URL. Creates a minimal
    /// stub row if the MPN isn't in the library yet — a boutique part we only have
    /// a photo for is still worth caching, and doesn't touch its verified status.
    pub fn set_image_url(&self, mpn: &str, image_url: Option<&str>) -> Result<(), PartsError> {
        self.sql(&format!(
            "INSERT INTO parts (mpn, image_url) VALUES ({}, {}) \
             ON DUPLICATE KEY UPDATE image_url={};",
            sql_str(mpn),
            sql_opt(image_url),
            sql_opt(image_url),
        ))
    }

    /// Replace a part's ordered assembly notes (empty clears them). Creates a
    /// minimal stub row if the MPN is new — a boutique part we only know a build
    /// tip for is still worth recording — without touching its verified status.
    pub fn set_assembly_steps(&self, mpn: &str, steps: &[String]) -> Result<(), PartsError> {
        let key = sql_str(mpn);
        let mut stmts = vec![
            "START TRANSACTION;".to_string(),
            format!("INSERT IGNORE INTO parts (mpn) VALUES ({key});"),
            format!("DELETE FROM part_assembly_steps WHERE mpn={key};"),
        ];
        for (i, step) in steps.iter().enumerate() {
            stmts.push(format!(
                "INSERT INTO part_assembly_steps (mpn, step_order, text) VALUES ({key}, {}, {});",
                i as i64,
                sql_str(step),
            ));
        }
        stmts.push("COMMIT;".to_string());
        self.sql(&stmts.join("\n"))
    }

    /// Mark a part human-verified (the gate other stages check).
    pub fn mark_verified(&self, mpn: &str, by: &str) -> Result<(), PartsError> {
        self.sql(&format!(
            "UPDATE parts SET verified_by_human=TRUE, verified_by={}, verified_at=NOW() WHERE mpn={};",
            sql_str(by),
            sql_str(mpn)
        ))
    }

    /// Store a part's panel/enclosure mechanical data (okm.14). Creates a
    /// minimal stub row if the MPN is new — mechanical data may be the only
    /// thing known about a boutique part — without touching verified status.
    pub fn set_cutout(&self, cutout: &CutoutRecord) -> Result<(), PartsError> {
        let key = sql_str(&cutout.mpn);
        let (_, bore, w, h, r) = match cutout.shape {
            CutoutGeometry::Circle { diameter_mm } => {
                ("circle", Some(diameter_mm), None, None, None)
            }
            CutoutGeometry::RoundedRect {
                width_mm,
                height_mm,
                corner_radius_mm,
            } => (
                "rect",
                None,
                Some(width_mm),
                Some(height_mm),
                Some(corner_radius_mm),
            ),
        };
        let stmts = [
            format!("INSERT IGNORE INTO parts (mpn) VALUES ({key});"),
            format!(
                "INSERT INTO part_cutouts (mpn, kind, shape, bore_diameter_mm, rect_w_mm, rect_h_mm, \
                 corner_radius_mm, anti_rotation, body_diameter_mm, body_depth_mm, cited_page, cited_source) \
                 VALUES ({key}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}) \
                 ON DUPLICATE KEY UPDATE kind=VALUES(kind), shape=VALUES(shape), \
                 bore_diameter_mm=VALUES(bore_diameter_mm), rect_w_mm=VALUES(rect_w_mm), \
                 rect_h_mm=VALUES(rect_h_mm), corner_radius_mm=VALUES(corner_radius_mm), \
                 anti_rotation=VALUES(anti_rotation), body_diameter_mm=VALUES(body_diameter_mm), \
                 body_depth_mm=VALUES(body_depth_mm), cited_page=VALUES(cited_page), \
                 cited_source=VALUES(cited_source);",
                sql_str(cutout.kind.as_str()),
                sql_str(match cutout.shape {
                    CutoutGeometry::Circle { .. } => "circle",
                    CutoutGeometry::RoundedRect { .. } => "rect",
                }),
                sql_opt(bore.map(|v| v.to_string()).as_deref()),
                sql_opt(w.map(|v| v.to_string()).as_deref()),
                sql_opt(h.map(|v| v.to_string()).as_deref()),
                sql_opt(r.map(|v| v.to_string()).as_deref()),
                sql_opt(cutout.anti_rotation.map(|a| a.as_str())),
                sql_opt(cutout.body_diameter_mm.map(|v| v.to_string()).as_deref()),
                sql_opt(cutout.body_depth_mm.map(|v| v.to_string()).as_deref()),
                sql_int(cutout.cited_page),
                sql_opt(cutout.cited_source.as_deref()),
            ),
        ];
        self.sql(&stmts.join("\n"))
    }

    /// Fetch a part's mechanical cutout, or `None` if absent.
    pub fn get_cutout(&self, mpn: &str) -> Result<Option<CutoutRecord>, PartsError> {
        let rows = self.query(&format!(
            "SELECT kind, shape, bore_diameter_mm, rect_w_mm, rect_h_mm, corner_radius_mm, \
             anti_rotation, body_diameter_mm, body_depth_mm, cited_page, cited_source \
             FROM part_cutouts WHERE mpn={}",
            sql_str(mpn)
        ))?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let Some(kind) = str_field(&row, "kind").and_then(|s| CutoutKind::parse(&s)) else {
            return Ok(None);
        };
        let shape = match str_field(&row, "shape").as_deref() {
            Some("circle") => float_field(&row, "bore_diameter_mm")
                .map(|d| CutoutGeometry::Circle { diameter_mm: d }),
            Some("rect") => {
                let (w, h) = (
                    float_field(&row, "rect_w_mm"),
                    float_field(&row, "rect_h_mm"),
                );
                match (w, h) {
                    (Some(w), Some(h)) => Some(CutoutGeometry::RoundedRect {
                        width_mm: w,
                        height_mm: h,
                        corner_radius_mm: float_field(&row, "corner_radius_mm").unwrap_or(0.0),
                    }),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(shape) = shape else {
            return Ok(None);
        };
        Ok(Some(CutoutRecord {
            mpn: mpn.to_string(),
            kind,
            shape,
            anti_rotation: str_field(&row, "anti_rotation").and_then(|s| match s.as_str() {
                "flat" => Some(AntiRotation::FlatShaft),
                "notch" => Some(AntiRotation::NotchedShaft),
                "dshaft" => Some(AntiRotation::DShaft),
                _ => None,
            }),
            body_diameter_mm: float_field(&row, "body_diameter_mm"),
            body_depth_mm: float_field(&row, "body_depth_mm"),
            cited_page: int_field(&row, "cited_page"),
            cited_source: str_field(&row, "cited_source"),
        }))
    }

    /// Commit the current state to Dolt history (no-op if nothing changed).
    pub fn commit(&self, message: &str) -> Result<(), PartsError> {
        self.dolt(&["add", "-A"], "add")?;
        let output = Command::new(&self.dolt)
            .current_dir(&self.root)
            .args(["commit", "-m", message])
            .output()?;
        if output.status.success() {
            return Ok(());
        }
        // A commit with no staged changes is not an error for our purposes.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("nothing to commit") || stderr.contains("no changes") {
            Ok(())
        } else {
            Err(PartsError::Dolt {
                context: "commit".into(),
                code: output.status.code().unwrap_or(-1),
                stderr: stderr.trim().to_string(),
            })
        }
    }

    // ---- dolt plumbing -------------------------------------------------

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

    /// Run a query and return its rows as JSON objects.
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

/// How a circuit part resolves against the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionStatus {
    /// The part declares no MPN (a generic passive) — nothing to resolve.
    NoMpn,
    /// Has an MPN, but it isn't in the library.
    Unknown,
    /// In the library, but not yet human-verified.
    Unverified,
    /// In the library and human-verified.
    Verified,
}

/// The result of resolving one circuit part against the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartResolution {
    pub refdes: String,
    pub mpn: Option<String>,
    /// The library record, if the MPN was present and found.
    pub record: Option<PartRecord>,
    pub status: ResolutionStatus,
}

impl PartResolution {
    /// Whether this part blocks verified-only operations — it declares an MPN
    /// but that MPN isn't a human-verified library entry. This is the check the
    /// gate (okm.4) enforces before `layout`/`generate_bom` ordering.
    pub fn blocks_verified_use(&self) -> bool {
        matches!(
            self.status,
            ResolutionStatus::Unknown | ResolutionStatus::Unverified
        )
    }
}

impl PartsLibrary {
    /// Resolve every part of a circuit against the library by MPN. This is the
    /// bridge circuits cross to reach verified part data — and what BOM pricing
    /// and the verification gate build on. okm.7.
    pub fn resolve_circuit(
        &self,
        circuit: &dyn CircuitSource,
    ) -> Result<Vec<PartResolution>, PartsError> {
        let mut resolutions = Vec::with_capacity(circuit.parts().len());
        for part in circuit.parts() {
            let refdes = part.refdes.0.clone();
            let Some(mpn) = part.mpn.clone() else {
                resolutions.push(PartResolution {
                    refdes,
                    mpn: None,
                    record: None,
                    status: ResolutionStatus::NoMpn,
                });
                continue;
            };
            let record = self.get_part(&mpn)?;
            let status = match &record {
                None => ResolutionStatus::Unknown,
                Some(r) if r.verified_by_human => ResolutionStatus::Verified,
                Some(_) => ResolutionStatus::Unverified,
            };
            resolutions.push(PartResolution {
                refdes,
                mpn: Some(mpn),
                record,
                status,
            });
        }
        Ok(resolutions)
    }
}

/// A parts-library-backed [`CutoutSource`](crate::panel::CutoutSource) (okm.14):
/// a verified part's mechanical data rides with it, replacing the built-in
/// fallback table. Unknown/unverified parts fall back to
/// [`BuiltinCutouts`](crate::panel::BuiltinCutouts) so current behaviour holds
/// until the library is populated; a library entry always wins when present.
pub struct LibraryCutouts<'a> {
    pub library: &'a PartsLibrary,
    pub fallback: crate::panel::BuiltinCutouts,
}

impl CutoutKind {
    /// The matching panel control kind.
    pub fn control_kind(self) -> crate::panel::ControlKind {
        match self {
            CutoutKind::Jack => crate::panel::ControlKind::Jack,
            CutoutKind::Pot => crate::panel::ControlKind::Pot,
            CutoutKind::Switch => crate::panel::ControlKind::Switch,
            CutoutKind::Led => crate::panel::ControlKind::Led,
        }
    }
}

impl<'a> crate::panel::CutoutSource for LibraryCutouts<'a> {
    fn cutout(&self, mpn: Option<&str>, footprint: &str) -> Option<crate::panel::CutoutSpec> {
        if let Some(mpn) = mpn {
            if let Ok(Some(cutout)) = self.library.get_cutout(mpn) {
                // Only a human-verified part's geometry is trusted.
                if let Ok(Some(record)) = self.library.get_part(mpn) {
                    if record.verified_by_human {
                        let shape = match cutout.shape {
                            CutoutGeometry::Circle { diameter_mm } => {
                                crate::panel::CutoutShape::Circle { diameter_mm }
                            }
                            CutoutGeometry::RoundedRect {
                                width_mm,
                                height_mm,
                                corner_radius_mm,
                            } => crate::panel::CutoutShape::RoundedRect {
                                width_mm,
                                height_mm,
                                corner_radius_mm,
                            },
                        };
                        return Some(crate::panel::CutoutSpec {
                            shape,
                            kind: cutout.kind.control_kind(),
                        });
                    }
                }
            }
        }
        self.fallback.cutout(mpn, footprint)
    }
}

/// The default cross-project parts-library location (override with `LOB_PARTS_DIR`).
pub fn default_parts_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOB_PARTS_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("legion-of-bom").join("parts")
}

// ---- SQL literal helpers (careful escaping for the shell-out layer) ----

/// A SQL string literal: wrap in single quotes, double any internal quote.
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn sql_opt(s: Option<&str>) -> String {
    s.map(sql_str).unwrap_or_else(|| "NULL".to_string())
}

fn sql_int(n: Option<i64>) -> String {
    n.map(|n| n.to_string())
        .unwrap_or_else(|| "NULL".to_string())
}

fn sql_bool(b: bool) -> String {
    if b { "TRUE" } else { "FALSE" }.to_string()
}

// ---- JSON field extraction (Dolt renders NULL as absent/null, bool as 0/1) ----

fn str_field(row: &serde_json::Value, key: &str) -> Option<String> {
    row.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn int_field(row: &serde_json::Value, key: &str) -> Option<i64> {
    row.get(key).and_then(serde_json::Value::as_i64)
}

fn float_field(row: &serde_json::Value, key: &str) -> Option<f64> {
    row.get(key).and_then(|v| v.as_f64())
}

fn bool_field(row: &serde_json::Value, key: &str) -> bool {
    match row.get(key) {
        Some(v) => v
            .as_bool()
            .unwrap_or_else(|| v.as_i64().is_some_and(|n| n != 0)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_escaping() {
        assert_eq!(sql_str("LM13700"), "'LM13700'");
        assert_eq!(sql_str("a'b"), "'a''b'"); // apostrophe doubled
        assert_eq!(sql_opt(None), "NULL");
        assert_eq!(sql_int(Some(3)), "3");
        assert_eq!(sql_int(None), "NULL");
    }

    /// Full round-trip against a real Dolt repo. Skipped if `dolt` is absent.
    #[test]
    fn roundtrip_when_dolt_available() {
        if find_on_path("dolt").is_none() {
            return; // no dolt in this environment — integration test skipped
        }
        let root = std::env::temp_dir().join(format!("lob-parts-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let lib = PartsLibrary::open(&root).expect("open");

        let mut part = PartRecord::new("LM13700");
        part.manufacturer = Some("Texas Instruments".into());
        part.pins = vec![
            PinRecord {
                pin_number: "1".into(),
                pin_name: "AMP BIAS INPUT".into(),
                cited_page: Some(3),
            },
            PinRecord {
                pin_number: "2".into(),
                pin_name: "DIODE BIAS".into(),
                cited_page: Some(3),
            },
        ];
        part.ratings = vec![RatingRecord {
            name: "Vcc_max".into(),
            value: "18".into(),
            unit: Some("V".into()),
            cited_page: Some(2),
        }];
        lib.upsert_part(&part).expect("upsert");

        part.image_url = Some("https://x/lm13700.jpg".into());
        part.assembly_steps = vec!["Use a socket.".into(), "Match the notch to pin 1.".into()];
        lib.upsert_part(&part)
            .expect("re-upsert with image + assembly");

        let got = lib.get_part("LM13700").expect("get").expect("present");
        assert_eq!(got.manufacturer.as_deref(), Some("Texas Instruments"));
        assert_eq!(got.image_url.as_deref(), Some("https://x/lm13700.jpg"));
        assert_eq!(
            got.assembly_steps,
            vec!["Use a socket.", "Match the notch to pin 1."]
        );
        assert_eq!(got.pins.len(), 2);
        assert_eq!(got.pins[0].pin_name, "AMP BIAS INPUT");
        assert_eq!(got.ratings.len(), 1);
        assert!(!got.verified_by_human);

        lib.mark_verified("LM13700", "avery").expect("verify");
        let verified = lib.get_part("LM13700").expect("get").expect("present");
        assert!(verified.verified_by_human);
        assert_eq!(verified.verified_by.as_deref(), Some("avery"));

        assert_eq!(lib.list_mpns().expect("list"), vec!["LM13700"]);
        assert!(lib.get_part("NONEXISTENT").expect("get").is_none());

        // set_image_url / set_assembly_steps on a new MPN create a stub row.
        lib.set_image_url("PJ398SM", Some("file:///photos/thonkiconn.jpg"))
            .expect("set image on new mpn");
        lib.set_assembly_steps(
            "PJ398SM",
            &["Fit the nut on the front of the panel.".into()],
        )
        .expect("set assembly on new mpn");
        let jack = lib.get_part("PJ398SM").expect("get").expect("stub present");
        assert_eq!(
            jack.image_url.as_deref(),
            Some("file:///photos/thonkiconn.jpg")
        );
        assert_eq!(
            jack.assembly_steps,
            vec!["Fit the nut on the front of the panel."]
        );
        assert!(!jack.verified_by_human); // a photo/tip doesn't imply verification

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Resolve a circuit's parts by MPN against the library. Skipped if no dolt.
    #[test]
    fn resolve_circuit_by_mpn_when_dolt_available() {
        use crate::model::{Circuit, Part};

        if find_on_path("dolt").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("lob-resolve-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let lib = PartsLibrary::open(&root).expect("open");

        lib.upsert_part(&PartRecord::new("LM13700"))
            .expect("upsert verified");
        lib.mark_verified("LM13700", "tester").expect("verify");
        lib.upsert_part(&PartRecord::new("TL072"))
            .expect("upsert unverified");

        let circuit = Circuit {
            name: "c".into(),
            parts: vec![
                Part::new("U1", "x").with_mpn("LM13700"), // verified
                Part::new("U2", "x").with_mpn("TL072"),   // unverified
                Part::new("U3", "x").with_mpn("FOO999"),  // unknown
                Part::new("R1", "1k"),                    // no MPN
            ],
            nets: vec![],
        };
        let res = lib.resolve_circuit(&circuit).expect("resolve");
        let status = |rd: &str| res.iter().find(|r| r.refdes == rd).unwrap().status;
        assert_eq!(status("U1"), ResolutionStatus::Verified);
        assert_eq!(status("U2"), ResolutionStatus::Unverified);
        assert_eq!(status("U3"), ResolutionStatus::Unknown);
        assert_eq!(status("R1"), ResolutionStatus::NoMpn);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Mechanical cutout data round-trips and gates through verification
    /// (okm.14). Skipped if no dolt.
    #[test]
    fn cutout_roundtrip_and_verified_source_when_dolt_available() {
        use crate::model::{Circuit, Part};
        use crate::panel::{ControlKind, CutoutShape, CutoutSource as _};

        if find_on_path("dolt").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("lob-cutout-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let lib = PartsLibrary::open(&root).expect("open");

        let record = CutoutRecord {
            mpn: "WQP-PJ398SM".into(),
            kind: CutoutKind::Jack,
            shape: CutoutGeometry::RoundedRect {
                width_mm: 6.2,
                height_mm: 5.4,
                corner_radius_mm: 0.4,
            },
            anti_rotation: None,
            body_diameter_mm: Some(13.0),
            body_depth_mm: Some(12.5),
            cited_page: Some(3),
            cited_source: Some("manufacturer datasheet".into()),
        };
        lib.set_cutout(&record).expect("set cutout");
        let got = lib
            .get_cutout("WQP-PJ398SM")
            .expect("get")
            .expect("present");
        assert_eq!(got, record);

        // The library-backed CutoutSource: geometry only for VERIFIED parts,
        // builtin fallback otherwise.
        lib.upsert_part(&PartRecord::new("WQP-PJ398SM"))
            .expect("stub part");
        let source = LibraryCutouts {
            library: &lib,
            fallback: crate::panel::BuiltinCutouts,
        };
        let circuit = Circuit {
            name: "c".into(),
            parts: vec![Part::new("J1", "Thonkiconn")
                .with_mpn("WQP-PJ398SM")
                .with_footprint("Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical")],
            nets: vec![],
        };
        let part = &circuit.parts[0];
        // Unverified → falls back to the builtin jack bore.
        let via_builtin = source
            .cutout(part.mpn.as_deref(), part.footprint.as_deref().unwrap_or(""))
            .expect("fallback");
        assert_eq!(
            via_builtin.shape,
            CutoutShape::Circle {
                diameter_mm: crate::panel::JACK_BARREL_MM,
            }
        );
        // Verified → library geometry wins.
        lib.mark_verified("WQP-PJ398SM", "tester").expect("verify");
        let via_lib = source
            .cutout(part.mpn.as_deref(), part.footprint.as_deref().unwrap_or(""))
            .expect("library");
        assert_eq!(
            via_lib.shape,
            CutoutShape::RoundedRect {
                width_mm: 6.2,
                height_mm: 5.4,
                corner_radius_mm: 0.4,
            }
        );
        assert_eq!(via_lib.kind, ControlKind::Jack);

        let _ = std::fs::remove_dir_all(&root);
    }
}
