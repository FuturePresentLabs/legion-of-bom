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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
        let Some(library_part) = part.library_part.as_deref() else {
            continue;
        };
        let Some((lib, name)) = library_part.split_once(':') else {
            continue;
        };

        if !lib_cache.contains_key(lib) {
            let path = symbol_dir.join(format!("{lib}.kicad_sym"));
            let text = std::fs::read_to_string(&path)
                .map_err(|e| StageError::Other(format!("reading {}: {e}", path.display())))?;
            let root = Sexpr::parse(&text)
                .map_err(|e| StageError::Other(format!("parsing {}: {e}", path.display())))?;
            lib_cache.insert(lib.to_string(), root);
        }

        if let Some(model) = model_from_symbol(&lib_cache[lib], name, symbol_dir)? {
            models.insert(part.refdes.0.clone(), model);
        }
    }
    Ok(models)
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
    let prop = |name: &str| {
        sym.get_all("property")
            .into_iter()
            .find(|p| p.nth_atom(1) == Some(name))
            .and_then(|p| p.nth_atom(2))
    };

    // Only subckt-modelled devices carry a model; anything else is a primitive.
    if prop("Sim.Device") != Some("SUBCKT") {
        return Ok(None);
    }

    let subckt = prop("Sim.Name")
        .ok_or_else(|| StageError::Other(format!("{part}: Sim.Device=SUBCKT but no Sim.Name")))?
        .to_string();
    let sim_library =
        prop("Sim.Library").ok_or_else(|| StageError::Other(format!("{part}: no Sim.Library")))?;
    let sim_pins =
        prop("Sim.Pins").ok_or_else(|| StageError::Other(format!("{part}: no Sim.Pins")))?;

    let include = expand_symbol_dir(sim_library, symbol_dir);

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
                    "{part}: subckt terminal '{terminal}' missing from Sim.Pins"
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

#[cfg(test)]
mod tests {
    use super::*;

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
