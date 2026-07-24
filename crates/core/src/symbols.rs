//! Resolve a component's SPICE model *from the component itself* — the seam the
//! parts library plugs into. DESIGN.md 3.5, 5.1.
//!
//! A KiCad symbol carries its own SPICE model in `Sim.*` properties
//! (`Sim.Device`, `Sim.Name`, `Sim.Library`, `Sim.Pins`) — e.g. the ideal
//! `Simulation_SPICE:OPAMP` references the `kicad_builtin_opamp` subckt with the
//! pin map `1=in+ 2=in- 3=vcc 4=vee 5=out`. SKiDL's KiCad-netlist export drops
//! these fields, so we recover the model by reading the symbol library here.
//!
//! This is deliberately *not* per-device logic in the SPICE generator: the
//! generator just instantiates whatever [`SpiceModel`] a part carries. Later,
//! the Dolt-backed parts library becomes the (verified, cited) source of these
//! models instead of reading symbol files directly.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::model::SimModel;
use crate::sexpr::Sexpr;
use crate::source::CircuitSource;
use crate::spice::SpiceModel;
use crate::stage::StageError;

/// Resolve SPICE models for every part in a circuit that declares one, keyed by
/// reference designator. Primitives (R/C/L — no `Sim.Device`) resolve to nothing
/// and are emitted as SPICE primitives by the generator.
pub fn resolve_models(
    circuit: &dyn CircuitSource,
    symbol_dir: &Path,
) -> Result<HashMap<String, SpiceModel>, StageError> {
    let mut models = HashMap::new();
    let mut lib_cache: HashMap<String, Sexpr> = HashMap::new();

    for part in circuit.parts() {
        let refdes = &part.refdes.0;

        // 1. Prefer a model the part carries itself — from the circuit definition's
        //    `Sim.*` fields today, the Dolt parts library later. A real device's
        //    model travels with the device.
        if let Some(sim) = &part.sim {
            if let Some(model) = model_from_sim(sim, symbol_dir, refdes)? {
                models.insert(refdes.clone(), model);
            }
            continue;
        }

        // 2. Otherwise recover it from the shipped symbol library — how KiCad's
        //    ideal `Simulation_SPICE:*` parts declare their built-in models. A
        //    missing/unparsable library is not fatal; we fall through to (3).
        let mut model = None;
        if let Some((lib, name)) = part.library_part.as_deref().and_then(|p| p.split_once(':')) {
            if !lib_cache.contains_key(lib) {
                let path = symbol_dir.join(format!("{lib}.kicad_sym"));
                if let Some(root) = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| Sexpr::parse(&t).ok())
                {
                    lib_cache.insert(lib.to_string(), root);
                }
            }
            if let Some(root) = lib_cache.get(lib) {
                model = model_from_symbol(root, name, symbol_dir)?;
            }
        }

        // 3. Fall back to the built-in behavioural catalog — the parts-library
        //    stand-in for common active parts (op-amps, the LM13700 OTA) whose
        //    KiCad symbols ship no `Sim.*` model.
        if let Some(m) = model.or_else(|| builtin_model(part)) {
            models.insert(refdes.clone(), m);
        }
    }
    Ok(models)
}

/// Build a [`SpiceModel`] from a part-carried [`SimModel`] (its `Sim.*` fields).
fn model_from_sim(
    sim: &SimModel,
    symbol_dir: &Path,
    label: &str,
) -> Result<Option<SpiceModel>, StageError> {
    build_subckt_model(
        |key| match key {
            "Sim.Device" => Some(sim.device.clone()),
            "Sim.Name" => Some(sim.name.clone()),
            "Sim.Library" => sim.library.clone(),
            "Sim.Pins" => sim.pins.clone(),
            _ => None,
        },
        symbol_dir,
        label,
    )
}

/// Build a [`SpiceModel`] from a symbol's `Sim.*` properties, or `None` if the
/// symbol has no subckt model (i.e. it's a SPICE primitive).
fn model_from_symbol(
    root: &Sexpr,
    part: &str,
    symbol_dir: &Path,
) -> Result<Option<SpiceModel>, StageError> {
    let Some(sym) = root
        .get_all("symbol")
        .into_iter()
        .find(|s| s.nth_atom(1) == Some(part))
    else {
        return Err(StageError::Other(format!(
            "symbol '{part}' not found in library"
        )));
    };
    build_subckt_model(
        |name| {
            sym.get_all("property")
                .into_iter()
                .find(|p| p.nth_atom(1) == Some(name))
                .and_then(|p| p.nth_atom(2))
                .map(str::to_string)
        },
        symbol_dir,
        part,
    )
}

/// Shared: build a subckt [`SpiceModel`] from a `Sim.*` property getter, whatever
/// the source (part-carried fields or a symbol's properties). `None` if the
/// device isn't a SUBCKT (i.e. a SPICE primitive).
fn build_subckt_model(
    prop: impl Fn(&str) -> Option<String>,
    symbol_dir: &Path,
    label: &str,
) -> Result<Option<SpiceModel>, StageError> {
    // Only subckt-modelled devices carry a model; anything else is a primitive.
    if prop("Sim.Device").as_deref() != Some("SUBCKT") {
        return Ok(None);
    }

    let subckt = prop("Sim.Name")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| StageError::Other(format!("{label}: Sim.Device=SUBCKT but no Sim.Name")))?;
    let sim_library =
        prop("Sim.Library").ok_or_else(|| StageError::Other(format!("{label}: no Sim.Library")))?;
    let sim_pins =
        prop("Sim.Pins").ok_or_else(|| StageError::Other(format!("{label}: no Sim.Pins")))?;

    let include = expand_symbol_dir(&sim_library, symbol_dir);

    // Sim.Pins: "1=in+ 2=in- 3=vcc 4=vee 5=out" → (pin, terminal) pairs.
    let pin_to_terminal: Vec<(&str, &str)> = sim_pins
        .split_whitespace()
        .filter_map(|tok| tok.split_once('='))
        .collect();

    // Order the part's pins by the subckt's declared terminal order.
    let terminals = subckt_terminals(&include, &subckt)?;
    let mut pin_order = Vec::with_capacity(terminals.len());
    for terminal in &terminals {
        let pin = pin_to_terminal
            .iter()
            .find(|(_, t)| t == terminal)
            .map(|(p, _)| p.to_string())
            .ok_or_else(|| {
                StageError::Other(format!(
                    "{label}: subckt terminal '{terminal}' missing from Sim.Pins"
                ))
            })?;
        pin_order.push(pin);
    }

    Ok(Some(SpiceModel::Subckt {
        subckt,
        include,
        pin_order,
        params: None,
    }))
}

/// Structured data read from a KiCad symbol — pins plus key properties. Used by
/// the parts library's KiCad-library fetch source ([`crate::fetch`], okm.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolData {
    /// `(pin_number, pin_name)`, sorted by pin number.
    pub pins: Vec<(String, String)>,
    pub datasheet: Option<String>,
    pub description: Option<String>,
}

/// Find which `<lib>.kicad_sym` in `symbol_dir` defines a symbol named `part`.
pub fn find_symbol_lib(symbol_dir: &Path, part: &str) -> Result<Option<String>, StageError> {
    let needle = format!("(symbol \"{part}\"");
    let mut libs: Vec<PathBuf> = std::fs::read_dir(symbol_dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("kicad_sym"))
        .collect();
    libs.sort();
    for path in libs {
        if std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains(&needle)
        {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                return Ok(Some(stem.to_string()));
            }
        }
    }
    Ok(None)
}

/// Read a symbol's pins + `Datasheet`/`Description` properties from a library.
/// Pins are collected from the symbol's nested unit sub-symbols and de-duplicated.
pub fn read_symbol(
    symbol_dir: &Path,
    lib: &str,
    part: &str,
) -> Result<Option<SymbolData>, StageError> {
    let path = symbol_dir.join(format!("{lib}.kicad_sym"));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| StageError::Other(format!("reading {}: {e}", path.display())))?;
    let root = Sexpr::parse(&text)
        .map_err(|e| StageError::Other(format!("parsing {}: {e}", path.display())))?;
    let Some(sym) = root
        .get_all("symbol")
        .into_iter()
        .find(|s| s.nth_atom(1) == Some(part))
    else {
        return Ok(None);
    };

    let prop = |name: &str| {
        sym.get_all("property")
            .into_iter()
            .find(|p| p.nth_atom(1) == Some(name))
            .and_then(|p| p.nth_atom(2))
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };

    let mut pins: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for unit in sym.get_all("symbol") {
        for pin in unit.get_all("pin") {
            let number = pin
                .get("number")
                .and_then(|n| n.nth_atom(1))
                .unwrap_or_default();
            if number.is_empty() || !seen.insert(number.to_string()) {
                continue;
            }
            let name = pin
                .get("name")
                .and_then(|n| n.nth_atom(1))
                .unwrap_or_default();
            pins.push((number.to_string(), name.to_string()));
        }
    }
    pins.sort_by_key(|p| pin_sort_key(&p.0));

    Ok(Some(SymbolData {
        pins,
        datasheet: prop("Datasheet"),
        description: prop("Description"),
    }))
}

/// Sort key that orders pin numbers numerically when possible, else lexically.
fn pin_sort_key(n: &str) -> (u64, String) {
    (n.parse::<u64>().unwrap_or(u64::MAX), n.to_string())
}

/// Expand a `${…SYMBOL_DIR}` prefix in a `Sim.Library` path to the real dir.
fn expand_symbol_dir(sim_library: &str, symbol_dir: &Path) -> PathBuf {
    if let Some(end) = sim_library.find('}') {
        if sim_library.starts_with("${") {
            let rest = sim_library[end + 1..].trim_start_matches(['/', '\\']);
            return symbol_dir.join(rest);
        }
    }
    PathBuf::from(sim_library)
}

/// Read a SPICE library and return the ordered terminal names of `.subckt name`.
fn subckt_terminals(sp_path: &Path, name: &str) -> Result<Vec<String>, StageError> {
    let text = std::fs::read_to_string(sp_path)
        .map_err(|e| StageError::Other(format!("reading {}: {e}", sp_path.display())))?;
    for line in text.lines() {
        let line = line.trim();
        if !line.to_ascii_lowercase().starts_with(".subckt ") {
            continue;
        }
        let mut toks = line.split_whitespace();
        toks.next(); // ".subckt"
        if toks.next() != Some(name) {
            continue;
        }
        // Terminals run until a `params:` keyword or a `key=value` param.
        let terminals = toks
            .take_while(|t| !t.eq_ignore_ascii_case("params:") && !t.contains('='))
            .map(str::to_string)
            .collect();
        return Ok(terminals);
    }
    Err(StageError::Other(format!(
        "subckt '{name}' not found in {}",
        sp_path.display()
    )))
}

/// Filename of the bundled behavioural model library, written next to the SPICE
/// deck (ngspice `.include`s it by this relative name).
pub const BUILTIN_LIB_NAME: &str = "lob_builtin.lib";

/// The bundled behavioural model library text (op-amp + LM13700 OTA).
const BUILTIN_LIB_TEXT: &str = include_str!("spice/lob_builtin.lib");

/// Write the bundled behavioural model library into `dir` so a deck that
/// `.include`s [`BUILTIN_LIB_NAME`] can find it. Idempotent.
pub fn write_builtin_lib(dir: &Path) -> std::io::Result<()> {
    std::fs::write(dir.join(BUILTIN_LIB_NAME), BUILTIN_LIB_TEXT)
}

/// The built-in behavioural [`SpiceModel`] for a common active part whose KiCad
/// symbol carries no `Sim.*` model — the parts-library stand-in. Matched on the
/// part's library id / value. `None` for anything not in the catalogue.
fn builtin_model(part: &crate::model::Part) -> Option<SpiceModel> {
    let hay = format!(
        "{} {}",
        part.library_part.as_deref().unwrap_or(""),
        part.value
    )
    .to_ascii_uppercase();
    // (subckt name, part pin numbers in the subckt's terminal order).
    let (subckt, pins): (&str, &[&str]) = if hay.contains("LM13700") || hay.contains("LM13600") {
        ("LM13700", &["1", "3", "4", "5", "6", "7", "8", "11"])
    } else if hay.contains("TL072") || hay.contains("TL082") || hay.contains("NE5532") {
        ("TL072", &["1", "2", "3", "4", "5", "6", "7", "8"])
    } else {
        return None;
    };
    Some(SpiceModel::Subckt {
        subckt: subckt.into(),
        include: PathBuf::from(BUILTIN_LIB_NAME),
        pin_order: pins.iter().map(|s| s.to_string()).collect(),
        params: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_models_active_parts_without_a_symbol_model() {
        // An LM13700 OTA + TL072 op-amp whose KiCad symbols carry no Sim.* model
        // resolve to the built-in behavioural subckts; primitives resolve to none.
        let c = crate::model::Circuit {
            name: "ota".into(),
            parts: vec![
                crate::model::Part::new("U1", "LM13700"),
                crate::model::Part::new("U2", "TL072"),
                crate::model::Part::new("R1", "1k"),
            ],
            nets: vec![],
        };
        let models = resolve_models(&c, Path::new("/nonexistent")).unwrap();
        let subckt = |r: &str| match models.get(r) {
            Some(SpiceModel::Subckt { subckt, .. }) => Some(subckt.as_str()),
            _ => None,
        };
        assert_eq!(subckt("U1"), Some("LM13700"));
        assert_eq!(subckt("U2"), Some("TL072"));
        assert!(!models.contains_key("R1"), "a resistor carries no model");
    }

    #[test]
    fn expands_symbol_dir_variable() {
        let dir = Path::new("/opt/kicad/symbols");
        assert_eq!(
            expand_symbol_dir("${KICAD9_SYMBOL_DIR}/Simulation_SPICE.sp", dir),
            Path::new("/opt/kicad/symbols/Simulation_SPICE.sp")
        );
        assert_eq!(
            expand_symbol_dir("/abs/path/models.sp", dir),
            Path::new("/abs/path/models.sp")
        );
    }
}
