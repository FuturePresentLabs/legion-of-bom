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
  PRIMARY KEY (mpn, step_order));";

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
}
