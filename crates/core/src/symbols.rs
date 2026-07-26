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

// ---------------------------------------------------------------------------
//  Symbol *graphics* — the drawn body, for the schematic view (n5l)
// ---------------------------------------------------------------------------

/// How a symbol shape is filled. KiCad has three modes and they mean different
/// things: `outline` paints it solid in the line colour (a jack's plug tip),
/// `background` paints it in the *sheet* colour so it occludes what's behind
/// without going black, and `none` leaves it open. Collapsing these to a boolean
/// turns every background-filled body into a black blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymFill {
    None,
    Background,
    Outline,
}

/// A drawable primitive from a symbol body, in KiCad symbol space (mm, Y **up**).
#[derive(Debug, Clone, PartialEq)]
pub enum SymShape {
    Rect {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        fill: SymFill,
    },
    Poly {
        pts: Vec<(f64, f64)>,
        fill: SymFill,
    },
    Circle {
        cx: f64,
        cy: f64,
        r: f64,
        fill: SymFill,
    },
    /// A three-point arc (start → mid → end), as KiCad stores it.
    Arc {
        start: (f64, f64),
        mid: (f64, f64),
        end: (f64, f64),
    },
}

/// A symbol pin, in symbol space (mm, Y up).
///
/// In KiCad's format a pin's `(at x y angle)` is its **connection point** — the
/// free end a wire attaches to — and the pin graphic runs from there *into* the
/// body along `angle`. (A `Device:R` pin sits at y = 3.81 while the body top is
/// 2.54: exactly one pin length away, pointing back at the body.) Reading this
/// backwards attaches every wire to the body edge instead of the pin end.
#[derive(Debug, Clone, PartialEq)]
pub struct SymPin {
    /// Pin identifier. KiCad calls it the "number" but it is often a name — an
    /// audio jack's pins are `T`, `S`, `TN` — and the netlist uses the same token,
    /// so the two match directly.
    pub number: String,
    /// The pin's human name as the symbol declares it — `IABC`, `+`, `-`, `OUT`.
    /// Empty when the symbol gives none, or names it `~`, KiCad's "no name".
    ///
    /// This is the only source for it: netlists carry no `pinfunction`, so a
    /// dropped name here cannot be recovered downstream. On an LM13700 it is the
    /// difference between a rectangle labelled 1,3,4,5 and one that shows the
    /// reader that pin 4 is In− and pin 5 the output.
    pub name: String,
    /// The connection point: where a wire attaches.
    pub x: f64,
    pub y: f64,
    /// Direction from the connection point toward the body, degrees CCW (0 = +X).
    pub angle: f64,
    pub length: f64,
}

impl SymPin {
    /// Where the pin meets the symbol body — the inner end of the drawn lead.
    pub fn body_end(&self) -> (f64, f64) {
        let r = self.angle.to_radians();
        (
            self.x + self.length * r.cos(),
            self.y + self.length * r.sin(),
        )
    }

    /// Unit vector pointing *away* from the body, so a wire can leave along the
    /// pin and read as continuing it rather than crossing it.
    pub fn outward(&self) -> (f64, f64) {
        let r = self.angle.to_radians();
        (-r.cos(), -r.sin())
    }
}

/// A symbol's drawn body plus its pins.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolGraphics {
    pub shapes: Vec<SymShape>,
    pub pins: Vec<SymPin>,
    /// Highest unit index in the definition. `1` is a plain single-unit part; more
    /// means the part is drawn as several separate units on a real schematic (an
    /// LM2904 is two amplifiers plus a power unit), which needs pin-to-unit
    /// splitting the caller may not want to attempt.
    pub units: usize,
    /// Every pin's name, keyed by number, harvested across **all** units — not
    /// just the one drawn in [`pins`](Self::pins).
    ///
    /// A multi-unit part keeps most of its pins in units 2+, which are declined
    /// for drawing; those are precisely the parts that fall back to a labelled box
    /// and most need their names. Reading names only from unit 1 left an LM13700
    /// box showing 1,3,4,5,7,8 and nothing else. Pins the symbol leaves unnamed
    /// (`~`, as KiCad names most op-amp outputs) are absent rather than empty.
    pub pin_names: HashMap<String, String>,
    /// The symbol asks for its pin names not to be drawn — `(pin_names … hide)`.
    /// Honour it: a resistor whose pins are labelled is noise, and the symbol
    /// author already made that call.
    pub hide_pin_names: bool,
}

impl SymbolGraphics {
    /// Bounding box of the body **and** pin tips: `(x0, y0, x1, y1)`.
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        let mut b = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        let mut add = |x: f64, y: f64| {
            b.0 = b.0.min(x);
            b.1 = b.1.min(y);
            b.2 = b.2.max(x);
            b.3 = b.3.max(y);
        };
        for s in &self.shapes {
            match s {
                SymShape::Rect { x0, y0, x1, y1, .. } => {
                    add(*x0, *y0);
                    add(*x1, *y1);
                }
                SymShape::Poly { pts, .. } => pts.iter().for_each(|&(x, y)| add(x, y)),
                SymShape::Circle { cx, cy, r, .. } => {
                    add(cx - r, cy - r);
                    add(cx + r, cy + r);
                }
                SymShape::Arc { start, mid, end } => {
                    for p in [start, mid, end] {
                        add(p.0, p.1);
                    }
                }
            }
        }
        for p in &self.pins {
            add(p.x, p.y);
            let (bx, by) = p.body_end();
            add(bx, by);
        }
        if b.0 > b.2 {
            (0.0, 0.0, 0.0, 0.0)
        } else {
            b
        }
    }
}

/// Read a symbol's drawn body from `<lib>.kicad_sym`, following `(extends …)`
/// inheritance (KiCad defines e.g. `TL072` as an extension of `LM2904`).
///
/// Returns `None` when the library or symbol isn't there — symbol libraries ship
/// with KiCad, so a machine without it simply gets no graphics and the caller
/// falls back to its own rendering.
pub fn read_symbol_graphics(symbol_dir: &Path, lib: &str, part: &str) -> Option<SymbolGraphics> {
    let text = std::fs::read_to_string(symbol_dir.join(format!("{lib}.kicad_sym"))).ok()?;
    let root = Sexpr::parse(&text).ok()?;
    let find = |name: &str| {
        root.get_all("symbol")
            .into_iter()
            .find(|s| s.nth_atom(1) == Some(name))
    };

    // Follow the inheritance chain to the definition that carries the drawing.
    let mut sym = find(part)?;
    let mut hops = 0;
    while let Some(base) = sym.field("extends") {
        if hops > 8 {
            break; // cycle guard
        }
        match find(base) {
            Some(next) => sym = next,
            None => break,
        }
        hops += 1;
    }

    let num = |e: Option<&Sexpr>, i: usize| -> Option<f64> { e?.nth_atom(i)?.parse().ok() };
    let xy = |e: Option<&Sexpr>| -> Option<(f64, f64)> { Some((num(e, 1)?, num(e, 2)?)) };
    let fill_of = |e: &Sexpr| -> SymFill {
        match e.get("fill").and_then(|f| f.field("type")) {
            Some("outline") => SymFill::Outline,
            Some("background") => SymFill::Background,
            _ => SymFill::None,
        }
    };

    let mut shapes = Vec::new();
    let mut pins: Vec<SymPin> = Vec::new();
    let mut units = 1usize;
    let mut pin_names: HashMap<String, String> = HashMap::new();
    // `(pin_names (offset 0) hide)` — the `hide` may be a bare atom or `(hide yes)`.
    let hide_pin_names = sym.get("pin_names").is_some_and(|p| {
        p.as_list().is_some_and(|l| {
            l.iter().any(|c| {
                c.as_atom() == Some("hide")
                    || (c.head() == Some("hide") && c.nth_atom(1) != Some("no"))
            })
        })
    });

    // Body graphics live in nested unit sub-symbols named `<NAME>_<unit>_<style>`.
    // Unit 0 is common to every unit; unit 1 is the first real one.
    for unit in sym.get_all("symbol") {
        let uname = unit.nth_atom(1).unwrap_or_default();
        let idx: usize = uname
            .rsplit('_')
            .nth(1)
            .and_then(|n| n.parse().ok())
            .unwrap_or(1);
        units = units.max(idx);

        // Names come from every unit, including the ones below that we decline to
        // draw — a boxed multi-unit part still shows all its pins, so it still
        // needs all their names.
        for p in unit.get_all("pin") {
            let Some(number) = p.get("number").and_then(|n| n.nth_atom(1)) else {
                continue;
            };
            if let Some(name) = p
                .get("name")
                .and_then(|n| n.nth_atom(1))
                .filter(|n| *n != "~" && !n.is_empty())
            {
                pin_names.insert(number.to_string(), name.to_string());
            }
        }

        if idx > 1 {
            continue; // additional units are drawn separately on a real schematic
        }

        for r in unit.get_all("rectangle") {
            if let (Some((x0, y0)), Some((x1, y1))) = (xy(r.get("start")), xy(r.get("end"))) {
                shapes.push(SymShape::Rect {
                    x0,
                    y0,
                    x1,
                    y1,
                    fill: fill_of(r),
                });
            }
        }
        for p in unit.get_all("polyline") {
            if let Some(pts) = p.get("pts") {
                let pts: Vec<(f64, f64)> = pts
                    .get_all("xy")
                    .into_iter()
                    .filter_map(|e| xy(Some(e)))
                    .collect();
                if pts.len() >= 2 {
                    shapes.push(SymShape::Poly {
                        pts,
                        fill: fill_of(p),
                    });
                }
            }
        }
        for c in unit.get_all("circle") {
            if let (Some((cx, cy)), Some(r)) = (xy(c.get("center")), num(c.get("radius"), 1)) {
                shapes.push(SymShape::Circle {
                    cx,
                    cy,
                    r,
                    fill: fill_of(c),
                });
            }
        }
        for a in unit.get_all("arc") {
            if let (Some(start), Some(mid), Some(end)) =
                (xy(a.get("start")), xy(a.get("mid")), xy(a.get("end")))
            {
                shapes.push(SymShape::Arc { start, mid, end });
            }
        }
        for p in unit.get_all("pin") {
            let at = p.get("at");
            let (Some(x), Some(y)) = (num(at, 1), num(at, 2)) else {
                continue;
            };
            let number = p
                .get("number")
                .and_then(|n| n.nth_atom(1))
                .unwrap_or_default()
                .to_string();
            if number.is_empty() {
                continue;
            }
            // `~` is KiCad's explicit "this pin has no name" — treat it as absent
            // rather than drawing a tilde on the schematic.
            let name = p
                .get("name")
                .and_then(|n| n.nth_atom(1))
                .filter(|n| *n != "~")
                .unwrap_or_default()
                .to_string();
            pins.push(SymPin {
                number,
                name,
                x,
                y,
                angle: num(at, 3).unwrap_or(0.0),
                length: p
                    .get("length")
                    .and_then(|l| l.nth_atom(1))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(2.54),
            });
        }
    }

    if shapes.is_empty() && pins.is_empty() {
        return None;
    }
    Some(SymbolGraphics {
        shapes,
        pins,
        units,
        pin_names,
        hide_pin_names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway symbol library, cleaned up on drop.
    struct TempLib(PathBuf);
    impl Drop for TempLib {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn temp_lib(tag: &str, body: &str) -> TempLib {
        let dir = std::env::temp_dir().join(format!(
            "lob-sym-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("T.kicad_sym"), body).unwrap();
        TempLib(dir)
    }

    /// KiCad has three fill modes and they are not interchangeable: `outline` is
    /// solid in the line colour, `background` paints the sheet colour so a body
    /// occludes what is behind it, `none` is open. Treating them as one boolean
    /// turned every background-filled symbol (an audio jack) into a black blob.
    #[test]
    fn reads_body_shapes_pins_and_distinguishes_fill_modes() {
        let lib = temp_lib(
            "fill",
            r#"(kicad_symbol_lib (symbol "P"
                 (symbol "P_0_1"
                   (rectangle (start -1 -2) (end 1 2) (fill (type none)))
                   (polyline (pts (xy 0 0) (xy 1 1)) (fill (type background)))
                   (circle (center 0 0) (radius 0.5) (fill (type outline))))
                 (symbol "P_1_1"
                   (pin passive line (at 0 3.81 270) (length 1.27) (number "1"))
                   (pin passive line (at 0 -3.81 90) (length 1.27) (number "2")))))"#,
        );
        let g = read_symbol_graphics(&lib.0, "T", "P").expect("graphics");
        assert_eq!(g.units, 1);
        assert_eq!(g.pins.len(), 2);
        let fills: Vec<SymFill> = g
            .shapes
            .iter()
            .map(|s| match s {
                SymShape::Rect { fill, .. }
                | SymShape::Poly { fill, .. }
                | SymShape::Circle { fill, .. } => *fill,
                SymShape::Arc { .. } => SymFill::None,
            })
            .collect();
        assert!(fills.contains(&SymFill::None));
        assert!(fills.contains(&SymFill::Background));
        assert!(fills.contains(&SymFill::Outline));

        // A pin's `(at …)` IS the wire connection point, and the drawn lead runs
        // from there *into* the body, one `length` along `angle`. Reading it the
        // other way round attaches every wire to the body edge instead of the pin.
        let p1 = g.pins.iter().find(|p| p.number == "1").unwrap();
        assert_eq!((p1.x, p1.y), (0.0, 3.81), "connection point is the `at`");
        let (bx, by) = p1.body_end();
        assert!(
            bx.abs() < 1e-9 && (by - 2.54).abs() < 1e-9,
            "body end sits one pin length toward the body, got ({bx},{by})"
        );
        // …and a wire leaves the opposite way, continuing the pin outward.
        let (ox, oy) = p1.outward();
        assert!(ox.abs() < 1e-9 && (oy - 1.0).abs() < 1e-9, "({ox},{oy})");
    }

    /// KiCad defines many parts by inheritance — `TL072` is `(extends "LM2904")` —
    /// so the graphics live on the base symbol and the chain must be followed.
    #[test]
    fn follows_extends_to_the_symbol_that_holds_the_drawing() {
        let lib = temp_lib(
            "ext",
            r#"(kicad_symbol_lib
                 (symbol "Base"
                   (symbol "Base_0_1" (rectangle (start -1 -1) (end 1 1) (fill (type none))))
                   (symbol "Base_1_1" (pin passive line (at 0 2 270) (length 1) (number "1"))))
                 (symbol "Derived" (extends "Base")))"#,
        );
        let g = read_symbol_graphics(&lib.0, "T", "Derived").expect("inherited graphics");
        assert_eq!(g.shapes.len(), 1);
        assert_eq!(g.pins.len(), 1);
    }

    /// A part drawn as several units (an op-amp is two amplifiers plus a power
    /// unit) needs its pins split across separately-placed units, so callers are
    /// told the unit count and can decline.
    #[test]
    fn reports_multi_unit_parts() {
        let lib = temp_lib(
            "units",
            r#"(kicad_symbol_lib (symbol "Dual"
                 (symbol "Dual_1_1" (pin passive line (at 0 2 270) (length 1) (number "1")))
                 (symbol "Dual_2_1" (pin passive line (at 0 2 270) (length 1) (number "5")))
                 (symbol "Dual_3_1" (pin power_in line (at 0 2 270) (length 1) (number "8")))))"#,
        );
        let g = read_symbol_graphics(&lib.0, "T", "Dual").expect("graphics");
        assert_eq!(g.units, 3, "three units detected");
        // Only unit 1 is drawn; the rest belong to separate placements.
        assert_eq!(g.pins.len(), 1);
    }

    /// Pin *names* come from every unit, not just the drawn one. A multi-unit part
    /// keeps most of its pins in units 2+ and is exactly the part that falls back
    /// to a labelled box, so harvesting only unit 1 left an LM13700 box showing
    /// bare numbers (`legion-of-bom-sto`).
    #[test]
    fn pin_names_are_read_from_every_unit_not_just_the_drawn_one() {
        let lib = temp_lib(
            "names",
            r#"(kicad_symbol_lib (symbol "OTA"
                 (symbol "OTA_1_1" (pin input line (at 0 2 270) (length 1)
                    (name "+") (number "3")))
                 (symbol "OTA_2_1" (pin input line (at 0 2 270) (length 1)
                    (name "DIODE_BIAS") (number "2")))
                 (symbol "OTA_3_1" (pin output line (at 0 2 270) (length 1)
                    (name "~") (number "5")))))"#,
        );
        let g = read_symbol_graphics(&lib.0, "T", "OTA").expect("graphics");
        assert_eq!(g.units, 3);
        assert_eq!(g.pins.len(), 1, "still only unit 1 is drawn");
        assert_eq!(g.pin_names.get("3").map(String::as_str), Some("+"));
        assert_eq!(
            g.pin_names.get("2").map(String::as_str),
            Some("DIODE_BIAS"),
            "a name from unit 2 is kept even though the unit is not drawn"
        );
        assert!(
            !g.pin_names.contains_key("5"),
            "`~` is KiCad's explicit no-name and must not become a label"
        );
        assert!(!g.hide_pin_names);
    }

    /// `(pin_names … hide)` is the symbol author saying "do not label these" —
    /// what keeps a resistor from growing two labels.
    #[test]
    fn a_symbol_can_ask_for_its_pin_names_not_to_be_drawn() {
        let lib = temp_lib(
            "hidden",
            r#"(kicad_symbol_lib (symbol "R" (pin_names (offset 0) hide)
                 (symbol "R_1_1" (pin passive line (at 0 2 270) (length 1)
                    (name "A") (number "1")))))"#,
        );
        let g = read_symbol_graphics(&lib.0, "T", "R").expect("graphics");
        assert!(g.hide_pin_names);
        // The name is still *read* — hiding is a drawing decision, not a data one.
        assert_eq!(g.pin_names.get("1").map(String::as_str), Some("A"));
    }

    #[test]
    fn missing_symbol_or_library_is_not_an_error() {
        let lib = temp_lib("none", r#"(kicad_symbol_lib (symbol "X"))"#);
        assert!(read_symbol_graphics(&lib.0, "T", "Nope").is_none());
        assert!(read_symbol_graphics(&lib.0, "NoSuchLib", "X").is_none());
    }

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
