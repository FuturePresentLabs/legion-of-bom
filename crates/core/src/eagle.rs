//! Eagle import — schematic to a real [`Circuit`], board to placements (w7e).
//!
//! Unlike a fab package ([`crate::import`]), an Eagle schematic carries the
//! **netlist**, so what comes out is a circuit the rest of the pipeline can
//! actually work on: simulate, ERC, re-lay-out, or re-emit as SKiDL. Since v6,
//! `.sch`/`.brd` are XML with a published DTD, so this is parsing rather than
//! archaeology.
//!
//! The reader is deliberately narrow. Eagle files are large — a schematic is
//! mostly `<wire>` elements describing how the drawing *looks* — and none of that
//! matters here: connectivity lives in `<net>`/`<segment>`/`<pinref>`, and
//! components in `<part>`, resolved to a footprint through the file's own
//! `<library>`/`<deviceset>`/`<device>` chain.

use std::collections::HashMap;

use crate::model::{Circuit, Net, Part, PinRef};

/// One XML element, flattened: its name, attributes, and how deep it sits.
#[derive(Debug, Clone)]
struct Element {
    name: String,
    attrs: HashMap<String, String>,
    /// Names of the enclosing elements, outermost first.
    path: Vec<String>,
}

impl Element {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.get(k).map(String::as_str)
    }
    /// Whether this element sits (at any depth) inside one named `name`.
    fn inside(&self, name: &str) -> bool {
        self.path.iter().any(|p| p == name)
    }
}

/// Scan well-formed XML into a flat element list with ancestry.
///
/// Hand-rolled rather than pulling in a dependency: Eagle's output is
/// machine-generated and regular, we need a handful of element types, and the
/// crate already reads its other formats (S-expressions, gerber) the same way.
fn scan(xml: &str) -> Vec<Element> {
    let mut out = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let bytes = xml.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Advance to the next tag.
        let Some(lt) = xml[i..].find('<').map(|p| i + p) else {
            break;
        };
        // Skip comments, declarations and doctypes wholesale.
        if xml[lt..].starts_with("<!--") {
            i = xml[lt..]
                .find("-->")
                .map(|p| lt + p + 3)
                .unwrap_or(bytes.len());
            continue;
        }
        if xml[lt..].starts_with("<?") || xml[lt..].starts_with("<!") {
            i = xml[lt..]
                .find('>')
                .map(|p| lt + p + 1)
                .unwrap_or(bytes.len());
            continue;
        }
        // Find the tag's end, ignoring '>' inside quoted attribute values.
        let mut j = lt + 1;
        let mut quoted = false;
        while j < bytes.len() {
            match bytes[j] {
                b'"' => quoted = !quoted,
                b'>' if !quoted => break,
                _ => {}
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let inner = &xml[lt + 1..j];
        i = j + 1;

        if let Some(close) = inner.strip_prefix('/') {
            let name = close.trim();
            if stack.last().map(String::as_str) == Some(name) {
                stack.pop();
            }
            continue;
        }
        let self_closing = inner.ends_with('/');
        let inner = inner.trim_end_matches('/').trim();
        let mut parts = inner.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let attrs = parse_attrs(parts.next().unwrap_or(""));
        out.push(Element {
            name: name.clone(),
            attrs,
            path: stack.clone(),
        });
        if !self_closing {
            stack.push(name);
        }
    }
    out
}

/// `name="value" other="v"` → a map. Values are always quoted in Eagle's output.
fn parse_attrs(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let Some(eq) = s[i..].find('=').map(|p| i + p) else {
            break;
        };
        let key = s[i..eq].trim().to_string();
        let rest = &s[eq + 1..];
        let Some(q) = rest.find('"') else { break };
        let Some(end) = rest[q + 1..].find('"') else {
            break;
        };
        let value = &rest[q + 1..q + 1 + end];
        out.insert(key, unescape(value));
        i = eq + 1 + q + 1 + end + 1;
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Map an Eagle package name onto a KiCad footprint.
///
/// Eagle names a package for its shape (`R0603`, `SOT23`); KiCad wants a
/// library-qualified path. Only the common passives and packages are translated —
/// anything unrecognised is passed through **prefixed with `eagle:`**, so an
/// unmapped footprint is visible as unmapped rather than silently wrong. A board
/// with `eagle:` footprints can be read and costed but should not be re-laid-out
/// until they are resolved.
pub fn map_footprint(package: &str) -> String {
    let p = package.trim().to_ascii_uppercase();
    // KiCad's library is named for the part class ("Resistor_SMD") but the
    // footprint inside it uses the short prefix ("R_0402_1005Metric").
    let smd = |lib: &str, short: &str, code: &str, metric: &str| {
        format!("{lib}_SMD:{short}_{code}_{metric}Metric")
    };
    // Passives share the 0402/0603/0805/1206 family across R, C and L.
    let sizes = [
        ("0402", "1005"),
        ("0603", "1608"),
        ("0805", "2012"),
        ("1206", "3216"),
        ("1210", "3225"),
    ];
    for (code, metric) in sizes {
        if p == format!("R{code}") || p == format!("R-{code}") {
            return smd("Resistor", "R", code, metric);
        }
        if p == format!("C{code}") || p == format!("C-{code}") {
            return smd("Capacitor", "C", code, metric);
        }
        if p == format!("L{code}") {
            return smd("Inductor", "L", code, metric);
        }
    }
    // A bare size ("0603") says nothing about the part class on its own — see
    // `map_footprint_for`, which disambiguates using the reference designator.
    match p.as_str() {
        "SOT23" | "SOT23-3" => "Package_TO_SOT_SMD:SOT-23".into(),
        "SOT23-5" => "Package_TO_SOT_SMD:SOT-23-5".into(),
        "SOIC8" | "SO08" | "SOIC-8" => "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm".into(),
        "SOIC14" | "SO14" => "Package_SO:SOIC-14_3.9x8.7mm_P1.27mm".into(),
        "TSSOP14" => "Package_SO:TSSOP-14_4.4x5mm_P0.65mm".into(),
        _ => format!("eagle:{package}"),
    }
}

/// [`map_footprint`], using the reference designator to settle an ambiguous
/// package name. Eagle projects often name a passive's package by size alone
/// (`0603`), which does not say whether it is a resistor or a capacitor — but
/// `R14` and `C9` do.
pub fn map_footprint_for(refdes: &str, package: &str) -> String {
    let direct = map_footprint(package);
    if !direct.starts_with("eagle:") {
        return direct;
    }
    let bare = package.trim().to_ascii_uppercase();
    if !bare.chars().all(|c| c.is_ascii_digit()) {
        return direct;
    }
    let prefix: String = refdes
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    let class = match prefix.as_str() {
        "R" => "R",
        "C" => "C",
        "L" => "L",
        _ => return direct,
    };
    map_footprint(&format!("{class}{bare}"))
}

/// What an Eagle schematic yields.
#[derive(Debug, Clone)]
pub struct EagleImport {
    /// The circuit, ready for the rest of the pipeline.
    pub circuit: Circuit,
    /// Parts skipped because they are drawing furniture rather than components —
    /// frames, ground and supply symbols — reported so the count is explainable.
    pub symbols_skipped: Vec<String>,
    /// Packages with no KiCad equivalent, carried through as `eagle:<name>`.
    pub unmapped_footprints: Vec<String>,
}

/// Parse an Eagle `.sch` into a [`Circuit`] with real nets.
pub fn parse_schematic(xml: &str, name: &str) -> EagleImport {
    let els = scan(xml);

    // device (library, deviceset, device) -> package, from the file's own libraries.
    let mut packages: HashMap<(String, String, String), String> = HashMap::new();
    // Track the library/deviceset a <device> is nested in.
    let mut lib = String::new();
    let mut devset = String::new();
    for e in &els {
        match e.name.as_str() {
            "library" => lib = e.attr("name").unwrap_or_default().to_string(),
            "deviceset" => devset = e.attr("name").unwrap_or_default().to_string(),
            "device" => {
                if let Some(pkg) = e.attr("package") {
                    let dev = e.attr("name").unwrap_or_default().to_string();
                    packages.insert((lib.clone(), devset.clone(), dev), pkg.to_string());
                }
            }
            _ => {}
        }
    }

    let mut circuit = Circuit::new(name);
    let mut symbols_skipped = Vec::new();
    let mut unmapped = Vec::new();

    for e in els.iter().filter(|e| e.name == "part" && e.inside("parts")) {
        let Some(refdes) = e.attr("name") else {
            continue;
        };
        let key = (
            e.attr("library").unwrap_or_default().to_string(),
            e.attr("deviceset").unwrap_or_default().to_string(),
            e.attr("device").unwrap_or_default().to_string(),
        );
        // No package means it is not a physical component: a sheet frame, a GND
        // or supply symbol. Those carry no BOM line and no footprint; the net
        // they name already records what they mean.
        let Some(pkg) = packages.get(&key).filter(|p| !p.is_empty()) else {
            symbols_skipped.push(refdes.to_string());
            continue;
        };
        let footprint = map_footprint_for(refdes, pkg);
        if footprint.starts_with("eagle:") && !unmapped.contains(&pkg.to_string()) {
            unmapped.push(pkg.to_string());
        }
        let mut part =
            Part::new(refdes, e.attr("value").unwrap_or_default()).with_footprint(footprint);
        // Keep the origin visible in the model, the way a netlist's libsource is.
        part.library_part = Some(format!("{}:{}", key.0, key.1));
        circuit.parts.push(part);
    }

    // Nets: every <pinref> under a <net>, whichever segment it sits in.
    let placed: Vec<&str> = circuit.parts.iter().map(|p| p.refdes.0.as_str()).collect();
    let mut current: Option<(String, Vec<PinRef>)> = None;
    let mut nets: Vec<Net> = Vec::new();
    for e in &els {
        match e.name.as_str() {
            "net" if e.inside("nets") => {
                if let Some((n, pins)) = current.take() {
                    if pins.len() > 1 {
                        nets.push(Net::new(n, pins));
                    }
                }
                current = Some((e.attr("name").unwrap_or_default().to_string(), Vec::new()));
            }
            "pinref" => {
                if let Some((_, pins)) = current.as_mut() {
                    let (Some(part), Some(pin)) = (e.attr("part"), e.attr("pin")) else {
                        continue;
                    };
                    // Skip pins of the symbols we dropped — a GND symbol's pin is
                    // not a connection, it *is* the net.
                    if placed.contains(&part) {
                        pins.push(PinRef::new(part, pin));
                    }
                }
            }
            _ => {}
        }
    }
    if let Some((n, pins)) = current.take() {
        if pins.len() > 1 {
            nets.push(Net::new(n, pins));
        }
    }
    circuit.nets = nets;

    EagleImport {
        circuit,
        symbols_skipped,
        unmapped_footprints: unmapped,
    }
}

/// One placed component from an Eagle `.brd`.
#[derive(Debug, Clone, PartialEq)]
pub struct EaglePlacement {
    pub refdes: String,
    pub package: String,
    pub x_mm: f64,
    pub y_mm: f64,
    pub rotation_deg: f64,
    pub back: bool,
}

/// Parse an Eagle `.brd` for where each component sits — the existing layout,
/// reusable as-is rather than re-derived.
pub fn parse_board(xml: &str) -> Vec<EaglePlacement> {
    scan(xml)
        .into_iter()
        .filter(|e| e.name == "element")
        .filter_map(|e| {
            // Eagle rotation is "R90" / "MR90" — M meaning mirrored onto the back.
            let rot = e.attr("rot").unwrap_or("R0");
            let back = rot.starts_with('M');
            let deg: f64 = rot.trim_start_matches(['M', 'R']).parse().unwrap_or(0.0);
            Some(EaglePlacement {
                refdes: e.attr("name")?.to_string(),
                package: e.attr("package").unwrap_or_default().to_string(),
                x_mm: e.attr("x")?.parse().ok()?,
                y_mm: e.attr("y")?.parse().ok()?,
                rotation_deg: deg,
                back,
            })
        })
        .collect()
}

/// Emit SKiDL for an imported circuit, so it becomes a definition that can be
/// edited and re-run rather than a fixed artefact.
///
/// The generated script is a starting point, not a finished design: an Eagle
/// schematic knows a part's package but not which KiCad symbol library it should
/// come from, so parts are emitted against `Device` where the value makes that
/// obvious and left marked otherwise. It is meant to be read and corrected.
pub fn to_skidl(imp: &EagleImport) -> String {
    let c = &imp.circuit;
    let mut s = String::new();
    s.push_str(&format!(
        "\"\"\"{} — imported from Eagle by legion-of-bom.\n\n\
         Generated from an Eagle schematic: the connectivity is real, taken from\n\
         the file's own nets. Review before building — footprints marked\n\
         `eagle:` had no KiCad equivalent, and symbol libraries are inferred from\n\
         each part's reference designator.\n\"\"\"\n\n\
         from skidl import Part, Net, generate_netlist\n\n",
        c.name
    ));

    if !imp.unmapped_footprints.is_empty() {
        s.push_str("# Footprints with no KiCad equivalent — resolve these first:\n");
        for p in &imp.unmapped_footprints {
            s.push_str(&format!("#   eagle:{p}\n"));
        }
        s.push('\n');
    }

    s.push_str("# --- parts ---\n");
    for p in &c.parts {
        let (lib, dev) = symbol_for(&p.refdes.0);
        s.push_str(&format!(
            "{} = Part(\"{lib}\", \"{dev}\", ref=\"{}\", value=\"{}\", footprint=\"{}\")\n",
            py_name(&p.refdes.0),
            p.refdes.0,
            p.value.replace('"', "\\\""),
            p.footprint.as_deref().unwrap_or(""),
        ));
    }

    s.push_str("\n# --- nets ---\n");
    for n in &c.nets {
        let var = py_name(&n.name);
        s.push_str(&format!("{var} = Net(\"{}\")\n", n.name));
        for pin in &n.pins {
            s.push_str(&format!(
                "{var} += {}[\"{}\"]\n",
                py_name(&pin.refdes.0),
                pin.pin
            ));
        }
    }
    s.push_str("\ngenerate_netlist()\n");
    s
}

/// Guess a KiCad symbol from a reference designator. Eagle records the package,
/// not the schematic symbol library, so this is a convenience for the common
/// cases and explicitly a guess for the rest.
fn symbol_for(refdes: &str) -> (&'static str, &'static str) {
    let prefix: String = refdes
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    match prefix.as_str() {
        "R" => ("Device", "R"),
        "C" => ("Device", "C"),
        "L" => ("Device", "L"),
        "D" => ("Device", "D"),
        "Q" => ("Device", "Q_NPN_BEC"),
        "RV" | "POT" => ("Device", "R_Potentiometer"),
        "J" | "JP" => ("Connector", "Conn_01x02"),
        _ => ("Device", "REVIEW_ME"),
    }
}

/// A Python-safe identifier for a refdes or net name.
fn py_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, 'n');
    }
    out.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCH: &str = r#"<?xml version="1.0"?>
<!DOCTYPE eagle SYSTEM "eagle.dtd">
<eagle version="7.7.0"><drawing><schematic>
 <libraries>
  <library name="rcl">
   <devicesets>
    <deviceset name="R-US_"><devices><device name="R0603" package="R0603"/></devices></deviceset>
    <deviceset name="C-US"><devices><device name="C0603" package="C0603"/></devices></deviceset>
   </devicesets>
  </library>
  <library name="supply">
   <devicesets>
    <deviceset name="GND"><devices><device name="" package=""/></devices></deviceset>
   </devicesets>
  </library>
 </libraries>
 <parts>
  <part name="R1" library="rcl" deviceset="R-US_" device="R0603" value="10k"/>
  <part name="C1" library="rcl" deviceset="C-US" device="C0603" value="100n"/>
  <part name="GND1" library="supply" deviceset="GND" device=""/>
 </parts>
 <sheets><sheet>
  <nets>
   <net name="MID" class="0">
    <segment>
     <wire x1="0" y1="0" x2="1" y2="0" width="0.15" layer="91"/>
     <pinref part="R1" gate="G$1" pin="2"/>
     <pinref part="C1" gate="G$1" pin="1"/>
    </segment>
   </net>
   <net name="GND" class="0">
    <segment>
     <pinref part="C1" gate="G$1" pin="2"/>
     <pinref part="GND1" gate="1" pin="GND"/>
    </segment>
   </net>
  </nets>
 </sheet></sheets>
</schematic></drawing></eagle>"#;

    #[test]
    fn recovers_parts_and_real_connectivity() {
        let imp = parse_schematic(SCH, "demo");
        // The GND *symbol* is drawing furniture, not a component.
        assert_eq!(imp.circuit.parts.len(), 2);
        assert_eq!(imp.symbols_skipped, vec!["GND1"]);

        let r1 = &imp.circuit.parts[0];
        assert_eq!(r1.refdes.0, "R1");
        assert_eq!(r1.value, "10k");
        assert_eq!(
            r1.footprint.as_deref(),
            Some("Resistor_SMD:R_0603_1608Metric")
        );
        assert_eq!(r1.library_part.as_deref(), Some("rcl:R-US_"));

        // MID joins two parts; GND has only one real component pin left after the
        // supply symbol is dropped, so it is not a connection between parts.
        let mid = imp.circuit.nets.iter().find(|n| n.name == "MID").unwrap();
        assert_eq!(mid.pins.len(), 2);
        assert!(imp.circuit.nets.iter().all(|n| n.name != "GND"));
    }

    #[test]
    fn maps_common_packages_and_flags_the_rest() {
        assert_eq!(map_footprint("R0402"), "Resistor_SMD:R_0402_1005Metric");
        assert_eq!(map_footprint("C0805"), "Capacitor_SMD:C_0805_2012Metric");
        assert_eq!(map_footprint("SOT23"), "Package_TO_SOT_SMD:SOT-23");
        // Unknown packages are visibly unmapped, never silently wrong.
        assert_eq!(map_footprint("WEIRD-QFN"), "eagle:WEIRD-QFN");
        // A bare size is ambiguous alone; the designator settles it.
        assert_eq!(map_footprint("0603"), "eagle:0603");
        assert_eq!(
            map_footprint_for("R14", "0603"),
            "Resistor_SMD:R_0603_1608Metric"
        );
        assert_eq!(
            map_footprint_for("C9", "0603"),
            "Capacitor_SMD:C_0603_1608Metric"
        );
        // An IC with an odd package is still flagged, not guessed at.
        assert_eq!(map_footprint_for("U1", "WEIRD-QFN"), "eagle:WEIRD-QFN");
        let imp = parse_schematic(SCH, "d");
        assert!(imp.unmapped_footprints.is_empty());
    }

    #[test]
    fn reads_board_placements_with_rotation_and_side() {
        let brd = r#"<eagle><drawing><board><elements>
          <element name="R1" library="rcl" package="R0603" value="10k" x="12.7" y="25.4" rot="R90"/>
          <element name="C1" library="rcl" package="C0603" value="1u" x="5.0" y="6.0" rot="MR180"/>
          <element name="U1" library="x" package="SOIC8" value="TL072" x="1.0" y="2.0"/>
        </elements></board></drawing></eagle>"#;
        let ps = parse_board(brd);
        assert_eq!(ps.len(), 3);
        assert!((ps[0].x_mm - 12.7).abs() < 1e-9 && (ps[0].rotation_deg - 90.0).abs() < 1e-9);
        assert!(!ps[0].back);
        // "MR180" is mirrored: the part is on the underside.
        assert!(ps[1].back && (ps[1].rotation_deg - 180.0).abs() < 1e-9);
        // A missing rot attribute is simply R0.
        assert_eq!(ps[2].rotation_deg, 0.0);
    }

    #[test]
    fn emits_runnable_looking_skidl() {
        let imp = parse_schematic(SCH, "demo");
        let py = to_skidl(&imp);
        assert!(py.contains("from skidl import"));
        assert!(py.contains(r#"Part("Device", "R", ref="R1""#), "{py}");
        assert!(py.contains(r#"footprint="Resistor_SMD:R_0603_1608Metric""#));
        assert!(py.contains(r#"mid = Net("MID")"#), "{py}");
        assert!(py.contains(r#"mid += r1["2"]"#), "{py}");
    }

    /// A '>' inside a quoted attribute must not end the tag early.
    #[test]
    fn scanner_survives_awkward_attributes() {
        let els = scan(r#"<a b="x&gt;y" c="2"/><d/>"#);
        assert_eq!(els.len(), 2);
        assert_eq!(els[0].attr("b"), Some("x>y"));
        assert_eq!(els[0].attr("c"), Some("2"));
        assert_eq!(els[1].name, "d");
    }
}
