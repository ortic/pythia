//! What the focused window contributes to an assistant request: the
//! statement or selection, errors, plans, and (labeled as such, for the data
//! policy) bind values and result rows.

use std::ops::Range;

use pythia_ai::{Part, Task};
use pythia_plsql::ast::SourceKind;

use super::{Lookup, Origin};
use crate::documents::DocId;
use crate::program_window::ProgramWindow;
use crate::sql_window::{BottomTab, Level, SqlWindow};

/// Tables looked up per statement at most.
const MAX_TABLE_REFS: usize = 12;
/// Result rows offered to the model (sent only if allowed).
const MAX_RESULT_ROWS: usize = 20;

/// Context from a window, and where answers can go back to.
#[derive(Default)]
pub struct Snapshot {
    pub parts: Vec<Part>,
    pub lookup: Lookup,
    pub origin: Option<Origin>,
    /// Where the context came from, e.g. "SQL 1 · statement at line 3".
    pub source: String,
}

impl Snapshot {
    /// Has a statement, selection or source to work on.
    pub fn has_code(&self) -> bool {
        self.origin.as_ref().is_some_and(|o| o.target.is_some())
            || self.parts.iter().any(|p| p.code)
    }
}

/// The database a request is about.
pub fn connection_part(server: &pythia_db::ServerInfo) -> Part {
    Part::text(
        "Database",
        format!(
            "{}\nCurrent schema: {}",
            server.banner.trim(),
            server.current_schema
        ),
    )
}

pub fn sql_window(w: &SqlWindow, doc: DocId, task: &Task) -> Snapshot {
    let mut s = Snapshot {
        source: w.title.clone(),
        origin: Some(Origin {
            doc,
            part: 0,
            target: None,
        }),
        ..Default::default()
    };
    if let Task::Generate(description) = task {
        s.lookup.words = words(description);
        s.lookup.tables = table_refs(w.editor.text());
        return s;
    }
    let Some((range, text, is_sql)) = sql_statement(w) else {
        return s;
    };
    let selected = selection(&w.editor).is_some();
    s.source = format!(
        "{} · {} at line {}",
        w.title,
        if selected { "selection" } else { "statement" },
        line_of(w.editor.text(), range.start)
    );
    let substituted = if w.substitution {
        crate::sql_window::substitute(&text, &w.defines)
    } else {
        text.clone()
    };
    s.parts.push(Part::code(
        if selected {
            "Selected code"
        } else {
            "Statement"
        },
        text.clone(),
    ));
    let problems: Vec<&str> = w
        .state
        .messages
        .iter()
        .filter(|m| m.level != Level::Info)
        .map(|m| m.text.trim())
        .collect();
    if !problems.is_empty() && *task != Task::AnalyzePlan {
        s.parts.push(Part::text(
            "Errors and warnings of the last run",
            problems.join("\n"),
        ));
    }
    if *task == Task::AnalyzePlan && is_sql {
        s.lookup.explain = Some(substituted.clone());
    }
    // A plan or statistics report counts when the window shows it.
    match w.state.bottom {
        BottomTab::Plan if *task != Task::AnalyzePlan => {
            if let Some(Ok(rows)) = w.state.plan.as_ref().and_then(|p| p.result.as_ref()) {
                s.parts.push(Part::text(
                    "Execution plan shown in the window",
                    pythia_ui::plan::text(rows),
                ));
            }
        }
        BottomTab::Statistics => {
            if let Some((_, Some(Ok(report)))) = &w.state.statistics {
                s.parts.extend(statistics_parts(report));
            }
        }
        _ => {}
    }
    let binds = bind_values(w, &text);
    if !binds.is_empty() {
        s.parts
            .push(Part::bind_values("Bind variable values", binds));
    }
    if let Some(result) = w
        .state
        .results
        .iter()
        .find(|r| r.sql.trim() == substituted.trim())
    {
        s.parts
            .extend(result_parts(&result.columns, &result.rows, result.more));
    }
    s.lookup.tables = table_refs(&text);
    s.origin = Some(Origin {
        doc,
        part: 0,
        target: Some((range, text)),
    });
    s
}

/// The selection, else the statement at the cursor: its range, text and
/// whether it is plain SQL (EXPLAIN PLAN works for it).
fn sql_statement(w: &SqlWindow) -> Option<(Range<usize>, String, bool)> {
    let text = w.editor.text();
    if let Some(range) = selection(&w.editor) {
        let selected = text[range.clone()].to_owned();
        let is_sql = is_sql(&selected);
        return Some((range, selected, is_sql));
    }
    let stmt = pythia_plsql::statement_at(text, w.editor.cursor())?;
    let is_sql = stmt.kind == pythia_plsql::StatementKind::Sql;
    Some((stmt.range.clone(), stmt.text(text).to_owned(), is_sql))
}

/// The selection, unless it is just a marker (errors select one character).
fn selection(editor: &crate::editor::Editor) -> Option<Range<usize>> {
    editor
        .selection()
        .filter(|r| editor.text()[r.clone()].trim().chars().count() >= 2)
}

/// Starts like a statement EXPLAIN PLAN accepts.
fn is_sql(text: &str) -> bool {
    let first = pythia_plsql::tokenize(text)
        .into_iter()
        .find(|t| !t.kind.is_trivia())
        .map(|t| text[t.span].to_ascii_uppercase());
    matches!(
        first.as_deref(),
        Some("SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "(")
    )
}

/// Values remembered for the statement's bind variables.
fn bind_values(w: &SqlWindow, sql: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for name in crate::sql_window::find_binds(sql) {
        let line = match w.bind_values.get(&name.to_lowercase()) {
            Some(Some(value)) => format!(":{name} = '{}'", value.replace('\'', "''")),
            Some(None) => format!(":{name} = NULL"),
            None => continue,
        };
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// A result's columns (metadata) and first rows (result data).
fn result_parts(
    columns: &[pythia_db::Column],
    rows: &[Vec<pythia_db::Cell>],
    more: bool,
) -> Vec<Part> {
    let more = if more { "+" } else { "" };
    let described: Vec<String> = columns
        .iter()
        .map(|c| format!("{} {}", c.name, c.type_name))
        .collect();
    let mut parts = vec![Part::text(
        "Result columns",
        format!(
            "{}\n({}{more} rows fetched)",
            described.join(", "),
            rows.len()
        ),
    )];
    if !rows.is_empty() {
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        let mut text = names.join(" | ");
        for row in rows.iter().take(MAX_RESULT_ROWS) {
            let cells: Vec<String> = row
                .iter()
                .map(|cell| match cell {
                    Some(v) if v.chars().count() > 200 => {
                        format!("{}…", v.chars().take(200).collect::<String>())
                    }
                    Some(v) => v.clone(),
                    None => "NULL".into(),
                })
                .collect();
            text.push('\n');
            text.push_str(&cells.join(" | "));
        }
        let shown = rows.len().min(MAX_RESULT_ROWS);
        parts.push(Part::result_data(
            format!("Result rows (first {shown})"),
            text,
        ));
    }
    parts
}

/// The actual plan and session statistics of "Run with statistics".
fn statistics_parts(report: &crate::statistics::Report) -> Vec<Part> {
    let mut parts = Vec::new();
    if let Ok(plan) = &report.plan {
        parts.push(Part::text(
            "Actual execution plan (run with statistics)",
            plan.clone(),
        ));
    }
    let mut text = format!(
        "{} rows fetched in {}",
        report.rows,
        crate::sql_window::fmt_duration(report.elapsed)
    );
    if let Ok(stats) = &report.stats {
        for (name, value) in stats {
            text.push_str(&format!("\n{name}: {value}"));
        }
    }
    parts.push(Part::text("Session statistics of the run", text));
    parts
}

pub fn program_window(w: &ProgramWindow, doc: DocId, task: &Task) -> Snapshot {
    let Some(part) = w.parts.get(w.active) else {
        return Snapshot::default();
    };
    let unit = format!(
        "{} {}.{}",
        part.object_type.dictionary_name(),
        w.object.owner,
        w.object.name
    );
    let mut s = Snapshot {
        source: unit.clone(),
        origin: Some(Origin {
            doc,
            part: w.active,
            target: None,
        }),
        ..Default::default()
    };
    let text = part.editor.text();
    if let Task::Generate(description) = task {
        s.parts.push(Part::text("Written in", unit));
        s.lookup.words = words(description);
        s.lookup.tables = table_refs(text);
        return s;
    }
    let selection = selection(&part.editor);
    let code = match &selection {
        Some(range) => {
            s.source = format!("{unit} · selection at line {}", line_of(text, range.start));
            s.parts.push(Part::code(
                format!("Selected code of {unit}"),
                &text[range.clone()],
            ));
            text[range.clone()].to_owned()
        }
        None => {
            s.parts.push(Part::code(format!("Source of {unit}"), text));
            text.to_owned()
        }
    };
    if !part.errors.is_empty() && *task != Task::AnalyzePlan {
        let errors: Vec<String> = part
            .errors
            .iter()
            .map(|e| {
                let kind = if e.warning { "warning" } else { "error" };
                format!(
                    "line {}, column {} ({kind}): {}",
                    e.line, e.position, e.text
                )
            })
            .collect();
        s.parts.push(Part::text(
            format!("Compile errors of {unit} (lines of the whole source)"),
            errors.join("\n"),
        ));
    }
    if *task == Task::AnalyzePlan && selection.is_some() && is_sql(&code) {
        s.lookup.explain = Some(code.clone());
    }
    s.lookup.tables = table_refs(&code);
    if let (Some(range), Some(origin)) = (selection, s.origin.as_mut()) {
        origin.target = Some((range, code));
    }
    s
}

/// Tables and views a statement or PL/SQL source reads or writes: (owner,
/// name) as stored, without CTEs and DUAL.
pub fn table_refs(sql: &str) -> Vec<(Option<String>, String)> {
    let ast = pythia_plsql::parse(sql);
    let ctes: Vec<&str> = ast
        .queries
        .iter()
        .flat_map(|q| q.ctes.iter().map(|c| c.name.name.as_str()))
        .collect();
    let mut found: Vec<(usize, (Option<String>, String))> = Vec::new();
    for query in &ast.queries {
        for source in &query.sources {
            let SourceKind::Table(path) = &source.kind else {
                continue;
            };
            let table = match path.as_slice() {
                [name] if !ctes.contains(&name.name.as_str()) && name.name != "DUAL" => {
                    (None, name.name.clone())
                }
                [owner, name] => (Some(owner.name.clone()), name.name.clone()),
                _ => continue,
            };
            found.push((source.span.start, table));
        }
    }
    // In the order they're written.
    found.sort_by_key(|(at, _)| *at);
    let mut out = Vec::new();
    for (_, table) in found {
        if !out.contains(&table) && out.len() < MAX_TABLE_REFS {
            out.push(table);
        }
    }
    out
}

/// Words of a description that may name tables.
pub fn words(description: &str) -> Vec<String> {
    const SKIP: &[&str] = &[
        "the", "and", "for", "from", "with", "without", "that", "which", "who", "whose", "what",
        "where", "when", "how", "many", "much", "all", "any", "each", "every", "per", "into",
        "than", "then", "are", "was", "were", "have", "has", "had", "not", "but", "only", "show",
        "list", "get", "find", "give", "select", "query", "sql", "their", "there", "them", "they",
        "this", "these", "those", "its", "our", "your",
    ];
    let mut out: Vec<String> = Vec::new();
    for word in description
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map(str::to_lowercase)
    {
        if word.chars().count() >= 3 && !SKIP.contains(&word.as_str()) && !out.contains(&word) {
            out.push(word);
        }
    }
    out
}

/// 1-based line of a byte offset.
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

#[cfg(test)]
mod tests {
    use pythia_ai::Sensitivity;

    use super::*;

    fn refs(sql: &str) -> Vec<String> {
        table_refs(sql)
            .into_iter()
            .map(|(o, n)| match o {
                Some(o) => format!("{o}.{n}"),
                None => n,
            })
            .collect()
    }

    #[test]
    fn tables_of_statements() {
        assert_eq!(
            refs(
                "with recent as (select * from orders where order_date > sysdate - 30)\n\
                 select c.name, r.total from recent r join sales.customers c on c.id = r.customer_id, dual"
            ),
            ["ORDERS", "SALES.CUSTOMERS"]
        );
        assert_eq!(
            refs("update emp set sal = sal * 1.1 where deptno in (select deptno from \"Dept\")"),
            ["EMP", "Dept"]
        );
        assert_eq!(
            refs("begin\n  insert into audit_log (msg) values ('x');\n  delete from t1;\nend;"),
            ["AUDIT_LOG", "T1"]
        );
    }

    #[test]
    fn description_words() {
        assert_eq!(
            words("Orders from last month without an invoice, per customer_id"),
            ["orders", "last", "month", "invoice", "customer_id"]
        );
    }

    fn window(text: &str) -> SqlWindow {
        let mut w = SqlWindow::new("SQL 1".into(), text.into());
        w.editor
            .select(text.find("select").unwrap()..text.find("select").unwrap());
        w
    }

    #[test]
    fn statement_with_binds_and_result() {
        let text = "-- first\nselect ename from emp where empno = :id;\nselect 2 from dual;\n";
        let mut w = window(text);
        w.bind_values.insert("id".into(), Some("7839".into()));
        let sql = "select ename from emp where empno = :id";
        w.state.results.push(crate::sql_window::QueryResult {
            id: 1,
            title: "Result 1".into(),
            sql: sql.into(),
            columns: vec![pythia_db::Column {
                name: "ENAME".into(),
                type_name: "VARCHAR2".into(),
                numeric: false,
            }],
            rows: vec![vec![Some("KING".into())]],
            more: false,
            elapsed: Default::default(),
            grid: Default::default(),
            edit: None,
            filter: String::new(),
            chart: None,
            relations: Vec::new(),
            link_relations: Vec::new(),
        });
        let s = sql_window(&w, DocId(7), &Task::Explain);
        assert_eq!(s.source, "SQL 1 · statement at line 2");
        let origin = s.origin.unwrap();
        let (range, target) = origin.target.unwrap();
        assert_eq!(target, sql);
        assert_eq!(&text[range], sql);
        let kinds: Vec<(&str, Sensitivity)> = s
            .parts
            .iter()
            .map(|p| (p.title.as_str(), p.sensitivity))
            .collect();
        assert_eq!(
            kinds,
            [
                ("Statement", Sensitivity::Metadata),
                ("Bind variable values", Sensitivity::BindValues),
                ("Result columns", Sensitivity::Metadata),
                ("Result rows (first 1)", Sensitivity::ResultData),
            ]
        );
        // Values only ever travel in the parts labeled as data.
        for part in s
            .parts
            .iter()
            .filter(|p| p.sensitivity == Sensitivity::Metadata)
        {
            assert!(!part.text.contains("7839") && !part.text.contains("KING"));
        }
        assert_eq!(s.parts[1].text, ":id = '7839'");
        assert_eq!(s.lookup.tables, [(None, "EMP".to_owned())]);
        assert_eq!(s.lookup.explain, None);

        let plan = sql_window(&w, DocId(7), &Task::AnalyzePlan);
        assert_eq!(plan.lookup.explain.as_deref(), Some(sql));
    }

    #[test]
    fn generate_looks_up_the_schema() {
        let w = window("select * from orders;");
        let s = sql_window(
            &w,
            DocId(1),
            &Task::Generate("customers with orders".into()),
        );
        assert!(s.parts.is_empty());
        assert_eq!(s.lookup.words, ["customers", "orders"]);
        assert_eq!(s.lookup.tables, [(None, "ORDERS".to_owned())]);
        assert!(s.origin.unwrap().target.is_none());
    }
}
