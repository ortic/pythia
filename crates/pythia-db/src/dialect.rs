//! What differs between the databases Pythia talks to.
//!
//! Every connection has a [`Dialect`]. Sessions speak the same
//! [`DbRequest`](crate::DbRequest)s and [`DbEvent`](crate::DbEvent)s for every
//! database; what a database can't do is a [`Feature`] it lacks, and the UI
//! asks [`Dialect::supports`] before offering it, so unsupported tools are
//! hidden instead of failing.

pub use pythia_config::DbKind;
use pythia_plsql::Syntax;

use crate::meta::{Detail, ObjectType};

/// Something Pythia offers that not every database has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    /// PL/SQL: program windows, compiling, test scripts, `exec`.
    PlSql,
    /// `DBMS_OUTPUT` lines after each statement.
    ServerOutput,
    ExplainPlan,
    /// Running a statement with its actual plan and session statistics.
    Statistics,
    Profiler,
    Debugger,
    /// The monitoring dashboard: KPIs, trends, findings, top SQL.
    Dashboard,
    /// The session list and the blocking tree.
    Sessions,
    /// AWR and trace analysis.
    Monitor,
    SchedulerJobs,
    InvalidObjects,
    /// Searching the stored source of the database.
    SourceSearch,
    /// Searching values in the tables of a schema.
    DataSearch,
    /// Editing query results in place.
    EditResults,
    /// Opening parent and child rows of a result row.
    FollowKeys,
    /// Streaming a query's rows into a file.
    ExecuteToFile,
    Import,
    ExportTables,
    /// Schema and data compare.
    Compare,
    /// Form editors (sequences, views, synonyms, indexes, grants) and the
    /// table editor.
    ObjectEditors,
    /// The forms that create and change users and roles.
    AccountEditors,
    /// The built-in reports.
    Reports,
    /// Completion and navigation with dictionary data.
    Completion,
    /// Indexes, constraints and triggers below a table in the object browser.
    TableParts,
    /// Dictionary lookups for the AI assistant.
    AiLookups,
    /// Comparing project files with the stored objects, and exporting a
    /// schema into files.
    ObjectFiles,
    /// Connecting through an SSH server ([`crate::ssh`]).
    SshTunnel,
}

/// The database of a connection and what it supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Dialect(pub DbKind);

impl From<DbKind> for Dialect {
    fn from(kind: DbKind) -> Self {
        Self(kind)
    }
}

impl Dialect {
    pub const ORACLE: Dialect = Dialect(DbKind::Oracle);
    pub const MYSQL: Dialect = Dialect(DbKind::MySql);
    pub const POSTGRES: Dialect = Dialect(DbKind::Postgres);
    pub const SQLSERVER: Dialect = Dialect(DbKind::SqlServer);
    pub const SQLITE: Dialect = Dialect(DbKind::Sqlite);

    pub fn kind(self) -> DbKind {
        self.0
    }

    /// The product name, e.g. for "Connected to … (MySQL 9.4.0)".
    pub fn name(self) -> &'static str {
        self.0.label()
    }

    /// Whether the UI should offer `feature` on this database. Everything
    /// was built for Oracle; other databases list what they have so far.
    pub fn supports(self, feature: Feature) -> bool {
        match self.0 {
            // Its connect strings (TNS aliases, descriptors) aren't pointed
            // at a tunnel yet.
            DbKind::Oracle => feature != Feature::SshTunnel,
            DbKind::MySql => matches!(
                feature,
                Feature::ExplainPlan
                    | Feature::Completion
                    | Feature::SshTunnel
                    | Feature::Dashboard
                    | Feature::Sessions
                    | Feature::EditResults
                    | Feature::AccountEditors
            ),
            // Rows found by key: MySQL's way, not yet theirs (#101).
            DbKind::Postgres | DbKind::SqlServer => matches!(
                feature,
                Feature::ExplainPlan
                    | Feature::Completion
                    | Feature::SshTunnel
                    | Feature::Dashboard
                    | Feature::Sessions
                    | Feature::AccountEditors
            ),
            // A file: no server to monitor, no accounts, nothing to tunnel.
            DbKind::Sqlite => matches!(feature, Feature::ExplainPlan | Feature::Completion),
        }
    }

    /// How the database's scripts end their statements.
    pub fn syntax(self) -> Syntax {
        match self.0 {
            DbKind::Oracle | DbKind::MySql => Syntax::Oracle,
            DbKind::Postgres => Syntax::Postgres,
            DbKind::SqlServer => Syntax::TSql,
            DbKind::Sqlite => Syntax::Sqlite,
        }
    }

    /// What the object browser lists of the database itself, in display
    /// order: users and roles everywhere, Oracle's profiles and directories.
    pub fn account_types(self) -> &'static [ObjectType] {
        match self.0 {
            DbKind::Oracle => &[
                ObjectType::User,
                ObjectType::Role,
                ObjectType::Profile,
                ObjectType::Directory,
            ],
            DbKind::MySql | DbKind::Postgres | DbKind::SqlServer => {
                &[ObjectType::User, ObjectType::Role]
            }
            DbKind::Sqlite => &[],
        }
    }

    /// Folders of a schema in the object browser, in display order.
    pub fn browser_types(self) -> &'static [ObjectType] {
        match self.0 {
            DbKind::Oracle => ObjectType::BROWSER,
            DbKind::MySql => &[
                ObjectType::Table,
                ObjectType::View,
                ObjectType::Procedure,
                ObjectType::Function,
                ObjectType::Trigger,
            ],
            DbKind::Postgres => &[
                ObjectType::Table,
                ObjectType::View,
                ObjectType::MaterializedView,
                ObjectType::Procedure,
                ObjectType::Function,
                ObjectType::Trigger,
                ObjectType::Sequence,
            ],
            DbKind::SqlServer => &[
                ObjectType::Table,
                ObjectType::View,
                ObjectType::Procedure,
                ObjectType::Function,
                ObjectType::Trigger,
                ObjectType::Sequence,
                ObjectType::Synonym,
            ],
            DbKind::Sqlite => &[
                ObjectType::Table,
                ObjectType::View,
                ObjectType::Index,
                ObjectType::Trigger,
            ],
        }
    }

    /// Tabs of an object view besides columns and source.
    pub fn details(self, object_type: ObjectType) -> &'static [Detail] {
        const TABLE: &[Detail] = &[
            Detail::Indexes,
            Detail::Constraints,
            Detail::Triggers,
            Detail::Grants,
            Detail::UsedBy,
        ];
        match (self.0, object_type) {
            (_, ObjectType::DatabaseLink) => Detail::LINK,
            (DbKind::Oracle, ObjectType::Table) => Detail::TABLE,
            (DbKind::Oracle, _) => Detail::CODE,
            // No privileges in a file.
            (DbKind::Sqlite, ObjectType::Table) => &[
                Detail::Indexes,
                Detail::Constraints,
                Detail::Triggers,
                Detail::UsedBy,
            ],
            (DbKind::Sqlite, ObjectType::View) => &[Detail::Uses, Detail::UsedBy],
            (DbKind::Sqlite, _) => &[],
            (_, ObjectType::Table) => TABLE,
            (DbKind::MySql, ObjectType::View) => &[Detail::Uses],
            (DbKind::MySql, _) => &[],
            (DbKind::Postgres, ObjectType::View | ObjectType::MaterializedView) => {
                &[Detail::Uses, Detail::UsedBy, Detail::Grants]
            }
            (DbKind::Postgres, ObjectType::Function | ObjectType::Procedure) => &[Detail::Grants],
            (DbKind::Postgres, _) => &[],
            (
                DbKind::SqlServer,
                ObjectType::View | ObjectType::Procedure | ObjectType::Function,
            ) => Detail::CODE,
            (DbKind::SqlServer, ObjectType::Trigger) => &[Detail::Uses],
            (DbKind::SqlServer, _) => &[],
        }
    }

    /// Quotes an identifier only if the database would need it. Oracle folds
    /// unquoted names to uppercase, PostgreSQL to lowercase; MySQL, SQL
    /// Server and SQLite keep their case.
    pub fn quote_ident(self, name: &str) -> String {
        let reserved = match self.0 {
            DbKind::Postgres | DbKind::SqlServer | DbKind::Sqlite => {
                pythia_plsql::is_reserved(name, self.syntax())
            }
            DbKind::Oracle | DbKind::MySql => pythia_plsql::is_keyword(name),
        };
        let plain = |start: fn(char) -> bool, rest: fn(char) -> bool| {
            name.chars().next().is_some_and(start) && name.chars().all(rest) && !reserved
        };
        match self.0 {
            DbKind::Oracle => {
                if plain(
                    |c| c.is_ascii_uppercase(),
                    |c| c.is_ascii_uppercase() || c.is_ascii_digit() || "_$#".contains(c),
                ) {
                    name.to_owned()
                } else {
                    format!("\"{}\"", name.replace('"', "\"\""))
                }
            }
            DbKind::MySql => {
                if plain(
                    |c| c.is_ascii_alphabetic() || c == '_',
                    |c| c.is_ascii_alphanumeric() || "_$".contains(c),
                ) {
                    name.to_owned()
                } else {
                    format!("`{}`", name.replace('`', "``"))
                }
            }
            DbKind::Postgres => {
                if plain(
                    |c| c.is_ascii_lowercase() || c == '_',
                    |c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_$".contains(c),
                ) {
                    name.to_owned()
                } else {
                    format!("\"{}\"", name.replace('"', "\"\""))
                }
            }
            DbKind::Sqlite => {
                if plain(
                    |c| c.is_ascii_alphabetic() || c == '_',
                    |c| c.is_ascii_alphanumeric() || "_$".contains(c),
                ) {
                    name.to_owned()
                } else {
                    format!("\"{}\"", name.replace('"', "\"\""))
                }
            }
            DbKind::SqlServer => {
                if plain(
                    |c| c.is_ascii_alphabetic() || "_@#".contains(c),
                    |c| c.is_ascii_alphanumeric() || "_@#$".contains(c),
                ) {
                    name.to_owned()
                } else {
                    format!("[{}]", name.replace(']', "]]"))
                }
            }
        }
    }

    /// `owner.name`, quoted where needed.
    pub fn qualified(self, owner: &str, name: &str) -> String {
        format!("{}.{}", self.quote_ident(owner), self.quote_ident(name))
    }

    /// How the dictionary stores an unquoted name the user typed: Oracle
    /// folds it to uppercase, PostgreSQL to lowercase; MySQL, SQL Server and
    /// SQLite keep it (and compare names without case where it matters).
    pub fn dictionary_name(self, name: &str) -> String {
        match self.0 {
            DbKind::Oracle => name.to_uppercase(),
            DbKind::Postgres => name.to_lowercase(),
            DbKind::MySql | DbKind::SqlServer | DbKind::Sqlite => name.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_has_everything_but_ssh() {
        for feature in [
            Feature::PlSql,
            Feature::Debugger,
            Feature::Dashboard,
            Feature::Sessions,
            Feature::Monitor,
        ] {
            assert!(Dialect::ORACLE.supports(feature));
        }
        assert!(!Dialect::ORACLE.supports(Feature::SshTunnel));
        for feature in [
            Feature::ExplainPlan,
            Feature::Dashboard,
            Feature::SshTunnel,
            Feature::EditResults,
            Feature::Sessions,
        ] {
            assert!(Dialect::MYSQL.supports(feature));
        }
        // Rows found by key: MySQL's way, not yet theirs (#101).
        for dialect in [Dialect::POSTGRES, Dialect::SQLSERVER] {
            assert!(!dialect.supports(Feature::EditResults));
        }
        for dialect in [Dialect::MYSQL, Dialect::POSTGRES, Dialect::SQLSERVER] {
            assert!(dialect.supports(Feature::ExplainPlan));
            assert!(!dialect.supports(Feature::Debugger));
            assert!(!dialect.supports(Feature::ServerOutput));
            assert!(!dialect.supports(Feature::Monitor));
        }
        // A dashboard (#92), SSH tunnels (#97), sessions (#131).
        for dialect in [Dialect::POSTGRES, Dialect::SQLSERVER] {
            assert!(dialect.supports(Feature::Dashboard));
            assert!(dialect.supports(Feature::Sessions));
            assert!(dialect.supports(Feature::SshTunnel));
        }
        // A file: SQL windows, explain plan and completion.
        let sqlite = Dialect::SQLITE;
        assert!(sqlite.supports(Feature::ExplainPlan));
        assert!(sqlite.supports(Feature::Completion));
        for feature in [
            Feature::SshTunnel,
            Feature::Dashboard,
            Feature::Sessions,
            Feature::AccountEditors,
            Feature::PlSql,
        ] {
            assert!(!sqlite.supports(feature));
        }
        assert!(sqlite.account_types().is_empty());
        assert_eq!(Dialect::default(), Dialect::ORACLE);
    }

    #[test]
    fn quoting() {
        let oracle = Dialect::ORACLE;
        assert_eq!(oracle.quote_ident("EMP"), "EMP");
        assert_eq!(oracle.quote_ident("emp"), "\"emp\"");
        assert_eq!(oracle.quote_ident("ORDER"), "\"ORDER\"");
        let mysql = Dialect::MYSQL;
        assert_eq!(mysql.quote_ident("emp"), "emp");
        assert_eq!(mysql.quote_ident("Emp_2"), "Emp_2");
        assert_eq!(mysql.quote_ident("order"), "`order`");
        assert_eq!(mysql.quote_ident("my table"), "`my table`");
        assert_eq!(mysql.quote_ident("a`b"), "`a``b`");
        assert_eq!(mysql.quote_ident("2fa"), "`2fa`");
        assert_eq!(mysql.qualified("pythia", "emp"), "pythia.emp");
        let postgres = Dialect::POSTGRES;
        assert_eq!(postgres.quote_ident("emp_2"), "emp_2");
        assert_eq!(postgres.quote_ident("Emp"), "\"Emp\"");
        assert_eq!(postgres.quote_ident("order"), "\"order\"");
        assert_eq!(postgres.qualified("public", "a\"b"), "public.\"a\"\"b\"");
        let sqlserver = Dialect::SQLSERVER;
        assert_eq!(sqlserver.quote_ident("Emp_2"), "Emp_2");
        assert_eq!(sqlserver.quote_ident("#tmp"), "#tmp");
        assert_eq!(sqlserver.quote_ident("order"), "[order]");
        assert_eq!(sqlserver.qualified("dbo", "my]table"), "dbo.[my]]table]");
        let sqlite = Dialect::SQLITE;
        assert_eq!(sqlite.quote_ident("Emp_2"), "Emp_2");
        assert_eq!(sqlite.quote_ident("key"), "key");
        assert_eq!(sqlite.quote_ident("order"), "\"order\"");
        assert_eq!(
            sqlite.qualified("main", "my \"table\""),
            "main.\"my \"\"table\"\"\""
        );
    }
}
