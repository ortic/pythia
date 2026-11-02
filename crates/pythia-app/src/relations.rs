//! Foreign key navigation: the parents and children of a table, so a row in
//! a result can open its related rows.

use pythia_db::{Cell, Column, DbError, oracle};
use pythia_ui::GridLink;

use crate::documents::quote_ident;

/// A table related to the queried one through a foreign key.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub constraint: String,
    pub owner: String,
    pub table: String,
    /// (column of the queried table, column of the related table)
    pub pairs: Vec<(String, String)>,
    /// The related table references the queried one (child rows).
    pub child: bool,
    /// In another schema than the queried table: named with it.
    pub other_schema: bool,
}

/// The foreign keys from and to `table` (as written in the query: optionally
/// schema-qualified, optionally quoted). Runs on a session worker.
pub fn load(conn: &oracle::Connection, table: &str) -> Result<Vec<Relation>, DbError> {
    let (owner, name) = match split_name(table) {
        (Some(owner), name) => (owner, name),
        (None, name) => {
            let schema = conn.query_row_as::<String>(
                "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
                &[],
            )?;
            (schema, name)
        }
    };
    // Synonyms and views resolve to nothing here, which just means no links.
    let sql = "
select 'P', c.constraint_name, r.owner, r.table_name, cc.column_name, rc.column_name, cc.position
  from all_constraints c
  join all_cons_columns cc on cc.owner = c.owner and cc.constraint_name = c.constraint_name
  join all_constraints r on r.owner = c.r_owner and r.constraint_name = c.r_constraint_name
  join all_cons_columns rc on rc.owner = r.owner and rc.constraint_name = r.constraint_name
                          and rc.position = cc.position
 where c.constraint_type = 'R' and c.owner = :1 and c.table_name = :2
union all
select 'C', c.constraint_name, c.owner, c.table_name, rc.column_name, cc.column_name, cc.position
  from all_constraints r
  join all_constraints c on c.r_owner = r.owner and c.r_constraint_name = r.constraint_name
                        and c.constraint_type = 'R'
  join all_cons_columns cc on cc.owner = c.owner and cc.constraint_name = c.constraint_name
  join all_cons_columns rc on rc.owner = r.owner and rc.constraint_name = r.constraint_name
                          and rc.position = cc.position
 where r.owner = :1 and r.table_name = :2 and r.constraint_type in ('P', 'U')
 order by 1, 3, 4, 2, 7";
    let mut out: Vec<Relation> = Vec::new();
    for row in conn
        .query_as::<(String, String, String, String, String, String, i64)>(sql, &[&owner, &name])?
    {
        let (kind, constraint, their_owner, table, ours, theirs, _) = row?;
        let child = kind == "C";
        // A key's columns come one after another, by owner first: tables
        // and keys of the same names in two schemas don't mix (#401).
        match out.last_mut() {
            Some(last)
                if last.constraint == constraint
                    && last.owner == their_owner
                    && last.child == child =>
            {
                last.pairs.push((ours, theirs));
            }
            _ => out.push(Relation {
                constraint,
                other_schema: their_owner != owner,
                owner: their_owner,
                table,
                pairs: vec![(ours, theirs)],
                child,
            }),
        }
    }
    Ok(out)
}

/// Splits `owner.table` and normalizes the parts like Oracle does (unquoted
/// names upper case).
fn split_name(name: &str) -> (Option<String>, String) {
    let norm = |part: &str| {
        let part = part.trim();
        match part.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
            Some(quoted) => quoted.replace("\"\"", "\""),
            None => part.to_uppercase(),
        }
    };
    // A dot inside quotes doesn't separate.
    let mut in_quotes = false;
    for (i, c) in name.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '.' if !in_quotes => return (Some(norm(&name[..i])), norm(&name[i + 1..])),
            _ => {}
        }
    }
    (None, norm(name))
}

/// Grid links for the relations whose key columns are all in the result.
/// Returns the links and, for each, the index of its relation.
pub fn links(relations: &[Relation], columns: &[Column]) -> (Vec<GridLink>, Vec<usize>) {
    let mut links = Vec::new();
    let mut index = Vec::new();
    for (i, r) in relations.iter().enumerate() {
        let cols: Option<Vec<usize>> = r
            .pairs
            .iter()
            .map(|(ours, _)| columns.iter().position(|c| c.name == *ours))
            .collect();
        let Some(cols) = cols else { continue };
        let keys: Vec<&str> = r.pairs.iter().map(|(_, theirs)| theirs.as_str()).collect();
        let table = if r.other_schema {
            format!("{}.{}", r.owner, r.table)
        } else {
            r.table.clone()
        };
        let label = if r.child {
            format!("{table} ({})", keys.join(", "))
        } else {
            let ours: Vec<&str> = r.pairs.iter().map(|(ours, _)| ours.as_str()).collect();
            format!("{table} via {}", ours.join(", "))
        };
        links.push(GridLink {
            label,
            many: r.child,
            columns: cols,
        });
        index.push(i);
    }
    (links, index)
}

/// The query for the rows related to `row`.
pub fn query(relation: &Relation, columns: &[Column], row: &[Cell]) -> String {
    let conditions: Vec<String> = relation
        .pairs
        .iter()
        .map(|(ours, theirs)| {
            let i = columns.iter().position(|c| c.name == *ours);
            let value = i.and_then(|i| row.get(i).cloned().flatten());
            let numeric = i.is_some_and(|i| columns[i].numeric);
            let literal = match value {
                Some(v) if numeric && v.parse::<f64>().is_ok() => v,
                Some(v) => format!("'{}'", v.replace('\'', "''")),
                None => "null".into(),
            };
            format!("{} = {literal}", quote_ident(theirs))
        })
        .collect();
    let what = if relation.child { "Children" } else { "Parent" };
    format!(
        "-- {what} via {}\nselect *\n  from {}.{}\n where {}",
        relation.constraint,
        quote_ident(&relation.owner),
        quote_ident(&relation.table),
        conditions.join("\n   and ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, numeric: bool) -> Column {
        Column {
            name: name.into(),
            type_name: if numeric { "NUMBER" } else { "VARCHAR2" }.into(),
            numeric,
        }
    }

    #[test]
    fn splits_names() {
        assert_eq!(split_name("emp"), (None, "EMP".into()));
        assert_eq!(split_name("hr.emp"), (Some("HR".into()), "EMP".into()));
        assert_eq!(
            split_name("\"a.b\".\"Mixed\""),
            (Some("a.b".into()), "Mixed".into())
        );
    }

    #[test]
    fn builds_links_and_queries() {
        let relations = vec![
            Relation {
                constraint: "EMP_DEPT_FK".into(),
                owner: "HR".into(),
                table: "DEPARTMENTS".into(),
                pairs: vec![("DEPARTMENT_ID".into(), "DEPARTMENT_ID".into())],
                child: false,
                other_schema: false,
            },
            Relation {
                constraint: "X_FK".into(),
                owner: "HR".into(),
                table: "X".into(),
                pairs: vec![("NOT_SELECTED".into(), "ID".into())],
                child: true,
                other_schema: false,
            },
        ];
        let columns = [col("NAME", false), col("DEPARTMENT_ID", true)];
        let (links, index) = links(&relations, &columns);
        assert_eq!(links.len(), 1);
        assert_eq!(index, vec![0]);
        assert_eq!(links[0].columns, vec![1]);
        assert!(!links[0].many);
        let sql = query(
            &relations[0],
            &columns,
            &[Some("O'Brien".into()), Some("20".into())],
        );
        assert!(
            sql.ends_with("from HR.DEPARTMENTS\n where DEPARTMENT_ID = 20"),
            "{sql}"
        );
    }

    #[test]
    fn loads_from_live_database() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = oracle::Connection::connect(user, pw, db).unwrap();
        conn.execute(
            "begin
               for t in (select table_name from user_tables
                          where table_name in ('PYTHIA_REL_C', 'PYTHIA_REL_P')) loop
                 execute immediate 'drop table ' || t.table_name || ' cascade constraints purge';
               end loop;
             end;",
            &[],
        )
        .unwrap();
        conn.execute(
            "create table pythia_rel_p (a number, b number, primary key (a, b))",
            &[],
        )
        .unwrap();
        conn.execute(
            "create table pythia_rel_c (id number primary key, pa number, pb number,
               constraint pythia_rel_fk foreign key (pa, pb) references pythia_rel_p)",
            &[],
        )
        .unwrap();
        let parents = load(&conn, "pythia_rel_c").unwrap();
        assert_eq!(parents.len(), 1);
        assert!(!parents[0].child);
        assert_eq!(
            parents[0].pairs,
            vec![("PA".into(), "A".into()), ("PB".into(), "B".into())]
        );
        let children = load(&conn, "pythia_rel_p").unwrap();
        assert_eq!(children.len(), 1);
        assert!(children[0].child);
        assert_eq!(children[0].table, "PYTHIA_REL_C");
        assert_eq!(
            children[0].pairs,
            vec![("A".into(), "PA".into()), ("B".into(), "PB".into())]
        );
        // A table and key of the same names in another schema: its own
        // relation, named with its schema, not mixed column by column.
        if let Ok(dba) = std::env::var("PYTHIA_TEST_DB_DBA") {
            let _accounts = crate::test_db::ACCOUNTS
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let (cred, db) = dba.rsplit_once('@').unwrap();
            let (user, pw) = cred.split_once('/').unwrap();
            let dba = oracle::Connection::connect(user, pw, db).unwrap();
            let owner = conn
                .query_row_as::<String>("select user from dual", &[])
                .unwrap();
            let _ = dba.execute("drop user pythia_rel_o cascade", &[]);
            for sql in [
                "create user pythia_rel_o identified by \"Rel_o_1\" quota unlimited on users"
                    .to_owned(),
                format!("grant references on {owner}.pythia_rel_p to pythia_rel_o"),
                format!(
                    "create table pythia_rel_o.pythia_rel_c (id number primary key, pa number,
                       pb number, constraint pythia_rel_fk foreign key (pa, pb)
                       references {owner}.pythia_rel_p)"
                ),
                format!("grant select on pythia_rel_o.pythia_rel_c to {owner}"),
            ] {
                dba.execute(&sql, &[])
                    .unwrap_or_else(|e| panic!("{sql}: {e}"));
            }
            let children = load(&conn, "pythia_rel_p").unwrap();
            dba.execute("drop user pythia_rel_o cascade", &[]).unwrap();
            assert_eq!(children.len(), 2, "{children:?}");
            for child in &children {
                assert_eq!(child.pairs.len(), 2, "{child:?}");
            }
            let other = children.iter().find(|c| c.owner == "PYTHIA_REL_O").unwrap();
            assert!(other.other_schema);
            let columns = [col("A", true), col("B", true)];
            let (links, _) = links(&children, &columns);
            assert!(
                links
                    .iter()
                    .any(|l| l.label == "PYTHIA_REL_O.PYTHIA_REL_C (PA, PB)"),
                "{:?}",
                links.iter().map(|l| &l.label).collect::<Vec<_>>()
            );
        }
        conn.execute("drop table pythia_rel_c purge", &[]).unwrap();
        conn.execute("drop table pythia_rel_p purge", &[]).unwrap();
    }
}
