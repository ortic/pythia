//! Context-aware code completion.

use std::collections::HashSet;
use std::ops::Range;

use pythia_plsql::ast::*;
use pythia_plsql::{Token, TokenKind};

use crate::builtins;
use crate::catalog::{CatalogMember, CatalogObject, CatalogRequest, MemberKind, NameStyle};
use crate::navigate::describe;
use crate::sema::{FieldInfo, MemberTarget, Part, Resolved, Sema, SymRef, is_ref_token};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionKind {
    Keyword,
    Snippet,
    Variable,
    Constant,
    Parameter,
    Cursor,
    Type,
    Exception,
    Procedure,
    Function,
    Field,
    Column,
    Alias,
    Table,
    View,
    Package,
    Sequence,
    Synonym,
    Schema,
    Object,
    Builtin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// Text shown in the list.
    pub label: String,
    pub kind: CompletionKind,
    /// Type, signature or origin, shown next to the label.
    pub detail: String,
    /// Text that replaces [`Completion::replace`].
    pub insert_text: String,
    /// Where to put the caret inside `insert_text` (snippets), in bytes.
    pub caret: Option<usize>,
    /// Items are sorted by this key; the app may re-sort but shouldn't need to.
    pub sort_key: String,
    /// What hover shows for it, as title and text (a column's type,
    /// nullability and comment, a subprogram's signatures and doc comment);
    /// `None` when there's no more to say than `detail`.
    pub documentation: Option<(String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    /// Range of the source to replace with the chosen item (the typed prefix).
    pub replace: Range<usize>,
    pub items: Vec<CompletionItem>,
    /// `items` were cut at `max_items`: names that match what is typed so
    /// far are missing, so typing on should complete again rather than
    /// filter these (LSP's `isIncomplete`).
    pub incomplete: bool,
    /// Dictionary data that would improve the result; fetch and call again.
    pub missing: Vec<CatalogRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseStyle {
    Upper,
    Lower,
}

#[derive(Debug, Clone)]
pub struct CompletionOptions {
    /// Case for keywords and dictionary names when nothing was typed yet (or
    /// `match_typing` is off).
    pub case: CaseStyle,
    /// Follow the case of what the user typed (`sel` → `select`).
    pub match_typing: bool,
    pub max_items: usize,
}

impl Default for CompletionOptions {
    fn default() -> Self {
        Self {
            case: CaseStyle::Lower,
            match_typing: true,
            max_items: 300,
        }
    }
}

#[derive(Debug)]
enum Ctx {
    Nothing,
    Member(Vec<Part>),
    Percent,
    Bind,
    StatementStart,
    SqlTable(QueryId),
    SqlExpr(QueryId),
    SqlAfterExpr(Option<Clause>),
    InsertColumns(QueryId),
    IntoTargets,
    DeclStart,
    DeclType,
    PlsqlStatement,
    PlsqlExpr,
    PlsqlAfterExpr,
    ObjectName,
}

/// Keywords after which an expression continues (so they aren't an
/// "expression end").
const CONTINUES: &[&str] = &[
    "SELECT",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "IN",
    "BY",
    "ON",
    "IS",
    "LIKE",
    "BETWEEN",
    "THEN",
    "ELSE",
    "WHEN",
    "CASE",
    "DISTINCT",
    "UNIQUE",
    "ALL",
    "ANY",
    "SOME",
    "EXISTS",
    "SET",
    "VALUES",
    "RETURN",
    "RETURNING",
    "INTO",
    "FROM",
    "HAVING",
    "PRIOR",
    "WITH",
    "USING",
    "ESCAPE",
    "ELSIF",
    "IF",
    "WHILE",
    "BEGIN",
    "LOOP",
    "DECLARE",
    "EXCEPTION",
    "COLLECT",
    "BULK",
    "RAISE",
    "OPEN",
    "FETCH",
    "CLOSE",
    "EXIT",
    "CONTINUE",
    "MOD",
    "PARTITION",
    "ORDER",
    "GROUP",
    "OVER",
    "CONNECT",
    "START",
    "UPDATE",
    "DELETE",
    "INSERT",
    "MERGE",
    "EXEC",
    "EXECUTE",
];

pub(crate) fn complete(sema: &Sema, offset: usize, opts: &CompletionOptions) -> Completion {
    let src = sema.src;
    let mut offset = offset.min(src.len());
    while !src.is_char_boundary(offset) {
        offset -= 1;
    }
    let mut completion = Completion {
        replace: offset..offset,
        ..Completion::default()
    };
    if in_comment_or_literal(sema.tokens, src, offset) {
        return completion;
    }

    // The word being typed.
    let mut pstart = offset;
    while let Some(c) = src[..pstart].chars().next_back() {
        if c.is_alphanumeric() || matches!(c, '_' | '$' | '#') {
            pstart -= c.len_utf8();
        } else {
            break;
        }
    }
    let mut quoted = false;
    let mut word_start = pstart;
    if src[..pstart].ends_with('"') {
        // Typing a quoted identifier: `"Mixed|`.
        let i = sema.tokens.partition_point(|t| t.span.end < pstart);
        if let Some(t) = sema.tokens.get(i)
            && t.kind == TokenKind::QuotedIdent
            && t.span.start == pstart - 1
        {
            quoted = true;
            word_start = pstart - 1;
        }
    }
    let prefix = &src[pstart..offset];
    completion.replace = word_start..offset;

    let ctx = context(sema, word_start, offset);
    let mut b = Builder::new(prefix, quoted, opts, sema.cat.names());
    fill(sema, &ctx, word_start, prefix, &mut b);
    (completion.items, completion.incomplete) = b.finish(opts.max_items);
    completion
}

/// Inside a comment, string or number: no completion.
fn in_comment_or_literal(tokens: &[Token], src: &str, offset: usize) -> bool {
    let i = tokens.partition_point(|t| t.span.end < offset);
    let Some(t) = tokens.get(i) else {
        return false;
    };
    if t.span.start >= offset {
        return false;
    }
    let text = &src[t.span.clone()];
    match t.kind {
        TokenKind::LineComment => true,
        TokenKind::BlockComment | TokenKind::Hint => {
            offset < t.span.end || !text.ends_with("*/") || text.len() < 4
        }
        TokenKind::String | TokenKind::QString => {
            offset < t.span.end || text.len() < 2 || !text.ends_with('\'')
        }
        TokenKind::Number => true,
        _ => false,
    }
}

fn upper(sema: &Sema, t: &Token) -> String {
    sema.text(&t.span).to_ascii_uppercase()
}

/// Last significant token ending at or before `pos`.
fn prev_token(sema: &Sema, pos: usize) -> Option<usize> {
    let i = sema.tokens.partition_point(|t| t.span.end <= pos);
    (0..i).rev().find(|&j| !sema.tokens[j].kind.is_trivia())
}

fn expression_end(sema: &Sema, t: &Token) -> bool {
    match t.kind {
        TokenKind::Ident
        | TokenKind::QuotedIdent
        | TokenKind::Number
        | TokenKind::String
        | TokenKind::QString => true,
        TokenKind::BindVar | TokenKind::SubstVar | TokenKind::RParen => true,
        TokenKind::Keyword => !CONTINUES.contains(&upper(sema, t).as_str()),
        TokenKind::Operator => sema.text(&t.span) == "*",
        _ => false,
    }
}

fn context(sema: &Sema, at: usize, offset: usize) -> Ctx {
    let tokens = sema.tokens;
    let src = sema.src;
    let before = &src[..at];

    if before.ends_with('.') {
        let Some(dot) = prev_token(sema, at) else {
            return Ctx::Nothing;
        };
        if tokens[dot].kind != TokenKind::Dot {
            return Ctx::Nothing;
        }
        let Some(q) = prev_token(sema, tokens[dot].span.start) else {
            return Ctx::Nothing;
        };
        let t = &tokens[q];
        if !(is_ref_token(t.kind) || t.kind == TokenKind::RParen)
            || t.span.end != tokens[dot].span.start
        {
            return Ctx::Nothing;
        }
        // `f(x).` ends with a group: find the name before it.
        if t.kind == TokenKind::RParen {
            return Ctx::Member(chain_before_dot(sema, dot));
        }
        return Ctx::Member(sema.chain_ending_at(q));
    }
    if before.ends_with('%') {
        return Ctx::Percent;
    }
    if before.ends_with(':') {
        return Ctx::Bind;
    }

    let Some(stmt) = sema.ast.statement_at(offset) else {
        return Ctx::StatementStart;
    };
    let prev = prev_token(sema, at).filter(|&p| tokens[p].span.start >= stmt.span.start);
    let Some(prev) = prev else {
        return Ctx::StatementStart;
    };
    let prev_tok = &tokens[prev];
    let prev_up = upper(sema, prev_tok);
    let prev_text = sema.text(&prev_tok.span);

    if let Some(q) = sema.ast.query_at(offset).filter(|q| {
        let span = &sema.ast.query(*q).span;
        span.start <= at && span.start >= stmt.span.start
    }) {
        let query = sema.ast.query(q);
        let clause = query.clause_at(at);
        if prev_up == "AS" {
            return Ctx::Nothing;
        }
        return match clause {
            Some(Clause::From | Clause::Target | Clause::Using) | Some(Clause::Into)
                if clause != Some(Clause::Into) || query.kind != QueryKind::Select =>
            {
                if matches!(
                    prev_up.as_str(),
                    "FROM" | "JOIN" | "UPDATE" | "INTO" | "USING" | "DELETE" | "APPLY"
                ) || prev_text == ","
                {
                    Ctx::SqlTable(q)
                } else if expression_end(sema, prev_tok) {
                    Ctx::SqlAfterExpr(clause)
                } else {
                    Ctx::SqlExpr(q)
                }
            }
            Some(Clause::Into) => {
                if prev_up == "INTO" || prev_text == "," {
                    Ctx::IntoTargets
                } else {
                    Ctx::SqlAfterExpr(clause)
                }
            }
            Some(Clause::InsertColumns) => {
                if prev_text == ")" {
                    Ctx::SqlAfterExpr(Some(Clause::Into))
                } else {
                    Ctx::InsertColumns(q)
                }
            }
            Some(Clause::With) => Ctx::Nothing,
            _ => {
                if expression_end(sema, prev_tok) {
                    Ctx::SqlAfterExpr(clause)
                } else {
                    Ctx::SqlExpr(q)
                }
            }
        };
    }

    match stmt.kind {
        StatementKind::Unit | StatementKind::Block | StatementKind::SqlPlus => {
            plsql_context(sema, at, prev, &prev_up, prev_text)
        }
        _ => {
            if matches!(
                prev_up.as_str(),
                "TABLE"
                    | "VIEW"
                    | "PACKAGE"
                    | "BODY"
                    | "PROCEDURE"
                    | "FUNCTION"
                    | "TRIGGER"
                    | "SEQUENCE"
                    | "SYNONYM"
                    | "INDEX"
                    | "TYPE"
                    | "ON"
                    | "DESC"
                    | "DESCRIBE"
            ) {
                Ctx::ObjectName
            } else {
                Ctx::Nothing
            }
        }
    }
}

fn chain_before_dot(sema: &Sema, dot: usize) -> Vec<Part> {
    // `name(args).` — find the name before the group.
    let tokens = sema.tokens;
    let mut depth = 0usize;
    let mut j = dot;
    while j > 0 {
        j -= 1;
        match sema.text(&tokens[j].span) {
            ")" => depth += 1,
            "(" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(name) = (0..j).rev().find(|&k| !tokens[k].kind.is_trivia()) else {
        return Vec::new();
    };
    if !is_ref_token(tokens[name].kind) {
        return Vec::new();
    }
    let mut parts = sema.chain_ending_at(name);
    if let Some(last) = parts.last_mut() {
        last.indexed = true;
    }
    parts
}

fn plsql_context(sema: &Sema, at: usize, prev: usize, prev_up: &str, prev_text: &str) -> Ctx {
    let tokens = sema.tokens;
    let scope_id = sema.ast.scope_at(at);
    let scope = sema.ast.scope(scope_id);

    // Subprogram header: parameters and return type.
    if matches!(
        scope.kind,
        ScopeKind::Procedure | ScopeKind::Function | ScopeKind::Cursor
    ) && scope.decls.as_ref().is_none_or(|d| at < d.start)
    {
        if matches!(prev_up, "IN" | "OUT" | "NOCOPY" | "RETURN") {
            return Ctx::DeclType;
        }
        let before_prev = prev
            .checked_sub(1)
            .and_then(|p| (0..=p).rev().find(|&j| !tokens[j].kind.is_trivia()));
        if is_ref_token(tokens[prev].kind)
            && before_prev.is_some_and(|b| matches!(sema.text(&tokens[b].span), "(" | ","))
        {
            return Ctx::DeclType;
        }
        if matches!(prev_text, "(" | ",") {
            return Ctx::Nothing;
        }
    }

    // Declaration section.
    if scope.kind != ScopeKind::Root
        && let Some(decls) = &scope.decls
        && decls.start <= at
        && (at <= decls.end || decls.is_empty())
        && !matches!(prev_up, "BEGIN")
    {
        if matches!(prev_text, ";") || matches!(prev_up, "IS" | "AS" | "DECLARE") {
            return Ctx::DeclStart;
        }
        if prev_up == "CONSTANT" || prev_up == "OF" || prev_up == "RETURN" {
            return Ctx::DeclType;
        }
        let before_prev = (0..prev).rev().find(|&j| !tokens[j].kind.is_trivia());
        if is_ref_token(tokens[prev].kind)
            && before_prev.is_some_and(|b| {
                let t = sema.text(&tokens[b].span).to_ascii_uppercase();
                matches!(t.as_str(), ";" | "IS" | "AS" | "DECLARE")
            })
        {
            return Ctx::DeclType;
        }
    }

    if matches!(prev_text, ";" | ">>")
        || matches!(
            prev_up,
            "BEGIN" | "THEN" | "ELSE" | "LOOP" | "EXEC" | "EXECUTE"
        )
    {
        return Ctx::PlsqlStatement;
    }
    if expression_end(sema, &tokens[prev]) {
        return Ctx::PlsqlAfterExpr;
    }
    Ctx::PlsqlExpr
}

// ----- items --------------------------------------------------------------

struct Builder {
    prefix: String,
    quoted: bool,
    case: CaseStyle,
    /// How the database writes dictionary names.
    names: NameStyle,
    items: Vec<(String, CompletionItem)>,
    seen: HashSet<(String, CompletionKind)>,
}

/// Priorities: lower sorts first.
const P_LOCAL: u8 = 0;
const P_COLUMN: u8 = 1;
const P_MEMBER: u8 = 2;
const P_OBJECT: u8 = 3;
const P_BUILTIN: u8 = 4;
const P_KEYWORD: u8 = 5;
const P_SCHEMA: u8 = 6;
const P_SNIPPET: u8 = 7;

impl Builder {
    fn new(prefix: &str, quoted: bool, opts: &CompletionOptions, names: NameStyle) -> Self {
        let letters: Vec<char> = prefix.chars().filter(|c| c.is_alphabetic()).collect();
        let case = if opts.match_typing && !letters.is_empty() {
            if letters.iter().all(|c| c.is_lowercase()) {
                CaseStyle::Lower
            } else if letters.iter().all(|c| c.is_uppercase()) {
                CaseStyle::Upper
            } else {
                opts.case
            }
        } else {
            opts.case
        };
        Self {
            prefix: prefix.to_owned(),
            quoted,
            case,
            names,
            items: Vec::new(),
            seen: HashSet::new(),
        }
    }

    fn cased(&self, word: &str) -> String {
        match self.case {
            CaseStyle::Upper => word.to_uppercase(),
            CaseStyle::Lower => word.to_lowercase(),
        }
    }

    /// A dictionary name as it is written: Oracle's and PostgreSQL's in the
    /// case the user types unless quoted, MySQL's and SQL Server's as stored.
    fn name_text(&self, name: &str) -> String {
        match self.names {
            NameStyle::Upper | NameStyle::Lower if self.quoted || self.needs_quotes(name) => {
                format!("\"{}\"", name.replace('"', "\"\""))
            }
            NameStyle::Upper | NameStyle::Lower => self.cased(name),
            NameStyle::Backticks if needs_backticks(name) => {
                format!("`{}`", name.replace('`', "``"))
            }
            NameStyle::Brackets if needs_brackets(name) => {
                format!("[{}]", name.replace(']', "]]"))
            }
            NameStyle::Backticks | NameStyle::Brackets => name.to_owned(),
        }
    }

    /// Whether a stored name must be written in double quotes.
    fn needs_quotes(&self, name: &str) -> bool {
        match self.names {
            NameStyle::Lower => needs_lowercase_quotes(name),
            _ => needs_quotes(name),
        }
    }

    /// Adds an item; returns it unless it doesn't match or is a duplicate.
    fn push(
        &mut self,
        label: String,
        kind: CompletionKind,
        detail: String,
        insert: String,
        caret: Option<usize>,
        priority: u8,
    ) -> Option<&mut CompletionItem> {
        let rank = match_rank(&label, &self.prefix)?;
        if !self.seen.insert((label.to_uppercase(), kind)) {
            return None;
        }
        let bucket = u8::from(rank > 0);
        let sort_key = format!("{bucket}{priority}{rank}{}", label.to_lowercase());
        let item = CompletionItem {
            label,
            kind,
            detail,
            insert_text: insert,
            caret,
            sort_key: sort_key.clone(),
            documentation: None,
        };
        self.items.push((sort_key, item));
        self.items.last_mut().map(|(_, item)| item)
    }

    fn keyword(&mut self, kw: &str) {
        let text = self.cased(kw);
        self.push(
            text.clone(),
            CompletionKind::Keyword,
            String::new(),
            text,
            None,
            P_KEYWORD,
        );
    }

    fn keywords(&mut self, kws: &[&str]) {
        for kw in kws {
            self.keyword(kw);
        }
    }

    fn dictionary_name(
        &mut self,
        name: &str,
        kind: CompletionKind,
        detail: String,
        priority: u8,
    ) -> Option<&mut CompletionItem> {
        let text = self.name_text(name);
        let label = match self.names {
            NameStyle::Upper | NameStyle::Lower if self.needs_quotes(name) => text.clone(),
            NameStyle::Upper | NameStyle::Lower => self.cased(name),
            NameStyle::Backticks | NameStyle::Brackets => name.to_owned(),
        };
        self.push(label, kind, detail, text, None, priority)
    }

    /// A name declared in the source: insert as written there.
    fn source_name(
        &mut self,
        written: &str,
        kind: CompletionKind,
        detail: String,
        priority: u8,
    ) -> Option<&mut CompletionItem> {
        self.push(
            written.to_owned(),
            kind,
            detail,
            written.to_owned(),
            None,
            priority,
        )
    }

    /// The best `max` items, and whether there were more.
    fn finish(mut self, max: usize) -> (Vec<CompletionItem>, bool) {
        self.items.sort_by(|a, b| a.0.cmp(&b.0));
        let cut = self.items.len() > max;
        let items = self
            .items
            .into_iter()
            .take(max)
            .map(|(_, item)| item)
            .collect();
        (items, cut)
    }
}

/// How well `label` matches `prefix` (case-insensitive): 0 = prefix,
/// 1 = word starts (`fn` → `first_name`, `mxn_it` → `mxn_inventory_tab`),
/// 2 = substring. Letters scattered through words don't match: in a large
/// schema they match nearly everything (#160). The editor filters with the
/// same rule while the popup is open.
pub fn match_rank(label: &str, prefix: &str) -> Option<u8> {
    if prefix.is_empty() {
        return Some(0);
    }
    let l = label.trim_start_matches('"').to_lowercase();
    let p = prefix.to_lowercase();
    if l.starts_with(&p) {
        return Some(0);
    }
    // Word starts: each prefix char starts a `_`-separated segment, or
    // continues the current one.
    let segments: Vec<&str> = l.split(['_', ' ', '.']).filter(|s| !s.is_empty()).collect();
    if word_start_match(&segments, &p) {
        return Some(1);
    }
    if l.contains(&p) {
        return Some(2);
    }
    None
}

fn word_start_match(segments: &[&str], p: &str) -> bool {
    /// Consumes `p` with non-empty prefixes of segments, in order (segments
    /// may be skipped). A typed `_` is where a segment ends.
    fn go(segments: &[&str], p: &str) -> bool {
        if p.is_empty() {
            return true;
        }
        if let Some(rest) = p.strip_prefix(['_', ' ', '.']) {
            return go(segments, rest);
        }
        for (i, segment) in segments.iter().enumerate() {
            let common = segment
                .chars()
                .zip(p.chars())
                .take_while(|(a, b)| a == b)
                .count();
            for take in (1..=common).rev() {
                let byte = p.char_indices().nth(take).map_or(p.len(), |(i, _)| i);
                if go(&segments[i + 1..], &p[byte..]) {
                    return true;
                }
            }
        }
        false
    }
    segments.len() > 1 && go(segments, p)
}

/// Whether a MySQL name must be written in backticks.
fn needs_backticks(name: &str) -> bool {
    let ok_start = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let ok_rest = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'));
    !(ok_start && ok_rest) || RESERVED.contains(&name.to_ascii_uppercase().as_str())
}

/// Whether a SQL Server name must be written in square brackets.
fn needs_brackets(name: &str) -> bool {
    let ok_start = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '_' | '@' | '#'));
    let ok_rest = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$'));
    !(ok_start && ok_rest) || pythia_plsql::is_reserved(name, pythia_plsql::Syntax::TSql)
}

/// Whether a PostgreSQL name (unquoted ones are stored in lowercase) must be
/// written in double quotes.
fn needs_lowercase_quotes(name: &str) -> bool {
    let ok_start = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    let ok_rest = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '$'));
    !(ok_start && ok_rest) || pythia_plsql::is_reserved(name, pythia_plsql::Syntax::Postgres)
}

/// Whether a stored name must be written in double quotes.
pub(crate) fn needs_quotes(name: &str) -> bool {
    let mut chars = name.chars();
    let ok_start = chars.next().is_some_and(|c| c.is_ascii_uppercase());
    let ok_rest = name
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#'));
    !(ok_start && ok_rest) || RESERVED.contains(&name)
}

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
    "END",
    "EXISTS",
    "FOR",
    "FROM",
    "GRANT",
    "GROUP",
    "HAVING",
    "IN",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "LIKE",
    "MINUS",
    "NOT",
    "NULL",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "PRIOR",
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
    "WHERE",
    "WITH",
];

fn fill(sema: &Sema, ctx: &Ctx, at: usize, prefix: &str, b: &mut Builder) {
    let typed = !prefix.is_empty();
    match ctx {
        Ctx::Nothing => {}
        Ctx::StatementStart => {
            b.keywords(builtins::STATEMENT_KEYWORDS);
            for (label, text, detail) in builtins::SNIPPETS {
                let caret = text.find('|');
                let insert = text.replacen('|', "", 1);
                let insert = if b.case == CaseStyle::Lower {
                    insert.to_lowercase()
                } else {
                    insert
                };
                b.push(
                    (*label).to_owned(),
                    CompletionKind::Snippet,
                    (*detail).to_owned(),
                    insert,
                    caret,
                    P_SNIPPET,
                );
            }
        }
        Ctx::Percent => b.keywords(&["TYPE", "ROWTYPE"]),
        Ctx::Bind => {
            if sema.trigger_table(at).is_some() {
                b.keywords(&["NEW", "OLD"]);
            }
        }
        Ctx::Member(parts) => member_items(sema, parts, at, b),
        Ctx::SqlTable(q) => {
            for id in sema.query_chain(*q) {
                for cte in &sema.ast.query(id).ctes {
                    b.source_name(
                        sema.text(&cte.name.span),
                        CompletionKind::Table,
                        "WITH query".into(),
                        P_LOCAL,
                    );
                }
            }
            objects(sema, b, &["TABLE", "VIEW", "MATERIALIZED VIEW", "SYNONYM"]);
            if let Some(schemas) = sema.cat.schemas() {
                for s in schemas {
                    b.dictionary_name(&s, CompletionKind::Schema, "schema".into(), P_SCHEMA);
                }
            }
        }
        Ctx::SqlExpr(q) => {
            source_items(sema, *q, b);
            if sema.ast.query(*q).scope != Ast::ROOT || sema.ast.scope_at(at) != Ast::ROOT {
                locals(sema, at, b, false);
            }
            builtin_items(sema, b);
            b.keywords(builtins::EXPRESSION_KEYWORDS);
            if typed {
                objects(sema, b, &["FUNCTION", "PACKAGE", "SEQUENCE", "SYNONYM"]);
            }
        }
        Ctx::SqlAfterExpr(clause) => b.keywords(builtins::after_expression(*clause)),
        Ctx::InsertColumns(q) => {
            if !sema.ast.query(*q).sources.is_empty() {
                for f in sema.source_columns(*q, 0, 0) {
                    field_item(sema, b, &f, None);
                }
            }
        }
        Ctx::IntoTargets => locals(sema, at, b, true),
        Ctx::DeclStart => b.keywords(builtins::DECLARATION_KEYWORDS),
        Ctx::DeclType => {
            b.keywords(builtins::DATA_TYPES);
            for r in sema.visible_symbols(at) {
                let s = sema.symbol(r);
                if matches!(s.kind, SymbolKind::Type | SymbolKind::Subtype) {
                    b.source_name(
                        sema.text(&s.name.span),
                        CompletionKind::Type,
                        "type".into(),
                        P_LOCAL,
                    );
                }
            }
            if typed {
                objects(sema, b, &["TABLE", "VIEW", "PACKAGE", "TYPE", "SYNONYM"]);
            }
        }
        Ctx::PlsqlStatement => {
            locals(sema, at, b, false);
            b.keywords(builtins::PLSQL_STATEMENT_KEYWORDS);
            builtin_items(sema, b);
            if typed {
                objects(sema, b, &["PACKAGE", "PROCEDURE", "FUNCTION", "SYNONYM"]);
            }
        }
        Ctx::PlsqlExpr => {
            locals(sema, at, b, false);
            builtin_items(sema, b);
            b.keywords(builtins::EXPRESSION_KEYWORDS);
            if typed {
                objects(sema, b, &["PACKAGE", "FUNCTION", "SEQUENCE", "SYNONYM"]);
            }
        }
        Ctx::PlsqlAfterExpr => {
            b.keywords(&[
                "THEN", "LOOP", "AND", "OR", "NOT", "IS", "IN", "LIKE", "BETWEEN", "ELSE", "END",
                "WHEN",
            ]);
        }
        Ctx::ObjectName => objects(sema, b, &[]),
    }
}

fn object_kind(t: &str) -> CompletionKind {
    match t {
        "TABLE" => CompletionKind::Table,
        "VIEW" | "MATERIALIZED VIEW" => CompletionKind::View,
        "PACKAGE" | "PACKAGE BODY" | "TYPE" => CompletionKind::Package,
        "PROCEDURE" => CompletionKind::Procedure,
        "FUNCTION" => CompletionKind::Function,
        "SEQUENCE" => CompletionKind::Sequence,
        "SYNONYM" => CompletionKind::Synonym,
        _ => CompletionKind::Object,
    }
}

/// Objects of the current schema with one of `types` (all if empty).
fn objects(sema: &Sema, b: &mut Builder, types: &[&str]) {
    let Some(objects) = sema.cat.objects(None) else {
        return;
    };
    for o in objects {
        if types.is_empty() || types.contains(&o.object_type.as_str()) {
            let detail = o.object_type.to_lowercase();
            b.dictionary_name(&o.name, object_kind(&o.object_type), detail, P_OBJECT);
        }
    }
}

fn builtin_items(sema: &Sema, b: &mut Builder) {
    for f in builtins::FUNCTIONS {
        let text = b.cased(f.name);
        if let Some(item) = b.push(
            text.clone(),
            CompletionKind::Builtin,
            f.signature(),
            text,
            None,
            P_BUILTIN,
        ) {
            item.documentation = Some(describe(sema, &Resolved::Builtin(f)));
        }
    }
}

fn field_item(sema: &Sema, b: &mut Builder, f: &FieldInfo, origin: Option<&str>) {
    let mut detail = f.data_type.to_lowercase();
    if let Some(origin) = origin {
        detail = if detail.is_empty() {
            origin.to_owned()
        } else {
            format!("{detail} · {origin}")
        };
    }
    let kind = if f.object.is_some() {
        CompletionKind::Column
    } else {
        CompletionKind::Field
    };
    if let Some(item) = b.dictionary_name(&f.name, kind, detail, P_COLUMN) {
        item.documentation = Some(describe(sema, &Resolved::Field(f.clone())));
    }
}

/// Columns of all sources in scope, plus their aliases.
fn source_items(sema: &Sema, q: QueryId, b: &mut Builder) {
    for (qid, idx) in sema.visible_sources(q) {
        let source = sema.source(qid, idx);
        let Some(name) = source.exposed_name() else {
            continue;
        };
        let written = sema.text(&name.span).to_owned();
        let what = match &source.kind {
            SourceKind::Table(path) => path
                .iter()
                .map(|p| sema.text(&p.span))
                .collect::<Vec<_>>()
                .join("."),
            SourceKind::Subquery(_) => "inline view".into(),
            SourceKind::Function => "table function".into(),
        };
        if let Some(item) = b.source_name(&written, CompletionKind::Alias, what, P_MEMBER) {
            let source = Resolved::Source {
                query: qid,
                index: idx,
            };
            item.documentation = Some(describe(sema, &source));
        }
        for f in sema.source_columns(qid, idx, 0) {
            field_item(sema, b, &f, Some(&written));
        }
    }
}

/// Local symbols visible at `at`. `targets_only`: variables and parameters.
fn locals(sema: &Sema, at: usize, b: &mut Builder, targets_only: bool) {
    for r in sema.visible_symbols(at) {
        let s = sema.symbol(r);
        let (kind, detail) = match s.kind {
            SymbolKind::Variable => (CompletionKind::Variable, type_text(s)),
            SymbolKind::Constant => (
                CompletionKind::Constant,
                format!("constant {}", type_text(s)),
            ),
            SymbolKind::Parameter(mode) => (
                CompletionKind::Parameter,
                format!("{} {}", mode.label().to_lowercase(), type_text(s)),
            ),
            SymbolKind::LoopIndex => (CompletionKind::Variable, "loop index".into()),
            _ if targets_only => continue,
            SymbolKind::Cursor => (CompletionKind::Cursor, "cursor".into()),
            SymbolKind::Exception => (CompletionKind::Exception, "exception".into()),
            SymbolKind::Procedure => (CompletionKind::Procedure, signature_detail(sema, r)),
            SymbolKind::Function => (CompletionKind::Function, signature_detail(sema, r)),
            SymbolKind::Type | SymbolKind::Subtype => (CompletionKind::Type, "type".into()),
            SymbolKind::Attribute => (CompletionKind::Field, type_text(s)),
            SymbolKind::Label => continue,
        };
        // Not loop indexes: describing a cursor loop's record may need more
        // dictionary data.
        if let Some(item) = b.source_name(sema.text(&s.name.span), kind, detail, P_LOCAL)
            && s.kind != SymbolKind::LoopIndex
        {
            item.documentation = Some(describe(sema, &Resolved::Local(r)));
        }
    }
}

fn type_text(s: &Symbol) -> String {
    s.ty.as_ref().map(|t| t.text.clone()).unwrap_or_default()
}

pub(crate) fn signature_detail(sema: &Sema, r: SymRef) -> String {
    let s = sema.symbol(r);
    let params: Vec<String> = s
        .params
        .iter()
        .map(|p| {
            let ty =
                p.ty.as_ref()
                    .map_or(String::new(), |t| format!(" {}", t.text));
            format!("{}{ty}", sema.text(&p.name.span))
        })
        .collect();
    let ret =
        s.ty.as_ref()
            .filter(|_| s.kind == SymbolKind::Function)
            .map_or(String::new(), |t| format!(" return {}", t.text));
    if params.is_empty() {
        ret.trim_start().to_owned()
    } else {
        format!("({}){ret}", params.join(", "))
    }
}

pub(crate) fn member_signature(m: &CatalogMember) -> String {
    let params: Vec<String> = m
        .params
        .iter()
        .map(|p| {
            let default = if p.has_default { " := …" } else { "" };
            format!(
                "{} {} {}{default}",
                p.name.to_lowercase(),
                p.mode.label(),
                p.data_type
            )
        })
        .collect();
    let ret = m
        .return_type
        .as_ref()
        .map_or(String::new(), |r| format!(" return {r}"));
    format!("({}){ret}", params.join(", "))
}

fn member_items(sema: &Sema, parts: &[Part], at: usize, b: &mut Builder) {
    let query = sema.ast.query_at(at);
    match sema.member_target(parts, at, query) {
        MemberTarget::Fields(fields) => {
            for f in &fields {
                field_item(sema, b, f, None);
            }
        }
        MemberTarget::Members {
            object,
            members,
            attributes,
        } => {
            for f in &attributes {
                field_item(sema, b, f, None);
            }
            add_members(sema, b, &object, &members);
        }
        MemberTarget::LocalPackage(pkg) => {
            let scope = sema.ast.scope(pkg);
            for (index, s) in scope.symbols.iter().enumerate() {
                let r = SymRef { scope: pkg, index };
                let (kind, detail) = match s.kind {
                    SymbolKind::Procedure => (CompletionKind::Procedure, signature_detail(sema, r)),
                    SymbolKind::Function => (CompletionKind::Function, signature_detail(sema, r)),
                    SymbolKind::Variable => (CompletionKind::Variable, type_text(s)),
                    SymbolKind::Constant => (CompletionKind::Constant, type_text(s)),
                    SymbolKind::Cursor => (CompletionKind::Cursor, "cursor".into()),
                    SymbolKind::Exception => (CompletionKind::Exception, "exception".into()),
                    SymbolKind::Type | SymbolKind::Subtype => (CompletionKind::Type, "type".into()),
                    _ => continue,
                };
                if let Some(item) = b.source_name(sema.text(&s.name.span), kind, detail, P_MEMBER) {
                    item.documentation = Some(describe(sema, &Resolved::Local(r)));
                }
            }
        }
        MemberTarget::Objects { objects, .. } => {
            for o in &objects {
                b.dictionary_name(
                    &o.name,
                    object_kind(&o.object_type),
                    o.object_type.to_lowercase(),
                    P_OBJECT,
                );
            }
        }
        MemberTarget::Sequence => {
            b.keywords(&["NEXTVAL", "CURRVAL"]);
        }
        MemberTarget::Nothing => {}
    }
}

fn add_members(sema: &Sema, b: &mut Builder, object: &CatalogObject, members: &[CatalogMember]) {
    let mut seen: Vec<&str> = Vec::new();
    for m in members {
        if seen.contains(&m.name.as_str()) {
            continue;
        }
        seen.push(&m.name);
        let overloads: Vec<CatalogMember> = members
            .iter()
            .filter(|x| x.name == m.name)
            .cloned()
            .collect();
        let mut detail = member_signature(m);
        if overloads.len() > 1 {
            detail.push_str(&format!("  (+{} overloads)", overloads.len() - 1));
        }
        let kind = match m.kind {
            MemberKind::Procedure => CompletionKind::Procedure,
            MemberKind::Function => CompletionKind::Function,
        };
        if let Some(item) = b.dictionary_name(&m.name, kind, detail, P_MEMBER) {
            let member = Resolved::Member {
                object: object.clone(),
                name: m.name.clone(),
                members: overloads,
            };
            item.documentation = Some(describe(sema, &member));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks() {
        assert_eq!(match_rank("FIRST_NAME", ""), Some(0));
        assert_eq!(match_rank("FIRST_NAME", "fir"), Some(0));
        assert_eq!(match_rank("FIRST_NAME", "fn"), Some(1));
        assert_eq!(match_rank("FIRST_NAME", "name"), Some(1));
        assert_eq!(match_rank("FIRST_NAME", "rst"), Some(2));
        assert_eq!(match_rank("FIRST_NAME", "xyz"), None);
        // Letters scattered through words don't match (#160).
        assert_eq!(match_rank("FIRST_NAME", "frnm"), None);
        assert_eq!(match_rank("mxn_access_mgmt_hist", "mxn_it"), None);
        assert_eq!(match_rank("mxn75_inventory_part_hist", "mxn_it"), None);
        // A typed `_` ends a word.
        assert_eq!(match_rank("mxn_item_tab", "mxn_it"), Some(0));
        assert_eq!(
            match_rank("mxn_inventory_transaction_tab", "mxn_it"),
            Some(1)
        );
        assert_eq!(match_rank("mxnit_x", "mxn_it"), None);
    }

    #[test]
    fn quoting() {
        assert!(!needs_quotes("EMPLOYEES"));
        assert!(needs_quotes("Mixed"));
        assert!(needs_quotes("A B"));
        assert!(needs_quotes("SELECT"));
        assert!(!needs_quotes("EMP$1#"));
        assert!(!needs_lowercase_quotes("public"));
        assert!(needs_lowercase_quotes("Emp"));
        assert!(needs_lowercase_quotes("order"));
        assert!(!needs_brackets("Emp_2"));
        assert!(needs_brackets("my table"));
        assert!(needs_brackets("Order"));

        let opts = CompletionOptions::default();
        let text = |names, name| Builder::new("", false, &opts, names).name_text(name);
        assert_eq!(text(NameStyle::Lower, "Emp"), "\"Emp\"");
        assert_eq!(text(NameStyle::Brackets, "a]b"), "[a]]b]");
        assert_eq!(text(NameStyle::Brackets, "Emp"), "Emp");
        assert_eq!(text(NameStyle::Backticks, "order"), "`order`");
    }
}
