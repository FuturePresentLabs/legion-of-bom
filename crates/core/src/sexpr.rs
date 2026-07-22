//! A small, self-contained S-expression reader shared by the KiCad netlist
//! parser ([`netlist`](crate::netlist)) and the symbol-library model resolver
//! ([`symbols`](crate::symbols)). No external dependency.

/// A parsed S-expression: an atom (symbol, number, or quoted string) or a
/// parenthesised list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sexpr {
    Atom(String),
    List(Vec<Sexpr>),
}

impl Sexpr {
    pub(crate) fn parse(input: &str) -> Result<Sexpr, String> {
        let tokens = tokenize(input)?;
        let mut pos = 0;
        let expr = parse_expr(&tokens, &mut pos)?;
        if pos != tokens.len() {
            return Err("trailing tokens after the top-level expression".into());
        }
        Ok(expr)
    }

    pub(crate) fn as_atom(&self) -> Option<&str> {
        match self {
            Sexpr::Atom(s) => Some(s),
            Sexpr::List(_) => None,
        }
    }

    pub(crate) fn as_list(&self) -> Option<&[Sexpr]> {
        match self {
            Sexpr::List(items) => Some(items),
            Sexpr::Atom(_) => None,
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

    #[test]
    fn parses_quotes_nesting_and_accessors() {
        let s = Sexpr::parse(r#"(a "quoted value" (b c) (property "Sim.Name" "x"))"#).unwrap();
        assert_eq!(s.head(), Some("a"));
        assert_eq!(s.nth_atom(1), Some("quoted value"));
        assert_eq!(
            s.get("b").and_then(|b| b.as_list()).map(<[_]>::len),
            Some(2)
        );
        // property-style lookup
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
}
