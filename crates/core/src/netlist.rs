//! Parse a KiCad netlist (as emitted by SKiDL) into the internal [`Circuit`]
//! model — the first producer of a [`CircuitSource`]. DESIGN.md 3.3.
//!
//! [`CircuitSource`]: crate::source::CircuitSource
//!
//! KiCad netlists are S-expressions:
//!
//! ```text
//! (export (version "E")
//!   (components
//!     (comp (ref "R1") (value "1k") (footprint "…") (libsource (lib "Device") (part "R")) …))
//!   (nets
//!     (net (code 3) (name "OUT")
//!       (node (ref "R1") (pin "2")) (node (ref "C1") (pin "1")))))
//! ```
//!
//! We parse with a small self-contained S-expr reader (no external dependency)
//! and pull out just the parts and nets. Output is sorted (parts by refdes, nets
//! by name, pins within a net) so the model is deterministic regardless of the
//! netlist's ordering — which keeps downstream BOM/tests stable.

use std::path::Path;

use crate::model::{Circuit, Net, Part, PinRef, RefDes, Side, SimModel};
use crate::sexpr::Sexpr;
use crate::stage::StageError;

/// Parse a KiCad netlist file into a [`Circuit`], naming it after the file stem.
pub fn parse_netlist_file(path: &Path) -> Result<Circuit, StageError> {
    let text = std::fs::read_to_string(path)?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("circuit");
    parse_netlist_str(&text, name)
}

/// Parse KiCad netlist text into a [`Circuit`] with the given name.
pub fn parse_netlist_str(text: &str, name: &str) -> Result<Circuit, StageError> {
    let root =
        Sexpr::parse(text).map_err(|e| StageError::Other(format!("netlist parse error: {e}")))?;
    if root.head() != Some("export") {
        return Err(StageError::Other(
            "not a KiCad netlist (expected a top-level `(export …)`)".into(),
        ));
    }

    let mut parts = Vec::new();
    if let Some(components) = root.get("components") {
        for comp in components.get_all("comp") {
            let refdes = comp
                .field("ref")
                .ok_or_else(|| StageError::Other("component missing (ref …)".into()))?;
            let value = comp.field("value").unwrap_or_default().to_string();
            let footprint = comp
                .field("footprint")
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            let library_part = comp
                .get("libsource")
                .and_then(|ls| Some(format!("{}:{}", ls.field("lib")?, ls.field("part")?)));
            let mpn = field_value(comp, "MPN");
            // A part-carried SPICE model, if the circuit declares `Sim.*` fields
            // (SKiDL passes manually-set fields through to the netlist).
            let sim = field_value(comp, "Sim.Device").map(|device| SimModel {
                device,
                name: field_value(comp, "Sim.Name").unwrap_or_default(),
                library: field_value(comp, "Sim.Library"),
                pins: field_value(comp, "Sim.Pins"),
            });
            // Which board side the circuit declares this part on (a `Side` field).
            let side = field_value(comp, "Side").and_then(|s| Side::parse(&s));
            parts.push(Part {
                refdes: RefDes(refdes.to_string()),
                value,
                footprint,
                library_part,
                mpn,
                sim,
                side,
            });
        }
    }

    let mut nets = Vec::new();
    if let Some(nets_node) = root.get("nets") {
        for net in nets_node.get_all("net") {
            let net_name = net.field("name").unwrap_or_default().to_string();
            let mut pins: Vec<PinRef> = net
                .get_all("node")
                .into_iter()
                .filter_map(|node| Some(PinRef::new(node.field("ref")?, node.field("pin")?)))
                .collect();
            pins.sort_by(|a, b| (&a.refdes, &a.pin).cmp(&(&b.refdes, &b.pin)));
            nets.push(Net {
                name: net_name,
                pins,
            });
        }
    }

    parts.sort_by(|a, b| a.refdes.cmp(&b.refdes));
    nets.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Circuit {
        name: name.to_string(),
        parts,
        nets,
    })
}

/// Value of a component's custom field named `wanted` (case-insensitive), from
/// its `(fields (field (name "X") "value") …)` block. Empty values → `None`.
fn field_value(comp: &Sexpr, wanted: &str) -> Option<String> {
    comp.get("fields")?
        .get_all("field")
        .into_iter()
        .find(|f| {
            f.get("name")
                .and_then(|n| n.nth_atom(1))
                .is_some_and(|n| n.eq_ignore_ascii_case(wanted))
        })
        .and_then(|f| f.nth_atom(2))
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::CircuitSource;

    /// A structurally faithful slice of the netlist SKiDL emits for the RC demo.
    const RC_NETLIST: &str = r#"
    (export (version "E")
      (design (source "rc_lowpass.py"))
      (components
        (comp (ref "C1") (value "159n")
          (footprint "Capacitor_SMD:C_0805_2012Metric")
          (libsource (lib "Device") (part "C") (description "Unpolarized capacitor")))
        (comp (ref "R1") (value "1k")
          (footprint "Resistor_SMD:R_0805_2012Metric")
          (libsource (lib "Device") (part "R") (description "Resistor"))))
      (nets
        (net (code 1) (name "GND") (class "Default")
          (node (ref "C1") (pin "2") (pintype "PASSIVE")))
        (net (code 2) (name "IN") (class "Default")
          (node (ref "R1") (pin "1") (pintype "PASSIVE")))
        (net (code 3) (name "OUT") (class "Default")
          (node (ref "C1") (pin "1") (pintype "PASSIVE"))
          (node (ref "R1") (pin "2") (pintype "PASSIVE")))))
    "#;

    #[test]
    fn parses_rc_netlist_into_circuit() {
        let c = parse_netlist_str(RC_NETLIST, "rc_lowpass").expect("parse");
        assert_eq!(c.name(), "rc_lowpass");

        // Parts: sorted by refdes → C1, R1.
        assert_eq!(c.parts().len(), 2);
        assert_eq!(c.parts()[0].refdes, RefDes("C1".into()));
        assert_eq!(c.parts()[1].refdes, RefDes("R1".into()));

        let r1 = c
            .parts()
            .iter()
            .find(|p| p.refdes == RefDes("R1".into()))
            .unwrap();
        assert_eq!(r1.value, "1k");
        assert_eq!(
            r1.footprint.as_deref(),
            Some("Resistor_SMD:R_0805_2012Metric")
        );
        assert_eq!(r1.library_part.as_deref(), Some("Device:R"));

        let c1 = c
            .parts()
            .iter()
            .find(|p| p.refdes == RefDes("C1".into()))
            .unwrap();
        assert_eq!(c1.value, "159n");
        assert_eq!(c1.library_part.as_deref(), Some("Device:C"));

        // Nets: sorted by name → GND, IN, OUT.
        assert_eq!(c.nets().len(), 3);
        assert_eq!(
            c.nets().iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            ["GND", "IN", "OUT"]
        );

        // OUT joins R1.2 and C1.1 (pins sorted by refdes then pin).
        let out = c.nets().iter().find(|n| n.name == "OUT").unwrap();
        assert_eq!(
            out.pins,
            vec![PinRef::new("C1", "1"), PinRef::new("R1", "2")]
        );
    }

    #[test]
    fn rejects_non_netlist() {
        let err = parse_netlist_str("(something_else (foo))", "x").unwrap_err();
        assert!(matches!(err, StageError::Other(_)));
    }

    #[test]
    fn extracts_mpn_field() {
        let nl = r#"
        (export (version "E")
          (components
            (comp (ref "U1") (value "LM13700")
              (fields (field (name "MPN") "LM13700") (field (name "Footprint") "Package_DIP:DIP-16"))
              (libsource (lib "Amplifier_Operational") (part "LM13700")))
            (comp (ref "R1") (value "1k")
              (libsource (lib "Device") (part "R"))))
          (nets))"#;
        let c = parse_netlist_str(nl, "x").unwrap();
        let u1 = c.parts.iter().find(|p| p.refdes.0 == "U1").unwrap();
        assert_eq!(u1.mpn.as_deref(), Some("LM13700"));
        // A generic part with no MPN field resolves to None.
        let r1 = c.parts.iter().find(|p| p.refdes.0 == "R1").unwrap();
        assert_eq!(r1.mpn, None);
    }

    #[test]
    fn extracts_part_carried_spice_model() {
        // A real device carries its SPICE model as Sim.* fields (SKiDL passes
        // manually-set fields through). The parser lifts them onto Part.sim.
        let nl = r#"
        (export (version "E")
          (components
            (comp (ref "U1") (value "TL072")
              (footprint "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm")
              (fields
                (field (name "Sim.Device") "SUBCKT")
                (field (name "Sim.Name") "kicad_builtin_opamp")
                (field (name "Sim.Library") "${KICAD9_SYMBOL_DIR}/Simulation_SPICE.sp")
                (field (name "Sim.Pins") "3=in+ 2=in- 8=vcc 4=vee 1=out"))
              (libsource (lib "Amplifier_Operational") (part "TL072")))
            (comp (ref "R1") (value "1k")
              (libsource (lib "Device") (part "R"))))
          (nets))"#;
        let c = parse_netlist_str(nl, "x").unwrap();
        let u1 = c.parts.iter().find(|p| p.refdes.0 == "U1").unwrap();
        let sim = u1.sim.as_ref().expect("U1 carries a SPICE model");
        assert_eq!(sim.device, "SUBCKT");
        assert_eq!(sim.name, "kicad_builtin_opamp");
        assert_eq!(sim.pins.as_deref(), Some("3=in+ 2=in- 8=vcc 4=vee 1=out"));
        assert!(sim
            .library
            .as_deref()
            .unwrap()
            .ends_with("Simulation_SPICE.sp"));
        // A primitive (no Sim.*) carries no model.
        let r1 = c.parts.iter().find(|p| p.refdes.0 == "R1").unwrap();
        assert!(r1.sim.is_none());
    }
}
