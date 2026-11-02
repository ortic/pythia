//! Turns the tokens of one statement into a [`Doc`].
//!
//! This is a forgiving structural walk, not a validating parser: it
//! recognizes clauses, blocks and declarations where it can and otherwise
//! passes tokens through with normalized spacing. Every token is emitted
//! exactly once, in order; the caller verifies that and falls back to the
//! original text if not.

use super::doc::{Doc, Row, cat, group, if_break, nest, rows, text};
use super::{Case, CommaStyle, FormatOptions, Wrap};
use crate::lexer::TokenKind;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Tok<'s> {
    pub kind: TokenKind,
    pub text: &'s str,
    /// Newlines between the previous token and this one.
    pub nl_before: u32,
}

fn is_comment(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::LineComment | TokenKind::BlockComment)
}

/// Nesting limit; deeper input is left untouched (stack safety).
const MAX_DEPTH: usize = 40;

/// Keywords followed by a space before `(`; other keywords (types,
/// functions like `varchar2(10)`, `exception_init(…)`) take none.
const SPACED_KEYWORDS: &[&str] = &[
    "ALL",
    "AND",
    "ANY",
    "AS",
    "BY",
    "CASE",
    "CHECK",
    "DEFAULT",
    "ELSE",
    "ELSIF",
    "EXISTS",
    "FROM",
    "GROUP",
    "IF",
    "IN",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LOOP",
    "MINUS",
    "NOT",
    "OBJECT",
    "OF",
    "ON",
    "OR",
    "OVER",
    "PRIOR",
    "RECORD",
    "RETURN",
    "SELECT",
    "SET",
    "SOME",
    "THEN",
    "UNION",
    "UNIQUE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WHILE",
    "WITH",
    "WITHIN",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "BEGIN",
    "PARTITION",
    "LIKE",
    "BETWEEN",
    "RETURNING",
    "MODE",
    "FOR",
    "OUT",
    "NOCOPY",
];

/// Keywords that are values: a following `-` is binary (`level - 1`).
const VALUE_KEYWORDS: &[&str] = &[
    "NULL",
    "END",
    "LEVEL",
    "ROWNUM",
    "ROWID",
    "SYSDATE",
    "SYSTIMESTAMP",
    "USER",
    "UID",
    "TRUE",
    "FALSE",
    "CURRENT_USER",
];

/// Words that precede a table name: `on t (cols)`, `references t (cols)`.
const NAME_BEFORE_COLUMNS: &[&str] = &["ON", "REFERENCES", "INTO", "TABLE", "INDEX", "VIEW", "KEY"];

/// Oracle words the lexer treats as identifiers but that read as keywords;
/// they follow the keyword case.
const EXTRA_KEYWORDS: &[&str] = &[
    "ALWAYS",
    "BOTH",
    "CACHE",
    "CYCLE",
    "DAY",
    "DISABLE",
    "ENABLE",
    "ERRORS",
    "FOLLOWING",
    "GENERATED",
    "HOUR",
    "IDENTITY",
    "KEEP",
    "LEADING",
    "LOCKED",
    "LOG",
    "LOGGING",
    "MAXVALUE",
    "MINUTE",
    "MINVALUE",
    "MONTH",
    "NOCACHE",
    "NOCYCLE",
    "NOLOGGING",
    "NOORDER",
    "NOVALIDATE",
    "PRECEDING",
    "PURGE",
    "REFERENCING",
    "REJECT",
    "SECOND",
    "SIBLINGS",
    "SKIP",
    "TABLESPACE",
    "TIME",
    "TRAILING",
    "UNBOUNDED",
    "WITHIN",
    "YEAR",
    "ZONE",
    "ROWS",
    "ONLY",
    "FETCH",
    "OFFSET",
    "INLINE",
    "FORCE",
    "NOFORCE",
    "RESULT",
    "BITMAP",
    "TEMPORARY",
    "GLOBAL",
    "PRESERVE",
    "DEFINITION",
    "INVISIBLE",
    "VISIBLE",
    "PERCENT",
    "TIES",
];

const SUBPROGRAM_PREFIXES: &[&str] = &[
    "MEMBER",
    "STATIC",
    "CONSTRUCTOR",
    "MAP",
    "ORDER",
    "OVERRIDING",
    "FINAL",
    "INSTANTIABLE",
    "NOT",
];

const TRIGGER_CLAUSES: &[&str] = &[
    "BEFORE",
    "AFTER",
    "INSTEAD",
    "FOR",
    "WHEN",
    "REFERENCING",
    "FOLLOWS",
    "PRECEDES",
    "ENABLE",
    "DISABLE",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Query,
    Insert,
    Update,
    Delete,
    Merge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Select,
    List,
    Cond,
    Join,
    Expr,
    SetOp,
    /// MERGE `WHEN [NOT] MATCHED THEN`: following clauses are indented.
    MergeWhen,
}

#[derive(Debug, Clone, Copy)]
struct Clause {
    kind: Kind,
    words: usize,
}

type Stop<'x, 'a, 's> = &'x dyn Fn(&Layout<'a, 's>) -> bool;

pub(crate) struct Layout<'a, 's> {
    syntax: crate::Syntax,
    toks: &'a [Tok<'s>],
    pos: usize,
    o: &'a FormatOptions,
    depth: usize,
    ctx: Ctx,
    /// Last significant token emitted, and the one before it.
    last: Option<Tok<'s>>,
    prev2: Option<Tok<'s>>,
    /// The last token was a unary sign: no space after it.
    unary: bool,
    /// Structure the formatter doesn't handle: keep the statement as is.
    pub bail: bool,
    /// Code that isn't to be reformatted at all (wrapped, Java).
    pub opaque: bool,
    /// The last subprogram had a body (not just a declaration).
    had_body: bool,
}

/// Separator between items of a declaration or statement list.
enum Item {
    /// Alignable `name | rest` line.
    Simple {
        name: Doc,
        rest: Doc,
    },
    Other(Doc),
}

impl<'a, 's> Layout<'a, 's> {
    pub fn new(toks: &'a [Tok<'s>], o: &'a FormatOptions, syntax: crate::Syntax) -> Self {
        Self {
            syntax,
            toks,
            pos: 0,
            o,
            depth: 0,
            ctx: Ctx::Query,
            last: None,
            prev2: None,
            unary: false,
            bail: false,
            opaque: false,
            had_body: false,
        }
    }

    // ----- token access ----------------------------------------------------

    fn sig(&self, n: usize) -> Option<Tok<'s>> {
        self.toks[self.pos.min(self.toks.len())..]
            .iter()
            .filter(|t| !is_comment(t.kind))
            .nth(n)
            .copied()
    }

    fn peek(&self) -> Option<Tok<'s>> {
        self.sig(0)
    }

    fn word_at(&self, n: usize, w: &str) -> bool {
        self.sig(n)
            .is_some_and(|t| t.kind.is_word() && t.text.eq_ignore_ascii_case(w))
    }

    fn is(&self, w: &str) -> bool {
        self.word_at(0, w)
    }

    fn is_any(&self, ws: &[&str]) -> bool {
        ws.iter().any(|w| self.is(w))
    }

    fn words(&self, ws: &[&str]) -> bool {
        ws.iter().enumerate().all(|(i, w)| self.word_at(i, w))
    }

    fn text_is(&self, s: &str) -> bool {
        self.peek().is_some_and(|t| t.text == s)
    }

    fn text_at(&self, n: usize, s: &str) -> bool {
        self.sig(n).is_some_and(|t| t.text == s)
    }

    fn done(&self) -> bool {
        self.peek().is_none()
    }

    /// The next raw token (incl. comments) follows a blank line.
    fn blank_ahead(&self) -> bool {
        self.o.blank_lines > 0 && self.toks.get(self.pos).is_some_and(|t| t.nl_before >= 2)
    }

    fn sep(&self) -> Doc {
        if self.blank_ahead() {
            Doc::Blank
        } else {
            Doc::HardLine
        }
    }

    fn enter(&mut self) -> bool {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.bail = true;
        }
        !self.bail
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    // ----- emitting tokens -------------------------------------------------

    fn token_text(&self, t: &Tok<'s>) -> String {
        // On MySQL a table's name is as written (on Linux): a word that may
        // be one keeps its case, User and Session as much as a column
        // (#419). Only MySQL's reserved words, which can't be names, change.
        let named = self.syntax.is_mysql() && !crate::keywords::is_mysql_reserved(t.text);
        match t.kind {
            TokenKind::Keyword | TokenKind::Ident if named => t.text.to_owned(),
            TokenKind::Keyword => apply_case(self.o.keyword_case, t.text),
            TokenKind::Ident
                if EXTRA_KEYWORDS
                    .iter()
                    .any(|k| t.text.eq_ignore_ascii_case(k)) =>
            {
                apply_case(self.o.keyword_case, t.text)
            }
            TokenKind::Ident => apply_case(self.o.identifier_case, t.text),
            TokenKind::LineComment => t.text.trim_end().to_owned(),
            _ => t.text.to_owned(),
        }
    }

    fn comment(&mut self, out: &mut Vec<Doc>, leading: bool) {
        let Some(c) = self.toks.get(self.pos).copied() else {
            return;
        };
        self.pos += 1;
        if leading {
            out.push(if c.nl_before >= 2 && self.o.blank_lines > 0 {
                Doc::Blank
            } else if c.nl_before >= 1 {
                Doc::HardLine
            } else {
                Doc::Space
            });
        } else {
            out.push(Doc::Space);
        }
        out.push(text(self.token_text(&c)));
        let next_on_new_line = self.toks.get(self.pos).is_some_and(|t| t.nl_before >= 1);
        if c.kind == TokenKind::LineComment || next_on_new_line {
            out.push(Doc::HardLine);
        }
    }

    /// Comments before the next significant token.
    fn leading_comments(&mut self) -> Doc {
        let mut out = Vec::new();
        while self.toks.get(self.pos).is_some_and(|t| is_comment(t.kind)) {
            self.comment(&mut out, true);
        }
        cat(out)
    }

    /// The next token with its comments, without a leading space.
    fn take(&mut self) -> Doc {
        let mut out = vec![self.leading_comments()];
        let Some(t) = self.toks.get(self.pos).copied() else {
            return cat(out);
        };
        self.pos += 1;
        let sign =
            matches!(t.text, "-" | "+") && t.kind == TokenKind::Operator && self.sign_is_unary();
        out.push(text(self.token_text(&t)));
        self.prev2 = self.last;
        self.last = Some(t);
        self.unary = sign;
        while self
            .toks
            .get(self.pos)
            .is_some_and(|c| is_comment(c.kind) && c.nl_before == 0)
        {
            self.comment(&mut out, false);
        }
        cat(out)
    }

    /// The next token with a space before it where one belongs.
    fn next(&mut self) -> Doc {
        let space = self.peek().is_some_and(|t| self.needs_space(&t));
        let tok = self.take();
        if space {
            cat(vec![Doc::Space, tok])
        } else {
            tok
        }
    }

    fn sign_is_unary(&self) -> bool {
        match self.last {
            None => true,
            Some(p) => match p.kind {
                TokenKind::Operator
                | TokenKind::LParen
                | TokenKind::Comma
                | TokenKind::Semicolon => true,
                TokenKind::Keyword => !VALUE_KEYWORDS
                    .iter()
                    .any(|k| p.text.eq_ignore_ascii_case(k)),
                _ => false,
            },
        }
    }

    fn needs_space(&self, next: &Tok<'s>) -> bool {
        let Some(prev) = self.last else { return false };
        if self.unary {
            return false;
        }
        let (p, n) = (prev.text, next.text);
        if matches!(n, "," | ";" | ")" | "." | ".." | "%" | "@" | ">>") {
            return false;
        }
        if matches!(p, "(" | "." | ".." | "%" | "@" | "<<") {
            return false;
        }
        if n == "(" {
            let after_name_word = self.prev2.is_some_and(|w| {
                w.kind.is_word()
                    && NAME_BEFORE_COLUMNS
                        .iter()
                        .any(|k| w.text.eq_ignore_ascii_case(k))
            });
            return match prev.kind {
                TokenKind::Ident | TokenKind::QuotedIdent if after_name_word => true,
                TokenKind::Ident if p.eq_ignore_ascii_case("KEEP") => true,
                TokenKind::Ident
                | TokenKind::QuotedIdent
                | TokenKind::BindVar
                | TokenKind::SubstVar => false,
                TokenKind::Keyword => SPACED_KEYWORDS.iter().any(|k| p.eq_ignore_ascii_case(k)),
                _ => true,
            };
        }
        true
    }

    fn semi(&mut self) -> Doc {
        if self.text_is(";") {
            self.take()
        } else {
            Doc::Nil
        }
    }

    fn close(&mut self) -> Doc {
        if self.text_is(")") {
            self.take()
        } else {
            Doc::Nil
        }
    }

    /// Everything that's left, with normalized spacing.
    pub fn rest(&mut self) -> Doc {
        let mut out = Vec::new();
        while !self.done() {
            let before = self.pos;
            if self.text_is(")") || self.bail {
                out.push(self.next());
            } else {
                out.push(self.atom());
            }
            if self.pos == before {
                out.push(self.next());
            }
        }
        out.push(self.leading_comments());
        cat(out)
    }

    // ----- expressions -----------------------------------------------------

    fn atom(&mut self) -> Doc {
        if self.text_is("(") {
            self.paren(None)
        } else if self.is("CASE") {
            self.case_expr()
        } else {
            self.next()
        }
    }

    /// Tokens up to `stop` or an unmatched `)`.
    fn expr(&mut self, stop: Stop<'_, 'a, 's>) -> Doc {
        let mut out = Vec::new();
        while let Some(t) = self.peek() {
            if t.text == ")" || stop(self) {
                break;
            }
            out.push(self.atom());
        }
        cat(out)
    }

    /// Like [`Self::expr`], with a break opportunity before top-level AND/OR.
    fn cond(&mut self, stop: Stop<'_, 'a, 's>) -> Doc {
        let mut out = Vec::new();
        let mut between = false;
        while let Some(t) = self.peek() {
            if t.text == ")" || stop(self) {
                break;
            }
            if self.is("AND") || self.is("OR") {
                if self.is("AND") && between {
                    between = false;
                } else {
                    out.push(Doc::Line);
                    out.push(self.take());
                    continue;
                }
            }
            if self.is("BETWEEN") {
                between = true;
            }
            out.push(self.atom());
        }
        cat(out)
    }

    /// A list comma with the break after (or before) it; `Always` breaks.
    fn comma_wrapped(&mut self, wrap: Wrap) -> Doc {
        let c = self.take();
        let always = wrap == Wrap::Always;
        match self.o.comma {
            CommaStyle::Trailing => cat(vec![c, if always { Doc::HardLine } else { Doc::Line }]),
            CommaStyle::Leading => cat(vec![
                if always { Doc::HardLine } else { Doc::SoftLine },
                c,
                Doc::Space,
            ]),
        }
    }

    /// A parenthesized group: a subquery or a comma-separated list.
    fn paren(&mut self, space: Option<bool>) -> Doc {
        self.paren_wrapped(space, Wrap::Fit)
    }

    /// [`Layout::paren`] whose list is laid out as `wrap` says (subqueries
    /// follow the subquery option).
    fn paren_wrapped(&mut self, space: Option<bool>, wrap: Wrap) -> Doc {
        let space = match space {
            Some(s) => s,
            None => self.peek().is_some_and(|t| self.needs_space(&t)),
        };
        let open = self.take();
        let open = if space {
            cat(vec![Doc::Space, open])
        } else {
            open
        };
        if !self.enter() {
            self.leave();
            return cat(vec![open, self.rest()]);
        }
        let doc = if self.text_is(")") {
            cat(vec![open, self.take()])
        } else if self.is("SELECT") || self.is("WITH") {
            let saved = self.ctx;
            self.ctx = Ctx::Query;
            let q = self.query();
            self.ctx = saved;
            let close = self.close();
            let line = self.line_for(self.o.subqueries);
            group(cat(vec![
                open,
                nest(self.o.indent, cat(vec![line.clone(), q])),
                line,
                close,
            ]))
        } else {
            let mut items = Vec::new();
            loop {
                items.push(self.cond(&|s| s.text_is(",")));
                if self.text_is(",") {
                    items.push(self.comma_wrapped(wrap));
                } else {
                    break;
                }
            }
            let close = self.close();
            let line = self.line_for(wrap);
            group(cat(vec![
                open,
                nest(self.o.indent, cat(vec![line.clone(), cat(items)])),
                line,
                close,
            ]))
        };
        self.leave();
        doc
    }

    /// The line break before a broken group's items: soft when it may stay
    /// on one line, hard when it always breaks.
    fn line_for(&self, wrap: Wrap) -> Doc {
        match wrap {
            Wrap::Fit => Doc::SoftLine,
            Wrap::Always => Doc::HardLine,
        }
    }

    fn case_expr(&mut self) -> Doc {
        let case_kw = self.next();
        if !self.enter() {
            self.leave();
            return cat(vec![case_kw, self.rest()]);
        }
        let arm_end = |s: &Self| s.is("WHEN") || s.is("ELSE") || s.is("END");
        let selector = self.expr(&arm_end);
        let mut arms = Vec::new();
        while self.is("WHEN") {
            let when = self.take();
            let c = self.cond(&|s: &Self| s.is("THEN") || arm_end(s));
            let then = if self.is("THEN") {
                self.next()
            } else {
                Doc::Nil
            };
            let value = self.expr(&arm_end);
            arms.push(cat(vec![self.arm_line(), when, c, then, value]));
        }
        if self.is("ELSE") {
            let e = self.take();
            let value = self.expr(&arm_end);
            arms.push(cat(vec![self.arm_line(), e, value]));
        }
        let end = if self.is("END") {
            cat(vec![self.arm_line(), self.take()])
        } else {
            Doc::Nil
        };
        self.leave();
        group(cat(vec![
            case_kw,
            selector,
            nest(self.o.indent, cat(arms)),
            end,
        ]))
    }

    fn arm_line(&self) -> Doc {
        match self.o.case_arms {
            Wrap::Fit => Doc::Line,
            Wrap::Always => Doc::HardLine,
        }
    }

    /// The break before `THEN` / `LOOP` of a block header.
    fn header_line(&self) -> Doc {
        if self.o.then_on_new_line {
            Doc::HardLine
        } else {
            Doc::Line
        }
    }

    // ----- statements ------------------------------------------------------

    /// Formats a whole top-level statement.
    pub fn statement(&mut self) -> Doc {
        let doc = if self.is("CREATE") {
            self.create()
        } else if self.is("DECLARE") || self.is("BEGIN") || self.text_is("<<") {
            self.block()
        } else if self.sql_start() {
            self.sql()
        } else {
            group(self.expr(&|_| false))
        };
        cat(vec![doc, self.rest()])
    }

    fn sql_start(&self) -> bool {
        self.is_any(&["SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE"]) || self.text_is("(")
    }

    fn sql(&mut self) -> Doc {
        let saved = self.ctx;
        let doc = if self.is("INSERT") {
            self.insert()
        } else if self.is("UPDATE") {
            self.dml(Ctx::Update, &["SET"])
        } else if self.is("DELETE") {
            self.dml(Ctx::Delete, &["WHERE", "RETURNING", "RETURN", "LOG"])
        } else if self.is("MERGE") {
            self.dml(Ctx::Merge, &["USING"])
        } else {
            self.ctx = Ctx::Query;
            self.query()
        };
        self.ctx = saved;
        doc
    }

    fn query_stop(&self) -> bool {
        self.peek().is_none_or(|t| t.text == ")" || t.text == ";")
    }

    fn boundary(&self) -> bool {
        self.query_stop() || self.clause_kind().is_some()
    }

    fn clause_kind(&self) -> Option<Clause> {
        let c = |kind, words| Some(Clause { kind, words });
        let ctx = self.ctx;
        let returning = || {
            if self.is_any(&["RETURNING", "RETURN", "INTO"]) {
                c(Kind::List, 1)
            } else if self.words(&["LOG", "ERRORS"]) {
                c(Kind::Expr, 2)
            } else {
                None
            }
        };
        match ctx {
            Ctx::Query => {
                if self.is("SELECT") {
                    c(Kind::Select, 1)
                } else if self.is("FROM") {
                    c(Kind::List, 1)
                } else if self.is("WHERE") || self.is("HAVING") {
                    c(Kind::Cond, 1)
                } else if self.words(&["GROUP", "BY"]) || self.words(&["ORDER", "BY"]) {
                    c(Kind::List, 2)
                } else if self.words(&["ORDER", "SIBLINGS", "BY"]) {
                    c(Kind::List, 3)
                } else if self.words(&["CONNECT", "BY", "NOCYCLE"]) {
                    c(Kind::Cond, 3)
                } else if self.words(&["CONNECT", "BY"]) || self.words(&["START", "WITH"]) {
                    c(Kind::Cond, 2)
                } else if self.is("INTO") {
                    c(Kind::List, 1)
                } else if self.words(&["BULK", "COLLECT", "INTO"]) {
                    c(Kind::List, 3)
                } else if self.words(&["UNION", "ALL"]) {
                    c(Kind::SetOp, 2)
                } else if self.is_any(&["UNION", "INTERSECT", "MINUS", "EXCEPT"]) {
                    c(Kind::SetOp, 1)
                } else if (self.is("FETCH")
                    && (self.word_at(1, "FIRST") || self.word_at(1, "NEXT")))
                    || (self.is("OFFSET") && (self.word_at(2, "ROW") || self.word_at(2, "ROWS")))
                    || self.words(&["FOR", "UPDATE"])
                    || (self.is("WINDOW") && self.word_at(2, "AS"))
                {
                    c(Kind::Expr, 1)
                } else {
                    self.join_words().map(|n| Clause {
                        kind: Kind::Join,
                        words: n,
                    })
                }
            }
            Ctx::Insert | Ctx::Delete => {
                if ctx == Ctx::Delete && self.is("WHERE") {
                    c(Kind::Cond, 1)
                } else {
                    returning()
                }
            }
            Ctx::Update => {
                if self.is("SET") {
                    c(Kind::List, 1)
                } else if self.is("WHERE") {
                    c(Kind::Cond, 1)
                } else {
                    returning()
                }
            }
            Ctx::Merge => {
                if self.is("USING") || self.is("ON") || self.is("VALUES") {
                    c(Kind::Expr, 1)
                } else if self.words(&["WHEN", "MATCHED", "THEN"]) {
                    c(Kind::MergeWhen, 3)
                } else if self.words(&["WHEN", "NOT", "MATCHED", "THEN"]) {
                    c(Kind::MergeWhen, 4)
                } else if self.words(&["UPDATE", "SET"]) {
                    c(Kind::List, 2)
                } else if self.is("INSERT") {
                    c(Kind::Expr, 1)
                } else if self.is("WHERE") {
                    c(Kind::Cond, 1)
                } else if self.words(&["DELETE", "WHERE"]) {
                    c(Kind::Cond, 2)
                } else {
                    returning()
                }
            }
        }
    }

    /// Number of words of a join operator at the cursor, if any.
    fn join_words(&self) -> Option<usize> {
        if self.words(&["CROSS", "APPLY"])
            || self.words(&["OUTER", "APPLY"])
            || self.words(&["CROSS", "JOIN"])
        {
            return Some(2);
        }
        let mut n = 0;
        if self.word_at(n, "NATURAL") {
            n += 1;
        }
        if ["INNER", "LEFT", "RIGHT", "FULL"]
            .iter()
            .any(|w| self.word_at(n, w))
        {
            n += 1;
            if self.word_at(n, "OUTER") {
                n += 1;
            }
        }
        if self.word_at(n, "JOIN") {
            return Some(n + 1);
        }
        if self.word_at(n, "PARTITION") {
            return None;
        }
        None
    }

    fn query(&mut self) -> Doc {
        if !self.enter() {
            self.leave();
            return self.rest();
        }
        let mut clauses: Vec<Doc> = Vec::new();
        let mut force = false;
        if self.is("WITH") {
            clauses.push(self.with_clause());
        }
        while !self.query_stop() {
            let before = self.pos;
            match self.clause_kind() {
                Some(c) => {
                    force |= c.kind == Kind::SetOp;
                    clauses.push(self.clause(c));
                }
                None => {
                    let d = self.atom();
                    match clauses.last_mut() {
                        Some(last) => *last = cat(vec![last.clone(), d]),
                        None => clauses.push(d),
                    }
                }
            }
            if self.pos == before {
                break;
            }
        }
        self.leave();
        join_clauses(clauses, force)
    }

    fn with_clause(&mut self) -> Doc {
        // A query's PL/SQL declarations (#358) aren't laid out as SQL: the
        // statement is kept as written.
        if (self.word_at(1, "FUNCTION") || self.word_at(1, "PROCEDURE"))
            && self.sig(2).is_some_and(|t| {
                (t.kind.is_word() && !t.text.eq_ignore_ascii_case("AS"))
                    || t.kind == TokenKind::QuotedIdent
            })
        {
            self.bail = true;
            return self.rest();
        }
        let kw = self.next();
        let mut items = Vec::new();
        loop {
            let item = self.expr(&|s| s.text_is(",") || s.is("SELECT") || s.query_stop());
            items.push(item);
            if self.text_is(",") {
                items.push(self.take());
                items.push(Doc::HardLine);
            } else {
                break;
            }
        }
        cat(vec![
            kw,
            nest(self.o.indent, cat(vec![Doc::HardLine, cat(items)])),
        ])
    }

    fn clause(&mut self, c: Clause) -> Doc {
        let mut head = Vec::new();
        for _ in 0..c.words {
            head.push(self.next());
        }
        match c.kind {
            Kind::Select => {
                while self.peek().is_some_and(|t| t.kind == TokenKind::Hint)
                    || self.is_any(&["DISTINCT", "UNIQUE", "ALL"])
                {
                    head.push(self.next());
                }
                self.list(cat(head), true)
            }
            Kind::List => self.list(cat(head), false),
            Kind::Cond => {
                // Broken: the condition goes below the keyword, one AND/OR
                // operand per line.
                let body = self.cond(&|s| s.boundary());
                group(cat(vec![
                    cat(head),
                    nest(self.o.indent, cat(vec![Doc::Line, body])),
                ]))
            }
            Kind::Join => {
                let table = self.expr(&|s| s.is("ON") || s.is("USING") || s.boundary());
                let mut parts = vec![cat(head), table];
                if self.is("ON") || self.is("USING") {
                    let on = self.take();
                    let body = self.cond(&|s| s.boundary());
                    let body = nest(self.o.indent, cat(vec![on, body]));
                    parts.push(if self.o.on_new_line {
                        nest(self.o.indent, cat(vec![Doc::HardLine, body]))
                    } else {
                        group(nest(self.o.indent, cat(vec![Doc::Line, body])))
                    });
                }
                cat(parts)
            }
            Kind::Expr | Kind::SetOp | Kind::MergeWhen => {
                let body = self.expr(&|s| s.boundary());
                group(cat(vec![cat(head), body]))
            }
        }
    }

    /// `head item, item, …`: one line if it fits, else one item per line.
    fn list(&mut self, head: Doc, select: bool) -> Doc {
        let mut items: Vec<(Doc, Doc)> = Vec::new();
        let mut commas: Vec<Doc> = Vec::new();
        while !self.boundary() {
            let item = if select {
                self.select_item()
            } else {
                (self.expr(&|s| s.text_is(",") || s.boundary()), Doc::Nil)
            };
            items.push(item);
            if self.text_is(",") {
                commas.push(self.take());
            } else {
                break;
            }
        }
        if items.is_empty() {
            return cat(vec![head, cat(commas)]);
        }
        if items.len() == 1 && commas.is_empty() {
            // A single item breaks on its own (e.g. a subquery in FROM).
            let (e, alias) = items.pop().unwrap_or((Doc::Nil, Doc::Nil));
            return group(cat(vec![head, e, alias]));
        }
        let n = items.len();
        let mut flat = Vec::new();
        let mut table = Vec::new();
        for (i, (e, alias)) in items.into_iter().enumerate() {
            let comma = commas.get(i).cloned().unwrap_or(Doc::Nil);
            flat.extend([e.clone(), alias.clone(), comma.clone()]);
            let (lead, trail) = match self.o.comma {
                CommaStyle::Trailing => (Doc::Nil, comma),
                CommaStyle::Leading => {
                    let lead = match i.checked_sub(1).and_then(|p| commas.get(p)) {
                        Some(c) => cat(vec![c.clone(), Doc::Space]),
                        None => Doc::Nil,
                    };
                    // A dangling comma after the last item stays with it.
                    (lead, if i + 1 == n { comma } else { Doc::Nil })
                }
            };
            let cells = match alias {
                Doc::Nil => vec![cat(vec![lead, e, trail])],
                alias => vec![cat(vec![lead, e]), cat(vec![alias, trail])],
            };
            table.push(Row {
                prefix: Doc::Nil,
                cells,
            });
        }
        let broken = nest(
            self.o.indent,
            cat(vec![Doc::HardLine, rows(table, self.o.align_aliases)]),
        );
        group(cat(vec![head, if_break(broken, cat(flat))]))
    }

    /// One select-list item, split into expression and `AS alias`.
    fn select_item(&mut self) -> (Doc, Doc) {
        let mut parts = Vec::new();
        let mut alias = Doc::Nil;
        while let Some(t) = self.peek() {
            if t.text == ")" || t.text == "," || self.boundary() {
                break;
            }
            if self.is("AS") && self.alias_follows() {
                let as_kw = self.next();
                let name = self.next();
                alias = cat(vec![as_kw, name]);
                continue;
            }
            parts.push(self.atom());
        }
        (cat(parts), alias)
    }

    fn alias_follows(&self) -> bool {
        let name_ok = self.sig(1).is_some_and(|t| {
            matches!(
                t.kind,
                TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword
            )
        });
        let after = self.sig(2);
        name_ok
            && after.is_none_or(|t| {
                matches!(t.text, "," | ")" | ";")
                    || ["FROM", "INTO", "BULK"]
                        .iter()
                        .any(|w| t.kind.is_word() && t.text.eq_ignore_ascii_case(w))
            })
    }

    fn insert(&mut self) -> Doc {
        self.ctx = Ctx::Insert;
        let mut head = vec![self.next()];
        while self.peek().is_some_and(|t| t.kind == TokenKind::Hint) {
            head.push(self.next());
        }
        if !self.is("INTO") {
            // Multi-table inserts: keep tokens, normalized.
            return cat(vec![cat(head), self.expr(&|_| false)]);
        }
        head.push(self.next());
        head.push(self.expr(&|s| {
            s.text_is("(")
                || s.is("VALUES")
                || s.is("SELECT")
                || s.is("WITH")
                || s.clause_kind().is_some()
        }));
        if self.text_is("(") && !(self.word_at(1, "SELECT") || self.word_at(1, "WITH")) {
            head.push(self.paren_wrapped(Some(true), self.o.insert_lists));
        }
        let mut clauses = vec![group(cat(head))];
        if self.is("VALUES") {
            let kw = self.next();
            let mut values = Vec::new();
            if self.text_is("(") {
                values.push(self.paren_wrapped(Some(true), self.o.insert_lists));
            }
            values.push(self.expr(&|s| s.boundary()));
            clauses.push(cat(vec![kw, cat(values)]));
        } else if self.is("SELECT") || self.is("WITH") || self.text_is("(") {
            self.ctx = Ctx::Query;
            clauses.push(self.query());
            self.ctx = Ctx::Insert;
        }
        self.tail(clauses)
    }

    fn dml(&mut self, ctx: Ctx, head_end: &[&str]) -> Doc {
        self.ctx = ctx;
        let mut head = vec![self.next()];
        let head_end: Vec<String> = head_end.iter().map(|s| s.to_string()).collect();
        head.push(self.expr(&|s| head_end.iter().any(|w| s.is(w)) || s.query_stop()));
        self.tail(vec![group(cat(head))])
    }

    /// Remaining DML clauses after the head.
    fn tail(&mut self, initial: Vec<Doc>) -> Doc {
        let mut clauses: Vec<(Doc, bool)> = initial.into_iter().map(|d| (d, false)).collect();
        let mut nested = false;
        while !self.query_stop() {
            let before = self.pos;
            match self.clause_kind() {
                Some(c) => {
                    let when = c.kind == Kind::MergeWhen;
                    let d = self.clause(c);
                    clauses.push((d, nested && !when));
                    nested |= when;
                }
                None => {
                    let d = self.atom();
                    match clauses.last_mut() {
                        Some(last) => last.0 = cat(vec![last.0.clone(), d]),
                        None => clauses.push((d, false)),
                    }
                }
            }
            if self.pos == before {
                break;
            }
        }
        // MERGE branches nest, so every clause goes on its own line there.
        let hard = self.ctx == Ctx::Merge || clauses.len() > 2;
        let mut parts = Vec::new();
        for (i, (d, nested)) in clauses.into_iter().enumerate() {
            let line = if i == 0 {
                Doc::Nil
            } else if hard {
                Doc::HardLine
            } else {
                Doc::Line
            };
            parts.push(if nested {
                nest(self.o.indent, cat(vec![line, d]))
            } else {
                cat(vec![line, d])
            });
        }
        group(cat(parts))
    }

    // ----- CREATE ----------------------------------------------------------

    fn create(&mut self) -> Doc {
        let kinds = [
            "PROCEDURE",
            "FUNCTION",
            "PACKAGE",
            "TYPE",
            "TRIGGER",
            "TABLE",
            "VIEW",
            "JAVA",
            "LIBRARY",
            "INDEX",
            "SEQUENCE",
            "SYNONYM",
            "USER",
            "ROLE",
        ];
        let mut n = 1;
        while n < 10 && !kinds.iter().any(|k| self.word_at(n, k)) {
            if self.word_at(n, "WRAPPED") || self.sig(n).is_none() {
                break;
            }
            n += 1;
        }
        if (0..14).any(|i| self.word_at(i, "WRAPPED")) {
            self.bail = true;
            self.opaque = true;
            return self.rest();
        }
        let kind = self
            .sig(n)
            .map(|t| t.text.to_ascii_uppercase())
            .unwrap_or_default();
        let mut head = Vec::new();
        match kind.as_str() {
            "PROCEDURE" | "FUNCTION" => {
                for _ in 0..n {
                    head.push(self.next());
                }
                let sub = self.subprogram();
                cat(vec![cat(head), sub])
            }
            "PACKAGE" => {
                for _ in 0..n {
                    head.push(self.next());
                }
                self.package(cat(head))
            }
            "TYPE" => {
                for _ in 0..n {
                    head.push(self.next());
                }
                if self.word_at(1, "BODY") {
                    self.package(cat(head))
                } else {
                    let spec = self.expr(&|s| s.text_is(";"));
                    cat(vec![cat(head), spec])
                }
            }
            "TRIGGER" => {
                for _ in 0..n {
                    head.push(self.next());
                }
                self.trigger(cat(head))
            }
            "TABLE" => self.create_table(),
            "VIEW" => self.create_view(),
            "JAVA" | "LIBRARY" => {
                self.bail = true;
                self.opaque = true;
                self.rest()
            }
            _ => group(self.expr(&|_| false)),
        }
    }

    fn create_table(&mut self) -> Doc {
        let head = self.expr(&|s| s.text_is("(") || s.is("AS"));
        let mut parts = vec![head];
        if self.text_is("(") {
            parts.push(self.columns());
        }
        if self.is("AS") && (self.word_at(1, "SELECT") || self.word_at(1, "WITH")) {
            parts.push(self.next());
            self.ctx = Ctx::Query;
            parts.push(nest(0, cat(vec![Doc::HardLine, self.query()])));
        }
        parts.push(self.expr(&|_| false));
        cat(parts)
    }

    /// `( column type ..., constraint ... )` of CREATE TABLE.
    fn columns(&mut self) -> Doc {
        let open = self.paren_open(true);
        if !self.enter() {
            self.leave();
            return cat(vec![open, self.rest()]);
        }
        let mut table = Vec::new();
        let constraint_words = [
            "CONSTRAINT",
            "PRIMARY",
            "UNIQUE",
            "FOREIGN",
            "CHECK",
            "SUPPLEMENTAL",
            "PERIOD",
        ];
        while !self.done() && !self.text_is(")") {
            let before = self.pos;
            let prefix = self.leading_comments();
            let column = self
                .peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent))
                && !self.is_any(&constraint_words);
            let cells = if column {
                let name = self.take_first();
                let rest = self.expr(&|s| s.text_is(","));
                let comma = if self.text_is(",") {
                    self.take()
                } else {
                    Doc::Nil
                };
                vec![name, cat(vec![rest, comma])]
            } else {
                let item = self.cond(&|s| s.text_is(","));
                let comma = if self.text_is(",") {
                    self.take()
                } else {
                    Doc::Nil
                };
                vec![cat(vec![item, comma])]
            };
            table.push(Row { prefix, cells });
            if self.pos == before {
                break;
            }
        }
        let close = self.close();
        self.leave();
        cat(vec![
            open,
            nest(
                self.o.indent,
                cat(vec![Doc::HardLine, rows(table, self.o.align_declarations)]),
            ),
            Doc::HardLine,
            close,
        ])
    }

    fn create_view(&mut self) -> Doc {
        let mut head = Vec::new();
        while !self.done() && !self.is("AS") {
            let before = self.pos;
            head.push(if self.text_is("(") {
                self.paren(Some(true))
            } else {
                self.next()
            });
            if self.pos == before {
                break;
            }
        }
        let mut parts = vec![group(cat(head))];
        if self.is("AS") {
            parts.push(self.next());
            self.ctx = Ctx::Query;
            parts.push(Doc::HardLine);
            parts.push(self.query());
        }
        cat(parts)
    }

    /// `(` with an explicit spacing decision.
    fn paren_open(&mut self, space: bool) -> Doc {
        let open = self.take();
        if space {
            cat(vec![Doc::Space, open])
        } else {
            open
        }
    }

    /// First token of a line: no leading space.
    fn take_first(&mut self) -> Doc {
        self.take()
    }

    // ----- PL/SQL ----------------------------------------------------------

    fn package(&mut self, head: Doc) -> Doc {
        let header = self.expr(&|s| s.is("IS") || s.is("AS") || s.text_is(";"));
        let mut parts = vec![group(cat(vec![head, header]))];
        if self.is("IS") || self.is("AS") {
            parts.push(self.next());
            let decls = self.decls(&["BEGIN", "END"]);
            parts.push(nest(self.o.indent, decls));
            if self.is("BEGIN") {
                parts.push(self.sep());
                parts.push(self.body());
            } else {
                parts.push(self.end_clause(&[]));
            }
        }
        cat(parts)
    }

    fn trigger(&mut self, head: Doc) -> Doc {
        let stop =
            |s: &Self| s.is_any(&["DECLARE", "BEGIN", "COMPOUND", "CALL"]) || s.text_is("<<");
        let name = self.expr(&|s| stop(s) || s.is_any(TRIGGER_CLAUSES));
        let mut parts = vec![head, name];
        while !self.done() && !stop(self) {
            let before = self.pos;
            if self.is_any(TRIGGER_CLAUSES) {
                let kw = self.take();
                let body = self.expr(&|s| stop(s) || s.is_any(TRIGGER_CLAUSES));
                parts.push(nest(self.o.indent, cat(vec![Doc::HardLine, kw, body])));
            } else {
                parts.push(self.atom());
            }
            if self.pos == before {
                break;
            }
        }
        if self.is("COMPOUND") {
            self.bail = true;
            return cat(vec![cat(parts), self.rest()]);
        }
        if !self.done() {
            parts.push(Doc::HardLine);
            parts.push(self.block());
        }
        cat(parts)
    }

    fn block(&mut self) -> Doc {
        if !self.enter() {
            self.leave();
            return self.rest();
        }
        let mut parts = Vec::new();
        if self.text_is("<<") {
            parts.push(self.label());
            parts.push(Doc::HardLine);
        }
        if self.is("DECLARE") {
            parts.push(self.take());
            parts.push(nest(self.o.indent, self.decls(&["BEGIN"])));
            parts.push(self.sep());
        }
        if self.is("BEGIN") {
            parts.push(self.body());
        }
        self.leave();
        cat(parts)
    }

    fn label(&mut self) -> Doc {
        let mut parts = vec![self.take()];
        while !self.done() {
            let close = self.text_is(">>");
            parts.push(self.next());
            if close {
                break;
            }
        }
        cat(parts)
    }

    /// `BEGIN statements [EXCEPTION handlers] END [label];`
    fn body(&mut self) -> Doc {
        let begin = self.take();
        let stmts = self.stmts(&["EXCEPTION", "END"]);
        let mut parts = vec![begin, nest(self.o.indent, stmts)];
        if self.is("EXCEPTION") {
            parts.push(self.sep());
            parts.push(self.take());
            parts.push(nest(self.o.indent, self.handlers()));
        }
        parts.push(self.end_clause(&[]));
        cat(parts)
    }

    fn handlers(&mut self) -> Doc {
        let mut parts = Vec::new();
        while self.is("WHEN") {
            parts.push(self.sep());
            parts.push(self.take());
            parts.push(self.expr(&|s| s.is("THEN") || s.text_is(";")));
            if self.is("THEN") {
                parts.push(self.next());
            }
            parts.push(nest(self.o.indent, self.stmts(&["WHEN", "END"])));
        }
        cat(parts)
    }

    /// `END [IF|LOOP|CASE] [label];` on its own line.
    fn end_clause(&mut self, kinds: &[&str]) -> Doc {
        if !self.is("END") {
            return Doc::Nil;
        }
        let mut parts = vec![self.sep(), self.take()];
        for k in kinds {
            if self.is(k) {
                parts.push(self.next());
            }
        }
        if self
            .peek()
            .is_some_and(|t| t.kind.is_word() || t.kind == TokenKind::QuotedIdent)
            && (self.text_at(1, ";") || self.sig(1).is_none())
        {
            parts.push(self.next());
        }
        parts.push(self.semi());
        cat(parts)
    }

    /// Statements up to one of `stop`, each on its own line.
    fn stmts(&mut self, stop: &[&str]) -> Doc {
        let mut items: Vec<(Doc, Doc, Item)> = Vec::new();
        while !self.done() && !self.is_any(stop) {
            let before = self.pos;
            let sep = if items.is_empty() {
                Doc::HardLine
            } else {
                self.sep()
            };
            let prefix = self.leading_comments();
            let item = if self.is_assignment() {
                let lhs = self.expr(&|s| s.text_is(":="));
                let op = self.next();
                let rhs = self.expr(&|s| s.text_is(";"));
                let semi = self.semi();
                Item::Simple {
                    name: lhs,
                    rest: cat(vec![op, rhs, semi]),
                }
            } else {
                Item::Other(self.pl_statement())
            };
            let item = if self.pos == before {
                Item::Other(self.next())
            } else {
                item
            };
            items.push((sep, prefix, item));
        }
        assemble(items, self.o.align_assignments)
    }

    fn is_assignment(&self) -> bool {
        let Some(first) = self.peek() else {
            return false;
        };
        if !matches!(
            first.kind,
            TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::BindVar
        ) {
            return false;
        }
        for i in 1..16 {
            let Some(t) = self.sig(i) else { return false };
            match t.kind {
                TokenKind::Operator if t.text == ":=" => return true,
                TokenKind::Ident
                | TokenKind::QuotedIdent
                | TokenKind::BindVar
                | TokenKind::Dot
                | TokenKind::LParen
                | TokenKind::RParen
                | TokenKind::Number
                | TokenKind::Comma => {}
                _ => return false,
            }
        }
        false
    }

    fn pl_statement(&mut self) -> Doc {
        if self.is("IF") {
            self.if_stmt()
        } else if self.is("LOOP")
            || self.is("WHILE")
            || (self.is("FOR") && !self.word_at(1, "UPDATE"))
        {
            self.loop_stmt()
        } else if self.is("FORALL") {
            self.forall()
        } else if self.is("CASE") {
            self.case_stmt()
        } else if self.is("BEGIN") || self.is("DECLARE") || self.text_is("<<") {
            self.block()
        } else if self.sql_start() {
            let q = self.sql();
            cat(vec![q, self.semi()])
        } else if self.is("OPEN") && (2..5).any(|i| self.word_at(i, "FOR")) {
            let head = self.expr(&|s| s.is("FOR"));
            let kw = self.next();
            let body = if self.is("SELECT") || self.is("WITH") {
                self.ctx = Ctx::Query;
                nest(self.o.indent, cat(vec![Doc::HardLine, self.query()]))
            } else {
                self.expr(&|s| s.text_is(";"))
            };
            cat(vec![
                head,
                kw,
                body,
                self.expr(&|s| s.text_is(";")),
                self.semi(),
            ])
        } else {
            let body = self.expr(&|s| s.text_is(";"));
            cat(vec![group(body), self.semi()])
        }
    }

    fn if_stmt(&mut self) -> Doc {
        if !self.enter() {
            self.leave();
            return self.rest();
        }
        let mut parts = Vec::new();
        loop {
            // IF / ELSIF condition THEN
            let kw = self.take();
            let c = self.cond(&|s| s.is("THEN") || s.text_is(";"));
            let then = if self.is("THEN") {
                cat(vec![self.header_line(), self.take()])
            } else {
                Doc::Nil
            };
            parts.push(group(cat(vec![kw, nest(self.o.indent, c), then])));
            parts.push(nest(self.o.indent, self.stmts(&["ELSIF", "ELSE", "END"])));
            if self.is("ELSIF") {
                parts.push(self.sep());
                continue;
            }
            break;
        }
        if self.is("ELSE") {
            parts.push(self.sep());
            parts.push(self.take());
            parts.push(nest(self.o.indent, self.stmts(&["END"])));
        }
        parts.push(self.end_clause(&["IF"]));
        self.leave();
        cat(parts)
    }

    fn loop_stmt(&mut self) -> Doc {
        if !self.enter() {
            self.leave();
            return self.rest();
        }
        let mut parts = Vec::new();
        if self.is("FOR") {
            // `for x in (subquery) loop`: the subquery indents itself.
            let kw = self.take();
            let header = self.expr(&|s| s.is("LOOP") || s.text_is(";"));
            let lp = if !self.is("LOOP") {
                Doc::Nil
            } else if self.o.then_on_new_line {
                cat(vec![Doc::HardLine, self.take()])
            } else {
                self.next()
            };
            parts.push(group(cat(vec![kw, header, lp])));
        } else if !self.is("LOOP") {
            let kw = self.take();
            let header = self.cond(&|s| s.is("LOOP") || s.text_is(";"));
            let lp = if self.is("LOOP") {
                cat(vec![self.header_line(), self.take()])
            } else {
                Doc::Nil
            };
            parts.push(group(cat(vec![kw, nest(self.o.indent, header), lp])));
        } else {
            parts.push(self.take());
        }
        parts.push(nest(self.o.indent, self.stmts(&["END"])));
        parts.push(self.end_clause(&["LOOP"]));
        self.leave();
        cat(parts)
    }

    fn forall(&mut self) -> Doc {
        let kw = self.take();
        let header = self.expr(&|s| {
            s.is_any(&["INSERT", "UPDATE", "DELETE", "MERGE", "EXECUTE"]) || s.text_is(";")
        });
        let body = if self.sql_start() {
            let q = self.sql();
            nest(self.o.indent, cat(vec![Doc::HardLine, q]))
        } else {
            self.expr(&|s| s.text_is(";"))
        };
        cat(vec![
            group(cat(vec![kw, nest(self.o.indent, header)])),
            body,
            self.semi(),
        ])
    }

    fn case_stmt(&mut self) -> Doc {
        if !self.enter() {
            self.leave();
            return self.rest();
        }
        let kw = self.take();
        let selector = self.expr(&|s| s.is("WHEN") || s.text_is(";"));
        let mut arms = Vec::new();
        while self.is("WHEN") {
            arms.push(self.sep());
            arms.push(self.take());
            arms.push(self.cond(&|s| s.is("THEN") || s.text_is(";")));
            if self.is("THEN") {
                arms.push(self.next());
            }
            arms.push(nest(self.o.indent, self.stmts(&["WHEN", "ELSE", "END"])));
        }
        if self.is("ELSE") {
            arms.push(self.sep());
            arms.push(self.take());
            arms.push(nest(self.o.indent, self.stmts(&["END"])));
        }
        let end = self.end_clause(&["CASE"]);
        self.leave();
        cat(vec![kw, selector, nest(self.o.indent, cat(arms)), end])
    }

    /// Declarations up to one of `stop`.
    fn decls(&mut self, stop: &[&str]) -> Doc {
        let mut items: Vec<(Doc, Doc, Item)> = Vec::new();
        // Subprogram bodies get a blank line before and after them.
        let mut prev_body = false;
        while !self.done() && !self.is_any(stop) {
            let before = self.pos;
            let mut sep = self.sep();
            let prefix = self.leading_comments();
            let mut body = false;
            let item = if self.subprogram_start() {
                self.had_body = false;
                let doc = self.subprogram();
                body = self.had_body;
                Item::Other(doc)
            } else if self.is("CURSOR") {
                Item::Other(self.cursor())
            } else if self.is("TYPE") && (2..5).any(|i| self.word_at(i, "RECORD")) {
                let head = self.expr(&|s| s.text_is("(") || s.text_is(";"));
                let fields = if self.text_is("(") {
                    self.params()
                } else {
                    Doc::Nil
                };
                let rest = self.expr(&|s| s.text_is(";"));
                Item::Other(cat(vec![head, fields, rest, self.semi()]))
            } else if self
                .peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent))
            {
                let name = self.take();
                let rest = self.expr(&|s| s.text_is(";"));
                Item::Simple {
                    name,
                    rest: cat(vec![rest, self.semi()]),
                }
            } else {
                let body = self.expr(&|s| s.text_is(";"));
                Item::Other(cat(vec![group(body), self.semi()]))
            };
            let item = if self.pos == before {
                Item::Other(self.next())
            } else {
                item
            };
            if !items.is_empty() && self.o.blank_lines > 0 && (body || prev_body) {
                sep = Doc::Blank;
            }
            prev_body = body;
            items.push((sep, prefix, item));
        }
        assemble(items, self.o.align_declarations)
    }

    fn subprogram_start(&self) -> bool {
        for i in 0..5 {
            if self.word_at(i, "PROCEDURE") || self.word_at(i, "FUNCTION") {
                return true;
            }
            if !SUBPROGRAM_PREFIXES.iter().any(|w| self.word_at(i, w)) {
                return false;
            }
        }
        false
    }

    fn subprogram(&mut self) -> Doc {
        if !self.enter() {
            self.leave();
            return self.rest();
        }
        let mut head = Vec::new();
        while !self.done() {
            let kind = self.is("PROCEDURE") || self.is("FUNCTION");
            head.push(self.next());
            if kind {
                break;
            }
        }
        let body_start = |s: &Self| s.is("IS") || (s.is("AS") && !s.word_at(1, "RESULT"));
        head.push(
            self.expr(&|s| s.text_is("(") || s.is("RETURN") || body_start(s) || s.text_is(";")),
        );
        if self.text_is("(") {
            head.push(self.params());
        }
        head.push(self.expr(&|s| body_start(s) || s.text_is(";")));
        let mut parts = vec![group(cat(head))];
        let has_body = body_start(self);
        if has_body {
            parts.push(self.next());
            if self.is_any(&["LANGUAGE", "EXTERNAL"]) {
                parts.push(nest(
                    self.o.indent,
                    cat(vec![Doc::HardLine, self.expr(&|s| s.text_is(";"))]),
                ));
                parts.push(self.semi());
            } else {
                parts.push(nest(self.o.indent, self.decls(&["BEGIN", "END"])));
                if self.is("BEGIN") {
                    parts.push(self.sep());
                    parts.push(self.body());
                } else {
                    parts.push(self.end_clause(&[]));
                }
            }
        } else {
            parts.push(self.semi());
        }
        self.had_body = has_body;
        self.leave();
        cat(parts)
    }

    /// `(name [IN|OUT|NOCOPY] type [default], ...)`, aligned when broken.
    fn params(&mut self) -> Doc {
        let space = self.peek().is_some_and(|t| self.needs_space(&t));
        let open = self.paren_open(space);
        if !self.enter() {
            self.leave();
            return cat(vec![open, self.rest()]);
        }
        let mut table = Vec::new();
        let mut flat = Vec::new();
        while !self.done() && !self.text_is(")") {
            let before = self.pos;
            let prefix = self.leading_comments();
            let name = self.next();
            let mut mode = Vec::new();
            while self.is_any(&["IN", "OUT", "NOCOPY"]) {
                mode.push(self.next());
            }
            let rest = self.expr(&|s| s.text_is(","));
            let comma = if self.text_is(",") {
                self.take()
            } else {
                Doc::Nil
            };
            let mode = cat(mode);
            flat.extend([
                prefix.clone(),
                name.clone(),
                mode.clone(),
                rest.clone(),
                comma.clone(),
            ]);
            table.push(Row {
                prefix,
                cells: vec![name, mode, cat(vec![rest, comma])],
            });
            if self.pos == before {
                break;
            }
        }
        let close = self.close();
        self.leave();
        let broken = cat(vec![
            nest(
                self.o.indent,
                cat(vec![Doc::HardLine, rows(table, self.o.align_declarations)]),
            ),
            Doc::HardLine,
        ]);
        match self.o.parameters {
            Wrap::Fit => group(cat(vec![open, if_break(broken, cat(flat)), close])),
            Wrap::Always => cat(vec![open, broken, close]),
        }
    }

    fn cursor(&mut self) -> Doc {
        let mut head = vec![self.take()];
        head.push(self.expr(&|s| s.text_is("(") || s.is_any(&["IS", "RETURN"]) || s.text_is(";")));
        if self.text_is("(") {
            head.push(self.params());
        }
        head.push(self.expr(&|s| s.is("IS") || s.text_is(";")));
        let mut parts = vec![group(cat(head))];
        if self.is("IS") {
            parts.push(self.next());
            let q = if self.is("SELECT") || self.is("WITH") || self.text_is("(") {
                self.ctx = Ctx::Query;
                self.query()
            } else {
                self.expr(&|s| s.text_is(";"))
            };
            parts.push(nest(self.o.indent, cat(vec![Doc::HardLine, q])));
        }
        parts.push(self.expr(&|s| s.text_is(";")));
        parts.push(self.semi());
        cat(parts)
    }
}

/// Lines of a statement or declaration list; runs of [`Item::Simple`] are
/// aligned.
fn assemble(items: Vec<(Doc, Doc, Item)>, align: bool) -> Doc {
    let mut out = Vec::new();
    let mut run: Vec<Row> = Vec::new();
    let mut run_sep = Doc::Nil;
    let flush = |out: &mut Vec<Doc>, run: &mut Vec<Row>, run_sep: &mut Doc| {
        if !run.is_empty() {
            out.push(std::mem::replace(run_sep, Doc::Nil));
            out.push(rows(std::mem::take(run), align));
        }
    };
    for (sep, prefix, item) in items {
        match item {
            Item::Simple { name, rest } => {
                if matches!(sep, Doc::Blank) {
                    flush(&mut out, &mut run, &mut run_sep);
                }
                if run.is_empty() {
                    run_sep = sep;
                }
                run.push(Row {
                    prefix,
                    cells: vec![name, rest],
                });
            }
            Item::Other(doc) => {
                flush(&mut out, &mut run, &mut run_sep);
                out.push(sep);
                out.push(prefix);
                out.push(doc);
            }
        }
    }
    flush(&mut out, &mut run, &mut run_sep);
    cat(out)
}

fn join_clauses(clauses: Vec<Doc>, force: bool) -> Doc {
    let hard = force || clauses.len() > 2;
    let mut parts = Vec::new();
    for (i, c) in clauses.into_iter().enumerate() {
        if i > 0 {
            parts.push(if hard { Doc::HardLine } else { Doc::Line });
        }
        parts.push(c);
    }
    group(cat(parts))
}

/// `s` in `case`, its ASCII letters only: a database folds those alike
/// (PostgreSQL no others), and Oracle keeps `ß`, which `to_uppercase`
/// made `SS`, another name (#419).
fn apply_case(case: Case, s: &str) -> String {
    match case {
        Case::Upper => s.to_ascii_uppercase(),
        Case::Lower => s.to_ascii_lowercase(),
        Case::Preserve => s.to_owned(),
    }
}
