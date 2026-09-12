//! Emit a pedalkernel `.pedal` file from a circuit (ef4.3, Path B).
//!
//! The `.pedal` DSL is pedalkernel's circuit-definition format (WDF engine).
//! This module translates the pipeline's own circuit IR into that DSL so the
//! same topology can be compiled and run by both engines — the foundation for
//! cross-engine validation (ef4.4): our ngspice decks and pedalkernel's
//! process should agree on the circuit's behaviour.
//!
//! Mapping (CircuitSource → .pedal):
//! - `R`/`C`/`L` primitives → `resistor`/`cap`/`inductor` (pins `a`/`b`)
//! - potentiometers (`RV…`, wiper = pin 2) → `pot` (pins `a`/`b`/`w`)
//! - op-amps (TL072 family, NE5532, …) → `opamp(<type>)` (`pos`/`neg`/`out`;
//!   pedalkernel's op-amp is single-rail, so the V− supply pin is not mapped)
//! - OTA parts (LM13700/LM13600/CA3080) → `opamp(ca3080)` with the bias pin
//!   mapped to the modulation sink `iabc`
//! - connectors/mechanical parts are skipped (same rule as the SPICE decks);
//!   their nets become the reserved `in`/`out`/`gnd` nodes or supply rails
//!
//! The emitted file is text; validating it against pedalkernel itself is the
//! cross-engine harness's job (ef4.4), which shells out to the pedalkernel CLI
//! as a tool — this crate deliberately does not depend on pedalkernel (ef4.1).

use crate::model::Part;
use crate::source::CircuitSource;
use crate::spice::{is_electrical_noop, SimConfig};
use crate::stage::StageError;
use std::collections::BTreeMap;

/// One component's .pedal emission: declared kind + per-pin name map.
struct PedalComponent {
    /// Constructor text, e.g. `resistor(1k)` or `opamp(ca3080)`.
    decl: String,
    /// Part pin number → .pedal pin name.
    pins: BTreeMap<String, &'static str>,
}

/// Classify a part into its .pedal component declaration, or `None` to skip it
/// (anything the DSL has no equivalent for).
fn pedal_decl(part: &Part) -> Option<PedalComponent> {
    // Potentiometers first — `RV…`, 3 pins: 1 — wiper(2) — 3.
    if part.refdes.0.to_ascii_uppercase().starts_with("RV") {
        return Some(PedalComponent {
            decl: format!("pot({}, a)", part.value),
            pins: BTreeMap::from([
                ("1".to_string(), "a"),
                ("2".to_string(), "w"),
                ("3".to_string(), "b"),
            ]),
        });
    }

    let hay = format!(
        "{} {}",
        part.library_part.as_deref().unwrap_or(""),
        part.value
    )
    .to_ascii_uppercase();

    // OTA: the LM13700's channel-A terminals; IABC is the modulation sink.
    if hay.contains("LM13700") || hay.contains("LM13600") || hay.contains("CA3080") {
        return Some(PedalComponent {
            decl: "opamp(ca3080)".to_string(),
            pins: BTreeMap::from([
                // Pin order: iabc(1) in+(3) in-(4) Iout(5).
                ("1".to_string(), "iabc"),
                ("3".to_string(), "pos"),
                ("4".to_string(), "neg"),
                ("5".to_string(), "out"),
            ]),
        });
    }

    // Op-amps: pedalkernel's op-amp is a single-rail behavioural VCVS — the
    // V+ supply pin maps to `vp`; there is no V− pin (pedal convention), so
    // the negative rail net is carried by the supplies block only.
    let opamp_type = if hay.contains("TL082") {
        // Same JFET-input family as the TL072; closest available model.
        "tl072"
    } else if hay.contains("TL072") || hay.contains("TL071") {
        "tl072"
    } else if hay.contains("4558") {
        "jrc4558"
    } else if hay.contains("LM308") {
        "lm308"
    } else if hay.contains("741") {
        "lm741"
    } else if hay.contains("5532") {
        "ne5532"
    } else {
        return None;
    };
    // Dual (unit B: pins 5–7) and single parts both map unit A.
    Some(PedalComponent {
        decl: format!("opamp({opamp_type})"),
        pins: BTreeMap::from([
            ("1".to_string(), "out"),
            ("2".to_string(), "neg"),
            ("3".to_string(), "pos"),
            ("8".to_string(), "vp"),
        ]),
    })
}

/// Classify a passive part (R/C/L by refdes prefix) into its .pedal component.
/// Potentiometers (`RV…`) are handled by [`pedal_decl`], not here.
fn passive_decl(part: &Part) -> Option<PedalComponent> {
    if part.refdes.0.to_ascii_uppercase().starts_with("RV") {
        return None;
    }
    let prefix = part.refdes.0.chars().next()?.to_ascii_uppercase();
    let decl = match prefix {
        'R' => format!("resistor({})", part.value),
        'C' => format!("cap({})", part.value),
        'L' => format!("inductor({})", part.value),
        _ => return None,
    };
    Some(PedalComponent {
        decl,
        pins: BTreeMap::from([("1".to_string(), "a"), ("2".to_string(), "b")]),
    })
}

/// Format a supply voltage the way the .pedal DSL writes rails: `15`, `-15`.
fn fmt_volts(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Emit a `.pedal` file for `circuit` using `config`'s I/O and supply nets.
pub fn emit_pedal(circuit: &dyn CircuitSource, config: &SimConfig) -> Result<String, StageError> {
    let net_names: Vec<String> = circuit.nets().iter().map(|n| n.name.clone()).collect();
    let supplies: Vec<(String, f64)> = config
        .supplies
        .iter()
        .filter(|(net, _)| net_names.iter().any(|n| n == net))
        .cloned()
        .collect();

    // Translate every part pin to its .pedal component pin, skipping no-op
    // parts (connectors/mechanical). Unclassifiable parts are an error — a
    // silent skip would drop a device from the cross-engine comparison.
    let mut decls: Vec<(String, String)> = Vec::new();
    let mut pot_ids: Vec<String> = Vec::new();
    let mut pin_of: BTreeMap<(String, String), &'static str> = BTreeMap::new();
    for part in circuit.parts() {
        if is_electrical_noop(part) {
            continue;
        }
        let comp = passive_decl(part)
            .or_else(|| pedal_decl(part))
            .ok_or_else(|| {
                StageError::Other(format!(
                    "{}: no .pedal mapping for part '{}' (value '{}')",
                    circuit.name(),
                    part.refdes.0,
                    part.value
                ))
            })?;
        if part.refdes.0.to_ascii_uppercase().starts_with("RV") {
            pot_ids.push(part.refdes.0.clone());
        }
        for (pin, name) in &comp.pins {
            pin_of.insert((part.refdes.0.clone(), pin.clone()), name);
        }
        decls.push((part.refdes.0.clone(), comp.decl));
    }

    // Reserved .pedal node each named net maps to. Supply rails have no
    // reserved alias — they are referenced by their declared name.
    let reserved_of = |name: &str| -> Option<&'static str> {
        if *name == config.input_net {
            Some("in")
        } else if *name == config.output_net {
            Some("out")
        } else if config.ground_nets.iter().any(|g| g == name) {
            Some("gnd")
        } else {
            None
        }
    };

    let mut lines = Vec::new();
    lines.push(format!("pedal \"{}\" {{", circuit.name().replace('"', "'")));

    if !supplies.is_empty() {
        lines.push("  supplies {".to_string());
        for (net, volts) in &supplies {
            lines.push(format!("    {net}: {}V", fmt_volts(*volts)));
        }
        lines.push("  }".to_string());
        lines.push(String::new());
    }

    lines.push("  components {".to_string());
    for (id, decl) in &decls {
        lines.push(format!("    {id}: {decl}"));
    }
    lines.push("  }".to_string());
    lines.push(String::new());

    lines.push("  nets {".to_string());
    for net in circuit.nets() {
        // A net's surviving members: (refdes, pedal pin), in deterministic
        // (refdes, pin) order. Pins on skipped parts drop out.
        let members: Vec<(String, String)> = net
            .pins
            .iter()
            .filter_map(|p| {
                let pin = pin_of.get(&(p.refdes.0.clone(), p.pin.clone()))?;
                Some((p.refdes.0.clone(), pin.to_string()))
            })
            .collect();

        let reserved = reserved_of(&net.name);
        let is_rail = supplies.iter().any(|(s, _)| *s == net.name);

        if let Some(node) = reserved {
            // in/out/gnd: representative is the reserved node.
            if members.is_empty() {
                continue; // net only touched connectors — nothing to bind
            }
            let dests: Vec<String> = members.iter().map(|(r, p)| format!("{r}.{p}")).collect();
            lines.push(format!("    {node} -> {}", dests.join(", ")));
        } else if is_rail {
            // Supply rail: representative is the rail name itself.
            if members.is_empty() {
                continue;
            }
            let dests: Vec<String> = members.iter().map(|(r, p)| format!("{r}.{p}")).collect();
            lines.push(format!("    {} -> {}", net.name, dests.join(", ")));
        } else {
            // Internal net: representative is its first member pin.
            let mut it = members.iter();
            let Some((r0, p0)) = it.next() else {
                continue; // only connector pins — nothing survives
            };
            let rest: Vec<String> = it.map(|(r, p)| format!("{r}.{p}")).collect();
            if rest.is_empty() {
                continue; // single dangling member — no connection to emit
            }
            lines.push(format!("    {r0}.{p0} -> {}", rest.join(", ")));
        }
    }
    lines.push("  }".to_string());

    if !pot_ids.is_empty() {
        lines.push(String::new());
        lines.push("  controls {".to_string());
        for id in &pot_ids {
            lines.push(format!("    {id}.position -> \"{id}\" [0.0, 1.0] = 0.5"));
        }
        lines.push("  }".to_string());
    }

    lines.push("}".to_string());
    Ok(lines.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Circuit, Net, PinRef};

    fn config() -> SimConfig {
        SimConfig::default()
    }

    #[test]
    fn emits_passive_nets_and_reserved_nodes() {
        // The RC low-pass: IN -> resistor -> OUT, cap to GND.
        let c = Circuit {
            name: "rc_lowpass".into(),
            parts: vec![Part::new("R1", "1k"), Part::new("C1", "159n")],
            nets: vec![
                Net::new("IN", vec![PinRef::new("R1", "1")]),
                Net::new("OUT", vec![PinRef::new("R1", "2"), PinRef::new("C1", "1")]),
                Net::new("GND", vec![PinRef::new("C1", "2")]),
            ],
        };
        let out = emit_pedal(&c, &config()).unwrap();
        assert!(out.contains("pedal \"rc_lowpass\" {"), "{out}");
        assert!(out.contains("R1: resistor(1k)"), "{out}");
        assert!(out.contains("C1: cap(159n)"), "{out}");
        assert!(out.contains("in -> R1.a"), "{out}");
        assert!(out.contains("out -> R1.b, C1.a"), "{out}");
        assert!(out.contains("gnd -> C1.b"), "{out}");
        assert!(
            !out.contains("supplies"),
            "no supply nets → no block: {out}"
        );
    }

    #[test]
    fn emits_supplies_and_opamp_mapping() {
        // TL072 non-inverting gain-2 with ±15 V rails: the op-amp maps to
        // opamp(tl072), VCC rides the supplies block, VEE is not mapped
        // (pedalkernel's op-amp is single-rail), the unused unit is skipped.
        let c = Circuit {
            name: "gain2".into(),
            parts: vec![
                Part::new("U1", "TL072"),
                Part::new("R1", "10k"),
                Part::new("R2", "10k"),
                Part::new("J1", "PJ-102"), // connector — skipped
            ],
            nets: vec![
                Net::new("IN", vec![PinRef::new("J1", "1"), PinRef::new("U1", "3")]),
                Net::new("OUT", vec![PinRef::new("U1", "1"), PinRef::new("R1", "1")]),
                Net::new(
                    "FB",
                    vec![
                        PinRef::new("U1", "2"),
                        PinRef::new("R1", "2"),
                        PinRef::new("R2", "1"),
                    ],
                ),
                Net::new("VCC", vec![PinRef::new("U1", "8")]),
                Net::new("VEE", vec![PinRef::new("U1", "4")]),
                Net::new(
                    "GND",
                    vec![
                        PinRef::new("J1", "2"),
                        PinRef::new("U1", "5"),
                        PinRef::new("U1", "6"),
                        PinRef::new("U1", "7"),
                        PinRef::new("R2", "2"),
                    ],
                ),
            ],
        };
        let out = emit_pedal(&c, &config()).unwrap();
        assert!(
            out.contains("supplies {\n    VCC: 15V\n    VEE: -15V\n  }"),
            "{out}"
        );
        assert!(out.contains("U1: opamp(tl072)"), "{out}");
        assert!(out.contains("in -> U1.pos"), "{out}");
        assert!(out.contains("out -> U1.out, R1.a"), "{out}");
        assert!(out.contains("VCC -> U1.vp"), "{out}");
        assert!(!out.contains("VEE ->"), "no V− pin on PK opamp: {out}");
        assert!(!out.contains("J1"), "connector skipped: {out}");
    }

    #[test]
    fn maps_ota_with_iabc_sink_and_pot_control() {
        let c = Circuit {
            name: "ota_stage".into(),
            parts: vec![
                Part::new("U1", "LM13700"),
                Part::new("R1", "1k"),
                Part::new("RV1", "100k"),
            ],
            nets: vec![
                Net::new("IN", vec![PinRef::new("R1", "1")]),
                Net::new("BIAS", vec![PinRef::new("R1", "2"), PinRef::new("U1", "1")]),
                Net::new("INP", vec![PinRef::new("U1", "3")]),
                Net::new("INN", vec![PinRef::new("U1", "4")]),
                Net::new("OUT", vec![PinRef::new("U1", "5")]),
                Net::new(
                    "GND",
                    vec![PinRef::new("RV1", "1"), PinRef::new("RV1", "3")],
                ),
                Net::new("WIPER", vec![PinRef::new("RV1", "2")]),
            ],
        };
        let out = emit_pedal(&c, &config()).unwrap();
        assert!(out.contains("U1: opamp(ca3080)"), "{out}");
        assert!(out.contains("R1.b -> U1.iabc"), "{out}");
        assert!(out.contains("RV1: pot(100k, a)"), "{out}");
        assert!(
            out.contains("RV1.position -> \"RV1\" [0.0, 1.0] = 0.5"),
            "{out}"
        );
    }

    #[test]
    fn unclassifiable_part_is_a_loud_error() {
        let c = Circuit {
            name: "weird".into(),
            parts: vec![Part::new("U9", "MYSTERY_CHIP")],
            nets: vec![],
        };
        let err = emit_pedal(&c, &config()).unwrap_err();
        assert!(
            matches!(err, StageError::Other(ref msg) if msg.contains("U9") || msg.contains("MYSTERY"))
        );
    }
}
