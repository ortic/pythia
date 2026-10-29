//! "Run with statistics": executes a query with runtime plan statistics and
//! reports the actual plan (DBMS_XPLAN.DISPLAY_CURSOR) and session
//! statistic deltas (V$MYSTAT).

use std::time::{Duration, Instant};

use pythia_db::DbError;
use pythia_db::oracle::Connection;
use pythia_plsql::{TokenKind, tokenize};

/// Statistics shown, in display order.
const STATS: &[&str] = &[
    "session logical reads",
    "consistent gets",
    "db block gets",
    "physical reads",
    "redo size",
    "sorts (memory)",
    "sorts (disk)",
    "parse count (hard)",
    "execute count",
    "CPU used by this session",
    "bytes sent via SQL*Net to client",
    "SQL*Net roundtrips to/from client",
];

#[derive(Debug, Clone)]
pub struct Report {
    pub rows: usize,
    pub elapsed: Duration,
    /// DISPLAY_CURSOR output, or why it isn't available.
    pub plan: Result<String, String>,
    /// The plan lines with estimated and actual rows (empty if the plan
    /// isn't available).
    pub plan_rows: Vec<pythia_ui::plan::PlanRow>,
    /// (statistic, delta), or why they aren't available.
    pub stats: Result<Vec<(String, i64)>, String>,
}

/// Adds `gather_plan_statistics` to the query's first SELECT (merging into
/// an existing hint comment).
pub fn with_runtime_statistics(sql: &str) -> String {
    let tokens = tokenize(sql);
    let Some(select) = tokens.iter().position(|t| {
        t.kind == TokenKind::Keyword && sql[t.span.clone()].eq_ignore_ascii_case("SELECT")
    }) else {
        return sql.to_owned();
    };
    let next = tokens[select + 1..]
        .iter()
        .find(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Newline));
    match next {
        Some(hint) if hint.kind == TokenKind::Hint => {
            let close = sql[hint.span.clone()]
                .rfind("*/")
                .map_or(hint.span.end, |i| hint.span.start + i);
            format!("{} gather_plan_statistics {}", &sql[..close], &sql[close..])
        }
        _ => {
            let at = tokens[select].span.end;
            format!("{} /*+ gather_plan_statistics */{}", &sql[..at], &sql[at..])
        }
    }
}

fn stat_values(conn: &Connection) -> Result<Vec<(String, i64)>, DbError> {
    let names: Vec<String> = STATS.iter().map(|n| format!("'{n}'")).collect();
    let sql = format!(
        "select n.name, s.value from v$mystat s join v$statname n on n.statistic# = s.statistic# \
          where n.name in ({})",
        names.join(", ")
    );
    let mut out = Vec::new();
    for row in conn.query_as::<(String, i64)>(&sql, &[])? {
        out.push(row?);
    }
    out.sort_by_key(|(name, _)| STATS.iter().position(|s| s == name));
    Ok(out)
}

/// Runs `sql` (a query), fetching every row, and collects the report.
pub fn run(conn: &Connection, sql: &str) -> Result<Report, DbError> {
    // A unique marker finds this execution in V$SQL afterwards.
    let marker = format!(
        "pythia:{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let tagged = with_runtime_statistics(sql).replacen(
        "gather_plan_statistics",
        &format!("gather_plan_statistics {marker}"),
        1,
    );
    // Two snapshots measure what reading the statistics itself costs.
    let calibrate = stat_values(conn);
    let before = stat_values(conn);
    let started = Instant::now();
    let mut rows = 0;
    for row in conn.query(&tagged, &[])? {
        row?;
        rows += 1;
    }
    let elapsed = started.elapsed();
    let after = stat_values(conn);
    let stats = match (calibrate, before, after) {
        (Ok(calibrate), Ok(before), Ok(after)) => Ok(after
            .into_iter()
            .map(|(name, value)| {
                let at = |list: &[(String, i64)]| {
                    list.iter().find(|(n, _)| *n == name).map_or(0, |(_, v)| *v)
                };
                let (start, overhead) = (at(&before), at(&before) - at(&calibrate));
                (name, (value - start - overhead).max(0))
            })
            .collect()),
        (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => Err(privilege_hint(e)),
    };
    let (plan, plan_rows) = match actual_plan(conn, &marker) {
        Ok((plan, rows)) => (Ok(plan), rows),
        Err(e) => (Err(privilege_hint(e)), Vec::new()),
    };
    Ok(Report {
        rows,
        elapsed,
        plan,
        plan_rows,
        stats,
    })
}

/// DISPLAY_CURSOR output for the cursor tagged with `marker`, and its plan
/// lines with the statistics.
fn actual_plan(
    conn: &Connection,
    marker: &str,
) -> Result<(String, Vec<pythia_ui::plan::PlanRow>), DbError> {
    let (sql_id, child): (String, i64) = conn.query_row_as(
        &pythia_db::compat::first_rows(
            "select sql_id, child_number from v$sql \
              where sql_text like :1 and sql_text not like '%v$sql%' \
              order by last_active_time desc",
            1,
        ),
        &[&format!("%{marker}%")],
    )?;
    let lines = conn.query_as::<Option<String>>(
        "select plan_table_output from table(dbms_xplan.display_cursor(:1, :2, 'ALLSTATS LAST +COST +PREDICATE'))",
        &[&sql_id, &child],
    )?;
    let lines: Vec<String> = lines
        .map(|l| l.map(Option::unwrap_or_default))
        .collect::<Result<_, _>>()?;
    let rows = match pythia_db::cursor_plan(conn, &sql_id, child)? {
        pythia_db::MetaResult::Table { columns, rows } => pythia_ui::plan::rows(&columns, &rows),
        _ => Vec::new(),
    };
    Ok((lines.join("\n"), rows))
}

fn privilege_hint(e: DbError) -> String {
    match e.code {
        Some(942) | Some(1031) => format!(
            "{e}\nNeeds SELECT_CATALOG_ROLE (or SELECT on V$MYSTAT, V$STATNAME, V$SQL_PLAN_STATISTICS_ALL)."
        ),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints() {
        assert_eq!(
            with_runtime_statistics("select * from t"),
            "select /*+ gather_plan_statistics */ * from t"
        );
        assert_eq!(
            with_runtime_statistics("SELECT /*+ full(t) */ * from t"),
            "SELECT /*+ full(t)  gather_plan_statistics */ * from t"
        );
        assert_eq!(
            with_runtime_statistics("with a as (select 1 x from dual) select * from a"),
            "with a as (select /*+ gather_plan_statistics */ 1 x from dual) select * from a"
        );
    }
}
