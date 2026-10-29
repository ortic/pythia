//! Breakpoint conditions and watch paths, evaluated on the debugger side
//! from variable values (JDWP can't evaluate PL/SQL expressions).
//!
//! Conditions are comparisons joined by `and`/`or` (with `not` and
//! parentheses): `l_total > 3 and l_name is not null`, `p_mode = 'X'`,
//! `l_flag = true`. Values compare as numbers when both sides are numbers,
//! else as text. Like SQL, comparing with NULL is never true.

use crate::types::Variable;

/// Splits `l_rec.items(2).name` into `["L_REC", "ITEMS", "(2)", "NAME"]`.
/// Unquoted names are upper-cased; `"Quoted"` names keep their case. Text
/// keys of index-by tables stay quoted: `g_rates('L1')` → `["G_RATES", "('L1')"]`.
pub fn parse_path(expr: &str) -> Option<Vec<String>> {
    parse_path_with(expr, &|_| None)
}

/// [`parse_path`] where a subscript may be a variable (`lines(i)`, `art_(rec_.n)`):
/// `subscript` turns its path into the segment, e.g. [`subscripts`].
pub fn parse_path_with(
    expr: &str,
    subscript: &dyn Fn(&[String]) -> Option<String>,
) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = expr.trim().chars().peekable();
    let mut expect_name = true;
    while let Some(&c) = chars.peek() {
        match c {
            '.' if !expect_name => {
                chars.next();
                expect_name = true;
            }
            '(' if !expect_name => {
                chars.next();
                while chars.next_if(|c| c.is_whitespace()).is_some() {}
                if chars.next_if_eq(&'\'').is_some() {
                    // A text key: `('L1')`, quotes doubled inside.
                    let mut key = String::new();
                    loop {
                        match chars.next()? {
                            '\'' if chars.next_if_eq(&'\'').is_some() => key.push('\''),
                            '\'' => break,
                            c => key.push(c),
                        }
                    }
                    while chars.next_if(|c| c.is_whitespace()).is_some() {}
                    chars.next_if_eq(&')')?;
                    out.push(format!("('{}')", key.replace('\'', "''")));
                } else {
                    let mut index = String::new();
                    for c in chars.by_ref() {
                        if c == ')' {
                            break;
                        }
                        index.push(c);
                    }
                    let index = index.trim();
                    let digits = index.strip_prefix('-').unwrap_or(index);
                    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                        out.push(format!("({index})"));
                    } else {
                        out.push(subscript(&parse_path_with(index, subscript)?)?);
                    }
                }
            }
            '"' if expect_name => {
                chars.next();
                let mut name = String::new();
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    name.push(c);
                }
                out.push(name);
                expect_name = false;
            }
            c if expect_name && is_ident_start(c) => {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if !is_ident(c) {
                        break;
                    }
                    name.push(c.to_ascii_uppercase());
                    chars.next();
                }
                out.push(name);
                expect_name = false;
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            _ => return None,
        }
    }
    (!out.is_empty() && !expect_name).then_some(out)
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '$' | '#')
}

/// Follows `path` (from the first segment on) through `vars`, matching
/// names case-insensitively.
pub fn lookup<'a>(vars: &'a [Variable], path: &[String]) -> Option<&'a Variable> {
    let (first, rest) = path.split_first()?;
    let var = vars.iter().find(|v| v.name.eq_ignore_ascii_case(first))?;
    descend(var, rest).ok()
}

/// Subscripts from the values of `vars`, for [`parse_path_with`]: `(3)`,
/// or `('L1')` for text.
pub fn subscripts(vars: &[Variable]) -> impl Fn(&[String]) -> Option<String> + '_ {
    |path| {
        let var = lookup(vars, path).filter(|v| v.children.is_empty())?;
        let value = var.value.as_ref()?;
        Some(if is_text(&var.type_name) {
            format!("('{}')", value.replace('\'', "''"))
        } else {
            format!("({value})")
        })
    }
}

/// Whether values of this type (as the debugger names it) are text.
pub fn is_text(type_name: &str) -> bool {
    matches!(
        type_name,
        "VARCHAR2" | "VARCHAR" | "STRING" | "CHAR" | "NVARCHAR2" | "NCHAR" | "LONG"
    )
}

/// Whether a field or element name matches a path segment: names ignore
/// case, text keys don't.
pub fn matches(name: &str, segment: &str) -> bool {
    if segment.starts_with("('") {
        name == segment
    } else {
        name.eq_ignore_ascii_case(segment)
    }
}

/// Follows `rest` into a variable's fields/elements.
pub fn descend<'a>(var: &'a Variable, rest: &[String]) -> Result<&'a Variable, String> {
    let mut var = var;
    for seg in rest {
        if var.type_name == OPAQUE {
            return Err(no_contents(var));
        }
        var = var
            .children
            .iter()
            .find(|c| matches(&c.name, seg))
            .ok_or_else(|| format!("{} has no {seg}", var.name.to_lowercase()))?;
    }
    Ok(var)
}

/// Type of values whose contents Oracle's agent doesn't show: their type
/// was compiled without debug information.
pub const OPAQUE: &str = "OPAQUE";

/// The opaque variable that `rest` would have to go into from `var`.
pub fn opaque_on_path<'a>(var: &'a Variable, rest: &[String]) -> Option<&'a Variable> {
    let mut var = var;
    for seg in rest {
        if var.type_name == OPAQUE {
            return Some(var);
        }
        var = var.children.iter().find(|c| matches(&c.name, seg))?;
    }
    None
}

/// Why there are no fields or elements to follow in an opaque variable.
pub fn no_contents(var: &Variable) -> String {
    format!(
        "{} is {OPAQUE}: its contents aren't available",
        var.name.to_lowercase()
    )
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Path(String),
    Number(String),
    Text(String),
    Op(&'static str),
    Word(String),
    LParen,
    RParen,
}

fn tokenize(src: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                out.push(Token::LParen);
                i += 1;
            }
            ')' => {
                out.push(Token::RParen);
                i += 1;
            }
            '\'' => {
                let mut text = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err("unterminated string".into()),
                        Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                            text.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(&c) => {
                            text.push(c);
                            i += 1;
                        }
                    }
                }
                out.push(Token::Text(text));
            }
            '=' => {
                out.push(Token::Op("="));
                i += 1;
            }
            '<' | '>' | '!' => {
                let next = chars.get(i + 1).copied();
                let op = match (c, next) {
                    ('<', Some('=')) => "<=",
                    ('>', Some('=')) => ">=",
                    ('<', Some('>')) | ('!', Some('=')) => "!=",
                    ('<', _) => "<",
                    ('>', _) => ">",
                    _ => return Err(format!("unexpected {c}")),
                };
                i += op.len();
                out.push(Token::Op(op));
            }
            c if c.is_ascii_digit()
                || (c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
                || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) =>
            {
                let start = i;
                i += 1;
                while chars
                    .get(i)
                    .is_some_and(|c| c.is_ascii_digit() || *c == '.')
                {
                    i += 1;
                }
                out.push(Token::Number(chars[start..i].iter().collect()));
            }
            c if is_ident_start(c) || c == '"' => {
                // A path: names joined by dots, with (n) indexes.
                let start = i;
                let mut depth = 0;
                while let Some(&c) = chars.get(i) {
                    let ok = is_ident(c)
                        || c == '.'
                        || c == '"'
                        || (c == '(' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
                        || (c == ')' && depth > 0)
                        || (depth > 0 && c.is_ascii_digit());
                    if !ok {
                        break;
                    }
                    match c {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let upper = text.to_ascii_uppercase();
                if matches!(
                    upper.as_str(),
                    "AND" | "OR" | "NOT" | "IS" | "NULL" | "TRUE" | "FALSE"
                ) {
                    out.push(Token::Word(upper));
                } else {
                    out.push(Token::Path(text));
                }
            }
            c => return Err(format!("unexpected {c}")),
        }
    }
    Ok(out)
}

/// Evaluates a condition; `value` resolves a variable path to its value
/// (`Ok(None)` = NULL) or an error (unknown variable).
pub fn evaluate(
    condition: &str,
    value: &mut dyn FnMut(&str) -> Result<Option<String>, String>,
) -> Result<bool, String> {
    let tokens = tokenize(condition)?;
    if tokens.is_empty() {
        return Err("empty condition".into());
    }
    let mut p = Parser {
        tokens,
        pos: 0,
        value,
    };
    let result = p.or()?;
    if p.pos != p.tokens.len() {
        return Err(format!("unexpected {:?}", p.tokens[p.pos]));
    }
    Ok(result)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    value: &'a mut dyn FnMut(&str) -> Result<Option<String>, String>,
}

impl Parser<'_> {
    fn peek_word(&self, word: &str) -> bool {
        matches!(self.tokens.get(self.pos), Some(Token::Word(w)) if w == word)
    }

    fn or(&mut self) -> Result<bool, String> {
        let mut result = self.and()?;
        while self.peek_word("OR") {
            self.pos += 1;
            // Evaluate both sides: a typo should be reported either way.
            let right = self.and()?;
            result = result || right;
        }
        Ok(result)
    }

    fn and(&mut self) -> Result<bool, String> {
        let mut result = self.not()?;
        while self.peek_word("AND") {
            self.pos += 1;
            let right = self.not()?;
            result = result && right;
        }
        Ok(result)
    }

    fn not(&mut self) -> Result<bool, String> {
        if self.peek_word("NOT") {
            self.pos += 1;
            return Ok(!self.not()?);
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<bool, String> {
        if self.tokens.get(self.pos) == Some(&Token::LParen) {
            self.pos += 1;
            let result = self.or()?;
            if self.tokens.get(self.pos) != Some(&Token::RParen) {
                return Err("missing )".into());
            }
            self.pos += 1;
            return Ok(result);
        }
        let left = self.operand()?;
        if self.peek_word("IS") {
            self.pos += 1;
            let negate = self.peek_word("NOT");
            if negate {
                self.pos += 1;
            }
            if !self.peek_word("NULL") {
                return Err("expected NULL after IS".into());
            }
            self.pos += 1;
            return Ok(left.is_none() != negate);
        }
        let op = match self.tokens.get(self.pos) {
            Some(Token::Op(op)) => *op,
            // A lone boolean variable: `l_flag`.
            _ => {
                return Ok(left
                    .as_deref()
                    .is_some_and(|v| v.eq_ignore_ascii_case("true")));
            }
        };
        self.pos += 1;
        let right = self.operand()?;
        let (Some(l), Some(r)) = (left, right) else {
            return Ok(false);
        };
        let ordering = match (l.trim().parse::<f64>(), r.trim().parse::<f64>()) {
            (Ok(a), Ok(b)) => a.partial_cmp(&b),
            _ if is_bool(&l) && is_bool(&r) => {
                Some(l.to_ascii_lowercase().cmp(&r.to_ascii_lowercase()))
            }
            _ => Some(l.cmp(&r)),
        };
        let Some(ordering) = ordering else {
            return Ok(false);
        };
        use std::cmp::Ordering::*;
        Ok(match op {
            "=" => ordering == Equal,
            "!=" => ordering != Equal,
            "<" => ordering == Less,
            "<=" => ordering != Greater,
            ">" => ordering == Greater,
            ">=" => ordering != Less,
            _ => unreachable!(),
        })
    }

    fn operand(&mut self) -> Result<Option<String>, String> {
        let token = self
            .tokens
            .get(self.pos)
            .cloned()
            .ok_or("incomplete condition")?;
        self.pos += 1;
        match token {
            Token::Number(n) | Token::Text(n) => Ok(Some(n)),
            Token::Word(w) if w == "NULL" => Ok(None),
            Token::Word(w) if w == "TRUE" || w == "FALSE" => Ok(Some(w.to_ascii_lowercase())),
            Token::Path(p) => (self.value)(&p),
            other => Err(format!("unexpected {other:?}")),
        }
    }
}

fn is_bool(v: &str) -> bool {
    v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str, value: Option<&str>, children: Vec<Variable>) -> Variable {
        Variable {
            name: name.into(),
            type_name: String::new(),
            value: value.map(Into::into),
            children,
        }
    }

    #[test]
    fn paths() {
        assert_eq!(parse_path("l_total"), Some(vec!["L_TOTAL".into()]));
        assert_eq!(
            parse_path(" pkg.g_count "),
            Some(vec!["PKG".into(), "G_COUNT".into()])
        );
        assert_eq!(
            parse_path("l_rec.items(2).\"Name\""),
            Some(vec![
                "L_REC".into(),
                "ITEMS".into(),
                "(2)".into(),
                "Name".into()
            ])
        );
        assert_eq!(
            parse_path("g_rates( 'L1' )"),
            Some(vec!["G_RATES".into(), "('L1')".into()])
        );
        assert_eq!(
            parse_path("names('O''Brien').x"),
            Some(vec!["NAMES".into(), "('O''Brien')".into(), "X".into()])
        );
        assert_eq!(parse_path("t(-1)"), Some(vec!["T".into(), "(-1)".into()]));
        assert_eq!(parse_path("a."), None);
        assert_eq!(parse_path("a + b"), None);
        assert_eq!(parse_path("a(x)"), None);
        assert_eq!(parse_path("a('open"), None);
        assert_eq!(parse_path("a('x' y)"), None);
    }

    /// Subscripts that are variables take their values (#165).
    #[test]
    fn variable_subscripts() {
        let vars = vec![
            var("I", Some("3"), vec![]),
            Variable {
                type_name: "VARCHAR2".into(),
                ..var("L_CODE", Some("O'K"), vec![])
            },
            var("L_REC", None, vec![var("N", Some("2"), vec![])]),
            var("L_NULL", None, vec![]),
        ];
        let parse = |expr| parse_path_with(expr, &subscripts(&vars));
        assert_eq!(
            parse("l_order.lines(i).qty"),
            Some(vec![
                "L_ORDER".into(),
                "LINES".into(),
                "(3)".into(),
                "QTY".into()
            ])
        );
        assert_eq!(
            parse("g_rates(l_code)"),
            Some(vec!["G_RATES".into(), "('O''K')".into()])
        );
        assert_eq!(
            parse("art_( l_rec.n )"),
            Some(vec!["ART_".into(), "(2)".into()])
        );
        assert_eq!(parse("a(2)"), Some(vec!["A".into(), "(2)".into()]));
        assert_eq!(parse("a(l_null)"), None, "NULL");
        assert_eq!(parse("a(l_rec)"), None, "a record");
        assert_eq!(parse("a(j)"), None, "unknown");
        assert_eq!(parse("a(i + 1)"), None);
    }

    #[test]
    fn lookups() {
        let vars = vec![
            var("L_TOTAL", Some("6"), vec![]),
            var(
                "L_REC",
                None,
                vec![var("A", Some("7"), vec![]), var("B", Some("bee"), vec![])],
            ),
            var(
                "L_ARR",
                Some("2 element(s)"),
                vec![var("(1)", Some("10"), vec![])],
            ),
            var(
                "G_RATES",
                Some("2 element(s)"),
                vec![
                    var("('L1')", Some("0.05"), vec![]),
                    var("('l1')", Some("0.5"), vec![]),
                ],
            ),
        ];
        let get = |p: &str| lookup(&vars, &parse_path(p).unwrap()).and_then(|v| v.value.clone());
        assert_eq!(get("l_total").as_deref(), Some("6"));
        assert_eq!(get("L_Rec.b").as_deref(), Some("bee"));
        assert_eq!(get("l_arr(1)").as_deref(), Some("10"));
        assert_eq!(get("l_rec.c"), None);
        assert_eq!(get("g_rates('L1')").as_deref(), Some("0.05"));
        assert_eq!(
            get("g_rates('l1')").as_deref(),
            Some("0.5"),
            "text keys keep case"
        );
        assert_eq!(get("g_rates('L2')"), None);
    }

    #[test]
    fn conditions() {
        let vars = vec![
            var("L_TOTAL", Some("6"), vec![]),
            var("L_NAME", Some("step 3"), vec![]),
            var("L_FLAG", Some("true"), vec![]),
            var("L_NOTE", None, vec![]),
        ];
        let eval = |c: &str| {
            evaluate(c, &mut |p| {
                let path = parse_path(p).ok_or_else(|| format!("bad path {p}"))?;
                lookup(&vars, &path)
                    .map(|v| v.value.clone())
                    .ok_or_else(|| format!("unknown variable {p}"))
            })
        };
        assert_eq!(eval("l_total > 3"), Ok(true));
        assert_eq!(eval("l_total >= 6.0 and l_total < 10"), Ok(true));
        assert_eq!(eval("l_total = 5 or l_name = 'step 3'"), Ok(true));
        assert_eq!(eval("l_total != 6"), Ok(false));
        assert_eq!(eval("l_total <> 6"), Ok(false));
        assert_eq!(eval("l_note is null and l_name is not null"), Ok(true));
        assert_eq!(eval("l_note = 'x'"), Ok(false), "NULL never compares");
        assert_eq!(eval("not (l_total > 3)"), Ok(false));
        assert_eq!(eval("l_flag"), Ok(true));
        assert_eq!(eval("l_flag = TRUE"), Ok(true));
        assert_eq!(eval("l_total > -1"), Ok(true));
        assert_eq!(eval("'it''s' = 'it''s'"), Ok(true));
        assert!(eval("l_missing > 1").is_err());
        assert!(eval("l_total >").is_err());
        assert!(eval("(l_total > 1").is_err());
        assert!(eval("").is_err());
    }
}
