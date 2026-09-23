//! The global, SQLite-backed parts library — verified part definitions keyed by
//! MPN. DESIGN.md 2.6, 3.5; MCP.md 1.2.
//!
//! This answers "is this part definition trustworthy" (pinout, ratings, and —
//! via [`crate::spice`]/later tasks — SPICE models), never pricing/stock/qty
//! (that's BOM's job, layered on top). It is *global* and cross-project on
//! purpose: once a part is verified it stays verified for every future project.
//!
//! Storage is a single SQLite file via `sqlx` — no version history (a write
//! simply overwrites what was there), no server process, nothing to install.
//! The public API stays synchronous (every other stage in this crate is): each
//! call runs its query against a small dedicated tokio runtime and blocks on
//! the result, so callers never see `async`/`.await`.
//!
//! [`PartsError::DoltNotFound`] and [`PartsError::Dolt`] are unused here — they
//! remain because [`crate::panel::PanelOrders`] still shells out to Dolt and
//! shares this error type. That store is a separate, deliberately unmigrated
//! concern.

use std::path::PathBuf;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};

use crate::source::CircuitSource;

/// The parts-library schema. `mpn` is the natural key across all three tables
/// (MCP.md 1.2 uses a surrogate `id`; the MPN is the real identity and keeps
/// queries simple — no id juggling).
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS parts (
  mpn TEXT PRIMARY KEY,
  manufacturer TEXT,
  datasheet_url TEXT,
  fetched_at TEXT,
  verified_by_human INTEGER NOT NULL DEFAULT 0,
  verified_at TEXT,
  verified_by TEXT,
  image_url TEXT
);
CREATE TABLE IF NOT EXISTS part_pins (
  mpn TEXT NOT NULL,
  pin_number TEXT NOT NULL,
  pin_name TEXT,
  cited_page INTEGER,
  PRIMARY KEY (mpn, pin_number)
);
CREATE TABLE IF NOT EXISTS part_ratings (
  mpn TEXT NOT NULL,
  rating_name TEXT NOT NULL,
  value TEXT,
  unit TEXT,
  cited_page INTEGER,
  PRIMARY KEY (mpn, rating_name)
);
CREATE TABLE IF NOT EXISTS part_assembly_steps (
  mpn TEXT NOT NULL,
  step_order INTEGER NOT NULL,
  text TEXT,
  PRIMARY KEY (mpn, step_order)
);
CREATE TABLE IF NOT EXISTS house_parts (
  kind TEXT NOT NULL,
  value TEXT NOT NULL,
  package TEXT NOT NULL,
  mpn TEXT NOT NULL,
  uses INTEGER NOT NULL DEFAULT 1,
  seen_on TEXT,
  photo TEXT,
  PRIMARY KEY (kind, value, package)
);";

/// A part we actually build with: what we reach for given a kind, a value and a
/// package, learned from boards that were really manufactured.
#[derive(Debug, Clone, PartialEq)]
pub struct HousePart {
    pub kind: String,
    pub value: String,
    pub package: String,
    pub mpn: String,
    /// How many boards we have seen it on — the tie-breaker between choices.
    pub uses: i64,
    /// Which boards, so a choice can be traced to something that shipped.
    pub seen_on: String,
    /// Our own photo of the part, if we have taken one — a repo-relative path
    /// or a URL. Worth more than a datasheet render for telling two similar
    /// jacks apart on the bench.
    pub photo: Option<String>,
    /// Whether this matched value *and* package, or was a broader fallback.
    pub exact: bool,
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
    /// Unused by [`PartsLibrary`] itself — kept for [`crate::panel::PanelOrders`],
    /// which still shells out to Dolt and shares this error type.
    #[error("`dolt` executable not found on PATH")]
    DoltNotFound,
    /// Unused by [`PartsLibrary`] itself — see [`PartsError::DoltNotFound`].
    #[error("dolt {context} failed (exit {code}): {stderr}")]
    Dolt {
        context: String,
        code: i32,
        stderr: String,
    },
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Unused by [`PartsLibrary`] itself — see [`PartsError::DoltNotFound`].
    #[error("parsing dolt JSON output: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// A handle to the SQLite-backed parts library at a given directory.
pub struct PartsLibrary {
    pool: SqlitePool,
    // A dedicated single-thread runtime so this otherwise-synchronous API can
    // drive `sqlx`'s async queries without forcing async on every caller
    // (`board`/`bom`/the CLI are all plain synchronous code).
    rt: tokio::runtime::Runtime,
}

impl PartsLibrary {
    /// Open (initialising if needed) the parts library at `root`, ensuring the
    /// schema exists.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, PartsError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let db_path = root.join("parts.sqlite");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let pool = rt.block_on(async {
            let opts = SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await?;
            sqlx::raw_sql(SCHEMA).execute(&pool).await?;
            Ok::<_, sqlx::Error>(pool)
        })?;

        Ok(PartsLibrary { pool, rt })
    }

    /// Run an async query against `self.pool` synchronously.
    fn block<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    /// Attach our own photo of a part we build with.
    ///
    /// Returns false when no such library entry exists, so a typo in the key is
    /// reported rather than silently storing a photo nothing points at.
    pub fn set_house_photo(
        &self,
        kind: &str,
        value: &str,
        package: &str,
        photo: &str,
    ) -> Result<bool, PartsError> {
        self.block(async {
            let result = sqlx::query(
                "UPDATE house_parts SET photo = ? WHERE kind = ? AND value = ? AND package = ?",
            )
            .bind(photo)
            .bind(kind)
            .bind(value)
            .bind(package)
            .execute(&self.pool)
            .await?;
            Ok(result.rows_affected() > 0)
        })
    }

    /// Insert or fully replace a part (and its pins/ratings) atomically.
    pub fn upsert_part(&self, part: &PartRecord) -> Result<(), PartsError> {
        self.block(async {
            let mut tx = self.pool.begin().await?;
            sqlx::query("DELETE FROM parts WHERE mpn = ?")
                .bind(&part.mpn)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM part_pins WHERE mpn = ?")
                .bind(&part.mpn)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM part_ratings WHERE mpn = ?")
                .bind(&part.mpn)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM part_assembly_steps WHERE mpn = ?")
                .bind(&part.mpn)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO parts (mpn, manufacturer, datasheet_url, verified_by_human, verified_by, image_url) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&part.mpn)
            .bind(&part.manufacturer)
            .bind(&part.datasheet_url)
            .bind(part.verified_by_human)
            .bind(&part.verified_by)
            .bind(&part.image_url)
            .execute(&mut *tx)
            .await?;
            for pin in &part.pins {
                sqlx::query(
                    "INSERT INTO part_pins (mpn, pin_number, pin_name, cited_page) VALUES (?, ?, ?, ?)",
                )
                .bind(&part.mpn)
                .bind(&pin.pin_number)
                .bind(&pin.pin_name)
                .bind(pin.cited_page)
                .execute(&mut *tx)
                .await?;
            }
            for rating in &part.ratings {
                sqlx::query(
                    "INSERT INTO part_ratings (mpn, rating_name, value, unit, cited_page) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(&part.mpn)
                .bind(&rating.name)
                .bind(&rating.value)
                .bind(&rating.unit)
                .bind(rating.cited_page)
                .execute(&mut *tx)
                .await?;
            }
            for (i, step) in part.assembly_steps.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO part_assembly_steps (mpn, step_order, text) VALUES (?, ?, ?)",
                )
                .bind(&part.mpn)
                .bind(i as i64)
                .bind(step)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            Ok(())
        })
    }

    /// Fetch a part by MPN, with its pins and ratings, or `None` if absent.
    pub fn get_part(&self, mpn: &str) -> Result<Option<PartRecord>, PartsError> {
        self.block(async {
            let row = sqlx::query(
                "SELECT mpn, manufacturer, datasheet_url, verified_by_human, verified_by, image_url \
                 FROM parts WHERE mpn = ?",
            )
            .bind(mpn)
            .fetch_optional(&self.pool)
            .await?;
            let Some(row) = row else {
                return Ok(None);
            };

            let pins = sqlx::query(
                "SELECT pin_number, pin_name, cited_page FROM part_pins WHERE mpn = ? ORDER BY pin_number",
            )
            .bind(mpn)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| PinRecord {
                pin_number: r.get("pin_number"),
                pin_name: r.get::<Option<String>, _>("pin_name").unwrap_or_default(),
                cited_page: r.get("cited_page"),
            })
            .collect();

            let ratings = sqlx::query(
                "SELECT rating_name, value, unit, cited_page FROM part_ratings WHERE mpn = ? ORDER BY rating_name",
            )
            .bind(mpn)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| RatingRecord {
                name: r.get("rating_name"),
                value: r.get::<Option<String>, _>("value").unwrap_or_default(),
                unit: r.get("unit"),
                cited_page: r.get("cited_page"),
            })
            .collect();

            let assembly_steps = sqlx::query(
                "SELECT text FROM part_assembly_steps WHERE mpn = ? ORDER BY step_order",
            )
            .bind(mpn)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .filter_map(|r| r.get::<Option<String>, _>("text"))
            .collect();

            Ok(Some(PartRecord {
                mpn: row.get("mpn"),
                manufacturer: row.get("manufacturer"),
                datasheet_url: row.get("datasheet_url"),
                verified_by_human: row.get("verified_by_human"),
                verified_by: row.get("verified_by"),
                image_url: row.get("image_url"),
                assembly_steps,
                pins,
                ratings,
            }))
        })
    }

    /// All MPNs in the library, sorted.
    pub fn list_mpns(&self) -> Result<Vec<String>, PartsError> {
        self.block(async {
            Ok(sqlx::query("SELECT mpn FROM parts ORDER BY mpn")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|r| r.get("mpn"))
                .collect())
        })
    }

    /// Set (or clear, with `None`) a part's product-photo URL. Creates a minimal
    /// stub row if the MPN isn't in the library yet — a boutique part we only have
    /// a photo for is still worth caching, and doesn't touch its verified status.
    pub fn set_image_url(&self, mpn: &str, image_url: Option<&str>) -> Result<(), PartsError> {
        self.block(async {
            sqlx::query(
                "INSERT INTO parts (mpn, image_url) VALUES (?, ?) \
                 ON CONFLICT(mpn) DO UPDATE SET image_url = excluded.image_url",
            )
            .bind(mpn)
            .bind(image_url)
            .execute(&self.pool)
            .await?;
            Ok(())
        })
    }

    /// Replace a part's ordered assembly notes (empty clears them). Creates a
    /// minimal stub row if the MPN is new — a boutique part we only know a build
    /// tip for is still worth recording — without touching its verified status.
    pub fn set_assembly_steps(&self, mpn: &str, steps: &[String]) -> Result<(), PartsError> {
        self.block(async {
            let mut tx = self.pool.begin().await?;
            sqlx::query("INSERT OR IGNORE INTO parts (mpn) VALUES (?)")
                .bind(mpn)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM part_assembly_steps WHERE mpn = ?")
                .bind(mpn)
                .execute(&mut *tx)
                .await?;
            for (i, step) in steps.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO part_assembly_steps (mpn, step_order, text) VALUES (?, ?, ?)",
                )
                .bind(mpn)
                .bind(i as i64)
                .bind(step)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            Ok(())
        })
    }

    /// Mark a part human-verified (the gate other stages check).
    pub fn mark_verified(&self, mpn: &str, by: &str) -> Result<(), PartsError> {
        self.block(async {
            sqlx::query(
                "UPDATE parts SET verified_by_human = 1, verified_by = ?, verified_at = CURRENT_TIMESTAMP \
                 WHERE mpn = ?",
            )
            .bind(by)
            .bind(mpn)
            .execute(&self.pool)
            .await?;
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    //  House parts — the parts we actually build with
    // -----------------------------------------------------------------------

    /// Record that a board used `mpn` for a given kind/value/package, or bump the
    /// count if we already knew.
    ///
    /// This is the other half of the library, and the more useful one day to day.
    /// `parts` describes a part we have looked up *by MPN*; `house_parts` answers
    /// the question a builder actually asks — "what do we use for a 10k 0603?" —
    /// so grabbing a jack or a pot needs no search at all. It is learned from the
    /// BOMs of boards that were really manufactured, which is a far better source
    /// than a keyword search: those parts were bought, assembled and shipped.
    pub fn learn_house_part(
        &self,
        kind: &str,
        value: &str,
        package: &str,
        mpn: &str,
        seen_on: &str,
    ) -> Result<(), PartsError> {
        self.block(async {
            sqlx::query(
                "INSERT INTO house_parts (kind, value, package, mpn, uses, seen_on) VALUES (?, ?, ?, ?, 1, ?) \
                 ON CONFLICT(kind, value, package) DO UPDATE SET \
                   uses = uses + 1, \
                   seen_on = CASE WHEN instr(seen_on, ?) > 0 THEN seen_on ELSE seen_on || ', ' || ? END",
            )
            .bind(kind)
            .bind(value)
            .bind(package)
            .bind(mpn)
            .bind(seen_on)
            .bind(seen_on)
            .bind(seen_on)
            .execute(&self.pool)
            .await?;
            Ok(())
        })
    }

    /// The part we use for a kind/value/package, most-used first.
    ///
    /// Falls back from the exact match outward, because how specific an answer
    /// needs to be depends on the part: a 10k resistor must match its value and
    /// package, but "a jack" is a jack — we only buy one kind.
    pub fn house_part(
        &self,
        kind: &str,
        value: &str,
        package: &str,
    ) -> Result<Option<HousePart>, PartsError> {
        // For a passive the value *is* the part — a 10k and a 100k are not
        // substitutes — so a match that ignores the value would quietly answer
        // the wrong resistor. There, no answer is the correct answer.
        let value_is_identity = matches!(kind, "resistor" | "capacitor" | "inductor");
        if value_is_identity && value.is_empty() {
            return Ok(None);
        }
        let value = (!value.is_empty()).then_some(value);
        let package = (!package.is_empty()).then_some(package);

        let mut steps = vec![(value, package), (value, None)];
        if !value_is_identity {
            steps.push((None, package));
            steps.push((None, None));
        }

        self.block(async {
            for (v, p) in steps {
                let row = match (v, p) {
                    (Some(v), Some(p)) => sqlx::query(
                        "SELECT kind, value, package, mpn, uses, seen_on, photo FROM house_parts \
                         WHERE kind = ? AND value = ? AND package = ? ORDER BY uses DESC LIMIT 1",
                    )
                    .bind(kind)
                    .bind(v)
                    .bind(p)
                    .fetch_optional(&self.pool)
                    .await?,
                    (Some(v), None) => sqlx::query(
                        "SELECT kind, value, package, mpn, uses, seen_on, photo FROM house_parts \
                         WHERE kind = ? AND value = ? ORDER BY uses DESC LIMIT 1",
                    )
                    .bind(kind)
                    .bind(v)
                    .fetch_optional(&self.pool)
                    .await?,
                    (None, Some(p)) => sqlx::query(
                        "SELECT kind, value, package, mpn, uses, seen_on, photo FROM house_parts \
                         WHERE kind = ? AND package = ? ORDER BY uses DESC LIMIT 1",
                    )
                    .bind(kind)
                    .bind(p)
                    .fetch_optional(&self.pool)
                    .await?,
                    (None, None) => sqlx::query(
                        "SELECT kind, value, package, mpn, uses, seen_on, photo FROM house_parts \
                         WHERE kind = ? ORDER BY uses DESC LIMIT 1",
                    )
                    .bind(kind)
                    .fetch_optional(&self.pool)
                    .await?,
                };
                if let Some(r) = row {
                    return Ok(Some(house_part_from_row(&r, v.is_some() && p.is_some())));
                }
            }
            Ok(None)
        })
    }

    /// Every house part, most-used first.
    pub fn house_parts(&self) -> Result<Vec<HousePart>, PartsError> {
        self.block(async {
            Ok(sqlx::query(
                "SELECT kind, value, package, mpn, uses, seen_on, photo FROM house_parts \
                 ORDER BY uses DESC, kind, value",
            )
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| house_part_from_row(r, true))
            .collect())
        })
    }
}

fn house_part_from_row(row: &SqliteRow, exact: bool) -> HousePart {
    HousePart {
        kind: row.get("kind"),
        value: row.get("value"),
        package: row.get("package"),
        mpn: row.get("mpn"),
        uses: row.get("uses"),
        seen_on: row.get::<Option<String>, _>("seen_on").unwrap_or_default(),
        photo: row.get("photo"),
        exact,
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

/// Where the parts library lives, in order of precedence:
///
/// 1. `LOB_PARTS_DIR`, an explicit override;
/// 2. `.lob/parts` in the nearest enclosing circuits repo — the parts a repo
///    buys are part of that repo's record, so they version with it and travel
///    to anyone who clones it;
/// 3. a user-global store, for work outside any repo.
pub fn default_parts_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOB_PARTS_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(repo) = enclosing_circuits_repo() {
        return repo.join(".lob").join("parts");
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("legion-of-bom").join("parts")
}

/// The house footprint library: `<parts dir>/footprints`, holding
/// `<Lib>.pretty/<Name>.kicad_mod` for parts KiCad does not ship.
///
/// Beside the part metadata and photos on purpose. A footprint is part data in
/// exactly the way a pinout or a product photo is, and the alternative — a
/// per-project `.pretty` — means the Dailywell toggle gets redrawn for every
/// module that uses it. `None` when the directory does not exist, so a repo
/// without one behaves exactly as before.
///
/// Generated boards *embed* their footprints, so a board still opens in KiCad
/// with no library attached; this is only needed at generation time.
pub fn house_footprint_dir() -> Option<PathBuf> {
    let dir = default_parts_dir().join("footprints");
    dir.is_dir().then_some(dir)
}

/// The nearest ancestor of the working directory holding a `lob.toml`.
///
/// A repo that already has a parts store counts too, so a library keeps
/// working after the manifest is renamed or moved.
fn enclosing_circuits_repo() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("lob.toml").is_file() || dir.join(".lob").join("parts").is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full round-trip against a real (temp-dir) SQLite-backed library.
    #[test]
    fn roundtrip() {
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

    /// Resolve a circuit's parts by MPN against the library.
    #[test]
    fn resolve_circuit_by_mpn() {
        use crate::model::{Circuit, Part};

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

    /// `house_part`'s value/package fallback ladder, and `learn_house_part`'s
    /// use-counting + provenance de-dup.
    #[test]
    fn house_parts_fallback_and_learning() {
        let root = std::env::temp_dir().join(format!("lob-house-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let lib = PartsLibrary::open(&root).expect("open");

        // A passive: value is identity, so an empty value never matches.
        assert!(lib
            .house_part("resistor", "", "0603")
            .expect("query")
            .is_none());

        lib.learn_house_part("resistor", "10k", "0603", "RC0603FR-0710KL", "board-a")
            .expect("learn");
        lib.learn_house_part("resistor", "10k", "0603", "RC0603FR-0710KL", "board-b")
            .expect("learn again bumps uses");
        lib.learn_house_part("resistor", "10k", "0603", "RC0603FR-0710KL", "board-a")
            .expect("learn same board again is a no-op on provenance");

        let hit = lib
            .house_part("resistor", "10k", "0603")
            .expect("query")
            .expect("present");
        assert_eq!(hit.mpn, "RC0603FR-0710KL");
        assert_eq!(hit.uses, 3);
        assert_eq!(hit.seen_on, "board-a, board-b");
        assert!(hit.exact);

        // A non-passive (jack) falls back to kind-only when value/package don't
        // narrow it — "a jack is a jack" if we only buy one kind.
        lib.learn_house_part("jack", "1/4in", "THT", "PJ398SM", "board-a")
            .expect("learn jack");
        let jack = lib
            .house_part("jack", "", "")
            .expect("query")
            .expect("kind-only fallback");
        assert_eq!(jack.mpn, "PJ398SM");
        assert!(!jack.exact);

        assert!(lib
            .set_house_photo("jack", "1/4in", "THT", "file:///photos/jack.jpg")
            .expect("set photo"));
        assert!(!lib
            .set_house_photo("jack", "nope", "THT", "file:///photos/jack.jpg")
            .expect("set photo on unknown key returns false"));

        let all = lib.house_parts().expect("list all");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].mpn, "RC0603FR-0710KL"); // most-used first

        let _ = std::fs::remove_dir_all(&root);
    }
}
