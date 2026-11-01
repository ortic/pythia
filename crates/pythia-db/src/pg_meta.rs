//! PostgreSQL's answers to [`MetaQuery`]s, from `pg_catalog`. Schemas are
//! the database's schemas; names are stored as written (unquoted ones in
//! lowercase), and filters match without case. PostgreSQL has no DDL of its
//! tables, so it is put together from the catalog.

use serde_json::Value;

use crate::dialect::Dialect;
use crate::meta::{
    CompileError, DbObject, Detail, MetaQuery, MetaResult, ObjectType, PlanStep, TableColumn,
    plan_table,
};
use crate::pg_session::{Pg, literal};
use crate::types::{Cell, Column, DbError};

const SCHEMAS: &str = "select nspname from pg_namespace \
                        where nspname not like 'pg\\_toast%' and nspname not like 'pg\\_temp\\_%' \
                        order by nspname";

/// Tables, views, sequences, routines (not those of extensions) and
/// triggers as `(owner, name, type)`; overloads and triggers of the same
/// name on several tables are one row.
const OBJECTS: &str = "\
    select n.nspname as owner, c.relname as name, \
           case c.relkind when 'v' then 'VIEW' when 'm' then 'MATERIALIZED VIEW' \
                          when 'S' then 'SEQUENCE' else 'TABLE' end as type \
      from pg_class c join pg_namespace n on n.oid = c.relnamespace \
     where c.relkind in ('r', 'p', 'f', 'v', 'm', 'S') and not c.relispartition \
    union \
    select n.nspname, p.proname, case p.prokind when 'p' then 'PROCEDURE' else 'FUNCTION' end \
      from pg_proc p join pg_namespace n on n.oid = p.pronamespace \
     where p.prokind in ('f', 'p') \
       and not exists (select 1 from pg_depend d where d.classid = 'pg_proc'::regclass \
                          and d.objid = p.oid and d.deptype = 'e') \
    union \
    select n.nspname, t.tgname, 'TRIGGER' \
      from pg_trigger t join pg_class c on c.oid = t.tgrelid \
      join pg_namespace n on n.oid = c.relnamespace \
     where not t.tgisinternal";

pub(crate) fn run(pg: &Pg<'_>, query: &MetaQuery) -> Result<MetaResult, DbError> {
    Ok(match query {
        MetaQuery::Schemas => MetaResult::Schemas(
            pg.rows(SCHEMAS, &[])?
                .into_iter()
                .filter_map(first)
                .collect(),
        ),
        MetaQuery::Objects {
            owner,
            object_type,
            filter,
        } => MetaResult::Objects(objects(pg, owner, *object_type, filter.as_deref())?),
        MetaQuery::Columns { owner, table, keys } => {
            MetaResult::Columns(columns(pg, owner, table, *keys)?)
        }
        // Routines live on their own, not in packages.
        MetaQuery::Members { .. } => MetaResult::Members(Vec::new()),
        MetaQuery::Details {
            owner,
            name,
            detail,
        } => details(pg, owner, name, *detail)?,
        MetaQuery::Source {
            owner,
            name,
            object_type,
        } => MetaResult::Text(ddl(pg, owner, name, *object_type, false)?),
        MetaQuery::Ddl {
            owner,
            name,
            object_type,
        } => MetaResult::Text(ddl(pg, owner, name, *object_type, true)?),
        // PostgreSQL reports errors when a routine is created.
        MetaQuery::Errors { .. } => MetaResult::Errors(Vec::<CompileError>::new()),
        MetaQuery::Find { pattern, limit } => MetaResult::Objects(find(pg, pattern, *limit)?),
        MetaQuery::ExplainPlan { sql } => explain_plan(pg, sql)?,
        MetaQuery::TestLink { .. } => {
            return Err(DbError::unsupported("Database links", Dialect::POSTGRES));
        }
        MetaQuery::Accounts(object_type) => MetaResult::Accounts(accounts(pg, *object_type)?),
    })
}

/// Roles that can log in as users, the others as roles; superusers say so.
fn accounts(pg: &Pg<'_>, object_type: ObjectType) -> Result<Vec<(String, String)>, DbError> {
    let login = if object_type == ObjectType::User {
        ""
    } else {
        "not "
    };
    let sql = format!(
        "select rolname, case when rolsuper then 'superuser' end from pg_roles \
          where {login}rolcanlogin order by rolname"
    );
    Ok(pg
        .rows(&sql, &[])?
        .into_iter()
        .filter_map(|row| {
            let mut values = row.into_iter();
            Some((values.next()??, values.next().flatten().unwrap_or_default()))
        })
        .collect())
}

fn first(row: Vec<Cell>) -> Option<String> {
    row.into_iter().next().flatten()
}

/// Rows of `(owner, name, type)` as objects.
fn object_rows(rows: Vec<Vec<Cell>>) -> Vec<DbObject> {
    rows.into_iter()
        .filter_map(|row| {
            let mut values = row.into_iter().map(Option::unwrap_or_default);
            let (owner, name, ty) = (values.next()?, values.next()?, values.next()?);
            Some(DbObject {
                owner,
                name,
                object_type: ObjectType::from_dictionary(&ty)?,
                valid: true,
                last_ddl: String::new(),
            })
        })
        .collect()
}

fn objects(
    pg: &Pg<'_>,
    owner: &str,
    object_type: ObjectType,
    filter: Option<&str>,
) -> Result<Vec<DbObject>, DbError> {
    let pattern = name_regex(filter.unwrap_or(""));
    let rows = pg.rows(
        &format!(
            "select owner, name, type from ({OBJECTS}) o \
              where owner = $1 and type = $2 and name ~ $3 order by name"
        ),
        &[owner, object_type.dictionary_name(), &pattern],
    )?;
    Ok(object_rows(rows))
}

/// A filter as a regular expression that finds the names containing it,
/// without case: `*` is any text, `?` any character, as in
/// [`like_pattern`](crate::meta::like_pattern). Each letter stands for its cases, `[čČ]`, as names
/// collate as C, where `upper()` and `ILIKE` change ASCII letters only, in
/// any database (#264).
fn name_regex(filter: &str) -> String {
    let mut out = String::new();
    for c in filter.trim().chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            c if c.is_ascii_punctuation() => {
                out.push('\\');
                out.push(c);
            }
            c => {
                let mut cases = vec![c];
                for case in [c.to_lowercase().to_string(), c.to_uppercase().to_string()] {
                    let mut chars = case.chars();
                    if let (Some(one), None) = (chars.next(), chars.next())
                        && !cases.contains(&one)
                    {
                        cases.push(one);
                    }
                }
                if cases.len() == 1 {
                    out.push(c);
                } else {
                    out.push('[');
                    out.extend(cases);
                    out.push(']');
                }
            }
        }
    }
    out
}

/// Objects of every schema whose name matches; the current schema's first,
/// then the shortest names.
fn find(pg: &Pg<'_>, pattern: &str, limit: usize) -> Result<Vec<DbObject>, DbError> {
    let pattern = name_regex(pattern);
    let rows = pg.rows(
        &format!(
            "select owner, name, type from ({OBJECTS}) o where name ~ $1 \
              order by (owner = current_schema()) desc, length(name), name, owner \
              limit {limit}"
        ),
        &[&pattern],
    )?;
    Ok(object_rows(rows))
}

/// The schema and name of a table, view or sequence as stored: names come
/// from the browser as stored, from code assistance in uppercase.
fn stored(pg: &Pg<'_>, owner: &str, name: &str) -> Result<(String, String), DbError> {
    let row = pg.first(
        "select n.nspname, c.relname from pg_class c join pg_namespace n on n.oid = c.relnamespace \
          where lower(n.nspname) = lower($1) and lower(c.relname) = lower($2) \
          order by (n.nspname = $1) desc, (c.relname = $2) desc, (c.relname = lower($2)) desc \
          limit 1",
        &[owner, name],
    )?;
    Ok(match row {
        Some(row) => {
            let mut values = row.into_iter().map(Option::unwrap_or_default);
            (
                values.next().unwrap_or_default(),
                values.next().unwrap_or_default(),
            )
        }
        None => (owner.to_owned(), name.to_owned()),
    })
}

/// Runs `f` with the session's search path set to `path`, then sets the
/// session's own back. PostgreSQL leaves the schema out of the names it
/// writes (`pg_get_constraintdef`, `format_type`, `::regclass`) where the
/// path finds them: on the session's path, a key from `tenant.note` to
/// `public.users` read `REFERENCES users` beside a `tenant.users` (#226).
fn on_path<T>(
    pg: &Pg<'_>,
    path: &str,
    f: impl FnOnce() -> Result<T, DbError>,
) -> Result<T, DbError> {
    let own = pg
        .first("select current_setting('search_path')", &[])?
        .and_then(first)
        .unwrap_or_default();
    let set = |path: &str| {
        pg.execute(&format!(
            "select set_config('search_path', {}, false)",
            literal(path)
        ))
    };
    set(path)?;
    let result = f();
    let restored = set(&own);
    let value = result?;
    restored?;
    Ok(value)
}

/// The column's default as written in DDL, identity and generated columns
/// included (`a` is `pg_attribute`, `d` is `pg_attrdef`); PostgreSQL 18's
/// generated columns are virtual unless stored (#227). An identity has the
/// options of its sequence that aren't the defaults (#263), and its
/// sequence's name where it isn't the one PostgreSQL chooses (#308):
/// `table_column_seq` in the table's schema, the two names cut to fit 63
/// bytes, the longer first, as `makeObjectName` cuts them. A chosen name is
/// left out, so that a copy chooses its own.
const DEFAULT: &str = "case when a.attidentity in ('a', 'd') \
                              then 'generated ' \
                                   || case a.attidentity when 'a' then 'always' else 'by default' end \
                                   || ' as identity' || coalesce(( \
                                select ' (' || nullif(concat_ws(' ', \
                                         case when sc.relnamespace <> r.relnamespace \
                                                or sc.relname <> chosen.name \
                                              then 'sequence name ' || quote_ident(sn.nspname) \
                                                   || '.' || quote_ident(sc.relname) end, \
                                         case when s.seqstart <> case when s.seqincrement > 0 \
                                                                      then s.seqmin else s.seqmax end \
                                              then 'start with ' || s.seqstart end, \
                                         case when s.seqincrement <> 1 \
                                              then 'increment by ' || s.seqincrement end, \
                                         case when s.seqmin <> case when s.seqincrement > 0 \
                                                                    then 1 else -t.top - 1 end \
                                              then 'minvalue ' || s.seqmin end, \
                                         case when s.seqmax <> case when s.seqincrement > 0 \
                                                                    then t.top else -1 end \
                                              then 'maxvalue ' || s.seqmax end, \
                                         case when s.seqcache <> 1 then 'cache ' || s.seqcache end, \
                                         case when s.seqcycle then 'cycle' end), '') || ')' \
                                  from pg_depend q join pg_sequence s on s.seqrelid = q.objid \
                                  join pg_class sc on sc.oid = q.objid \
                                  join pg_namespace sn on sn.oid = sc.relnamespace \
                                  join pg_class r on r.oid = q.refobjid \
                                 cross join lateral ( \
                                       select case s.seqtypid when 'int2'::regtype then 32767 \
                                                   when 'int4'::regtype then 2147483647 \
                                                   else 9223372036854775807 end as top) t \
                                 cross join lateral ( \
                                       select least(octet_length(r.relname), \
                                                    greatest(29, 58 - octet_length(a.attname))) as table_bytes, \
                                              least(octet_length(a.attname), \
                                                    greatest(29, 58 - octet_length(r.relname))) as column_bytes) cut \
                                 cross join lateral ( \
                                       select (select left(r.relname, max(i)) from generate_series(0, cut.table_bytes) i \
                                                where octet_length(left(r.relname, i)) <= cut.table_bytes) \
                                              || '_' \
                                              || (select left(a.attname, max(i)) from generate_series(0, cut.column_bytes) i \
                                                   where octet_length(left(a.attname, i)) <= cut.column_bytes) \
                                              || '_seq' as name) chosen \
                                 where q.classid = 'pg_class'::regclass \
                                   and q.refclassid = 'pg_class'::regclass \
                                   and q.refobjid = a.attrelid and q.refobjsubid = a.attnum \
                                   and q.deptype = 'i'), '') \
                            when a.attgenerated = 's' \
                              then 'generated always as (' || pg_get_expr(d.adbin, d.adrelid) || ') stored' \
                            when a.attgenerated = 'v' \
                              then 'generated always as (' || pg_get_expr(d.adbin, d.adrelid) || ') virtual' \
                            else pg_get_expr(d.adbin, d.adrelid) end";

fn columns(pg: &Pg<'_>, owner: &str, table: &str, keys: bool) -> Result<Vec<TableColumn>, DbError> {
    let (owner, table) = stored(pg, owner, table)?;
    let pk = if keys {
        "(select k.n from pg_index i \
           cross join lateral unnest(i.indkey::int2[]) with ordinality k(attnum, n) \
          where i.indrelid = c.oid and i.indisprimary and k.attnum = a.attnum)"
    } else {
        "null"
    };
    let rows = pg.rows(
        &format!(
            "select a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, {DEFAULT}, \
                    col_description(c.oid, a.attnum), {pk} \
               from pg_attribute a \
               join pg_class c on c.oid = a.attrelid \
               join pg_namespace n on n.oid = c.relnamespace \
               left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
              where n.nspname = $1 and c.relname = $2 and a.attnum > 0 and not a.attisdropped \
              order by a.attnum"
        ),
        &[&owner, &table],
    )?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let mut values = row.into_iter();
            let mut next = || values.next().flatten();
            TableColumn {
                name: next().unwrap_or_default(),
                data_type: next().unwrap_or_default(),
                nullable: next().as_deref() != Some("t"),
                default: next(),
                comment: next(),
                pk_position: next().and_then(|p| p.parse().ok()),
            }
        })
        .collect())
}

/// `n` (`pg_namespace`) and `c` (`pg_class`) of the table or view.
const RELATION: &str = "join pg_class c on c.oid = {rel} join pg_namespace n on n.oid = c.relnamespace \
                         where n.nspname = $1 and c.relname = $2";

fn details(pg: &Pg<'_>, owner: &str, name: &str, detail: Detail) -> Result<MetaResult, DbError> {
    let relation = |rel: &str| RELATION.replace("{rel}", rel);
    let sql = match detail {
        Detail::Indexes => format!(
            "select i.relname as \"INDEX_NAME\", am.amname as \"INDEX_TYPE\", \
                    case when x.indisprimary then 'PRIMARY' when x.indisunique then 'UNIQUE' \
                         else 'NONUNIQUE' end as \"UNIQUENESS\", \
                    pg_get_indexdef(x.indexrelid, 0, true) as \"COLUMNS\", null as \"INCLUDE\", \
                    pg_get_expr(x.indpred, x.indrelid, true) as \"WHERE\" \
               from pg_index x join pg_class i on i.oid = x.indexrelid \
               join pg_am am on am.oid = i.relam {} \
              order by x.indisprimary desc, i.relname",
            relation("x.indrelid")
        ),
        Detail::Constraints => format!(
            "select k.conname as \"CONSTRAINT_NAME\", \
                    case k.contype when 'p' then 'Primary key' when 'u' then 'Unique' \
                         when 'f' then 'Foreign key' when 'c' then 'Check' \
                         when 'x' then 'Exclusion' else k.contype::text end as \"TYPE\", \
                    pg_get_constraintdef(k.oid, true) as \"DEFINITION\", \
                    case when k.contype = 'f' then k.confrelid::regclass::text end as \"REFERENCES\" \
               from pg_constraint k {} and k.contype in ('p', 'u', 'f', 'c', 'x') \
              order by position(k.contype::text in 'pufcx'), k.conname",
            relation("k.conrelid")
        ),
        Detail::Triggers => format!(
            "select t.tgname as \"TRIGGER_NAME\", \
                    case t.tgenabled when 'D' then 'DISABLED' else 'ENABLED' end as \"STATUS\", \
                    pg_get_triggerdef(t.oid, true) as \"DEFINITION\" \
               from pg_trigger t {} and not t.tgisinternal \
              order by t.tgname",
            relation("t.tgrelid")
        ),
        Detail::Grants => "select grantee as \"GRANTEE\", privilege_type as \"PRIVILEGE\", \
                                  is_grantable as \"GRANTABLE\" \
                             from information_schema.role_table_grants \
                            where table_schema = $1 and table_name = $2 \
                           union all \
                           select grantee, privilege_type, is_grantable \
                             from information_schema.role_routine_grants \
                            where routine_schema = $1 and routine_name = $2 \
                            order by 1, 2"
            .to_owned(),
        Detail::Uses => format!(
            "select distinct rn.nspname as \"OWNER\", r.relname as \"NAME\", \
                    case r.relkind when 'v' then 'VIEW' when 'm' then 'MATERIALIZED VIEW' \
                         else 'TABLE' end as \"TYPE\" \
               from pg_rewrite w \
               join pg_depend d on d.objid = w.oid and d.classid = 'pg_rewrite'::regclass \
                and d.refclassid = 'pg_class'::regclass \
               join pg_class r on r.oid = d.refobjid \
               join pg_namespace rn on rn.oid = r.relnamespace {} and r.oid <> c.oid \
              order by 1, 2",
            relation("w.ev_class")
        ),
        Detail::UsedBy => format!(
            "select distinct vn.nspname as \"OWNER\", v.relname as \"NAME\", \
                    case v.relkind when 'm' then 'MATERIALIZED VIEW' else 'VIEW' end as \"TYPE\" \
               from pg_depend d \
               join pg_rewrite w on w.oid = d.objid and d.classid = 'pg_rewrite'::regclass \
               join pg_class v on v.oid = w.ev_class \
               join pg_namespace vn on vn.oid = v.relnamespace {} \
                and d.refclassid = 'pg_class'::regclass and v.oid <> c.oid \
             union \
             select tn.nspname, t.relname, 'TABLE' \
               from pg_constraint k \
               join pg_class t on t.oid = k.conrelid \
               join pg_namespace tn on tn.oid = t.relnamespace {} and k.contype = 'f' \
              order by 1, 3, 2",
            relation("d.refobjid"),
            relation("k.confrelid")
        ),
        Detail::Link => return Err(DbError::unsupported("Database links", Dialect::POSTGRES)),
    };
    // Names as seen from the object's schema: others' with their schema.
    let path = Dialect::POSTGRES.quote_ident(owner);
    let (names, mut rows) = on_path(pg, &path, || pg.query(&sql, &[owner, name]))?;
    if detail == Detail::Indexes {
        // The index's definition, read for its columns.
        for row in &mut rows {
            if let Some((columns, include)) = row[3].as_deref().and_then(index_columns) {
                row[3] = Some(columns);
                row[4] = include;
            }
        }
    }
    let columns = names
        .into_iter()
        .map(|name| Column {
            name,
            type_name: "text".into(),
            numeric: false,
        })
        .collect();
    Ok(MetaResult::Table { columns, rows })
}

/// The key columns of an index and its INCLUDE columns, if any, as its
/// definition (`… USING btree (code COLLATE "C" text_pattern_ops) INCLUDE
/// (n)`) writes them. A column's own definition leaves out its collation,
/// operator class and order (#228).
fn index_columns(definition: &str) -> Option<(String, Option<String>)> {
    use pythia_plsql::TokenKind;
    let tokens: Vec<_> = pythia_plsql::tokenize_in(definition, pythia_plsql::Syntax::Postgres)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    let is = |i: usize, word: &str| {
        tokens.get(i).is_some_and(|t| {
            t.kind.is_word() && definition[t.span.clone()].eq_ignore_ascii_case(word)
        })
    };
    // What the parenthesis at `open` encloses, and the token after it.
    let group = |open: usize| {
        if tokens.get(open)?.kind != TokenKind::LParen {
            return None;
        }
        let mut depth = 0;
        tokens.iter().enumerate().skip(open).find_map(|(i, t)| {
            match t.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth -= 1,
                _ => {}
            }
            (depth == 0).then(|| {
                let inside = &definition[tokens[open].span.end..t.span.start];
                (inside.trim().to_owned(), i + 1)
            })
        })
    };
    // `USING`, the access method, the key columns, `INCLUDE (…)`.
    let using = (0..tokens.len()).find(|&i| is(i, "USING"))?;
    let (columns, next) = group(using + 2)?;
    let include = if is(next, "INCLUDE") {
        group(next + 1).map(|(include, _)| include)
    } else {
        None
    };
    Some((columns, include))
}

/// An object's DDL; `runnable` ends each statement with `;` (a single
/// view or routine reads better without it).
fn ddl(
    pg: &Pg<'_>,
    owner: &str,
    name: &str,
    object_type: ObjectType,
    runnable: bool,
) -> Result<String, DbError> {
    let end = if runnable { ";" } else { "" };
    let missing = || {
        DbError::new(format!(
            "No {} {owner}.{name}",
            object_type.dictionary_name().to_lowercase()
        ))
    };
    match object_type {
        // Every name with its schema, as pg_dump writes them: run with
        // another search path, the DDL still means the same tables (#226).
        ObjectType::Table => on_path(pg, "", || table_ddl(pg, owner, name)),
        // A view's too: Edit runs its DDL in a window whose search path may
        // find other tables and functions first (#329).
        ObjectType::View | ObjectType::MaterializedView => {
            on_path(pg, "", || view_ddl(pg, owner, name, object_type, end))?.ok_or_else(missing)
        }
        // Each overload with its grants (#331).
        ObjectType::Function | ObjectType::Procedure => {
            let rows = pg.rows(
                "select pg_get_functiondef(p.oid), p.oid, \
                        case p.prokind when 'p' then 'procedure ' else 'function ' end \
                        || quote_ident(n.nspname) || '.' || quote_ident(p.proname) \
                        || '(' || pg_get_function_identity_arguments(p.oid) || ')' \
                   from pg_proc p \
                   join pg_namespace n on n.oid = p.pronamespace \
                  where n.nspname = $1 and p.proname = $2 and p.prokind in ('f', 'p') \
                  order by p.oid",
                &[owner, name],
            )?;
            let mut overloads = Vec::new();
            for row in rows {
                if let [Some(definition), Some(oid), Some(on)] = row.as_slice() {
                    let mut rest = String::new();
                    grants(pg, &mut rest, Privileges::Routine, oid, on)?;
                    overloads.push(followed(definition.trim_end(), &rest, end));
                }
            }
            if overloads.is_empty() {
                return Err(missing());
            }
            Ok(overloads.join("\n\n"))
        }
        ObjectType::Trigger => {
            let rows = pg.rows(
                "select pg_get_triggerdef(t.oid, true) from pg_trigger t \
                   join pg_class c on c.oid = t.tgrelid \
                   join pg_namespace n on n.oid = c.relnamespace \
                  where n.nspname = $1 and t.tgname = $2 and not t.tgisinternal \
                  order by c.relname",
                &[owner, name],
            )?;
            let triggers: Vec<String> = rows
                .into_iter()
                .filter_map(first)
                .map(|t| format!("{t}{end}"))
                .collect();
            if triggers.is_empty() {
                return Err(missing());
            }
            Ok(triggers.join("\n\n"))
        }
        ObjectType::Sequence => {
            let (owner, name) = stored(pg, owner, name)?;
            let (oid, create) = sequence_ddl(pg, &owner, &name, true)?.ok_or_else(missing)?;
            let on = format!("sequence {}", Dialect::POSTGRES.qualified(&owner, &name));
            let mut rest = String::new();
            grants(pg, &mut rest, Privileges::Relation, &oid, &on)?;
            Ok(followed(&create, &rest, end))
        }
        other => Err(DbError::new(format!(
            "PostgreSQL has no {}",
            other.plural().to_lowercase()
        ))),
    }
}

/// A statement of DDL and the statements after it, `rest`: with them it
/// takes its `;`, alone it ends with `end`.
fn followed(ddl: &str, rest: &str, end: &str) -> String {
    if rest.is_empty() {
        format!("{ddl}{end}")
    } else {
        format!("{ddl};{rest}")
    }
}

/// A relation's storage parameters, then its TOAST table's as `toast.…`, as
/// `with (…)` takes them (`c` is its `pg_class`); null without any. A value
/// is quoted unless it is a name that needs no quotes, as `pg_get_indexdef`
/// writes them: `fillfactor='70', autovacuum_enabled=off`.
const STORAGE: &str = "(select string_agg(p.name || '=' \
                                          || case when quote_ident(p.value) = p.value then p.value \
                                                  else quote_literal(p.value) end, \
                                          ', ' order by p.toast, p.n) \
                          from (select false as toast, o.n, quote_ident(split_part(o.x, '=', 1)) as name, \
                                       substr(o.x, strpos(o.x, '=') + 1) as value \
                                  from unnest(c.reloptions) with ordinality o(x, n) \
                                union all \
                                select true, o.n, 'toast.' || quote_ident(split_part(o.x, '=', 1)), \
                                       substr(o.x, strpos(o.x, '=') + 1) \
                                  from pg_class s cross join unnest(s.reloptions) with ordinality o(x, n) \
                                 where s.oid = c.reltoastrelid) p)";

/// `CREATE` of a view or materialized view as stored, if there is one. A
/// view is replaced in place, but Edit drops a materialized view and
/// creates it again from this: without them the new one would come without
/// its indexes, comments and grants (#301), and belong to the role that ran
/// it, populated, with the default storage (#330). So a materialized view's
/// `CREATE` has its access method, storage parameters and tablespace where
/// they aren't the defaults, and `with no data` where it has none; its
/// owner follows where it isn't the role reading the DDL. A role that can
/// drop the view but can't give it back to its owner gets an error, and
/// Edit's block leaves the view as it was.
fn view_ddl(
    pg: &Pg<'_>,
    owner: &str,
    name: &str,
    object_type: ObjectType,
    end: &str,
) -> Result<Option<String>, DbError> {
    let (owner, name) = stored(pg, owner, name)?;
    let Some(row) = pg.first(
        &format!(
            "select pg_get_viewdef(c.oid, true), c.oid, obj_description(c.oid, 'pg_class'), \
                    c.relispopulated, \
                    concat(case when a.amname <> current_setting('default_table_access_method') \
                                then ' using ' || quote_ident(a.amname) end, \
                           ' with (' || {STORAGE} || ')', \
                           ' tablespace ' || quote_ident(t.spcname)), \
                    case when r.rolname <> current_user then quote_ident(r.rolname) end \
               from pg_class c join pg_namespace n on n.oid = c.relnamespace \
               join pg_roles r on r.oid = c.relowner \
               left join pg_am a on a.oid = c.relam \
               left join pg_tablespace t on t.oid = c.reltablespace \
              where n.nspname = $1 and c.relname = $2"
        ),
        &[&owner, &name],
    )?
    else {
        return Ok(None);
    };
    let mut row = row.into_iter();
    let mut next = || row.next().flatten();
    let Some(definition) = next() else {
        return Ok(None);
    };
    let oid = next().unwrap_or_default();
    let comment = next();
    let populated = next().as_deref() != Some("f");
    let storage = next().unwrap_or_default();
    let role = next();
    let qualified = Dialect::POSTGRES.qualified(&owner, &name);
    let definition = definition.trim_end().trim_end_matches(';');
    let ddl = if object_type == ObjectType::View {
        format!("create or replace view {qualified} as\n{definition}")
    } else {
        let data = if populated { "" } else { "\nwith no data" };
        format!("create materialized view {qualified}{storage} as\n{definition}{data}")
    };
    let mut rest = String::new();
    if object_type == ObjectType::MaterializedView {
        if let Some(role) = role {
            rest.push_str(&format!(
                "\n\nalter materialized view {qualified} owner to {role};"
            ));
        }
        indexes_and_comments(
            pg,
            &mut rest,
            &oid,
            "materialized view",
            &qualified,
            comment,
        )?;
    }
    // A view's too, for a copy: Edit's CREATE OR REPLACE keeps them anyway
    // (#331).
    grants(pg, &mut rest, Privileges::Relation, &oid, &qualified)?;
    Ok(Some(followed(&ddl, &rest, end)))
}

/// `CREATE SEQUENCE` of a sequence as stored, without its `;`, and its oid,
/// if there is one. `owned` names the column that owns it (`serial`, `OWNED
/// BY`), so that a copy goes with the copy's column (#307); a table's DDL
/// makes the column the owner after the table. An identity column's
/// sequence is made by its table's DDL (#263): its own says so and sets the
/// identity's options, which next to a copy of the table gives the copy's
/// identity the same and elsewhere fails, where a `CREATE SEQUENCE` made a
/// sequence no column uses (#332).
fn sequence_ddl(
    pg: &Pg<'_>,
    owner: &str,
    name: &str,
    owned: bool,
) -> Result<Option<(String, String)>, DbError> {
    let row = pg.first(
        "select c.oid, d.deptype, format_type(s.seqtypid, null), s.seqincrement, s.seqmin, \
                s.seqmax, s.seqstart, s.seqcache, s.seqcycle, tn.nspname, t.relname, a.attname \
           from pg_sequence s join pg_class c on c.oid = s.seqrelid \
           join pg_namespace n on n.oid = c.relnamespace \
           left join pg_depend d on d.classid = 'pg_class'::regclass and d.objid = c.oid \
                and d.refclassid = 'pg_class'::regclass and d.deptype in ('a', 'i') \
           left join pg_class t on t.oid = d.refobjid \
           left join pg_namespace tn on tn.oid = t.relnamespace \
           left join pg_attribute a on a.attrelid = d.refobjid and a.attnum = d.refobjsubid \
          where n.nspname = $1 and c.relname = $2",
        &[owner, name],
    )?;
    Ok(row.map(|row| {
        let (head, row) = row.split_at(2);
        let (v, column) = row.split_at(7);
        let v: Vec<&str> = v.iter().map(|v| v.as_deref().unwrap_or_default()).collect();
        let oid = head[0].clone().unwrap_or_default();
        let column = match column {
            [Some(schema), Some(table), Some(column)] => Some((
                Dialect::POSTGRES.qualified(schema, table),
                Dialect::POSTGRES.quote_ident(column),
            )),
            _ => None,
        };
        if let Some((table, column)) = &column
            && head[1].as_deref() == Some("i")
        {
            let alter = format!(
                "-- The sequence of the identity column {table}.{column}, made by the table's DDL.\n\
                 alter table {table} alter column {column}\n  \
                 set increment by {} set minvalue {} set maxvalue {}\n  \
                 set start with {} set cache {} set {}",
                v[1],
                v[2],
                v[3],
                v[4],
                v[5],
                if v[6] == "t" { "cycle" } else { "no cycle" }
            );
            return (oid, alter);
        }
        let owned_by = match column {
            Some((table, column)) if owned => format!("\n  owned by {table}.{column}"),
            _ => String::new(),
        };
        let create = format!(
            "create sequence {} as {}\n  increment by {} minvalue {} maxvalue {}\n  \
             start with {} cache {}{}{owned_by}",
            Dialect::POSTGRES.qualified(owner, name),
            v[0],
            v[1],
            v[2],
            v[3],
            v[4],
            v[5],
            if v[6] == "t" { " cycle" } else { "" }
        );
        (oid, create)
    }))
}

/// The sequences that columns of the table own (`serial`, `OWNED BY`), in
/// its schema, as `(oid, sequence, column)`; not those of identity columns.
fn owned_sequences(pg: &Pg<'_>, oid: &str) -> Result<Vec<(String, String, String)>, DbError> {
    Ok(pg
        .rows(
            &format!(
                "select s.oid, s.relname, a.attname from pg_depend d \
                   join pg_class s on s.oid = d.objid and s.relkind = 'S' \
                   join pg_attribute a on a.attrelid = d.refobjid and a.attnum = d.refobjsubid \
                  where d.classid = 'pg_class'::regclass and d.refclassid = 'pg_class'::regclass \
                    and d.refobjid = {oid} and d.deptype = 'a' \
                  order by a.attnum, s.relname"
            ),
            &[],
        )?
        .into_iter()
        .filter_map(|row| {
            let mut values = row.into_iter().flatten();
            Some((values.next()?, values.next()?, values.next()?))
        })
        .collect())
}

/// The sequences its columns own, `CREATE TABLE` with its columns and
/// constraints, the sequences' owners, then its other indexes and
/// comments, its grants and the sequences'. A serial column's sequence goes
/// with the table, as `pg_dump` writes it: run elsewhere, the DDL makes the
/// copy a sequence of its own (#263), which roles use as they use the
/// table's (#331).
fn table_ddl(pg: &Pg<'_>, owner: &str, name: &str) -> Result<String, DbError> {
    let (owner, name) = stored(pg, owner, name)?;
    let qualified = Dialect::POSTGRES.qualified(&owner, &name);
    let table = pg
        .first(
            "select c.oid, c.relkind, pg_get_partkeydef(c.oid), obj_description(c.oid, 'pg_class') \
               from pg_class c join pg_namespace n on n.oid = c.relnamespace \
              where n.nspname = $1 and c.relname = $2",
            &[&owner, &name],
        )?
        .ok_or_else(|| DbError::new(format!("No table {owner}.{name}")))?;
    let mut table = table.into_iter();
    let oid = table.next().flatten().unwrap_or_default();
    let partitions = table.nth(1).flatten();
    let comment = table.next().flatten();
    let mut lines: Vec<String> = pg
        .rows(
            &format!(
                "select quote_ident(a.attname) || ' ' || format_type(a.atttypid, a.atttypmod) \
                        || coalesce(case when a.attidentity <> '' or a.attgenerated <> '' \
                                         then ' ' else ' default ' end || {DEFAULT}, '') \
                        || case when a.attnotnull then ' not null' else '' end \
                   from pg_attribute a \
                   left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
                  where a.attrelid = {oid} and a.attnum > 0 and not a.attisdropped \
                  order by a.attnum"
            ),
            &[],
        )?
        .into_iter()
        .filter_map(first)
        .collect();
    lines.extend(
        pg.rows(
            &format!(
                "select 'constraint ' || quote_ident(conname) || ' ' || pg_get_constraintdef(oid, true) \
                   from pg_constraint where conrelid = {oid} and contype in ('p', 'u', 'f', 'c', 'x') \
                  order by position(contype::text in 'pucfx'), conname"
            ),
            &[],
        )?
        .into_iter()
        .filter_map(first),
    );
    let sequences = owned_sequences(pg, &oid)?;
    let mut ddl = String::new();
    for (_, sequence, _) in &sequences {
        if let Some((_, create)) = sequence_ddl(pg, &owner, sequence, false)? {
            ddl.push_str(&format!("{create};\n\n"));
        }
    }
    ddl.push_str(&format!(
        "create table {qualified} (\n    {}\n)",
        lines.join(",\n    ")
    ));
    if let Some(key) = partitions {
        ddl.push_str(&format!(" partition by {key}"));
    }
    ddl.push(';');
    for (_, sequence, column) in &sequences {
        ddl.push_str(&format!(
            "\n\nalter sequence {} owned by {qualified}.{};",
            Dialect::POSTGRES.qualified(&owner, sequence),
            Dialect::POSTGRES.quote_ident(column)
        ));
    }
    indexes_and_comments(pg, &mut ddl, &oid, "table", &qualified, comment)?;
    grants(pg, &mut ddl, Privileges::Relation, &oid, &qualified)?;
    for (sequence_oid, sequence, _) in &sequences {
        let on = format!("sequence {}", Dialect::POSTGRES.qualified(&owner, sequence));
        grants(pg, &mut ddl, Privileges::Relation, sequence_oid, &on)?;
    }
    Ok(ddl)
}

/// The relation's indexes that no constraint makes, then the comments on
/// it (a `kind`: `table`, `materialized view`) and on its columns, each
/// statement with its `;`, appended to `ddl`.
fn indexes_and_comments(
    pg: &Pg<'_>,
    ddl: &mut String,
    oid: &str,
    kind: &str,
    qualified: &str,
    comment: Option<String>,
) -> Result<(), DbError> {
    let indexes = pg.rows(
        &format!(
            "select pg_get_indexdef(i.indexrelid) from pg_index i \
              where i.indrelid = {oid} \
                and not exists (select 1 from pg_constraint k where k.conindid = i.indexrelid \
                                   and k.conrelid = i.indrelid) \
              order by 1"
        ),
        &[],
    )?;
    for index in indexes.into_iter().filter_map(first) {
        ddl.push_str(&format!("\n\n{index};"));
    }
    if let Some(comment) = comment {
        ddl.push_str(&format!(
            "\n\ncomment on {kind} {qualified} is {};",
            literal(&comment)
        ));
    }
    let comments = pg.rows(
        &format!(
            "select quote_ident(attname), col_description(attrelid, attnum) from pg_attribute \
              where attrelid = {oid} and attnum > 0 and not attisdropped \
                and col_description(attrelid, attnum) is not null \
              order by attnum"
        ),
        &[],
    )?;
    for row in comments {
        if let [Some(column), Some(comment)] = row.as_slice() {
            ddl.push_str(&format!(
                "\ncomment on column {qualified}.{column} is {};",
                literal(comment)
            ));
        }
    }
    Ok(())
}

/// Whose privileges [`grants`] writes.
#[derive(Clone, Copy)]
enum Privileges {
    /// A table's, view's, materialized view's or sequence's (`pg_class`),
    /// and its columns'.
    Relation,
    /// A function's or procedure's (`pg_proc`).
    Routine,
}

/// The privileges of the object `oid`, appended to `ddl` as what makes a
/// new one's the same (#301, #331): one `revoke` for each grantee of the
/// privileges a new one has that it hasn't (`acldefault`: its owner's, and
/// `PUBLIC`'s `EXECUTE` on a routine), then one `grant` for each grantee
/// and grant option of those it has beyond them, a relation's columns'
/// with them, `grant select, update (m) on …`. `on` is what they are on:
/// `shop.t`, `sequence shop.s`, `function shop.f(x integer)`. An object
/// with the default privileges has none.
fn grants(
    pg: &Pg<'_>,
    ddl: &mut String,
    privileges: Privileges,
    oid: &str,
    on: &str,
) -> Result<(), DbError> {
    let (object, columns) = match privileges {
        Privileges::Relation => (
            format!(
                "select relacl as acl, \
                        acldefault(case relkind when 'S' then 's' else 'r' end::\"char\", relowner) as new \
                   from pg_class where oid = {oid}"
            ),
            format!(
                "select quote_ident(attname) as name, attnum, attacl as acl from pg_attribute \
                  where attrelid = {oid} and attnum > 0 and not attisdropped"
            ),
        ),
        Privileges::Routine => (
            format!(
                "select proacl as acl, acldefault('f', proowner) as new from pg_proc where oid = {oid}"
            ),
            "select null::text as name, null::int2 as attnum, null::aclitem[] as acl where false"
                .to_owned(),
        ),
    };
    let rows = pg.rows(
        &format!(
            "with o as ({object}), c as ({columns}), \
                  granted as (select x.grantee, x.is_grantable, \
                                     lower(x.privilege_type) as privilege, 0 as attnum \
                                from o cross join aclexplode(o.acl) x \
                              except \
                              select x.grantee, x.is_grantable, lower(x.privilege_type), 0 \
                                from o cross join aclexplode(o.new) x \
                              union \
                              select x.grantee, x.is_grantable, \
                                     lower(x.privilege_type) || ' (' || c.name || ')', c.attnum \
                                from c cross join aclexplode(c.acl) x), \
                  revoked as (select x.grantee, lower(x.privilege_type) as privilege \
                                from o cross join aclexplode(o.new) x where o.acl is not null \
                              except \
                              select x.grantee, lower(x.privilege_type) \
                                from o cross join aclexplode(o.acl) x) \
             select g.revoke, \
                    case when g.grantee = 0 then 'public' else quote_ident(r.rolname) end, \
                    g.is_grantable, string_agg(g.privilege, ', ' order by g.attnum, g.privilege) \
               from (select false as revoke, grantee, is_grantable, privilege, attnum \
                       from granted \
                     union all \
                     select true, grantee, false, privilege, 0 from revoked) g \
               left join pg_roles r on r.oid = g.grantee \
              group by 1, 2, 3 \
              order by 1 desc, 2, 3"
        ),
        &[],
    )?;
    for row in rows {
        if let [Some(revoke), Some(grantee), Some(grantable), Some(list)] = row.as_slice() {
            if revoke == "t" {
                ddl.push_str(&format!("\n\nrevoke {list} on {on} from {grantee};"));
            } else {
                let option = if grantable == "t" {
                    " with grant option"
                } else {
                    ""
                };
                ddl.push_str(&format!("\n\ngrant {list} on {on} to {grantee}{option};"));
            }
        }
    }
    Ok(())
}

/// The plan of `sql` from `EXPLAIN (FORMAT JSON)`. Bind variables become
/// parameters of a generic plan (PostgreSQL 16 and newer).
fn explain_plan(pg: &Pg<'_>, sql: &str) -> Result<MetaResult, DbError> {
    let (sql, binds) = numbered_binds(sql);
    let options = if binds == 0 {
        "format json"
    } else if pg.release >= (16, 0) {
        "format json, generic_plan"
    } else {
        return Err(DbError::new(
            "Explaining a statement with bind variables takes PostgreSQL 16 or newer",
        ));
    };
    let text: String = pg
        .rows(&format!("explain ({options}) {sql}"), &[])?
        .into_iter()
        .filter_map(first)
        .collect::<Vec<_>>()
        .join("\n");
    let plan: Value = serde_json::from_str(&text)
        .map_err(|e| DbError::new(format!("Can't read the plan: {e}")))?;
    let mut steps = Vec::new();
    if let Some(root) = plan.get(0).and_then(|p| p.get("Plan")) {
        plan_steps(root, None, 0, &mut steps);
    }
    Ok(plan_table(steps))
}

/// Replaces `:name` binds by `$1`, `$2`, … (the same name, the same
/// number); returns how many names there were.
fn numbered_binds(sql: &str) -> (String, usize) {
    let mut names: Vec<String> = Vec::new();
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for token in pythia_plsql::tokenize(sql) {
        if token.kind != pythia_plsql::TokenKind::BindVar {
            continue;
        }
        let name = sql[token.span.start + 1..token.span.end].to_lowercase();
        let n = match names.iter().position(|n| *n == name) {
            Some(i) => i + 1,
            None => {
                names.push(name);
                names.len()
            }
        };
        out.push_str(&sql[last..token.span.start]);
        out.push_str(&format!("${n}"));
        last = token.span.end;
    }
    out.push_str(&sql[last..]);
    (out, names.len())
}

/// Adds a plan node and its children, named the way `EXPLAIN` writes them
/// as text (`Hash Left Join`, `Index Scan using emp_pkey`).
fn plan_steps(node: &Value, parent: Option<usize>, depth: usize, steps: &mut Vec<PlanStep>) {
    let text = |key: &str| match node.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let mut operation = text("Node Type").unwrap_or_default();
    if operation == "Aggregate" {
        operation = match text("Strategy").as_deref() {
            Some("Hashed") => "HashAggregate",
            Some("Sorted") => "GroupAggregate",
            Some("Mixed") => "MixedAggregate",
            _ => "Aggregate",
        }
        .into();
    }
    if let Some(join) = text("Join Type").filter(|j| j != "Inner") {
        operation = match operation.strip_suffix(" Join") {
            Some(method) => format!("{method} {join} Join"),
            None => format!("{operation} {join} Join"),
        };
    }
    if let Some(index) = text("Index Name") {
        operation.push_str(&format!(" using {index}"));
    }
    if let Some(subplan) = text("Subplan Name") {
        operation = format!("{subplan}: {operation}");
    }
    let object = text("Relation Name")
        .map(|relation| match text("Alias") {
            Some(alias) if alias != relation => format!("{relation} {alias}"),
            _ => relation,
        })
        .or_else(|| text("CTE Name"))
        .or_else(|| text("Function Name"));
    let access = [
        "Index Cond",
        "Hash Cond",
        "Merge Cond",
        "Recheck Cond",
        "TID Cond",
    ]
    .into_iter()
    .find_map(text);
    let filters: Vec<String> = ["Join Filter", "Filter"]
        .into_iter()
        .filter_map(text)
        .collect();
    let index = steps.len();
    steps.push(PlanStep {
        parent,
        depth,
        operation,
        object,
        cost: text("Total Cost"),
        rows: text("Plan Rows"),
        access,
        filter: (!filters.is_empty()).then(|| filters.join(" and ")),
    });
    for child in node
        .get("Plans")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        plan_steps(child, Some(index), depth + 1, steps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_regexes() {
        assert_eq!(name_regex(""), "");
        assert_eq!(name_regex(" čaj "), "[čČ][aA][jJ]");
        assert_eq!(name_regex("ÖL_1"), "[Öö][Ll]\\_1");
        assert_eq!(name_regex("a*b?"), "[aA].*[bB].");
        assert_eq!(name_regex("x.%$("), "[xX]\\.\\%\\$\\(");
        // Without a single other case, a letter stands for itself.
        assert_eq!(name_regex("ß表"), "ß表");
    }

    #[test]
    fn plan_nodes() {
        let plan: Value = serde_json::from_str(
            r#"[{"Plan": {"Node Type": "Hash Join", "Join Type": "Left", "Total Cost": 35.5,
                 "Plan Rows": 10, "Hash Cond": "(e.deptno = d.deptno)",
                 "Plans": [
                   {"Node Type": "Seq Scan", "Relation Name": "emp", "Alias": "e",
                    "Total Cost": 22.7, "Plan Rows": 1270, "Filter": "(sal > 100)"},
                   {"Node Type": "Hash", "Plans": [
                     {"Node Type": "Index Scan", "Relation Name": "dept", "Alias": "dept",
                      "Index Name": "dept_pkey", "Index Cond": "(deptno = 10)"}]}]}}]"#,
        )
        .unwrap();
        let mut steps = Vec::new();
        plan_steps(&plan[0]["Plan"], None, 0, &mut steps);
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].operation, "Hash Left Join");
        assert_eq!(steps[0].access.as_deref(), Some("(e.deptno = d.deptno)"));
        assert_eq!(steps[1].object.as_deref(), Some("emp e"));
        assert_eq!(steps[1].filter.as_deref(), Some("(sal > 100)"));
        assert_eq!((steps[2].parent, steps[3].parent), (Some(0), Some(2)));
        assert_eq!(steps[3].operation, "Index Scan using dept_pkey");
        assert_eq!(steps[3].object.as_deref(), Some("dept"));
        assert_eq!(steps[3].depth, 2);
    }

    #[test]
    fn index_columns_as_defined() {
        assert_eq!(
            index_columns(
                "CREATE UNIQUE INDEX \"Cover (x)\" ON t USING btree (code COLLATE \"C\" \
                 text_pattern_ops, lower(\"Name\") DESC NULLS LAST) INCLUDE (name, n) \
                 WHERE (n > 0)"
            ),
            Some((
                "code COLLATE \"C\" text_pattern_ops, lower(\"Name\") DESC NULLS LAST".into(),
                Some("name, n".into())
            ))
        );
        assert_eq!(
            index_columns(
                "CREATE INDEX f ON only t USING gin (to_tsvector('portuguese)'::regconfig, name))"
            ),
            Some(("to_tsvector('portuguese)'::regconfig, name)".into(), None))
        );
        assert_eq!(index_columns("CREATE INDEX x ON t"), None);
    }

    #[test]
    fn binds_become_parameters() {
        assert_eq!(
            numbered_binds("select * from t where a = :x and b = :Y and c = :X and d::int = 1"),
            (
                "select * from t where a = $1 and b = $2 and c = $1 and d::int = 1".into(),
                2
            )
        );
    }
}
