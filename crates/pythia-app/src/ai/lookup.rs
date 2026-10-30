//! Dictionary lookups for assistant requests, run as a task on the
//! connection's main session: the tables a statement uses (columns, keys,
//! indexes, statistics), tables matching a description, and plans.
//!
//! Only definitions and statistics are read, never table data: column
//! low/high values and histogram endpoints are left out because they are
//! data.

use pythia_ai::Part;
use pythia_db::oracle::{Connection, ErrorKind};
use pythia_db::{DbError, TaskFn};

/// Tables and views described at most per request.
const MAX_TABLES: usize = 12;
/// Tables found by a description's words, before adding related ones.
const MAX_MATCHES: usize = 8;
/// Names listed in the overview of the other tables of a schema.
const MAX_OVERVIEW: usize = 400;

/// What to look up before asking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lookup {
    /// Tables and views to describe: (owner, name), names as stored. Without
    /// an owner they are resolved the way Oracle does (own object, private
    /// and public synonyms).
    pub tables: Vec<(Option<String>, String)>,
    /// Words of a description, to find matching tables in the current schema.
    pub words: Vec<String>,
    /// Run EXPLAIN PLAN for this statement.
    pub explain: Option<String>,
    /// The plan of this SQL_ID in the cursor cache.
    pub sql_id: Option<String>,
}

impl Lookup {
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
            && self.words.is_empty()
            && self.explain.is_none()
            && self.sql_id.is_none()
    }
}

/// The task answering `lookup` with context parts (`Vec<Part>`).
pub fn task(lookup: Lookup) -> TaskFn {
    Box::new(move |conn: &Connection| {
        Ok(Box::new(run(conn, &lookup)) as Box<dyn std::any::Any + Send>)
    })
}

/// Looks everything up; what can't be read is reported in its part.
fn run(conn: &Connection, lookup: &Lookup) -> Vec<Part> {
    let mut parts = Vec::new();
    let schema = conn
        .query_row_as::<String>(
            "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
            &[],
        )
        .unwrap_or_default();
    let mut tables: Vec<(String, String)> = Vec::new();
    let add = |tables: &mut Vec<(String, String)>, table: (String, String)| {
        if !tables.contains(&table) {
            tables.push(table);
        }
    };
    for (owner, name) in &lookup.tables {
        if let Some(table) = resolve(conn, &schema, owner.as_deref(), name) {
            add(&mut tables, table);
        }
    }
    if !lookup.words.is_empty() {
        match schema_tables(conn, &schema) {
            Ok(all) => {
                let found: Vec<&str> = if all.len() <= MAX_TABLES {
                    all.iter().map(|(name, _)| name.as_str()).collect()
                } else {
                    rank(&lookup.words, &all)
                        .into_iter()
                        .take(MAX_MATCHES)
                        .collect()
                };
                for name in found {
                    add(&mut tables, (schema.clone(), name.to_owned()));
                }
                // Tables joined to the matches, so the model sees the paths.
                for table in tables.clone() {
                    for related in related_tables(conn, &table.0, &table.1).unwrap_or_default() {
                        if tables.len() >= MAX_TABLES {
                            break;
                        }
                        add(&mut tables, related);
                    }
                }
                let others: Vec<&str> = all
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .filter(|name| !tables.iter().any(|(o, n)| *o == schema && n == name))
                    .collect();
                if !others.is_empty() {
                    let mut text = others
                        .iter()
                        .take(MAX_OVERVIEW)
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", ");
                    if others.len() > MAX_OVERVIEW {
                        text.push_str(&format!(" and {} more", others.len() - MAX_OVERVIEW));
                    }
                    parts.push(Part::text(
                        format!("Other tables and views in {schema} (not described)"),
                        text,
                    ));
                }
            }
            Err(e) => parts.push(Part::text(
                format!("Tables of {schema}"),
                format!("Could not list them: {e}"),
            )),
        }
    }
    let mut described = Vec::new();
    for (owner, name) in tables.iter().take(MAX_TABLES) {
        match describe(conn, owner, name) {
            Ok(Some(info)) => described.push(Part::text(
                format!("{} {owner}.{name}", info.kind),
                render(&info),
            )),
            Ok(None) => {}
            Err(e) => described.push(Part::text(
                format!("{owner}.{name}"),
                format!("Could not read its definition: {e}"),
            )),
        }
    }
    // Definitions first: they explain the plans that follow.
    described.append(&mut parts);
    let mut parts = described;
    if let Some(sql) = &lookup.explain {
        parts.push(explain(conn, sql));
    }
    if let Some(sql_id) = &lookup.sql_id {
        parts.push(cursor_plan(conn, sql_id));
    }
    parts
}

/// The table or view `name` refers to, as (owner, name).
fn resolve(
    conn: &Connection,
    schema: &str,
    owner: Option<&str>,
    name: &str,
) -> Option<(String, String)> {
    let exists = |owner: &str, name: &str| {
        conn.query_row_as::<String>(
            &pythia_db::compat::first_rows(
                "select object_name from all_objects \
                  where owner = :1 and object_name = :2 \
                    and object_type in ('TABLE', 'VIEW', 'MATERIALIZED VIEW')",
                1,
            ),
            &[&owner, &name],
        )
        .is_ok()
    };
    let in_schema = owner.unwrap_or(schema);
    if exists(in_schema, name) {
        return Some((in_schema.to_owned(), name.to_owned()));
    }
    if owner.is_some() {
        return None;
    }
    let target = conn
        .query_row_as::<(String, String)>(
            &pythia_db::compat::first_rows(
                "select table_owner, table_name from all_synonyms \
                  where synonym_name = :1 and owner in (:2, 'PUBLIC') and db_link is null \
                  order by decode(owner, 'PUBLIC', 2, 1)",
                1,
            ),
            &[&name, &schema],
        )
        .ok()?;
    exists(&target.0, &target.1).then_some(target)
}

/// Tables and views of a schema with their comments.
fn schema_tables(
    conn: &Connection,
    schema: &str,
) -> Result<Vec<(String, Option<String>)>, DbError> {
    let rows = conn.query_as::<(String, Option<String>)>(
        &pythia_db::compat::first_rows(
            "select table_name, comments from all_tab_comments \
              where owner = :1 and table_type in ('TABLE', 'VIEW') and table_name not like 'BIN$%' \
              order by table_name",
            5000,
        ),
        &[&schema],
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Tables a table references with foreign keys, and tables referencing it.
fn related_tables(
    conn: &Connection,
    owner: &str,
    name: &str,
) -> Result<Vec<(String, String)>, DbError> {
    let rows = conn.query_as::<(String, String)>(
        "select p.owner, p.table_name from all_constraints c \
           join all_constraints p on p.owner = c.r_owner and p.constraint_name = c.r_constraint_name \
          where c.owner = :1 and c.table_name = :2 and c.constraint_type = 'R' \
         union \
         select c.owner, c.table_name from all_constraints c \
           join all_constraints p on p.owner = c.r_owner and p.constraint_name = c.r_constraint_name \
          where p.owner = :1 and p.table_name = :2 and c.constraint_type = 'R'",
        &[&owner, &name],
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Names of tables matching a description's words, best first. Words match
/// name parts (`ORDER_ITEMS` for "order"), singular or plural, and words of
/// the table's comment.
pub fn rank<'a>(words: &[String], tables: &'a [(String, Option<String>)]) -> Vec<&'a str> {
    let words: Vec<String> = words.iter().map(|w| singular(&w.to_lowercase())).collect();
    let mut scored: Vec<(i32, &str)> = tables
        .iter()
        .filter_map(|(name, comment)| {
            let lower = name.to_lowercase();
            let parts: Vec<String> = lower.split('_').map(singular).collect();
            let comment = comment.as_deref().unwrap_or_default().to_lowercase();
            let comment_words: Vec<String> = comment
                .split(|c: char| !c.is_alphanumeric())
                .map(singular)
                .collect();
            let score: i32 = words
                .iter()
                .map(|w| {
                    let prefix = w.len() >= 4
                        && parts.iter().any(|p| {
                            p.len() >= 4 && (p.starts_with(w.as_str()) || w.starts_with(p.as_str()))
                        });
                    if parts.iter().any(|p| p == w) {
                        3
                    } else if prefix || comment_words.iter().any(|c| c == w) {
                        1
                    } else {
                        0
                    }
                })
                .sum();
            (score > 0).then_some((score, name.as_str()))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.len().cmp(&b.1.len()))
            .then(a.1.cmp(b.1))
    });
    scored.into_iter().map(|(_, name)| name).collect()
}

fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies").filter(|s| s.len() >= 2) {
        format!("{stem}y")
    } else if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
        word[..word.len() - 1].to_owned()
    } else {
        word.to_owned()
    }
}

/// What the assistant is told about a table or view.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableInfo {
    /// TABLE, VIEW or MATERIALIZED VIEW.
    pub kind: String,
    pub comment: Option<String>,
    pub num_rows: Option<i64>,
    pub blocks: Option<i64>,
    pub last_analyzed: Option<String>,
    pub partitioned: bool,
    pub columns: Vec<ColumnInfo>,
    /// (type P/U/R, columns, referenced table, referenced columns).
    pub constraints: Vec<(String, String, Option<String>, Option<String>)>,
    pub indexes: Vec<IndexInfo>,
    /// A view's query.
    pub query: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub num_distinct: Option<i64>,
    pub num_nulls: Option<i64>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexInfo {
    pub name: String,
    pub unique: bool,
    pub index_type: String,
    pub columns: String,
    pub valid: bool,
    pub distinct_keys: Option<i64>,
    pub clustering_factor: Option<i64>,
}

fn describe(conn: &Connection, owner: &str, name: &str) -> Result<Option<TableInfo>, DbError> {
    let head = conn.query_row_as::<(String, Option<i64>, Option<i64>, Option<String>, Option<String>, Option<String>)>(
        &pythia_db::compat::first_rows("select o.object_type, t.num_rows, t.blocks, to_char(t.last_analyzed, 'YYYY-MM-DD HH24:MI'), \
                t.partitioned, c.comments \
           from all_objects o \
           left join all_tables t on t.owner = o.owner and t.table_name = o.object_name \
           left join all_tab_comments c on c.owner = o.owner and c.table_name = o.object_name \
          where o.owner = :1 and o.object_name = :2 \
            and o.object_type in ('TABLE', 'VIEW', 'MATERIALIZED VIEW') \
          order by decode(o.object_type, 'MATERIALIZED VIEW', 1, 'TABLE', 2, 3)",
            1,
        ),
        &[&owner, &name],
    );
    let (kind, num_rows, blocks, last_analyzed, partitioned, comment) = match head {
        Ok(head) => head,
        Err(e) if e.kind() == ErrorKind::NoDataFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut info = TableInfo {
        kind,
        comment,
        num_rows,
        blocks,
        last_analyzed,
        partitioned: partitioned.as_deref() == Some("YES"),
        ..Default::default()
    };
    let columns = conn.query_as::<(String, String, String, Option<i64>, Option<i64>, Option<String>)>(
        "select c.column_name, \
                case when c.data_type in ('VARCHAR2', 'NVARCHAR2', 'CHAR', 'NCHAR', 'RAW') \
                       then c.data_type || '(' || decode(c.char_used, 'C', c.char_length || ' CHAR', c.data_length) || ')' \
                     when c.data_type = 'NUMBER' and c.data_precision is not null \
                       then 'NUMBER(' || c.data_precision || nvl2(nullif(c.data_scale, 0), ',' || c.data_scale, '') || ')' \
                     else c.data_type end, \
                c.nullable, c.num_distinct, c.num_nulls, m.comments \
           from all_tab_columns c \
           left join all_col_comments m \
             on m.owner = c.owner and m.table_name = c.table_name and m.column_name = c.column_name \
          where c.owner = :1 and c.table_name = :2 \
          order by c.column_id",
        &[&owner, &name],
    )?;
    for row in columns {
        let (name, data_type, nullable, num_distinct, num_nulls, comment) = row?;
        info.columns.push(ColumnInfo {
            name,
            data_type,
            nullable: nullable == "Y",
            num_distinct,
            num_nulls,
            comment,
        });
    }
    let constraints = conn.query_as::<(String, Option<String>, Option<String>, Option<String>, Option<String>)>(
        "select c.constraint_type, \
                (select listagg(cc.column_name, ', ') within group (order by cc.position) \
                   from all_cons_columns cc where cc.owner = c.owner and cc.constraint_name = c.constraint_name), \
                r.owner, r.table_name, \
                (select listagg(rc.column_name, ', ') within group (order by rc.position) \
                   from all_cons_columns rc where rc.owner = r.owner and rc.constraint_name = r.constraint_name) \
           from all_constraints c \
           left join all_constraints r on r.owner = c.r_owner and r.constraint_name = c.r_constraint_name \
          where c.owner = :1 and c.table_name = :2 and c.constraint_type in ('P', 'U', 'R') \
          order by decode(c.constraint_type, 'P', 1, 'U', 2, 3), c.constraint_name",
        &[&owner, &name],
    )?;
    for row in constraints {
        let (kind, columns, r_owner, r_table, r_columns) = row?;
        let target = r_owner.zip(r_table).map(|(o, t)| format!("{o}.{t}"));
        info.constraints
            .push((kind, columns.unwrap_or_default(), target, r_columns));
    }
    let indexes = conn.query_as::<(String, String, String, String, Option<i64>, Option<i64>, Option<String>)>(
        "select i.index_name, i.uniqueness, i.index_type, i.status, i.distinct_keys, i.clustering_factor, \
                (select listagg(ic.column_name || decode(ic.descend, 'DESC', ' DESC'), ', ') \
                          within group (order by ic.column_position) \
                   from all_ind_columns ic where ic.index_owner = i.owner and ic.index_name = i.index_name) \
           from all_indexes i \
          where i.table_owner = :1 and i.table_name = :2 \
          order by i.index_name",
        &[&owner, &name],
    )?;
    for row in indexes {
        let (name, uniqueness, index_type, status, distinct_keys, clustering_factor, columns) =
            row?;
        info.indexes.push(IndexInfo {
            name,
            unique: uniqueness == "UNIQUE",
            index_type,
            columns: columns.unwrap_or_default(),
            valid: status != "UNUSABLE",
            distinct_keys,
            clustering_factor,
        });
    }
    if info.kind == "VIEW" {
        // TEXT_VC (12c+) holds the first 4000 characters; before, there is
        // only the LONG TEXT.
        let column = if pythia_db::compat::at_least(conn, 12, 1)? {
            "text_vc"
        } else {
            "text"
        };
        info.query = conn
            .query_row_as::<Option<String>>(
                &format!("select {column} from all_views where owner = :1 and view_name = :2"),
                &[&owner, &name],
            )
            .ok()
            .flatten()
            .map(|q| q.chars().take(4000).collect());
    }
    Ok(Some(info))
}

/// A table's definition in a compact, DDL-like form.
pub fn render(info: &TableInfo) -> String {
    let mut out = String::new();
    let mut facts = Vec::new();
    if let Some(rows) = info.num_rows {
        facts.push(format!("{rows} rows"));
    }
    if let Some(blocks) = info.blocks {
        facts.push(format!("{blocks} blocks"));
    }
    if info.partitioned {
        facts.push("partitioned".into());
    }
    match &info.last_analyzed {
        Some(at) => facts.push(format!("statistics from {at}")),
        None if info.kind != "VIEW" => facts.push("no statistics".into()),
        None => {}
    }
    if !facts.is_empty() {
        out.push_str(&facts.join(", "));
        out.push('\n');
    }
    if let Some(comment) = info.comment.as_deref().filter(|c| !c.trim().is_empty()) {
        out.push_str(&format!("Comment: {}\n", comment.trim()));
    }
    out.push_str("Columns:\n");
    for c in &info.columns {
        out.push_str(&format!("  {} {}", c.name, c.data_type));
        if !c.nullable {
            out.push_str(" NOT NULL");
        }
        match (c.num_distinct, c.num_nulls) {
            (Some(d), Some(n)) if n > 0 => out.push_str(&format!("  [{d} distinct, {n} nulls]")),
            (Some(d), _) => out.push_str(&format!("  [{d} distinct]")),
            _ => {}
        }
        if let Some(comment) = c.comment.as_deref().filter(|c| !c.trim().is_empty()) {
            out.push_str(&format!("  -- {}", comment.trim()));
        }
        out.push('\n');
    }
    for (kind, columns, target, target_columns) in &info.constraints {
        match kind.as_str() {
            "P" => out.push_str(&format!("Primary key ({columns})\n")),
            "U" => out.push_str(&format!("Unique ({columns})\n")),
            _ => out.push_str(&format!(
                "Foreign key ({columns}) references {} ({})\n",
                target.as_deref().unwrap_or("?"),
                target_columns.as_deref().unwrap_or_default()
            )),
        }
    }
    if !info.indexes.is_empty() {
        out.push_str("Indexes:\n");
        for i in &info.indexes {
            let unique = if i.unique { "unique " } else { "" };
            let kind = if i.index_type == "NORMAL" {
                String::new()
            } else {
                format!("{} ", i.index_type.to_lowercase())
            };
            out.push_str(&format!("  {} {unique}{kind}({})", i.name, i.columns));
            if let Some(keys) = i.distinct_keys {
                out.push_str(&format!(", {keys} distinct keys"));
            }
            if let Some(cf) = i.clustering_factor {
                out.push_str(&format!(", clustering factor {cf}"));
            }
            if !i.valid {
                out.push_str(", UNUSABLE");
            }
            out.push('\n');
        }
    }
    if let Some(query) = &info.query {
        out.push_str(&format!("Query:\n{}\n", query.trim()));
    }
    out
}

/// EXPLAIN PLAN for `sql`, as DBMS_XPLAN shows it.
fn explain(conn: &Connection, sql: &str) -> Part {
    let id = format!("PYTHIA_AI{}", std::process::id());
    let sql = sql.trim().trim_end_matches(';');
    let plan = (|| -> Result<String, DbError> {
        conn.execute("delete from plan_table where statement_id = :1", &[&id])?;
        conn.execute(
            &format!("explain plan set statement_id = '{id}' for {sql}"),
            &[],
        )?;
        xplan(
            conn,
            "select plan_table_output from table(dbms_xplan.display('PLAN_TABLE', :1, 'TYPICAL'))",
            &id,
        )
    })();
    let _ = conn.execute("delete from plan_table where statement_id = :1", &[&id]);
    let _ = conn.rollback();
    match plan {
        Ok(plan) => Part::text("Execution plan (EXPLAIN PLAN)", plan),
        Err(e) => Part::text("Execution plan", format!("EXPLAIN PLAN failed: {e}")),
    }
}

/// The plan of a cursor in the shared pool, with the actual rows of its
/// last execution if it collected row source statistics.
fn cursor_plan(conn: &Connection, sql_id: &str) -> Part {
    let measured = conn
        .query_row_as::<i64>(
            "select count(*) from v$sql_plan_statistics where sql_id = :1 and rownum = 1",
            &[&sql_id],
        )
        .is_ok_and(|n| n > 0);
    let (title, format) = if measured {
        (
            format!(
                "Execution plan of SQL_ID {sql_id} with the last execution's row source \
                 statistics (cursor cache; E-Rows is per start, A-Rows over all starts)"
            ),
            "ALLSTATS LAST",
        )
    } else {
        (
            format!("Execution plan of SQL_ID {sql_id} (cursor cache)"),
            "TYPICAL",
        )
    };
    match pythia_db::display_cursor(conn, sql_id, None, format) {
        Ok(plan) => Part::text(title, plan.join("\n")),
        Err(e) => Part::text(title, format!("Not available: {e}")),
    }
}

fn xplan(conn: &Connection, sql: &str, id: &str) -> Result<String, DbError> {
    let lines = conn.query_as::<Option<String>>(sql, &[&id])?;
    let lines: Vec<Option<String>> = lines.collect::<Result<_, _>>()?;
    Ok(lines
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_tables_by_description_words() {
        let tables: Vec<(String, Option<String>)> = [
            ("CUSTOMERS", None),
            ("INVOICES", None),
            ("ORDERS", None),
            ("ORDER_ITEMS", None),
            ("PAYMENT_TERMS", Some("Invoice payment conditions")),
            ("LOG_ENTRIES", None),
            ("COUNTRIES", None),
        ]
        .into_iter()
        .map(|(n, c)| (n.to_owned(), c.map(str::to_owned)))
        .collect();
        let words: Vec<String> = ["orders", "last", "month", "invoice"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(
            rank(&words, &tables),
            ["ORDERS", "INVOICES", "ORDER_ITEMS", "PAYMENT_TERMS"]
        );
        let words = vec!["country".to_owned()];
        assert_eq!(rank(&words, &tables), ["COUNTRIES"]);
        assert!(rank(&["xyz".to_owned()], &tables).is_empty());
    }

    #[test]
    fn renders_a_table_compactly() {
        let info = TableInfo {
            kind: "TABLE".into(),
            comment: Some("People working here".into()),
            num_rows: Some(1200),
            blocks: Some(13),
            last_analyzed: Some("2026-10-20 14:03".into()),
            columns: vec![
                ColumnInfo {
                    name: "EMPLOYEE_ID".into(),
                    data_type: "NUMBER(6)".into(),
                    nullable: false,
                    num_distinct: Some(1200),
                    num_nulls: Some(0),
                    comment: None,
                },
                ColumnInfo {
                    name: "SALARY".into(),
                    data_type: "NUMBER(8,2)".into(),
                    nullable: true,
                    num_distinct: Some(1105),
                    num_nulls: Some(3),
                    comment: Some("Monthly salary in CHF".into()),
                },
            ],
            constraints: vec![
                ("P".into(), "EMPLOYEE_ID".into(), None, None),
                (
                    "R".into(),
                    "DEPARTMENT_ID".into(),
                    Some("HR.DEPARTMENTS".into()),
                    Some("DEPARTMENT_ID".into()),
                ),
            ],
            indexes: vec![IndexInfo {
                name: "EMP_PK".into(),
                unique: true,
                index_type: "NORMAL".into(),
                columns: "EMPLOYEE_ID".into(),
                valid: true,
                distinct_keys: Some(1200),
                clustering_factor: Some(10),
            }],
            ..Default::default()
        };
        assert_eq!(
            render(&info),
            "1200 rows, 13 blocks, statistics from 2026-10-20 14:03\n\
             Comment: People working here\n\
             Columns:\n  \
             EMPLOYEE_ID NUMBER(6) NOT NULL  [1200 distinct]\n  \
             SALARY NUMBER(8,2)  [1105 distinct, 3 nulls]  -- Monthly salary in CHF\n\
             Primary key (EMPLOYEE_ID)\n\
             Foreign key (DEPARTMENT_ID) references HR.DEPARTMENTS (DEPARTMENT_ID)\n\
             Indexes:\n  \
             EMP_PK unique (EMPLOYEE_ID), 1200 distinct keys, clustering factor 10\n"
        );
    }

    /// Definitions, a description search and plans from a database
    /// (`PYTHIA_TEST_DB`).
    #[test]
    fn looks_up_a_live_database() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            eprintln!("PYTHIA_TEST_DB not set, skipping");
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        for table in ["pythia_ai_lines", "pythia_ai_orders"] {
            let _ = conn.execute(&format!("drop table {table} purge"), &[]);
        }
        for ddl in [
            "create table pythia_ai_orders (order_id number(10) primary key, \
             customer varchar2(40 char) not null, ordered date)",
            "comment on column pythia_ai_orders.customer is 'Who ordered'",
            "create index pythia_ai_orders_date on pythia_ai_orders (ordered)",
            "create table pythia_ai_lines (order_id number(10) references pythia_ai_orders, \
             line_no number(3), amount number(12,2), primary key (order_id, line_no))",
        ] {
            conn.execute(ddl, &[]).unwrap();
        }
        let schema: String = conn
            .query_row_as(
                "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
                &[],
            )
            .unwrap();
        let parts = run(
            &conn,
            &Lookup {
                tables: vec![
                    (None, "PYTHIA_AI_LINES".into()),
                    (None, "PYTHIA_AI_NO_SUCH_TABLE".into()),
                ],
                explain: Some("select * from pythia_ai_orders where ordered > sysdate - 1;".into()),
                ..Default::default()
            },
        );
        let titles: Vec<&str> = parts.iter().map(|p| p.title.as_str()).collect();
        let lines = format!("TABLE {schema}.PYTHIA_AI_LINES");
        assert_eq!(titles, [lines.as_str(), "Execution plan (EXPLAIN PLAN)"]);
        assert!(
            parts[0].text.contains("Primary key (ORDER_ID, LINE_NO)"),
            "{}",
            parts[0].text
        );
        assert!(
            parts[0].text.contains(&format!(
                "Foreign key (ORDER_ID) references {schema}.PYTHIA_AI_ORDERS (ORDER_ID)"
            )),
            "{}",
            parts[0].text
        );
        assert!(
            parts[1].text.contains("PYTHIA_AI_ORDERS"),
            "{}",
            parts[1].text
        );

        // A description finds the table; its definition has comments and indexes.
        let parts = run(
            &conn,
            &Lookup {
                words: vec!["pythia_ai_orders".into(), "orders".into()],
                explain: Some("select * from pythia_ai_no_such_table".into()),
                ..Default::default()
            },
        );
        let orders = parts
            .iter()
            .find(|p| p.title == format!("TABLE {schema}.PYTHIA_AI_ORDERS"))
            .expect("orders described");
        assert!(
            orders
                .text
                .contains("CUSTOMER VARCHAR2(40 CHAR) NOT NULL  -- Who ordered"),
            "{}",
            orders.text
        );
        assert!(
            orders.text.contains("PYTHIA_AI_ORDERS_DATE (ORDERED)"),
            "{}",
            orders.text
        );
        let plan = parts.last().unwrap();
        assert!(plan.text.contains("ORA-00942"), "{}", plan.text);

        for table in ["pythia_ai_lines", "pythia_ai_orders"] {
            conn.execute(&format!("drop table {table} purge"), &[])
                .unwrap();
        }
    }

    #[test]
    fn empty_lookup() {
        assert!(Lookup::default().is_empty());
        let lookup = Lookup {
            sql_id: Some("abc".into()),
            ..Default::default()
        };
        assert!(!lookup.is_empty());
    }
}
