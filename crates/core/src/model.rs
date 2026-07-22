//! Core circuit domain model — the internal, DSL-agnostic representation.
//!
//! Stages read this through [`CircuitSource`](crate::source::CircuitSource);
//! they never touch a concrete DSL or netlist type. See DESIGN.md 2.3, 3.3.

use std::fmt;

/// A reference designator, e.g. `R1`, `C3`, `U2`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefDes(pub String);

impl fmt::Display for RefDes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<S: Into<String>> From<S> for RefDes {
    fn from(s: S) -> Self {
        RefDes(s.into())
    }
}

/// A single component instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Reference designator (`R1`).
    pub refdes: RefDes,
    /// Component value as written on the schematic (`"10k"`, `"100n"`, `"TL072"`).
    pub value: String,
    /// KiCad footprint, if assigned (`"Resistor_SMD:R_0805_2012Metric"`).
    pub footprint: Option<String>,
    /// Source library part, if known (`"Device:R"`). Frontend-provided hint.
    pub library_part: Option<String>,
}

impl Part {
    /// A part with just a refdes and value; no footprint or library part yet.
    pub fn new(refdes: impl Into<RefDes>, value: impl Into<String>) -> Self {
        Part {
            refdes: refdes.into(),
            value: value.into(),
            footprint: None,
            library_part: None,
        }
    }

    /// Builder-style: attach a footprint.
    pub fn with_footprint(mut self, footprint: impl Into<String>) -> Self {
        self.footprint = Some(footprint.into());
        self
    }
}

/// A reference to one pin of one part, as it appears on a net.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PinRef {
    /// The part this pin belongs to.
    pub refdes: RefDes,
    /// Pin number or name (`"1"`, `"2"`, `"OUT"`).
    pub pin: String,
}

impl PinRef {
    pub fn new(refdes: impl Into<RefDes>, pin: impl Into<String>) -> Self {
        PinRef {
            refdes: refdes.into(),
            pin: pin.into(),
        }
    }
}

/// An electrical net connecting a set of pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Net {
    /// Net name (`"VCC"`, `"GND"`, `"N$3"`).
    pub name: String,
    /// Pins joined by this net.
    pub pins: Vec<PinRef>,
}

impl Net {
    pub fn new(name: impl Into<String>, pins: Vec<PinRef>) -> Self {
        Net {
            name: name.into(),
            pins,
        }
    }
}

/// A complete circuit: the parsed, DSL-agnostic representation every stage
/// consumes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Circuit {
    /// Human-readable circuit name.
    pub name: String,
    /// Component instances.
    pub parts: Vec<Part>,
    /// Electrical nets.
    pub nets: Vec<Net>,
}

impl Circuit {
    pub fn new(name: impl Into<String>) -> Self {
        Circuit {
            name: name.into(),
            parts: Vec::new(),
            nets: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal RC low-pass: one resistor, one capacitor, three nets.
    pub(crate) fn rc_lowpass() -> Circuit {
        Circuit {
            name: "rc_lowpass".into(),
            parts: vec![
                Part::new("R1", "1k").with_footprint("Resistor_SMD:R_0805_2012Metric"),
                Part::new("C1", "159n").with_footprint("Capacitor_SMD:C_0805_2012Metric"),
            ],
            nets: vec![
                Net::new("IN", vec![PinRef::new("R1", "1")]),
                Net::new("OUT", vec![PinRef::new("R1", "2"), PinRef::new("C1", "1")]),
                Net::new("GND", vec![PinRef::new("C1", "2")]),
            ],
        }
    }

    #[test]
    fn builds_rc_lowpass() {
        let c = rc_lowpass();
        assert_eq!(c.parts.len(), 2);
        assert_eq!(c.nets.len(), 3);
        assert_eq!(c.parts[0].refdes, RefDes("R1".into()));
        assert_eq!(
            c.parts[0].footprint.as_deref(),
            Some("Resistor_SMD:R_0805_2012Metric")
        );
    }

    #[test]
    fn refdes_display_and_from() {
        let r: RefDes = "C7".into();
        assert_eq!(r.to_string(), "C7");
    }
}
