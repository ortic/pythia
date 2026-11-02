//! Snippets with tab stops (`${1:default}`, `$2`, `$0`), VS Code style.
//!
//! A tab inside the body (`\t`) stands for one indentation unit; lines after
//! the first get the indentation of the line the snippet is inserted on.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// Word that expands with Tab, e.g. `sel`.
    pub trigger: String,
    pub description: String,
    pub body: String,
}

impl Snippet {
    pub fn new(trigger: &str, description: &str, body: &str) -> Self {
        Self {
            trigger: trigger.into(),
            description: description.into(),
            body: body.into(),
        }
    }
}

/// A body with indentation applied and its tab stops located.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    pub text: String,
    /// Char ranges in `text` per stop, in visiting order (`$1`, `$2`, …, `$0`
    /// last). Mirrored stops (same number) have several ranges.
    pub stops: Vec<Vec<Range<usize>>>,
}

/// Whether `body` uses snippet syntax (otherwise it's plain text).
pub fn has_tab_stops(body: &str) -> bool {
    let b = body.as_bytes();
    b.windows(2)
        .any(|w| w[0] == b'$' && (w[1].is_ascii_digit() || w[1] == b'{'))
}

/// Expands a snippet body. `indent` is the current line's indentation,
/// `unit` one indentation level.
pub fn expand(body: &str, indent: &str, unit: &str) -> Expanded {
    let mut text = String::new();
    let mut len = 0; // chars in `text`
    let mut stops: Vec<(u32, Range<usize>)> = Vec::new();
    // Default text per stop number, repeated by mirrors (`$1` after `${1:x}`).
    let mut defaults: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    let mut chars = body.chars().peekable();
    let push = |text: &mut String, len: &mut usize, c: char| {
        if c == '\n' {
            text.push('\n');
            text.push_str(indent);
            *len += 1 + indent.chars().count();
        } else if c == '\t' {
            text.push_str(unit);
            *len += unit.chars().count();
        } else {
            text.push(c);
            *len += 1;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('$' | '}' | '\\')) => {
                let next = chars.next().expect("peeked");
                push(&mut text, &mut len, next);
            }
            '$' if chars.peek().is_some_and(|c| c.is_ascii_digit()) => {
                let mut n = 0u32;
                while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                    n = n * 10 + d;
                    chars.next();
                }
                let start = len;
                for c in defaults.get(&n).cloned().unwrap_or_default().chars() {
                    push(&mut text, &mut len, c);
                }
                stops.push((n, start..len));
            }
            '$' if chars.peek() == Some(&'{') => {
                chars.next();
                let mut n = 0u32;
                while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                    n = n * 10 + d;
                    chars.next();
                }
                let start = len;
                if chars.peek() == Some(&':') {
                    chars.next();
                    let mut default = String::new();
                    while let Some(c) = chars.next() {
                        match c {
                            '}' => break,
                            '\\' if matches!(chars.peek(), Some('$' | '}' | '\\')) => {
                                default.push(chars.next().expect("peeked"));
                            }
                            c => default.push(c),
                        }
                    }
                    for c in default.chars() {
                        push(&mut text, &mut len, c);
                    }
                    defaults.entry(n).or_insert(default);
                } else {
                    if chars.peek() == Some(&'}') {
                        chars.next();
                    }
                    for c in defaults.get(&n).cloned().unwrap_or_default().chars() {
                        push(&mut text, &mut len, c);
                    }
                }
                stops.push((n, start..len));
            }
            c => push(&mut text, &mut len, c),
        }
    }
    // Group the ranges per stop number, in visiting order.
    let mut ordered: Vec<u32> = stops.iter().map(|(n, _)| *n).filter(|n| *n != 0).collect();
    ordered.sort_unstable();
    ordered.dedup();
    let mut grouped: Vec<Vec<Range<usize>>> = ordered
        .iter()
        .map(|n| {
            stops
                .iter()
                .filter(|(m, _)| m == n)
                .map(|(_, r)| r.clone())
                .collect()
        })
        .collect();
    // `$0` (or the end) is where the caret ends up.
    let last = stops
        .iter()
        .find(|(n, _)| *n == 0)
        .map_or(len..len, |(_, r)| r.clone());
    grouped.push(vec![last]);
    Expanded {
        text,
        stops: grouped,
    }
}

/// An active snippet: its stops (as absolute char ranges, kept up to date
/// through edits) and the current one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub stops: Vec<Vec<Range<usize>>>,
    pub current: usize,
}

impl Session {
    pub fn is_last(&self) -> bool {
        self.current + 1 >= self.stops.len()
    }

    /// Whether `pos` is inside (or at the edge of) a range of the current stop.
    pub fn contains(&self, pos: usize) -> bool {
        self.stops
            .get(self.current)
            .is_some_and(|rs| rs.iter().any(|r| r.start <= pos && pos <= r.end))
    }
}

/// The [`default_snippets`] of PL/SQL, Oracle's: other databases' editors
/// don't offer them, which their server refused (#469).
pub const PLSQL_TRIGGERS: &[&str] = &[
    "begin", "decl", "if", "ifel", "elsif", "case", "loop", "whilel", "fori", "forc", "cur", "exc",
    "rae", "dbms", "proc", "func", "pkg", "pkgb", "trg", "rec",
];

/// Built-in SQL and PL/SQL snippets.
pub fn default_snippets() -> Vec<Snippet> {
    vec![
        Snippet::new(
            "sel",
            "select … from …",
            "select ${1:*}\n  from ${2:table_name}\n where ${3:1 = 1}$0",
        ),
        Snippet::new(
            "selc",
            "select count(*)",
            "select count(*)\n  from ${1:table_name}\n where ${2:1 = 1}$0",
        ),
        Snippet::new(
            "ins",
            "insert … values",
            "insert into ${1:table_name}\n  (${2:columns})\nvalues\n  (${3:values});$0",
        ),
        Snippet::new(
            "inss",
            "insert … select",
            "insert into ${1:table_name}\n  (${2:columns})\nselect ${3:columns}\n  from ${4:source};$0",
        ),
        Snippet::new(
            "upd",
            "update … set … where",
            "update ${1:table_name}\n   set ${2:column} = ${3:value}\n where ${4:condition};$0",
        ),
        Snippet::new(
            "del",
            "delete … where",
            "delete from ${1:table_name}\n where ${2:condition};$0",
        ),
        Snippet::new(
            "merge",
            "merge into … using …",
            "merge into ${1:target} t\nusing ${2:source} s\n   on (t.${3:id} = s.$3)\n when matched then\n\tupdate set t.${4:column} = s.$4\n when not matched then\n\tinsert (${5:columns})\n\tvalues (${6:values});$0",
        ),
        Snippet::new(
            "ctas",
            "create table … as select",
            "create table ${1:new_table} as\nselect ${2:*}\n  from ${3:source_table};$0",
        ),
        Snippet::new("begin", "begin … end block", "begin\n\t$0\nend;"),
        Snippet::new(
            "decl",
            "declare … begin … end block",
            "declare\n\t${1:l_value} ${2:number};\nbegin\n\t$0\nend;",
        ),
        Snippet::new("if", "if … end if", "if ${1:condition} then\n\t$0\nend if;"),
        Snippet::new(
            "ifel",
            "if … else … end if",
            "if ${1:condition} then\n\t${2:null;}\nelse\n\t${3:null;}\nend if;$0",
        ),
        Snippet::new("elsif", "elsif branch", "elsif ${1:condition} then\n\t$0"),
        Snippet::new(
            "case",
            "case statement",
            "case ${1:selector}\n\twhen ${2:value} then\n\t\t${3:null;}\n\telse\n\t\t${4:null;}\nend case;$0",
        ),
        Snippet::new(
            "loop",
            "loop … exit when … end loop",
            "loop\n\texit when ${1:condition};\n\t$0\nend loop;",
        ),
        Snippet::new(
            "whilel",
            "while … loop",
            "while ${1:condition} loop\n\t$0\nend loop;",
        ),
        Snippet::new(
            "fori",
            "for i in 1 .. n loop",
            "for ${1:i} in ${2:1} .. ${3:n} loop\n\t$0\nend loop;",
        ),
        Snippet::new(
            "forc",
            "cursor for loop over a query",
            "for ${1:r} in (\n\tselect ${2:*}\n\t  from ${3:table_name}\n) loop\n\t$0\nend loop;",
        ),
        Snippet::new(
            "cur",
            "explicit cursor with a for loop",
            "cursor ${1:c_rows} is\n\tselect ${2:*}\n\t  from ${3:table_name};\n\nfor ${4:r} in $1 loop\n\t$0\nend loop;",
        ),
        Snippet::new(
            "exc",
            "exception handler",
            "exception\n\twhen ${1:no_data_found} then\n\t\t${2:null;}\n\twhen others then\n\t\t${3:raise;}$0",
        ),
        Snippet::new(
            "rae",
            "raise_application_error",
            "raise_application_error(-${1:20001}, '${2:message}');$0",
        ),
        Snippet::new(
            "dbms",
            "dbms_output.put_line",
            "dbms_output.put_line(${1:'message'});$0",
        ),
        Snippet::new(
            "proc",
            "create or replace procedure",
            "create or replace procedure ${1:proc_name}(\n\t${2:p_id} in ${3:number}\n) is\nbegin\n\t$0\nend $1;\n/",
        ),
        Snippet::new(
            "func",
            "create or replace function",
            "create or replace function ${1:func_name}(\n\t${2:p_id} in ${3:number}\n) return ${4:number} is\n\tl_result $4;\nbegin\n\t$0\n\treturn l_result;\nend $1;\n/",
        ),
        Snippet::new(
            "pkg",
            "package specification",
            "create or replace package ${1:pkg_name} as\n\n\tprocedure ${2:do_something}(${3:p_id} in number);\n\nend $1;\n/",
        ),
        Snippet::new(
            "pkgb",
            "package body",
            "create or replace package body ${1:pkg_name} as\n\n\tprocedure ${2:do_something}(${3:p_id} in number) is\n\tbegin\n\t\t$0\n\tend $2;\n\nend $1;\n/",
        ),
        Snippet::new(
            "trg",
            "row-level trigger",
            "create or replace trigger ${1:trg_name}\n\t${2:before} ${3:insert or update} on ${4:table_name}\n\tfor each row\nbegin\n\t$0\nend;\n/",
        ),
        Snippet::new(
            "rec",
            "record type",
            "type ${1:t_rec} is record (\n\t${2:id} ${3:number},\n\t${4:name} ${5:varchar2(100)}\n);$0",
        ),
        Snippet::new(
            "tab",
            "collection (table) type",
            "type ${1:t_tab} is table of ${2:number} index by ${3:pls_integer};$0",
        ),
        Snippet::new(
            "bulk",
            "bulk collect + forall",
            "declare\n\ttype t_ids is table of ${1:table_name}.${2:id}%type;\n\tl_ids t_ids;\nbegin\n\tselect $2\n\t  bulk collect into l_ids\n\t  from $1\n\t where ${3:1 = 1};\n\n\tforall i in 1 .. l_ids.count\n\t\tupdate $1\n\t\t   set ${4:column} = ${5:value}\n\t\t where $2 = l_ids(i);\nend;$0",
        ),
        Snippet::new(
            "ex",
            "execute immediate … using",
            "execute immediate '${1:statement}'\n\tusing ${2:l_value};$0",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_stops_and_indentation() {
        let e = expand("if ${1:x > 0} then\n\t$0\nend if;", "  ", "    ");
        assert_eq!(e.text, "if x > 0 then\n      \n  end if;");
        assert_eq!(e.stops, vec![vec![3..8], vec![20..20]]);
    }

    #[test]
    fn mirrors_and_order() {
        let e = expand("${2:b} ${1:a} $1 ${3}\\$x", "", "\t");
        assert_eq!(e.text, "b a a $x");
        // Visiting order 1, 2, 3, then the end ($0 missing); `$1` mirrors `a`.
        assert_eq!(
            e.stops,
            vec![vec![2..3, 4..5], vec![0..1], vec![6..6], vec![8..8]]
        );
    }

    #[test]
    fn detects_snippet_syntax() {
        assert!(has_tab_stops("select ${1:*}"));
        assert!(has_tab_stops("$0"));
        assert!(!has_tab_stops("v$session"));
    }

    #[test]
    fn default_snippets_are_well_formed() {
        let all = default_snippets();
        for trigger in PLSQL_TRIGGERS {
            assert!(all.iter().any(|s| s.trigger == *trigger), "{trigger}");
        }
        let mut triggers: Vec<&str> = all.iter().map(|s| s.trigger.as_str()).collect();
        triggers.sort_unstable();
        triggers.dedup();
        assert_eq!(triggers.len(), all.len(), "duplicate triggers");
        for s in &all {
            let e = expand(&s.body, "", "  ");
            assert!(!e.text.contains("${"), "{}: {}", s.trigger, e.text);
            assert!(
                e.stops
                    .iter()
                    .all(|rs| rs.iter().all(|r| r.end <= e.text.chars().count()))
            );
        }
    }
}
