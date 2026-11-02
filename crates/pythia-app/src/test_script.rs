//! "Test" scripts: an anonymous block that calls a procedure or function.
//!
//! IN parameters become bind variables (entered in the SQL window's
//! Variables panel, which keeps them); BOOLEAN, BLOB and XMLTYPE ones are
//! converted from their text. OUT/IN OUT parameters and function results
//! are declared as locals and printed with DBMS_OUTPUT: scalars as text,
//! collections element by element, records and objects field by field,
//! ref cursors row by row. What a text bind can't carry (collections,
//! records, objects, cursors) is a local for the test to fill, and what
//! can't be printed says so: the block always compiles.
//!
//! [`task`] reads the subprogram's arguments and describes their types from
//! the dictionary; [`test_block`] writes the block.

use std::collections::HashSet;

use pythia_db::oracle::Connection;
use pythia_db::{DbError, TaskFn};
use pythia_lsp_core::ParamMode;

use crate::documents::quote_ident;

/// A qualified name in lowercase for the block, except what is quoted
/// (`HR."MyPkg".LOAD` → `hr."MyPkg".load`): quoted names are case-sensitive.
pub fn lowercase_unquoted(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut quoted = false;
    for c in name.chars() {
        if c == '"' {
            quoted = !quoted;
        }
        if quoted {
            out.push(c);
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// A procedure or function to test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subprogram {
    pub params: Vec<Param>,
    /// A function's result.
    pub result: Option<Type>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub mode: ParamMode,
    pub ty: Type,
}

/// A parameter's or result's type: how a local of it is declared and how
/// its value is printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Type {
    /// E.g. `number`, `varchar2(32767)`, `hr.emp_pkg.t_list`,
    /// `hr.employees%rowtype`, `sys_refcursor`.
    pub decl: String,
    pub shape: Shape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// A value that prints as text: its base type name (`NUMBER`, `DATE`,
    /// `BOOLEAN`, `CLOB`, `BLOB`, `XMLTYPE`, …).
    Scalar(String),
    /// A nested table or varray (`nullable`: it can be NULL) or an
    /// index-by table (`text_keys`: indexed by VARCHAR2).
    Collection {
        nullable: bool,
        text_keys: bool,
        element: Box<Shape>,
    },
    /// A record or `%ROWTYPE` (not nullable) or an object (nullable).
    Fields {
        nullable: bool,
        fields: Vec<(String, Shape)>,
    },
    Cursor,
    /// What isn't printed, and why.
    Opaque(String),
}

/// Elements of a collection and rows of a cursor printed, at most.
const MAX_PRINTED: u32 = 100;
/// Records, objects and collections inside one another that are printed.
const MAX_DEPTH: usize = 4;

/// Builds the test block for `subprogram` called as `qualified` (e.g.
/// `hr.emp_pkg.raise_salary` or `hr.my_proc`).
pub fn test_block(qualified: &str, subprogram: &Subprogram) -> String {
    let mut locals = Locals::default();
    // Names first, so the helpers of the printing code don't take them.
    let result = subprogram
        .result
        .as_ref()
        .map(|ty| (locals.add("l_result", &ty.decl), ty));
    let mut args = Vec::new();
    // Before the call: IN OUT locals take the bind value, and what the
    // test must fill says so.
    let mut before = Vec::new();
    // OUT and IN OUT locals, to print after the call.
    let mut printed = Vec::new();
    for p in &subprogram.params {
        let name = ident(&p.name);
        let bind = format!(":{}", p.name.to_lowercase());
        let value = match p.mode {
            ParamMode::Out => None,
            ParamMode::In | ParamMode::InOut => from_bind(&p.ty.shape, &bind, &mut locals, &p.name),
        };
        if p.mode == ParamMode::In
            && let Some(value) = value
        {
            args.push(format!("{name} => {value}"));
            continue;
        }
        let local = locals.add(&format!("l_{}", local_base(&p.name)), &p.ty.decl);
        match (&p.mode, value) {
            (ParamMode::InOut, Some(value)) => before.push(format!("  {local} := {value};")),
            (ParamMode::In | ParamMode::InOut, None) => before.extend(fill_hint(&local, &p.ty)),
            _ => {}
        }
        if p.mode != ParamMode::In {
            printed.push((p.name.to_lowercase(), local.clone(), &p.ty.shape));
        }
        args.push(format!("{name} => {local}"));
    }
    let call_args = if args.is_empty() {
        String::new()
    } else {
        format!("(\n    {}\n  )", args.join(",\n    "))
    };
    let call = match &result {
        Some((local, _)) => format!("  {local} := {qualified}{call_args};"),
        None => format!("  {qualified}{call_args};"),
    };
    let mut after = Writer::default();
    if let Some((local, ty)) = &result {
        print(
            &mut after,
            &mut locals,
            &ty.shape,
            local,
            &Label::text("result"),
            0,
        );
    }
    for (label, local, shape) in printed {
        print(
            &mut after,
            &mut locals,
            shape,
            &local,
            &Label::text(&label),
            0,
        );
    }

    let mut out = format!("-- Test {qualified}\n");
    if !locals.decls.is_empty() {
        out.push_str("declare\n");
        for decl in &locals.decls {
            out.push_str(&format!("  {decl};\n"));
        }
    }
    out.push_str("begin\n");
    for line in before.iter().chain(std::iter::once(&call)) {
        out.push_str(line);
        out.push('\n');
    }
    for line in after.lines {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("end;\n");
    out
}

/// `p_name` → `name` for the local `l_name`; quoted names keep their
/// letters, without the quotes.
fn local_base(name: &str) -> String {
    let lower = name.to_lowercase();
    let base = lower.strip_prefix("p_").unwrap_or(&lower);
    let clean: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if clean.is_empty() {
        "value".into()
    } else {
        clean
    }
}

/// A name as written in the block: lowercase, or quoted if it must be.
fn ident(name: &str) -> String {
    let quoted = quote_ident(name);
    if quoted.starts_with('"') {
        quoted
    } else {
        quoted.to_lowercase()
    }
}

/// The locals of the block, each name used once.
#[derive(Default)]
struct Locals {
    used: HashSet<String>,
    decls: Vec<String>,
}

impl Locals {
    /// Declares a local named `base` (or `base2`, … if taken) of `decl`
    /// (with `:= value` if it starts with one); returns its name.
    fn add(&mut self, base: &str, decl: &str) -> String {
        let mut name = base.to_owned();
        let mut n = 1;
        while !self.used.insert(name.clone()) {
            n += 1;
            name = format!("{base}{n}");
        }
        self.decls.push(format!("{name} {decl}"));
        name
    }
}

/// The value of an IN (or the start of an IN OUT) parameter from its bind
/// variable, if a text bind can carry it.
fn from_bind(shape: &Shape, bind: &str, locals: &mut Locals, name: &str) -> Option<String> {
    let Shape::Scalar(t) = shape else {
        return None;
    };
    Some(match t.as_str() {
        "BOOLEAN" | "PL/SQL BOOLEAN" => {
            format!("case upper({bind}) when 'TRUE' then true when 'FALSE' then false end")
        }
        "BLOB" => format!("hextoraw({bind})"),
        // XMLTYPE(NULL) fails (ORA-19032): the text first.
        "XMLTYPE" => {
            let text = locals.add(
                &format!("l_{}_text", local_base(name)),
                &format!("varchar2(32767) := {bind}"),
            );
            format!("case when {text} is not null then xmltype({text}) end")
        }
        t if bindable(t) => bind.to_owned(),
        _ => return None,
    })
}

/// What the test has to give a local that no bind fills: a comment, and an
/// empty collection to add to.
fn fill_hint(local: &str, ty: &Type) -> Vec<String> {
    match &ty.shape {
        Shape::Cursor => vec![format!("  -- TODO: open {local} for select …;")],
        Shape::Collection { nullable: true, .. } => vec![
            format!("  {local} := {}();", ty.decl),
            format!("  -- TODO: fill {local}"),
        ],
        _ => vec![format!("  -- TODO: give {local} a value")],
    }
}

/// Scalar types that come from a text bind and print as text as they are.
fn bindable(t: &str) -> bool {
    matches!(
        t,
        "NUMBER"
            | "FLOAT"
            | "INTEGER"
            | "INT"
            | "SMALLINT"
            | "DECIMAL"
            | "DEC"
            | "NUMERIC"
            | "REAL"
            | "DOUBLE PRECISION"
            | "BINARY_INTEGER"
            | "PLS_INTEGER"
            | "NATURAL"
            | "NATURALN"
            | "POSITIVE"
            | "POSITIVEN"
            | "SIGNTYPE"
            | "SIMPLE_INTEGER"
            | "BINARY_FLOAT"
            | "BINARY_DOUBLE"
            | "VARCHAR2"
            | "VARCHAR"
            | "STRING"
            | "CHAR"
            | "CHARACTER"
            | "NCHAR"
            | "NVARCHAR2"
            | "LONG"
            | "RAW"
            | "LONG RAW"
            | "ROWID"
            | "UROWID"
            | "DATE"
            | "TIMESTAMP"
            | "TIMESTAMP WITH TIME ZONE"
            | "TIMESTAMP WITH LOCAL TIME ZONE"
            | "INTERVAL DAY TO SECOND"
            | "INTERVAL YEAR TO MONTH"
            | "CLOB"
            | "NCLOB"
    )
}

/// The shape of a scalar type (`TIMESTAMP(6)` is `TIMESTAMP`).
fn scalar(type_name: &str) -> Shape {
    let t = base_type(type_name);
    if bindable(&t)
        || matches!(
            t.as_str(),
            "BOOLEAN" | "PL/SQL BOOLEAN" | "BLOB" | "XMLTYPE"
        )
    {
        Shape::Scalar(t)
    } else {
        Shape::Opaque(format!("a {} isn't printed", type_name.to_lowercase()))
    }
}

/// `TIMESTAMP(6) WITH TIME ZONE` → `TIMESTAMP WITH TIME ZONE`.
fn base_type(type_name: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in type_name.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if depth == 0 => out.push(c.to_ascii_uppercase()),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The text of a scalar for DBMS_OUTPUT.
fn display(expr: &str, t: &str) -> String {
    match t {
        "BOOLEAN" | "PL/SQL BOOLEAN" => {
            format!("case when {expr} then 'TRUE' when not {expr} then 'FALSE' else 'NULL' end")
        }
        "DATE" => format!("to_char({expr}, 'YYYY-MM-DD HH24:MI:SS')"),
        "CLOB" | "NCLOB" => format!("dbms_lob.substr({expr}, 4000)"),
        "BLOB" => format!(
            "case when {expr} is not null then dbms_lob.getlength({expr}) || ' bytes: ' \
             || rawtohex(dbms_lob.substr({expr}, 100)) end"
        ),
        "XMLTYPE" => {
            format!(
                "case when {expr} is not null then dbms_lob.substr({expr}.getclobval(), 4000) end"
            )
        }
        _ => expr.to_owned(),
    }
}

/// The start of a printed line: text and PL/SQL expressions, e.g.
/// `result(` `l_i` `).name`.
#[derive(Debug, Clone)]
struct Label(Vec<(bool, String)>);

impl Label {
    fn text(text: &str) -> Self {
        Self(vec![(false, text.to_owned())])
    }

    fn and_text(&self, text: &str) -> Self {
        let mut parts = self.0.clone();
        parts.push((false, text.to_owned()));
        Self(parts)
    }

    fn and_expr(&self, expr: &str) -> Self {
        let mut parts = self.0.clone();
        parts.push((true, expr.to_owned()));
        Self(parts)
    }

    /// As a PL/SQL string expression, followed by `suffix` (text), e.g.
    /// `'result(' || l_i || ') = '`.
    fn with(&self, suffix: &str) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut text = String::new();
        for (is_expr, part) in self.0.iter().chain([(false, suffix.to_owned())].iter()) {
            if *is_expr {
                if !text.is_empty() {
                    parts.push(format!(
                        "'{}'",
                        std::mem::take(&mut text).replace('\'', "''")
                    ));
                }
                parts.push(part.clone());
            } else {
                text.push_str(part);
            }
        }
        if !text.is_empty() || parts.is_empty() {
            parts.push(format!("'{}'", text.replace('\'', "''")));
        }
        parts.join(" || ")
    }
}

/// Indented lines of the block.
#[derive(Default)]
struct Writer {
    lines: Vec<String>,
    indent: usize,
}

impl Writer {
    fn line(&mut self, line: impl AsRef<str>) {
        self.lines
            .push(format!("{}{}", "  ".repeat(self.indent + 1), line.as_ref()));
    }

    fn put(&mut self, text: String) {
        self.line(format!("dbms_output.put_line({text});"));
    }
}

/// Prints `expr` of `shape`, labelled `label`.
fn print(
    w: &mut Writer,
    locals: &mut Locals,
    shape: &Shape,
    expr: &str,
    label: &Label,
    depth: usize,
) {
    let too_deep = depth >= MAX_DEPTH && !matches!(shape, Shape::Scalar(_) | Shape::Opaque(_));
    if too_deep {
        w.put(label.with(": nested too deep to print"));
        return;
    }
    match shape {
        Shape::Scalar(t) => w.put(format!("{} || {}", label.with(" = "), display(expr, t))),
        Shape::Opaque(why) => w.put(label.with(&format!(": {why}"))),
        Shape::Fields { fields, .. } if fields.is_empty() => {
            w.put(label.with(": its contents aren't printed"));
        }
        Shape::Fields { nullable, fields } => {
            let close = if_null(w, *nullable, expr, label);
            for (name, field) in fields {
                let field_expr = format!("{expr}.{}", ident(name));
                let field_label = label.and_text(&format!(".{}", name.to_lowercase()));
                print(w, locals, field, &field_expr, &field_label, depth + 1);
            }
            end_if(w, close);
        }
        Shape::Collection {
            nullable,
            text_keys,
            element,
        } => {
            let close = if_null(w, *nullable, expr, label);
            let index = locals.add(
                "l_i",
                if *text_keys {
                    "varchar2(32767)"
                } else {
                    "pls_integer"
                },
            );
            let count = locals.add("l_n", "pls_integer");
            w.put(format!(
                "{} || {expr}.count || ' element(s)'",
                label.with(": ")
            ));
            w.line(format!("{count} := 0;"));
            w.line(format!("{index} := {expr}.first;"));
            w.line(format!(
                "while {index} is not null and {count} < {MAX_PRINTED} loop"
            ));
            w.indent += 1;
            w.line(format!("{count} := {count} + 1;"));
            let key = if *text_keys {
                format!("'''' || {index} || ''''")
            } else {
                index.clone()
            };
            let element_label = label.and_text("(").and_expr(&key).and_text(")");
            let element_expr = format!("{expr}({index})");
            print(w, locals, element, &element_expr, &element_label, depth + 1);
            w.line(format!("{index} := {expr}.next({index});"));
            w.indent -= 1;
            w.line("end loop;");
            w.line(format!("if {index} is not null then"));
            w.indent += 1;
            w.put(format!(
                "{} || ({expr}.count - {count}) || ' more not printed'",
                label.with(": ")
            ));
            w.indent -= 1;
            w.line("end if;");
            end_if(w, close);
        }
        Shape::Cursor if depth > 0 => w.put(label.with(": a cursor isn't printed")),
        Shape::Cursor => print_cursor(w, locals, expr, label),
    }
}

/// `if expr is null then (prints NULL) else`, for what can be NULL as a
/// whole; returns whether to close it.
fn if_null(w: &mut Writer, nullable: bool, expr: &str, label: &Label) -> bool {
    if nullable {
        w.line(format!("if {expr} is null then"));
        w.indent += 1;
        w.put(label.with(" = NULL"));
        w.indent -= 1;
        w.line("else");
        w.indent += 1;
    }
    nullable
}

fn end_if(w: &mut Writer, close: bool) {
    if close {
        w.indent -= 1;
        w.line("end if;");
    }
}

/// DBMS_SQL column types fetched as text: VARCHAR2, NUMBER, DATE, RAW,
/// ROWID, CHAR, CLOB, BINARY_FLOAT/DOUBLE, timestamps, intervals, UROWID.
const TEXT_COLUMNS: &str = "1, 2, 12, 23, 69, 96, 100, 101, 112, 180, 181, 182, 183, 208, 231";

/// Prints a cursor's rows, its columns as `name = value`.
fn print_cursor(w: &mut Writer, locals: &mut Locals, expr: &str, label: &Label) {
    let cursor = locals.add("l_cursor", "integer");
    let columns = locals.add("l_columns", "integer");
    let described = locals.add("l_described", "dbms_sql.desc_tab2");
    let value = locals.add("l_value", "varchar2(32767)");
    let row = locals.add("l_row", "varchar2(4000)");
    let rows = locals.add("l_rows", "pls_integer");
    w.line(format!("if not {expr}%isopen then"));
    w.indent += 1;
    w.put(label.with(": not open"));
    w.indent -= 1;
    w.line("else");
    w.indent += 1;
    w.line(format!("{cursor} := dbms_sql.to_cursor_number({expr});"));
    w.line(format!(
        "dbms_sql.describe_columns2({cursor}, {columns}, {described});"
    ));
    w.line(format!("for i in 1 .. {columns} loop"));
    w.indent += 1;
    w.line(format!(
        "if {described}(i).col_type in ({TEXT_COLUMNS}) then"
    ));
    w.indent += 1;
    w.line(format!(
        "dbms_sql.define_column({cursor}, i, {value}, 32767);"
    ));
    w.indent -= 1;
    w.line("end if;");
    w.indent -= 1;
    w.line("end loop;");
    w.line(format!("{rows} := 0;"));
    w.line(format!(
        "while {rows} < {MAX_PRINTED} and dbms_sql.fetch_rows({cursor}) > 0 loop"
    ));
    w.indent += 1;
    w.line(format!("{rows} := {rows} + 1;"));
    w.line(format!("{row} := null;"));
    w.line(format!("for i in 1 .. {columns} loop"));
    w.indent += 1;
    w.line(format!(
        "if {described}(i).col_type in ({TEXT_COLUMNS}) then"
    ));
    w.indent += 1;
    w.line(format!("dbms_sql.column_value({cursor}, i, {value});"));
    w.indent -= 1;
    w.line("else");
    w.indent += 1;
    w.line(format!("{value} := '(not shown)';"));
    w.indent -= 1;
    w.line("end if;");
    w.line(format!(
        "{row} := substr({row} || case when i > 1 then ', ' end || lower({described}(i).col_name) \
         || ' = ' || nvl(substr({value}, 1, 1000), 'NULL'), 1, 4000);"
    ));
    w.indent -= 1;
    w.line("end loop;");
    let row_label = label.and_text("(").and_expr(&rows).and_text(")");
    w.put(format!("{} || {row}", row_label.with(": ")));
    w.indent -= 1;
    w.line("end loop;");
    w.line(format!(
        "if {rows} = {MAX_PRINTED} and dbms_sql.fetch_rows({cursor}) > 0 then"
    ));
    w.indent += 1;
    w.put(label.with(": more rows not printed"));
    w.indent -= 1;
    w.line("end if;");
    w.line(format!("dbms_sql.close_cursor({cursor});"));
    w.indent -= 1;
    w.line("end if;");
}

/// A test block, ready to open.
pub struct Script {
    pub block: String,
    /// The subprogram's name as the dictionary has it (to stop on entry
    /// when debugging).
    pub subprogram: String,
}

/// Reads subprogram `name` (of `package`, or standalone) of `owner` with
/// its argument types, and builds its test block called as `qualified`.
/// `overload`: that one, else the first. The result is a [`Script`].
pub fn task(
    owner: String,
    package: Option<String>,
    name: String,
    overload: Option<u32>,
    qualified: String,
) -> TaskFn {
    Box::new(move |conn: &Connection| {
        let (subprogram, found) = describe(conn, &owner, package.as_deref(), &name, overload)?;
        let script = Script {
            block: test_block(&qualified, &subprogram),
            subprogram: found,
        };
        Ok(Box::new(script) as Box<dyn std::any::Any + Send>)
    })
}

/// One `ALL_ARGUMENTS` row of a subprogram (data level 0).
struct Argument {
    position: u32,
    name: Option<String>,
    in_out: String,
    data_type: Option<String>,
    pls_type: Option<String>,
    type_owner: Option<String>,
    type_name: Option<String>,
    type_subname: Option<String>,
}

/// The subprogram's parameters and result with their types, and its name
/// as the dictionary has it. `name` is the dictionary's: `MyProc` isn't
/// `MYPROC` (#359).
fn describe(
    conn: &Connection,
    owner: &str,
    package: Option<&str>,
    name: &str,
    overload: Option<u32>,
) -> Result<(Subprogram, String), DbError> {
    #[allow(clippy::type_complexity)]
    let rows = conn.query_as::<(
        Option<String>,
        String,
        u32,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )>(
        "select a.overload, a.object_name, a.position, a.argument_name, a.in_out, \
                a.data_type, a.pls_type, a.type_owner, a.type_name, a.type_subname \
           from all_arguments a \
          where a.owner = :1 and nvl(a.package_name, '-') = :2 \
            and a.object_name = :3 and a.data_level = 0 \
          order by to_number(a.overload) nulls first, a.position",
        &[&owner, &package.unwrap_or("-"), &name],
    )?;
    // Each overload's rows; the asked one, else the first.
    let mut overloads: Vec<(Option<u32>, String, Vec<Argument>)> = Vec::new();
    for row in rows {
        let (ov, object, position, arg, in_out, data_type, pls_type, t_owner, t_name, t_sub) = row?;
        let ov: Option<u32> = ov.and_then(|o| o.parse().ok());
        if overloads.last().is_none_or(|(o, ..)| *o != ov) {
            overloads.push((ov, object, Vec::new()));
        }
        overloads.last_mut().expect("pushed").2.push(Argument {
            position,
            name: arg,
            in_out,
            data_type,
            pls_type,
            type_owner: t_owner,
            type_name: t_name,
            type_subname: t_sub,
        });
    }
    let chosen = overloads
        .iter()
        .position(|(o, ..)| overload.is_some() && *o == overload)
        .unwrap_or(0);
    // A subprogram without parameters may have no rows at all.
    let Some((_, found, args)) = overloads.into_iter().nth(chosen) else {
        let subprogram = Subprogram {
            params: Vec::new(),
            result: None,
        };
        return Ok((subprogram, name.to_owned()));
    };
    let mut subprogram = Subprogram {
        params: Vec::new(),
        result: None,
    };
    for arg in &args {
        match (arg.position, &arg.name) {
            (0, _) => subprogram.result = Some(argument_type(conn, arg)),
            (_, Some(name)) => subprogram.params.push(Param {
                name: name.clone(),
                mode: match arg.in_out.as_str() {
                    "OUT" => ParamMode::Out,
                    "IN/OUT" => ParamMode::InOut,
                    _ => ParamMode::In,
                },
                ty: argument_type(conn, arg),
            }),
            // 11g's row for a subprogram without parameters.
            (_, None) => {}
        }
    }
    Ok((subprogram, found))
}

/// The type of an argument: its declaration and shape.
fn argument_type(conn: &Connection, arg: &Argument) -> Type {
    let data_type = arg.data_type.as_deref().unwrap_or_default();
    let owner = arg.type_owner.as_deref();
    // A strong cursor's TYPE_SUBNAME is its row type, not the cursor type;
    // SYS_REFCURSOR takes any.
    if data_type == "REF CURSOR" {
        return Type {
            decl: "sys_refcursor".into(),
            shape: Shape::Cursor,
        };
    }
    let Some(type_name) = arg.type_name.as_deref() else {
        let scalar_type = arg.pls_type.as_deref().unwrap_or(data_type);
        return Type {
            decl: scalar_decl(scalar_type),
            shape: scalar(scalar_type),
        };
    };
    // Types resolved through a public synonym (XMLTYPE) are named as is.
    let owner = owner.filter(|o| *o != "PUBLIC");
    let prefix = owner.map(|o| format!("{}.", ident(o))).unwrap_or_default();
    if arg.pls_type.as_deref() == Some("ROWTYPE") {
        return Type {
            decl: format!("{prefix}{}%rowtype", ident(type_name)),
            shape: owner.map_or_else(
                || Shape::Opaque("its columns aren't known".into()),
                |owner| rowtype(conn, owner, type_name),
            ),
        };
    }
    if data_type == "OPAQUE/XMLTYPE" || type_name == "XMLTYPE" {
        return Type {
            decl: "xmltype".into(),
            shape: Shape::Scalar("XMLTYPE".into()),
        };
    }
    let decl = match &arg.type_subname {
        Some(sub) => format!("{prefix}{}.{}", ident(type_name), ident(sub)),
        None => format!("{prefix}{}", ident(type_name)),
    };
    let shape = match (owner, &arg.type_subname) {
        (Some(owner), Some(sub)) => named(conn, owner, Some(type_name), sub, 0),
        (Some(owner), None) => named(conn, owner, None, type_name, 0),
        (None, _) => Shape::Opaque(format!("a {} isn't printed", decl)),
    };
    Type { decl, shape }
}

/// How a local of a scalar argument type is declared.
fn scalar_decl(type_name: &str) -> String {
    let t = type_name.to_ascii_uppercase();
    match t.as_str() {
        "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" | "RAW" => {
            format!("{}(32767)", t.to_lowercase())
        }
        "PL/SQL BOOLEAN" | "BOOLEAN" => "boolean".into(),
        _ => t.to_lowercase(),
    }
}

/// The fields of `owner.table%ROWTYPE`.
fn rowtype(conn: &Connection, owner: &str, table: &str) -> Shape {
    let rows = conn.query_as::<(String, Option<String>, Option<String>)>(
        "select column_name, data_type_owner, data_type from all_tab_columns \
          where owner = :1 and table_name = :2 order by column_id",
        &[&owner, &table],
    );
    let fields: Result<Vec<_>, DbError> = rows.map_err(DbError::from).and_then(|rows| {
        rows.map(|row| {
            let (column, type_owner, data_type) = row?;
            let data_type = data_type.unwrap_or_default();
            let shape = match type_owner {
                Some(o) => element(conn, Some(&o), None, &data_type, 1),
                None => scalar(&data_type),
            };
            Ok((column, shape))
        })
        .collect()
    });
    match fields {
        Ok(fields) if !fields.is_empty() => Shape::Fields {
            nullable: false,
            fields,
        },
        _ => Shape::Opaque("its columns aren't known".into()),
    }
}

/// A field's or element's type: a scalar (no owner) or a named type.
fn element(
    conn: &Connection,
    owner: Option<&str>,
    package: Option<&str>,
    name: &str,
    depth: usize,
) -> Shape {
    match owner {
        _ if name == "XMLTYPE" => Shape::Scalar("XMLTYPE".into()),
        Some(owner) if owner != "PUBLIC" => named(conn, owner, package, name, depth),
        _ => scalar(name),
    }
}

/// A named type: a package's record or collection type (`package`) or a
/// schema's object or collection type.
fn named(conn: &Connection, owner: &str, package: Option<&str>, name: &str, depth: usize) -> Shape {
    let unknown = || Shape::Opaque(format!("a {} isn't printed", name.to_lowercase()));
    if depth >= MAX_DEPTH {
        return unknown();
    }
    let result = match package {
        Some(package) => package_type(conn, owner, package, name, depth),
        None => schema_type(conn, owner, name, depth),
    };
    // ALL_PLSQL_TYPES & co need 12.1.
    result.ok().flatten().unwrap_or_else(unknown)
}

fn package_type(
    conn: &Connection,
    owner: &str,
    package: &str,
    name: &str,
    depth: usize,
) -> Result<Option<Shape>, DbError> {
    let binds: [&dyn pythia_db::oracle::sql_type::ToSql; 3] = [&owner, &package, &name];
    let typecode = conn
        .query_row_as::<String>(
            "select typecode from all_plsql_types \
              where owner = :1 and package_name = :2 and type_name = :3",
            &binds,
        )
        .ok();
    Ok(match typecode.as_deref() {
        Some("COLLECTION") => {
            let (coll_type, e_owner, e_package, e_name, index_by) = conn.query_row_as::<(
                String,
                Option<String>,
                Option<String>,
                String,
                Option<String>,
            )>(
                "select coll_type, elem_type_owner, elem_type_package, elem_type_name, \
                            index_by \
                       from all_plsql_coll_types \
                      where owner = :1 and package_name = :2 and type_name = :3",
                &binds,
            )?;
            let index_table = coll_type == "PL/SQL INDEX TABLE";
            Some(Shape::Collection {
                nullable: !index_table,
                text_keys: index_table && index_by.as_deref() == Some("VARCHAR2"),
                element: Box::new(element(
                    conn,
                    e_owner.as_deref(),
                    e_package.as_deref(),
                    &e_name,
                    depth + 1,
                )),
            })
        }
        Some("PL/SQL RECORD") => {
            let rows = conn.query_as::<(
                String,
                Option<String>,
                Option<String>,
                String,
                Option<String>,
            )>(
                "select attr_name, attr_type_owner, attr_type_package, attr_type_name, \
                        attr_type_mod \
                   from all_plsql_type_attrs \
                  where owner = :1 and package_name = :2 and type_name = :3 order by attr_no",
                &binds,
            )?;
            let mut fields = Vec::new();
            for row in rows {
                let (attr, a_owner, a_package, a_name, modifier) = row?;
                let shape = if modifier.is_some() {
                    Shape::Opaque("a reference isn't printed".into())
                } else {
                    element(
                        conn,
                        a_owner.as_deref(),
                        a_package.as_deref(),
                        &a_name,
                        depth + 1,
                    )
                };
                fields.push((attr, shape));
            }
            Some(Shape::Fields {
                nullable: false,
                fields,
            })
        }
        _ => None,
    })
}

fn schema_type(
    conn: &Connection,
    owner: &str,
    name: &str,
    depth: usize,
) -> Result<Option<Shape>, DbError> {
    let typecode = conn
        .query_row_as::<String>(
            "select typecode from all_types where owner = :1 and type_name = :2",
            &[&owner, &name],
        )
        .ok();
    Ok(match typecode.as_deref() {
        Some("COLLECTION") => {
            let (e_owner, e_name) = conn.query_row_as::<(Option<String>, String)>(
                "select elem_type_owner, elem_type_name from all_coll_types \
                  where owner = :1 and type_name = :2",
                &[&owner, &name],
            )?;
            Some(Shape::Collection {
                nullable: true,
                text_keys: false,
                element: Box::new(element(conn, e_owner.as_deref(), None, &e_name, depth + 1)),
            })
        }
        Some("OBJECT") => {
            let rows = conn.query_as::<(String, Option<String>, String, Option<String>)>(
                "select attr_name, attr_type_owner, attr_type_name, attr_type_mod \
                   from all_type_attrs where owner = :1 and type_name = :2 order by attr_no",
                &[&owner, &name],
            )?;
            let mut fields = Vec::new();
            for row in rows {
                let (attr, a_owner, a_name, modifier) = row?;
                let shape = if modifier.is_some() {
                    Shape::Opaque("a reference isn't printed".into())
                } else {
                    element(conn, a_owner.as_deref(), None, &a_name, depth + 1)
                };
                fields.push((attr, shape));
            }
            Some(Shape::Fields {
                nullable: true,
                fields,
            })
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(name: &str, mode: ParamMode, decl: &str, shape: Shape) -> Param {
        Param {
            name: name.into(),
            mode,
            ty: Type {
                decl: decl.into(),
                shape,
            },
        }
    }

    fn scalar_param(name: &str, mode: ParamMode, t: &str) -> Param {
        param(name, mode, &scalar_decl(t), scalar(t))
    }

    #[test]
    fn quoted_names_keep_their_case() {
        assert_eq!(lowercase_unquoted("HR.EMP_PKG.RAISE"), "hr.emp_pkg.raise");
        assert_eq!(
            lowercase_unquoted("HR.\"MyPkg\".\"Load\""),
            "hr.\"MyPkg\".\"Load\""
        );
    }

    #[test]
    fn function_with_out_param() {
        let subprogram = Subprogram {
            params: vec![
                scalar_param("P_ID", ParamMode::In, "NUMBER"),
                scalar_param("P_NAME", ParamMode::Out, "VARCHAR2"),
            ],
            result: Some(Type {
                decl: "date".into(),
                shape: scalar("DATE"),
            }),
        };
        assert_eq!(
            test_block("hr.pkg.f", &subprogram),
            "-- Test hr.pkg.f\n\
             declare\n  l_result date;\n  l_name varchar2(32767);\n\
             begin\n  l_result := hr.pkg.f(\n    p_id => :p_id,\n    p_name => l_name\n  );\n\
             \x20 dbms_output.put_line('result = ' || to_char(l_result, 'YYYY-MM-DD HH24:MI:SS'));\n\
             \x20 dbms_output.put_line('p_name = ' || l_name);\n\
             end;\n"
        );
    }

    #[test]
    fn procedure_without_params() {
        let subprogram = Subprogram {
            params: vec![],
            result: None,
        };
        assert_eq!(
            test_block("p", &subprogram),
            "-- Test p\nbegin\n  p;\nend;\n"
        );
    }

    #[test]
    fn collections_print_their_elements() {
        let subprogram = Subprogram {
            params: vec![],
            result: Some(Type {
                decl: "hr.pkg.t_map".into(),
                shape: Shape::Collection {
                    nullable: false,
                    text_keys: true,
                    element: Box::new(scalar("NUMBER")),
                },
            }),
        };
        let block = test_block("hr.pkg.f", &subprogram);
        assert!(
            block.contains("  l_i varchar2(32767);\n  l_n pls_integer;\n"),
            "{block}"
        );
        assert!(
            block.contains(
                "  dbms_output.put_line('result: ' || l_result.count || ' element(s)');\n"
            ),
            "{block}"
        );
        assert!(
            block.contains(
                "    dbms_output.put_line('result(' || '''' || l_i || '''' || ') = ' || l_result(l_i));\n"
            ),
            "{block}"
        );
        assert!(
            !block.contains("is null then"),
            "an index-by table isn't NULL: {block}"
        );
    }

    #[test]
    fn records_and_objects_print_their_fields() {
        let object = Shape::Fields {
            nullable: true,
            fields: vec![
                ("A".into(), scalar("NUMBER")),
                (
                    "B".into(),
                    Shape::Opaque("a reference isn't printed".into()),
                ),
            ],
        };
        let subprogram = Subprogram {
            params: vec![param("P_OBJ", ParamMode::Out, "hr.t_obj", object)],
            result: None,
        };
        let block = test_block("hr.p", &subprogram);
        assert!(
            block.contains(
                "  if l_obj is null then\n    dbms_output.put_line('p_obj = NULL');\n  else\n\
                 \x20   dbms_output.put_line('p_obj.a = ' || l_obj.a);\n\
                 \x20   dbms_output.put_line('p_obj.b: a reference isn''t printed');\n  end if;\n"
            ),
            "{block}"
        );
    }

    #[test]
    fn what_no_bind_carries_is_a_local_to_fill() {
        let list = Shape::Collection {
            nullable: true,
            text_keys: false,
            element: Box::new(scalar("VARCHAR2")),
        };
        let subprogram = Subprogram {
            params: vec![
                param("P_LIST", ParamMode::In, "hr.t_list", list),
                param("P_CUR", ParamMode::In, "sys_refcursor", Shape::Cursor),
                scalar_param("P_FLAG", ParamMode::In, "BOOLEAN"),
                scalar_param("P_DATA", ParamMode::In, "BLOB"),
                param("P_DOC", ParamMode::InOut, "xmltype", scalar("XMLTYPE")),
            ],
            result: None,
        };
        let block = test_block("hr.p", &subprogram);
        for expected in [
            "  l_list hr.t_list;\n",
            "  l_list := hr.t_list();\n  -- TODO: fill l_list\n",
            "  -- TODO: open l_cur for select …;\n",
            "    p_list => l_list,\n    p_cur => l_cur,\n",
            "    p_flag => case upper(:p_flag) when 'TRUE' then true when 'FALSE' then false end,\n",
            "    p_data => hextoraw(:p_data),\n",
            "  l_doc_text varchar2(32767) := :p_doc;\n",
            "  l_doc := case when l_doc_text is not null then xmltype(l_doc_text) end;\n",
            "dbms_output.put_line('p_doc = ' || case when l_doc is not null then \
             dbms_lob.substr(l_doc.getclobval(), 4000) end);",
        ] {
            assert!(block.contains(expected), "{expected}\nin\n{block}");
        }
    }

    #[test]
    fn locals_dont_clash() {
        // An OUT parameter named like the result, and like the helpers.
        let subprogram = Subprogram {
            params: vec![
                scalar_param("P_RESULT", ParamMode::Out, "NUMBER"),
                param("P_CURSOR", ParamMode::Out, "sys_refcursor", Shape::Cursor),
            ],
            result: Some(Type {
                decl: "sys_refcursor".into(),
                shape: Shape::Cursor,
            }),
        };
        let block = test_block("hr.f", &subprogram);
        assert!(
            block.contains("  l_result sys_refcursor;\n  l_result2 number;\n"),
            "{block}"
        );
        assert!(block.contains("  l_cursor sys_refcursor;\n"), "{block}");
        assert!(block.contains("  l_cursor2 integer;\n"), "{block}");
    }

    #[test]
    fn labels() {
        let label = Label::text("result")
            .and_text("(")
            .and_expr("l_i")
            .and_text(")");
        assert_eq!(label.with(" = "), "'result(' || l_i || ') = '");
        assert_eq!(Label::text("it's").with(""), "'it''s'");
    }

    #[test]
    fn base_types() {
        assert_eq!(
            base_type("TIMESTAMP(6) WITH TIME ZONE"),
            "TIMESTAMP WITH TIME ZONE"
        );
        assert_eq!(
            base_type("INTERVAL DAY(2) TO SECOND(6)"),
            "INTERVAL DAY TO SECOND"
        );
        assert_eq!(scalar("VARCHAR2(10)"), Shape::Scalar("VARCHAR2".into()));
        assert!(matches!(scalar("BFILE"), Shape::Opaque(_)));
    }
}

/// Test blocks built from the dictionary and run, for every kind of
/// parameter and result (#120). Needs PYTHIA_TEST_DB.
#[cfg(test)]
mod live {
    use super::*;

    const TYPES: [&str; 3] = [
        "create or replace type ts_obj as object (a number, b varchar2(10))",
        "create or replace type ts_list as table of varchar2(10)",
        "create or replace type ts_objs as varray(10) of ts_obj",
    ];

    const DROPS: [&str; 4] = [
        "drop package ts_pkg",
        "drop type ts_objs force",
        "drop type ts_list force",
        "drop type ts_obj force",
    ];

    const SPEC: &str = "create or replace package ts_pkg as
  type t_tab is table of number index by pls_integer;
  type t_map is table of varchar2(10) index by varchar2(10);
  type t_rec is record (a number, b varchar2(10), d date);
  type t_nest is record (r t_rec, l ts_list, f boolean);
  type t_recs is table of t_rec;
  type t_cur is ref cursor return t_rec;
  function f_number return number;
  function f_bool return boolean;
  function f_blob return blob;
  function f_xml return xmltype;
  function f_assoc return t_tab;
  function f_map return t_map;
  function f_list return ts_list;
  function f_null_list return ts_list;
  function f_objs return ts_objs;
  function f_null_obj return ts_obj;
  function f_rec return t_rec;
  function f_nest return t_nest;
  function f_recs return t_recs;
  function f_rowtype return demo_employees%rowtype;
  function f_cur return t_cur;
  function f_rows return sys_refcursor;
  function f_big return t_tab;
  procedure p_all(
    p_obj in out ts_obj, p_list out ts_list, p_cur out sys_refcursor, p_blob out blob,
    p_flag in boolean, p_n in pls_integer, p_data in blob, p_doc in out xmltype,
    p_rec in t_rec, p_items in ts_list, p_result out number);
  procedure p_over(p_a number);
  procedure p_over(p_b varchar2);
end;";

    const BODY: &str = "create or replace package body ts_pkg as
  function f_number return number is begin return 1; end;
  function f_bool return boolean is begin return true; end;
  function f_blob return blob is begin return to_blob(hextoraw('0AFF')); end;
  function f_xml return xmltype is begin return xmltype('<a>1</a>'); end;
  function f_assoc return t_tab is l t_tab; begin l(1) := 10; l(5) := 50; return l; end;
  function f_map return t_map is l t_map; begin l('a') := 'x'; return l; end;
  function f_list return ts_list is begin return ts_list('a', 'b'); end;
  function f_null_list return ts_list is begin return null; end;
  function f_objs return ts_objs is begin return ts_objs(ts_obj(1, 'x'), null); end;
  function f_null_obj return ts_obj is begin return null; end;
  function f_rec return t_rec is l t_rec; begin l.a := 1; l.b := 'bee'; return l; end;
  function f_nest return t_nest is l t_nest;
  begin l.r.a := 1; l.l := ts_list('a'); l.f := true; return l; end;
  function f_recs return t_recs is l t_recs := t_recs();
  begin l.extend; l(1).a := 7; return l; end;
  function f_rowtype return demo_employees%rowtype is l demo_employees%rowtype;
  begin select * into l from demo_employees where employee_id = 1; return l; end;
  function f_cur return t_cur is c t_cur;
  begin open c for select 1 a, 'x' b, date '2024-02-29' d from dual; return c; end;
  function f_rows return sys_refcursor is c sys_refcursor;
  begin open c for select level n, to_clob('c' || level) c, to_blob(hextoraw('00')) b
                     from dual connect by level <= 120; return c; end;
  function f_big return t_tab is l t_tab;
  begin for i in 1 .. 150 loop l(i) := i; end loop; return l; end;
  procedure p_all(
    p_obj in out ts_obj, p_list out ts_list, p_cur out sys_refcursor, p_blob out blob,
    p_flag in boolean, p_n in pls_integer, p_data in blob, p_doc in out xmltype,
    p_rec in t_rec, p_items in ts_list, p_result out number) is
  begin
    dbms_output.put_line('in: ' || p_n || ' ' || case when p_flag then 'T' else 'F' end
      || ' ' || rawtohex(p_data) || ' ' || p_doc.getstringval() || ' ' || p_items.count);
    p_obj := ts_obj(2, 'y');
    p_list := ts_list('a');
    open p_cur for select 'r' r from dual;
    p_blob := to_blob(hextoraw('01'));
    p_doc := xmltype('<b>2</b>');
    p_result := 42;
  end;
  procedure p_over(p_a number) is begin null; end;
  procedure p_over(p_b varchar2) is begin null; end;
end;";

    fn connect() -> Option<Connection> {
        let spec = std::env::var("PYTHIA_TEST_DB").ok()?;
        crate::test_db::demo_schema();
        let (cred, db) = spec.rsplit_once('@')?;
        let (user, pw) = cred.split_once('/')?;
        Some(Connection::connect(user, pw, db).expect("connect"))
    }

    /// The block of `ts_pkg.<name>` (that overload, else the first).
    fn script(conn: &Connection, name: &str, overload: Option<u32>) -> Script {
        let owner: String = conn
            .query_row_as(
                "select sys_context('userenv', 'current_schema') from dual",
                &[],
            )
            .unwrap();
        let task = task(
            owner,
            Some("TS_PKG".into()),
            name.to_uppercase(),
            overload,
            format!("ts_pkg.{name}"),
        );
        *task(conn).unwrap().downcast::<Script>().unwrap()
    }

    /// Builds the block of `ts_pkg.<name>` and runs it with `binds`: its
    /// DBMS_OUTPUT lines.
    fn run(conn: &Connection, name: &str, binds: &[(&str, &str)]) -> Vec<String> {
        let script = script(conn, name, None);
        let block: String = script
            .block
            .lines()
            .filter(|l| !l.starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        conn.execute("begin dbms_output.enable(null); end;", &[])
            .unwrap();
        let binds: Vec<(&str, &dyn pythia_db::oracle::sql_type::ToSql)> = binds
            .iter()
            .map(|(n, v)| (*n, v as &dyn pythia_db::oracle::sql_type::ToSql))
            .collect();
        if let Err(e) = conn.execute_named(&block, &binds) {
            panic!("{name}: {e}\n{}", script.block);
        }
        pythia_debugger::target::read_output(conn).unwrap()
    }

    #[test]
    fn generated_blocks_run_and_print() {
        let Some(conn) = connect() else {
            return;
        };
        // What a failed run left (types with dependents can't be replaced).
        for drop in DROPS {
            let _ = conn.execute(drop, &[]);
        }
        for ddl in TYPES.iter().chain(&[SPEC, BODY]) {
            conn.execute(ddl, &[]).unwrap();
        }
        let has = |name: &str, expected: &[&str]| {
            let lines = run(&conn, name, &[]);
            for e in expected {
                assert!(
                    lines.iter().any(|l| l == e),
                    "{name}: no {e:?} in {lines:#?}"
                );
            }
        };
        has("f_number", &["result = 1"]);
        has("f_bool", &["result = TRUE"]);
        has("f_blob", &["result = 2 bytes: 0AFF"]);
        has("f_xml", &["result = <a>1</a>"]);
        has(
            "f_assoc",
            &["result: 2 element(s)", "result(1) = 10", "result(5) = 50"],
        );
        has("f_map", &["result: 1 element(s)", "result('a') = x"]);
        has("f_list", &["result(1) = a", "result(2) = b"]);
        has("f_null_list", &["result = NULL"]);
        has(
            "f_objs",
            &["result(1).a = 1", "result(1).b = x", "result(2) = NULL"],
        );
        has("f_null_obj", &["result = NULL"]);
        has("f_rec", &["result.a = 1", "result.b = bee", "result.d = "]);
        has(
            "f_nest",
            &["result.r.a = 1", "result.l(1) = a", "result.f = TRUE"],
        );
        has("f_recs", &["result(1).a = 7"]);
        has(
            "f_rowtype",
            &[
                "result.employee_id = 1",
                "result.email = user1@example.com",
                "result.hire_date = 2015-01-04 00:00:00",
            ],
        );
        has("f_cur", &["result(1): a = 1, b = x, d = 29-FEB-24"]);
        has(
            "f_rows",
            &[
                "result(1): n = 1, c = c1, b = (not shown)",
                "result(100): n = 100, c = c100, b = (not shown)",
                "result: more rows not printed",
            ],
        );
        has("f_big", &["result: 50 more not printed"]);

        // IN parameters: binds where text carries them, locals otherwise.
        let lines = run(
            &conn,
            "p_all",
            &[
                ("p_flag", "TRUE"),
                ("p_n", "3"),
                ("p_data", "0AFF"),
                ("p_doc", "<a>1</a>"),
            ],
        );
        for e in [
            "in: 3 T 0AFF <a>1</a> 0",
            "p_obj.a = 2",
            "p_list(1) = a",
            "p_cur(1): r = r",
            "p_blob = 1 bytes: 01",
            "p_doc = <b>2</b>",
            "p_result = 42",
        ] {
            assert!(lines.iter().any(|l| l == e), "no {e:?} in {lines:#?}");
        }

        // Signatures name the types the same way: no PUBLIC owner, and a
        // strong cursor is a cursor.
        let declared = |function: &str| -> String {
            conn.query_row_as(
                &format!(
                    "select {} from all_arguments a where a.package_name = 'TS_PKG' \
                      and a.object_name = :1 and a.position = 0 and a.data_level = 0",
                    pythia_db::compat::ARGUMENT_TYPE
                ),
                &[&function],
            )
            .unwrap()
        };
        assert_eq!(declared("F_XML"), "XMLTYPE");
        assert_eq!(declared("F_CUR"), "REF CURSOR");
        assert_eq!(declared("F_NEST"), "TS_PKG.T_NEST");
        assert_eq!(declared("F_ROWTYPE"), "DEMO_EMPLOYEES%ROWTYPE");

        // That overload's parameters, the first one when it isn't known (#85).
        let over = |overload| script(&conn, "p_over", overload).block;
        assert!(over(Some(2)).contains("p_b => :p_b"), "{}", over(Some(2)));
        assert!(over(Some(1)).contains("p_a => :p_a"));
        assert!(over(None).contains("p_a => :p_a"), "the first");
        assert_eq!(script(&conn, "p_over", None).subprogram, "P_OVER");

        for drop in DROPS {
            conn.execute(drop, &[]).unwrap();
        }
    }
}
