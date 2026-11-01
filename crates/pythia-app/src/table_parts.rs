//! Indexes, constraints and triggers of a table, for the object browser.

use pythia_db::oracle::Connection;
use pythia_db::{DbError, Dialect};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartKind {
    Index {
        unique: bool,
    },
    /// P, U, R or C.
    Constraint {
        kind: char,
    },
    Trigger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub kind: PartKind,
    pub name: String,
    /// Columns, the referenced table or the trigger timing.
    pub detail: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableParts {
    pub indexes: Vec<Part>,
    pub constraints: Vec<Part>,
    pub triggers: Vec<Part>,
    /// Foreign keys of other tables pointing at this one.
    pub referenced_by: Vec<Part>,
    /// Columns that are part of a foreign key.
    pub fk_columns: Vec<String>,
}

pub fn load(conn: &Connection, owner: &str, table: &str) -> Result<TableParts, DbError> {
    let mut parts = TableParts::default();
    let sql = "
select i.index_name, i.uniqueness, i.status,
       listagg(c.column_name, ', ') within group (order by c.column_position)
  from all_indexes i
  join all_ind_columns c on c.index_owner = i.owner and c.index_name = i.index_name
 where i.table_owner = :1 and i.table_name = :2
 group by i.index_name, i.uniqueness, i.status
 order by i.index_name";
    for row in conn.query_as::<(String, String, String, Option<String>)>(sql, &[&owner, &table])? {
        let (name, uniqueness, status, columns) = row?;
        parts.indexes.push(Part {
            kind: PartKind::Index {
                unique: uniqueness == "UNIQUE",
            },
            name,
            detail: shown_list(&columns.unwrap_or_default()),
            enabled: status != "UNUSABLE",
        });
    }
    // The condition is a LONG before 12.1, which SQL can't filter or cut.
    let condition = if pythia_db::compat::at_least(conn, 12, 1)? {
        "c.search_condition_vc"
    } else {
        "c.search_condition"
    };
    let sql = format!(
        "
select c.constraint_name, c.constraint_type, c.status, c.generated,
       (select listagg(cc.column_name, ', ') within group (order by cc.position)
          from all_cons_columns cc
         where cc.owner = c.owner and cc.constraint_name = c.constraint_name),
       r.table_name, {condition}
  from all_constraints c
  left join all_constraints r on r.owner = c.r_owner and r.constraint_name = c.r_constraint_name
 where c.owner = :1 and c.table_name = :2
   and c.constraint_type in ('P', 'U', 'R', 'C')
 order by decode(c.constraint_type, 'P', 1, 'U', 2, 'R', 3, 4), c.constraint_name"
    );
    for row in conn.query_as::<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )>(&sql, &[&owner, &table])?
    {
        let (name, kind, status, generated, columns, references, condition) = row?;
        let kind = kind.chars().next().unwrap_or('C');
        // Leave out the NOT NULL checks Oracle generates for columns.
        if kind == 'C'
            && generated == "GENERATED NAME"
            && condition
                .as_deref()
                .is_some_and(|c| c.ends_with("IS NOT NULL"))
        {
            continue;
        }
        let condition = condition
            .filter(|_| kind == 'C')
            .map(|c| c.chars().take(200).collect());
        let columns = columns.unwrap_or_default();
        // As stored, which the browser compares with the columns (#295).
        if kind == 'R' {
            parts
                .fk_columns
                .extend(columns.split(", ").map(str::to_owned));
        }
        let columns = shown_list(&columns);
        let detail = match (kind, references, condition) {
            ('R', Some(table), _) => format!("{columns} → {}", shown(&table)),
            ('C', _, Some(condition)) => condition,
            _ => columns,
        };
        parts.constraints.push(Part {
            kind: PartKind::Constraint { kind },
            name,
            detail,
            enabled: status == "ENABLED",
        });
    }
    let sql = "
select c.constraint_name, c.owner, c.table_name, c.status,
       (select listagg(cc.column_name, ', ') within group (order by cc.position)
          from all_cons_columns cc
         where cc.owner = c.owner and cc.constraint_name = c.constraint_name)
  from all_constraints r
  join all_constraints c on c.r_owner = r.owner and c.r_constraint_name = r.constraint_name
 where r.owner = :1 and r.table_name = :2 and r.constraint_type in ('P', 'U')
   and c.constraint_type = 'R'
 order by c.table_name, c.constraint_name";
    for row in
        conn.query_as::<(String, String, String, String, Option<String>)>(sql, &[&owner, &table])?
    {
        let (name, child_owner, child, status, columns) = row?;
        let child = if child_owner == owner {
            shown(&child)
        } else {
            format!("{}.{}", shown(&child_owner), shown(&child))
        };
        parts.referenced_by.push(Part {
            kind: PartKind::Constraint { kind: 'R' },
            name,
            detail: format!("{child} ({})", shown_list(&columns.unwrap_or_default())),
            enabled: status == "ENABLED",
        });
    }
    let sql = "
select trigger_name, lower(trigger_type || ' ' || triggering_event), status
  from all_triggers
 where table_owner = :1 and table_name = :2
 order by trigger_name";
    for row in conn.query_as::<(String, String, String)>(sql, &[&owner, &table])? {
        let (name, detail, status) = row?;
        parts.triggers.push(Part {
            kind: PartKind::Trigger,
            name,
            detail,
            enabled: status == "ENABLED",
        });
    }
    Ok(parts)
}

/// A name as the object browser shows it (#272).
fn shown(name: &str) -> String {
    crate::browser::shown_name(Dialect::ORACLE, name)
}

/// Names listed as `A, B`, each [`shown`].
fn shown_list(names: &str) -> String {
    if names.is_empty() {
        return String::new();
    }
    names.split(", ").map(shown).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_from_live_database() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        conn.execute(
            "begin
               for t in (select table_name from user_tables
                          where table_name in ('PYTHIA_PARTS_C', 'PYTHIA_PARTS_P')) loop
                 execute immediate 'drop table ' || t.table_name || ' cascade constraints purge';
               end loop;
             end;",
            &[],
        )
        .unwrap();
        for sql in [
            "create table pythia_parts_p (id number primary key)",
            "create table pythia_parts_c (id number constraint pythia_parts_pk primary key,
               p_id number not null constraint pythia_parts_fk references pythia_parts_p,
               qty number constraint pythia_parts_qty check (qty > 0))",
            "create index pythia_parts_ix on pythia_parts_c (p_id, qty)",
            "create trigger pythia_parts_trg before insert on pythia_parts_c for each row begin null; end;",
        ] {
            conn.execute(sql, &[]).unwrap();
        }
        let user = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        let parts = load(&conn, &user, "PYTHIA_PARTS_C").unwrap();
        let names = |p: &[Part]| p.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        assert_eq!(
            names(&parts.indexes),
            ["PYTHIA_PARTS_IX", "PYTHIA_PARTS_PK"]
        );
        assert_eq!(parts.indexes[0].detail, "p_id, qty");
        assert_eq!(
            names(&parts.constraints),
            ["PYTHIA_PARTS_PK", "PYTHIA_PARTS_FK", "PYTHIA_PARTS_QTY"]
        );
        assert_eq!(parts.constraints[1].detail, "p_id → pythia_parts_p");
        assert_eq!(parts.constraints[2].detail, "qty > 0");
        assert_eq!(parts.fk_columns, ["P_ID"]);
        assert_eq!(names(&parts.triggers), ["PYTHIA_PARTS_TRG"]);
        assert_eq!(parts.triggers[0].detail, "before each row insert");
        let parent = load(&conn, &user, "PYTHIA_PARTS_P").unwrap();
        assert_eq!(names(&parent.referenced_by), ["PYTHIA_PARTS_FK"]);
        assert_eq!(parent.referenced_by[0].detail, "pythia_parts_c (p_id)");
        conn.execute("drop table pythia_parts_c purge", &[])
            .unwrap();
        conn.execute("drop table pythia_parts_p purge", &[])
            .unwrap();
    }

    /// Columns and tables read as the browser shows names (#272): `"Id"`
    /// with its quotes, not as `id`; a foreign key's columns as they are.
    #[test]
    fn details_keep_the_case_of_names() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        let drops = [
            "drop table if exists r64_child purge",
            "drop table if exists \"R64_Parent\" purge",
        ];
        for sql in drops {
            conn.execute(sql, &[]).unwrap();
        }
        for sql in [
            "create table \"R64_Parent\" (\"Id\" number primary key)",
            "create table r64_child (id number, \"P_Id\" number \
               constraint r64_child_fk references \"R64_Parent\")",
            "create index r64_child_ix on r64_child (\"P_Id\", id)",
        ] {
            conn.execute(sql, &[]).unwrap();
        }
        let user = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        let parts = load(&conn, &user, "R64_CHILD").unwrap();
        assert_eq!(parts.indexes[0].detail, "\"P_Id\", id");
        assert_eq!(parts.constraints[0].detail, "\"P_Id\" → \"R64_Parent\"");
        assert_eq!(parts.fk_columns, ["P_Id"]);
        let parent = load(&conn, &user, "R64_Parent").unwrap();
        assert_eq!(parent.referenced_by[0].detail, "r64_child (\"P_Id\")");
        for sql in drops {
            conn.execute(sql, &[]).unwrap();
        }
    }
}
