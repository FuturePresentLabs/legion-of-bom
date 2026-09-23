//! Synthesis: a brief becomes a circuit by typed decisions over the catalog
//! (legion-of-bom-uvdm).
//!
//! Nothing here knows any board, or any domain: what a brief can ask for is
//! the catalog's feature vocabulary (`catalog/features.json`), and what can
//! meet it is the catalog's parts. The steps:
//!
//! 1. **Requirements** — each feature a yes/no typed decision from the brief.
//! 2. **Parts** — every slot is filled by [`pick`] from the catalog parts that
//!    can fill it: one candidate is *derived*, several are a typed **choice**.
//!    The `function` slot's candidates are the minimal sets of parts covering
//!    the required roles; then the MCU (one with a master for every bus those
//!    parts are slaves on), whatever any chosen part `needs` (a crystal),
//!    the rail regulators, and a connector per port.
//! 3. **Bindings** — each bus is bound to one of the MCU's peripheral
//!    instances that can carry every signal it needs (a choice); each option
//!    tells the decider which pins it would use and on which side of the
//!    package they sit. Pins follow the KiCad symbol's alternates, lowest-
//!    numbered free pin first; control lines (`gpio`, a missing `cs`) take
//!    free port pins.
//!
//! The [`DesignSpec`] records every outcome plus the catalog's fingerprint, so
//! [`circuit`] — spec to SKiDL — is a pure function of it, and refuses a
//! catalog that has changed since the decisions were made.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ooda::{Client, Criteria, Question, Request, Trace};
use serde::{Deserialize, Serialize};

use crate::catalog::{
    Catalog, CatalogPart, Endpoint, Interface, PartSymbol, Scoped, Signal, Subcircuit,
};
use crate::oscillator::{fmt_pf, load_cap_pf, round_e12_pf};
use crate::skidl_emit::{Circuit, EmitPart, PinRef, SymbolSrc};
use crate::spec::{expect_choice, expect_noul, SpecError};
use crate::stage::StageError;

/// How one slot came to be filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    pub chosen: Vec<String>,
    /// `decided` (a typed choice among several) or `derived` (the only one).
    pub how: String,
}

/// Every decision and derivation a design rests on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesignSpec {
    pub brief: String,
    pub requirements: BTreeMap<String, bool>,
    /// Slot → parts: `function`, `mcu`, `needs:hse`, `rail:+3V3`, `port:line_out`, …
    pub parts: BTreeMap<String, Selection>,
    /// Bus kind → MCU peripheral instance: `i2s` → `SAI1`.
    pub bindings: BTreeMap<String, Selection>,
    /// The catalog these were chosen from ([`Catalog::fingerprint`]).
    pub catalog: String,
}

/// Why synthesis could not go on.
#[derive(Debug, thiserror::Error)]
pub enum SynthError {
    #[error(transparent)]
    Decision(#[from] SpecError),
    #[error(transparent)]
    Stage(#[from] StageError),
    #[error("no catalog part can fill {0}")]
    Unfilled(String),
    #[error("the catalog changed since this spec was decided (spec {spec}, catalog {now}): decide again")]
    StaleCatalog { spec: String, now: String },
    #[error("{0}")]
    Invalid(String),
}

/// The board's supply input net.
const SUPPLY_NET: &str = "+5V";

/// Fill a slot: one candidate is derived, several are a typed choice.
fn pick(
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
    key: &str,
    question: &str,
    options: Vec<(String, String)>,
) -> Result<Selection, SynthError> {
    match options.len() {
        0 => Err(SynthError::Unfilled(key.to_string())),
        1 => Ok(Selection {
            chosen: split(&options[0].0),
            how: "derived".into(),
        }),
        _ => {
            let criteria: Criteria = options
                .iter()
                .map(|(k, d)| (k.as_str(), d.clone()))
                .collect();
            let request = Request::new(serde_json::json!({ "brief": brief }))
                .with(key, Question::choice(question, criteria));
            let outcome = client.decide(&request).map_err(SpecError::from)?;
            let chosen = expect_choice(&outcome, trace, key)?;
            if !options.iter().any(|(k, _)| *k == chosen) {
                return Err(
                    SpecError::Unexpected(format!("{key}: {chosen:?} is not an option")).into(),
                );
            }
            Ok(Selection {
                chosen: split(&chosen),
                how: "decided".into(),
            })
        }
    }
}

fn split(key: &str) -> Vec<String> {
    key.split('+').map(str::to_string).collect()
}

/// What fills a role: one catalog part, or a subcircuit around several.
#[derive(Debug, Clone, Copy)]
pub enum Unit<'a> {
    Part(&'a CatalogPart),
    Sub(&'a Subcircuit),
}

impl<'a> Unit<'a> {
    pub fn name(&self) -> &'a str {
        match self {
            Unit::Part(p) => &p.mpn,
            Unit::Sub(s) => &s.name,
        }
    }
    fn summary(&self) -> &'a str {
        match self {
            Unit::Part(p) => &p.summary,
            Unit::Sub(s) => &s.summary,
        }
    }
    fn provides(&self) -> &'a [String] {
        match self {
            Unit::Part(p) => &p.provides,
            Unit::Sub(s) => &s.provides,
        }
    }
}

/// The smallest sets of parts and subcircuits (of one or two) that between
/// them provide every role in `roles`: no set with a unit it could drop.
/// Sorted, stable.
pub fn minimal_covers<'a>(catalog: &'a Catalog, roles: &BTreeSet<&str>) -> Vec<Vec<Unit<'a>>> {
    if roles.is_empty() {
        return vec![Vec::new()];
    }
    let useful: Vec<Unit> = catalog
        .parts
        .iter()
        .map(Unit::Part)
        .chain(catalog.subcircuits.iter().map(Unit::Sub))
        .filter(|u| u.provides().iter().any(|r| roles.contains(r.as_str())))
        .collect();
    let covers = |set: &[Unit]| {
        roles
            .iter()
            .all(|r| set.iter().any(|u| u.provides().iter().any(|x| x == r)))
    };
    let mut out: Vec<Vec<Unit>> = Vec::new();
    for (i, a) in useful.iter().enumerate() {
        if covers(&[*a]) {
            out.push(vec![*a]);
        }
        for b in &useful[i + 1..] {
            let pair = [*a, *b];
            if covers(&pair) && !covers(&[*a]) && !covers(&[*b]) {
                out.push(pair.to_vec());
            }
        }
    }
    for set in &mut out {
        set.sort_by(|x, y| x.name().cmp(y.name()));
    }
    out.sort_by_key(|s| s.iter().map(|u| u.name().to_string()).collect::<Vec<_>>());
    out
}

/// A slave's signal in its master's terms: an I2S slave's `din` is the
/// master's `dout`. SPI's MOSI/MISO and everything else are named for the bus.
fn master_signal(kind: &str, sig: &str) -> String {
    match (kind, sig) {
        ("i2s", "din") => "dout".into(),
        ("i2s", "dout") => "din".into(),
        _ => sig.into(),
    }
}

/// A signal the master need not carry as a peripheral function: a chip select
/// is any free port pin.
fn gpio_ok(sig: &str) -> bool {
    sig == "cs"
}

fn interfaces<'a>(
    p: &'a CatalogPart,
    kind: &'a str,
    role: &'a str,
) -> impl Iterator<Item = &'a Interface> {
    p.interfaces
        .iter()
        .filter(move |i| i.kind == kind && i.role == role)
}

/// The buses a set of parts are slaves on (control lines excepted).
fn buses(parts: &[&CatalogPart]) -> BTreeSet<String> {
    parts
        .iter()
        .flat_map(|p| &p.interfaces)
        .filter(|i| i.role == "slave" && i.kind != "gpio")
        .map(|i| i.kind.clone())
        .collect()
}

/// A part's pins and alternates, and where each pin sits on the package.
struct Pinout {
    pins: Vec<(String, String)>,
    alternates: Vec<(String, String)>,
    quad: bool,
}

impl Pinout {
    fn of(p: &CatalogPart, symbol_dir: &Path) -> Result<Pinout, SynthError> {
        let (pins, alternates) = p.pins(symbol_dir)?;
        let quad = ["QFP", "QFN", "DFN"]
            .iter()
            .any(|k| p.footprint.contains(k));
        Ok(Pinout {
            pins,
            alternates,
            quad,
        })
    }

    fn name_of(&self, number: &str) -> Option<&String> {
        self.pins
            .iter()
            .find(|(n, _)| n == number)
            .map(|(_, name)| name)
    }

    fn number_of(&self, name: &str) -> Option<&String> {
        self.pins
            .iter()
            .find(|(_, n)| n == name)
            .map(|(num, _)| num)
    }

    /// Which side of a quad package a pin sits on, counting from pin 1 at the
    /// top of the left side, counter-clockwise (the IPC numbering).
    fn side(&self, number: &str) -> Option<&'static str> {
        let n: usize = number.parse().ok()?;
        let count = self
            .pins
            .iter()
            .filter(|(num, _)| num.parse::<usize>().is_ok())
            .count();
        let per = count / 4;
        (self.quad && per > 0 && n >= 1 && n <= per * 4)
            .then(|| ["left", "bottom", "right", "top"][(n - 1) / per])
    }

    /// The lowest-numbered pin carrying `alt` that nothing has taken.
    fn free_pin_for(&self, alt: &str, taken: &BTreeSet<String>) -> Option<String> {
        let mut numbers: Vec<&String> = self
            .alternates
            .iter()
            .filter(|(_, a)| a == alt)
            .map(|(n, _)| n)
            .collect();
        numbers.sort_by_key(|n| (n.parse::<u32>().unwrap_or(u32::MAX), n.to_string()));
        numbers
            .into_iter()
            .filter_map(|n| self.name_of(n))
            .find(|name| !taken.contains(*name))
            .cloned()
    }

    /// The lowest-numbered general-purpose port pin (`PA0`, `PB12`, …) that
    /// nothing has taken — where a chip select or a control line goes.
    fn free_gpio(&self, taken: &BTreeSet<String>) -> Option<String> {
        let port_pin = |name: &str| {
            let b = name.as_bytes();
            b.len() >= 3
                && b[0] == b'P'
                && b[1].is_ascii_uppercase()
                && name[2..].chars().all(|c| c.is_ascii_digit())
        };
        let mut pins: Vec<&(String, String)> = self
            .pins
            .iter()
            .filter(|(_, n)| port_pin(n) && !taken.contains(n))
            .collect();
        pins.sort_by_key(|(num, _)| num.parse::<u32>().unwrap_or(u32::MAX));
        pins.first().map(|(_, n)| n.clone())
    }
}

/// The slot one of a subcircuit's slots is filled in.
fn sub_slot(sub: &str, slot: &str) -> String {
    format!("sub:{sub}:{slot}")
}

/// The slot a part's need of `kind` is filled in.
fn need_slot(kind: &str, needer: &str) -> String {
    format!("needs:{kind}:{needer}")
}

/// Whether `source` can meet what `needer` states about its need: a needer
/// with `xtal_freq_hz` takes only a crystal cut for that frequency, one with
/// `xtal_min_hz` / `xtal_max_hz` only a crystal in that range. A stated
/// constraint and a source with no cited `freq_hz` never fit.
fn fits_need(needer: &CatalogPart, source: &CatalogPart) -> bool {
    let want = |k: &str| needer.params.get(k).map(|p| p.value);
    let (exact, min, max) = (
        want("xtal_freq_hz"),
        want("xtal_min_hz"),
        want("xtal_max_hz"),
    );
    if exact.is_none() && min.is_none() && max.is_none() {
        return true;
    }
    let Some(have) = source.params.get("freq_hz").map(|p| p.value) else {
        return false;
    };
    exact.is_none_or(|f| (f - have).abs() < 1.0)
        && min.is_none_or(|m| have >= m)
        && max.is_none_or(|m| have <= m)
}

/// Pin names a part's own support (and its fixed interfaces) already use: not
/// free for a bus or a control line.
fn support_pins(p: &CatalogPart) -> BTreeSet<String> {
    let fixed = p
        .interfaces
        .iter()
        .filter(|i| i.kind == "swd" || i.kind == "hse")
        .flat_map(|i| i.signals.values())
        .filter_map(|s| match s {
            Signal::At(a) => Some(a),
            Signal::Alt { .. } => None,
        });
    p.support
        .iter()
        .flat_map(|s| &s.between)
        .chain(fixed)
        .filter_map(|e| match Endpoint::parse(e) {
            Ok(Endpoint::Pin(n)) => Some(n),
            _ => None,
        })
        .collect()
}

/// Decide a design for `brief` from `catalog`. With `symbol_dir`, each
/// binding option is described by the pins it would use and where they sit.
pub fn design(
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
    catalog: &Catalog,
    symbol_dir: Option<&Path>,
) -> Result<DesignSpec, SynthError> {
    // 1. Requirements.
    let mut request = Request::new(serde_json::json!({ "brief": brief }));
    for f in &catalog.features {
        request = request.with(&f.key, Question::noul(&f.question));
    }
    let outcome = client.decide(&request).map_err(SpecError::from)?;
    let mut requirements = BTreeMap::new();
    for f in &catalog.features {
        requirements.insert(f.key.clone(), expect_noul(&outcome, trace, &f.key)? >= 0.5);
    }
    let wanted: Vec<_> = catalog
        .features
        .iter()
        .filter(|f| requirements[&f.key])
        .collect();

    let mut parts: BTreeMap<String, Selection> = BTreeMap::new();
    let describe = |p: &CatalogPart| (p.mpn.clone(), p.summary.clone());
    let chosen = |parts: &BTreeMap<String, Selection>, slot: &str| -> Vec<&CatalogPart> {
        parts
            .get(slot)
            .map(|s| s.chosen.iter().filter_map(|m| catalog.part(m)).collect())
            .unwrap_or_default()
    };

    // 2a. The function parts: minimal covers of the required roles.
    let roles: BTreeSet<&str> = wanted.iter().map(|f| f.role.as_str()).collect();
    let options = minimal_covers(catalog, &roles)
        .iter()
        .map(|set| {
            (
                set.iter().map(|u| u.name()).collect::<Vec<_>>().join("+"),
                set.iter()
                    .map(|u| u.summary())
                    .collect::<Vec<_>>()
                    .join(" + "),
            )
        })
        .collect();
    let function = pick(
        client,
        trace,
        brief,
        "function",
        "Which parts should the board be built around?",
        options,
    )?;
    parts.insert("function".into(), function.clone());

    // A chosen subcircuit's slots, each filled from the parts that meet it;
    // from here on its members stand in for it.
    let mut function_parts: Vec<&CatalogPart> = Vec::new();
    for name in &function.chosen {
        let Some(sub) = catalog.subcircuit(name) else {
            function_parts.extend(catalog.part(name));
            continue;
        };
        for (slot_name, slot) in &sub.slots {
            let key = sub_slot(&sub.name, slot_name);
            let sel = pick(
                client,
                trace,
                brief,
                &key,
                &format!("Which part should be the {slot_name} of {}?", sub.name),
                catalog.candidates(slot).map(describe).collect(),
            )?;
            parts.insert(key.clone(), sel);
            function_parts.extend(chosen(&parts, &key));
        }
    }

    // 2b. The MCU: a master for every bus the function parts are slaves on.
    let bus_kinds = buses(&function_parts);
    let options = catalog
        .providing("mcu")
        .filter(|m| {
            bus_kinds
                .iter()
                .all(|k| interfaces(m, k, "master").next().is_some())
        })
        .map(describe)
        .collect();
    let mcu_sel = pick(
        client,
        trace,
        brief,
        "mcu",
        "Which microcontroller should the board use?",
        options,
    )?;
    parts.insert("mcu".into(), mcu_sel);
    let mcu = chosen(&parts, "mcu")[0];

    // 2c. Whatever each chosen part needs from outside it (the MCU's crystal,
    // the radio's): one slot per needing part, and only sources that fit it.
    let needers: Vec<&CatalogPart> = function_parts.iter().copied().chain([mcu]).collect();
    for needer in needers {
        for kind in needer
            .interfaces
            .iter()
            .filter(|i| i.role == "needs")
            .map(|i| i.kind.clone())
        {
            let options = catalog
                .parts
                .iter()
                .filter(|p| interfaces(p, &kind, "source").next().is_some())
                .filter(|p| fits_need(needer, p))
                .map(describe)
                .collect();
            let slot = need_slot(&kind, &needer.mpn);
            let sel = pick(
                client,
                trace,
                brief,
                &slot,
                &format!("Which part should supply {kind}?"),
                options,
            )?;
            parts.insert(slot, sel);
        }
    }

    // 2d. Rails every chosen part ties to, other than ground and the input.
    let mut rails: BTreeSet<String> = BTreeSet::new();
    for sub in function.chosen.iter().filter_map(|n| catalog.subcircuit(n)) {
        for e in sub.support.iter().flat_map(|s| &s.between) {
            if let Ok(Scoped::Own(Endpoint::Net(n))) = Scoped::parse(e) {
                if crate::model::is_supply_rail(&n) && n != SUPPLY_NET {
                    rails.insert(n);
                }
            }
        }
    }
    for slot in parts.keys().cloned().collect::<Vec<_>>() {
        for p in chosen(&parts, &slot) {
            for e in p.support.iter().flat_map(|s| &s.between) {
                if let Ok(Endpoint::Net(n)) = Endpoint::parse(e) {
                    if crate::model::is_supply_rail(&n) && n != SUPPLY_NET {
                        rails.insert(n);
                    }
                }
            }
        }
    }
    for rail in &rails {
        let role = format!("rail:{rail}");
        let options = catalog.providing(&role).map(describe).collect();
        let sel = pick(
            client,
            trace,
            brief,
            &role,
            &format!("Which part should supply {rail}?"),
            options,
        )?;
        parts.insert(role, sel);
    }

    // 2e. Connectors: one per wanted feature, the supply, and the debug port.
    let ports = |kind: &str, role: &str| -> Vec<(String, String)> {
        catalog
            .parts
            .iter()
            .filter(|p| {
                p.interfaces
                    .iter()
                    .any(|i| i.kind == kind && i.role == role)
            })
            .map(describe)
            .collect()
    };
    for f in &wanted {
        let slot = format!("port:{}", f.key);
        let q = format!("Which connector carries {}?", f.key);
        let sel = pick(client, trace, brief, &slot, &q, ports(&f.port, "port"))?;
        parts.insert(slot, sel);
    }
    let supply = pick(
        client,
        trace,
        brief,
        "port:supply",
        "Which connector brings in the 5 V supply?",
        ports("power-in", "port"),
    )?;
    parts.insert("port:supply".into(), supply);
    if interfaces(mcu, "swd", "target").next().is_some() {
        let swd = pick(
            client,
            trace,
            brief,
            "port:swd",
            "Which footprint lands the SWD programmer?",
            ports("swd", "debugger"),
        )?;
        parts.insert("port:swd".into(), swd);
    }

    // 3. Bindings, each option described by the pins it would take.
    let pinout = symbol_dir.map(|d| Pinout::of(mcu, d)).transpose()?;
    let mut taken = support_pins(mcu);
    let mut bindings = BTreeMap::new();
    for kind in &bus_kinds {
        let need: BTreeSet<String> = function_parts
            .iter()
            .flat_map(|p| interfaces(p, kind, "slave"))
            .flat_map(|i| i.signals.keys())
            .map(|s| master_signal(kind, s))
            .filter(|s| !gpio_ok(s))
            .collect();
        let describe_pins = |i: &Interface| -> Vec<String> {
            let Some(po) = &pinout else {
                return Vec::new();
            };
            need.iter()
                .filter_map(|s| match &i.signals[s] {
                    Signal::Alt { alt } => po.free_pin_for(alt, &taken).map(|pin| {
                        let num = po.number_of(&pin).cloned().unwrap_or_default();
                        match po.side(&num) {
                            Some(side) => {
                                format!("{} on {pin} (pin {num}, {side} side)", s.to_uppercase())
                            }
                            None => format!("{} on {pin} (pin {num})", s.to_uppercase()),
                        }
                    }),
                    Signal::At(_) => None,
                })
                .collect()
        };
        let options: Vec<(String, String)> = interfaces(mcu, kind, "master")
            .filter(|i| need.iter().all(|s| i.signals.contains_key(s)))
            .filter_map(|i| {
                let name = i.instance.clone()?;
                let pins = describe_pins(i);
                let desc = if pins.is_empty() {
                    format!("{kind} on {name}")
                } else {
                    format!("{kind} on {name}: {}", pins.join(", "))
                };
                Some((name, desc))
            })
            .collect();
        let question = format!(
            "Which {} peripheral should carry the {kind} bus? Prefer one whose pins sit \
             together on one side of the package.",
            mcu.mpn
        );
        let sel = pick(
            client,
            trace,
            brief,
            &format!("bind_{kind}"),
            &question,
            options,
        )?;
        // What this binding takes is not free for the next one.
        if let (Some(po), Some(inst)) = (&pinout, sel.chosen.first()) {
            if let Some(i) = interfaces(mcu, kind, "master")
                .find(|i| i.instance.as_deref() == Some(inst.as_str()))
            {
                for s in &need {
                    if let Signal::Alt { alt } = &i.signals[s] {
                        if let Some(pin) = po.free_pin_for(alt, &taken) {
                            taken.insert(pin);
                        }
                    }
                }
            }
        }
        bindings.insert(kind.clone(), sel);
    }

    Ok(DesignSpec {
        brief: brief.to_string(),
        requirements,
        parts,
        bindings,
        catalog: catalog.fingerprint(),
    })
}

/// Union-find over connection keys, so ties merge nets and a tie that would
/// join two named nets is caught instead of emitted.
#[derive(Default)]
struct Nets {
    parent: BTreeMap<String, String>,
}

impl Nets {
    fn find(&mut self, k: &str) -> String {
        let p = self
            .parent
            .entry(k.to_string())
            .or_insert_with(|| k.to_string())
            .clone();
        if p == k {
            return p;
        }
        let root = self.find(&p);
        self.parent.insert(k.to_string(), root.clone());
        root
    }
    fn union(&mut self, a: &str, b: &str) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            // Keep a named net as the root, so it names the group.
            if rb.starts_with("net:") {
                self.parent.insert(ra, rb);
            } else {
                self.parent.insert(rb, ra);
            }
        }
    }
}

/// A part placed on the board.
struct Placed<'a> {
    reference: String,
    slot: String,
    part: &'a CatalogPart,
    pinout: Pinout,
}

fn pin_key(r: &str, n: &str) -> String {
    format!("{r}.pin.{n}")
}

fn endpoint_key(r: &str, e: &str) -> Result<String, SynthError> {
    Ok(match Endpoint::parse(e).map_err(SynthError::Invalid)? {
        Endpoint::Pin(n) => pin_key(r, &n),
        Endpoint::Net(n) => format!("net:{n}"),
        Endpoint::Node(n) => format!("{r}.node.{n}"),
    })
}

type Taken = BTreeMap<String, BTreeSet<String>>;

/// A signal's key on its part: a named pin or node, or — on an MCU — the
/// lowest-numbered free pin carrying the alternate.
fn resolve(pl: &Placed, s: &Signal, taken: &mut Taken) -> Result<String, SynthError> {
    match s {
        Signal::At(at) => endpoint_key(&pl.reference, at),
        Signal::Alt { alt } => {
            let t = taken.entry(pl.reference.clone()).or_default();
            let pin = pl.pinout.free_pin_for(alt, t).ok_or_else(|| {
                SynthError::Invalid(format!("{}: no free pin carries {alt}", pl.part.mpn))
            })?;
            t.insert(pin.clone());
            Ok(pin_key(&pl.reference, &pin))
        }
    }
}

/// A free MCU port pin, for a chip select or a control line.
fn free_gpio(pl: &Placed, taken: &mut Taken) -> Result<String, SynthError> {
    let t = taken.entry(pl.reference.clone()).or_default();
    let pin = pl
        .pinout
        .free_gpio(t)
        .ok_or_else(|| SynthError::Invalid(format!("{}: no free port pin left", pl.part.mpn)))?;
    t.insert(pin.clone());
    Ok(pin_key(&pl.reference, &pin))
}

/// A subcircuit on the board: its members are placed parts, `scope` says
/// which fills each slot, and its own nodes are keyed under `reference`.
struct PlacedSub<'a> {
    reference: String,
    sub: &'a Subcircuit,
    scope: BTreeMap<String, String>,
}

impl PlacedSub<'_> {
    /// An endpoint of the subcircuit as a connection key: a slot's pin is its
    /// member's pin.
    fn key(&self, e: &str) -> Result<String, SynthError> {
        Ok(match Scoped::parse(e).map_err(SynthError::Invalid)? {
            Scoped::SlotPin { slot, pin } => pin_key(&self.scope[&slot], &pin),
            Scoped::SlotNode { slot, node } => format!("{}.node.{node}", self.scope[&slot]),
            Scoped::Own(Endpoint::Net(n)) => format!("net:{n}"),
            Scoped::Own(Endpoint::Node(n)) => format!("{}.node.{n}", self.reference),
            Scoped::Own(Endpoint::Pin(_)) => unreachable!("Scoped::parse refuses a bare pin"),
        })
    }
}

/// The passive a support entry places (`tie` is not one).
fn passive_kind(part: &str) -> &'static str {
    match part {
        "R" => "R",
        "CP" => "CP",
        "L" => "L",
        _ => "C",
    }
}

fn first_iface(pl: &Placed, kind: &str, role: &str) -> Option<Interface> {
    interfaces(pl.part, kind, role).next().cloned()
}

/// Build the circuit a spec describes — a pure function of the spec, the
/// catalog and the KiCad symbols.
pub fn circuit(
    spec: &DesignSpec,
    catalog: &Catalog,
    symbol_dir: &Path,
) -> Result<Circuit, SynthError> {
    if spec.catalog != catalog.fingerprint() {
        return Err(SynthError::StaleCatalog {
            spec: spec.catalog.clone(),
            now: catalog.fingerprint(),
        });
    }

    // Place every chosen part, in a fixed order so designators are stable.
    let mut next: BTreeMap<char, usize> = BTreeMap::new();
    let mut placed: Vec<Placed> = Vec::new();
    let rank = |s: &str| match s {
        "mcu" => 0,
        "function" => 1,
        s if s.starts_with("sub:") => 1,
        s if s.starts_with("needs:") => 2,
        s if s.starts_with("rail:") => 3,
        _ => 4,
    };
    let mut slots: Vec<&String> = spec.parts.keys().collect();
    slots.sort_by_key(|s| (rank(s), s.to_string()));
    for slot in slots {
        for mpn in &spec.parts[slot].chosen {
            // A subcircuit is placed as its members, in their own slots.
            if catalog.subcircuit(mpn).is_some() {
                continue;
            }
            let part = catalog.part(mpn).ok_or_else(|| {
                SynthError::Invalid(format!("spec names {mpn}, which the catalog lacks"))
            })?;
            let prefix = if part.provides.iter().any(|r| r == "crystal") {
                'Y'
            } else if part.provides.iter().any(|r| r == "connector") {
                'J'
            } else {
                'U'
            };
            let n = next.entry(prefix).or_insert(0);
            *n += 1;
            placed.push(Placed {
                reference: format!("{prefix}{n}"),
                slot: slot.clone(),
                part,
                pinout: Pinout::of(part, symbol_dir)?,
            });
        }
    }
    let in_slot = |slot: &str| -> Vec<usize> {
        (0..placed.len())
            .filter(|&i| placed[i].slot == slot)
            .collect()
    };
    let mcu = *in_slot("mcu")
        .first()
        .ok_or_else(|| SynthError::Unfilled("mcu".into()))?;
    let mut subs: Vec<PlacedSub> = Vec::new();
    for sub in spec.parts["function"]
        .chosen
        .iter()
        .filter_map(|n| catalog.subcircuit(n))
    {
        let mut scope = BTreeMap::new();
        for slot in sub.slots.keys() {
            let key = sub_slot(&sub.name, slot);
            let &m = in_slot(&key)
                .first()
                .ok_or_else(|| SynthError::Unfilled(key.clone()))?;
            scope.insert(slot.clone(), placed[m].reference.clone());
        }
        subs.push(PlacedSub {
            reference: format!("SC{}", subs.len() + 1),
            sub,
            scope,
        });
    }

    let mut nets = Nets::default();
    let mut passives: Vec<(&'static str, String, String, String, String)> = Vec::new();
    let mut taken: Taken = placed
        .iter()
        .map(|pl| (pl.reference.clone(), support_pins(pl.part)))
        .collect();

    // Buses: the bound MCU instance to every slave on it, signal by signal.
    for (kind, sel) in &spec.bindings {
        let instance = &sel.chosen[0];
        let master = interfaces(placed[mcu].part, kind, "master")
            .find(|i| i.instance.as_deref() == Some(instance.as_str()))
            .cloned()
            .ok_or_else(|| {
                SynthError::Invalid(format!("{} has no {kind} {instance}", placed[mcu].part.mpn))
            })?;
        for s in 0..placed.len() {
            if s == mcu {
                continue;
            }
            let Some(slave) = first_iface(&placed[s], kind, "slave") else {
                continue;
            };
            for (sig, at) in &slave.signals {
                let msig = master_signal(kind, sig);
                // A chip select is per slave; every other bus line is shared.
                let bus_net = if gpio_ok(&msig) {
                    format!(
                        "net:{}_{}_{}",
                        kind.to_uppercase(),
                        msig.to_uppercase(),
                        placed[s].reference
                    )
                } else {
                    format!("net:{}_{}", kind.to_uppercase(), msig.to_uppercase())
                };
                if !nets.parent.contains_key(&bus_net) {
                    let mk = match master.signals.get(&msig) {
                        Some(m) => resolve(&placed[mcu], m, &mut taken)?,
                        None if gpio_ok(&msig) => free_gpio(&placed[mcu], &mut taken)?,
                        None => {
                            return Err(SynthError::Invalid(format!(
                                "{instance} cannot carry {kind} {msig}"
                            )))
                        }
                    };
                    nets.union(&bus_net, &mk);
                }
                let sk = resolve(&placed[s], at, &mut taken)?;
                nets.union(&bus_net, &sk);
            }
        }
    }

    // Control lines: each to a free MCU port pin.
    for s in 0..placed.len() {
        if s == mcu {
            continue;
        }
        let gpios: Vec<Interface> = interfaces(placed[s].part, "gpio", "slave")
            .cloned()
            .collect();
        for gpio in gpios {
            for (sig, at) in &gpio.signals {
                let net = format!("net:{}_{}", placed[s].reference, sig.to_uppercase());
                let mk = free_gpio(&placed[mcu], &mut taken)?;
                nets.union(&net, &mk);
                let sk = resolve(&placed[s], at, &mut taken)?;
                nets.union(&net, &sk);
            }
        }
    }

    // SWD: the MCU's debug interface to the programming footprint.
    if let (Some(&dbg), Some(target)) = (
        in_slot("port:swd").first(),
        first_iface(&placed[mcu], "swd", "target"),
    ) {
        let debugger = first_iface(&placed[dbg], "swd", "debugger").ok_or_else(|| {
            SynthError::Invalid("the SWD footprint has no debugger interface".into())
        })?;
        for (sig, at) in &target.signals {
            let net = format!("net:{}", sig.to_uppercase());
            let mk = resolve(&placed[mcu], at, &mut taken)?;
            nets.union(&net, &mk);
            if let Some(d) = debugger.signals.get(sig) {
                let dk = resolve(&placed[dbg], d, &mut taken)?;
                nets.union(&net, &dk);
            }
        }
    }

    // Needs: each part's need to the part filling it; a crystal's load caps
    // are computed from its own load capacitance.
    for n in 0..placed.len() {
        let needs: Vec<Interface> = placed[n]
            .part
            .interfaces
            .iter()
            .filter(|i| i.role == "needs")
            .cloned()
            .collect();
        for need in needs {
            let Some(&src) = in_slot(&need_slot(&need.kind, &placed[n].part.mpn)).first() else {
                continue;
            };
            let source = first_iface(&placed[src], &need.kind, "source").ok_or_else(|| {
                SynthError::Invalid(format!(
                    "{} has no {} source",
                    placed[src].part.mpn, need.kind
                ))
            })?;
            // A needer that trims its crystal load internally takes no caps.
            let internal = placed[n]
                .part
                .params
                .get("xtal_load_internal")
                .is_some_and(|p| p.value != 0.0);
            let load = placed[src]
                .part
                .params
                .get("cl_pf")
                .filter(|_| !internal)
                .map(|p| fmt_pf(round_e12_pf(load_cap_pf(p.value))));
            for (sig, at) in &need.signals {
                let net = format!(
                    "net:{}_{}_{}",
                    placed[n].reference,
                    need.kind.to_uppercase(),
                    sig.to_uppercase()
                );
                let nk = resolve(&placed[n], at, &mut taken)?;
                nets.union(&net, &nk);
                if let Some(s) = source.signals.get(sig) {
                    let sk = resolve(&placed[src], s, &mut taken)?;
                    nets.union(&net, &sk);
                }
                if let Some(cap) = &load {
                    passives.push(("C", cap.clone(), String::new(), net, "net:GND".into()));
                }
            }
        }
    }

    // Ports: each wanted feature's interface to its connector.
    for f in &catalog.features {
        let Some(&j) = in_slot(&format!("port:{}", f.key)).first() else {
            continue;
        };
        // The feature's signals, as keys: a subcircuit's export first (it
        // wraps its members' own), else a function part's interface.
        let mut signals: Vec<(String, String)> = Vec::new();
        let exported = subs.iter().find_map(|ps| {
            ps.sub
                .interfaces
                .iter()
                .find(|i| i.kind == f.interface)
                .map(|i| (ps, i))
        });
        if let Some((ps, iface)) = exported {
            for (sig, s) in &iface.signals {
                let Signal::At(at) = s else {
                    unreachable!("a subcircuit's alternates are refused on load")
                };
                signals.push((sig.clone(), ps.key(at)?));
            }
        } else {
            let (src, iface) = in_slot("function")
                .into_iter()
                .find_map(|a| {
                    placed[a]
                        .part
                        .interfaces
                        .iter()
                        .find(|i| i.kind == f.interface)
                        .map(|i| (a, i.clone()))
                })
                .ok_or_else(|| {
                    SynthError::Invalid(format!("no function part has {}", f.interface))
                })?;
            for (sig, at) in &iface.signals {
                signals.push((sig.clone(), resolve(&placed[src], at, &mut taken)?));
            }
        }
        let port = first_iface(&placed[j], &f.port, "port")
            .ok_or_else(|| SynthError::Invalid(format!("connector has no {} port", f.port)))?;
        let port_sigs: Vec<&Signal> = port.signals.values().collect();
        for (k, (sig, sk)) in signals.iter().enumerate() {
            let net = format!("net:{}_{}", f.net, sig.to_uppercase());
            nets.union(&net, sk);
            let p = port_sigs.get(k).ok_or_else(|| {
                SynthError::Invalid(format!("{} port too narrow for {}", f.port, f.key))
            })?;
            let pk = resolve(&placed[j], p, &mut taken)?;
            nets.union(&net, &pk);
        }
    }
    if let Some(&j) = in_slot("port:supply").first() {
        let port = first_iface(&placed[j], "power-in", "port").ok_or_else(|| {
            SynthError::Invalid("the supply connector has no power-in port".into())
        })?;
        for at in port.signals.values() {
            let pk = resolve(&placed[j], at, &mut taken)?;
            nets.union(&format!("net:{SUPPLY_NET}"), &pk);
        }
    }

    // Every part's own support: ties merge, passives are placed between.
    let mut name_used: BTreeSet<(String, String)> = BTreeSet::new();
    for pl in &placed {
        for s in &pl.part.support {
            let (a, b) = (&s.between[0], &s.between[1]);
            if s.part == "tie" {
                nets.union(
                    &endpoint_key(&pl.reference, a)?,
                    &endpoint_key(&pl.reference, b)?,
                );
                for e in [a, b] {
                    if let Ok(Endpoint::Pin(n)) = Endpoint::parse(e) {
                        name_used.insert((pl.reference.clone(), n));
                    }
                }
                continue;
            }
            let fp = s.footprint.clone().unwrap_or_default();
            let value = s.value.clone().unwrap_or_default();
            let kind = passive_kind(&s.part);
            let repeat = s.between.iter().find_map(|e| match Endpoint::parse(e) {
                Ok(Endpoint::Pin(n)) if s.each_pin => Some(n),
                _ => None,
            });
            match repeat {
                Some(pin) => {
                    let count = pl.pinout.pins.iter().filter(|(_, n)| *n == pin).count();
                    let other = s
                        .between
                        .iter()
                        .find(|e| **e != format!("pin:{pin}"))
                        .expect("two ends");
                    for k in 0..count {
                        let key = format!("{}#{k}", pin_key(&pl.reference, &pin));
                        passives.push((
                            kind,
                            value.clone(),
                            fp.clone(),
                            key,
                            endpoint_key(&pl.reference, other)?,
                        ));
                    }
                }
                None => passives.push((
                    kind,
                    value,
                    fp,
                    endpoint_key(&pl.reference, a)?,
                    endpoint_key(&pl.reference, b)?,
                )),
            }
        }
    }
    // Each subcircuit's own wiring: ties between its slots, passives around
    // them.
    for ps in &subs {
        for s in &ps.sub.support {
            let (a, b) = (ps.key(&s.between[0])?, ps.key(&s.between[1])?);
            if s.part == "tie" {
                nets.union(&a, &b);
                for k in [&a, &b] {
                    if let Some((r, n)) = k.split_once(".pin.") {
                        name_used.insert((r.to_string(), n.to_string()));
                    }
                }
                continue;
            }
            passives.push((
                passive_kind(&s.part),
                s.value.clone().unwrap_or_default(),
                s.footprint.clone().unwrap_or_default(),
                a,
                b,
            ));
        }
    }
    // Interface pins count as named too, so a repeated one joins its siblings.
    for k in nets.parent.keys() {
        if let Some((r, rest)) = k.split_once(".pin.") {
            if !rest.contains('#') {
                name_used.insert((r.to_string(), rest.to_string()));
            }
        }
    }

    // Physical pins: every key naming a pin becomes a connection.
    let mut keyed_pins: BTreeSet<String> = nets
        .parent
        .keys()
        .filter(|k| k.contains(".pin."))
        .cloned()
        .collect();
    for (_, _, _, a, b) in &passives {
        for k in [a, b] {
            if k.contains(".pin.") {
                keyed_pins.insert(k.clone());
            }
        }
    }
    let mut pin_links: Vec<(String, String, PinRef)> = Vec::new();
    for k in &keyed_pins {
        let (reference, rest) = k.split_once(".pin.").expect("a pin key");
        match rest.split_once('#') {
            // One instance of a repeated pin: joined to the rest when the
            // name is tied (VDD), its own net when it is not (VCAP).
            Some((name, idx)) => {
                if name_used.contains(&(reference.to_string(), name.to_string())) {
                    nets.union(k, &pin_key(reference, name));
                    pin_links.push((
                        pin_key(reference, name),
                        reference.into(),
                        PinRef::Name(name.into()),
                    ));
                } else {
                    let idx: usize = idx.parse().expect("an index");
                    pin_links.push((
                        k.clone(),
                        reference.into(),
                        PinRef::NameAt(name.into(), idx),
                    ));
                }
            }
            None => pin_links.push((k.clone(), reference.into(), PinRef::Name(rest.into()))),
        }
    }
    pin_links.sort();
    pin_links.dedup();

    // Name each net after its named member, or its root key; two named nets
    // in one group is a short.
    let mut roots_named: BTreeMap<String, String> = BTreeMap::new();
    for k in nets.parent.keys().cloned().collect::<Vec<_>>() {
        let root = nets.find(&k);
        if let Some(name) = k.strip_prefix("net:") {
            if let Some(prev) = roots_named.insert(root.clone(), name.to_string()) {
                if prev != name {
                    return Err(SynthError::Invalid(format!(
                        "ties short net {prev} to net {name}"
                    )));
                }
            }
        }
    }
    let net_name = |nets: &mut Nets, k: &str| -> String {
        let root = nets.find(k);
        roots_named.get(&root).cloned().unwrap_or_else(|| {
            root.replace(".pin.", "_")
                .replace(".node.", "_")
                .replace('#', "_")
        })
    };

    let mut c = Circuit {
        title: format!("{} — synthesized by legion-of-bom", spec.brief),
        ..Circuit::default()
    };
    for pl in &placed {
        let mut fields = BTreeMap::from([("MPN".to_string(), pl.part.mpn.clone())]);
        if let Some(l) = &pl.part.lcsc {
            fields.insert("LCSC".into(), l.clone());
        }
        if pl.part.sim_excluded {
            fields.insert("Sim.Enable".into(), "0".into());
        }
        c.parts.push(EmitPart {
            reference: pl.reference.clone(),
            symbol: match &pl.part.symbol {
                PartSymbol::Kicad { kicad } => {
                    let (lib, name) = kicad.split_once(':').expect("checked on load");
                    SymbolSrc::Kicad(lib.into(), name.into())
                }
                PartSymbol::Inline { pins } => SymbolSrc::Inline(
                    pins.iter()
                        .map(|p| (p.number.clone(), p.name.clone(), p.io.clone()))
                        .collect(),
                ),
            },
            value: pl.part.mpn.clone(),
            footprint: pl.part.footprint.clone(),
            fields,
        });
    }
    for (key, reference, pin) in pin_links {
        let net = net_name(&mut nets, &key);
        c.connect(&net, &reference, pin);
    }
    let (mut nc, mut nr, mut nl) = (0usize, 0usize, 0usize);
    for (kind, value, fp, a, b) in passives {
        let (prefix, n) = match kind {
            "R" => ("R", &mut nr),
            "L" => ("L", &mut nl),
            _ => ("C", &mut nc),
        };
        *n += 1;
        let reference = format!("{prefix}{n}");
        let (sym, default_fp) = match kind {
            "R" => ("R", "Resistor_SMD:R_0603_1608Metric"),
            "L" => ("L", "Inductor_SMD:L_0603_1608Metric"),
            "CP" => (
                "C_Polarized",
                "Capacitor_Tantalum_SMD:CP_EIA-3528-21_Kemet-B",
            ),
            _ => ("C", "Capacitor_SMD:C_0603_1608Metric"),
        };
        c.parts.push(EmitPart {
            reference: reference.clone(),
            symbol: SymbolSrc::Kicad("Device".into(), sym.into()),
            value,
            footprint: if fp.is_empty() { default_fp.into() } else { fp },
            fields: BTreeMap::new(),
        });
        let (na, nb) = (net_name(&mut nets, &a), net_name(&mut nets, &b));
        c.connect(&na, &reference, PinRef::Num(1));
        c.connect(&nb, &reference, PinRef::Num(2));
    }
    Ok(c)
}

/// Every reading no person has confirmed yet, across the parts and
/// subcircuits a spec uses.
pub fn unconfirmed(spec: &DesignSpec, catalog: &Catalog) -> Vec<String> {
    let names: BTreeSet<&String> = spec.parts.values().flat_map(|s| &s.chosen).collect();
    let mut out: Vec<String> = names
        .iter()
        .filter_map(|m| catalog.part(m))
        .flat_map(|p| p.unconfirmed())
        .chain(
            names
                .iter()
                .filter_map(|m| catalog.subcircuit(m))
                .flat_map(|s| s.unconfirmed()),
        )
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::default_catalog_dir;

    fn catalog() -> Catalog {
        Catalog::load(&default_catalog_dir()).expect("catalog loads")
    }

    fn noul(p: f64) -> String {
        format!(r#"{{"type":"boolean","probability":{p}}}"#)
    }

    fn choice(c: &str) -> String {
        format!(
            r#"{{"type":"choice","choice":"{c}","confidence":0.9,"probabilities":{{"{c}":0.9}}}}"#
        )
    }

    /// A decider that answers whatever it is asked, by rule: yes to the listed
    /// features, the preferred option where a test cares, the first option
    /// otherwise. The answers go through ooda's own wire format, so a test
    /// holds however many decisions a growing catalog turns into choices.
    struct Decider {
        yes: &'static [&'static str],
        prefer: &'static [(&'static str, &'static str)],
    }

    impl ooda::Client for Decider {
        fn decide(&self, request: &Request) -> Result<ooda::Outcome, ooda::Error> {
            let answers: Vec<String> = request
                .questions
                .iter()
                .map(|(k, q)| {
                    let a = match q {
                        Question::Choice { criteria, .. } => {
                            let first = criteria.keys().next().unwrap_or_default().to_string();
                            let c = self
                                .prefer
                                .iter()
                                .find(|(key, _)| key == k)
                                .map(|(_, c)| c.to_string())
                                .unwrap_or(first);
                            choice(&c)
                        }
                        _ => noul(if self.yes.contains(&k.as_str()) {
                            0.9
                        } else {
                            0.1
                        }),
                    };
                    format!(r#""{k}": {a}"#)
                })
                .collect();
            ooda::ScriptedClient::new([format!(r#"{{"answers": {{{}}}}}"#, answers.join(", "))])
                .decide(request)
        }
    }

    /// Line in + line out on the H7, the pins-only converter pair, SAI2.
    fn line_io_design() -> DesignSpec {
        let decider = Decider {
            yes: &["line_in", "line_out"],
            prefer: &[
                ("function", "PCM1808PWR+PCM5102APWR"),
                ("mcu", "STM32H743VIT6"),
                ("bind_i2s", "SAI2"),
            ],
        };
        design(
            &decider,
            &mut Trace::new(),
            "stereo line in and out, no I2C setup",
            &catalog(),
            None,
        )
        .expect("designs")
    }

    #[test]
    fn the_options_are_every_minimal_cover_the_catalog_offers() {
        let cat = catalog();
        let roles: BTreeSet<&str> = ["i2s-adc", "i2s-dac"].into();
        let covers: Vec<String> = minimal_covers(&cat, &roles)
            .iter()
            .map(|s| s.iter().map(|u| u.name()).collect::<Vec<_>>().join("+"))
            .collect();
        assert!(
            covers.contains(&"PCM1808PWR+PCM5102APWR".to_string()),
            "{covers:?}"
        );
        assert!(covers.contains(&"WM8731SEDS".to_string()), "{covers:?}");
        assert!(
            covers.iter().all(|c| !c.starts_with("WM8731SEDS+")),
            "no redundant pairs: {covers:?}"
        );
    }

    #[test]
    fn a_brief_becomes_decisions_and_derivations_over_the_catalog() {
        let spec = line_io_design();
        assert_eq!(spec.parts["function"].chosen, ["PCM1808PWR", "PCM5102APWR"]);
        assert_eq!(spec.parts["function"].how, "decided");
        assert_eq!(spec.bindings["i2s"].chosen, ["SAI2"]);
        assert!(
            !spec.bindings.contains_key("i2c"),
            "pin-strapped parts need no control bus"
        );
        assert!(
            spec.parts.contains_key("needs:hse:STM32H743VIT6"),
            "the MCU's crystal is a need, filled"
        );
    }

    #[test]
    fn a_spec_from_another_catalog_is_refused() {
        let mut spec = line_io_design();
        spec.catalog = "0000000000000000".into();
        let err = circuit(&spec, &catalog(), Path::new("/nonexistent")).unwrap_err();
        assert!(matches!(err, SynthError::StaleCatalog { .. }), "{err}");
    }

    #[test]
    fn a_need_is_met_only_by_a_source_that_fits_it() {
        let cat = catalog();
        let mut radio = cat.part("STM32H743VIT6").unwrap().clone();
        let xtal_8 = cat.part("ECS-80-20-4X").unwrap();
        assert!(
            fits_need(&radio, xtal_8),
            "no stated frequency: any crystal fits"
        );
        radio.params.insert(
            "xtal_freq_hz".into(),
            crate::catalog::Param {
                value: 32e6,
                cite: crate::catalog::Cite::Reading {
                    reading: "test".into(),
                    page: None,
                    confirmed_by: None,
                },
            },
        );
        assert!(
            !fits_need(&radio, xtal_8),
            "a 32 MHz need refuses an 8 MHz crystal"
        );
        assert_eq!(need_slot("hse", "SX1262IMLTRT"), "needs:hse:SX1262IMLTRT");
    }

    #[test]
    fn a_crystal_outside_the_needers_range_does_not_fit() {
        let cat = catalog();
        let param = |value| crate::catalog::Param {
            value,
            cite: crate::catalog::Cite::Reading {
                reading: "test".into(),
                page: None,
                confirmed_by: None,
            },
        };
        let mut mcu = cat.part("STM32F411CEU6").unwrap().clone();
        mcu.params.insert("xtal_min_hz".into(), param(4e6));
        mcu.params.insert("xtal_max_hz".into(), param(26e6));
        let xtal = |mhz: f64| {
            cat.parts
                .iter()
                .find(|p| {
                    p.params
                        .get("freq_hz")
                        .is_some_and(|f| f.value == mhz * 1e6)
                })
                .unwrap_or_else(|| panic!("a {mhz} MHz crystal in the catalog"))
        };
        assert!(fits_need(&mcu, xtal(8.0)), "8 MHz is in 4-26 MHz");
        assert!(!fits_need(&mcu, xtal(32.0)), "32 MHz is above 26 MHz");
    }

    /// A catalog with one subcircuit: a radio slot any_of the CC1101, whose
    /// RF_P reaches the subcircuit's own antenna node through a capacitor.
    fn catalog_with_radio_subcircuit() -> Catalog {
        let mut cat = catalog();
        cat.subcircuits.push(
            serde_json::from_str(
                r#"{"name": "test-radio", "summary": "a radio block",
                    "provides": ["radio-subghz"],
                    "slots": {"radio": {"provides": "radio-subghz", "any_of": ["CC1101RGPR"]}},
                    "interfaces": [{"kind": "rf", "role": "source", "signals": {"rf": "node:ant"}}],
                    "support": [{"between": ["radio.pin:RF_P", "node:ant"], "part": "C",
                                 "value": "47pF", "cite": {"reading": "test"}}]}"#,
            )
            .unwrap(),
        );
        cat
    }

    fn radio_design(cat: &Catalog) -> DesignSpec {
        let decider = Decider {
            yes: &["subghz_radio"],
            prefer: &[("function", "test-radio"), ("mcu", "STM32G0B1KEU6")],
        };
        design(&decider, &mut Trace::new(), "a 915 MHz node", cat, None).expect("designs")
    }

    #[test]
    fn a_subcircuit_is_offered_for_its_role_and_its_slots_are_filled() {
        let cat = catalog_with_radio_subcircuit();
        let roles: BTreeSet<&str> = ["radio-subghz"].into();
        let covers: Vec<&str> = minimal_covers(&cat, &roles)
            .iter()
            .flat_map(|s| s.iter().map(|u| u.name()))
            .collect();
        assert!(covers.contains(&"test-radio"), "{covers:?}");

        let spec = radio_design(&cat);
        assert_eq!(spec.parts["function"].chosen, ["test-radio"]);
        let radio = &spec.parts["sub:test-radio:radio"];
        assert_eq!(
            (radio.chosen.as_slice(), radio.how.as_str()),
            (["CC1101RGPR".to_string()].as_slice(), "derived"),
            "one candidate: derived"
        );
        // Its member stands in for it: the radio's crystal need is filled.
        assert!(spec.parts.contains_key("needs:hse:CC1101RGPR"), "{spec:#?}");
        assert!(
            unconfirmed(&spec, &cat)
                .iter()
                .any(|u| u == "test-radio: test"),
            "the subcircuit's readings are the spec's too"
        );
    }

    #[test]
    #[ignore = "needs KiCad symbols"]
    fn a_subcircuit_wires_its_members_and_exports_its_own_node() {
        let cat = catalog_with_radio_subcircuit();
        let dir = crate::skidl::kicad_symbol_dir().expect("KiCad symbol library");
        let c = circuit(&radio_design(&cat), &cat, dir.path()).unwrap();
        let cap = c
            .parts
            .iter()
            .find(|p| p.value == "47pF")
            .expect("the subcircuit's capacitor is placed");
        let rf = &c.nets["RF_RF"];
        assert!(
            rf.iter().any(|(r, _)| r.starts_with('J'))
                && rf.iter().any(|(r, _)| *r == cap.reference),
            "the connector meets the capacitor at the exported node: {rf:?}"
        );
        let radio_side = c
            .nets
            .values()
            .find(|n| n.iter().any(|(r, _)| *r == cap.reference) && n != &rf)
            .expect("the capacitor's other side");
        assert!(
            radio_side
                .iter()
                .any(|(_, p)| *p == PinRef::Name("RF_P".into())),
            "{radio_side:?}"
        );
    }

    #[test]
    fn pins_on_a_quad_package_have_sides() {
        let po = Pinout {
            pins: (1..=100)
                .map(|n| (n.to_string(), format!("P{n}")))
                .collect(),
            alternates: Vec::new(),
            quad: true,
        };
        assert_eq!(po.side("1"), Some("left"));
        assert_eq!(po.side("26"), Some("bottom"));
        assert_eq!(po.side("75"), Some("right"));
        assert_eq!(po.side("100"), Some("top"));
    }

    /// Needs the installed KiCad symbol library (pin names and alternates).
    #[test]
    #[ignore = "needs KiCad symbols"]
    fn the_design_wires_the_bound_instance_and_every_cited_support_part() {
        let dir = crate::skidl::kicad_symbol_dir().expect("KiCad symbol library");
        let c = circuit(&line_io_design(), &catalog(), dir.path()).unwrap();
        let py = c.to_skidl();
        let bck = &c.nets["I2S_BCK"];
        assert_eq!(
            bck.len(),
            3,
            "MCU, DAC and ADC share the bit clock: {bck:?}"
        );
        let vdd_caps = c.parts.iter().filter(|p| p.value == "100nF").count();
        assert!(vdd_caps >= 5, "{vdd_caps}");
        assert!(
            py.contains(r#"pins(u1, "VCAP")[0]"#) && py.contains(r#"pins(u1, "VCAP")[1]"#),
            "{py}"
        );
        assert!(c.nets.contains_key("LINE_OUT_L") && c.nets.contains_key("LINE_IN_R"));
    }

    /// Binding options carry the pins they would take, and where.
    #[test]
    #[ignore = "needs KiCad symbols"]
    fn binding_options_say_which_pins_and_which_side() {
        let dir = crate::skidl::kicad_symbol_dir().expect("KiCad symbol library");
        let cat = catalog();
        let po = Pinout::of(cat.part("STM32H743VIT6").unwrap(), dir.path()).unwrap();
        let pin = po.free_pin_for("SAI1_SCK_A", &BTreeSet::new()).unwrap();
        assert_eq!(pin, "PE5");
        assert_eq!(
            po.side(po.number_of(&pin).unwrap()),
            Some("left"),
            "PE5 is pin 4"
        );
    }
}
