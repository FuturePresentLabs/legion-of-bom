//! Synthesis: a brief becomes a circuit by typed decisions over the catalog
//! (legion-of-bom-uvdm).
//!
//! Nothing here knows any board. The steps:
//!
//! 1. **Requirements** — a fixed vocabulary ([`FEATURES`]), each a yes/no
//!    typed decision answered from the brief.
//! 2. **Parts** — every slot (the audio parts, the MCU, the regulator, the
//!    crystal, each connector) is filled by [`pick`] from the catalog parts
//!    that can fill it: one candidate is *derived*, several are a typed
//!    **choice**. The audio slot's candidates are the *minimal* sets of
//!    catalog parts that cover the required roles.
//! 3. **Bindings** — each bus between the MCU and a peripheral is bound to one
//!    of the MCU's peripheral instances that can carry every signal the bus
//!    needs (a choice again); pins within the instance follow from the KiCad
//!    symbol's alternates, lowest-numbered free pin first.
//!
//! The [`DesignSpec`] records every outcome plus the catalog's fingerprint, so
//! [`circuit`] — spec to SKiDL — is a pure function of it, and refuses a
//! catalog that has changed since the decisions were made.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ooda::{Client, Criteria, Question, Request, Trace};
use serde::{Deserialize, Serialize};

use crate::catalog::{Catalog, CatalogPart, Endpoint, Interface, PartSymbol, Signal};
use crate::oscillator::{fmt_pf, load_cap_pf, round_e12_pf};
use crate::skidl_emit::{Circuit, EmitPart, PinRef, SymbolSrc};
use crate::spec::{expect_choice, expect_noul, SpecError};
use crate::stage::StageError;

/// One requirement the brief can ask of the board, and what it takes.
pub struct Feature {
    pub key: &'static str,
    pub question: &'static str,
    /// The catalog role a part must provide to meet it.
    pub role: &'static str,
    /// The audio part's interface that carries it out to the world.
    pub interface: &'static str,
    /// The connector port kind it lands on, and the board net prefix.
    pub port: &'static str,
    pub net: &'static str,
}

/// The requirement vocabulary.
pub const FEATURES: [Feature; 4] = [
    Feature {
        key: "line_in",
        question: "Does the board need a stereo line-level audio input?",
        role: "i2s-adc",
        interface: "audio-line-in",
        port: "stereo-audio",
        net: "LINE_IN",
    },
    Feature {
        key: "line_out",
        question: "Does the board need a stereo line-level audio output?",
        role: "i2s-dac",
        interface: "audio-line-out",
        port: "stereo-audio",
        net: "LINE_OUT",
    },
    Feature {
        key: "mic_in",
        question: "Does the board need a microphone input?",
        role: "mic-in",
        interface: "audio-mic-in",
        port: "mono-audio",
        net: "MIC_IN",
    },
    Feature {
        key: "headphone_out",
        question: "Does the board need to drive headphones directly?",
        role: "headphone-out",
        interface: "headphone-out",
        port: "stereo-audio",
        net: "HEADPHONE",
    },
];

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
    /// Slot → parts: `audio`, `mcu`, `crystal`, `rail:+3V3`, `port:line_out`, …
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

/// Fill a slot: one candidate is derived, several are a typed choice.
fn pick(
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
    key: &str,
    question: &str,
    options: Vec<(String, String)>,
) -> Result<(String, String), SynthError> {
    match options.len() {
        0 => Err(SynthError::Unfilled(key.to_string())),
        1 => Ok((options[0].0.clone(), "derived".into())),
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
            Ok((chosen, "decided".into()))
        }
    }
}

/// The smallest sets of parts that between them provide every role in
/// `roles`: no set with a part it could drop. Each set is sorted by MPN, and
/// the list is in a stable order.
pub fn minimal_covers<'a>(
    catalog: &'a Catalog,
    roles: &BTreeSet<&str>,
) -> Vec<Vec<&'a CatalogPart>> {
    if roles.is_empty() {
        return vec![Vec::new()];
    }
    let useful: Vec<&CatalogPart> = catalog
        .parts
        .iter()
        .filter(|p| p.provides.iter().any(|r| roles.contains(r.as_str())))
        .collect();
    let covers = |set: &[&CatalogPart]| {
        roles
            .iter()
            .all(|r| set.iter().any(|p| p.provides.iter().any(|x| x == r)))
    };
    let mut out: Vec<Vec<&CatalogPart>> = Vec::new();
    // Sets of one, then two: an audio board is not a four-chip puzzle, and a
    // cover needing more is reported as unfillable rather than searched for.
    for (i, a) in useful.iter().enumerate() {
        if covers(&[a]) {
            out.push(vec![a]);
        }
        for b in &useful[i + 1..] {
            let pair = [*a, *b];
            if covers(&pair) && !covers(&[a]) && !covers(&[b]) {
                out.push(pair.to_vec());
            }
        }
    }
    for set in &mut out {
        set.sort_by(|x, y| x.mpn.cmp(&y.mpn));
    }
    out.sort_by_key(|s| s.iter().map(|p| p.mpn.clone()).collect::<Vec<_>>());
    out
}

/// The signals a set of slave interfaces needs from the master, in the
/// master's terms (a slave's `din` is the master's `dout`).
fn master_signals<'a>(slaves: impl Iterator<Item = &'a Interface>) -> BTreeSet<String> {
    slaves
        .flat_map(|i| i.signals.keys())
        .map(|s| match s.as_str() {
            "din" => "dout".to_string(),
            "dout" => "din".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// Decide a design for `brief` from `catalog`.
pub fn design(
    client: &impl Client,
    trace: &mut Trace,
    brief: &str,
    catalog: &Catalog,
) -> Result<DesignSpec, SynthError> {
    // 1. Requirements.
    let mut request = Request::new(serde_json::json!({ "brief": brief }));
    for f in &FEATURES {
        request = request.with(f.key, Question::noul(f.question));
    }
    let outcome = client.decide(&request).map_err(SpecError::from)?;
    let mut requirements = BTreeMap::new();
    for f in &FEATURES {
        requirements.insert(
            f.key.to_string(),
            expect_noul(&outcome, trace, f.key)? >= 0.5,
        );
    }
    let wanted: Vec<&Feature> = FEATURES.iter().filter(|f| requirements[f.key]).collect();

    let mut parts = BTreeMap::new();
    let fill = |parts: &mut BTreeMap<String, Selection>,
                slot: &str,
                question: &str,
                options: Vec<(String, String)>,
                trace: &mut Trace| {
        let (chosen, how) = pick(client, trace, brief, slot, question, options)?;
        parts.insert(
            slot.to_string(),
            Selection {
                chosen: chosen.split('+').map(str::to_string).collect(),
                how,
            },
        );
        Ok::<_, SynthError>(())
    };

    // 2. Parts: the audio set, then everything it needs.
    let roles: BTreeSet<&str> = wanted.iter().map(|f| f.role).collect();
    let covers = minimal_covers(catalog, &roles);
    let options = covers
        .iter()
        .map(|set| {
            (
                set.iter()
                    .map(|p| p.mpn.as_str())
                    .collect::<Vec<_>>()
                    .join("+"),
                set.iter()
                    .map(|p| p.summary.as_str())
                    .collect::<Vec<_>>()
                    .join(" + "),
            )
        })
        .collect();
    fill(
        &mut parts,
        "audio",
        "Which audio parts should the board use?",
        options,
        trace,
    )?;
    let audio: Vec<&CatalogPart> = parts["audio"]
        .chosen
        .iter()
        .filter_map(|m| catalog.part(m))
        .collect();

    let needs_bus = |kind: &str| {
        audio.iter().any(|p| {
            p.interfaces
                .iter()
                .any(|i| i.kind == kind && i.role == "slave")
        })
    };
    let buses: Vec<&str> = ["i2s", "i2c"]
        .into_iter()
        .filter(|k| needs_bus(k))
        .collect();
    let mcu_ok = |p: &&CatalogPart| {
        buses.iter().all(|k| {
            p.interfaces
                .iter()
                .any(|i| i.kind == *k && i.role == "master")
        })
    };
    let options = catalog
        .providing("mcu")
        .filter(mcu_ok)
        .map(|p| (p.mpn.clone(), p.summary.clone()))
        .collect();
    fill(
        &mut parts,
        "mcu",
        "Which microcontroller should the board use?",
        options,
        trace,
    )?;
    let mcu = catalog
        .part(&parts["mcu"].chosen[0])
        .expect("picked from the catalog");

    if mcu
        .interfaces
        .iter()
        .any(|i| i.kind == "hse" && i.role == "needs")
    {
        let options = catalog
            .providing("crystal")
            .map(|p| (p.mpn.clone(), p.summary.clone()))
            .collect();
        fill(
            &mut parts,
            "crystal",
            "Which crystal should clock the MCU?",
            options,
            trace,
        )?;
    }

    // Rails the chosen parts tie to, other than ground and the supply input.
    let mut rails: BTreeSet<String> = BTreeSet::new();
    for p in audio.iter().copied().chain([mcu]) {
        for s in &p.support {
            for e in &s.between {
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
        let options = catalog
            .providing(&role)
            .map(|p| (p.mpn.clone(), p.summary.clone()))
            .collect();
        fill(
            &mut parts,
            &role,
            &format!("Which part should supply {rail}?"),
            options,
            trace,
        )?;
    }

    // Connectors: one per wanted feature, the supply, and the debug port.
    let port_options = |kind: &str| -> Vec<(String, String)> {
        catalog
            .parts
            .iter()
            .filter(|p| {
                p.interfaces
                    .iter()
                    .any(|i| i.kind == kind && i.role == "port")
            })
            .map(|p| (p.mpn.clone(), p.summary.clone()))
            .collect()
    };
    for f in &wanted {
        fill(
            &mut parts,
            &format!("port:{}", f.key),
            &format!("Which connector carries {}?", f.key),
            port_options(f.port),
            trace,
        )?;
    }
    fill(
        &mut parts,
        "port:supply",
        "Which connector brings in the 5 V supply?",
        port_options("power-in"),
        trace,
    )?;
    let debug: Vec<(String, String)> = catalog
        .parts
        .iter()
        .filter(|p| {
            p.interfaces
                .iter()
                .any(|i| i.kind == "swd" && i.role == "debugger")
        })
        .map(|p| (p.mpn.clone(), p.summary.clone()))
        .collect();
    if mcu.interfaces.iter().any(|i| i.kind == "swd") {
        fill(
            &mut parts,
            "port:swd",
            "Which footprint lands the SWD programmer?",
            debug,
            trace,
        )?;
    }

    // 3. Bindings: which MCU peripheral instance carries each bus.
    let mut bindings = BTreeMap::new();
    for kind in &buses {
        let need = master_signals(
            audio
                .iter()
                .flat_map(|p| &p.interfaces)
                .filter(|i| i.kind == *kind && i.role == "slave"),
        );
        let options = mcu
            .interfaces
            .iter()
            .filter(|i| i.kind == *kind && i.role == "master")
            .filter(|i| need.iter().all(|s| i.signals.contains_key(s)))
            .filter_map(|i| {
                i.instance
                    .clone()
                    .map(|n| (n.clone(), format!("{kind} on {n}")))
            })
            .collect();
        let (chosen, how) = pick(
            client,
            trace,
            brief,
            &format!("bind_{kind}"),
            &format!("Which {} peripheral should carry the {kind} bus?", mcu.mpn),
            options,
        )?;
        bindings.insert(
            kind.to_string(),
            Selection {
                chosen: vec![chosen],
                how,
            },
        );
    }

    Ok(DesignSpec {
        brief: brief.to_string(),
        requirements,
        parts,
        bindings,
        catalog: catalog.fingerprint(),
    })
}

/// The board's supply input net.
const SUPPLY_NET: &str = "+5V";

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
    part: &'a CatalogPart,
    pins: Vec<(String, String)>,
    alternates: Vec<(String, String)>,
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
    let part = |mpn: &str| {
        catalog.part(mpn).ok_or_else(|| {
            SynthError::Invalid(format!("spec names {mpn}, which the catalog lacks"))
        })
    };

    // Place every chosen part, in a fixed order so designators are stable.
    let mut next: BTreeMap<char, usize> = BTreeMap::new();
    let mut designate = |p: &CatalogPart| {
        let prefix = if p.provides.iter().any(|r| r == "crystal") {
            'Y'
        } else if p.provides.iter().any(|r| r == "connector") {
            'J'
        } else {
            'U'
        };
        let n = next.entry(prefix).or_insert(0);
        *n += 1;
        format!("{prefix}{n}")
    };
    let mut placed: Vec<Placed> = Vec::new();
    let mut slots: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let order = ["mcu", "audio", "crystal"];
    let mut slot_names: Vec<&String> = spec.parts.keys().collect();
    slot_names.sort_by_key(|s| {
        (
            order.iter().position(|o| o == s).unwrap_or(order.len()),
            s.to_string(),
        )
    });
    for slot in slot_names {
        for mpn in &spec.parts[slot].chosen {
            let p = part(mpn)?;
            let (pins, alternates) = p.pins(symbol_dir)?;
            slots.entry(slot.clone()).or_default().push(placed.len());
            placed.push(Placed {
                reference: designate(p),
                part: p,
                pins,
                alternates,
            });
        }
    }
    let one = |slot: &str| slots.get(slot).and_then(|v| v.first().copied());

    let mut nets = Nets::default();
    // `(key, reference, pin)` — which key each physical pin connection hangs off.
    let mut pin_links: Vec<(String, String, PinRef)> = Vec::new();
    let mut passives: Vec<(&'static str, String, String, String, String)> = Vec::new();
    let pin_key = |r: &str, n: &str| format!("{r}.pin.{n}");
    let node_key = |r: &str, n: &str| format!("{r}.node.{n}");
    let endpoint_key = |r: &str, e: &str| -> Result<String, SynthError> {
        Ok(match Endpoint::parse(e).map_err(SynthError::Invalid)? {
            Endpoint::Pin(n) => pin_key(r, &n),
            Endpoint::Net(n) => format!("net:{n}"),
            Endpoint::Node(n) => node_key(r, &n),
        })
    };
    // An interface signal's key on the part; an MCU alternate resolves to the
    // lowest-numbered pin carrying it that nothing else has taken.
    let mut taken: BTreeSet<(String, String)> = BTreeSet::new();
    let signal_key = |pl: &Placed,
                      s: &Signal,
                      taken: &mut BTreeSet<(String, String)>|
     -> Result<String, SynthError> {
        match s {
            Signal::At(at) => endpoint_key(&pl.reference, at),
            Signal::Alt { alt } => {
                let mut numbers: Vec<&String> = pl
                    .alternates
                    .iter()
                    .filter(|(_, a)| a == alt)
                    .map(|(n, _)| n)
                    .collect();
                numbers.sort_by_key(|n| (n.parse::<u32>().unwrap_or(u32::MAX), n.to_string()));
                let name = numbers
                    .iter()
                    .filter_map(|n| {
                        pl.pins
                            .iter()
                            .find(|(num, _)| num == *n)
                            .map(|(_, name)| name)
                    })
                    .find(|name| !taken.contains(&(pl.reference.clone(), (*name).clone())))
                    .ok_or_else(|| {
                        SynthError::Invalid(format!("{}: no free pin carries {alt}", pl.part.mpn))
                    })?;
                taken.insert((pl.reference.clone(), name.clone()));
                Ok(pin_key(&pl.reference, name))
            }
        }
    };
    let iface =
        |pl: &Placed, kind: &str, role: &str, instance: Option<&str>| -> Option<Interface> {
            pl.part
                .interfaces
                .iter()
                .find(|i| {
                    i.kind == kind
                        && i.role == role
                        && (instance.is_none() || i.instance.as_deref() == instance)
                })
                .cloned()
        };

    // Pins the MCU's own support uses are not free for a bus.
    if let Some(m) = one("mcu") {
        for s in &placed[m].part.support {
            for e in &s.between {
                if let Ok(Endpoint::Pin(n)) = Endpoint::parse(e) {
                    taken.insert((placed[m].reference.clone(), n));
                }
            }
        }
    }

    // Buses: the bound MCU instance to every slave on it, signal by signal.
    let mcu_idx = one("mcu").ok_or_else(|| SynthError::Unfilled("mcu".into()))?;
    for (kind, sel) in &spec.bindings {
        let instance = &sel.chosen[0];
        let master = iface(&placed[mcu_idx], kind, "master", Some(instance)).ok_or_else(|| {
            SynthError::Invalid(format!(
                "{} has no {kind} {instance}",
                placed[mcu_idx].part.mpn
            ))
        })?;
        for &a in slots.get("audio").map(Vec::as_slice).unwrap_or(&[]) {
            let Some(slave) = iface(&placed[a], kind, "slave", None) else {
                continue;
            };
            for (sig, at) in &slave.signals {
                let msig = match sig.as_str() {
                    "din" => "dout",
                    "dout" => "din",
                    other => other,
                };
                let bus_net = format!("net:{}_{}", kind.to_uppercase(), msig.to_uppercase());
                let m_sig = master.signals.get(msig).ok_or_else(|| {
                    SynthError::Invalid(format!("{instance} cannot carry {kind} {msig}"))
                })?;
                let mk = match nets.parent.contains_key(&bus_net) {
                    true => None,
                    false => Some(signal_key(&placed[mcu_idx], m_sig, &mut taken)?),
                };
                if let Some(mk) = mk {
                    nets.union(&bus_net, &mk);
                }
                nets.union(&bus_net, &signal_key(&placed[a], at, &mut taken)?);
            }
        }
    }

    // SWD: the MCU's debug interface to the programming footprint.
    if let (Some(dbg), Some(target)) = (
        one("port:swd"),
        iface(&placed[mcu_idx], "swd", "target", None),
    ) {
        let debugger = iface(&placed[dbg], "swd", "debugger", None).ok_or_else(|| {
            SynthError::Invalid("the SWD footprint has no debugger interface".into())
        })?;
        for (sig, at) in &target.signals {
            let net = format!("net:{}", sig.to_uppercase());
            nets.union(&net, &signal_key(&placed[mcu_idx], at, &mut taken)?);
            if let Some(d) = debugger.signals.get(sig) {
                nets.union(&net, &signal_key(&placed[dbg], d, &mut taken)?);
            }
        }
    }

    // Crystal: the MCU's HSE pins to the crystal, with load caps computed from
    // the crystal's own load capacitance.
    if let (Some(y), Some(needs)) = (
        one("crystal"),
        iface(&placed[mcu_idx], "hse", "needs", None),
    ) {
        let source = iface(&placed[y], "hse", "source", None)
            .ok_or_else(|| SynthError::Invalid("the crystal has no hse interface".into()))?;
        let cl = placed[y]
            .part
            .params
            .get("cl_pf")
            .ok_or_else(|| SynthError::Invalid(format!("{} has no cl_pf", placed[y].part.mpn)))?
            .value;
        let cap = fmt_pf(round_e12_pf(load_cap_pf(cl)));
        for (sig, at) in &needs.signals {
            let net = format!("net:HSE_{}", sig.to_uppercase());
            nets.union(&net, &signal_key(&placed[mcu_idx], at, &mut taken)?);
            if let Some(s) = source.signals.get(sig) {
                nets.union(&net, &signal_key(&placed[y], s, &mut taken)?);
            }
            passives.push(("C", cap.clone(), String::new(), net, "net:GND".into()));
        }
    }

    // Ports: each wanted feature's audio interface to its connector.
    for f in &FEATURES {
        let Some(j) = one(&format!("port:{}", f.key)) else {
            continue;
        };
        let (src, src_iface) = slots
            .get("audio")
            .into_iter()
            .flatten()
            .find_map(|&a| {
                placed[a]
                    .part
                    .interfaces
                    .iter()
                    .find(|i| i.kind == f.interface)
                    .map(|i| (a, i.clone()))
            })
            .ok_or_else(|| SynthError::Invalid(format!("no audio part has {}", f.interface)))?;
        let port = iface(&placed[j], f.port, "port", None)
            .ok_or_else(|| SynthError::Invalid(format!("connector has no {} port", f.port)))?;
        let port_sigs: Vec<(&String, &Signal)> = port.signals.iter().collect();
        for (k, (sig, at)) in src_iface.signals.iter().enumerate() {
            let net = format!("net:{}_{}", f.net, sig.to_uppercase());
            nets.union(&net, &signal_key(&placed[src], at, &mut taken)?);
            let (_, p) = port_sigs.get(k).ok_or_else(|| {
                SynthError::Invalid(format!("{} port too narrow for {}", f.port, f.key))
            })?;
            nets.union(&net, &signal_key(&placed[j], p, &mut taken)?);
        }
    }
    if let Some(j) = one("port:supply") {
        let port = iface(&placed[j], "power-in", "port", None).ok_or_else(|| {
            SynthError::Invalid("the supply connector has no power-in port".into())
        })?;
        for at in port.signals.values() {
            nets.union(
                &format!("net:{SUPPLY_NET}"),
                &signal_key(&placed[j], at, &mut taken)?,
            );
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
            let kind: &'static str = match s.part.as_str() {
                "R" => "R",
                "CP" => "CP",
                _ => "C",
            };
            let repeat = s.between.iter().find_map(|e| match Endpoint::parse(e) {
                Ok(Endpoint::Pin(n)) if s.each_pin => Some(n),
                _ => None,
            });
            match repeat {
                Some(pin) => {
                    let count = pl.pins.iter().filter(|(_, n)| *n == pin).count();
                    for k in 0..count {
                        let key = format!("{}#{k}", pin_key(&pl.reference, &pin));
                        let other = s
                            .between
                            .iter()
                            .find(|e| **e != format!("pin:{pin}"))
                            .expect("two ends");
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
    for k in &keyed_pins {
        let (reference, rest) = k.split_once(".pin.").expect("a pin key");
        match rest.split_once('#') {
            // One instance of a repeated pin: joined to the rest when the
            // name is tied (VDD), its own net when it is not (VCAP).
            Some((name, idx)) => {
                if name_used.contains(&(reference.to_string(), name.to_string())) {
                    nets.union(k, &pin_key(reference, name));
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

    // Assemble, naming each net after its named member or its first key.
    let mut c = Circuit {
        title: format!("{} — synthesized by legion-of-bom", spec.brief),
        ..Circuit::default()
    };
    let mut roots_named: BTreeMap<String, String> = BTreeMap::new();
    let all_keys: Vec<String> = nets.parent.keys().cloned().collect();
    for k in &all_keys {
        let root = nets.find(k);
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
    let (mut nc, mut nr) = (0usize, 0usize);
    for (kind, value, fp, a, b) in passives {
        let (prefix, n) = if kind == "R" {
            ("R", &mut nr)
        } else {
            ("C", &mut nc)
        };
        *n += 1;
        let reference = format!("{prefix}{n}");
        let (lib, sym, default_fp) = match kind {
            "R" => ("Device", "R", "Resistor_SMD:R_0603_1608Metric"),
            "CP" => (
                "Device",
                "C_Polarized",
                "Capacitor_Tantalum_SMD:CP_EIA-3528-21_Kemet-B",
            ),
            _ => ("Device", "C", "Capacitor_SMD:C_0603_1608Metric"),
        };
        c.parts.push(EmitPart {
            reference: reference.clone(),
            symbol: SymbolSrc::Kicad(lib.into(), sym.into()),
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

/// Every reading no person has confirmed yet, across the parts a spec uses.
pub fn unconfirmed(spec: &DesignSpec, catalog: &Catalog) -> Vec<String> {
    let mut out: Vec<String> = spec
        .parts
        .values()
        .flat_map(|s| &s.chosen)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|m| catalog.part(m))
        .flat_map(|p| p.unconfirmed())
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

    /// Line in + line out, pins-only codec pair, SAI2 — every answer scripted.
    fn line_io_design() -> DesignSpec {
        let client = ooda::ScriptedClient::new([
            format!(
                r#"{{"answers": {{"line_in": {}, "line_out": {}, "mic_in": {}, "headphone_out": {}}}}}"#,
                noul(0.9),
                noul(0.9),
                noul(0.1),
                noul(0.1)
            ),
            format!(
                r#"{{"answers": {{"audio": {}}}}}"#,
                choice("PCM1808PWR+PCM5102APWR")
            ),
            format!(r#"{{"answers": {{"bind_i2s": {}}}}}"#, choice("SAI2")),
        ]);
        let mut trace = Trace::new();
        let spec = design(
            &client,
            &mut trace,
            "stereo line in and out, no I2C setup",
            &catalog(),
        )
        .expect("designs");
        assert_eq!(trace.records().len(), 6, "4 requirements + audio + binding");
        spec
    }

    #[test]
    fn the_options_are_every_minimal_cover_the_catalog_offers() {
        let cat = catalog();
        let roles: BTreeSet<&str> = ["i2s-adc", "i2s-dac"].into();
        let covers: Vec<String> = minimal_covers(&cat, &roles)
            .iter()
            .map(|s| {
                s.iter()
                    .map(|p| p.mpn.as_str())
                    .collect::<Vec<_>>()
                    .join("+")
            })
            .collect();
        assert_eq!(covers, ["ES8388", "PCM1808PWR+PCM5102APWR", "WM8731SEDS"]);
        let with_mic: BTreeSet<&str> = ["i2s-adc", "i2s-dac", "mic-in"].into();
        let covers = minimal_covers(&cat, &with_mic);
        assert_eq!(covers.len(), 1, "only the WM8731 has a mic input");
    }

    #[test]
    fn a_brief_becomes_decisions_and_derivations_over_the_catalog() {
        let spec = line_io_design();
        assert_eq!(spec.parts["audio"].chosen, ["PCM1808PWR", "PCM5102APWR"]);
        assert_eq!(spec.parts["audio"].how, "decided");
        // One MCU, one crystal, one 3V3 supply in the catalog: derived, not asked.
        for slot in [
            "mcu",
            "crystal",
            "rail:+3V3",
            "port:line_in",
            "port:line_out",
            "port:supply",
            "port:swd",
        ] {
            assert_eq!(spec.parts[slot].how, "derived", "{slot}");
        }
        assert_eq!(spec.bindings["i2s"].chosen, ["SAI2"]);
        assert!(
            !spec.bindings.contains_key("i2c"),
            "pin-strapped parts need no control bus"
        );
    }

    #[test]
    fn a_spec_from_another_catalog_is_refused() {
        let mut spec = line_io_design();
        spec.catalog = "0000000000000000".into();
        let err = circuit(&spec, &catalog(), Path::new("/nonexistent")).unwrap_err();
        assert!(matches!(err, SynthError::StaleCatalog { .. }), "{err}");
    }

    /// Needs the installed KiCad symbol library (pin names and alternates).
    #[test]
    #[ignore = "needs KiCad symbols"]
    fn the_design_wires_the_bound_instance_and_every_cited_support_part() {
        let dir = crate::skidl::kicad_symbol_dir().expect("KiCad symbol library");
        let c = circuit(&line_io_design(), &catalog(), dir.path()).unwrap();
        let py = c.to_skidl();
        // SAI2's block-A clock lands on whichever pin carries SAI2_SCK_A.
        let bck = &c.nets["I2S_BCK"];
        assert_eq!(
            bck.len(),
            3,
            "MCU, DAC and ADC share the bit clock: {bck:?}"
        );
        // Five VDD pins, five 100 nF — counted from the symbol, not written.
        let vdd_caps = c.parts.iter().filter(|p| p.value == "100nF").count();
        assert!(vdd_caps >= 5, "{vdd_caps}");
        // Each VCAP pin its own cap, never tied together.
        assert!(
            py.contains(r#"pins(u1, "VCAP")[0]"#) && py.contains(r#"pins(u1, "VCAP")[1]"#),
            "{py}"
        );
        assert!(c.nets.contains_key("LINE_OUT_L") && c.nets.contains_key("LINE_IN_R"));
    }
}
