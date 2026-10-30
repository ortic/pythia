//! Error-tolerant recursive-descent parser for PL/SQL and SQL.
//!
//! This is not a validating grammar. It recognizes the structure that
//! navigation, outline and completion need — units, scopes, declarations,
//! query blocks with their sources — and recovers from anything it doesn't
//! understand by skipping to the next plausible boundary. It never panics and
//! always produces a tree, also for code that is being typed.

mod query;

use crate::ast::*;
use crate::lexer::{Token, TokenKind, tokenize};
use crate::splitter::{self, StatementKind as SplitKind};

/// Parses a script (one or more statements, SQL*Plus style).
pub fn parse(src: &str) -> Ast {
    parse_tokens(src, &tokenize(src))
}

/// Like [`parse`], reusing tokens from [`tokenize`].
pub fn parse_tokens(src: &str, tokens: &[Token]) -> Ast {
    let mut ast = Ast::default();
    ast.scopes.push(Scope {
        kind: ScopeKind::Root,
        name: None,
        owner: None,
        span: 0..src.len(),
        parent: None,
        children: Vec::new(),
        symbols: Vec::new(),
        decls: None,
        trigger_table: None,
    });
    lexer_diagnostics(src, tokens, &mut ast.diagnostics);

    let significant = selected_code(src, tokens, &mut ast.diagnostics);
    let statements = splitter::split_with_tokens(src, tokens.to_vec());
    for (i, stmt) in statements.iter().enumerate() {
        let next_start = statements.get(i + 1).map_or(src.len(), |s| s.range.start);
        let first = significant.partition_point(|t| t.span.start < stmt.range.start);
        let last = significant.partition_point(|t| t.span.end <= stmt.range.end);
        let extent_end = extent_end(src, &significant, last, next_start);
        let mut parser = Parser {
            src,
            toks: &significant[..last.max(first)],
            pos: first,
            extent_end,
            ast: &mut ast,
            static_method: false,
        };
        let kind = parser.statement(stmt.kind);
        ast.statements.push(StatementInfo {
            kind,
            span: stmt.range.clone(),
            extent_end,
        });
    }
    finish_spans(&mut ast);
    ast
}

/// The significant tokens the parser reads. Of Oracle's conditional
/// compilation that is each `$IF`'s first branch, as if its condition held
/// (#171): the directives, the conditions, the other branches and `$ERROR …
/// $END` are left out. Read as code, they make stray IFs, ELSEs and ENDs,
/// and alternatives that each open a block get unbalanced. Inquiry
/// directives (`$$name`) are values and stay.
fn selected_code(src: &str, tokens: &[Token], diagnostics: &mut Vec<Diagnostic>) -> Vec<Token> {
    #[derive(Clone, Copy, PartialEq)]
    enum Part {
        /// Between `$IF` and `$THEN`.
        Condition,
        /// The first branch.
        Taken,
        /// `$ELSIF` and `$ELSE` branches with their conditions.
        Skipped,
        /// `$ERROR … $END`.
        Error,
    }
    let mut diag = |span: Span, message: &str| {
        diagnostics.push(Diagnostic {
            span,
            message: message.into(),
            severity: Severity::Error,
        })
    };
    // Open `$IF`s and `$ERROR`s, where they start.
    let mut open: Vec<(Part, Span)> = Vec::new();
    let mut code = Vec::new();
    for t in tokens.iter().filter(|t| !t.kind.is_trivia()) {
        let text = &src[t.span.clone()];
        if t.kind != TokenKind::Directive || text.starts_with("$$") {
            if open.iter().all(|(part, _)| *part == Part::Taken) {
                code.push(t.clone());
            }
            continue;
        }
        let word = text[1..].to_ascii_uppercase();
        match word.as_str() {
            "IF" => open.push((Part::Condition, t.span.clone())),
            "ERROR" => open.push((Part::Error, t.span.clone())),
            "THEN" => match open.last_mut() {
                Some((part @ Part::Condition, _)) => *part = Part::Taken,
                Some((Part::Skipped, _)) => {}
                _ => diag(t.span.clone(), "$THEN outside of $IF"),
            },
            "ELSIF" | "ELSE" => match open.last_mut() {
                Some((part, span)) if *part != Part::Error => {
                    if *part == Part::Condition {
                        diag(span.clone(), "$IF without $THEN");
                    }
                    *part = Part::Skipped;
                }
                _ => diag(t.span.clone(), &format!("${word} outside of $IF")),
            },
            _ => match open.pop() {
                Some((Part::Condition, span)) => diag(span, "$IF without $THEN"),
                Some(_) => {}
                None => diag(t.span.clone(), "$END without matching $IF"),
            },
        }
    }
    for (part, span) in open {
        let what = if part == Part::Error { "$ERROR" } else { "$IF" };
        diag(span, &format!("{what} without $END"));
    }
    code
}

/// A statement's extent ends at its terminator (`;` or `/`) if one follows,
/// else where the next statement starts.
fn extent_end(src: &str, sig: &[Token], last: usize, next_start: usize) -> usize {
    match sig.get(last) {
        Some(t) if t.span.start < next_start && matches!(&src[t.span.clone()], ";" | "/") => {
            t.span.start
        }
        _ => next_start,
    }
}

/// Root scope children must be sorted for `scope_at`.
fn finish_spans(ast: &mut Ast) {
    for i in 0..ast.scopes.len() {
        let mut children = std::mem::take(&mut ast.scopes[i].children);
        children.sort_by_key(|c| ast.scopes[c.0].span.start);
        ast.scopes[i].children = children;
    }
}

fn lexer_diagnostics(src: &str, tokens: &[Token], out: &mut Vec<Diagnostic>) {
    for t in tokens {
        let text = &src[t.span.clone()];
        let message = match t.kind {
            TokenKind::String if !closed_string(text) => "Unterminated string literal",
            TokenKind::QString if !closed_q_string(text) => "Unterminated q-quoted string",
            TokenKind::BlockComment | TokenKind::Hint
                if !text.ends_with("*/") || text.len() < 4 =>
            {
                "Unterminated comment"
            }
            TokenKind::QuotedIdent if text.len() < 2 || !text.ends_with(&text[..1]) => {
                "Unterminated quoted identifier"
            }
            _ => continue,
        };
        out.push(Diagnostic {
            span: t.span.clone(),
            message: message.into(),
            severity: Severity::Error,
        });
    }
}

/// `'...'` / `N'...'` is closed if the quotes after the opening one pair up
/// with an odd one at the end.
fn closed_string(text: &str) -> bool {
    let body = text.trim_start_matches(['n', 'N']);
    let inner = &body[1..];
    // Count trailing quotes: an odd number means the last one closes.
    let trailing = inner.bytes().rev().take_while(|b| *b == b'\'').count();
    trailing % 2 == 1
}

fn closed_q_string(text: &str) -> bool {
    let body = text.trim_start_matches(['n', 'N']);
    let Some(rest) = body.get(2..) else {
        return false;
    };
    let mut chars = rest.chars();
    let Some(open) = chars.next() else {
        return false;
    };
    let close = match open {
        '[' => ']',
        '{' => '}',
        '(' => ')',
        '<' => '>',
        c => c,
    };
    let inner = &rest[open.len_utf8()..];
    inner.len() > close.len_utf8()
        && inner.ends_with('\'')
        && inner[..inner.len() - 1].ends_with(close)
}

/// Words that can never be identifiers.
const RESERVED: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "CHECK",
    "CONNECT",
    "CREATE",
    "DECLARE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "ELSIF",
    "END",
    "EXCEPTION",
    "EXISTS",
    "FOR",
    "FROM",
    "GOTO",
    "GRANT",
    "GROUP",
    "HAVING",
    "IF",
    "IN",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "LIKE",
    "LOOP",
    "MINUS",
    "NOT",
    "NULL",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "PRIOR",
    "PROCEDURE",
    "FUNCTION",
    "SELECT",
    "SET",
    "START",
    "TABLE",
    "THEN",
    "TO",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WHILE",
    "WITH",
];

pub(crate) fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(word))
}

/// Normalizes an identifier token's text the way Oracle stores names.
pub fn normalize_ident(text: &str) -> (String, bool) {
    if let Some(inner) = text.strip_prefix('"') {
        (
            inner
                .strip_suffix('"')
                .unwrap_or(inner)
                .replace("\"\"", "\""),
            true,
        )
    } else {
        (text.to_uppercase(), false)
    }
}

pub(crate) struct Parser<'a> {
    src: &'a str,
    /// Significant tokens of the whole file, cut off at the statement end.
    toks: &'a [Token],
    pos: usize,
    /// Byte offset where constructs that run to the end of input stop.
    extent_end: usize,
    ast: &'a mut Ast,
    /// The subprogram being parsed was declared `STATIC` (no `SELF`).
    static_method: bool,
}

/// Statements inside a block stop at these words (owned by the caller).
const BLOCK_STOPS: &[&str] = &["END", "ELSIF", "ELSE", "EXCEPTION", "WHEN"];

/// A PL/SQL statement that lost its `;` ends before these words.
const STATEMENT_BOUNDARY: &[&str] = &[
    "BEGIN",
    "DECLARE",
    "END",
    "ELSIF",
    "ELSE",
    "EXCEPTION",
    "LOOP",
    "WHEN",
    "THEN",
    "PROCEDURE",
    "FUNCTION",
];

impl<'a> Parser<'a> {
    // ----- token helpers ------------------------------------------------

    fn tok(&self, n: usize) -> Option<&'a Token> {
        self.toks.get(self.pos + n)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn text_of(&self, t: &Token) -> &'a str {
        &self.src[t.span.clone()]
    }

    fn text(&self, n: usize) -> &'a str {
        self.tok(n).map_or("", |t| self.text_of(t))
    }

    fn kind(&self, n: usize) -> Option<TokenKind> {
        self.tok(n).map(|t| t.kind)
    }

    fn is_w(&self, n: usize, word: &str) -> bool {
        self.tok(n)
            .is_some_and(|t| t.kind.is_word() && self.text_of(t).eq_ignore_ascii_case(word))
    }

    fn is_any(&self, n: usize, words: &[&str]) -> bool {
        self.tok(n).is_some_and(|t| {
            t.kind.is_word()
                && words
                    .iter()
                    .any(|w| self.text_of(t).eq_ignore_ascii_case(w))
        })
    }

    fn is_punct(&self, n: usize, p: &str) -> bool {
        self.tok(n)
            .is_some_and(|t| !t.kind.is_word() && self.text_of(t) == p)
    }

    fn eat_w(&mut self, word: &str) -> bool {
        let yes = self.is_w(0, word);
        if yes {
            self.pos += 1;
        }
        yes
    }

    fn eat_punct(&mut self, p: &str) -> bool {
        let yes = self.is_punct(0, p);
        if yes {
            self.pos += 1;
        }
        yes
    }

    fn start(&self) -> usize {
        self.tok(0).map_or(self.extent_end, |t| t.span.start)
    }

    /// End of the last consumed token, or the extent end if input ran out.
    fn close(&self) -> usize {
        if self.at_end() {
            self.extent_end
        } else {
            self.pos
                .checked_sub(1)
                .and_then(|p| self.toks.get(p))
                .map_or(self.extent_end, |t| t.span.end)
        }
    }

    fn prev_end(&self) -> usize {
        self.pos
            .checked_sub(1)
            .and_then(|p| self.toks.get(p))
            .map_or(0, |t| t.span.end)
    }

    /// Token usable as a name: identifier, quoted identifier or a
    /// non-reserved keyword.
    fn is_name(&self, n: usize) -> bool {
        match self.tok(n) {
            Some(t) => match t.kind {
                TokenKind::Ident | TokenKind::QuotedIdent => true,
                TokenKind::Keyword => !is_reserved(self.text_of(t)),
                _ => false,
            },
            None => false,
        }
    }

    fn ident_at(&self, n: usize) -> Option<Ident> {
        let t = self.tok(n)?;
        let (name, quoted) = normalize_ident(self.text_of(t));
        Some(Ident {
            name,
            span: t.span.clone(),
            quoted,
        })
    }

    fn name(&mut self) -> Option<Ident> {
        if !self.is_name(0) {
            return None;
        }
        let id = self.ident_at(0);
        self.pos += 1;
        id
    }

    /// `a[.b[.c]]`
    fn dotted_name(&mut self) -> Vec<Ident> {
        let mut parts = Vec::new();
        if let Some(first) = self.name() {
            parts.push(first);
            while self.is_punct(0, ".") && self.is_name(1) {
                self.pos += 1;
                parts.extend(self.name());
            }
        }
        parts
    }

    /// Span of the token starting at `offset` (for diagnostics).
    fn token_span_at(&self, offset: usize) -> Span {
        let i = self.toks.partition_point(|t| t.span.start < offset);
        self.toks
            .get(i)
            .filter(|t| t.span.start == offset)
            .map_or(offset..offset, |t| t.span.clone())
    }

    fn diag(&mut self, span: Span, message: impl Into<String>) {
        self.ast.diagnostics.push(Diagnostic {
            span,
            message: message.into(),
            severity: Severity::Error,
        });
    }

    fn new_scope(
        &mut self,
        kind: ScopeKind,
        parent: ScopeId,
        name: Option<Ident>,
        start: usize,
    ) -> ScopeId {
        let id = ScopeId(self.ast.scopes.len());
        self.ast.scopes.push(Scope {
            kind,
            name,
            owner: None,
            span: start..start,
            parent: Some(parent),
            children: Vec::new(),
            symbols: Vec::new(),
            decls: None,
            trigger_table: None,
        });
        self.ast.scopes[parent.0].children.push(id);
        id
    }

    fn end_scope(&mut self, scope: ScopeId) {
        let end = self.close();
        let s = &mut self.ast.scopes[scope.0];
        s.span.end = end.max(s.span.start);
    }

    fn add_symbol(&mut self, scope: ScopeId, symbol: Symbol) {
        self.ast.scopes[scope.0].symbols.push(symbol);
    }

    /// Skips a parenthesized group (at `(`), parsing subqueries inside.
    fn skip_group(&mut self, scope: ScopeId, parent: Option<QueryId>) {
        if !self.eat_punct("(") {
            return;
        }
        while !self.at_end() {
            if self.eat_punct(")") {
                return;
            }
            if self.is_any(0, &["SELECT", "WITH"]) {
                self.query_expr(scope, parent, true);
                continue;
            }
            if self.is_punct(0, "(") {
                self.skip_group(scope, parent);
                continue;
            }
            if self.is_punct(0, ";") {
                return;
            }
            self.pos += 1;
        }
    }

    /// Skips an expression until `stop` matches at paren depth 0 outside of
    /// `CASE ... END`. Returns whether `stop` was reached.
    fn skip_expr(&mut self, scope: ScopeId, stop: &dyn Fn(&Self) -> bool) -> bool {
        let mut case_depth = 0usize;
        while !self.at_end() {
            if case_depth == 0 && stop(self) {
                return true;
            }
            if self.is_punct(0, ";") {
                return false;
            }
            if self.is_punct(0, "(") {
                self.skip_group(scope, None);
                continue;
            }
            if self.is_w(0, "CASE") {
                case_depth += 1;
            } else if self.is_w(0, "END") {
                if case_depth == 0 {
                    return false;
                }
                case_depth -= 1;
            } else if case_depth == 0 && self.is_any(0, &["BEGIN", "DECLARE", "ELSIF", "EXCEPTION"])
            {
                return false;
            }
            self.pos += 1;
        }
        false
    }

    /// Skips to the end of a statement and eats its `;`. Stops early before
    /// block keywords when the `;` is missing.
    fn skip_statement(&mut self, scope: ScopeId, allow_when: bool) {
        let mut case_depth = 0usize;
        while !self.at_end() {
            if self.eat_punct(";") {
                return;
            }
            if self.is_punct(0, "(") {
                self.skip_group(scope, None);
                continue;
            }
            if self.is_w(0, "CASE") {
                case_depth += 1;
            } else if case_depth > 0 && self.is_w(0, "END") {
                case_depth -= 1;
            } else if case_depth == 0
                && self.is_any(0, STATEMENT_BOUNDARY)
                && !(allow_when && self.is_w(0, "WHEN"))
            {
                return;
            }
            self.pos += 1;
        }
    }

    // ----- statements ---------------------------------------------------

    fn statement(&mut self, kind: SplitKind) -> StatementKind {
        let root = Ast::ROOT;
        match kind {
            SplitKind::SqlPlus => {
                if self.is_any(0, &["EXEC", "EXECUTE"]) {
                    // `exec pkg.proc(...)` is a PL/SQL call: parse it as such.
                    let start = self.start();
                    self.pos += 1;
                    let scope = self.new_scope(ScopeKind::Block, root, None, start);
                    self.statements(scope);
                    self.end_scope(scope);
                }
                StatementKind::SqlPlus
            }
            _ if self.is_w(0, "CREATE") => self.create(root),
            _ if self.is_any(0, &["DECLARE", "BEGIN"]) || self.is_punct(0, "<<") => {
                let start = self.start();
                self.labels(root);
                let declare = self.eat_w("DECLARE");
                self.block(root, ScopeKind::Block, None, start, declare);
                StatementKind::Block
            }
            _ if self.is_any(
                0,
                &["SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE"],
            ) || self.is_punct(0, "(") =>
            {
                self.dml_or_query(root, None);
                self.scan_rest(root);
                StatementKind::Query
            }
            _ => {
                self.scan_rest(root);
                StatementKind::Other
            }
        }
    }

    /// Parses queries embedded anywhere in the rest of the statement
    /// (`CREATE VIEW v AS SELECT`, `CREATE TABLE t AS SELECT`, ...).
    fn scan_rest(&mut self, scope: ScopeId) {
        while !self.at_end() {
            let cte = self.is_w(0, "WITH")
                && self.is_name(1)
                && (self.is_w(2, "AS") || self.is_punct(2, "("));
            if self.is_w(0, "SELECT") || cte {
                self.query_expr(scope, None, false);
                continue;
            }
            if self.is_punct(0, "(") {
                self.skip_group(scope, None);
                continue;
            }
            self.pos += 1;
        }
    }

    fn create(&mut self, root: ScopeId) -> StatementKind {
        let start = self.start();
        self.pos += 1; // CREATE
        while self.is_any(
            0,
            &[
                "OR",
                "REPLACE",
                "EDITIONABLE",
                "NONEDITIONABLE",
                "EDITIONING",
                "FORCE",
                "NOFORCE",
                "AND",
                "RESOLVE",
                "COMPILE",
                "NOSHARE",
            ],
        ) {
            self.pos += 1;
        }
        let body = self.is_w(1, "BODY");
        if self.is_w(0, "PACKAGE") {
            self.pos += 1 + usize::from(body);
            let kind = if body {
                ScopeKind::PackageBody
            } else {
                ScopeKind::PackageSpec
            };
            self.package(root, kind, start);
            StatementKind::Unit
        } else if self.is_any(0, &["PROCEDURE", "FUNCTION"]) {
            self.subprogram(root, start, true);
            StatementKind::Unit
        } else if self.is_w(0, "TRIGGER") {
            self.pos += 1;
            self.trigger(root, start);
            StatementKind::Unit
        } else if self.is_w(0, "TYPE") {
            self.pos += 1 + usize::from(body);
            self.object_type(root, body, start);
            StatementKind::Unit
        } else {
            self.scan_rest(root);
            StatementKind::Other
        }
    }

    /// `[owner.]name`, setting the scope's owner.
    fn unit_name(&mut self) -> (Option<Ident>, Option<Ident>) {
        let mut parts = self.dotted_name();
        let name = parts.pop();
        (parts.pop(), name)
    }

    fn package(&mut self, root: ScopeId, kind: ScopeKind, start: usize) {
        let (owner, name) = self.unit_name();
        let scope = self.new_scope(kind, root, name, start);
        self.ast.scopes[scope.0].owner = owner;
        // AUTHID ..., ACCESSIBLE BY (...), SHARING = ..., DEFAULT COLLATION ...
        while !self.at_end() && !self.is_any(0, &["IS", "AS"]) {
            if self.is_punct(0, "(") {
                self.skip_group(scope, None);
            } else {
                self.pos += 1;
            }
        }
        self.pos += 1;
        self.declarations(scope);
        if self.eat_w("BEGIN") {
            self.statements(scope);
            if self.eat_w("EXCEPTION") {
                self.handlers(scope);
            }
        }
        self.end_keyword(scope, start);
        self.end_scope(scope);
    }

    fn trigger(&mut self, root: ScopeId, start: usize) {
        let (owner, name) = self.unit_name();
        let scope = self.new_scope(ScopeKind::Trigger, root, name, start);
        self.ast.scopes[scope.0].owner = owner;
        let mut compound = false;
        while !self.at_end() && !self.is_any(0, &["DECLARE", "BEGIN", "CALL"]) {
            if self.is_w(0, "ON") && self.ast.scopes[scope.0].trigger_table.is_none() {
                self.pos += 1;
                if self.is_any(0, &["DATABASE", "SCHEMA"]) {
                    continue;
                }
                let table = self.dotted_name();
                self.ast.scopes[scope.0].trigger_table = Some(table);
                continue;
            }
            if self.is_w(0, "COMPOUND") && self.is_w(1, "TRIGGER") {
                self.pos += 2;
                compound = true;
                break;
            }
            if self.is_punct(0, "(") {
                self.skip_group(scope, None);
            } else {
                self.pos += 1;
            }
        }
        if compound {
            self.declarations(scope);
            // Timing point sections: BEFORE STATEMENT IS BEGIN ... END BEFORE STATEMENT;
            while self.is_any(0, &["BEFORE", "AFTER", "INSTEAD"]) {
                let section_start = self.start();
                while !self.at_end() && !self.is_any(0, &["IS", "BEGIN"]) {
                    self.pos += 1;
                }
                let declare = self.eat_w("IS");
                self.block(scope, ScopeKind::Block, None, section_start, declare);
            }
            self.end_keyword(scope, start);
        } else if self.eat_w("DECLARE") {
            self.block_body(scope, start, true);
        } else if self.is_w(0, "BEGIN") {
            self.block_body(scope, start, false);
        } else {
            self.skip_statement(scope, false);
        }
        self.end_scope(scope);
    }

    fn object_type(&mut self, root: ScopeId, body: bool, start: usize) {
        let (owner, name) = self.unit_name();
        let kind = if body {
            ScopeKind::TypeBody
        } else {
            ScopeKind::TypeSpec
        };
        let scope = self.new_scope(kind, root, name, start);
        self.ast.scopes[scope.0].owner = owner;
        while !self.at_end() && !self.is_any(0, &["IS", "AS", "UNDER"]) {
            self.pos += 1;
        }
        if body {
            self.pos += 1;
            self.declarations(scope);
            self.end_keyword(scope, start);
        } else {
            if self.eat_w("UNDER") {
                self.dotted_name();
            } else {
                self.pos += 1;
            }
            if self.eat_w("OBJECT") || self.is_punct(0, "(") {
                self.object_members(scope);
            }
            self.skip_statement(scope, false);
        }
        self.end_scope(scope);
    }

    /// Attributes and method declarations of `CREATE TYPE ... AS OBJECT (...)`.
    fn object_members(&mut self, scope: ScopeId) {
        if !self.eat_punct("(") {
            return;
        }
        while !self.at_end() && !self.eat_punct(")") {
            let start = self.start();
            while self.is_any(
                0,
                &[
                    "MEMBER",
                    "STATIC",
                    "CONSTRUCTOR",
                    "MAP",
                    "ORDER",
                    "FINAL",
                    "INSTANTIABLE",
                    "OVERRIDING",
                    "NOT",
                ],
            ) {
                self.pos += 1;
            }
            if self.is_any(0, &["PROCEDURE", "FUNCTION"]) {
                self.method_spec(scope, start);
            } else if let Some(name) = self.name() {
                let ty = self.type_ref(scope);
                self.add_symbol(
                    scope,
                    symbol(name, SymbolKind::Attribute, start..self.prev_end(), ty),
                );
            } else {
                self.pos += 1;
            }
            // Up to the next member.
            while !self.at_end() && !self.is_punct(0, ",") && !self.is_punct(0, ")") {
                if self.is_punct(0, "(") {
                    self.skip_group(scope, None);
                } else {
                    self.pos += 1;
                }
            }
            self.eat_punct(",");
        }
    }

    /// Method declaration inside an object type spec (no body).
    fn method_spec(&mut self, scope: ScopeId, start: usize) {
        let is_function = self.is_w(0, "FUNCTION");
        self.pos += 1;
        let Some(name) = self.name() else {
            return;
        };
        let params = if self.is_punct(0, "(") {
            self.params(scope)
        } else {
            Vec::new()
        };
        let ty = if is_function && self.eat_w("RETURN") {
            self.return_type(scope)
        } else {
            None
        };
        let kind = if is_function {
            SymbolKind::Function
        } else {
            SymbolKind::Procedure
        };
        let mut s = symbol(name, kind, start..self.prev_end(), ty);
        s.params = params;
        s.forward = true;
        self.add_symbol(scope, s);
    }

    fn return_type(&mut self, scope: ScopeId) -> Option<TypeRef> {
        if self.is_w(0, "SELF") && self.is_w(1, "AS") && self.is_w(2, "RESULT") {
            let start = self.start();
            let path = vec![self.ident_at(0)?];
            self.pos += 3;
            let span = start..self.prev_end();
            return Some(TypeRef {
                path,
                attr: TypeAttr::None,
                text: "SELF AS RESULT".into(),
                span,
            });
        }
        self.type_ref(scope)
    }

    /// Optional `<<label>>`s before a block or loop.
    fn labels(&mut self, scope: ScopeId) {
        while self.is_punct(0, "<<") {
            let start = self.start();
            self.pos += 1;
            let name = self.name();
            self.eat_punct(">>");
            if let Some(name) = name {
                self.add_symbol(
                    scope,
                    symbol(name, SymbolKind::Label, start..self.prev_end(), None),
                );
            }
        }
    }

    /// `[DECLARE] decls BEGIN stmts [EXCEPTION handlers] END [label];`
    /// (`DECLARE` already consumed if `declare`).
    fn block(
        &mut self,
        parent: ScopeId,
        kind: ScopeKind,
        name: Option<Ident>,
        start: usize,
        declare: bool,
    ) -> ScopeId {
        let scope = self.new_scope(kind, parent, name, start);
        self.block_body(scope, start, declare);
        self.end_scope(scope);
        scope
    }

    fn block_body(&mut self, scope: ScopeId, start: usize, declare: bool) {
        if declare {
            self.declarations(scope);
        }
        let has_begin = self.eat_w("BEGIN");
        if has_begin || (!self.at_end() && !self.is_w(0, "END")) {
            self.statements(scope);
            if self.eat_w("EXCEPTION") {
                self.handlers(scope);
            }
        }
        self.end_keyword(scope, start);
    }

    /// Consumes `END [name];`, reporting stray `END IF`/`END LOOP` on the way.
    fn end_keyword(&mut self, scope: ScopeId, start: usize) {
        loop {
            if self.is_w(0, "END") && self.is_any(1, &["IF", "LOOP", "CASE"]) {
                let span = self.start()..self.tok(1).map_or(self.extent_end, |t| t.span.end);
                let what = self.text(1).to_uppercase();
                self.diag(span, format!("END {what} without matching {what}"));
                self.pos += 2;
                self.eat_punct(";");
                self.statements(scope);
                if self.eat_w("EXCEPTION") {
                    self.handlers(scope);
                }
                continue;
            }
            if self.is_any(0, &["ELSIF", "ELSE", "WHEN"]) {
                // Branch keyword outside of IF/CASE: skip it and go on.
                let span = self.tok(0).map_or(start..start, |t| t.span.clone());
                let word = self.text(0).to_uppercase();
                self.diag(span, format!("{word} outside of IF or CASE"));
                self.pos += 1;
                self.skip_statement(scope, true);
                self.statements(scope);
                continue;
            }
            break;
        }
        if self.eat_w("END") {
            // Optional label/name (or timing point words in compound triggers).
            let mut n = 0;
            while n < 3
                && !self.at_end()
                && !self.is_punct(0, ";")
                && !self.is_any(0, &["BEGIN", "END", "PROCEDURE", "FUNCTION", "CREATE"])
            {
                self.pos += 1;
                n += 1;
            }
            self.eat_punct(";");
        } else if self.at_end() {
            let span = self.token_span_at(start);
            self.diag(span, "Missing END");
        }
    }

    /// `WHEN ex [OR ex] THEN stmts ...` up to `END`.
    fn handlers(&mut self, scope: ScopeId) {
        while self.eat_w("WHEN") {
            self.skip_expr(scope, &|p| p.is_w(0, "THEN"));
            self.eat_w("THEN");
            self.statements(scope);
        }
    }

    /// Statements up to one of [`BLOCK_STOPS`] (not consumed) or the end.
    fn statements(&mut self, scope: ScopeId) {
        while !self.at_end() {
            let before = self.pos;
            if self.is_any(0, BLOCK_STOPS) {
                return;
            }
            self.plsql_statement(scope);
            if self.pos == before {
                self.pos += 1;
            }
        }
    }

    fn plsql_statement(&mut self, scope: ScopeId) {
        let start = self.start();
        if self.is_punct(0, "<<") {
            self.labels(scope);
            return;
        }
        let word = if self.kind(0).is_some_and(TokenKind::is_word) {
            self.text(0).to_ascii_uppercase()
        } else {
            String::new()
        };
        match word.as_str() {
            "BEGIN" => {
                self.block(scope, ScopeKind::Block, None, start, false);
            }
            "DECLARE" => {
                self.pos += 1;
                self.block(scope, ScopeKind::Block, None, start, true);
            }
            "IF" => self.if_statement(scope, start),
            "CASE" => self.case_statement(scope, start),
            "LOOP" => self.loop_body(scope, start),
            "WHILE" => {
                self.pos += 1;
                self.skip_expr(scope, &|p| p.is_w(0, "LOOP"));
                self.loop_body(scope, start);
            }
            "FOR" => self.for_loop(scope, start),
            "FORALL" => self.forall(scope, start),
            "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" => {
                self.dml_or_query(scope, None);
                self.skip_statement(scope, false);
            }
            "OPEN" => {
                self.pos += 1;
                self.dotted_name();
                if self.is_punct(0, "(") {
                    self.skip_group(scope, None);
                }
                if self.eat_w("FOR") && self.is_any(0, &["SELECT", "WITH"]) {
                    self.query_expr(scope, None, false);
                }
                self.skip_statement(scope, false);
            }
            "EXIT" | "CONTINUE" => {
                self.pos += 1;
                self.skip_statement(scope, true);
            }
            "PROCEDURE" | "FUNCTION" | "CURSOR" | "TYPE" | "SUBTYPE" => {
                // Declaration in a statement section (missing BEGIN?): parse it
                // so it still shows up in scope.
                self.declaration(scope);
            }
            _ => self.skip_statement(scope, false),
        }
    }

    fn if_statement(&mut self, scope: ScopeId, start: usize) {
        let if_span = start..self.tok(0).map_or(start, |t| t.span.end);
        self.pos += 1;
        self.skip_expr(scope, &|p| p.is_w(0, "THEN"));
        self.eat_w("THEN");
        self.statements(scope);
        loop {
            if self.eat_w("ELSIF") {
                self.skip_expr(scope, &|p| p.is_w(0, "THEN"));
                self.eat_w("THEN");
                self.statements(scope);
            } else if self.eat_w("ELSE") {
                self.statements(scope);
            } else {
                break;
            }
        }
        if self.is_w(0, "END") && self.is_w(1, "IF") {
            self.pos += 2;
            self.eat_punct(";");
        } else {
            self.diag(if_span, "IF without END IF");
        }
    }

    fn case_statement(&mut self, scope: ScopeId, start: usize) {
        let case_span = start..self.tok(0).map_or(start, |t| t.span.end);
        self.pos += 1;
        self.skip_expr(scope, &|p| p.is_w(0, "WHEN"));
        loop {
            if self.eat_w("WHEN") {
                self.skip_expr(scope, &|p| p.is_w(0, "THEN"));
                self.eat_w("THEN");
                self.statements(scope);
            } else if self.eat_w("ELSE") {
                self.statements(scope);
            } else {
                break;
            }
        }
        if self.is_w(0, "END") && self.is_w(1, "CASE") {
            self.pos += 2;
            while !self.at_end() && !self.is_punct(0, ";") && self.is_name(0) {
                self.pos += 1;
            }
            self.eat_punct(";");
        } else {
            self.diag(case_span, "CASE without END CASE");
        }
    }

    /// `LOOP stmts END LOOP [label];` (at `LOOP`).
    fn loop_body(&mut self, scope: ScopeId, start: usize) {
        if !self.is_w(0, "LOOP") {
            return;
        }
        let loop_span = self.tok(0).map_or(start..start, |t| t.span.clone());
        self.pos += 1;
        self.statements(scope);
        if self.is_w(0, "END") && self.is_w(1, "LOOP") {
            self.pos += 2;
            if self.is_name(0) {
                self.pos += 1;
            }
            self.eat_punct(";");
        } else {
            self.diag(loop_span, "LOOP without END LOOP");
        }
    }

    fn for_loop(&mut self, scope: ScopeId, start: usize) {
        self.pos += 1;
        let loop_scope = self.new_scope(ScopeKind::Loop, scope, None, start);
        let Some(index) = self.name() else {
            self.skip_expr(loop_scope, &|p| p.is_w(0, "LOOP"));
            self.loop_body(loop_scope, start);
            self.end_scope(loop_scope);
            return;
        };
        self.eat_w("IN");
        self.eat_w("REVERSE");
        let mut sym = symbol(index, SymbolKind::LoopIndex, start..start, None);
        if self.is_punct(0, "(") && self.is_any(1, &["SELECT", "WITH"]) {
            self.pos += 1;
            sym.query = self.query_expr(loop_scope, None, true);
            self.eat_punct(")");
        } else if self.is_name(0)
            && !self.is_punct(1, "..")
            && !self.is_punct(1, "+")
            && !self.is_punct(1, "-")
        {
            // Cursor FOR loop: `FOR r IN c[(args)] LOOP`. A numeric range starts
            // with a literal or expression followed by `..`.
            let save = self.pos;
            let tstart = self.start();
            let path = self.dotted_name();
            if self.is_punct(0, "(") {
                self.skip_group(loop_scope, None);
            }
            if self.is_w(0, "LOOP") {
                let span = tstart..self.prev_end();
                let text = self.src[span.clone()].to_owned();
                sym.ty = Some(TypeRef {
                    path,
                    attr: TypeAttr::RowType,
                    text,
                    span,
                });
            } else {
                self.pos = save;
            }
        }
        if sym.ty.is_none() && sym.query.is_none() {
            let span = sym.name.span.clone();
            sym.ty = Some(TypeRef {
                path: vec![Ident {
                    name: "PLS_INTEGER".into(),
                    span: span.clone(),
                    quoted: false,
                }],
                attr: TypeAttr::None,
                text: "PLS_INTEGER".into(),
                span,
            });
        }
        self.skip_expr(loop_scope, &|p| p.is_w(0, "LOOP"));
        sym.span = start..self.prev_end();
        self.add_symbol(loop_scope, sym);
        self.loop_body(loop_scope, start);
        self.end_scope(loop_scope);
    }

    fn forall(&mut self, scope: ScopeId, start: usize) {
        self.pos += 1;
        let loop_scope = self.new_scope(ScopeKind::Loop, scope, None, start);
        if let Some(index) = self.name() {
            let span = index.span.clone();
            let ty = TypeRef {
                path: vec![Ident {
                    name: "PLS_INTEGER".into(),
                    span: span.clone(),
                    quoted: false,
                }],
                attr: TypeAttr::None,
                text: "PLS_INTEGER".into(),
                span,
            };
            self.add_symbol(
                loop_scope,
                symbol(
                    index,
                    SymbolKind::LoopIndex,
                    start..self.prev_end(),
                    Some(ty),
                ),
            );
        }
        self.skip_expr(loop_scope, &|p| {
            p.is_any(0, &["INSERT", "UPDATE", "DELETE", "MERGE", "EXECUTE"])
        });
        if self.is_any(0, &["INSERT", "UPDATE", "DELETE", "MERGE"]) {
            self.dml_or_query(loop_scope, None);
        }
        self.skip_statement(loop_scope, false);
        self.end_scope(loop_scope);
    }

    // ----- declarations -------------------------------------------------

    /// Declarations up to `BEGIN` / `END` / a statement keyword.
    fn declarations(&mut self, scope: ScopeId) {
        let start = self.start();
        while !self.at_end() {
            if self.is_any(0, &["BEGIN", "END"]) {
                break;
            }
            if self.is_any(
                0,
                &[
                    "IF", "LOOP", "WHILE", "FOR", "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE",
                    "RETURN", "NULL", "RAISE", "OPEN", "FETCH", "CLOSE", "EXIT", "COMMIT",
                    "ROLLBACK",
                ],
            ) {
                // Statements without BEGIN: let the caller parse them.
                break;
            }
            if self.is_any(0, &["BEFORE", "AFTER", "INSTEAD"])
                && self.ast.scopes[scope.0].kind == ScopeKind::Trigger
            {
                break;
            }
            let before = self.pos;
            self.declaration(scope);
            if self.pos == before {
                self.pos += 1;
            }
        }
        let end = self.start().max(start);
        if end > start {
            self.ast.scopes[scope.0].decls = Some(start..end);
        } else if self.ast.scopes[scope.0].decls.is_none() {
            self.ast.scopes[scope.0].decls = Some(start..start);
        }
    }

    fn declaration(&mut self, scope: ScopeId) {
        let start = self.start();
        self.static_method = false;
        while self.is_any(
            0,
            &[
                "MEMBER",
                "STATIC",
                "CONSTRUCTOR",
                "MAP",
                "ORDER",
                "FINAL",
                "INSTANTIABLE",
                "OVERRIDING",
            ],
        ) || (self.is_w(0, "NOT") && self.is_any(1, &["FINAL", "INSTANTIABLE", "OVERRIDING"]))
        {
            self.static_method |= self.is_w(0, "STATIC");
            self.pos += 1;
        }
        let word = if self.kind(0).is_some_and(TokenKind::is_word) {
            self.text(0).to_ascii_uppercase()
        } else {
            String::new()
        };
        match word.as_str() {
            "PROCEDURE" | "FUNCTION" => self.subprogram(scope, start, false),
            "CURSOR" => self.cursor(scope, start),
            "TYPE" => self.type_decl(scope, start),
            "SUBTYPE" => {
                self.pos += 1;
                if let Some(name) = self.name() {
                    self.eat_w("IS");
                    let ty = self.type_ref(scope);
                    self.skip_statement(scope, false);
                    self.add_symbol(
                        scope,
                        symbol(name, SymbolKind::Subtype, start..self.prev_end(), ty),
                    );
                } else {
                    self.skip_statement(scope, false);
                }
            }
            "PRAGMA" => self.skip_statement(scope, false),
            _ if self.is_name(0) => {
                let name = self.name().expect("name");
                if self.eat_w("EXCEPTION") {
                    self.eat_punct(";");
                    self.add_symbol(
                        scope,
                        symbol(name, SymbolKind::Exception, start..self.prev_end(), None),
                    );
                    return;
                }
                let constant = self.eat_w("CONSTANT");
                let ty = self.type_ref(scope);
                self.skip_statement(scope, false);
                let kind = if constant {
                    SymbolKind::Constant
                } else {
                    SymbolKind::Variable
                };
                self.add_symbol(scope, symbol(name, kind, start..self.prev_end(), ty));
            }
            _ => self.skip_statement(scope, false),
        }
    }

    /// `PROCEDURE|FUNCTION name [(params)] [RETURN t] [modifiers] (; | IS|AS body)`.
    fn subprogram(&mut self, scope: ScopeId, start: usize, top_level: bool) {
        let is_function = self.is_w(0, "FUNCTION");
        self.pos += 1;
        let (owner, name) = if top_level {
            self.unit_name()
        } else {
            (None, self.name())
        };
        let Some(name) = name else {
            self.skip_statement(scope, false);
            return;
        };
        let kind = if is_function {
            SymbolKind::Function
        } else {
            SymbolKind::Procedure
        };
        let scope_kind = if is_function {
            ScopeKind::Function
        } else {
            ScopeKind::Procedure
        };
        let body = self.new_scope(scope_kind, scope, Some(name.clone()), start);
        self.ast.scopes[body.0].owner = owner;
        let params = if self.is_punct(0, "(") {
            self.params(body)
        } else {
            Vec::new()
        };
        for p in &params {
            self.add_symbol(
                body,
                symbol(
                    p.name.clone(),
                    SymbolKind::Parameter(p.mode),
                    p.span.clone(),
                    p.ty.clone(),
                ),
            );
        }
        let ty = if is_function && self.eat_w("RETURN") {
            self.return_type(body)
        } else {
            None
        };
        // Modifiers: DETERMINISTIC, PIPELINED, RESULT_CACHE, AUTHID ..., ...
        while !self.at_end() && !self.is_any(0, &["IS", "AS"]) && !self.is_punct(0, ";") {
            if self.is_any(
                0,
                &["BEGIN", "END", "PROCEDURE", "FUNCTION", "CURSOR", "TYPE"],
            ) {
                break;
            }
            if self.is_punct(0, "(") {
                self.skip_group(body, None);
            } else {
                self.pos += 1;
            }
        }
        let mut sym = symbol(name, kind, start..start, ty);
        sym.params = params;
        if self.is_any(0, &["IS", "AS"]) && !self.is_any(1, &["LANGUAGE", "EXTERNAL"]) {
            self.pos += 1;
            if self.ast.scopes[scope.0].kind == ScopeKind::TypeBody
                && !std::mem::take(&mut self.static_method)
            {
                // Methods see SELF.
                if let Some(type_name) = self.ast.scopes[scope.0].name.clone() {
                    let span = sym.name.span.clone();
                    let self_ident = Ident {
                        name: "SELF".into(),
                        span: span.clone(),
                        quoted: false,
                    };
                    let ty = TypeRef {
                        path: vec![type_name.clone()],
                        attr: TypeAttr::None,
                        text: type_name.name.clone(),
                        span: span.clone(),
                    };
                    self.add_symbol(
                        body,
                        symbol(
                            self_ident,
                            SymbolKind::Parameter(ParamMode::InOut),
                            span,
                            Some(ty),
                        ),
                    );
                }
            }
            self.block_body(body, start, true);
            sym.body = Some(body);
        } else {
            // Forward declaration, spec, or call spec.
            self.skip_statement(scope, false);
            sym.forward = true;
        }
        self.end_scope(body);
        sym.span = start..self.close();
        if sym.forward {
            // A declaration without body doesn't need its own scope span beyond
            // the declaration itself.
            let end = sym.span.end;
            self.ast.scopes[body.0].span = start..end;
        }
        self.add_symbol(scope, sym);
    }

    fn params(&mut self, scope: ScopeId) -> Vec<Param> {
        let mut params = Vec::new();
        if !self.eat_punct("(") {
            return params;
        }
        while !self.at_end() {
            if self.eat_punct(")") {
                break;
            }
            let start = self.start();
            let Some(name) = self.name() else {
                // Skip junk up to the next separator.
                while !self.at_end()
                    && !self.is_punct(0, ",")
                    && !self.is_punct(0, ")")
                    && !self.is_punct(0, ";")
                {
                    self.pos += 1;
                }
                if self.is_punct(0, ";") || !self.eat_punct(",") && !self.is_punct(0, ")") {
                    break;
                }
                continue;
            };
            let mut mode = ParamMode::In;
            if self.eat_w("IN") {
                if self.eat_w("OUT") {
                    mode = ParamMode::InOut;
                }
            } else if self.eat_w("OUT") {
                mode = ParamMode::Out;
            }
            self.eat_w("NOCOPY");
            let ty = self.type_ref(scope);
            let mut has_default = false;
            let mut depth = 0usize;
            while !self.at_end() {
                if depth == 0 && (self.is_punct(0, ",") || self.is_punct(0, ")")) {
                    break;
                }
                if self.is_punct(0, ";") || self.is_any(0, &["IS", "AS", "RETURN"]) && depth == 0 {
                    break;
                }
                if self.is_punct(0, ":=") || self.is_w(0, "DEFAULT") {
                    has_default = true;
                }
                if self.is_punct(0, "(") {
                    depth += 1;
                } else if self.is_punct(0, ")") {
                    depth -= 1;
                }
                self.pos += 1;
            }
            params.push(Param {
                name,
                mode,
                ty,
                has_default,
                span: start..self.prev_end(),
            });
            if !self.eat_punct(",") {
                if !self.eat_punct(")") {
                    break;
                }
                break;
            }
        }
        params
    }

    /// A type as written. Returns `None` if no type name follows.
    fn type_ref(&mut self, scope: ScopeId) -> Option<TypeRef> {
        if !self.tok(0).is_some_and(|t| {
            matches!(
                t.kind,
                TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword
            )
        }) {
            return None;
        }
        if self.is_any(0, &["IS", "AS", "DEFAULT", "NOT", "NULL", "BEGIN", "END"]) {
            return None;
        }
        let start = self.start();
        let mut path = Vec::new();
        if let Some(first) = self.ident_at(0) {
            path.push(first);
            self.pos += 1;
            while self.is_punct(0, ".")
                && self
                    .tok(1)
                    .is_some_and(|t| t.kind.is_word() || t.kind == TokenKind::QuotedIdent)
            {
                self.pos += 1;
                path.extend(self.ident_at(0));
                self.pos += 1;
            }
        }
        let mut attr = TypeAttr::None;
        if self.is_punct(0, "%") {
            if self.is_w(1, "ROWTYPE") {
                attr = TypeAttr::RowType;
                self.pos += 2;
            } else if self.is_w(1, "TYPE") {
                attr = TypeAttr::Type;
                self.pos += 2;
            }
        }
        // Precision, multi-word types (`LONG RAW`, `TIMESTAMP WITH TIME ZONE`, ...).
        loop {
            if self.is_punct(0, "(") {
                self.skip_group(scope, None);
            } else if self.is_any(
                0,
                &[
                    "RAW",
                    "PRECISION",
                    "VARYING",
                    "WITH",
                    "LOCAL",
                    "TIME",
                    "ZONE",
                    "DAY",
                    "SECOND",
                    "YEAR",
                    "MONTH",
                    "CHAR",
                    "BYTE",
                    "CHARACTER",
                    "SET",
                ],
            ) && !self.is_punct(1, ":=")
                || self.is_w(0, "TO") && self.is_any(1, &["SECOND", "MONTH"])
            {
                self.pos += 1;
            } else {
                break;
            }
        }
        let end = self.prev_end().max(start);
        let text = self.src[start..end]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        Some(TypeRef {
            path,
            attr,
            text,
            span: start..end,
        })
    }

    fn cursor(&mut self, scope: ScopeId, start: usize) {
        self.pos += 1;
        let Some(name) = self.name() else {
            self.skip_statement(scope, false);
            return;
        };
        let cursor_scope = self.new_scope(ScopeKind::Cursor, scope, None, start);
        let params = if self.is_punct(0, "(") {
            self.params(cursor_scope)
        } else {
            Vec::new()
        };
        for p in &params {
            self.add_symbol(
                cursor_scope,
                symbol(
                    p.name.clone(),
                    SymbolKind::Parameter(p.mode),
                    p.span.clone(),
                    p.ty.clone(),
                ),
            );
        }
        let ty = if self.eat_w("RETURN") {
            self.type_ref(cursor_scope)
        } else {
            None
        };
        let mut sym = symbol(name, SymbolKind::Cursor, start..start, ty);
        sym.params = params;
        if self.eat_w("IS") {
            if self.is_any(0, &["SELECT", "WITH"]) || self.is_punct(0, "(") {
                sym.query = self.query_expr(cursor_scope, None, false);
            }
        } else {
            sym.forward = true;
        }
        self.skip_statement(cursor_scope, false);
        self.end_scope(cursor_scope);
        sym.span = start..self.prev_end();
        self.add_symbol(scope, sym);
    }

    fn type_decl(&mut self, scope: ScopeId, start: usize) {
        self.pos += 1;
        let Some(name) = self.name() else {
            self.skip_statement(scope, false);
            return;
        };
        if !self.eat_w("IS") {
            self.eat_w("AS");
        }
        let def = if self.eat_w("RECORD") {
            TypeDef::Record(self.fields(scope))
        } else if self.eat_w("OBJECT") {
            TypeDef::Object(self.fields(scope))
        } else if self.is_w(0, "TABLE") && self.is_w(1, "OF") {
            self.pos += 2;
            TypeDef::Collection(self.type_ref(scope))
        } else if self.is_any(0, &["VARRAY", "VARYING"]) {
            while !self.at_end() && !self.is_w(0, "OF") && !self.is_punct(0, ";") {
                if self.is_punct(0, "(") {
                    self.skip_group(scope, None);
                } else {
                    self.pos += 1;
                }
            }
            self.eat_w("OF");
            TypeDef::Collection(self.type_ref(scope))
        } else if self.is_w(0, "REF") && self.is_w(1, "CURSOR") {
            self.pos += 2;
            let ret = if self.eat_w("RETURN") {
                self.type_ref(scope)
            } else {
                None
            };
            TypeDef::RefCursor(ret)
        } else {
            TypeDef::Other
        };
        self.skip_statement(scope, false);
        let mut sym = symbol(name, SymbolKind::Type, start..self.prev_end(), None);
        sym.type_def = Some(def);
        self.add_symbol(scope, sym);
    }

    /// `(name type [NOT NULL] [:= default], ...)`
    fn fields(&mut self, scope: ScopeId) -> Vec<Field> {
        let mut fields = Vec::new();
        if !self.eat_punct("(") {
            return fields;
        }
        while !self.at_end() {
            if self.eat_punct(")") {
                break;
            }
            if let Some(name) = self.name() {
                let ty = self.type_ref(scope);
                fields.push(Field { name, ty });
            }
            let mut depth = 0usize;
            while !self.at_end() && !self.is_punct(0, ";") {
                if depth == 0 && (self.is_punct(0, ",") || self.is_punct(0, ")")) {
                    break;
                }
                if self.is_punct(0, "(") {
                    depth += 1;
                } else if self.is_punct(0, ")") {
                    depth -= 1;
                }
                self.pos += 1;
            }
            if self.is_punct(0, ";") || !self.eat_punct(",") {
                self.eat_punct(")");
                break;
            }
        }
        fields
    }
}

fn symbol(name: Ident, kind: SymbolKind, span: Span, ty: Option<TypeRef>) -> Symbol {
    Symbol {
        name,
        kind,
        span,
        ty,
        params: Vec::new(),
        type_def: None,
        query: None,
        body: None,
        forward: false,
    }
}

#[cfg(test)]
mod tests;
