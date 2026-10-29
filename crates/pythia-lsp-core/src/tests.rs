use super::*;

const HR_PKG: &str = include_str!("../../pythia-plsql/testdata/hr_pkg.sql");
const TRIGGERS: &str = include_str!("../../pythia-plsql/testdata/triggers.sql");
const TYPES: &str = include_str!("../../pythia-plsql/testdata/types.sql");
const SCRIPT: &str = include_str!("../../pythia-plsql/testdata/script.sql");

/// Cursor marker in test sources.
const CURSOR: char = '¦';

fn split(src: &str) -> (String, usize) {
    let offset = src.find(CURSOR).expect("cursor marker");
    (src.replacen(CURSOR, "", 1), offset)
}

fn obj(owner: &str, name: &str, t: &str) -> CatalogObject {
    CatalogObject {
        owner: owner.into(),
        name: name.into(),
        object_type: t.into(),
    }
}

fn col(name: &str, t: &str, nullable: bool) -> CatalogColumn {
    CatalogColumn {
        name: name.into(),
        data_type: t.into(),
        nullable,
        comment: None,
    }
}

fn param(name: &str, mode: ParamMode, t: &str, has_default: bool) -> CatalogParam {
    CatalogParam {
        name: name.into(),
        mode,
        data_type: t.into(),
        has_default,
    }
}

/// An HR-like schema with everything cached.
fn hr() -> CatalogCache {
    let mut c = CatalogCache::new("HR");
    c.set_schemas(vec!["HR".into(), "SCOTT".into(), "SYS".into()]);
    c.set_objects(
        None,
        vec![
            obj("HR", "EMPLOYEES", "TABLE"),
            obj("HR", "DEPARTMENTS", "TABLE"),
            obj("HR", "LOCATIONS", "TABLE"),
            obj("HR", "SALARY_LOG", "TABLE"),
            obj("HR", "EMP_V", "VIEW"),
            obj("HR", "EMP_PKG", "PACKAGE"),
            obj("HR", "EMP_SEQ", "SEQUENCE"),
            obj("HR", "GET_BONUS", "FUNCTION"),
            obj("HR", "ADDRESS_T", "TYPE"),
            obj("HR", "Weird Tab", "TABLE"),
        ],
    );
    c.set_objects(
        Some("SCOTT"),
        vec![obj("SCOTT", "EMP", "TABLE"), obj("SCOTT", "DEPT", "TABLE")],
    );
    c.set_resolved(
        None,
        "DBMS_OUTPUT",
        Some(obj("SYS", "DBMS_OUTPUT", "PACKAGE")),
    );
    c.set_resolved(None, "DUAL", Some(obj("SYS", "DUAL", "TABLE")));
    c.set_resolved(None, "SCOTT", None);
    c.set_resolved(None, "SYS", None);
    let mut last_name = col("LAST_NAME", "VARCHAR2(25)", false);
    last_name.comment = Some("Family name".into());
    c.set_columns(
        "HR",
        "EMPLOYEES",
        vec![
            col("EMPLOYEE_ID", "NUMBER(6)", false),
            col("FIRST_NAME", "VARCHAR2(20)", true),
            last_name,
            col("SALARY", "NUMBER(8,2)", true),
            col("DEPARTMENT_ID", "NUMBER(4)", true),
            col("MANAGER_ID", "NUMBER(6)", true),
        ],
    );
    c.set_columns(
        "HR",
        "DEPARTMENTS",
        vec![
            col("DEPARTMENT_ID", "NUMBER(4)", false),
            col("DEPARTMENT_NAME", "VARCHAR2(30)", false),
            col("MANAGER_ID", "NUMBER(6)", true),
            col("LOCATION_ID", "NUMBER(4)", true),
        ],
    );
    c.set_columns(
        "HR",
        "LOCATIONS",
        vec![
            col("LOCATION_ID", "NUMBER(4)", false),
            col("CITY", "VARCHAR2(30)", false),
        ],
    );
    c.set_columns(
        "HR",
        "SALARY_LOG",
        vec![
            col("EMPLOYEE_ID", "NUMBER", true),
            col("OLD_SALARY", "NUMBER", true),
        ],
    );
    c.set_columns(
        "HR",
        "Weird Tab",
        vec![col("My Col", "NUMBER", true), col("ID", "NUMBER", true)],
    );
    c.set_columns(
        "SCOTT",
        "EMP",
        vec![
            col("EMPNO", "NUMBER(4)", false),
            col("ENAME", "VARCHAR2(10)", true),
        ],
    );
    c.set_columns(
        "HR",
        "ADDRESS_T",
        vec![
            col("STREET", "VARCHAR2(100)", true),
            col("CITY", "VARCHAR2(50)", true),
        ],
    );
    c.set_members(
        "HR",
        "EMP_PKG",
        vec![
            CatalogMember {
                name: "RAISE_SALARY".into(),
                kind: MemberKind::Procedure,
                params: vec![
                    param("P_EMP_ID", ParamMode::In, "NUMBER", false),
                    param("P_PCT", ParamMode::In, "NUMBER", true),
                ],
                return_type: None,
                overload: Some(1),
            },
            CatalogMember {
                name: "RAISE_SALARY".into(),
                kind: MemberKind::Procedure,
                params: vec![
                    param("P_EMP_ID", ParamMode::In, "NUMBER", false),
                    param("P_AMOUNT", ParamMode::In, "NUMBER", false),
                    param("P_REASON", ParamMode::In, "VARCHAR2", false),
                ],
                return_type: None,
                overload: Some(2),
            },
            CatalogMember {
                name: "GET_NAME".into(),
                kind: MemberKind::Function,
                params: vec![param("P_EMP_ID", ParamMode::In, "NUMBER", false)],
                return_type: Some("VARCHAR2".into()),
                overload: None,
            },
        ],
    );
    c.set_members(
        "SYS",
        "DBMS_OUTPUT",
        vec![
            CatalogMember {
                name: "PUT_LINE".into(),
                kind: MemberKind::Procedure,
                params: vec![param("A", ParamMode::In, "VARCHAR2", false)],
                return_type: None,
                overload: None,
            },
            CatalogMember {
                name: "ENABLE".into(),
                kind: MemberKind::Procedure,
                params: vec![param("BUFFER_SIZE", ParamMode::In, "NUMBER", true)],
                return_type: None,
                overload: None,
            },
        ],
    );
    c.set_members(
        "HR",
        "GET_BONUS",
        vec![CatalogMember {
            name: "GET_BONUS".into(),
            kind: MemberKind::Function,
            params: vec![param("P_EMP_ID", ParamMode::In, "NUMBER", false)],
            return_type: Some("NUMBER".into()),
            overload: None,
        }],
    );
    c
}

fn completion(src: &str, cat: &dyn Catalog) -> Completion {
    let (text, offset) = split(src);
    Analysis::new(&text).complete(offset, cat)
}

fn labels(src: &str) -> Vec<String> {
    completion(src, &hr())
        .items
        .into_iter()
        .map(|i| i.label)
        .collect()
}

fn has(labels: &[String], expected: &[&str]) {
    for e in expected {
        assert!(labels.iter().any(|l| l == e), "missing {e:?} in {labels:?}");
    }
}

fn lacks(labels: &[String], unexpected: &[&str]) {
    for e in unexpected {
        assert!(
            !labels.iter().any(|l| l == e),
            "unexpected {e:?} in {labels:?}"
        );
    }
}

// ----- completion: SQL ------------------------------------------------------

#[test]
fn alias_columns_before_from() {
    let l = labels("select e.¦ from employees e");
    assert_eq!(
        l,
        [
            "department_id",
            "employee_id",
            "first_name",
            "last_name",
            "manager_id",
            "salary"
        ]
    );
}

#[test]
fn join_alias_columns() {
    let l = labels("select * from employees e join departments d on d.¦");
    has(&l, &["department_name", "location_id"]);
    lacks(&l, &["first_name"]);
}

#[test]
fn prefix_replaces_typed_word() {
    let (text, offset) = split("select * from employees e where e.sal¦");
    let c = Analysis::new(&text).complete(offset, &hr());
    assert_eq!(c.items[0].label, "salary");
    assert_eq!(&text[c.replace.clone()], "sal");
}

#[test]
fn select_list_offers_all_sources() {
    let l = labels("select ¦ from employees e, departments d");
    has(
        &l,
        &["first_name", "department_name", "e", "d", "nvl", "case"],
    );
    // Columns rank above keywords and builtins.
    let first_kw = l.iter().position(|x| x == "case").unwrap();
    let first_col = l.iter().position(|x| x == "first_name").unwrap();
    assert!(first_col < first_kw);
}

#[test]
fn table_position() {
    let l = labels("select * from emp¦");
    has(&l, &["employees", "emp_v"]);
    lacks(&l, &["emp_pkg", "emp_seq"]);
    let l = labels("select * from employees e join ¦");
    has(&l, &["departments", "locations", "scott"]);
    let l = labels("select * from employees e, dep¦");
    assert_eq!(l[0], "departments");
}

#[test]
fn alias_position_offers_keywords() {
    let l = labels("select * from employees e wh¦");
    assert_eq!(l[0], "where");
    let l = labels("select * from employees e where e.salary > 10 ¦");
    has(&l, &["and", "or", "order by"]);
}

#[test]
fn cte_columns() {
    let l = labels(
        "with t as (select department_id, count(*) cnt from employees group by department_id) select t.¦ from t",
    );
    assert_eq!(l, ["cnt", "department_id"]);
    let l = labels("with t (a, b) as (select 1, 2 from dual) select * from ¦");
    has(&l, &["t", "employees"]);
}

#[test]
fn inline_view_columns() {
    let l = labels("select v.¦ from (select employee_id, salary * 12 annual from employees) v");
    assert_eq!(l, ["annual", "employee_id"]);
    let l = labels("select v.¦ from (select e.*, 1 extra from employees e) v");
    has(&l, &["first_name", "extra"]);
}

#[test]
fn correlated_subquery_sees_outer_alias() {
    let l = labels(
        "select * from departments d where exists (select 1 from employees e where e.department_id = d.¦)",
    );
    has(&l, &["department_name"]);
    let l =
        labels("select * from departments d where exists (select 1 from employees e where e.¦)");
    has(&l, &["salary"]);
}

#[test]
fn union_blocks_have_own_sources() {
    let l = labels("select e.employee_id from employees e union all select d.¦ from departments d");
    has(&l, &["department_name"]);
}

#[test]
fn schema_and_package_members() {
    let l = labels("select * from scott.¦");
    assert_eq!(l, ["dept", "emp"]);
    let l = labels("select s.¦ from scott.emp s");
    assert_eq!(l, ["empno", "ename"]);
    let c = completion("begin emp_pkg.¦ end;", &hr());
    let names: Vec<_> = c.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(names, ["get_name", "raise_salary"]);
    assert!(
        c.items[1].detail.contains("+1 overloads"),
        "{:?}",
        c.items[1]
    );
    let l = labels("begin dbms_output.¦");
    has(&l, &["put_line", "enable"]);
    let l = labels("select emp_seq.¦ from dual");
    assert_eq!(l, ["currval", "nextval"]);
}

#[test]
fn insert_update_delete_merge() {
    let l = labels("insert into employees (¦");
    has(&l, &["employee_id", "salary"]);
    lacks(&l, &["nvl"]);
    let l = labels("insert into ¦");
    has(&l, &["employees"]);
    let l = labels("update employees e set e.¦");
    has(&l, &["salary"]);
    let l = labels("update employees set sal¦");
    assert_eq!(l[0], "salary");
    let l = labels("delete from employees where ¦");
    has(&l, &["manager_id"]);
    let l = labels("merge into employees t using departments s on (t.department_id = s.¦");
    has(&l, &["department_name"]);
}

#[test]
fn case_follows_typing() {
    let l = labels("SELECT * FROM EMP¦");
    has(&l, &["EMPLOYEES"]);
    let l = labels("SELECT * FROM EMPLOYEES E WHERE E.SAL¦");
    assert_eq!(l[0], "SALARY");
    let options = CompletionOptions {
        case: CaseStyle::Upper,
        ..CompletionOptions::default()
    };
    let (text, offset) = split("select e.¦ from employees e");
    let c = Analysis::new(&text).complete_with(offset, &hr(), &options);
    assert_eq!(c.items[0].label, "DEPARTMENT_ID");
}

#[test]
fn quoted_identifiers() {
    let c = completion("select w.¦ from \"Weird Tab\" w", &hr());
    let item = c
        .items
        .iter()
        .find(|i| i.label.contains("My Col"))
        .expect("quoted column");
    assert_eq!(item.insert_text, "\"My Col\"");
    let l = labels("select * from \"Wei¦");
    has(&l, &["\"Weird Tab\""]);
}

#[test]
fn no_completion_in_comments_and_strings() {
    assert!(labels("select '¦' from dual").is_empty());
    assert!(labels("select 1 -- emp¦").is_empty());
    assert!(labels("select /* e¦ */ 1 from dual").is_empty());
}

#[test]
fn statement_start() {
    let c = completion("sel¦", &hr());
    let l: Vec<_> = c.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(l[0], "select");
    let snippet = c
        .items
        .iter()
        .find(|i| i.kind == CompletionKind::Snippet && i.label == "sel")
        .unwrap();
    assert_eq!(snippet.insert_text, "select * from ");
    assert_eq!(snippet.caret, Some(14));
    let l = labels("select 1 from dual;\nupd¦");
    assert_eq!(l[0], "update");
}

#[test]
fn clauses_without_alias() {
    has(
        &labels("select * from employees where ¦"),
        &["salary", "employees"],
    );
    has(
        &labels("select first_name from employees order by ¦"),
        &["last_name"],
    );
    has(
        &labels("select department_id, count(*) from employees group by ¦"),
        &["department_id"],
    );
    has(
        &labels("select * from employees where employees.¦"),
        &["salary"],
    );
    has(
        &labels("select * from hr.employees where employees.¦"),
        &["salary"],
    );
}

// ----- completion: PL/SQL -----------------------------------------------------

#[test]
fn typing_inside_real_package() {
    let at = |code: &str| {
        labels(&HR_PKG.replacen(
            "check_limit(l_new);",
            &format!("check_limit(l_new);\n    {code}"),
            1,
        ))
    };
    has(&at("l_emp.¦"), &["salary", "first_name"]);
    has(
        &at("update employees set salary = l_¦"),
        &["l_new", "l_emp"],
    );
    has(
        &at("select * into l_emp from employees x where x.¦"),
        &["manager_id"],
    );
    let l = at("¦");
    has(
        &l,
        &["l_emp", "check_limit", "p_pct", "g_count", "if", "fmt"],
    );
    let l = labels(&HR_PKG.replacen("g_cache(r.employee_id)", "g_cache(r.¦employee_id)", 1));
    assert_eq!(l, ["department_name", "employee_id", "sal"]);
}

const RECORDS: &str = "declare
  l_emp employees%rowtype;
  cursor c is select employee_id, first_name nm from employees;
  type t_rec is record (a number, b varchar2(10));
  l_rec t_rec;
  l_c c%rowtype;
  type t_tab is table of t_rec index by pls_integer;
  l_tab t_tab;
  l_count number := 0;
  l_sal employees.salary%type;
begin
  ¦
end;";

fn in_records(code: &str) -> Vec<String> {
    labels(&RECORDS.replace('¦', code))
}

#[test]
fn record_fields() {
    has(&in_records("l_emp.¦"), &["salary", "employee_id"]);
    assert_eq!(in_records("l_rec.¦"), ["a", "b"]);
    assert_eq!(in_records("l_c.¦"), ["employee_id", "nm"]);
    assert_eq!(in_records("l_tab(1).¦"), ["a", "b"]);
    assert_eq!(
        in_records("for r in c loop r.¦ end loop;"),
        ["employee_id", "nm"]
    );
    assert_eq!(
        in_records("for r in (select department_name from departments) loop r.¦ end loop;"),
        ["department_name"]
    );
    assert!(in_records("l_sal.¦").is_empty());
}

#[test]
fn locals_first_in_plsql() {
    let l = in_records("l_c¦");
    assert_eq!(&l[..2], ["l_c", "l_count"]);
    let l = in_records("x := l_¦");
    has(&l, &["l_emp", "l_rec", "l_count"]);
    let l = in_records("select count(*) into ¦");
    has(&l, &["l_count", "l_sal"]);
    lacks(&l, &["t_rec", "nvl"]);
}

#[test]
fn package_level_declarations_in_body() {
    // Inside the package body, spec declarations are visible.
    let src = HR_PKG.replacen("check_limit(l_new);", "check_limit(l_new); c_m¦", 1);
    let l = labels(&src);
    assert_eq!(l[0], "c_max_salary");
    let src = HR_PKG.replacen("check_limit(l_new);", "check_limit(l_new); emp_pkg.¦", 1);
    let l = labels(&src);
    has(
        &l,
        &[
            "raise_salary",
            "get_name",
            "count_by_dept",
            "g_count",
            "t_emp_rec",
            "e_not_found",
        ],
    );
}

#[test]
fn declaration_contexts() {
    let l = labels("declare\n  l_x num¦");
    assert_eq!(l[0], "number");
    let l = labels("declare\n  l_x emp¦");
    has(&l, &["employees"]);
    let l = labels("declare\n  l_x employees%¦");
    assert_eq!(l, ["rowtype", "type"]);
    let l = labels("declare\n  l_x number;\n  ¦");
    has(&l, &["procedure", "cursor", "begin"]);
    let l = labels("create or replace procedure p(a in ¦");
    has(&l, &["varchar2", "number"]);
}

#[test]
fn trigger_new_old() {
    let src = TRIGGERS.replacen(":new.updated_at", ":new.¦updated_at", 1);
    let l = labels(&src);
    has(&l, &["salary", "employee_id"]);
    let src = TRIGGERS.replacen(":new.updated_at", ":¦", 1);
    assert_eq!(labels(&src), ["new", "old"]);
}

// ----- the missing-data protocol ----------------------------------------------

#[test]
fn missing_columns_are_requested() {
    let mut cat = CatalogCache::new("HR");
    cat.set_objects(None, vec![obj("HR", "EMPLOYEES", "TABLE")]);
    let c = completion("select e.¦ from employees e", &cat);
    assert!(c.items.is_empty());
    assert_eq!(
        c.missing,
        [CatalogRequest::Columns {
            owner: "HR".into(),
            name: "EMPLOYEES".into()
        }]
    );

    cat.set_columns("HR", "EMPLOYEES", vec![col("EMPLOYEE_ID", "NUMBER", false)]);
    let c = completion("select e.¦ from employees e", &cat);
    assert_eq!(c.items[0].label, "employee_id");
    assert!(c.missing.is_empty());
}

#[test]
fn unknown_names_are_resolved_first() {
    let cat = CatalogCache::new("HR");
    let c = completion("select e.¦ from employees e", &cat);
    assert_eq!(
        c.missing,
        [CatalogRequest::Resolve {
            schema: None,
            name: "EMPLOYEES".into()
        }]
    );
    let c = completion("select * from ¦", &cat);
    assert!(
        c.missing
            .contains(&CatalogRequest::Objects { schema: None })
    );
    assert!(c.missing.contains(&CatalogRequest::Schemas));
}

#[test]
fn empty_catalog_still_completes_locals() {
    let (text, offset) = split(&RECORDS.replace('¦', "l_rec.¦"));
    let c = Analysis::new(&text).complete(offset, &EmptyCatalog);
    assert_eq!(c.items.len(), 2);
    assert!(c.missing.is_empty());
}

// ----- signature help ---------------------------------------------------------

fn sig(src: &str) -> SignatureHelp {
    let (text, offset) = split(src);
    Analysis::new(&text)
        .signature_help(offset, &hr())
        .expect("signature help")
}

#[test]
fn signature_overloads_and_active_param() {
    let h = sig("begin emp_pkg.raise_salary(1, ¦");
    assert_eq!(h.signatures.len(), 2);
    assert_eq!((h.active_signature, h.active_param), (0, 1));
    let s = &h.signatures[0];
    assert_eq!(&s.label[s.params[1].clone()], "p_pct IN NUMBER := …");

    let h = sig("begin emp_pkg.raise_salary(1, 2, ¦");
    assert_eq!((h.active_signature, h.active_param), (1, 2));

    let h = sig("begin emp_pkg.raise_salary(p_reason => ¦");
    assert_eq!((h.active_signature, h.active_param), (1, 2));
}

#[test]
fn signature_builtin_nested() {
    let h = sig("select nvl(substr(first_name, 1, 2), ¦ from employees");
    assert!(h.signatures[0].label.starts_with("NVL(expr1, expr2)"));
    assert_eq!(h.active_param, 1);
    let h = sig("select substr(first_name, ¦");
    assert_eq!(h.active_param, 1);
}

#[test]
fn signature_local_and_standalone() {
    let src = HR_PKG.replacen("check_limit(l_new);", "check_limit(¦l_new);", 1);
    let h = sig(&src);
    assert_eq!(h.signatures[0].label, "check_limit(p_amount IN number)");
    let h = sig("begin get_bonus(¦");
    assert_eq!(
        h.signatures[0].label,
        "get_bonus(p_emp_id IN NUMBER) return NUMBER"
    );
    let h = sig("begin dbms_output.put_line(¦");
    assert_eq!(h.signatures[0].label, "put_line(a IN VARCHAR2)");
}

#[test]
fn signature_reports_missing_members() {
    let mut cat = CatalogCache::new("HR");
    cat.set_objects(None, vec![obj("HR", "EMP_PKG", "PACKAGE")]);
    let (text, offset) = split("begin emp_pkg.raise_salary(¦");
    let h = Analysis::new(&text).signature_help(offset, &cat).unwrap();
    assert!(h.signatures.is_empty());
    assert_eq!(
        h.missing,
        [CatalogRequest::Members {
            owner: "HR".into(),
            name: "EMP_PKG".into()
        }]
    );
}

// ----- hover --------------------------------------------------------------------

fn hover_at(src: &str) -> Hover {
    let (text, offset) = split(src);
    Analysis::new(&text).hover(offset, &hr()).expect("hover")
}

#[test]
fn hover_info() {
    let h = hover_at("select e.sal¦ary from employees e");
    assert_eq!(h.title, "column HR.EMPLOYEES.SALARY");
    assert_eq!(h.detail, "NUMBER(8,2)");
    let h = hover_at("select e.last_name¦ from employees e");
    assert_eq!(h.detail, "VARCHAR2(25) NOT NULL\nFamily name");
    let h = hover_at("select e¦.salary from employees e");
    assert_eq!(h.title, "alias e");
    assert!(h.detail.starts_with("EMPLOYEES"), "{}", h.detail);
    let h = hover_at("begin emp_pkg.get_na¦me(1); end;");
    assert_eq!(h.title, "function HR.EMP_PKG.GET_NAME");
    assert_eq!(h.detail, "get_name(p_emp_id IN NUMBER) return VARCHAR2");
    let h = hover_at("select nv¦l(a, b) from dual");
    assert!(h.detail.starts_with("NVL(expr1, expr2)"));
    let h = hover_at("select * from employ¦ees");
    assert_eq!(h.title, "table HR.EMPLOYEES");

    let src = HR_PKG.replacen("check_limit(l_new);", "check_limit(l_ne¦w);", 1);
    let h = hover_at(&src);
    assert_eq!(h.title, "variable l_new");
    assert_eq!(h.detail, "l_new employees.salary%type");
    let src = HR_PKG.replacen("g_cache(r.employee_id)", "g_cache(r¦.employee_id)", 1);
    let h = hover_at(&src);
    assert_eq!(h.title, "loop index r");
    assert_eq!(h.detail, "record (employee_id, sal, department_name)");
}

#[test]
fn completion_items_carry_what_hover_shows() {
    let doc = |src: &str, label: &str| {
        completion(src, &hr())
            .items
            .into_iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("no {label}"))
            .documentation
    };
    let pair = |title: &str, text: &str| Some((title.to_owned(), text.to_owned()));
    assert_eq!(
        doc("select e.¦ from employees e", "last_name"),
        pair(
            "column HR.EMPLOYEES.LAST_NAME",
            "VARCHAR2(25) NOT NULL\nFamily name"
        )
    );
    let (title, text) = doc("begin emp_pkg.¦ end;", "raise_salary").unwrap();
    assert_eq!(title, "procedure HR.EMP_PKG.RAISE_SALARY");
    assert_eq!(text.lines().count(), 2, "both overloads: {text}");
    let src = "declare\n  -- Running total.\n  l_total number;\nbegin\n  l_¦\nend;";
    assert_eq!(
        doc(src, "l_total"),
        pair("variable l_total", "Running total.\nl_total number")
    );
    let (title, text) = doc("select nv¦ from dual", "nvl").unwrap();
    assert_eq!(title, "function NVL");
    assert!(text.starts_with("NVL(expr1, expr2)"), "{text}");
    assert_eq!(doc("select * from employ¦", "employees"), None);
}

#[test]
fn hover_reports_missing() {
    let cat = CatalogCache::new("HR");
    let (text, offset) = split("select * from employ¦ees");
    let h = Analysis::new(&text).hover(offset, &cat).unwrap();
    assert!(h.title.is_empty());
    assert_eq!(
        h.missing,
        [CatalogRequest::Resolve {
            schema: None,
            name: "EMPLOYEES".into()
        }]
    );
}

// ----- definition ----------------------------------------------------------------

fn def(src: &str) -> Option<Definition> {
    let (text, offset) = split(src);
    Analysis::new(&text).definition(offset, &hr()).target
}

/// Span of the n-th occurrence of `needle` in `HR_PKG`.
fn nth(src: &str, needle: &str, n: usize) -> std::ops::Range<usize> {
    let start = src.match_indices(needle).nth(n).expect("occurrence").0;
    start..start + needle.len()
}

fn local(src: &str, cursor_needle: &str, cursor_n: usize) -> Option<std::ops::Range<usize>> {
    let at = nth(src, cursor_needle, cursor_n).start + 1;
    match Analysis::new(src).definition(at, &hr()).target {
        Some(Definition::Local(r)) => Some(r),
        other => panic!("expected local definition, got {other:?}"),
    }
}

#[test]
fn definition_within_package() {
    // Call of a nested procedure → its declaration.
    assert_eq!(
        local(HR_PKG, "check_limit(l_new)", 0),
        Some(nth(HR_PKG, "check_limit", 0))
    );
    // Private procedure in the body.
    assert_eq!(
        local(HR_PKG, "log_change(p_emp_id, l_emp", 0),
        Some(nth(HR_PKG, "log_change", 0))
    );
    // Body implementation → spec declaration (matching overload).
    // Occurrences: 0, 1 spec; 2 body impl; 3 `end raise_salary`; 4 second impl.
    assert_eq!(
        local(HR_PKG, "raise_salary", 4),
        Some(nth(HR_PKG, "raise_salary", 1))
    );
    assert_eq!(
        local(HR_PKG, "raise_salary", 2),
        Some(nth(HR_PKG, "raise_salary", 0))
    );
    // Spec declaration → body implementation.
    assert_eq!(
        local(HR_PKG, "get_name", 0),
        Some(nth(HR_PKG, "get_name", 1))
    );
    // Variable use → declaration.
    assert_eq!(local(HR_PKG, "l_new :=", 0), Some(nth(HR_PKG, "l_new", 0)));
    // Package-level constant from the spec, used in the body.
    assert_eq!(
        local(HR_PKG, "c_max_salary then", 0),
        Some(nth(HR_PKG, "c_max_salary", 0))
    );
    // Cursor loop record field → select item.
    let item = nth(HR_PKG, "sal, d.", 0).start;
    assert_eq!(local(HR_PKG, "sal;", 0), Some(item..item + 3));
}

#[test]
fn definition_in_sql() {
    let src = "select e.salary from employees e";
    assert_eq!(
        def("select e¦.salary from employees e"),
        Some(Definition::Local(31..32))
    );
    assert_eq!(
        def("select e.sal¦ary from employees e"),
        Some(Definition::Object {
            owner: "HR".into(),
            name: "EMPLOYEES".into(),
            object_type: "TABLE".into(),
            member: Some("SALARY".into()),
            parameter: None
        })
    );
    let _ = src;
    assert_eq!(
        def("with t as (select 1 x from dual) select t.x from t¦"),
        Some(Definition::Local(5..6))
    );
    assert_eq!(
        def("begin emp_pkg.get_na¦me(1); end;"),
        Some(Definition::Object {
            owner: "HR".into(),
            name: "EMP_PKG".into(),
            object_type: "PACKAGE".into(),
            member: Some("GET_NAME".into()),
            parameter: None
        })
    );
    assert_eq!(
        def("select * from employ¦ees"),
        Some(Definition::Object {
            owner: "HR".into(),
            name: "EMPLOYEES".into(),
            object_type: "TABLE".into(),
            member: None,
            parameter: None
        })
    );
}

#[test]
fn definition_of_qualified_local_package_member() {
    let src = format!("{HR_PKG}\nbegin\n  emp_pkg.get_name(1);\nend;\n/\n");
    let at = src.rfind("get_name(1)").unwrap() + 2;
    let target = Analysis::new(&src).definition(at, &hr()).target;
    assert_eq!(target, Some(Definition::Local(nth(&src, "get_name", 1))));
}

// ----- references -----------------------------------------------------------------

#[test]
fn references_of_local_variable() {
    let at = nth(HR_PKG, "l_count pls_integer", 0).start;
    let refs = Analysis::new(HR_PKG).references(at);
    assert_eq!(refs.len(), 10, "{refs:?}");
    assert!(
        refs.iter()
            .all(|r| HR_PKG[r.clone()].eq_ignore_ascii_case("l_count"))
    );
}

#[test]
fn references_of_subprogram_across_spec_and_body() {
    let at = nth(HR_PKG, "get_name", 0).start;
    let refs = Analysis::new(HR_PKG).references(at);
    assert_eq!(
        refs,
        [
            nth(HR_PKG, "get_name", 0),
            nth(HR_PKG, "get_name", 1),
            nth(HR_PKG, "get_name", 2)
        ]
    );
}

#[test]
fn references_of_alias_and_case_insensitivity() {
    let src = "select E.salary, e.first_name from employees e where e.salary > 1";
    let refs = Analysis::new(src).references(src.find("e where").unwrap());
    assert_eq!(refs.len(), 4, "{refs:?}");
    let src = "declare l_x number; \"l_x\" number; begin l_x := \"l_x\"; end;";
    let refs = Analysis::new(src).references(src.find("l_x").unwrap());
    assert_eq!(refs.len(), 2, "{refs:?}");
}

// ----- outline & diagnostics ----------------------------------------------------------

fn render_outline(items: &[OutlineItem], depth: usize, out: &mut String) {
    for i in items {
        let detail = if i.detail.is_empty() {
            String::new()
        } else {
            format!("  {}", i.detail)
        };
        out.push_str(&format!(
            "{}{:?} {}{detail}\n",
            "  ".repeat(depth),
            i.kind,
            i.name
        ));
        render_outline(&i.children, depth + 1, out);
    }
}

#[test]
fn outline_snapshot() {
    let mut out = String::new();
    for src in [HR_PKG, TRIGGERS, TYPES, SCRIPT] {
        render_outline(&outline(src), 0, &mut out);
        out.push_str("---\n");
    }
    insta::assert_snapshot!(out);
}

#[test]
fn diagnostics_on_samples() {
    for src in [HR_PKG, TRIGGERS, TYPES, SCRIPT] {
        assert!(diagnostics(src).is_empty());
    }
    let d = diagnostics("begin\n  if a then null;\nend;");
    assert_eq!(d[0].message, "IF without END IF");
}

#[test]
fn analysis_update_reparses_only_on_change() {
    let mut a = Analysis::new("select 1 from dual");
    assert!(!a.update("select 1 from dual"));
    assert!(a.update("select 2 from dual"));
    assert_eq!(a.source(), "select 2 from dual");
}

// ----- robustness & performance ------------------------------------------------------

#[test]
fn requests_never_panic_anywhere() {
    let cat = hr();
    for src in [HR_PKG, TRIGGERS, TYPES, SCRIPT] {
        let a = Analysis::new(src);
        let mut offset = 0;
        while offset <= src.len() {
            if src.is_char_boundary(offset) {
                let _ = a.complete(offset, &cat);
                let _ = a.signature_help(offset, &cat);
                let _ = a.hover(offset, &cat);
                let _ = a.definition(offset, &cat);
            }
            offset += 7;
        }
    }
}

#[test]
fn completion_on_every_prefix() {
    let cat = hr();
    let src = &HR_PKG[..HR_PKG.len().min(3000)];
    for cut in (0..src.len()).step_by(11) {
        if src.is_char_boundary(cut) {
            let _ = Analysis::new(&src[..cut]).complete(cut, &cat);
        }
    }
}

fn big_package(subprograms: usize) -> String {
    let mut src = String::from("create or replace package body big_pkg as\n");
    for i in 0..subprograms {
        src.push_str(&format!(
            "  procedure proc_{i}(p_id in number, p_name in varchar2) is\n    l_emp employees%rowtype;\n    l_total number := 0;\n  begin\n    select * into l_emp from employees e where e.employee_id = p_id;\n    for r in (select d.department_name from departments d where d.department_id = l_emp.department_id) loop\n      l_total := l_total + 1;\n    end loop;\n    if l_total > 10 then\n      proc_{}(p_id, p_name);\n    end if;\n  end proc_{i};\n\n",
            i.saturating_sub(1)
        ));
    }
    src.push_str("end big_pkg;\n/\n");
    src
}

#[test]
fn performance_20k_lines() {
    let src = big_package(1600);
    let lines = src.lines().count();
    assert!(lines >= 20_000, "{lines}");
    let t = std::time::Instant::now();
    let a = Analysis::new(&src);
    let parse = t.elapsed();
    let at = src.rfind("e.employee_id").unwrap() + 2;
    let cat = hr();
    let t = std::time::Instant::now();
    let c = a.complete(at, &cat);
    let complete = t.elapsed();
    let at_local = src.rfind("l_total := l_total").unwrap() + 3;
    let t = std::time::Instant::now();
    let c2 = a.complete(at_local, &cat);
    let complete_local = t.elapsed();
    eprintln!(
        "{lines} lines: parse {parse:?}, column completion {complete:?}, local completion {complete_local:?}"
    );
    assert!(!c.items.is_empty());
    assert!(!c2.items.is_empty());
    #[cfg(not(debug_assertions))]
    {
        assert!(complete.as_millis() < 5, "completion took {complete:?}");
        assert!(
            complete_local.as_millis() < 5,
            "completion took {complete_local:?}"
        );
        assert!(parse.as_millis() < 100, "parse took {parse:?}");
    }
}

// ----- navigation in packages (#7) ---------------------------------------------

const NAV_PKG: &str = "create or replace package nav_pkg as
  -- Raises a salary by a percentage.
  procedure raise(p_id in number, p_pct in number);
  /* Raises a salary by an amount,
     logged with a reason. */
  procedure raise(p_id in number, p_amount in number, p_reason in varchar2);
  g_count number; -- trailing, about g_count
  g_limit number;
end nav_pkg;
/
create or replace package body nav_pkg as
  procedure log_change(p_id in number, p_note in varchar2 := null) is
  begin
    null;
  end;

  procedure raise(p_id in number, p_pct in number) is
  begin
    raise(p_id, p_pct, 'pct');
    log_change(p_id => p_id);
  end;

  procedure raise(p_id in number, p_amount in number, p_reason in varchar2) is
  begin
    <<outer_loop>>
    for i in 1 .. 3 loop
      for j in 1 .. 3 loop
        exit outer_loop when j = 2;
        continue outer_loop;
      end loop;
    end loop outer_loop;
    goto done;
    <<done>>
    null;
    raise(p_id, 10);
    nav_pkg.raise(p_id => p_id, p_amount => 1, p_reason => 'x');
    emp_pkg.raise_salary(p_emp_id => p_id);
  end;
end nav_pkg;
/
";

fn nav_def(needle: &str, n: usize) -> Option<Definition> {
    let at = nth(NAV_PKG, needle, n).start + 1;
    Analysis::new(NAV_PKG).definition(at, &hr()).target
}

#[test]
fn labels_are_navigable() {
    let label = nth(NAV_PKG, "outer_loop", 0);
    for n in 1..=3 {
        // exit …, continue …, end loop …
        assert_eq!(
            nav_def("outer_loop", n),
            Some(Definition::Local(label.clone()))
        );
    }
    assert_eq!(
        nav_def("done;", 0),
        Some(Definition::Local(nth(NAV_PKG, "done", 1)))
    );
    let at = nth(NAV_PKG, "outer_loop", 1).start;
    let h = Analysis::new(NAV_PKG).hover(at, &hr()).unwrap();
    assert_eq!(h.title, "label outer_loop");
    let refs = Analysis::new(NAV_PKG).references(label.start);
    assert_eq!(refs.len(), 4, "{refs:?}");
}

/// The name in the n-th occurrence of `needle`, which starts with it.
fn name_in(needle: &str, n: usize, name: &str) -> std::ops::Range<usize> {
    let start = nth(NAV_PKG, needle, n).start;
    assert!(NAV_PKG[start..].starts_with(name));
    start..start + name.len()
}

#[test]
fn calls_resolve_to_the_overload_their_arguments_fit() {
    // Occurrence 1 of each header is the body implementation.
    let by_pct = name_in("raise(p_id in number, p_pct", 1, "raise");
    let by_amount = name_in("raise(p_id in number, p_amount", 1, "raise");
    assert_eq!(
        nav_def("raise(p_id, p_pct, 'pct')", 0),
        Some(Definition::Local(by_amount.clone()))
    );
    assert_eq!(
        nav_def("raise(p_id, 10)", 0),
        Some(Definition::Local(by_pct))
    );
    let at = nth(NAV_PKG, "nav_pkg.raise(p_id =>", 0).start + "nav_pkg.".len() + 1;
    assert_eq!(
        Analysis::new(NAV_PKG).definition(at, &hr()).target,
        Some(Definition::Local(by_amount))
    );
}

#[test]
fn named_arguments_name_the_callee_parameter() {
    // `log_change(p_id => p_id)`: the formal is log_change's p_id, the
    // actual the enclosing procedure's.
    let formal = nth(NAV_PKG, "p_id => p_id);", 0).start;
    let param = nth(NAV_PKG, "log_change(p_id in number", 0).start + "log_change(".len();
    let target = Analysis::new(NAV_PKG).definition(formal + 1, &hr()).target;
    assert_eq!(target, Some(Definition::Local(param..param + 4)));
    let actual = formal + "p_id => ".len();
    let enclosing = name_in("raise(p_id in number, p_pct", 1, "raise").end + 1;
    assert_eq!(
        Analysis::new(NAV_PKG).definition(actual + 1, &hr()).target,
        Some(Definition::Local(enclosing..enclosing + 4))
    );
    let h = Analysis::new(NAV_PKG).hover(formal + 1, &hr()).unwrap();
    assert_eq!(h.title, "parameter p_id of log_change");
    // A package in the database: its member, at the parameter.
    let at = nth(NAV_PKG, "p_emp_id =>", 0).start + 1;
    assert_eq!(
        Analysis::new(NAV_PKG).definition(at, &hr()).target,
        Some(Definition::Object {
            owner: "HR".into(),
            name: "EMP_PKG".into(),
            object_type: "PACKAGE".into(),
            member: Some("RAISE_SALARY".into()),
            parameter: Some("P_EMP_ID".into()),
        })
    );
}

#[test]
fn hover_shows_the_comment_above_a_declaration() {
    // On the body implementations: the spec's comments.
    let at = name_in("raise(p_id in number, p_pct", 1, "raise").start + 1;
    let h = Analysis::new(NAV_PKG).hover(at, &hr()).unwrap();
    assert!(
        h.detail.starts_with("Raises a salary by a percentage.\n"),
        "{}",
        h.detail
    );
    let at = name_in("raise(p_id in number, p_amount", 1, "raise").start + 1;
    let h = Analysis::new(NAV_PKG).hover(at, &hr()).unwrap();
    assert!(
        h.detail
            .starts_with("Raises a salary by an amount,\nlogged with a reason.\n"),
        "{}",
        h.detail
    );
    // A trailing comment belongs to its own line, not the next declaration.
    let at = nth(NAV_PKG, "g_limit", 0).start + 1;
    let h = Analysis::new(NAV_PKG).hover(at, &hr()).unwrap();
    assert_eq!(h.detail, "g_limit number");
}

/// A large schema: more names start with `mx` than fit the list. Completing
/// at `mx` says the list was cut, and completing again at `mxn_it` finds
/// the names past the cut, without names that merely contain the letters
/// (#160).
#[test]
fn cut_lists_say_so() {
    let mut c = CatalogCache::new("IFSAPP");
    let mut objects: Vec<CatalogObject> = (0..400)
        .map(|i| obj("IFSAPP", &format!("MXN_ACCESS_{i:03}_HIST"), "TABLE"))
        .collect();
    objects.push(obj("IFSAPP", "MXN_ITEM_TAB", "TABLE"));
    objects.push(obj("IFSAPP", "MXN_INVENTORY_TRANSACTION_TAB", "TABLE"));
    c.set_objects(None, objects);
    let early = completion("select * from mx¦", &c);
    assert!(early.incomplete);
    assert_eq!(early.items.len(), CompletionOptions::default().max_items);
    let names = |c: &Completion| c.items.iter().map(|i| i.label.clone()).collect::<Vec<_>>();
    assert!(!names(&early).contains(&"mxn_item_tab".to_owned()));
    let later = completion("select * from mxn_it¦", &c);
    assert!(!later.incomplete);
    assert_eq!(
        names(&later),
        ["mxn_item_tab", "mxn_inventory_transaction_tab"]
    );
}
