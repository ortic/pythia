//! What differs between the Oracle versions Pythia talks to (11.2 onwards):
//! SQL that every version understands, and checks for features that only
//! newer releases have.

use crate::oracle::Connection;

/// Major and minor release of a server, e.g. `(19, 0)` or `(11, 2)`.
pub type Release = (i32, i32);

/// The server's release. The driver caches it, so this costs no round trip
/// after the first call on a connection.
pub fn release(conn: &Connection) -> Result<Release, crate::oracle::Error> {
    let (version, _) = conn.server_version()?;
    Ok((version.major(), version.minor()))
}

/// Whether the server is at least `major.minor`, e.g. `at_least(conn, 12, 1)`.
pub fn at_least(conn: &Connection, major: i32, minor: i32) -> Result<bool, crate::oracle::Error> {
    Ok(release(conn)? >= (major, minor))
}

/// The columns of table `owner.table` to read its rows by: with its
/// invisible columns (12.1), which `select *` leaves out (#444), and
/// without its virtual ones unless `with_virtual`, as no INSERT or UPDATE
/// takes them (#443). Dictionary names, in the table's order, invisible
/// ones last; none if the dictionary doesn't show the table.
pub fn table_columns(
    conn: &Connection,
    owner: &str,
    table: &str,
    with_virtual: bool,
) -> Result<Vec<String>, crate::oracle::Error> {
    conn.query_as::<String>(
        "select column_name from all_tab_cols \
          where owner = :1 and table_name = :2 and user_generated = 'YES' \
            and (:3 = 1 or virtual_column = 'NO') \
          order by column_id nulls last, internal_column_id",
        &[&owner, &table, &i32::from(with_virtual)],
    )?
    .collect()
}

/// The first `n` rows of `sql` (which may end in `order by`). `FETCH FIRST`
/// needs 12.1; an ordered inline view with ROWNUM works on every version
/// and gets the same top-N plan.
pub fn first_rows(sql: &str, n: usize) -> String {
    format!("select * from ({sql}) where rownum <= {n}")
}

/// The declared type of an `ALL_ARGUMENTS a` row: `owner.table%ROWTYPE`
/// (18c+ names the table; before, it is a bare `PL/SQL RECORD`), `pkg.rec_t`
/// for package types, `owner.obj_t` for object types, else the scalar type.
/// Cursors are `REF CURSOR` (a strong one's type names its row type, not
/// the cursor's); types found through a public synonym (`XMLTYPE`) have no
/// owner (theirs is `PUBLIC`).
pub const ARGUMENT_TYPE: &str = "case when a.data_type = 'REF CURSOR' then 'REF CURSOR' \
     when a.type_name is null then nvl(a.pls_type, a.data_type) \
     else decode(a.type_owner, a.owner, null, 'PUBLIC', null, a.type_owner || '.') || a.type_name \
          || nvl2(a.type_subname, '.' || a.type_subname, null) \
          || decode(a.pls_type, 'ROWTYPE', '%ROWTYPE') end";

/// Schemas Oracle creates and maintains, for 11.2, which has no
/// `ALL_USERS.ORACLE_MAINTAINED` yet.
const ORACLE_SCHEMAS_11G: &[&str] = &[
    "ANONYMOUS",
    "APPQOSSYS",
    "CTXSYS",
    "DBSNMP",
    "DIP",
    "DMSYS",
    "DVF",
    "DVSYS",
    "EXFSYS",
    "LBACSYS",
    "MDDATA",
    "MDSYS",
    "MGMT_VIEW",
    "OLAPSYS",
    "ORACLE_OCM",
    "ORDDATA",
    "ORDPLUGINS",
    "ORDSYS",
    "OUTLN",
    "SI_INFORMTN_SCHEMA",
    "SYS",
    "SYSMAN",
    "SYSTEM",
    "TSMSYS",
    "WMSYS",
    "XDB",
    "XS$NULL",
];

/// A subquery of the schemas Oracle maintains (`oracle` true) or of all
/// others, for `owner [not] in (…)` filters.
pub fn schemas_query(release: Release, oracle: bool) -> String {
    if release >= (12, 1) {
        let flag = if oracle { 'Y' } else { 'N' };
        return format!("select username from all_users where oracle_maintained = '{flag}'");
    }
    let names: Vec<String> = ORACLE_SCHEMAS_11G
        .iter()
        .map(|s| format!("'{s}'"))
        .collect();
    let known = format!(
        "(username in ({}) or username like 'APEX\\_%' escape '\\' \
         or username like 'FLOWS\\_%' escape '\\' or username like 'SPATIAL\\_%' escape '\\' \
         or username like 'OWBSYS%')",
        names.join(", ")
    );
    let condition = if oracle {
        known
    } else {
        format!("not {known}")
    };
    format!("select username from all_users where {condition}")
}

/// Rewrites curated SQL written for 12.1+ (the built-in reports, which
/// open in an editor) so `release` runs it: before 12.1 the
/// `ORACLE_MAINTAINED` subqueries become the fixed list and a trailing
/// `fetch first N rows only` becomes ROWNUM.
pub fn for_release(sql: &str, release: Release) -> String {
    if release >= (12, 1) {
        return sql.to_owned();
    }
    let mut out = sql.to_owned();
    for oracle in [true, false] {
        out = out.replace(
            &schemas_query((12, 1), oracle),
            &schemas_query(release, oracle),
        );
    }
    let lower = out.to_lowercase();
    if let Some(at) = lower.rfind("fetch first ")
        && let Some(n) = lower[at + "fetch first ".len()..]
            .strip_suffix(" rows only")
            .and_then(|n| n.trim().parse::<usize>().ok())
    {
        out = first_rows(out[..at].trim_end(), n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_rows_wraps_the_ordered_query() {
        assert_eq!(
            first_rows("select a from t order by a", 5),
            "select * from (select a from t order by a) where rownum <= 5"
        );
    }

    #[test]
    fn oracle_schemas_before_12c() {
        let sql = "select * from all_objects where owner not in \
                   (select username from all_users where oracle_maintained = 'Y')";
        assert_eq!(for_release(sql, (19, 0)), sql);
        let old = for_release(sql, (11, 2));
        assert!(!old.contains("oracle_maintained"), "{old}");
        assert!(old.contains("'SYS', 'SYSMAN', 'SYSTEM'"), "{old}");
        assert!(schemas_query((11, 2), false).contains("where not (username in"));
    }

    #[test]
    fn fetch_first_before_12c() {
        let sql = "select a\n  from t\n order by a desc\n fetch first 50 rows only";
        assert_eq!(for_release(sql, (12, 2)), sql);
        assert_eq!(
            for_release(sql, (11, 2)),
            "select * from (select a\n  from t\n order by a desc) where rownum <= 50"
        );
    }
}
