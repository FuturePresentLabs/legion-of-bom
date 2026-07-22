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

// ---- minimal S-expression reader ----------------------------------------

/// A parsed S-expression: either an atom (symbol, number, or quoted string) or
/// a parenthesised list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sexpr {
    Atom(String),
    List(Vec<Sexpr>),
}

impl Sexpr {
    fn parse(input: &str) -> Result<Sexpr, String> {
        let tokens = tokenize(input)?;
        let mut pos = 0;
        let expr = parse_expr(&tokens, &mut pos)?;
        if pos != tokens.len() {
            return Err("trailing tokens after the top-level expression".into());
        }
        Ok(expr)
    }

    fn as_atom(&self) -> Option<&str> {
        match self {
            Sexpr::Atom(s) => Some(s),
            Sexpr::List(_) => None,
        }
    }

    fn as_list(&self) -> Option<&[Sexpr]> {
        match self {
            Sexpr::List(items) => Some(items),
            Sexpr::Atom(_) => None,
        }
    }

    /// The head symbol of a list, e.g. `comp` for `(comp …)`.
    fn head(&self) -> Option<&str> {
        self.as_list()?.first()?.as_atom()
    }

    /// The first direct child list whose head symbol equals `key`.
    fn get(&self, key: &str) -> Option<&Sexpr> {
        self.as_list()?.iter().find(|c| c.head() == Some(key))
    }

    /// All direct child lists whose head symbol equals `key`.
    fn get_all(&self, key: &str) -> Vec<&Sexpr> {
        self.as_list()
            .map(|items| items.iter().filter(|c| c.head() == Some(key)).collect())
            .unwrap_or_default()
    }

    /// For a child `(key value)`, the `value` atom.
    fn field(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_list()?.get(1)?.as_atom()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    Atom(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '(' => {
                tokens.push(Token::Open);
                chars.next();
            }
            ')' => {
                tokens.push(Token::Close);
                chars.next();
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            '"' => {
                chars.next(); // opening quote
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some('\\') => match chars.next() {
                            // Fields we care about (refdes, values, footprints,
                            // net names) don't use escapes; keep the escaped char.
                            Some(escaped) => s.push(escaped),
                            None => return Err("unterminated escape in string".into()),
                        },
                        Some('"') => break,
                        Some(ch) => s.push(ch),
                        None => return Err("unterminated string literal".into()),
                    }
                }
                tokens.push(Token::Atom(s));
            }
            _ => {
                let mut s = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_whitespace() || ch == '(' || ch == ')' {
                        break;
                    }
                    s.push(ch);
                    chars.next();
                }
                tokens.push(Token::Atom(s));
            }
        }
    }
    Ok(tokens)
}

fn parse_expr(tokens: &[Token], pos: &mut usize) -> Result<Sexpr, String> {
    match tokens.get(*pos) {
        Some(Token::Open) => {
            *pos += 1;
            let mut items = Vec::new();
            loop {
                match tokens.get(*pos) {
                    Some(Token::Close) => {
                        *pos += 1;
                        return Ok(Sexpr::List(items));
                    }
                    Some(_) => items.push(parse_expr(tokens, pos)?),
                    None => return Err("unexpected end of input, expected `)`".into()),
                }
            }
        }
        Some(Token::Atom(s)) => {
            *pos += 1;
            Ok(Sexpr::Atom(s.clone()))
        }
        Some(Token::Close) => Err("unexpected `)`".into()),
        None => Err("unexpected end of input".into()),
    }
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
    fn tokenizer_handles_quotes_and_nesting() {
        let s = Sexpr::parse(r#"(a "quoted value" (b c))"#).unwrap();
        assert_eq!(s.head(), Some("a"));
        assert_eq!(s.as_list().unwrap()[1].as_atom(), Some("quoted value"));
        assert_eq!(
            s.get("b").and_then(|b| b.as_list()).map(|l| l.len()),
            Some(2)
        );
    }

    #[test]
    fn unterminated_string_is_an_error() {
        assert!(Sexpr::parse(r#"(a "oops)"#).is_err());
    }
}
