//! Splits a script into executable statements, following SQL*Plus rules.
//!
//! - SQL statements end at `;` or at a line containing only `/`.
//! - PL/SQL blocks (`DECLARE`, `BEGIN`, `CREATE [OR REPLACE] PACKAGE ...`, ...)
//!   end only at a line containing only `/` (or end of input), since they
//!   contain `;` themselves.
//! - SQL*Plus commands (`SET`, `PROMPT`, `@file`, ...) end at the end of the line.
//!
//! Other databases' scripts follow their own rules ([`Syntax`]).

use std::ops::Range;

use crate::lexer::{Token, TokenKind, tokenize_in};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Sql,
    PlSql,
    SqlPlus,
}

/// Where a database's scripts end their statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Syntax {
    /// SQL*Plus.
    #[default]
    Oracle,
    /// MySQL and MariaDB: statements end as in SQL*Plus, lexed MySQL's way
    /// ([`crate::tokenize_in`]), or as the `mysql` client's: after a line
    /// `DELIMITER x`, at `x` (`DELIMITER ;` goes back).
    MySql,
    /// PostgreSQL: every statement ends at `;`. Routine bodies are
    /// (dollar-quoted) strings; `BEGIN ATOMIC … END` keeps its `;`.
    Postgres,
    /// T-SQL: `GO` on a line of its own ends a batch. Routines and scripts
    /// that `DECLARE` variables need a batch of their own, so they run up to
    /// the next `GO` (or `/`); `BEGIN … END` blocks keep their `;`.
    TSql,
    /// SQLite: every statement ends at `;`, but a trigger's `BEGIN … END`
    /// keeps its `;`. `BEGIN` and `END` starting a statement are
    /// transaction control.
    Sqlite,
}

impl Syntax {
    /// Statements end as in SQL*Plus: at `;`, or at `/` after a block.
    fn sqlplus(self) -> bool {
        matches!(self, Syntax::Oracle | Syntax::MySql)
    }
}

/// The client commands of SQL*Plus that Pythia runs itself; the rest of
/// SQL*Plus's words are statements on other databases.
const CLIENT_COMMANDS: &[&str] = &[
    "DEF", "DEFINE", "PRO", "PROMPT", "REM", "REMARK", "UNDEF", "UNDEFINE", "WHENEVER",
];

/// T-SQL routines, which must start a batch.
const TSQL_UNITS: &[&str] = &["PROC", "PROCEDURE", "FUNCTION", "TRIGGER"];

/// Words after T-SQL's `BEGIN` that make it a statement rather than a block.
const TSQL_BEGIN_STATEMENTS: &[&str] = &[
    "TRAN",
    "TRANSACTION",
    "DISTRIBUTED",
    "DIALOG",
    "CONVERSATION",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub kind: StatementKind,
    /// Byte range of the statement text in the script, without the terminator
    /// (`;` for SQL, `/` for PL/SQL) and without surrounding trivia.
    pub range: Range<usize>,
}

impl Statement {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.range.clone()]
    }
}

const SQLPLUS_COMMANDS: &[&str] = &[
    "ACCEPT",
    "CLEAR",
    "COL",
    "COLUMN",
    "CONN",
    "CONNECT",
    "DEF",
    "DEFINE",
    "DESC",
    "DESCRIBE",
    "DISC",
    "DISCONNECT",
    "EXEC",
    "EXECUTE",
    "HOST",
    "PAUSE",
    "PRI",
    "PRINT",
    "PRO",
    "PROMPT",
    "REM",
    "REMARK",
    "SET",
    "SHO",
    "SHOW",
    "SPO",
    "SPOOL",
    "STA",
    "START",
    "UNDEF",
    "UNDEFINE",
    "VAR",
    "VARIABLE",
    "WHENEVER",
];

/// `SET` is a SQL*Plus command unless followed by one of these.
const SQL_SET_TARGETS: &[&str] = &["TRANSACTION", "ROLE", "CONSTRAINT", "CONSTRAINTS"];

const PLSQL_UNITS: &[&str] = &[
    "FUNCTION",
    "PROCEDURE",
    "PACKAGE",
    "TRIGGER",
    "TYPE",
    "LIBRARY",
    "JAVA",
];

/// Words allowed between `CREATE` and the unit type of a PL/SQL unit.
const CREATE_MODIFIERS: &[&str] = &[
    "OR",
    "REPLACE",
    "EDITIONABLE",
    "NONEDITIONABLE",
    "EDITIONING",
    "AND",
    "RESOLVE",
    "COMPILE",
    "NOFORCE",
    "FORCE",
    "SOURCE",
    "NAMED",
];

pub fn split_statements(src: &str) -> Vec<Statement> {
    split_statements_in(src, Syntax::Oracle)
}

/// Splits a script of a database with `syntax`.
pub fn split_statements_in(src: &str, syntax: Syntax) -> Vec<Statement> {
    Splitter::new(src, syntax).run()
}

/// Like [`split_statements`], reusing tokens from [`crate::tokenize`].
pub(crate) fn split_with_tokens(src: &str, tokens: Vec<Token>) -> Vec<Statement> {
    Splitter {
        src,
        tokens,
        syntax: Syntax::Oracle,
        out: Vec::new(),
    }
    .run()
}

/// Returns the statement the cursor at byte `offset` belongs to.
///
/// A cursor in the gap between two statements belongs to the preceding one,
/// so executing right after typing `;` runs the statement just written.
pub fn statement_at(src: &str, offset: usize) -> Option<Statement> {
    statement_at_in(src, offset, Syntax::Oracle)
}

/// [`statement_at`] in a script of a database with `syntax`.
pub fn statement_at_in(src: &str, offset: usize, syntax: Syntax) -> Option<Statement> {
    let statements = split_statements_in(src, syntax);
    let idx = statements
        .iter()
        .rposition(|s| s.range.start <= offset)
        .unwrap_or(0);
    statements.into_iter().nth(idx)
}

/// `sql` ended by `;`, unless it has one, in a script of a database with
/// `syntax`. After a line comment, which would take it in, the `;` goes on
/// a line of its own (#206).
pub fn terminated(sql: &str, syntax: Syntax) -> String {
    let sql = sql.trim_end();
    match tokenize_in(sql, syntax).last().map(|t| t.kind) {
        Some(TokenKind::Semicolon) => sql.to_owned(),
        Some(TokenKind::LineComment) => format!("{sql}\n;"),
        _ => format!("{sql};"),
    }
}

struct Splitter<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    syntax: Syntax,
    out: Vec<Statement>,
}

impl<'a> Splitter<'a> {
    fn new(src: &'a str, syntax: Syntax) -> Self {
        Self {
            src,
            tokens: tokenize_in(src, syntax),
            syntax,
            out: Vec::new(),
        }
    }

    fn text(&self, t: &Token) -> &'a str {
        &self.src[t.span.clone()]
    }

    fn word_eq(&self, t: &Token, any_of: &[&str]) -> bool {
        t.kind.is_word() && any_of.iter().any(|w| self.text(t).eq_ignore_ascii_case(w))
    }

    /// True if the token is a `/` alone on its line (ignoring whitespace).
    fn is_slash_line(&self, idx: usize) -> bool {
        let t = &self.tokens[idx];
        t.kind == TokenKind::Operator && self.text(t) == "/" && self.line_of(t).trim() == "/"
    }

    /// True if the token starts T-SQL's batch separator: `GO`, or `GO n`,
    /// alone on its line.
    fn is_go_line(&self, idx: usize) -> bool {
        let t = &self.tokens[idx];
        if self.syntax != Syntax::TSql || !self.word_eq(t, &["GO"]) {
            return false;
        }
        let line = self.line_of(t).trim();
        line.get(..2)
            .is_some_and(|go| go.eq_ignore_ascii_case("go"))
            && line[2..].trim().chars().all(|c| c.is_ascii_digit())
            && self.src[..t.span.start]
                .rsplit('\n')
                .next()
                .is_some_and(|before| before.trim().is_empty())
    }

    /// True if the token ends a block or batch: `/` or `GO` on its own line.
    fn is_terminator_line(&self, idx: usize) -> bool {
        self.is_slash_line(idx) || self.is_go_line(idx)
    }

    /// The index of the newline ending the line of token `idx` (the count
    /// of `GO 2` belongs to the separator).
    fn line_end(&self, idx: usize) -> usize {
        self.tokens[idx..]
            .iter()
            .position(|t| t.kind == TokenKind::Newline)
            .map_or(self.tokens.len(), |n| idx + n)
    }

    /// The text of the line the token is on.
    fn line_of(&self, t: &Token) -> &'a str {
        let line_start = self.src[..t.span.start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = self.src[t.span.end..]
            .find('\n')
            .map_or(self.src.len(), |i| t.span.end + i);
        &self.src[line_start..line_end]
    }

    /// The `mysql` client's `DELIMITER x` starting at token `idx`, on a
    /// line of its own (#203): `x`, and where the line ends.
    fn delimiter_command(&self, idx: usize) -> Option<(&'a str, usize)> {
        let t = &self.tokens[idx];
        if self.syntax != Syntax::MySql || !self.word_eq(t, &["DELIMITER"]) {
            return None;
        }
        let line_end = self.src[t.span.end..]
            .find('\n')
            .map_or(self.src.len(), |i| t.span.end + i);
        let rest = &self.src[t.span.end..line_end];
        let before = self.src[..t.span.start].trim_end_matches([' ', '\t']);
        if !(before.is_empty() || before.ends_with('\n')) || !rest.starts_with([' ', '\t']) {
            return None;
        }
        Some((rest.split_whitespace().next()?, line_end))
    }

    /// Where `delimiter` starts in token `idx`, unless that is a string, a
    /// quoted name or a comment. It may go on into the next tokens, and
    /// start inside a word (`END$$`).
    fn delimiter_in(&self, idx: usize, delimiter: &str) -> Option<usize> {
        let t = &self.tokens[idx];
        if t.kind.is_trivia()
            || matches!(
                t.kind,
                TokenKind::String | TokenKind::QString | TokenKind::QuotedIdent | TokenKind::Hint
            )
        {
            return None;
        }
        t.span
            .clone()
            .find(|&at| self.src.is_char_boundary(at) && self.src[at..].starts_with(delimiter))
    }

    /// The index of the token that starts at byte `at`. When a token runs
    /// across it, the rest of the script is lexed again from there.
    fn token_at(&mut self, at: usize) -> usize {
        let idx = self.tokens.partition_point(|t| t.span.end <= at);
        if self.tokens.get(idx).is_some_and(|t| t.span.start < at) {
            self.tokens.truncate(idx);
            self.tokens.extend(
                tokenize_in(&self.src[at..], self.syntax)
                    .into_iter()
                    .map(|t| Token {
                        kind: t.kind,
                        span: t.span.start + at..t.span.end + at,
                    }),
            );
        }
        idx
    }

    /// The first significant token after `idx`.
    fn next_word(&self, idx: usize) -> Option<&Token> {
        self.significant_from(idx + 1).next()
    }

    /// How the token changes the nesting of blocks whose `;` don't end the
    /// statement: `BEGIN … END` (T-SQL, a SQLite trigger's body), `BEGIN
    /// ATOMIC … END` (PostgreSQL) and the `CASE … END` inside them.
    fn depth_change(&self, idx: usize, depth: usize) -> isize {
        let t = &self.tokens[idx];
        if self.syntax.sqlplus() || !t.kind.is_word() {
            return 0;
        }
        if self.word_eq(t, &["CASE"]) {
            return 1;
        }
        if self.word_eq(t, &["END"]) {
            return if depth > 0 { -1 } else { 0 };
        }
        if self.word_eq(t, &["BEGIN"]) {
            let next = self.next_word(idx);
            let block = match self.syntax {
                Syntax::TSql => !next.is_some_and(|n| self.word_eq(n, TSQL_BEGIN_STATEMENTS)),
                // Inside a statement: a trigger's body (see `run`).
                Syntax::Sqlite => true,
                _ => next.is_some_and(|n| self.word_eq(n, &["ATOMIC"])),
            };
            return isize::from(block);
        }
        0
    }

    /// Whether the statement at token `start` is `CREATE [OR REPLACE] RULE`.
    fn is_rule(&self, start: usize) -> bool {
        let mut words = self.significant_from(start);
        words.next().is_some_and(|t| self.word_eq(t, &["CREATE"]))
            && words
                .find(|t| !self.word_eq(t, &["OR", "REPLACE"]))
                .is_some_and(|t| self.word_eq(t, &["RULE"]))
    }

    fn significant_from(&self, idx: usize) -> impl Iterator<Item = &Token> {
        self.tokens[idx..].iter().filter(|t| !t.kind.is_trivia())
    }

    fn classify(&self, start: usize) -> StatementKind {
        let mut words = self.significant_from(start);
        let Some(first) = words.next() else {
            return StatementKind::Sql;
        };
        if self.text(first) == "@" || self.text(first) == "@@" {
            return StatementKind::SqlPlus;
        }
        if self.syntax != Syntax::Oracle {
            if self.word_eq(first, CLIENT_COMMANDS) {
                return StatementKind::SqlPlus;
            }
            // `SET DEFINE ON` switches substitution variables on (#190).
            if self.word_eq(first, &["SET"]) {
                let define = words
                    .next()
                    .is_some_and(|n| self.word_eq(n, &["DEF", "DEFINE"]));
                return if define {
                    StatementKind::SqlPlus
                } else {
                    StatementKind::Sql
                };
            }
        }
        if !self.syntax.sqlplus() {
            if self.syntax == Syntax::TSql {
                if self.word_eq(first, &["DECLARE"]) {
                    return StatementKind::PlSql;
                }
                if self.word_eq(first, &["CREATE", "ALTER"]) {
                    for t in words.take(3) {
                        if self.word_eq(t, TSQL_UNITS) {
                            return StatementKind::PlSql;
                        }
                        if !self.word_eq(t, &["OR", "ALTER"]) {
                            break;
                        }
                    }
                }
            }
            return StatementKind::Sql;
        }
        if self.syntax == Syntax::MySql && self.word_eq(first, &["BEGIN"]) {
            // `BEGIN [WORK]` starts a transaction; MariaDB's `BEGIN NOT
            // ATOMIC` opens a block (#225).
            let block = words.next().is_some_and(|n| self.word_eq(n, &["NOT"]));
            return if block {
                StatementKind::PlSql
            } else {
                StatementKind::Sql
            };
        }
        if self.word_eq(first, &["DECLARE", "BEGIN"]) || self.text(first) == "<<" {
            return StatementKind::PlSql;
        }
        // SQL*Plus's other words are MySQL's statements (`SHOW`, `DESC`,
        // `START TRANSACTION`, `EXECUTE`), which end at `;` (#225).
        if self.syntax == Syntax::Oracle {
            if self.word_eq(first, &["SET"]) {
                let next = words.next();
                return if next.is_some_and(|n| self.word_eq(n, SQL_SET_TARGETS)) {
                    StatementKind::Sql
                } else {
                    StatementKind::SqlPlus
                };
            }
            if self.word_eq(first, SQLPLUS_COMMANDS) {
                return StatementKind::SqlPlus;
            }
        }
        if self.word_eq(first, &["CREATE"]) {
            // MySQL's `DEFINER = user@host` (quoted any way) may come first.
            let mut definer = false;
            for t in words.take(24) {
                if self.word_eq(t, PLSQL_UNITS) {
                    return StatementKind::PlSql;
                }
                if definer {
                    if self.word_eq(t, &["VIEW", "EVENT", "SQL"]) {
                        break;
                    }
                } else if self.word_eq(t, &["DEFINER"]) {
                    definer = true;
                } else if !self.word_eq(t, CREATE_MODIFIERS) {
                    break;
                }
            }
        }
        StatementKind::Sql
    }

    fn push(&mut self, kind: StatementKind, first: usize, mut last: usize) {
        // SQL*Plus commands may carry a trailing `;` (e.g. `exec foo;`).
        if kind == StatementKind::SqlPlus
            && self.tokens[last].kind == TokenKind::Semicolon
            && last > first
        {
            last -= 1;
            while self.tokens[last].kind.is_trivia() {
                last -= 1;
            }
        }
        let range = self.tokens[first].span.start..self.tokens[last].span.end;
        self.out.push(Statement { kind, range });
    }

    /// Adds the statement from token `start` while `DELIMITER x` holds
    /// (#203): it ends at `x`, a client command also at the end of its
    /// line, and anything at `/` on a line of its own. Returns the index of
    /// the token after it.
    fn delimited(&mut self, start: usize, kind: StatementKind, delimiter: &str) -> usize {
        let mut last = None;
        let mut i = start;
        while i < self.tokens.len() {
            if let Some(at) = self.delimiter_in(i, delimiter) {
                if at > self.tokens[i].span.start {
                    let range = self.tokens[start].span.start..at;
                    self.out.push(Statement { kind, range });
                } else if let Some(last) = last {
                    self.push(kind, start, last);
                }
                return self.token_at(at + delimiter.len());
            }
            if i > start {
                match kind {
                    StatementKind::SqlPlus if self.tokens[i].kind == TokenKind::Newline => break,
                    StatementKind::Sql | StatementKind::PlSql if self.is_terminator_line(i) => {
                        self.push(kind, start, last.unwrap_or(start));
                        return self.line_end(i);
                    }
                    _ => {}
                }
            }
            if !self.tokens[i].kind.is_trivia() {
                last = Some(i);
            }
            i += 1;
        }
        self.push(kind, start, last.unwrap_or(start));
        i
    }

    fn run(mut self) -> Vec<Statement> {
        let mut i = 0;
        // What ends statements instead of `;` after MySQL's `DELIMITER x`.
        let mut delimiter = None;
        while i < self.tokens.len() {
            // A `;` with no statement before it ends none (#313): `select
            // 1;;`, a `;` on a line of its own.
            if self.tokens[i].kind.is_trivia() || self.tokens[i].kind == TokenKind::Semicolon {
                i += 1;
                continue;
            }
            if self.is_terminator_line(i) {
                i = self.line_end(i);
                continue;
            }
            if let Some((new, line_end)) = self.delimiter_command(i) {
                delimiter = (new != ";").then_some(new);
                i = self.token_at(line_end);
                continue;
            }
            let start = i;
            let kind = self.classify(start);
            if let Some(delimiter) = delimiter {
                i = self.delimited(start, kind, delimiter);
                continue;
            }
            // Index of the last significant token that belongs to the statement.
            let mut last = start;
            // SQLite's `BEGIN` starting a statement begins a transaction.
            let mut depth = if self.syntax == Syntax::Sqlite {
                0
            } else {
                self.depth_change(start, 0).max(0) as usize
            };
            // A PostgreSQL rule's actions in parentheses keep their `;`
            // (#346): `do also (select 1; select 2)`. Where they aren't
            // closed by the end of the script, as while they are typed, it
            // ends at the first of them as other statements do, and those
            // after it stay apart: that one, and the last token before it.
            let rule = self.syntax == Syntax::Postgres && self.is_rule(start);
            let mut parens = 0usize;
            let mut inner: Option<(usize, usize)> = None;
            let mut ended = false;
            i += 1;
            while i < self.tokens.len() {
                let t = &self.tokens[i];
                match kind {
                    StatementKind::SqlPlus if t.kind == TokenKind::Newline => break,
                    StatementKind::Sql if t.kind == TokenKind::Semicolon && depth == 0 => {
                        if parens == 0 {
                            i += 1;
                            ended = true;
                            break;
                        }
                        inner.get_or_insert((i, last));
                    }
                    StatementKind::Sql | StatementKind::PlSql if self.is_terminator_line(i) => {
                        i = self.line_end(i);
                        ended = true;
                        break;
                    }
                    _ => {}
                }
                depth = depth.saturating_add_signed(self.depth_change(i, depth));
                if rule {
                    match t.kind {
                        TokenKind::LParen => parens += 1,
                        TokenKind::RParen => parens = parens.saturating_sub(1),
                        _ => {}
                    }
                }
                if !t.kind.is_trivia() {
                    last = i;
                }
                i += 1;
            }
            if !ended && let Some((semicolon, before)) = inner {
                i = semicolon + 1;
                last = before;
            }
            self.push(kind, start, last);
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(src: &str) -> Vec<(StatementKind, &str)> {
        split_statements(src)
            .iter()
            .map(|s| (s.kind, s.text(src)))
            .collect()
    }

    /// A statement inside a comment that holds a comment is no statement
    /// on PostgreSQL and SQL Server, which nest them.
    #[test]
    fn nested_comments_hide_statements() {
        let src = "/* disabled\n/* step 2: purge */\ndelete from t;\n*/\nselect count(*) from t;";
        for syntax in [Syntax::Postgres, Syntax::TSql] {
            let statements: Vec<&str> = split_statements_in(src, syntax)
                .iter()
                .map(|s| s.text(src))
                .collect();
            assert_eq!(statements.len(), 1, "{syntax:?}: {statements:?}");
            assert!(
                statements[0].ends_with("select count(*) from t"),
                "{syntax:?}: {statements:?}"
            );
        }
    }

    #[test]
    fn sql_statements() {
        assert_eq!(
            split(
                "select 1 from dual;\n\n  update t set x = ';' where y = 1 ;\nselect 2 from dual"
            ),
            vec![
                (StatementKind::Sql, "select 1 from dual"),
                (StatementKind::Sql, "update t set x = ';' where y = 1"),
                (StatementKind::Sql, "select 2 from dual"),
            ]
        );
    }

    #[test]
    fn slash_terminates_sql() {
        assert_eq!(
            split("select 1\nfrom dual\n/\nselect 4/2 from dual\n /\n"),
            vec![
                (StatementKind::Sql, "select 1\nfrom dual"),
                (StatementKind::Sql, "select 4/2 from dual")
            ]
        );
    }

    #[test]
    fn plsql_blocks() {
        let src = "create or replace package body p as\n  procedure x is begin null; end;\nend p;\n/\n\
                   begin\n  p.x;\nend;\n/\ndeclare v number; begin null; end;";
        assert_eq!(
            split(src),
            vec![
                (
                    StatementKind::PlSql,
                    "create or replace package body p as\n  procedure x is begin null; end;\nend p;"
                ),
                (StatementKind::PlSql, "begin\n  p.x;\nend;"),
                (StatementKind::PlSql, "declare v number; begin null; end;"),
            ]
        );
    }

    #[test]
    fn mysql_definers() {
        let src = "CREATE DEFINER=`app`@`%` PROCEDURE `p`()\nbegin\n  select 1;\nend\n/\n\
                   create definer = current_user trigger t before insert on x for each row set new.a = 1;\n/\n\
                   CREATE ALGORITHM=UNDEFINED DEFINER=`app`@`%` SQL SECURITY DEFINER VIEW `v` AS select type from t;\n\
                   create definer=app@localhost view w as select 1 as procedure_count;";
        assert_eq!(
            split(src),
            vec![
                (
                    StatementKind::PlSql,
                    "CREATE DEFINER=`app`@`%` PROCEDURE `p`()\nbegin\n  select 1;\nend"
                ),
                (
                    StatementKind::PlSql,
                    "create definer = current_user trigger t before insert on x for each row set new.a = 1;"
                ),
                (
                    StatementKind::Sql,
                    "CREATE ALGORITHM=UNDEFINED DEFINER=`app`@`%` SQL SECURITY DEFINER VIEW `v` AS select type from t"
                ),
                (
                    StatementKind::Sql,
                    "create definer=app@localhost view w as select 1 as procedure_count"
                ),
            ]
        );
    }

    #[test]
    fn create_table_is_sql() {
        assert_eq!(
            split(
                "create table t (a number);\ncreate or replace editionable function f return number is begin return 1; end;\n/"
            ),
            vec![
                (StatementKind::Sql, "create table t (a number)"),
                (
                    StatementKind::PlSql,
                    "create or replace editionable function f return number is begin return 1; end;"
                ),
            ]
        );
    }

    #[test]
    fn sqlplus_commands() {
        assert_eq!(
            split(
                "set serveroutput on\nset transaction read only;\nprompt hello; world\nexec dbms_lock.sleep(1);\n@install.sql\nselect 1 from dual;"
            ),
            vec![
                (StatementKind::SqlPlus, "set serveroutput on"),
                (StatementKind::Sql, "set transaction read only"),
                (StatementKind::SqlPlus, "prompt hello; world"),
                (StatementKind::SqlPlus, "exec dbms_lock.sleep(1)"),
                (StatementKind::SqlPlus, "@install.sql"),
                (StatementKind::Sql, "select 1 from dual"),
            ]
        );
    }

    fn split_in(src: &str, syntax: Syntax) -> Vec<(StatementKind, &str)> {
        split_statements_in(src, syntax)
            .iter()
            .map(|s| (s.kind, s.text(src)))
            .collect()
    }

    /// A `;` without a statement before it is none, and starts none
    /// (#313): `select 1;;` sent `;` to the database, and a `;` on a line
    /// of its own went with the next statement.
    #[test]
    fn lone_semicolons() {
        for syntax in [
            Syntax::Oracle,
            Syntax::MySql,
            Syntax::Postgres,
            Syntax::TSql,
            Syntax::Sqlite,
        ] {
            let sql = |src: &'static str| -> Vec<&'static str> {
                split_in(src, syntax).into_iter().map(|(_, s)| s).collect()
            };
            assert_eq!(sql("select 1;;"), ["select 1"], "{syntax:?}");
            assert_eq!(
                sql("select 1;\n;\nselect 2;\n ; /* a */ ;\n"),
                ["select 1", "select 2"],
                "{syntax:?}"
            );
            assert_eq!(sql(";"), Vec::<&str>::new(), "{syntax:?}");
            assert_eq!(sql(" ; -- a\n;"), Vec::<&str>::new(), "{syntax:?}");
        }
        // T-SQL's `;WITH`.
        assert_eq!(
            split_in(
                "select 1\n;with c as (select 1 as a) select a from c",
                Syntax::TSql
            ),
            [
                (StatementKind::Sql, "select 1"),
                (
                    StatementKind::Sql,
                    "with c as (select 1 as a) select a from c"
                )
            ]
        );
        // After MySQL's DELIMITER too.
        assert_eq!(
            split_in("DELIMITER //\nselect 1 //\n;\nselect 2 //", Syntax::MySql),
            [
                (StatementKind::Sql, "select 1"),
                (StatementKind::Sql, "select 2")
            ]
        );
    }

    #[test]
    fn postgres_scripts() {
        let src = "set search_path to app, public;\n\
                   create function f(a int) returns int language plpgsql as $$\nbegin\n  return a + 1;\nend;\n$$;\n\
                   do $body$ begin perform f(1); end $body$;\n\
                   select x::text, case when y then 1 end from t;\n\
                   create function g() returns int language sql begin atomic select 1; end;\n\
                   begin;\nprompt done\nshow work_mem";
        assert_eq!(
            split_in(src, Syntax::Postgres),
            vec![
                (StatementKind::Sql, "set search_path to app, public"),
                (
                    StatementKind::Sql,
                    "create function f(a int) returns int language plpgsql as $$\nbegin\n  return a + 1;\nend;\n$$"
                ),
                (
                    StatementKind::Sql,
                    "do $body$ begin perform f(1); end $body$"
                ),
                (
                    StatementKind::Sql,
                    "select x::text, case when y then 1 end from t"
                ),
                (
                    StatementKind::Sql,
                    "create function g() returns int language sql begin atomic select 1; end"
                ),
                (StatementKind::Sql, "begin"),
                (StatementKind::SqlPlus, "prompt done"),
                (StatementKind::Sql, "show work_mem"),
            ]
        );
    }

    #[test]
    fn postgres_rules() {
        // A rule's actions in parentheses keep their `;` (#346).
        let src = "create rule r as on insert to t do also (select 1; insert into u values (2));\n\
                   create or replace rule s as on delete to t do instead (delete from u; );\n\
                   select (1; 2);\nselect 3";
        assert_eq!(
            split_in(src, Syntax::Postgres),
            vec![
                (
                    StatementKind::Sql,
                    "create rule r as on insert to t do also (select 1; insert into u values (2))"
                ),
                (
                    StatementKind::Sql,
                    "create or replace rule s as on delete to t do instead (delete from u; )"
                ),
                // Elsewhere a `;` ends the statement, parentheses or not.
                (StatementKind::Sql, "select (1"),
                (StatementKind::Sql, "2)"),
                (StatementKind::Sql, "select 3"),
            ]
        );
        // One whose parenthesis isn't closed, as while it is typed, ends at
        // its first `;`, so that the statements after it stay apart.
        let src = "create rule r as on insert to t do also (select 1;\nselect 2;\nselect (3);";
        assert_eq!(
            split_in(src, Syntax::Postgres),
            vec![
                (
                    StatementKind::Sql,
                    "create rule r as on insert to t do also (select 1"
                ),
                (StatementKind::Sql, "select 2"),
                (StatementKind::Sql, "select (3)"),
            ]
        );
        // Not on the databases without rules.
        assert_eq!(split_in("create rule r (a; b);", Syntax::Sqlite).len(), 2);
    }

    #[test]
    fn postgres_quotes() {
        // A dollar-quoted body may start with a word (#201); `E'…'` takes
        // backslash escapes (#202).
        let src = "create or replace function f() returns int language plpgsql as $$BEGIN RETURN 3; END$$;\n\
                   select $$abc$$ as e;\nselect 5;\n\
                   select E'it\\'s; here' as e;\nselect 7;";
        assert_eq!(
            split_in(src, Syntax::Postgres),
            vec![
                (
                    StatementKind::Sql,
                    "create or replace function f() returns int language plpgsql as $$BEGIN RETURN 3; END$$"
                ),
                (StatementKind::Sql, "select $$abc$$ as e"),
                (StatementKind::Sql, "select 5"),
                (StatementKind::Sql, "select E'it\\'s; here' as e"),
                (StatementKind::Sql, "select 7"),
            ]
        );
    }

    #[test]
    fn mysql_delimiters() {
        // `DELIMITER x` lines (#203): statements end at x, outside strings
        // and comments, until `DELIMITER ;`.
        let src = "DELIMITER $$\n\
                   CREATE PROCEDURE p()\nBEGIN\n  INSERT INTO t VALUES (1);\n  \
                   SELECT ';$$', `a$$` -- $$\n  # $$\n  ;\nEND$$\n\
                   select 1$$ commit$$\n\
                   DELIMITER ;\n\
                   select 2; select 3;\n\
                   delimiter ;;\n\
                   CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW BEGIN\n  SET NEW.a = 1;\nEND ;;\n\
                   ;;\n\
                   create procedure q() begin select 1; end\n/\n\
                   DELIMITER ;\n\
                   create procedure r() begin select 1; end\n/\n\
                   select 4";
        assert_eq!(
            split_in(src, Syntax::MySql),
            vec![
                (
                    StatementKind::PlSql,
                    "CREATE PROCEDURE p()\nBEGIN\n  INSERT INTO t VALUES (1);\n  \
                     SELECT ';$$', `a$$` -- $$\n  # $$\n  ;\nEND"
                ),
                (StatementKind::Sql, "select 1"),
                (StatementKind::Sql, "commit"),
                (StatementKind::Sql, "select 2"),
                (StatementKind::Sql, "select 3"),
                (
                    StatementKind::PlSql,
                    "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW BEGIN\n  SET NEW.a = 1;\nEND"
                ),
                (
                    StatementKind::PlSql,
                    "create procedure q() begin select 1; end"
                ),
                (
                    StatementKind::PlSql,
                    "create procedure r() begin select 1; end"
                ),
                (StatementKind::Sql, "select 4"),
            ]
        );
        // Other databases have no DELIMITER.
        assert_eq!(
            split_in("delimiter //\nselect 1//", Syntax::Postgres),
            vec![(StatementKind::Sql, "delimiter //\nselect 1//")]
        );
    }

    #[test]
    fn mysql_executable_comments() {
        // mysqldump's `/*!NNNNN …*/` are code (#216), its `/*M!…*/` aren't.
        let src = "/*M!999999\\- enable the sandbox mode */ \n\n\
                   /*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */;\n\
                   /*!50003 SET @saved_cs_client      = @@character_set_client */ ;\n\
                   CREATE TABLE `t` (\n  `a` int\n) ENGINE=InnoDB\n/*!50100 PARTITION BY HASH (`a`)\nPARTITIONS 2 */;\n\
                   DELIMITER ;;\n\
                   /*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER `t_bi` \
                   BEFORE INSERT ON `t` FOR EACH ROW begin set new.a = 1; end \n*/;;\n\
                   DELIMITER ;\n\
                   /* not this; */ select 1;";
        assert_eq!(
            split_in(src, Syntax::MySql),
            vec![
                (
                    StatementKind::Sql,
                    "/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */"
                ),
                (
                    StatementKind::Sql,
                    "/*!50003 SET @saved_cs_client      = @@character_set_client */"
                ),
                (
                    StatementKind::Sql,
                    "CREATE TABLE `t` (\n  `a` int\n) ENGINE=InnoDB\n/*!50100 PARTITION BY HASH (`a`)\nPARTITIONS 2 */"
                ),
                (
                    StatementKind::Sql,
                    "/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER `t_bi` \
                     BEFORE INSERT ON `t` FOR EACH ROW begin set new.a = 1; end \n*/"
                ),
                (StatementKind::Sql, "select 1"),
            ]
        );
    }

    #[test]
    fn sqlite_scripts() {
        let src = "pragma foreign_keys = on;\n\
                   begin;\n\
                   create trigger t_audit after update of name on t for each row\n\
                   when new.name <> old.name\n\
                   begin\n  insert into audit values (old.id, case when new.name is null then 'x' end);\n  \
                   update t set changed = 1 where id = new.id;\nend;\n\
                   select case when a then 1 else 2 end from t;\n\
                   end transaction;\n\
                   prompt done";
        assert_eq!(
            split_in(src, Syntax::Sqlite),
            vec![
                (StatementKind::Sql, "pragma foreign_keys = on"),
                (StatementKind::Sql, "begin"),
                (
                    StatementKind::Sql,
                    "create trigger t_audit after update of name on t for each row\n\
                     when new.name <> old.name\n\
                     begin\n  insert into audit values (old.id, case when new.name is null then 'x' end);\n  \
                     update t set changed = 1 where id = new.id;\nend"
                ),
                (
                    StatementKind::Sql,
                    "select case when a then 1 else 2 end from t"
                ),
                (StatementKind::Sql, "end transaction"),
                (StatementKind::SqlPlus, "prompt done"),
            ]
        );
    }

    #[test]
    fn mysql_scripts() {
        // Statements end as in SQL*Plus; strings, comments and `--` are
        // MySQL's (#187, #188, #189).
        let src = "select 'it\\'s; here' as a;\n\
                   # drop it; it's gone\n\
                   insert into t values (1,'a\\\\'),(2,'b;c');\n\
                   select 1 --x; select 2\n;\n\
                   select 3 -- the end; really\n;\n\
                   create procedure p() begin select 'x\\';'; end;\n/\n";
        assert_eq!(
            split_in(src, Syntax::MySql),
            vec![
                (StatementKind::Sql, "select 'it\\'s; here' as a"),
                (
                    StatementKind::Sql,
                    "insert into t values (1,'a\\\\'),(2,'b;c')"
                ),
                (StatementKind::Sql, "select 1 --x"),
                (StatementKind::Sql, "select 2"),
                (StatementKind::Sql, "select 3"),
                (
                    StatementKind::PlSql,
                    "create procedure p() begin select 'x\\';'; end;"
                ),
            ]
        );
    }

    /// SQL*Plus's words are MySQL's statements, ended by `;` like the
    /// others; `BEGIN` starts a transaction, MariaDB's `BEGIN NOT ATOMIC` a
    /// block (#225).
    #[test]
    fn mysql_statements_of_sqlplus_words() {
        let src = "select 1; set @x = 3; select @x;\n\
                   show tables; desc t;\n\
                   set @a = 1,\n    @b = 2;\n\
                   show\n  full processlist;\n\
                   prepare s from 'select 1'; execute s; deallocate prepare s;\n\
                   start transaction; begin; begin work;\n\
                   insert into t values (1);\ncommit;\n\
                   begin not atomic select 1; end;\n/\n\
                   prompt done; really\nset define off\n@more.sql\nselect 2;";
        assert_eq!(
            split_in(src, Syntax::MySql),
            vec![
                (StatementKind::Sql, "select 1"),
                (StatementKind::Sql, "set @x = 3"),
                (StatementKind::Sql, "select @x"),
                (StatementKind::Sql, "show tables"),
                (StatementKind::Sql, "desc t"),
                (StatementKind::Sql, "set @a = 1,\n    @b = 2"),
                (StatementKind::Sql, "show\n  full processlist"),
                (StatementKind::Sql, "prepare s from 'select 1'"),
                (StatementKind::Sql, "execute s"),
                (StatementKind::Sql, "deallocate prepare s"),
                (StatementKind::Sql, "start transaction"),
                (StatementKind::Sql, "begin"),
                (StatementKind::Sql, "begin work"),
                (StatementKind::Sql, "insert into t values (1)"),
                (StatementKind::Sql, "commit"),
                (StatementKind::PlSql, "begin not atomic select 1; end;"),
                (StatementKind::SqlPlus, "prompt done; really"),
                (StatementKind::SqlPlus, "set define off"),
                (StatementKind::SqlPlus, "@more.sql"),
                (StatementKind::Sql, "select 2"),
            ]
        );
    }

    #[test]
    fn tsql_batches() {
        let src = "set nocount on;\n\
                   create or alter procedure dbo.p as\nbegin\n  select 1;\n  select 2;\nend\nGO\n\
                   declare @n int = 1;\nselect @n;\ngo 2\n\
                   begin tran;\n\
                   if @@trancount > 0 begin\n  update t set a = 1;\n  commit;\nend;\n\
                   begin try select 1/0; end try begin catch select error_message(); end catch;\n\
                   select case when a = 1 then 'x' end as go from t;\n\
                   exec sp_who\n  @loginame = 'sa';";
        assert_eq!(
            split_in(src, Syntax::TSql),
            vec![
                (StatementKind::Sql, "set nocount on"),
                (
                    StatementKind::PlSql,
                    "create or alter procedure dbo.p as\nbegin\n  select 1;\n  select 2;\nend"
                ),
                (StatementKind::PlSql, "declare @n int = 1;\nselect @n;"),
                (StatementKind::Sql, "begin tran"),
                (
                    StatementKind::Sql,
                    "if @@trancount > 0 begin\n  update t set a = 1;\n  commit;\nend"
                ),
                (
                    StatementKind::Sql,
                    "begin try select 1/0; end try begin catch select error_message(); end catch"
                ),
                (
                    StatementKind::Sql,
                    "select case when a = 1 then 'x' end as go from t"
                ),
                (StatementKind::Sql, "exec sp_who\n  @loginame = 'sa'"),
            ]
        );
    }

    #[test]
    fn comments_are_not_terminators() {
        assert_eq!(
            split("-- leading; comment\nselect 1 /* ; */ from dual -- trailing;\n;"),
            vec![(StatementKind::Sql, "select 1 /* ; */ from dual")]
        );
    }

    #[test]
    fn cursor_position() {
        let src = "select 1 from dual;\n\nselect 2 from dual;";
        assert_eq!(
            statement_at(src, 3).unwrap().text(src),
            "select 1 from dual"
        );
        assert_eq!(
            statement_at(src, 20).unwrap().text(src),
            "select 1 from dual"
        );
        assert_eq!(
            statement_at(src, 25).unwrap().text(src),
            "select 2 from dual"
        );
        assert_eq!(statement_at("", 0), None);
    }

    #[test]
    fn terminated_statements() {
        let lite = |sql| terminated(sql, Syntax::Sqlite);
        assert_eq!(lite("select 1 \n"), "select 1;");
        assert_eq!(lite("select 1;\n"), "select 1;");
        assert_eq!(lite("select 1 /* one; */"), "select 1 /* one; */;");
        // A line comment would take the `;` in (#206).
        assert_eq!(lite("select 1\n  -- one;"), "select 1\n  -- one;\n;");
    }
}
