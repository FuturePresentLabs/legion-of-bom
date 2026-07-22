//! A small S-expression reader **and writer**, shared by the KiCad netlist
//! parser ([`netlist`](crate::netlist)), the symbol/model resolver
//! ([`symbols`](crate::symbols)), and the board generator
//! ([`board`](crate::board)). No external dependency.
//!
//! Symbols (`footprint`, `smd`) and quoted strings (`"R1"`, `"F.Cu"`) are kept
//! distinct so the writer can round-trip a file faithfully — KiCad requires
//! strings quoted and bare symbols unquoted.

/// A parsed S-expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sexpr {
    /// A bare symbol: `footprint`, `smd`, `yes`.
    Sym(String),
    /// A quoted string's (unescaped) content: `R1`, `F.Cu`.
    Str(String),
    List(Vec<Sexpr>),
}

impl Sexpr {
    pub(crate) fn sym(s: impl Into<String>) -> Sexpr {
        Sexpr::Sym(s.into())
    }
    pub(crate) fn string(s: impl Into<String>) -> Sexpr {
        Sexpr::Str(s.into())
    }
    pub(crate) fn list(items: Vec<Sexpr>) -> Sexpr {
        Sexpr::List(items)
    }

    pub(crate) fn parse(input: &str) -> Result<Sexpr, String> {
        let tokens = tokenize(input)?;
        let mut pos = 0;
        let expr = parse_expr(&tokens, &mut pos)?;
        if pos != tokens.len() {
            return Err("trailing tokens after the top-level expression".into());
        }
        Ok(expr)
    }

    /// The atom text for a symbol or string (unquoted); `None` for a list.
    pub(crate) fn as_atom(&self) -> Option<&str> {
        match self {
            Sexpr::Sym(s) | Sexpr::Str(s) => Some(s),
            Sexpr::List(_) => None,
        }
    }

    pub(crate) fn as_list(&self) -> Option<&[Sexpr]> {
        match self {
            Sexpr::List(items) => Some(items),
            _ => None,
        }
    }

    pub(crate) fn as_list_mut(&mut self) -> Option<&mut Vec<Sexpr>> {
        match self {
            Sexpr::List(items) => Some(items),
            _ => None,
        }
    }

    /// The head symbol of a list, e.g. `comp` for `(comp …)`.
    pub(crate) fn head(&self) -> Option<&str> {
        self.as_list()?.first()?.as_atom()
    }

    /// The `n`th element of a list, as an atom.
    pub(crate) fn nth_atom(&self, n: usize) -> Option<&str> {
        self.as_list()?.get(n)?.as_atom()
    }

    /// The first direct child list whose head symbol equals `key`.
    pub(crate) fn get(&self, key: &str) -> Option<&Sexpr> {
        self.as_list()?.iter().find(|c| c.head() == Some(key))
    }

    /// All direct child lists whose head symbol equals `key`.
    pub(crate) fn get_all(&self, key: &str) -> Vec<&Sexpr> {
        self.as_list()
            .map(|items| items.iter().filter(|c| c.head() == Some(key)).collect())
            .unwrap_or_default()
    }

    /// For a child `(key value)`, the `value` atom.
    pub(crate) fn field(&self, key: &str) -> Option<&str> {
        self.get(key)?.nth_atom(1)
    }

    /// Serialize back to KiCad-style S-expression text (valid, tab-indented;
    /// exact whitespace differs from KiCad's own but parses identically).
    pub(crate) fn to_sexpr_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, indent: usize) {
        match self {
            Sexpr::Sym(s) => out.push_str(s),
            Sexpr::Str(s) => {
                out.push('"');
                for c in s.chars() {
                    if c == '"' || c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            }
            Sexpr::List(items) => {
                out.push('(');
                let has_sublist = items.iter().any(|x| matches!(x, Sexpr::List(_)));
                if !has_sublist {
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(' ');
                        }
                        item.write(out, indent);
                    }
                } else {
                    // Head + leading atoms inline; lists and everything after on
                    // their own indented lines.
                    let pad = "\t".repeat(indent + 1);
                    let mut broke = false;
                    for (i, item) in items.iter().enumerate() {
                        let is_list = matches!(item, Sexpr::List(_));
                        if i == 0 {
                            item.write(out, indent);
                        } else if !broke && !is_list {
                            out.push(' ');
                            item.write(out, indent);
                        } else {
                            broke = true;
                            out.push('\n');
                            out.push_str(&pad);
                            item.write(out, indent + 1);
                        }
                    }
                }
                out.push(')');
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    Sym(String),
    Str(String),
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
                            Some(escaped) => s.push(escaped),
                            None => return Err("unterminated escape in string".into()),
                        },
                        Some('"') => break,
                        Some(ch) => s.push(ch),
                        None => return Err("unterminated string literal".into()),
                    }
                }
                tokens.push(Token::Str(s));
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
                tokens.push(Token::Sym(s));
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
        Some(Token::Sym(s)) => {
            *pos += 1;
            Ok(Sexpr::Sym(s.clone()))
        }
        Some(Token::Str(s)) => {
            *pos += 1;
            Ok(Sexpr::Str(s.clone()))
        }
        Some(Token::Close) => Err("unexpected `)`".into()),
        None => Err("unexpected end of input".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quotes_nesting_and_accessors() {
        let s = Sexpr::parse(r#"(a "quoted value" (b c) (property "Sim.Name" "x"))"#).unwrap();
        assert_eq!(s.head(), Some("a"));
        assert_eq!(s.nth_atom(1), Some("quoted value"));
        assert_eq!(
            s.get("b").and_then(|b| b.as_list()).map(<[_]>::len),
            Some(2)
        );
        let prop = s
            .get_all("property")
            .into_iter()
            .find(|p| p.nth_atom(1) == Some("Sim.Name"))
            .and_then(|p| p.nth_atom(2));
        assert_eq!(prop, Some("x"));
    }

    #[test]
    fn rejects_unterminated_string() {
        assert!(Sexpr::parse(r#"(a "oops)"#).is_err());
    }

    #[test]
    fn round_trips_symbols_and_strings() {
        // Symbols stay bare, strings stay quoted, escapes survive.
        let src = r#"(footprint "R_0805" (layer "F.Cu") (attr smd) (descr "a \"quote\""))"#;
        let parsed = Sexpr::parse(src).unwrap();
        let out = parsed.to_sexpr_string();
        // Re-parse the output and compare structurally (whitespace differs).
        assert_eq!(Sexpr::parse(&out).unwrap(), parsed);
        assert!(out.contains("(attr smd)"), "symbols stay bare: {out}");
        assert!(out.contains(r#""F.Cu""#), "strings stay quoted: {out}");
    }

    #[test]
    fn builds_and_serializes() {
        let e = Sexpr::list(vec![
            Sexpr::sym("net"),
            Sexpr::sym("1"),
            Sexpr::string("IN"),
        ]);
        assert_eq!(e.to_sexpr_string(), r#"(net 1 "IN")"#);
    }
}
