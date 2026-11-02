//! Built-in knowledge: SQL functions with signatures, data types, keywords
//! per context and snippets.

/// A built-in SQL / PL/SQL function. `params` are display strings; `[x]`
/// marks optional parameters.
#[derive(Debug, Clone, Copy)]
pub struct Builtin {
    pub name: &'static str,
    pub params: &'static [&'static str],
    pub returns: &'static str,
    pub doc: &'static str,
}

macro_rules! b {
    ($name:literal, [$($p:literal),*], $ret:literal, $doc:literal) => {
        Builtin { name: $name, params: &[$($p),*], returns: $ret, doc: $doc }
    };
}

pub const FUNCTIONS: &[Builtin] = &[
    b!("ABS", ["n"], "NUMBER", "Absolute value."),
    b!(
        "ADD_MONTHS",
        ["date", "months"],
        "DATE",
        "Date plus a number of months."
    ),
    b!("ASCII", ["char"], "NUMBER", "Code of the first character."),
    b!("AVG", ["expr"], "NUMBER", "Average (aggregate / analytic)."),
    b!(
        "CAST",
        ["expr AS type"],
        "type",
        "Converts to another data type."
    ),
    b!("CEIL", ["n"], "NUMBER", "Smallest integer >= n."),
    b!("CHR", ["n"], "VARCHAR2", "Character with the given code."),
    b!(
        "COALESCE",
        ["expr1", "expr2", "[expr, ...]"],
        "same as args",
        "First non-null expression."
    ),
    b!(
        "CONCAT",
        ["char1", "char2"],
        "VARCHAR2",
        "Concatenates two strings."
    ),
    b!(
        "COUNT",
        ["expr | *"],
        "NUMBER",
        "Number of rows / non-null values."
    ),
    b!(
        "CURRENT_DATE",
        [],
        "DATE",
        "Current date in the session time zone."
    ),
    b!(
        "CURRENT_TIMESTAMP",
        [],
        "TIMESTAMP WITH TIME ZONE",
        "Current timestamp in the session time zone."
    ),
    b!(
        "DECODE",
        [
            "expr",
            "search",
            "result",
            "[search, result, ...]",
            "[default]"
        ],
        "same as result",
        "Compares expr to each search value."
    ),
    b!("DENSE_RANK", [], "NUMBER", "Rank without gaps (analytic)."),
    b!(
        "EXTRACT",
        ["field FROM datetime"],
        "NUMBER",
        "Extracts a datetime field."
    ),
    b!(
        "FIRST_VALUE",
        ["expr"],
        "same as expr",
        "First value in the window (analytic)."
    ),
    b!("FLOOR", ["n"], "NUMBER", "Largest integer <= n."),
    b!(
        "GREATEST",
        ["expr", "[expr, ...]"],
        "same as args",
        "Largest value."
    ),
    b!(
        "INITCAP",
        ["char"],
        "VARCHAR2",
        "Capitalizes the first letter of each word."
    ),
    b!(
        "INSTR",
        ["string", "substring", "[position]", "[occurrence]"],
        "NUMBER",
        "Position of a substring."
    ),
    b!(
        "JSON_OBJECT",
        ["key VALUE expr, ..."],
        "VARCHAR2",
        "Builds a JSON object."
    ),
    b!(
        "JSON_QUERY",
        ["expr", "path"],
        "VARCHAR2",
        "Selects JSON data."
    ),
    b!(
        "JSON_VALUE",
        ["expr", "path"],
        "VARCHAR2",
        "Selects a scalar JSON value."
    ),
    b!(
        "LAG",
        ["expr", "[offset]", "[default]"],
        "same as expr",
        "Value of a previous row (analytic)."
    ),
    b!("LAST_DAY", ["date"], "DATE", "Last day of the month."),
    b!(
        "LAST_VALUE",
        ["expr"],
        "same as expr",
        "Last value in the window (analytic)."
    ),
    b!(
        "LEAD",
        ["expr", "[offset]", "[default]"],
        "same as expr",
        "Value of a following row (analytic)."
    ),
    b!(
        "LEAST",
        ["expr", "[expr, ...]"],
        "same as args",
        "Smallest value."
    ),
    b!("LENGTH", ["char"], "NUMBER", "Length in characters."),
    b!(
        "LISTAGG",
        ["expr", "[delimiter]"],
        "VARCHAR2",
        "Concatenates values of a group."
    ),
    b!("LOWER", ["char"], "VARCHAR2", "Lowercase."),
    b!(
        "LPAD",
        ["expr", "length", "[pad]"],
        "VARCHAR2",
        "Left-pads to a length."
    ),
    b!(
        "LTRIM",
        ["char", "[set]"],
        "VARCHAR2",
        "Removes leading characters."
    ),
    b!(
        "MAX",
        ["expr"],
        "same as expr",
        "Maximum (aggregate / analytic)."
    ),
    b!(
        "MIN",
        ["expr"],
        "same as expr",
        "Minimum (aggregate / analytic)."
    ),
    b!("MOD", ["n2", "n1"], "NUMBER", "Remainder of n2 / n1."),
    b!(
        "MONTHS_BETWEEN",
        ["date1", "date2"],
        "NUMBER",
        "Months between two dates."
    ),
    b!(
        "NEXT_DAY",
        ["date", "weekday"],
        "DATE",
        "Next given weekday after date."
    ),
    b!(
        "NULLIF",
        ["expr1", "expr2"],
        "same as expr1",
        "NULL if both are equal, else expr1."
    ),
    b!(
        "NVL",
        ["expr1", "expr2"],
        "same as expr1",
        "expr2 if expr1 is NULL."
    ),
    b!(
        "NVL2",
        ["expr1", "expr2", "expr3"],
        "same as expr2",
        "expr2 if expr1 is not NULL, else expr3."
    ),
    b!("RANK", [], "NUMBER", "Rank with gaps (analytic)."),
    b!(
        "RAISE_APPLICATION_ERROR",
        ["error_number", "message", "[keep_errors]"],
        "",
        "Raises a user-defined error (-20000..-20999)."
    ),
    b!(
        "REGEXP_COUNT",
        ["source", "pattern", "[position]", "[match_param]"],
        "NUMBER",
        "Number of pattern matches."
    ),
    b!(
        "REGEXP_INSTR",
        [
            "source",
            "pattern",
            "[position]",
            "[occurrence]",
            "[return_opt]",
            "[match_param]"
        ],
        "NUMBER",
        "Position of a pattern match."
    ),
    b!(
        "REGEXP_LIKE",
        ["source", "pattern", "[match_param]"],
        "BOOLEAN",
        "Condition: source matches pattern."
    ),
    b!(
        "REGEXP_REPLACE",
        [
            "source",
            "pattern",
            "[replace]",
            "[position]",
            "[occurrence]",
            "[match_param]"
        ],
        "VARCHAR2",
        "Replaces pattern matches."
    ),
    b!(
        "REGEXP_SUBSTR",
        [
            "source",
            "pattern",
            "[position]",
            "[occurrence]",
            "[match_param]"
        ],
        "VARCHAR2",
        "Substring matching a pattern."
    ),
    b!(
        "REPLACE",
        ["char", "search", "[replacement]"],
        "VARCHAR2",
        "Replaces occurrences of a string."
    ),
    b!(
        "ROUND",
        ["n | date", "[precision | fmt]"],
        "NUMBER | DATE",
        "Rounds a number or date."
    ),
    b!(
        "ROW_NUMBER",
        [],
        "NUMBER",
        "Sequential row number (analytic)."
    ),
    b!(
        "RPAD",
        ["expr", "length", "[pad]"],
        "VARCHAR2",
        "Right-pads to a length."
    ),
    b!(
        "RTRIM",
        ["char", "[set]"],
        "VARCHAR2",
        "Removes trailing characters."
    ),
    b!("SIGN", ["n"], "NUMBER", "-1, 0 or 1."),
    b!(
        "SQLCODE",
        [],
        "NUMBER",
        "Error code of the current exception."
    ),
    b!(
        "SQLERRM",
        ["[code]"],
        "VARCHAR2",
        "Error message of the current exception."
    ),
    b!(
        "SUBSTR",
        ["char", "position", "[length]"],
        "VARCHAR2",
        "Substring."
    ),
    b!("SUM", ["expr"], "NUMBER", "Sum (aggregate / analytic)."),
    b!(
        "SYSDATE",
        [],
        "DATE",
        "Current date and time of the database server."
    ),
    b!(
        "SYSTIMESTAMP",
        [],
        "TIMESTAMP WITH TIME ZONE",
        "Current timestamp of the database server."
    ),
    b!(
        "SYS_CONTEXT",
        ["namespace", "parameter", "[length]"],
        "VARCHAR2",
        "Value of a context attribute, e.g. SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')."
    ),
    b!("SYS_GUID", [], "RAW", "Globally unique identifier."),
    b!(
        "TO_CHAR",
        ["expr", "[fmt]", "[nlsparam]"],
        "VARCHAR2",
        "Converts a number or date to text."
    ),
    b!(
        "TO_DATE",
        ["char", "[fmt]", "[nlsparam]"],
        "DATE",
        "Converts text to a date."
    ),
    b!(
        "TO_NUMBER",
        ["expr", "[fmt]", "[nlsparam]"],
        "NUMBER",
        "Converts text to a number."
    ),
    b!(
        "TO_TIMESTAMP",
        ["char", "[fmt]", "[nlsparam]"],
        "TIMESTAMP",
        "Converts text to a timestamp."
    ),
    b!(
        "TRANSLATE",
        ["expr", "from_string", "to_string"],
        "VARCHAR2",
        "Replaces characters one by one."
    ),
    b!(
        "TRIM",
        ["[LEADING | TRAILING | BOTH] [char FROM] source"],
        "VARCHAR2",
        "Removes leading/trailing characters."
    ),
    b!(
        "TRUNC",
        ["n | date", "[precision | fmt]"],
        "NUMBER | DATE",
        "Truncates a number or date."
    ),
    b!("UPPER", ["char"], "VARCHAR2", "Uppercase."),
    b!("USER", [], "VARCHAR2", "Name of the session user."),
    b!(
        "XMLAGG",
        ["XMLType"],
        "XMLType",
        "Aggregates XML fragments."
    ),
];

pub fn function(name: &str) -> Option<&'static Builtin> {
    FUNCTIONS.iter().find(|f| f.name.eq_ignore_ascii_case(name))
}

/// Functions called without parentheses.
const NO_PARENS: &[&str] = &[
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "SQLCODE",
    "SYSDATE",
    "SYSTIMESTAMP",
    "USER",
];

impl Builtin {
    pub fn takes_parens(&self) -> bool {
        !NO_PARENS.contains(&self.name)
    }

    /// `NAME(a, b) → t`
    pub fn signature(&self) -> String {
        let ret = if self.returns.is_empty() {
            String::new()
        } else {
            format!(" → {}", self.returns)
        };
        if self.takes_parens() {
            format!("{}({}){ret}", self.name, self.params.join(", "))
        } else {
            format!("{}{ret}", self.name)
        }
    }
}

pub const DATA_TYPES: &[&str] = &[
    "BINARY_DOUBLE",
    "BINARY_FLOAT",
    "BINARY_INTEGER",
    "BLOB",
    "BOOLEAN",
    "CHAR",
    "CLOB",
    "DATE",
    "INTEGER",
    "INTERVAL DAY TO SECOND",
    "INTERVAL YEAR TO MONTH",
    "JSON",
    "LONG",
    "NATURAL",
    "NCHAR",
    "NCLOB",
    "NUMBER",
    "NVARCHAR2",
    "PLS_INTEGER",
    "POSITIVE",
    "RAW",
    "ROWID",
    "SIMPLE_INTEGER",
    "SYS_REFCURSOR",
    "TIMESTAMP",
    "TIMESTAMP WITH TIME ZONE",
    "TIMESTAMP WITH LOCAL TIME ZONE",
    "VARCHAR2",
    "XMLTYPE",
];

/// Keywords that start a SQL statement (SQL window).
pub const STATEMENT_KEYWORDS: &[&str] = &[
    "SELECT",
    "WITH",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "CREATE",
    "ALTER",
    "DROP",
    "TRUNCATE",
    "COMMENT",
    "GRANT",
    "REVOKE",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "DECLARE",
    "BEGIN",
    "EXPLAIN PLAN FOR",
    "CALL",
    "LOCK TABLE",
];

/// Keywords that start a PL/SQL statement.
pub const PLSQL_STATEMENT_KEYWORDS: &[&str] = &[
    "IF",
    "CASE",
    "LOOP",
    "FOR",
    "WHILE",
    "FORALL",
    "BEGIN",
    "DECLARE",
    "RETURN",
    "RAISE",
    "NULL",
    "EXIT",
    "CONTINUE",
    "GOTO",
    "OPEN",
    "FETCH",
    "CLOSE",
    "EXECUTE IMMEDIATE",
    "PIPE ROW",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "END",
    "ELSE",
    "ELSIF",
    "EXCEPTION",
    "WHEN",
    "LOCK TABLE",
];

/// Keywords starting a declaration.
pub const DECLARATION_KEYWORDS: &[&str] = &[
    "PROCEDURE",
    "FUNCTION",
    "CURSOR",
    "TYPE",
    "SUBTYPE",
    "PRAGMA",
    "BEGIN",
    "CONSTANT",
    "EXCEPTION",
];

/// Keywords valid inside expressions.
pub const EXPRESSION_KEYWORDS: &[&str] = &[
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "BETWEEN",
    "EXISTS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "TRUE",
    "FALSE",
    "DISTINCT",
    "PRIOR",
    "LEVEL",
    "ROWNUM",
    "ROWID",
    "SYSDATE",
    "SYSTIMESTAMP",
    "USER",
];

/// Keywords after an expression in a query, per clause.
pub fn after_expression(clause: Option<pythia_plsql::ast::Clause>) -> &'static [&'static str] {
    use pythia_plsql::ast::Clause::*;
    match clause {
        Some(Select) => &["FROM", "AS", "INTO", "BULK COLLECT INTO", "OVER", "KEEP"],
        Some(From) => &[
            "WHERE",
            "JOIN",
            "INNER JOIN",
            "LEFT JOIN",
            "LEFT OUTER JOIN",
            "RIGHT JOIN",
            "FULL OUTER JOIN",
            "CROSS JOIN",
            "NATURAL JOIN",
            "ON",
            "USING",
            "GROUP BY",
            "ORDER BY",
            "CONNECT BY",
            "START WITH",
            "UNION",
            "UNION ALL",
            "MINUS",
            "INTERSECT",
            "FETCH FIRST",
            "FOR UPDATE",
            "PIVOT",
            "UNPIVOT",
        ],
        Some(Where) | Some(Hierarchy) | Some(Having) | Some(On) => &[
            "AND",
            "OR",
            "NOT",
            "IS NULL",
            "IS NOT NULL",
            "IN",
            "LIKE",
            "BETWEEN",
            "GROUP BY",
            "ORDER BY",
            "CONNECT BY",
            "START WITH",
            "UNION",
            "UNION ALL",
            "MINUS",
            "INTERSECT",
            "FETCH FIRST",
            "FOR UPDATE",
        ],
        Some(GroupBy) => &[
            "HAVING",
            "ORDER BY",
            "ROLLUP",
            "CUBE",
            "GROUPING SETS",
            "UNION",
            "UNION ALL",
        ],
        Some(OrderBy) => &[
            "ASC",
            "DESC",
            "NULLS FIRST",
            "NULLS LAST",
            "FETCH FIRST",
            "OFFSET",
            "FOR UPDATE",
        ],
        Some(Set) => &["WHERE", "RETURNING"],
        Some(Into) => &["FROM", "VALUES", "SELECT"],
        Some(Values) => &["RETURNING"],
        _ => &["AND", "OR", "NOT", "IS", "IN", "LIKE", "BETWEEN"],
    }
}

/// Snippets offered at the start of a statement: (label, text, caret offset).
/// The [`SNIPPETS`] of PL/SQL, offered on Oracle only (#469).
pub const PLSQL_SNIPPETS: &[&str] = &["beg", "dec", "proc", "func", "pkg", "forr"];

pub const SNIPPETS: &[(&str, &str, &str)] = &[
    ("sel", "SELECT * FROM |", "SELECT * FROM table"),
    (
        "selw",
        "SELECT * FROM | WHERE ",
        "SELECT * FROM table WHERE ...",
    ),
    (
        "ins",
        "INSERT INTO | () VALUES ();",
        "INSERT INTO table (...) VALUES (...)",
    ),
    (
        "upd",
        "UPDATE | SET  WHERE ;",
        "UPDATE table SET ... WHERE ...",
    ),
    (
        "del",
        "DELETE FROM | WHERE ;",
        "DELETE FROM table WHERE ...",
    ),
    ("beg", "BEGIN\n  |\nEND;", "BEGIN ... END block"),
    (
        "dec",
        "DECLARE\n  |\nBEGIN\n  NULL;\nEND;",
        "DECLARE ... BEGIN ... END block",
    ),
    (
        "proc",
        "CREATE OR REPLACE PROCEDURE | IS\nBEGIN\n  NULL;\nEND;",
        "CREATE PROCEDURE",
    ),
    (
        "func",
        "CREATE OR REPLACE FUNCTION | RETURN NUMBER IS\nBEGIN\n  RETURN NULL;\nEND;",
        "CREATE FUNCTION",
    ),
    (
        "pkg",
        "CREATE OR REPLACE PACKAGE | AS\nEND;",
        "CREATE PACKAGE",
    ),
    (
        "forr",
        "FOR r IN (|) LOOP\n  NULL;\nEND LOOP;",
        "Cursor FOR loop",
    ),
];
