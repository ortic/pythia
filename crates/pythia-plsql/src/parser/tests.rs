use std::fmt::Write as _;

use super::parse;
use crate::ast::*;

const HR_PKG: &str = include_str!("../../testdata/hr_pkg.sql");
const TRIGGERS: &str = include_str!("../../testdata/triggers.sql");
const TYPES: &str = include_str!("../../testdata/types.sql");
const SCRIPT: &str = include_str!("../../testdata/script.sql");

/// Compact, readable rendering of the tree for snapshots.
fn dump(src: &str) -> String {
    let ast = parse(src);
    let mut out = String::new();
    dump_scope(&ast, Ast::ROOT, 0, &mut out);
    for (i, q) in ast.queries.iter().enumerate() {
        let _ = write!(out, "Q{i} {:?} in S{}", q.kind, q.scope.0);
        if let Some(p) = q.parent {
            let _ = write!(out, " parent=Q{}", p.0);
        }
        if let Some(n) = q.next {
            let _ = write!(out, " next=Q{}", n.0);
        }
        out.push('\n');
        for c in &q.ctes {
            let cols: Vec<_> = c.columns.iter().map(|c| c.name.as_str()).collect();
            let _ = writeln!(
                out,
                "  cte {} ({}) = {:?}",
                c.name.name,
                cols.join(","),
                c.query.map(|q| q.0)
            );
        }
        for s in &q.sources {
            let what = match &s.kind {
                SourceKind::Table(p) => p
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>()
                    .join("."),
                SourceKind::Subquery(q) => format!("(Q{})", q.0),
                SourceKind::Function => "fn(..)".into(),
            };
            let alias = s
                .alias
                .as_ref()
                .map_or(String::new(), |a| format!(" {}", a.name));
            let _ = writeln!(out, "  from {what}{alias}");
        }
        let items: Vec<String> = q
            .items
            .iter()
            .map(|i| match (&i.name, &i.star) {
                (_, Some(q)) if q.is_empty() => "*".to_owned(),
                (_, Some(q)) => format!(
                    "{}.*",
                    q.iter()
                        .map(|i| i.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".")
                ),
                (Some(n), None) => n.name.clone(),
                (None, None) => "?".into(),
            })
            .collect();
        if !items.is_empty() {
            let _ = writeln!(out, "  items {}", items.join(", "));
        }
        let clauses: Vec<String> = q.clauses.iter().map(|(c, _)| format!("{c:?}")).collect();
        let _ = writeln!(out, "  clauses {}", clauses.join(" "));
    }
    for d in &ast.diagnostics {
        let _ = writeln!(out, "DIAG {:?} {} @{:?}", d.severity, d.message, d.span);
    }
    out
}

fn dump_scope(ast: &Ast, id: ScopeId, depth: usize, out: &mut String) {
    let s = ast.scope(id);
    let indent = "  ".repeat(depth);
    let name = s
        .name
        .as_ref()
        .map_or(String::new(), |n| format!(" {}", n.name));
    let owner = s
        .owner
        .as_ref()
        .map_or(String::new(), |o| format!(" owner={}", o.name));
    let trig = s.trigger_table.as_ref().map_or(String::new(), |t| {
        format!(
            " on={}",
            t.iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>()
                .join(".")
        )
    });
    let _ = writeln!(out, "{indent}S{} {:?}{name}{owner}{trig}", id.0, s.kind);
    for sym in &s.symbols {
        let ty = sym
            .ty
            .as_ref()
            .map_or(String::new(), |t| format!(": {} {:?}", t.text, t.attr));
        let params: Vec<String> = sym
            .params
            .iter()
            .map(|p| format!("{} {}", p.name.name, p.mode.label()))
            .collect();
        let params = if params.is_empty() {
            String::new()
        } else {
            format!("({})", params.join(", "))
        };
        let flags = format!(
            "{}{}{}",
            if sym.forward { " fwd" } else { "" },
            sym.body
                .map_or(String::new(), |b| format!(" body=S{}", b.0)),
            sym.query
                .map_or(String::new(), |q| format!(" query=Q{}", q.0)),
        );
        let def = match &sym.type_def {
            Some(TypeDef::Record(f)) | Some(TypeDef::Object(f)) => {
                format!(
                    " {{{}}}",
                    f.iter()
                        .map(|f| f.name.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Some(TypeDef::Collection(Some(t))) => format!(" of {}", t.text),
            Some(TypeDef::RefCursor(_)) => " ref cursor".into(),
            _ => String::new(),
        };
        let _ = writeln!(
            out,
            "{indent}  {:?} {}{params}{ty}{def}{flags}",
            sym.kind, sym.name.name
        );
    }
    for &c in &s.children {
        dump_scope(ast, c, depth + 1, out);
    }
}

#[test]
fn package_structure() {
    insta::assert_snapshot!(dump(HR_PKG));
}

#[test]
fn triggers() {
    insta::assert_snapshot!(dump(TRIGGERS));
}

#[test]
fn object_types() {
    insta::assert_snapshot!(dump(TYPES));
}

#[test]
fn script_queries() {
    insta::assert_snapshot!(dump(SCRIPT));
}

#[test]
fn valid_code_has_no_diagnostics() {
    for src in [HR_PKG, TRIGGERS, TYPES, SCRIPT] {
        let ast = parse(src);
        assert!(ast.diagnostics.is_empty(), "{:#?}", ast.diagnostics);
    }
}

#[test]
fn spans_are_consistent() {
    for src in [HR_PKG, TRIGGERS, TYPES, SCRIPT] {
        let ast = parse(src);
        for s in &ast.scopes {
            assert!(
                s.span.start <= s.span.end && s.span.end <= src.len(),
                "{s:?}"
            );
            if let Some(p) = s.parent {
                let ps = &ast.scope(p).span;
                assert!(
                    ps.start <= s.span.start && s.span.end <= ps.end,
                    "child {:?} outside parent {:?}",
                    s.span,
                    ps
                );
            }
        }
        for q in &ast.queries {
            assert!(q.span.start <= q.span.end && q.span.end <= src.len());
            for (_, c) in &q.clauses {
                assert!(c.start <= c.end, "{q:?}");
            }
        }
    }
}

#[test]
fn diagnostics_for_broken_code() {
    let ast = parse("begin\n  if x > 1 then\n    null;\nend;");
    let messages: Vec<_> = ast.diagnostics.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(messages, ["IF without END IF"]);

    let ast = parse("begin\n  loop\n    null;\nend;");
    assert_eq!(ast.diagnostics[0].message, "LOOP without END LOOP");

    let ast = parse("begin\n  null;\n");
    assert_eq!(ast.diagnostics[0].message, "Missing END");

    let ast = parse("select 'abc from dual");
    assert_eq!(ast.diagnostics[0].message, "Unterminated string literal");

    let ast = parse("select 1 /* never closed");
    assert_eq!(ast.diagnostics[0].message, "Unterminated comment");

    let ast = parse("begin\n  null;\n  end if;\nend;");
    assert_eq!(ast.diagnostics[0].message, "END IF without matching IF");
}

#[test]
fn strings_are_not_misreported() {
    for ok in [
        "select '' from dual",
        "select 'it''s' from dual",
        "select q'[a]' from dual",
        "select n'x' from dual",
        "select q'{a}}'||'b' from dual",
    ] {
        let ast = parse(ok);
        assert!(ast.diagnostics.is_empty(), "{ok}: {:?}", ast.diagnostics);
    }
}

#[test]
fn incomplete_select_while_typing() {
    let src = "select e. from employees e join departments d on d.";
    let ast = parse(src);
    let q = &ast.queries[0];
    let names: Vec<_> = q
        .sources
        .iter()
        .filter_map(|s| s.exposed_name())
        .map(|i| i.name.as_str())
        .collect();
    assert_eq!(names, ["E", "D"]);
    assert_eq!(q.clause_at(9), Some(Clause::Select));
    assert_eq!(q.clause_at(src.len()), Some(Clause::From));
}

#[test]
fn cursor_after_statement_end_belongs_to_it() {
    let src = "select * from employees e where   \n\nselect 1 from dual";
    let ast = parse(src);
    let first = &ast.statements[0];
    assert!(first.extent_end >= 33, "{first:?}");
    let q = ast.query_at(33).expect("query");
    assert_eq!(ast.query(q).clause_at(33), Some(Clause::Where));
}

#[test]
fn quoted_identifiers() {
    let ast = parse("select \"Mixed\".x from \"Weird Tab\" \"Mixed\"");
    let s = &ast.queries[0].sources[0];
    assert!(matches!(&s.kind, SourceKind::Table(p) if p[0].name == "Weird Tab" && p[0].quoted));
    assert_eq!(s.alias.as_ref().unwrap().name, "Mixed");
}

/// The parser must never panic and must keep spans inside the source, also on
/// arbitrary prefixes and mangled versions of real code.
#[test]
fn never_panics_on_partial_input() {
    for src in [HR_PKG, TRIGGERS, TYPES, SCRIPT] {
        let mut cut = 0;
        while cut <= src.len() {
            if src.is_char_boundary(cut) {
                let prefix = &src[..cut];
                let ast = parse(prefix);
                for s in &ast.scopes {
                    assert!(s.span.end <= prefix.len());
                }
                for q in &ast.queries {
                    assert!(q.span.end <= prefix.len());
                }
            }
            cut += 1;
        }
    }
}

#[test]
fn never_panics_on_mangled_input() {
    // Deterministic pseudo-random mangling: drop / duplicate / swap tokens.
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for src in [HR_PKG, TRIGGERS, TYPES, SCRIPT] {
        let words: Vec<&str> = src.split_inclusive(char::is_whitespace).collect();
        for _ in 0..200 {
            let mut out = String::new();
            for w in &words {
                match next() % 23 {
                    0 => {}
                    1 => {
                        out.push_str(w);
                        out.push_str(w);
                    }
                    2 => out.push_str(") "),
                    3 => out.push_str("end; "),
                    4 => out.push_str("( "),
                    5 => out.push_str("begin "),
                    _ => out.push_str(w),
                }
            }
            let _ = parse(&out);
        }
    }
}

/// Messages of a parse's diagnostics, with the text each one marks.
fn messages(src: &str) -> Vec<(String, &str)> {
    parse(src)
        .diagnostics
        .into_iter()
        .map(|d| (d.message, &src[d.span]))
        .collect()
}

#[test]
fn conditional_compilation() {
    // IFS code (#171): `$ELSE` read as a stray ELSE, `$END` and the next
    // line's IF as an END IF, and the real END IF without its IF.
    let ifs = "create or replace package body p is
  procedure refresh(n_ number) is
    ret_ varchar2(100);
  begin
    $IF Component_Order_SYS.INSTALLED $THEN
    for r in (select 1 x from dual) loop
      null;
    end loop;
    IF n_ > 0 THEN
      commit;
      ret_ := n_;
    END IF;
$ELSE
NULL;
$END
    IF ret_ is null then
      ret_ := 'no data';
    END IF;
  end refresh;
end p;";
    assert_eq!(messages(ifs), []);
    // Alternatives: each branch opens the IF that follows.
    let alternatives = "create or replace procedure p is
begin
  $if dbms_db_version.ver_le_11 $then
  if a = 1 then
  $elsif $$new_code $then
  if a = 2 then
  $else
  if a = 3 then
  $end
    null;
  end if;
end;";
    assert_eq!(messages(alternatives), []);
    // `$ERROR … $END`, and inquiry directives as values.
    let error = "create or replace procedure p is
begin
  $IF $$debug_level IS NULL $THEN
    $ERROR 'no debug level for ' || $$PLSQL_UNIT $END
  $END
  if $$debug_level > 1 then
    dbms_output.put_line($$PLSQL_UNIT || ':' || $$PLSQL_LINE);
  end if;
end;";
    assert_eq!(messages(error), []);
    // The first branch is the code: its declarations, not the others'.
    let ast = parse(
        "create or replace package body p is
  $IF x.y $THEN
  procedure a is begin null; end;
  $ELSE
  procedure b is begin null; end;
  $END
  procedure c is begin null; end;
end;",
    );
    assert_eq!(ast.diagnostics, []);
    let names: Vec<&str> = ast
        .scopes
        .iter()
        .filter_map(|s| s.name.as_ref().map(|n| n.name.as_str()))
        .collect();
    assert_eq!(names, ["P", "A", "C"]);

    // Directives that don't pair up.
    assert_eq!(
        messages("begin\n  $IF x $THEN\n  null;\nend;"),
        [("$IF without $END".to_owned(), "$IF")]
    );
    assert_eq!(
        messages("begin\n  null;\n  $END\nend;"),
        [("$END without matching $IF".to_owned(), "$END")]
    );
    assert_eq!(
        messages("begin\n  $else\n  null;\nend;"),
        [("$ELSE outside of $IF".to_owned(), "$else")]
    );
    assert_eq!(
        messages("begin\n  $IF x null; $END\n  null;\nend;"),
        [("$IF without $THEN".to_owned(), "$IF")]
    );
}
