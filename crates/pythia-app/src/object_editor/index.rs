//! Index: table, columns (or expressions) with sort order, unique or bitmap,
//! tablespace. Structural changes recreate the index; a new tablespace
//! rebuilds it.

use egui::{RichText, Ui};
use pythia_db::DbError;
use pythia_db::oracle::Connection;

use super::{Lists, form_grid, form_row, ident, name_field, picker, qualified};
use crate::documents::quote_ident;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IndexColumn {
    /// Column name or expression.
    pub expr: String,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IndexDef {
    pub name: String,
    pub table: String,
    pub columns: Vec<IndexColumn>,
    pub unique: bool,
    pub bitmap: bool,
    /// Empty: the default tablespace.
    pub tablespace: String,
    /// Build without blocking DML on the table.
    pub online: bool,
    /// The table's owner where it isn't the index's: the index was made
    /// again on the index owner's table of the name (#466).
    pub table_owner: String,
    /// What the form doesn't edit and making the index again keeps (#466):
    /// `compress 1`, `reverse`, `invisible`, `local`.
    pub kept: Vec<String>,
    /// Why the index can't be made again from the form, which would lose
    /// it: a domain index's parameters, a global partitioning.
    pub not_remade: Option<String>,
}

impl IndexDef {
    pub fn new() -> Self {
        Self {
            columns: vec![IndexColumn::default()],
            ..Self::default()
        }
    }

    fn structure(&self) -> (&str, &[IndexColumn], bool, bool) {
        (&self.table, &self.columns, self.unique, self.bitmap)
    }
}

pub fn load(conn: &Connection, owner: &str, name: &str) -> Result<IndexDef, DbError> {
    #[allow(clippy::type_complexity)]
    let (
        table_owner,
        table,
        uniqueness,
        index_type,
        tablespace,
        compression,
        prefix,
        visibility,
        locality,
    ) = conn
        .query_row_as::<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<u32>,
            Option<String>,
            Option<String>,
        )>(
            "select i.table_owner, i.table_name, i.uniqueness, i.index_type, i.tablespace_name,
                    i.compression, i.prefix_length, i.visibility,
                    (select p.locality from all_part_indexes p
                      where p.owner = i.owner and p.index_name = i.index_name)
               from all_indexes i where i.owner = :1 and i.index_name = :2",
            &[&owner, &name],
        )
        .map_err(|e| match e.db_error().map(|d| d.code()) {
            Some(1403) => DbError::new(format!("Index {owner}.{name} not found")),
            _ => e.into(),
        })?;
    // Descending and function-based columns have their expression in
    // ALL_IND_EXPRESSIONS (a LONG, so read on its own); the column name is
    // then a system name.
    let mut expressions = std::collections::HashMap::new();
    for row in conn.query_as::<(u32, Option<String>)>(
        "select column_position, column_expression from all_ind_expressions
          where index_owner = :1 and index_name = :2",
        &[&owner, &name],
    )? {
        let (position, expression) = row?;
        expressions.insert(position, expression.unwrap_or_default());
    }
    let mut columns = Vec::new();
    for row in conn.query_as::<(u32, String, String)>(
        "select column_position, column_name, descend from all_ind_columns
          where index_owner = :1 and index_name = :2
          order by column_position",
        &[&owner, &name],
    )? {
        let (position, column, descend) = row?;
        let expr = match expressions.remove(&position) {
            Some(e) => {
                let e = e.trim();
                // `"NAME"` of a descending column is just the column.
                match e.strip_prefix('"').and_then(|e| e.strip_suffix('"')) {
                    Some(plain) if !plain.contains('"') => quote_ident(plain),
                    _ => e.to_owned(),
                }
            }
            None => quote_ident(&column),
        };
        columns.push(IndexColumn {
            expr,
            desc: descend == "DESC",
        });
    }
    let mut kept = Vec::new();
    match compression.as_deref() {
        Some("ENABLED") => kept.push(match prefix {
            Some(n) => format!("compress {n}"),
            None => "compress".to_owned(),
        }),
        Some(advanced @ ("ADVANCED LOW" | "ADVANCED HIGH")) => {
            kept.push(format!("compress {}", advanced.to_lowercase()));
        }
        _ => {}
    }
    if index_type.ends_with("/REV") {
        kept.push("reverse".to_owned());
    }
    if visibility.as_deref() == Some("INVISIBLE") {
        kept.push("invisible".to_owned());
    }
    let not_remade = match (index_type.as_str(), locality.as_deref()) {
        ("DOMAIN", _) => Some("a domain index, whose parameters only its DDL keeps".to_owned()),
        (_, Some("GLOBAL")) => {
            Some("a globally partitioned index, whose partitions only its DDL keeps".to_owned())
        }
        (_, Some("LOCAL")) => {
            kept.push("local".to_owned());
            None
        }
        _ => None,
    };
    Ok(IndexDef {
        name: quote_ident(name),
        table: quote_ident(&table),
        columns,
        unique: uniqueness == "UNIQUE",
        // FUNCTION-BASED BITMAP too.
        bitmap: index_type.contains("BITMAP"),
        // As typed, like the other names: "my_ts" isn't MY_TS (#365).
        tablespace: tablespace.map(|t| quote_ident(&t)).unwrap_or_default(),
        online: false,
        table_owner: if table_owner == owner {
            String::new()
        } else {
            table_owner
        },
        kept,
        not_remade,
    })
}

fn create(owner: &str, def: &IndexDef) -> String {
    let kind = if def.unique {
        "unique "
    } else if def.bitmap {
        "bitmap "
    } else {
        ""
    };
    let columns: Vec<String> = def
        .columns
        .iter()
        .map(|c| {
            let expr = c.expr.trim();
            // A plain name is normalized like any identifier.
            let expr = if expr
                .chars()
                .all(|ch| ch.is_alphanumeric() || "_$#".contains(ch))
            {
                quote_ident(&ident(expr))
            } else {
                expr.to_owned()
            };
            if c.desc { format!("{expr} desc") } else { expr }
        })
        .collect();
    let table_owner = if def.table_owner.is_empty() {
        owner
    } else {
        &def.table_owner
    };
    let mut sql = format!(
        "create {kind}index {}\n  on {} ({})",
        qualified(owner, &def.name),
        qualified(table_owner, &def.table),
        columns.join(", ")
    );
    if !def.tablespace.trim().is_empty() {
        sql.push_str(&format!(
            "\n  tablespace {}",
            quote_ident(&ident(&def.tablespace))
        ));
    }
    // Index attributes, then ONLINE, then the partitioning, as Oracle
    // reads them.
    for clause in def.kept.iter().filter(|c| *c != "local") {
        sql.push_str(&format!("\n  {clause}"));
    }
    if def.online {
        sql.push_str("\n  online");
    }
    if def.kept.iter().any(|c| c == "local") {
        sql.push_str("\n  local");
    }
    sql
}

pub fn statements(
    owner: &str,
    original: Option<&IndexDef>,
    def: &IndexDef,
) -> Result<Vec<String>, String> {
    if def.name.trim().is_empty() {
        return Err("Enter a name".into());
    }
    if def.table.trim().is_empty() {
        return Err("Choose the table".into());
    }
    if def.columns.iter().all(|c| c.expr.trim().is_empty()) {
        return Err("Add at least one column".into());
    }
    if def.columns.iter().any(|c| c.expr.trim().is_empty()) {
        return Err("Remove empty columns".into());
    }
    if def.unique && def.bitmap {
        return Err("A bitmap index can't be unique".into());
    }
    let mut def = def.clone();
    def.columns.retain(|c| !c.expr.trim().is_empty());
    let Some(original) = original else {
        return Ok(vec![create(owner, &def)]);
    };
    if def.structure() != original.structure() {
        if let Some(why) = &original.not_remade {
            return Err(format!("It is {why}: change it with its DDL"));
        }
        return Ok(vec![
            format!("drop index {}", qualified(owner, &def.name)),
            create(owner, &def),
        ]);
    }
    if ident(&def.tablespace) != ident(&original.tablespace) && !def.tablespace.trim().is_empty() {
        let online = if def.online { " online" } else { "" };
        return Ok(vec![format!(
            "alter index {} rebuild tablespace {}{online}",
            qualified(owner, &def.name),
            quote_ident(&ident(&def.tablespace))
        )]);
    }
    Ok(Vec::new())
}

pub fn ui(ui: &mut Ui, def: &mut IndexDef, existing: bool, lists: &mut Lists) {
    form_grid(ui, "index", |ui| {
        name_field(ui, &mut def.name, existing);
        form_row(ui, "Table", |ui| {
            ui.add(egui::TextEdit::singleline(&mut def.table).desired_width(240.0));
            if let Some(table) = picker(ui, "table", &lists.tables) {
                def.table = quote_ident(&table);
            }
        });
        form_row(ui, "Kind", |ui| {
            if ui.checkbox(&mut def.unique, "unique").changed() && def.unique {
                def.bitmap = false;
            }
            if ui
                .checkbox(&mut def.bitmap, "bitmap")
                .on_hover_text("For low-cardinality columns in reporting tables")
                .changed()
                && def.bitmap
            {
                def.unique = false;
            }
        });
        form_row(ui, "Tablespace", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut def.tablespace)
                    .hint_text("default")
                    .desired_width(240.0),
            );
            if let Some(ts) = picker(ui, "tablespace", &lists.tablespaces) {
                def.tablespace = quote_ident(&ts);
            }
        });
        form_row(ui, "Build", |ui| {
            ui.checkbox(&mut def.online, "online").on_hover_text(
                "Keep the table writable while the index builds (Enterprise Edition)",
            );
        });
        if !def.kept.is_empty() {
            form_row(ui, "Also", |ui| {
                ui.weak(def.kept.join(", "))
                    .on_hover_text("Kept when the index is made again for new columns");
            });
        }
        if let Some(why) = &def.not_remade {
            form_row(ui, "Note", |ui| {
                ui.weak(format!(
                    "It is {why}: its columns and kind change with its DDL"
                ));
            });
        }
    });
    ui.add_space(8.0);
    ui.label(RichText::new("Columns").strong());
    let table = ident(&def.table);
    let columns = if table.is_empty() {
        None
    } else {
        match lists.columns.get(&table) {
            Some(c) => Some(c.clone()),
            None => {
                lists.want_columns = Some(table);
                None
            }
        }
    };
    let mut remove = None;
    let mut swap = None;
    let n = def.columns.len();
    egui::Grid::new("index_columns")
        .num_columns(5)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            for (i, column) in def.columns.iter_mut().enumerate() {
                ui.weak(format!("{}", i + 1));
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [260.0, ui.spacing().interact_size.y],
                        egui::TextEdit::singleline(&mut column.expr)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("column or expression"),
                    );
                    if let Some(columns) = &columns
                        && let Some(c) = picker(ui, ("column", i), columns)
                    {
                        column.expr = quote_ident(&c);
                    }
                });
                ui.checkbox(&mut column.desc, "descending");
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(i > 0, egui::Button::new("↑").small())
                        .clicked()
                    {
                        swap = Some((i - 1, i));
                    }
                    if ui
                        .add_enabled(i + 1 < n, egui::Button::new("↓").small())
                        .clicked()
                    {
                        swap = Some((i, i + 1));
                    }
                });
                if ui.small_button("×").on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    if let Some((a, b)) = swap {
        def.columns.swap(a, b);
    }
    if let Some(i) = remove {
        def.columns.remove(i);
    }
    if ui.button("Add column").clicked() {
        def.columns.push(IndexColumn::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> IndexDef {
        IndexDef {
            name: "orders_customer_ix".into(),
            table: "orders".into(),
            columns: vec![
                IndexColumn {
                    expr: "customer_id".into(),
                    desc: false,
                },
                IndexColumn {
                    expr: "upper(name)".into(),
                    desc: true,
                },
            ],
            ..IndexDef::default()
        }
    }

    #[test]
    fn creates_indexes() {
        assert_eq!(
            statements("APP", None, &def()).unwrap(),
            [
                "create index APP.ORDERS_CUSTOMER_IX\n  on APP.ORDERS (CUSTOMER_ID, upper(name) desc)"
            ]
        );
        let mut d = def();
        d.unique = true;
        d.tablespace = "users".into();
        d.online = true;
        assert_eq!(
            statements("APP", None, &d).unwrap(),
            [
                "create unique index APP.ORDERS_CUSTOMER_IX\n  on APP.ORDERS (CUSTOMER_ID, upper(name) desc)\n  tablespace USERS\n  online"
            ]
        );
        d.bitmap = true;
        assert!(statements("APP", None, &d).is_err());
    }

    /// Made again for a new column, an index keeps its table's owner and
    /// what the form doesn't edit (#466); a domain index isn't made again.
    #[test]
    fn remade_indexes_keep_what_the_form_doesnt_edit() {
        let original = IndexDef {
            table_owner: "SALES".into(),
            kept: vec![
                "compress 1".into(),
                "reverse".into(),
                "invisible".into(),
                "local".into(),
            ],
            ..def()
        };
        let mut d = original.clone();
        d.columns.pop();
        d.online = true;
        assert_eq!(
            statements("APP", Some(&original), &d).unwrap(),
            [
                "drop index APP.ORDERS_CUSTOMER_IX",
                "create index APP.ORDERS_CUSTOMER_IX\n  on SALES.ORDERS (CUSTOMER_ID)\n  \
                 compress 1\n  reverse\n  invisible\n  online\n  local",
            ]
        );
        let domain = IndexDef {
            not_remade: Some("a domain index".into()),
            ..def()
        };
        let mut d = domain.clone();
        d.columns.pop();
        assert!(statements("APP", Some(&domain), &d).is_err());
    }

    /// On Oracle: an index made again from the form is as it was but for
    /// its columns (#466).
    #[test]
    fn remade_indexes_on_oracle() {
        let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
            return;
        };
        let _ = conn.execute("drop table r101_ixp purge", &[]);
        for sql in [
            "create table r101_ixp (a number, b number) partition by range (a) \
               (partition p1 values less than (10), partition p2 values less than (maxvalue))",
            "create index r101_ixp_i on r101_ixp (a, b) compress 1 reverse invisible local",
        ] {
            conn.execute(sql, &[]).unwrap();
        }
        let owner: String = conn
            .query_row_as(
                "select sys_context('USERENV', 'CURRENT_SCHEMA') from dual",
                &[],
            )
            .unwrap();
        let original = load(&conn, &owner, "R101_IXP_I").unwrap();
        assert_eq!(
            original.kept,
            ["compress 1", "reverse", "invisible", "local"]
        );
        assert!(original.table_owner.is_empty() && original.not_remade.is_none());
        let mut d = original.clone();
        d.columns.pop();
        for sql in statements(&owner, Some(&original), &d).unwrap() {
            conn.execute(&sql, &[])
                .unwrap_or_else(|e| panic!("{e}\n{sql}"));
        }
        let (index_type, compression, visibility, partitioned): (String, String, String, String) =
            conn.query_row_as(
                "select index_type, compression, visibility, partitioned from user_indexes \
                  where index_name = 'R101_IXP_I'",
                &[],
            )
            .unwrap();
        assert_eq!(
            (
                index_type.as_str(),
                compression.as_str(),
                visibility.as_str(),
                partitioned.as_str()
            ),
            ("NORMAL/REV", "ENABLED", "INVISIBLE", "YES")
        );
        conn.execute("drop table r101_ixp purge", &[]).unwrap();
    }

    #[test]
    fn alters_indexes() {
        let original = def();
        let mut d = def();
        assert!(statements("APP", Some(&original), &d).unwrap().is_empty());
        d.tablespace = "fast".into();
        assert_eq!(
            statements("APP", Some(&original), &d).unwrap(),
            ["alter index APP.ORDERS_CUSTOMER_IX rebuild tablespace FAST"]
        );
        d.columns.pop();
        let s = statements("APP", Some(&original), &d).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], "drop index APP.ORDERS_CUSTOMER_IX");
    }
}
