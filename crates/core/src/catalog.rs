//! The catalog of known-good parts — **data, not code** (legion-of-bom-uvdm).
//!
//! Circuit synthesis chooses among options; this is where the options come
//! from. Each part is one hand-curated JSON file under `catalog/parts/`,
//! reviewed in git like code, and says what the part can do and how it is
//! wired — never *which board it goes on*:
//!
//! * `provides` — the roles it can fill (`mcu`, `i2s-dac`, `ldo`, …), so "what
//!   could be the ADC?" is a query.
//! * `interfaces` — each interface's signals, mapped to a pin by **name**
//!   (`pin:BCK`), a part-local node (`node:out_l`), or — for an MCU — an
//!   **alternate function** (`{"alt": "SAI1_SCK_A"}`) whose candidate pins are
//!   read from the KiCad symbol, so a pin-binding decision's options come from
//!   data too.
//! * `support` — every tie, capacitor and resistor the part needs, each between
//!   two endpoints (`pin:`, `net:`, `node:`) and each carrying a [`Cite`]: a
//!   verbatim quote on a page of the part's pinned datasheet (checked
//!   mechanically), or a reading a person must confirm.
//!
//! [`check_symbols`] holds every pin name and alternate to the KiCad symbol;
//! [`quotes`] hands every quote to [`crate::datasheet`] for checking.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::stage::StageError;
use crate::symbols;

/// One known-good part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPart {
    pub mpn: String,
    pub manufacturer: String,
    #[serde(default)]
    pub lcsc: Option<String>,
    /// What a decider is told about the part: capabilities, not a verdict.
    pub summary: String,
    pub symbol: PartSymbol,
    pub footprint: String,
    #[serde(default)]
    pub datasheet: Option<DatasheetRef>,
    #[serde(default)]
    pub provides: Vec<String>,
    /// Numeric facts synthesis derives from (a crystal's `freq_hz`, `cl_pf`),
    /// each cited like any other fact.
    #[serde(default)]
    pub params: BTreeMap<String, Param>,
    #[serde(default)]
    pub interfaces: Vec<Interface>,
    #[serde(default)]
    pub support: Vec<Support>,
    /// Not an analog circuit SPICE models (an IC, a crystal): `Sim.Enable = 0`.
    #[serde(default)]
    pub sim_excluded: bool,
}

/// Where the part's pins come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum PartSymbol {
    /// An official KiCad symbol, `"Lib:Name"`.
    Kicad { kicad: String },
    /// No KiCad symbol: the datasheet's own pin table, each row cited.
    Inline { pins: Vec<InlinePin> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlinePin {
    pub number: String,
    pub name: String,
    /// The datasheet's I/O column: `I`, `O`, `I/O`, `power`, or `passive`.
    pub io: String,
    pub cite: Cite,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasheetRef {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    pub value: f64,
    pub cite: Cite,
}

/// One interface the part exposes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    /// `i2s`, `i2c`, `swd`, `hse`, `audio-line-in`, `audio-line-out`, …
    pub kind: String,
    /// `master`/`slave` for a bus, `source`/`sink` for a signal port,
    /// `needs` for something the part requires from outside (a crystal).
    pub role: String,
    /// The peripheral instance for an MCU interface (`SAI1`), when it has one.
    #[serde(default)]
    pub instance: Option<String>,
    pub signals: BTreeMap<String, Signal>,
    #[serde(default)]
    pub cite: Option<Cite>,
}

/// Where an interface signal lands on the part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Signal {
    /// `pin:NAME` or `node:NAME`.
    At(String),
    /// Any pin whose symbol alternates include this function.
    Alt { alt: String },
}

/// A tie, capacitor or resistor between two endpoints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Support {
    /// `pin:NAME`, `net:NAME` or `node:NAME`.
    pub between: [String; 2],
    /// `tie` (a direct connection), `C`, `CP` (polarised), or `R`.
    pub part: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub footprint: Option<String>,
    /// Repeat once for every pin carrying the `pin:` endpoint's name ("one
    /// 100 nF per VDD pin").
    #[serde(default)]
    pub each_pin: bool,
    pub cite: Cite,
}

/// Why a fact is believed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Cite {
    /// Verbatim text on a page of the part's pinned datasheet.
    Quote { page: usize, quote: String },
    /// Read off a figure, or not stated in the datasheet at all; counts only
    /// once a person confirms it.
    Reading {
        reading: String,
        #[serde(default)]
        page: Option<usize>,
        #[serde(default)]
        confirmed_by: Option<String>,
    },
}

/// An endpoint of a [`Support`] or an [`Signal::At`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Endpoint {
    Pin(String),
    Net(String),
    Node(String),
}

impl Endpoint {
    pub fn parse(s: &str) -> Result<Endpoint, String> {
        let (kind, name) = s
            .split_once(':')
            .ok_or_else(|| format!("endpoint {s:?} is not pin:/net:/node:NAME"))?;
        if name.is_empty() {
            return Err(format!("endpoint {s:?} has no name"));
        }
        match kind {
            "pin" => Ok(Endpoint::Pin(name.into())),
            "net" => Ok(Endpoint::Net(name.into())),
            "node" => Ok(Endpoint::Node(name.into())),
            _ => Err(format!("endpoint {s:?}: {kind:?} is not pin, net or node")),
        }
    }
}

/// The loaded catalog.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub parts: Vec<CatalogPart>,
}

/// A catalog that could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("reading {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
}

/// The catalog shipped with this checkout (override with `LOB_CATALOG`).
pub fn default_catalog_dir() -> PathBuf {
    std::env::var_os("LOB_CATALOG")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog/parts"))
}

impl Catalog {
    /// Load every `*.json` in `dir`, sorted by file name so the same catalog
    /// always loads the same way. A file whose name is not its part's `mpn`
    /// is rejected: the file name is how a reviewer finds the part.
    pub fn load(dir: &Path) -> Result<Catalog, CatalogError> {
        let io = |source| CatalogError::Io {
            path: dir.to_path_buf(),
            source,
        };
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(io)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        let mut parts = Vec::new();
        for path in files {
            let text = std::fs::read_to_string(&path).map_err(|source| CatalogError::Io {
                path: path.clone(),
                source,
            })?;
            let part: CatalogPart =
                serde_json::from_str(&text).map_err(|source| CatalogError::Parse {
                    path: path.clone(),
                    source,
                })?;
            let invalid = |message: String| CatalogError::Invalid {
                path: path.clone(),
                message,
            };
            if path.file_stem().and_then(|s| s.to_str()) != Some(part.mpn.as_str()) {
                return Err(invalid(format!("file name must be {}.json", part.mpn)));
            }
            let problems = part.shape_problems();
            if !problems.is_empty() {
                return Err(invalid(problems.join("; ")));
            }
            parts.push(part);
        }
        Ok(Catalog { parts })
    }

    pub fn part(&self, mpn: &str) -> Option<&CatalogPart> {
        self.parts.iter().find(|p| p.mpn == mpn)
    }

    /// Every part that can fill `role`, in catalog order.
    pub fn providing<'a>(&'a self, role: &'a str) -> impl Iterator<Item = &'a CatalogPart> + 'a {
        self.parts
            .iter()
            .filter(move |p| p.provides.iter().any(|r| r == role))
    }
}

impl CatalogPart {
    /// Problems visible in the file alone, before any symbol or datasheet is
    /// read: malformed endpoints, a quote with no datasheet to be on, a
    /// support part that is not one of the four kinds.
    fn shape_problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut cites: Vec<&Cite> = Vec::new();
        for s in &self.support {
            for e in &s.between {
                if let Err(m) = Endpoint::parse(e) {
                    out.push(m);
                }
            }
            if !matches!(s.part.as_str(), "tie" | "C" | "CP" | "R") {
                out.push(format!("support part {:?} is not tie, C, CP or R", s.part));
            }
            if s.part != "tie" && s.value.is_none() {
                out.push(format!("{} between {:?} has no value", s.part, s.between));
            }
            if s.each_pin && !s.between.iter().any(|e| e.starts_with("pin:")) {
                out.push(format!(
                    "each_pin support {:?} has no pin: endpoint",
                    s.between
                ));
            }
            cites.push(&s.cite);
        }
        for i in &self.interfaces {
            for (name, sig) in &i.signals {
                if let Signal::At(at) = sig {
                    match Endpoint::parse(at) {
                        Ok(Endpoint::Net(_)) => out.push(format!(
                            "{} signal {name} lands on a net, not the part",
                            i.kind
                        )),
                        Ok(_) => {}
                        Err(m) => out.push(m),
                    }
                }
            }
            cites.extend(i.cite.iter());
        }
        if let PartSymbol::Inline { pins } = &self.symbol {
            cites.extend(pins.iter().map(|p| &p.cite));
        }
        cites.extend(self.params.values().map(|p| &p.cite));
        if self.datasheet.is_none() && cites.iter().any(|c| matches!(c, Cite::Quote { .. })) {
            out.push("quotes a datasheet but names none".into());
        }
        out
    }

    /// Every citation the part carries, wherever it sits.
    fn cites(&self) -> Vec<&Cite> {
        let mut out: Vec<&Cite> = self.support.iter().map(|s| &s.cite).collect();
        out.extend(self.interfaces.iter().filter_map(|i| i.cite.as_ref()));
        out.extend(self.params.values().map(|p| &p.cite));
        if let PartSymbol::Inline { pins } = &self.symbol {
            out.extend(pins.iter().map(|p| &p.cite));
        }
        out
    }

    /// Every datasheet quote the part rests on, as `(page, quote)`.
    pub fn quotes(&self) -> Vec<(usize, &str)> {
        self.cites()
            .into_iter()
            .filter_map(|c| match c {
                Cite::Quote { page, quote } => Some((*page, quote.as_str())),
                Cite::Reading { .. } => None,
            })
            .collect()
    }

    /// Every reading no person has confirmed yet.
    pub fn unconfirmed(&self) -> Vec<String> {
        self.cites()
            .into_iter()
            .filter_map(|c| match c {
                Cite::Reading {
                    reading,
                    page,
                    confirmed_by: None,
                } => Some(match page {
                    Some(p) => format!("{} p.{p}: {reading}", self.mpn),
                    None => format!("{}: {reading}", self.mpn),
                }),
                _ => None,
            })
            .collect()
    }

    /// `(number, name)` for every pin, from the KiCad symbol or the inline
    /// table, plus `(number, alternate)` for every alternate function.
    #[allow(clippy::type_complexity)]
    pub fn pins(
        &self,
        symbol_dir: &Path,
    ) -> Result<(Vec<(String, String)>, Vec<(String, String)>), StageError> {
        match &self.symbol {
            PartSymbol::Inline { pins } => Ok((
                pins.iter()
                    .map(|p| (p.number.clone(), p.name.clone()))
                    .collect(),
                Vec::new(),
            )),
            PartSymbol::Kicad { kicad } => {
                let (lib, name) = kicad.split_once(':').ok_or_else(|| {
                    StageError::Other(format!("{}: symbol {kicad:?} is not Lib:Name", self.mpn))
                })?;
                let data = symbols::read_symbol(symbol_dir, lib, name)?.ok_or_else(|| {
                    StageError::Other(format!("{}: no KiCad symbol {kicad}", self.mpn))
                })?;
                Ok((data.pins, data.alternates))
            }
        }
    }
}

/// Hold every pin name and alternate the catalog uses to the symbols they
/// come from. One line per problem; empty means every name exists.
pub fn check_symbols(catalog: &Catalog, symbol_dir: &Path) -> Result<Vec<String>, StageError> {
    let mut problems = Vec::new();
    for part in &catalog.parts {
        let (pins, alternates) = part.pins(symbol_dir)?;
        let missing = |endpoint: &str, context: &str| match Endpoint::parse(endpoint) {
            Ok(Endpoint::Pin(n)) if !pins.iter().any(|(_, name)| *name == n) => Some(format!(
                "{}: {context} names pin {n:?}, which the symbol lacks",
                part.mpn
            )),
            _ => None,
        };
        for s in &part.support {
            problems.extend(s.between.iter().filter_map(|e| missing(e, "support")));
        }
        for i in &part.interfaces {
            for (sig, s) in &i.signals {
                match s {
                    Signal::At(at) => problems.extend(missing(at, &format!("{} {sig}", i.kind))),
                    Signal::Alt { alt } => {
                        if !alternates.iter().any(|(_, a)| a == alt) {
                            problems.push(format!(
                                "{}: {} {sig} wants alternate {alt}, which no pin has",
                                part.mpn, i.kind
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(problems)
}

/// Check every quote in the catalog against its part's pinned datasheet.
/// One line per failure; empty means every quote is on its page.
pub fn check_quotes(catalog: &Catalog, cache_dir: &Path) -> Result<Vec<String>, StageError> {
    let mut failures = Vec::new();
    for part in &catalog.parts {
        let quotes = part.quotes();
        let Some(ds) = &part.datasheet else {
            continue;
        };
        if quotes.is_empty() {
            continue;
        }
        let pdf = crate::datasheet::fetch_pinned(&part.mpn, &ds.url, &ds.sha256, cache_dir)?;
        let pages = crate::datasheet::pages(&pdf)?;
        for (page, quote) in quotes {
            if let Err(e) = crate::datasheet::check_quote(&part.mpn, page, quote, &pages) {
                failures.push(e);
            }
        }
    }
    Ok(failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part_json(extra: &str) -> String {
        format!(
            r#"{{"mpn": "X1", "manufacturer": "M", "summary": "s",
                "symbol": {{"kicad": "Audio:PCM5102A"}},
                "footprint": "Package_SO:TSSOP-20_4.4x6.5mm_P0.65mm"{extra}}}"#
        )
    }

    fn load_one(json: &str) -> Result<Catalog, CatalogError> {
        let dir = std::env::temp_dir().join(format!(
            "lob-catalog-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("X1.json"), json).unwrap();
        let out = Catalog::load(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn a_misspelled_field_is_an_error_not_a_silent_default() {
        let err = load_one(&part_json(r#", "provide": ["i2s-dac"]"#)).unwrap_err();
        assert!(err.to_string().contains("provide"), "{err}");
    }

    #[test]
    fn support_endpoints_kinds_and_values_are_checked_on_load() {
        let bad = [
            (r#"["AVDD", "net:GND"]"#, "tie", "", "not pin:/net:/node:"),
            (
                r#"["pin:AVDD", "net:GND"]"#,
                "L",
                r#", "value": "1u""#,
                "not tie, C, CP or R",
            ),
            (r#"["pin:AVDD", "net:GND"]"#, "C", "", "has no value"),
        ];
        for (between, part, value, want) in bad {
            let json = part_json(&format!(
                r#", "support": [{{"between": {between}, "part": "{part}"{value},
                    "cite": {{"reading": "r"}}}}]"#
            ));
            let err = load_one(&json).unwrap_err().to_string();
            assert!(err.contains(want), "{want}: {err}");
        }
    }

    #[test]
    fn a_quote_needs_a_datasheet_to_be_on() {
        let json = part_json(
            r#", "support": [{"between": ["pin:AVDD", "net:GND"], "part": "C",
                "value": "100nF", "cite": {"page": 26, "quote": "0.1mF"}}]"#,
        );
        let err = load_one(&json).unwrap_err().to_string();
        assert!(err.contains("names none"), "{err}");
    }

    #[test]
    fn a_file_must_be_named_for_its_part() {
        let json = part_json("").replace(r#""mpn": "X1""#, r#""mpn": "OTHER""#);
        let err = load_one(&json).unwrap_err().to_string();
        assert!(err.contains("OTHER.json"), "{err}");
    }

    /// The shipped catalog is part of the contract: a hand edit that breaks
    /// the schema fails here, in the ordinary test run.
    #[test]
    fn the_shipped_catalog_loads() {
        let cat = Catalog::load(&default_catalog_dir()).expect("catalog/parts loads");
        assert!(cat.part("STM32H743VIT6").is_some());
        assert!(
            cat.providing("i2s-dac").count() >= 3,
            "DAC options to choose among"
        );
    }

    /// Needs the installed KiCad symbol library, so it is ignored by default
    /// rather than silently passing without it (legion-of-bom-69v.2).
    #[test]
    #[ignore = "needs KiCad symbols"]
    fn every_catalog_pin_and_alternate_exists_in_its_symbol() {
        let dir = crate::skidl::kicad_symbol_dir().expect("KiCad symbol library");
        let cat = Catalog::load(&default_catalog_dir()).unwrap();
        let problems = check_symbols(&cat, dir.path()).unwrap();
        assert!(problems.is_empty(), "{problems:#?}");
    }

    /// Needs the pinned datasheets (fetched into the cache on first run).
    #[test]
    #[ignore = "needs the pinned datasheets and pdftotext"]
    fn every_catalog_quote_is_on_its_page() {
        let cat = Catalog::load(&default_catalog_dir()).unwrap();
        let failures = check_quotes(&cat, &crate::datasheet::default_cache_dir()).unwrap();
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn readings_are_reported_until_someone_confirms_them() {
        let json = part_json(
            r#", "support": [
                {"between": ["pin:AVDD", "net:GND"], "part": "C", "value": "10uF",
                 "cite": {"reading": "Figure 57", "page": 60}},
                {"between": ["pin:DVDD", "net:GND"], "part": "C", "value": "10uF",
                 "cite": {"reading": "Figure 57", "page": 60, "confirmed_by": "Avery"}}]"#,
        );
        let cat = load_one(&json).unwrap();
        assert_eq!(
            cat.parts[0].unconfirmed(),
            vec!["X1 p.60: Figure 57".to_string()]
        );
    }
}
