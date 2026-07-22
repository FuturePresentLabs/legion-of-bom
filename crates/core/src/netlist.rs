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

use crate::model::{Circuit, Net, Part, PinRef, RefDes};
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
            parts.push(Part {
                refdes: RefDes(refdes.to_string()),
                value,
                footprint,
                library_part,
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
}
