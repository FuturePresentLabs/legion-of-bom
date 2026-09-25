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
    /// Cited source/load/regulator intent carried into the emitted netlist.
    #[serde(default)]
    pub power: Option<PowerIntent>,
    /// Source-backed conductive behavior used to reconstruct possible paths
    /// from the rendered circuit. This describes the part, never a product
    /// requirement or a pass/fail conclusion.
    #[serde(default)]
    pub conduction: Vec<ConductionPath>,
    /// Source-backed supervisory/control outputs exposed by this part.
    #[serde(default)]
    pub control_outputs: Vec<ControlOutput>,
    #[serde(default)]
    pub interfaces: Vec<Interface>,
    #[serde(default)]
    pub support: Vec<Support>,
    /// Not an analog circuit SPICE models (an IC, a crystal): `Sim.Enable = 0`.
    #[serde(default)]
    pub sim_excluded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConductionPath {
    pub id: String,
    pub from_pin: String,
    pub to_pin: String,
    /// Whether current may flow from `to_pin` back toward `from_pin` in the
    /// state being modeled. Unknown behavior must not be encoded as false.
    pub reverse_conducting: bool,
    /// Whether the path conducts before any active controller configures it.
    pub default_conducting: bool,
    #[serde(default)]
    pub control_pin: Option<String>,
    /// Stable part-local control identity. Equal identities represent one
    /// control cause; distinct identities may form independent series cuts.
    #[serde(default)]
    pub control_identity: Option<String>,
    pub cite: Cite,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlOutput {
    /// General function such as `reset`, `watchdog`, `interlock`, or another
    /// caller-defined kind. This is descriptive behavior, not applicability.
    pub kind: String,
    pub pin: String,
    pub cite: Cite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerRole {
    Source,
    Regulator,
    Load,
}

impl PowerRole {
    fn field_value(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Regulator => "regulator",
            Self::Load => "load",
        }
    }
}

/// Power intent is catalog evidence, not a synthesis guess. Net names describe
/// the rails at the component boundary; numeric facts retain their own citation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerIntent {
    pub role: PowerRole,
    #[serde(default)]
    pub input_net: Option<String>,
    #[serde(default)]
    pub output_net: Option<String>,
    pub cite: Cite,
    #[serde(default)]
    pub input_voltage_v: Option<Param>,
    #[serde(default)]
    pub output_voltage_v: Option<Param>,
    #[serde(default)]
    pub output_current_a: Option<Param>,
    #[serde(default)]
    pub load_current_a: Option<Param>,
    #[serde(default)]
    pub dropout_v: Option<Param>,
}

impl PowerIntent {
    fn facts(&self) -> [(&'static str, Option<&Param>); 5] {
        [
            ("Power.InputVoltageV", self.input_voltage_v.as_ref()),
            ("Power.OutputVoltageV", self.output_voltage_v.as_ref()),
            ("Power.OutputCurrentA", self.output_current_a.as_ref()),
            ("Power.LoadCurrentA", self.load_current_a.as_ref()),
            ("Power.DropoutV", self.dropout_v.as_ref()),
        ]
    }
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
    /// `tie` (a direct connection), `C`, `CP` (polarised), `L`, or `R`.
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

impl Cite {
    /// Whether this citation is strong enough to drive deterministic output.
    /// Quotes are checked against pinned documents; readings require an
    /// explicit confirmer. Unconfirmed readings remain visible but inert.
    fn verified(&self) -> bool {
        matches!(
            self,
            Cite::Quote { .. }
                | Cite::Reading {
                    confirmed_by: Some(_),
                    ..
                }
        )
    }
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

/// A reusable circuit around role **slots** — a reference design, not a board
/// (DESIGN.md 3.4). One JSON file per subcircuit under `catalog/subcircuits/`,
/// named `<name>.json`.
///
/// Synthesis offers a subcircuit wherever a part providing the same role
/// would go; each slot is then filled from the catalog parts that meet the
/// slot's requirement. Its facts — the wiring between slots, the matching
/// network — come from the reference design pinned as `source`, cited like a
/// part's, and its endpoints may name a slot's pin: `radio.pin:RFO`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subcircuit {
    pub name: String,
    /// What a decider is told about it.
    pub summary: String,
    /// The reference design its quotes are on.
    #[serde(default)]
    pub source: Option<DatasheetRef>,
    /// The roles it fills on a board.
    pub provides: Vec<String>,
    pub slots: BTreeMap<String, Slot>,
    /// What it exposes to the board, each signal at a slot's pin
    /// (`switch.pin:RFC`) or one of its own nodes (`node:ant`).
    #[serde(default)]
    pub interfaces: Vec<Interface>,
    /// Ties between slots and the passives around them.
    #[serde(default)]
    pub support: Vec<Support>,
}

/// What the part filling a slot must be.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    /// The role it must provide.
    pub provides: String,
    /// When the subcircuit's values hold only for certain parts (a matching
    /// network tuned to one PA), exactly those; empty means any part with
    /// the role.
    #[serde(default)]
    pub any_of: Vec<String>,
}

/// An endpoint inside a subcircuit: its own (`node:ant`, `net:GND`), a
/// slot's pin (`radio.pin:RFO`), or a node its slot's part names in its own
/// support (`switch.node:rfc`, past the switch's DC block).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scoped {
    Own(Endpoint),
    SlotPin { slot: String, pin: String },
    SlotNode { slot: String, node: String },
}

impl Scoped {
    pub fn parse(s: &str) -> Result<Scoped, String> {
        let (kind, name) = s
            .split_once(':')
            .ok_or_else(|| format!("endpoint {s:?} is not [slot.]pin:/net:/node:NAME"))?;
        match kind.split_once('.') {
            Some((slot, "pin")) if !slot.is_empty() && !name.is_empty() => Ok(Scoped::SlotPin {
                slot: slot.into(),
                pin: name.into(),
            }),
            Some((slot, "node")) if !slot.is_empty() && !name.is_empty() => Ok(Scoped::SlotNode {
                slot: slot.into(),
                node: name.into(),
            }),
            Some(_) => Err(format!(
                "endpoint {s:?}: only a slot's pin or node can be named (slot.pin:NAME, slot.node:NAME)"
            )),
            None => match Endpoint::parse(s)? {
                Endpoint::Pin(_) => Err(format!(
                    "endpoint {s:?}: a subcircuit has no pins of its own; name the slot (slot.pin:NAME)"
                )),
                e => Ok(Scoped::Own(e)),
            },
        }
    }
}

/// A board **form factor** — a standard's outline and mounting holes, or a
/// free outline with holes in its corners (legion-of-bom-3wbu). One JSON file
/// per form factor under `catalog/formfactors/`, named `<name>.json`; which
/// one a board uses is a typed decision from its brief.
///
/// Coordinates are board-local millimetres from the top-left corner, x right,
/// y down (KiCad's sense) — a drawing dimensioned from the bottom left is
/// converted when the file is written, and the cite says so.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormFactor {
    pub name: String,
    /// What a decider is told about it.
    pub summary: String,
    /// The standard's drawing its quotes are on.
    #[serde(default)]
    pub source: Option<DatasheetRef>,
    /// The fixed outline; none means the board is sized to its parts.
    #[serde(default)]
    pub outline: Option<Outline>,
    #[serde(default)]
    pub holes: Option<Holes>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outline {
    pub width_mm: f64,
    pub height_mm: f64,
    pub cite: Cite,
}

/// Mounting holes: one KiCad footprint (`Lib:Name`, whose pad and courtyard
/// are the hole's geometry and keep-out), either in the corners of the board
/// or at the standard's points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Holes {
    pub footprint: String,
    #[serde(default)]
    pub corners: bool,
    #[serde(default)]
    pub at: Vec<HolePoint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HolePoint {
    pub x_mm: f64,
    pub y_mm: f64,
    pub cite: Cite,
}

impl FormFactor {
    fn shape_problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(h) = &self.holes {
            if h.footprint.split_once(':').is_none() {
                out.push(format!("hole footprint {:?} is not Lib:Name", h.footprint));
            }
            match (h.corners, h.at.is_empty()) {
                (true, false) => out.push("holes are in the corners or at points, not both".into()),
                (false, true) => out.push("holes name no corners and no points".into()),
                _ => {}
            }
            if let Some(o) = &self.outline {
                for p in &h.at {
                    if p.x_mm <= 0.0
                        || p.y_mm <= 0.0
                        || p.x_mm >= o.width_mm
                        || p.y_mm >= o.height_mm
                    {
                        out.push(format!("hole at ({}, {}) is off the board", p.x_mm, p.y_mm));
                    }
                }
            } else if !h.at.is_empty() {
                out.push("holes at points need a fixed outline to be on".into());
            }
        }
        if self.source.is_none() && !self.quotes().is_empty() {
            out.push("quotes a source but names none".into());
        }
        out
    }

    fn cites(&self) -> Vec<&Cite> {
        let mut out: Vec<&Cite> = self.outline.iter().map(|o| &o.cite).collect();
        out.extend(self.holes.iter().flat_map(|h| h.at.iter().map(|p| &p.cite)));
        out
    }

    /// Every quote from its standard, as `(page, quote)`.
    pub fn quotes(&self) -> Vec<(usize, &str)> {
        quotes_of(self.cites())
    }

    /// Every reading no person has confirmed yet.
    pub fn unconfirmed(&self) -> Vec<String> {
        unconfirmed_of(&self.name, self.cites())
    }
}

/// The loaded catalog.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub parts: Vec<CatalogPart>,
    /// Board form factors (`formfactors/` beside the parts directory).
    pub form_factors: Vec<FormFactor>,
    /// Reusable circuits over the parts (`subcircuits/` beside the parts
    /// directory).
    pub subcircuits: Vec<Subcircuit>,
    /// The requirement vocabulary (`features.json` beside the parts
    /// directory): what a brief can ask for, and the role, interface and
    /// connector port that meet it.
    pub features: Vec<Feature>,
}

/// One requirement a brief can ask of a board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feature {
    pub key: String,
    /// The yes/no question put to the decider.
    pub question: String,
    /// The role a catalog part must provide to meet it.
    pub role: String,
    /// That part's interface which carries it out to the world.
    pub interface: String,
    /// The connector port kind it lands on.
    pub port: String,
    /// Board net prefix for its signals.
    pub net: String,
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
        let mut parts = Vec::new();
        for (path, part) in read_json_dir::<CatalogPart>(dir)? {
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
        let mut catalog = Catalog {
            parts,
            ..Catalog::default()
        };
        // Subcircuits sit beside the parts; a parts-only directory has none.
        let sub_dir = dir.parent().map(|p| p.join("subcircuits"));
        for (path, sub) in match sub_dir.filter(|d| d.is_dir()) {
            Some(d) => read_json_dir::<Subcircuit>(&d)?,
            None => Vec::new(),
        } {
            let mut problems = Vec::new();
            if path.file_stem().and_then(|s| s.to_str()) != Some(sub.name.as_str()) {
                problems.push(format!("file name must be {}.json", sub.name));
            }
            problems.extend(sub.shape_problems(&catalog));
            if !problems.is_empty() {
                return Err(CatalogError::Invalid {
                    path,
                    message: problems.join("; "),
                });
            }
            catalog.subcircuits.push(sub);
        }
        let ff_dir = dir.parent().map(|p| p.join("formfactors"));
        for (path, ff) in match ff_dir.filter(|d| d.is_dir()) {
            Some(d) => read_json_dir::<FormFactor>(&d)?,
            None => Vec::new(),
        } {
            let mut problems = Vec::new();
            if path.file_stem().and_then(|s| s.to_str()) != Some(ff.name.as_str()) {
                problems.push(format!("file name must be {}.json", ff.name));
            }
            problems.extend(ff.shape_problems());
            if !problems.is_empty() {
                return Err(CatalogError::Invalid {
                    path,
                    message: problems.join("; "),
                });
            }
            catalog.form_factors.push(ff);
        }
        // The vocabulary sits beside the parts; a parts-only directory has none.
        let features_path = dir.parent().map(|p| p.join("features.json"));
        let features = match features_path.filter(|p| p.is_file()) {
            Some(path) => {
                let text = std::fs::read_to_string(&path).map_err(|source| CatalogError::Io {
                    path: path.clone(),
                    source,
                })?;
                serde_json::from_str(&text)
                    .map_err(|source| CatalogError::Parse { path, source })?
            }
            None => Vec::new(),
        };
        catalog.features = features;
        Ok(catalog)
    }

    /// A hash of every part's content, in part order: what a design spec
    /// records so a spec decided against one catalog is not silently rendered
    /// against another.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for p in &self.parts {
            h.update(
                serde_json::to_string(p)
                    .expect("a part serializes")
                    .as_bytes(),
            );
            h.update([0]);
        }
        for s in &self.subcircuits {
            h.update(
                serde_json::to_string(s)
                    .expect("a subcircuit serializes")
                    .as_bytes(),
            );
            h.update([0]);
        }
        for f in &self.form_factors {
            h.update(
                serde_json::to_string(f)
                    .expect("a form factor serializes")
                    .as_bytes(),
            );
            h.update([0]);
        }
        h.update(
            serde_json::to_string(&self.features)
                .expect("features serialize")
                .as_bytes(),
        );
        h.finalize()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
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

    pub fn form_factor(&self, name: &str) -> Option<&FormFactor> {
        self.form_factors.iter().find(|f| f.name == name)
    }

    pub fn subcircuit(&self, name: &str) -> Option<&Subcircuit> {
        self.subcircuits.iter().find(|s| s.name == name)
    }

    /// Every part that can fill `slot`, in catalog order.
    pub fn candidates<'a>(&'a self, slot: &'a Slot) -> impl Iterator<Item = &'a CatalogPart> + 'a {
        self.providing(&slot.provides)
            .filter(|p| slot.any_of.is_empty() || slot.any_of.contains(&p.mpn))
    }
}

/// Every `*.json` in `dir`, parsed, sorted by file name so the same catalog
/// always loads the same way.
fn read_json_dir<T: serde::de::DeserializeOwned>(
    dir: &Path,
) -> Result<Vec<(PathBuf, T)>, CatalogError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|source| CatalogError::Io {
            path: dir.to_path_buf(),
            source,
        })?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).map_err(|source| CatalogError::Io {
                path: path.clone(),
                source,
            })?;
            let value = serde_json::from_str(&text).map_err(|source| CatalogError::Parse {
                path: path.clone(),
                source,
            })?;
            Ok((path, value))
        })
        .collect()
}

/// Problems with one support entry's kind and value, whatever owns it.
fn support_problems(s: &Support) -> Vec<String> {
    let mut out = Vec::new();
    if !matches!(s.part.as_str(), "tie" | "C" | "CP" | "L" | "R") {
        out.push(format!(
            "support part {:?} is not tie, C, CP, L or R",
            s.part
        ));
    }
    if s.part != "tie" && s.value.is_none() {
        out.push(format!("{} between {:?} has no value", s.part, s.between));
    }
    out
}

/// The quotes among `cites`, as `(page, quote)`.
fn quotes_of<'a>(cites: impl IntoIterator<Item = &'a Cite>) -> Vec<(usize, &'a str)> {
    cites
        .into_iter()
        .filter_map(|c| match c {
            Cite::Quote { page, quote } => Some((*page, quote.as_str())),
            Cite::Reading { .. } => None,
        })
        .collect()
}

/// The readings among `cites` no person has confirmed, labelled with `who`.
fn unconfirmed_of<'a>(who: &str, cites: impl IntoIterator<Item = &'a Cite>) -> Vec<String> {
    cites
        .into_iter()
        .filter_map(|c| match c {
            Cite::Reading {
                reading,
                page,
                confirmed_by: None,
            } => Some(match page {
                Some(p) => format!("{who} p.{p}: {reading}"),
                None => format!("{who}: {reading}"),
            }),
            _ => None,
        })
        .collect()
}

impl Subcircuit {
    /// Every endpoint it names, with where it is named.
    fn endpoints(&self) -> Vec<(String, &str)> {
        let mut out: Vec<(String, &str)> = Vec::new();
        for s in &self.support {
            out.extend(
                s.between
                    .iter()
                    .map(|e| ("support".to_string(), e.as_str())),
            );
        }
        for i in &self.interfaces {
            for (sig, s) in &i.signals {
                match s {
                    Signal::At(at) => out.push((format!("{} {sig}", i.kind), at.as_str())),
                    // Caught in shape_problems: a subcircuit has no alternates.
                    Signal::Alt { .. } => {}
                }
            }
        }
        out
    }

    /// Problems visible from the file and the parts it draws on: malformed or
    /// unknown-slot endpoints, a slot no part can fill, a quote with no
    /// source to be on.
    fn shape_problems(&self, catalog: &Catalog) -> Vec<String> {
        let mut out = Vec::new();
        for s in &self.support {
            out.extend(support_problems(s));
            if s.each_pin {
                out.push(format!(
                    "each_pin support {:?}: a subcircuit names each pin",
                    s.between
                ));
            }
        }
        for i in &self.interfaces {
            for (sig, s) in &i.signals {
                if let Signal::Alt { alt } = s {
                    out.push(format!(
                        "{} {sig} wants alternate {alt}: a subcircuit wires named pins only",
                        i.kind
                    ));
                }
            }
        }
        for (context, e) in self.endpoints() {
            match Scoped::parse(e) {
                Ok(Scoped::SlotPin { slot, .. } | Scoped::SlotNode { slot, .. })
                    if !self.slots.contains_key(&slot) =>
                {
                    out.push(format!("{context} names slot {slot:?}, which it lacks"))
                }
                // A slot's node must be one every candidate part names.
                Ok(Scoped::SlotNode { slot, node }) => {
                    let want = format!("node:{node}");
                    for p in catalog.candidates(&self.slots[&slot]) {
                        let in_support = p
                            .support
                            .iter()
                            .flat_map(|s| &s.between)
                            .any(|e| *e == want);
                        let in_interface = p
                            .interfaces
                            .iter()
                            .flat_map(|i| i.signals.values())
                            .any(|s| matches!(s, Signal::At(at) if *at == want));
                        if !in_support && !in_interface {
                            out.push(format!(
                                "{context} names {slot} node {node:?}, which {} lacks",
                                p.mpn
                            ));
                        }
                    }
                }
                Ok(Scoped::Own(Endpoint::Net(_))) if context != "support" => {
                    out.push(format!("{context} lands on a net, not the subcircuit"))
                }
                Ok(_) => {}
                Err(m) => out.push(m),
            }
        }
        for (name, slot) in &self.slots {
            for mpn in &slot.any_of {
                match catalog.part(mpn) {
                    None => out.push(format!("slot {name} names {mpn}, which the catalog lacks")),
                    Some(p) if !p.provides.contains(&slot.provides) => out.push(format!(
                        "slot {name} names {mpn}, which does not provide {}",
                        slot.provides
                    )),
                    Some(_) => {}
                }
            }
            if catalog.candidates(slot).next().is_none() {
                out.push(format!(
                    "slot {name}: no catalog part provides {}",
                    slot.provides
                ));
            }
        }
        if self.source.is_none() && !self.quotes().is_empty() {
            out.push("quotes a source but names none".into());
        }
        out
    }

    fn cites(&self) -> Vec<&Cite> {
        let mut out: Vec<&Cite> = self.support.iter().map(|s| &s.cite).collect();
        out.extend(self.interfaces.iter().filter_map(|i| i.cite.as_ref()));
        out
    }

    /// Every quote from its reference design, as `(page, quote)`.
    pub fn quotes(&self) -> Vec<(usize, &str)> {
        quotes_of(self.cites())
    }

    /// Every reading no person has confirmed yet.
    pub fn unconfirmed(&self) -> Vec<String> {
        unconfirmed_of(&self.name, self.cites())
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
            out.extend(support_problems(s));
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
        if let Some(power) = &self.power {
            cites.push(&power.cite);
            cites.extend(
                power
                    .facts()
                    .into_iter()
                    .filter_map(|(_, fact)| fact.map(|p| &p.cite)),
            );
            let missing = match power.role {
                PowerRole::Source => power.output_net.is_none(),
                PowerRole::Regulator => power.input_net.is_none() || power.output_net.is_none(),
                PowerRole::Load => power.input_net.is_none(),
            };
            if missing {
                out.push(format!(
                    "power role {} is missing its required input/output net",
                    power.role.field_value()
                ));
            }
            for net in [power.input_net.as_deref(), power.output_net.as_deref()]
                .into_iter()
                .flatten()
            {
                if !crate::model::is_supply_rail(net) {
                    out.push(format!("power net {net:?} is not a named supply rail"));
                }
            }
            for (name, fact) in power.facts() {
                if fact.is_some_and(|fact| !fact.value.is_finite() || fact.value < 0.0) {
                    out.push(format!("{name} must be finite and non-negative"));
                }
            }
        }
        for path in &self.conduction {
            cites.push(&path.cite);
            if path.id.trim().is_empty()
                || path.from_pin.trim().is_empty()
                || path.to_pin.trim().is_empty()
            {
                out.push("conduction paths require non-empty id/from_pin/to_pin".into());
            }
            if path.control_pin.is_some() != path.control_identity.is_some() {
                out.push(format!(
                    "conduction path {:?} must declare control_pin and control_identity together",
                    path.id
                ));
            }
        }
        for output in &self.control_outputs {
            cites.push(&output.cite);
            if output.kind.trim().is_empty() || output.pin.trim().is_empty() {
                out.push("control outputs require non-empty kind and pin".into());
            }
        }
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
        if let Some(power) = &self.power {
            out.push(&power.cite);
            out.extend(
                power
                    .facts()
                    .into_iter()
                    .filter_map(|(_, fact)| fact.map(|p| &p.cite)),
            );
        }
        out.extend(self.conduction.iter().map(|path| &path.cite));
        out.extend(self.control_outputs.iter().map(|output| &output.cite));
        if let PartSymbol::Inline { pins } = &self.symbol {
            out.extend(pins.iter().map(|p| &p.cite));
        }
        out
    }

    /// Every datasheet quote the part rests on, as `(page, quote)`.
    pub fn quotes(&self) -> Vec<(usize, &str)> {
        quotes_of(self.cites())
    }

    /// Every reading no person has confirmed yet.
    pub fn unconfirmed(&self) -> Vec<String> {
        unconfirmed_of(&self.mpn, self.cites())
    }

    /// Netlist fields whose provenance is verified. This is the single adapter
    /// used by synthesis and replay plans, so the two paths cannot drift.
    #[must_use]
    pub fn netlist_fields(&self) -> BTreeMap<String, String> {
        let mut fields = BTreeMap::from([("MPN".into(), self.mpn.clone())]);
        if let Some(lcsc) = &self.lcsc {
            fields.insert("LCSC".into(), lcsc.clone());
        }
        if self.sim_excluded {
            fields.insert("Sim.Enable".into(), "0".into());
        }
        if let Some(power) = &self.power {
            if power.cite.verified() {
                fields.insert("Power.Role".into(), power.role.field_value().into());
                if let Some(net) = &power.input_net {
                    fields.insert("Power.InputNet".into(), net.clone());
                }
                if let Some(net) = &power.output_net {
                    fields.insert("Power.OutputNet".into(), net.clone());
                }
            }
            for (name, fact) in power.facts() {
                if let Some(fact) = fact.filter(|fact| fact.cite.verified()) {
                    fields.insert(name.into(), fact.value.to_string());
                }
            }
        }
        for (index, path) in self.conduction.iter().enumerate() {
            if !path.cite.verified() {
                continue;
            }
            let prefix = format!("Conduction.{index}");
            fields.insert(format!("{prefix}.Id"), path.id.clone());
            fields.insert(format!("{prefix}.FromPin"), path.from_pin.clone());
            fields.insert(format!("{prefix}.ToPin"), path.to_pin.clone());
            fields.insert(
                format!("{prefix}.ReverseConducting"),
                path.reverse_conducting.to_string(),
            );
            fields.insert(
                format!("{prefix}.DefaultConducting"),
                path.default_conducting.to_string(),
            );
            if let Some(control_pin) = &path.control_pin {
                fields.insert(format!("{prefix}.ControlPin"), control_pin.clone());
            }
            if let Some(identity) = &path.control_identity {
                fields.insert(format!("{prefix}.ControlIdentity"), identity.clone());
            }
        }
        for (index, output) in self.control_outputs.iter().enumerate() {
            if !output.cite.verified() {
                continue;
            }
            let prefix = format!("ControlOutput.{index}");
            fields.insert(format!("{prefix}.Kind"), output.kind.clone());
            fields.insert(format!("{prefix}.Pin"), output.pin.clone());
        }
        fields
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
        for path in &part.conduction {
            for pin in [
                Some(path.from_pin.as_str()),
                Some(path.to_pin.as_str()),
                path.control_pin.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if !pins.iter().any(|(_, name)| name == pin) {
                    problems.push(format!(
                        "{}: conduction path {} names pin {pin:?}, which the symbol lacks",
                        part.mpn, path.id
                    ));
                }
            }
        }
        for output in &part.control_outputs {
            if !pins.iter().any(|(_, name)| name == &output.pin) {
                problems.push(format!(
                    "{}: control output {} names pin {:?}, which the symbol lacks",
                    part.mpn, output.kind, output.pin
                ));
            }
        }
    }
    // A subcircuit's slot pins must exist on every part that could fill it.
    for sub in &catalog.subcircuits {
        for (context, e) in sub.endpoints() {
            let Ok(Scoped::SlotPin { slot, pin }) = Scoped::parse(e) else {
                continue;
            };
            for part in catalog.candidates(&sub.slots[&slot]) {
                let (pins, _) = part.pins(symbol_dir)?;
                if !pins.iter().any(|(_, name)| *name == pin) {
                    problems.push(format!(
                        "{}: {context} names {slot} pin {pin:?}, which {} lacks",
                        sub.name, part.mpn
                    ));
                }
            }
        }
    }
    Ok(problems)
}

/// Every footprint a form factor names must exist in the KiCad library. One
/// line per problem.
pub fn check_footprints(catalog: &Catalog, footprint_dir: &Path) -> Vec<String> {
    catalog
        .form_factors
        .iter()
        .filter_map(|f| f.holes.as_ref().map(|h| (&f.name, &h.footprint)))
        .filter_map(|(who, fp)| {
            let (lib, name) = fp.split_once(':')?;
            let path = footprint_dir
                .join(format!("{lib}.pretty"))
                .join(format!("{name}.kicad_mod"));
            (!path.is_file()).then(|| format!("{who}: no KiCad footprint {fp}"))
        })
        .collect()
}

/// Check every quote in the catalog against its part's pinned datasheet.
/// One line per failure; empty means every quote is on its page.
pub fn check_quotes(catalog: &Catalog, cache_dir: &Path) -> Result<Vec<String>, StageError> {
    let parts = catalog
        .parts
        .iter()
        .map(|p| (&p.mpn, &p.datasheet, p.quotes()));
    let subs = catalog
        .subcircuits
        .iter()
        .map(|s| (&s.name, &s.source, s.quotes()));
    let form_factors = catalog
        .form_factors
        .iter()
        .map(|f| (&f.name, &f.source, f.quotes()));
    let mut failures = Vec::new();
    for (who, source, quotes) in parts.chain(subs).chain(form_factors) {
        let Some(ds) = source else {
            continue;
        };
        if quotes.is_empty() {
            continue;
        }
        let pdf = crate::datasheet::fetch_pinned(who, &ds.url, &ds.sha256, cache_dir)?;
        let pages = crate::datasheet::pages(&pdf)?;
        for (page, quote) in quotes {
            if let Err(e) = crate::datasheet::check_quote(who, page, quote, &pages) {
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
                "D",
                r#", "value": "1u""#,
                "not tie, C, CP, L or R",
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
    fn power_roles_require_their_boundary_nets() {
        let json = part_json(
            r#", "power": {"role": "regulator", "input_net": "+5V",
                "cite": {"reading": "fixture", "confirmed_by": "test"}}"#,
        );
        let err = load_one(&json).unwrap_err().to_string();
        assert!(
            err.contains("missing its required input/output net"),
            "{err}"
        );
    }

    #[test]
    fn only_confirmed_behavior_becomes_netlist_evidence() {
        let json = part_json(
            r#", "conduction": [
                {"id": "main", "from_pin": "IN", "to_pin": "OUT",
                 "reverse_conducting": false, "default_conducting": false,
                 "control_pin": "EN", "control_identity": "enable",
                 "cite": {"reading": "confirmed behavior", "confirmed_by": "test"}},
                {"id": "guess", "from_pin": "A", "to_pin": "B",
                 "reverse_conducting": true, "default_conducting": true,
                 "cite": {"reading": "unconfirmed behavior"}}
            ], "control_outputs": [
                {"kind": "watchdog", "pin": "WDO",
                 "cite": {"reading": "confirmed output", "confirmed_by": "test"}}
            ]"#,
        );
        let catalog = load_one(&json).unwrap();
        let fields = catalog.parts[0].netlist_fields();
        assert_eq!(fields["Conduction.0.FromPin"], "IN");
        assert_eq!(fields["Conduction.0.ControlPin"], "EN");
        assert!(!fields.contains_key("Conduction.1.FromPin"));
        assert_eq!(fields["ControlOutput.0.Kind"], "watchdog");
    }

    #[test]
    fn only_verified_power_facts_become_netlist_fields() {
        let json = part_json(
            r#", "power": {"role": "regulator", "input_net": "+5V",
                "output_net": "+3V3",
                "cite": {"reading": "topology", "confirmed_by": "test"},
                "output_voltage_v": {"value": 3.3,
                    "cite": {"reading": "marked value", "confirmed_by": "test"}},
                "output_current_a": {"value": 0.5,
                    "cite": {"reading": "not checked yet"}}}"#,
        );
        let catalog = load_one(&json).unwrap();
        let fields = catalog.parts[0].netlist_fields();
        assert_eq!(
            fields.get("Power.Role").map(String::as_str),
            Some("regulator")
        );
        assert_eq!(
            fields.get("Power.InputNet").map(String::as_str),
            Some("+5V")
        );
        assert_eq!(
            fields.get("Power.OutputVoltageV").map(String::as_str),
            Some("3.3")
        );
        assert!(!fields.contains_key("Power.OutputCurrentA"));
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
        let ams = cat.part("AMS1117-3.3").expect("catalogued regulator");
        let fields = ams.netlist_fields();
        assert_eq!(
            fields.get("Power.Role").map(String::as_str),
            Some("regulator")
        );
        assert_eq!(
            fields.get("Power.InputNet").map(String::as_str),
            Some("+5V")
        );
        assert_eq!(
            fields.get("Power.OutputNet").map(String::as_str),
            Some("+3V3")
        );
        assert_eq!(
            fields.get("Power.OutputVoltageV").map(String::as_str),
            Some("3.3")
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
    fn a_subcircuit_endpoint_is_its_own_or_a_slots_pin() {
        assert_eq!(
            Scoped::parse("radio.pin:RFO"),
            Ok(Scoped::SlotPin {
                slot: "radio".into(),
                pin: "RFO".into()
            })
        );
        assert_eq!(
            Scoped::parse("node:ant"),
            Ok(Scoped::Own(Endpoint::Node("ant".into())))
        );
        assert_eq!(
            Scoped::parse("switch.node:rfc"),
            Ok(Scoped::SlotNode {
                slot: "switch".into(),
                node: "rfc".into()
            })
        );
        for bad in ["pin:RFO", "radio.net:x", ".pin:RFO", "RFO"] {
            assert!(Scoped::parse(bad).is_err(), "{bad}");
        }
    }

    /// Load a catalog of the shipped parts plus one subcircuit file.
    fn load_with_subcircuit(name: &str, json: &str) -> Result<Catalog, CatalogError> {
        let root = std::env::temp_dir().join(format!(
            "lob-subcircuit-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let parts = root.join("parts");
        std::fs::create_dir_all(&parts).unwrap();
        std::fs::create_dir_all(root.join("subcircuits")).unwrap();
        for e in std::fs::read_dir(default_catalog_dir()).unwrap() {
            let p = e.unwrap().path();
            std::fs::copy(&p, parts.join(p.file_name().unwrap())).unwrap();
        }
        std::fs::write(root.join("subcircuits").join(format!("{name}.json")), json).unwrap();
        let out = Catalog::load(&parts);
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    fn subcircuit_json(slot: &str, support_end: &str) -> String {
        format!(
            r#"{{"name": "s", "summary": "s", "provides": ["radio-subghz"],
                "slots": {{"radio": {slot}}},
                "support": [{{"between": ["{support_end}", "net:GND"], "part": "C",
                             "value": "1pF", "cite": {{"reading": "r"}}}}]}}"#
        )
    }

    #[test]
    fn a_subcircuit_is_checked_against_its_slots_and_the_catalog_on_load() {
        let ok_slot = r#"{"provides": "radio-subghz", "any_of": ["CC1101RGPR"]}"#;
        let cat = load_with_subcircuit("s", &subcircuit_json(ok_slot, "radio.pin:RF_P")).unwrap();
        assert_eq!(cat.subcircuits.len(), 1);
        assert_eq!(
            cat.candidates(&cat.subcircuits[0].slots["radio"])
                .map(|p| p.mpn.as_str())
                .collect::<Vec<_>>(),
            ["CC1101RGPR"]
        );

        let bad = [
            (ok_slot, "switch.pin:RFC", "slot \"switch\""),
            (ok_slot, "pin:RF_P", "name the slot"),
            (
                r#"{"provides": "radio-subghz", "any_of": ["NOPE"]}"#,
                "radio.pin:RF_P",
                "catalog lacks",
            ),
            (
                r#"{"provides": "radio-subghz", "any_of": ["AMS1117-3.3"]}"#,
                "radio.pin:RF_P",
                "does not provide",
            ),
            (
                r#"{"provides": "no-such-role"}"#,
                "radio.pin:RF_P",
                "no catalog part",
            ),
        ];
        for (slot, end, want) in bad {
            let err = load_with_subcircuit("s", &subcircuit_json(slot, end))
                .unwrap_err()
                .to_string();
            assert!(err.contains(want), "{want}: {err}");
        }
        let err = load_with_subcircuit("other", &subcircuit_json(ok_slot, "radio.pin:RF_P"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("s.json"), "{err}");
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
