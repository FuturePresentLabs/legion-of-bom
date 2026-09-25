//! Core circuit domain model — the internal, DSL-agnostic representation.
//!
//! Stages read this through [`CircuitSource`](crate::source::CircuitSource);
//! they never touch a concrete DSL or netlist type. See DESIGN.md 2.3, 3.3.

use std::collections::BTreeMap;
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

/// A SPICE model a part carries with it (from the circuit definition's `Sim.*`
/// fields, or later the parts library) — DESIGN.md 3.5/5.1. Mirrors KiCad's
/// `Sim.Device`/`Sim.Name`/`Sim.Library`/`Sim.Pins`. The resolver
/// ([`crate::symbols`]) turns this into an emittable model. Keeping it *on the
/// part* is the whole point: a real device's model travels with the device, so
/// the SPICE generator never special-cases per-device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimModel {
    /// `Sim.Device`, e.g. `"SUBCKT"`.
    pub device: String,
    /// `Sim.Name` — the subckt/model name, e.g. `"kicad_builtin_opamp"`.
    pub name: String,
    /// `Sim.Library` — where the model lives (may use `${KICAD9_SYMBOL_DIR}`).
    pub library: Option<String>,
    /// `Sim.Pins` — pin→terminal map, e.g. `"3=in+ 2=in- 8=vcc 4=vee 1=out"`.
    pub pins: Option<String>,
}

/// Which physical side of the board a part mounts on (DESIGN 6.1). Whether a
/// board is single- or double-sided, and which parts go where, is a *design
/// choice* declared per part — not derivable from SMD-vs-through-hole (Mutable
/// boards are single-sided despite mixing both; Super Synthesis is double-sided).
/// Parts default to `Front` (single-sided); a double-sided board declares the
/// parts that belong on the `Back`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Front,
    Back,
}

impl Side {
    /// Parse a declared side (`front`/`top` → `Front`, `back`/`bottom` → `Back`).
    pub fn parse(s: &str) -> Option<Side> {
        match s.trim().to_ascii_lowercase().as_str() {
            "front" | "top" | "f" => Some(Side::Front),
            "back" | "bottom" | "b" => Some(Side::Back),
            _ => None,
        }
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
    /// Manufacturer part number, if the circuit declares one (an `MPN` field).
    /// The key that resolves against the global parts library. Generic passives
    /// usually have none.
    pub mpn: Option<String>,
    /// SPICE model carried by the part, if it declares one (`Sim.*` fields). A
    /// primitive (R/C/L) carries none. This is the seam the parts library fills.
    pub sim: Option<SimModel>,
    /// Declared out of simulation (KiCad's `Sim.Enable = 0`): an MCU, a codec,
    /// a crystal — parts that are not an analog circuit a SPICE netlist models.
    /// The declaration is the author's, so a part merely *missing* a model is
    /// still an error, not a silent exclusion.
    pub sim_excluded: bool,
    /// The board side this part is declared to mount on (a `Side` field). `None`
    /// means the default, front.
    pub side: Option<Side>,
    /// Source-declared component fields not promoted to first-class model
    /// properties. Engineering proof consumes namespaced `Power.*` fields
    /// from here; it never guesses ratings from a part number or value.
    pub fields: BTreeMap<String, String>,
}

impl Part {
    /// A part with just a refdes and value; no footprint, library part, MPN,
    /// model, or side.
    pub fn new(refdes: impl Into<RefDes>, value: impl Into<String>) -> Self {
        Part {
            refdes: refdes.into(),
            value: value.into(),
            footprint: None,
            library_part: None,
            mpn: None,
            sim: None,
            sim_excluded: false,
            side: None,
            fields: BTreeMap::new(),
        }
    }

    /// Builder-style: attach a footprint.
    pub fn with_footprint(mut self, footprint: impl Into<String>) -> Self {
        self.footprint = Some(footprint.into());
        self
    }

    /// Builder-style: attach an MPN.
    pub fn with_mpn(mut self, mpn: impl Into<String>) -> Self {
        self.mpn = Some(mpn.into());
        self
    }

    /// Builder-style: attach a SPICE model.
    pub fn with_sim(mut self, sim: SimModel) -> Self {
        self.sim = Some(sim);
        self
    }

    /// Builder-style: declare the board side this part mounts on.
    pub fn with_side(mut self, side: Side) -> Self {
        self.side = Some(side);
        self
    }
}

/// Electrical behavior declared by the source symbol for one connected pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PinElectricalType {
    Input,
    Output,
    Bidirectional,
    TriState,
    Passive,
    PowerInput,
    PowerOutput,
    OpenCollector,
    OpenEmitter,
    NoConnect,
    Unspecified,
}

impl PinElectricalType {
    /// Parse KiCad/SKiDL's netlist spelling without silently reclassifying an
    /// unfamiliar type.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_uppercase().replace(['_', '-'], "");
        Some(match normalized.as_str() {
            "INPUT" => Self::Input,
            "OUTPUT" => Self::Output,
            "BIDIRECTIONAL" | "BIDI" => Self::Bidirectional,
            "TRISTATE" => Self::TriState,
            "PASSIVE" => Self::Passive,
            "POWERIN" | "POWERINPUT" => Self::PowerInput,
            "POWEROUT" | "POWEROUTPUT" => Self::PowerOutput,
            "OPENCOLLECTOR" => Self::OpenCollector,
            "OPENEMITTER" => Self::OpenEmitter,
            "NOCONNECT" | "NC" => Self::NoConnect,
            "UNSPECIFIED" => Self::Unspecified,
            _ => return None,
        })
    }
}

/// A reference to one pin of one part, as it appears on a net.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PinRef {
    /// The part this pin belongs to.
    pub refdes: RefDes,
    /// Pin number or name (`"1"`, `"2"`, `"OUT"`).
    pub pin: String,
    /// Symbol pin function/name (`VIN`, `OUT`, `EN`) when the netlist carries it.
    /// Catalog behavior binds to this stable semantic name instead of guessing
    /// from package pin numbers.
    pub function: Option<String>,
    /// Source symbol electrical type, when the frontend supplied it.
    pub electrical_type: Option<PinElectricalType>,
}

impl PinRef {
    pub fn new(refdes: impl Into<RefDes>, pin: impl Into<String>) -> Self {
        PinRef {
            refdes: refdes.into(),
            pin: pin.into(),
            function: None,
            electrical_type: None,
        }
    }

    /// Attach a source-declared electrical type.
    #[must_use]
    pub fn with_electrical_type(mut self, electrical_type: PinElectricalType) -> Self {
        self.electrical_type = Some(electrical_type);
        self
    }

    #[must_use]
    pub fn with_function(mut self, function: impl Into<String>) -> Self {
        self.function = Some(function.into());
        self
    }
}

/// An electrical net connecting a set of pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Net {
    /// Net name (`"VCC"`, `"GND"`, `"N$3"`).
    pub name: String,
    /// Pins joined by this net.
    pub pins: Vec<PinRef>,
    /// KiCad net class carried from the netlist's `(class "…")` field, when it is
    /// not the default. The circuit author sets it in SKiDL (`net.netclass =
    /// NetClass("Critical")`) to tag a net for analog-careful layout treatment
    /// (DESIGN §6.4); the layout loop reads it via [`Net::is_critical`]. `None`
    /// means the netlist's `"Default"` class (or no class node at all).
    pub net_class: Option<String>,
}

impl Net {
    pub fn new(name: impl Into<String>, pins: Vec<PinRef>) -> Self {
        Net {
            name: name.into(),
            pins,
            net_class: None,
        }
    }

    /// Set the net class (builder). The netlist's `"Default"` (and the empty
    /// string) normalise to `None` — only a meaningful class is retained.
    pub fn with_class(mut self, class: impl Into<String>) -> Self {
        let class = class.into();
        self.net_class =
            (!class.is_empty() && !class.eq_ignore_ascii_case("Default")).then_some(class);
        self
    }

    /// Whether this net is tagged **critical** — it wants a short, direct route
    /// (feedback network, high-impedance input, matched stereo pair). DESIGN §6.4.
    /// A net can carry several classes (SKiDL appends, e.g. `"Default,Critical"`),
    /// so match `"Critical"` against any comma-separated member, case-insensitively.
    pub fn is_critical(&self) -> bool {
        self.net_class.as_deref().is_some_and(|classes| {
            classes
                .split(',')
                .any(|c| c.trim().eq_ignore_ascii_case("Critical"))
        })
    }
}

/// Whether a net name is a **ground** — the one classifier every stage uses
/// (a copy per module disagreed about `AGND` and `VSSA`).
pub fn is_ground_net(name: &str) -> bool {
    let u = name.trim().to_ascii_uppercase();
    matches!(u.as_str(), "0" | "VSS" | "VSSA" | "VSSD")
        || u.starts_with("GND")
        || u.ends_with("GND")
}

/// Whether a net name is a **supply rail** (not ground): `+12V`/`-12V`, the
/// bare `VCC`/`VDD`/`VEE`, and the names an MCU/codec board actually uses —
/// `3V3`/`1V8`/`5V`, `VDDA`/`VDD_USB`/`AVDD`/`DVDD`/`IOVDD`, `VBUS`, `VBAT`,
/// `VREF+`. (Not `VIN`: in an audio circuit that is as often a signal.) Everything downstream that treats a rail specially —
/// decoupling a pin, drawing a stub instead of a wire, keeping a rail off a
/// panel label — asks this, so an MCU board is not a board of signals.
pub fn is_supply_rail(name: &str) -> bool {
    let u = name.trim().to_ascii_uppercase();
    if u.is_empty() || is_ground_net(&u) {
        return false;
    }
    if u.starts_with('+') || u.starts_with('-') || matches!(u.as_str(), "V+" | "V-") {
        return true;
    }
    const RAIL_PREFIXES: [&str; 10] = [
        "VCC", "VDD", "VEE", "AVDD", "DVDD", "IOVDD", "PVDD", "VBUS", "VBAT", "VREF",
    ];
    if RAIL_PREFIXES.iter().any(|p| u.starts_with(p)) {
        return true;
    }
    // A voltage as the name: 5V, 12V, 3V3, 1V8, 3.3V, optionally suffixed
    // (3V3_A, 5V_USB).
    let head = u.split(['_', '-']).next().unwrap_or("");
    let Some((volts, frac)) = head.split_once('V') else {
        return false;
    };
    let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit() || c == '.');
    !volts.is_empty() && digits(volts) && digits(frac)
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

    #[test]
    fn rails_and_grounds_are_told_apart_including_mcu_names() {
        for rail in [
            "+12V", "-12V", "VCC", "VDD", "V+", "3V3", "1V8", "5V", "3.3V", "3V3_A", "VDDA",
            "VDD_USB", "AVDD", "IOVDD", "VBUS", "VBAT", "VREF+",
        ] {
            assert!(is_supply_rail(rail), "{rail} is a rail");
            assert!(!is_ground_net(rail), "{rail} is not ground");
        }
        for gnd in ["GND", "AGND", "DGND", "GNDA", "PGND", "VSS", "VSSA", "0"] {
            assert!(is_ground_net(gnd), "{gnd} is ground");
            assert!(!is_supply_rail(gnd), "{gnd} is not a rail");
        }
        for signal in [
            "SIG_IN", "VIN", "VOUT", "CV1", "I2S_SCK", "OUT", "V", "USB_DP",
        ] {
            assert!(
                !is_supply_rail(signal) && !is_ground_net(signal),
                "{signal} is a signal"
            );
        }
    }

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

    #[test]
    fn side_parses_common_spellings() {
        assert_eq!(Side::parse("front"), Some(Side::Front));
        assert_eq!(Side::parse("Top"), Some(Side::Front));
        assert_eq!(Side::parse("back"), Some(Side::Back));
        assert_eq!(Side::parse("BOTTOM"), Some(Side::Back));
        assert_eq!(Side::parse("sideways"), None);
    }
}
