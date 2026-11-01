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
    /// ([`crate::tokenize_in`]).
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
        if !self.syntax.sqlplus() {
            if self.word_eq(first, CLIENT_COMMANDS) {
                return StatementKind::SqlPlus;
            }
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
        if self.word_eq(first, &["DECLARE", "BEGIN"]) || self.text(first) == "<<" {
            return StatementKind::PlSql;
        }
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

    fn push(&mut self, kind: StatementKind, first: usize, last: usize) {
        let range = self.tokens[first].span.start..self.tokens[last].span.end;
        self.out.push(Statement { kind, range });
    }

    fn run(mut self) -> Vec<Statement> {
        let mut i = 0;
        while i < self.tokens.len() {
            if self.tokens[i].kind.is_trivia() {
                i += 1;
                continue;
            }
            if self.is_terminator_line(i) {
                i = self.line_end(i);
                continue;
            }
            let start = i;
            let kind = self.classify(start);
            // Index of the last significant token that belongs to the statement.
            let mut last = start;
            // SQLite's `BEGIN` starting a statement begins a transaction.
            let mut depth = if self.syntax == Syntax::Sqlite {
                0
            } else {
                self.depth_change(start, 0).max(0) as usize
            };
            i += 1;
            while i < self.tokens.len() {
                let t = &self.tokens[i];
                match kind {
                    StatementKind::SqlPlus if t.kind == TokenKind::Newline => break,
                    StatementKind::Sql if t.kind == TokenKind::Semicolon && depth == 0 => {
                        i += 1;
                        break;
                    }
                    StatementKind::Sql | StatementKind::PlSql if self.is_terminator_line(i) => {
                        i = self.line_end(i);
                        break;
                    }
                    _ => {}
                }
                depth = depth.saturating_add_signed(self.depth_change(i, depth));
                if !t.kind.is_trivia() {
                    last = i;
                }
                i += 1;
            }
            // SQL*Plus commands may carry a trailing `;` (e.g. `exec foo;`).
            if kind == StatementKind::SqlPlus
                && self.tokens[last].kind == TokenKind::Semicolon
                && last > start
            {
                last -= 1;
                while self.tokens[last].kind.is_trivia() {
                    last -= 1;
                }
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
}
