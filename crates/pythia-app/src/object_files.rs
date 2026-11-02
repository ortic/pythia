//! Database objects as files: one file per object in a folder per type
//! (`packages/billing.pks`), written by "Export schema to folder" and read
//! back to compare a file with the database.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use pythia_db::oracle::Connection;
use pythia_db::{DbError, ObjectType};

/// Folder and extension of each exported type.
const LAYOUT: &[(ObjectType, &str, &str)] = &[
    (ObjectType::Table, "tables", "sql"),
    (ObjectType::View, "views", "vw"),
    (ObjectType::MaterializedView, "materialized_views", "sql"),
    (ObjectType::Sequence, "sequences", "sql"),
    (ObjectType::Synonym, "synonyms", "sql"),
    (ObjectType::Type, "types", "tps"),
    (ObjectType::TypeBody, "types", "tpb"),
    (ObjectType::Package, "packages", "pks"),
    (ObjectType::PackageBody, "packages", "pkb"),
    (ObjectType::Procedure, "procedures", "prc"),
    (ObjectType::Function, "functions", "fnc"),
    (ObjectType::Trigger, "triggers", "trg"),
];

/// Where an object goes, relative to the export folder.
pub fn relative_path(object_type: ObjectType, name: &str) -> Option<PathBuf> {
    let (_, dir, ext) = LAYOUT.iter().find(|(t, ..)| *t == object_type)?;
    Some(Path::new(dir).join(format!("{}.{ext}", file_stem(name))))
}

/// The file name of the object `name`, without its extension: one of its
/// own whatever the file system, also where `Foo` and `foo` are one file
/// (Windows, macOS), and one Windows takes (#280). A name Oracle reads
/// unquoted is its lowercase as before (`DEMO_EMP` → `demo_emp`). Letters
/// are lowercase, and a `^` stands before each run of the other case
/// (`"Foo"` → `f^oo`, `"foo"` → `^foo`). What a file name can't hold on
/// Windows, `%` and `^` themselves, a `.` (which would read as the start
/// of the extension: `X.DDL`'s data was `x.ddl.sql`, the DDL file of `X`)
/// and a letter whose case doesn't turn back (`ß`) are `%` and the hex of
/// their UTF-8 (`"A/B"` → `a%2Fb`), as is the last letter of a device's
/// name (`CON` → `co%4E`).
/// [`object_name`] reads it back.
pub fn file_stem(name: &str) -> String {
    let mut stem = String::new();
    let escape = |stem: &mut String, c: char| {
        for b in c.encode_utf8(&mut [0; 4]).bytes() {
            stem.push_str(&format!("%{b:02X}"));
        }
    };
    let mut upper = true;
    for c in name.chars() {
        let case = match (lowered(c), raised(c)) {
            (Some(lower), _) => Some((true, lower)),
            (None, Some(_)) => Some((false, c)),
            (None, None) => None,
        };
        let cased = !c.to_lowercase().eq([c]) || !c.to_uppercase().eq([c]);
        match case {
            Some((case, letter)) => {
                if case != upper {
                    stem.push('^');
                    upper = case;
                }
                stem.push(letter);
            }
            None if cased || c.is_control() || "<>:\"/\\|?*%^.".contains(c) => {
                escape(&mut stem, c);
            }
            None => stem.push(c),
        }
    }
    // `con.sql` is the console on Windows, `nul.csv` nothing.
    let base = stem.split('.').next().unwrap_or_default();
    if is_device(base.trim_end_matches(' '))
        && let Some((at, c)) = base.trim_end_matches(' ').char_indices().last()
    {
        let mut escaped = String::new();
        escape(&mut escaped, raised(c).unwrap_or(c));
        stem.replace_range(at..at + c.len_utf8(), &escaped);
    }
    stem
}

/// The name of the object in a file named `stem`, as [`file_stem`] writes
/// it; also a stem of an older export, whose letters are all the name's in
/// uppercase. `None` if a `%` isn't followed by hex that makes UTF-8.
pub fn object_name(stem: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut upper = true;
    let mut chars = stem.chars();
    while let Some(c) = chars.next() {
        match c {
            '^' => upper = !upper,
            '%' => {
                let hex: String = chars.by_ref().take(2).collect();
                if hex.len() != 2 || !hex.chars().all(|h| h.is_ascii_hexdigit()) {
                    return None;
                }
                bytes.push(u8::from_str_radix(&hex, 16).ok()?);
            }
            c => {
                let c = raised(c).filter(|_| upper).unwrap_or(c);
                bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
        }
    }
    String::from_utf8(bytes).ok()
}

/// The lowercase of an uppercase letter that has one, which turns back
/// into it: `F` → `f`, `Ä` → `ä`; not `İ` (two characters) nor `K`, the
/// Kelvin sign, whose `k` turns into `K`.
fn lowered(c: char) -> Option<char> {
    let mut lower = c.to_lowercase();
    let (Some(l), None) = (lower.next(), lower.next()) else {
        return None;
    };
    let mut upper = l.to_uppercase();
    (l != c && upper.next() == Some(c) && upper.next().is_none()).then_some(l)
}

/// The uppercase of a lowercase letter that [`lowered`] turns back into it.
fn raised(c: char) -> Option<char> {
    let mut upper = c.to_uppercase();
    let (Some(u), None) = (upper.next(), upper.next()) else {
        return None;
    };
    (lowered(u) == Some(c)).then_some(u)
}

/// A name Windows keeps for a device, with any extension.
fn is_device(base: &str) -> bool {
    let base = base.to_ascii_uppercase();
    let port = |prefix: &str| {
        base.strip_prefix(prefix).is_some_and(|n: &str| {
            n.chars().count() == 1 && n.chars().all(|d| d.is_ascii_digit() || "¹²³".contains(d))
        })
    };
    port("COM")
        || port("LPT")
        || matches!(
            base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
}

/// The object a file holds: its name and type from the `CREATE` statement,
/// else from the file name (see [`file_stem`]) and extension
/// (`billing.pkb`).
pub fn object_of_file(path: &Path, text: &str) -> Option<(Option<String>, String, ObjectType)> {
    if let Some(header) = pythia_plsql::unit_header(text)
        && let Some(t) = ObjectType::from_dictionary(&header.object_type)
    {
        return Some((header.owner, header.name, t));
    }
    let stem = object_name(path.file_stem()?.to_str()?)?;
    let ext = path.extension()?.to_str()?.to_lowercase();
    let dir = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|d| d.to_str())
        .unwrap_or_default()
        .to_lowercase();
    // `.sql` is shared by several types; the folder decides.
    let t = LAYOUT
        .iter()
        .find(|(_, d, e)| *e == ext && (ext != "sql" || *d == dir))
        .map(|(t, ..)| *t)?;
    Some((None, stem, t))
}

/// The file contents for an object: runnable DDL ending with `/` for
/// stored units, without the schema name.
pub fn object_text(
    conn: &Connection,
    owner: &str,
    name: &str,
    object_type: ObjectType,
) -> Result<String, DbError> {
    if object_type.has_source() {
        let source = pythia_db::object_source(conn, owner, name, object_type)?;
        Ok(format!("{}\n/\n", source.trim_end()))
    } else {
        let ddl = pythia_db::object_ddl(conn, owner, name, object_type, false)?;
        Ok(format!("{}\n", ddl.trim_end()))
    }
}

#[derive(Default)]
pub struct ExportProgress {
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    pub stop: AtomicBool,
}

/// Writes every object of `owner` into `dir`. Returns the number of files.
pub fn export_schema(
    conn: &Connection,
    owner: &str,
    dir: &Path,
    progress: &ExportProgress,
) -> Result<usize, DbError> {
    let types: Vec<String> = LAYOUT
        .iter()
        .map(|(t, ..)| format!("'{}'", t.dictionary_name()))
        .collect();
    let sql = format!(
        "select object_name, object_type from all_objects
          where owner = :1 and object_type in ({})
            and generated = 'N' and object_name not like 'BIN$%'
            and not (object_type = 'TABLE' and (object_name like 'SYS\\_%' escape '\\'
                     or object_name in (select mview_name from all_mviews where owner = :1)))
          order by object_type, object_name",
        types.join(", ")
    );
    let objects: Vec<(String, String)> = conn
        .query_as::<(String, String)>(&sql, &[&owner])?
        .collect::<Result<_, _>>()?;
    progress.total.store(objects.len(), Ordering::Relaxed);
    let mut written = 0;
    for (name, type_name) in objects {
        if progress.stop.load(Ordering::Relaxed) {
            break;
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
        let Some(t) = ObjectType::from_dictionary(&type_name) else {
            continue;
        };
        let Some(relative) = relative_path(t, &name) else {
            continue;
        };
        let text = match object_text(conn, owner, &name, t) {
            Ok(text) => text,
            // Dropped since the list was read.
            Err(e) if e.code == Some(31603) || e.message.starts_with("No source found") => {
                continue;
            }
            Err(e) => return Err(e),
        };
        let path = dir.join(relative);
        let io = |e: std::io::Error| DbError::new(format!("{}: {e}", path.display()));
        std::fs::create_dir_all(path.parent().unwrap_or(dir)).map_err(io)?;
        std::fs::write(&path, text).map_err(io)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_files_to_objects() {
        assert_eq!(
            relative_path(ObjectType::PackageBody, "BILLING"),
            Some(PathBuf::from("packages/billing.pkb"))
        );
        let found = |p: &str, text: &str| object_of_file(Path::new(p), text);
        assert_eq!(
            found("x/packages/billing.pkb", ""),
            Some((None, "BILLING".into(), ObjectType::PackageBody))
        );
        assert_eq!(
            found("x/tables/orders.sql", "create table orders (id number);"),
            Some((None, "ORDERS".into(), ObjectType::Table))
        );
        assert_eq!(
            found(
                "anything.sql",
                "create or replace package body hr.pay as\nend;\n/"
            ),
            Some((Some("HR".into()), "PAY".into(), ObjectType::PackageBody))
        );
        assert_eq!(found("notes.txt", "hello"), None);
    }

    /// Each name has a file of its own, also where the file system takes
    /// `Foo` and `foo` for one file (Windows, macOS), with no character a
    /// file name can't hold on Windows; the file reads back as its name.
    #[test]
    fn every_name_has_a_file_of_its_own() {
        let names = [
            "FOO",
            "Foo",
            "foo",
            "fOO",
            "MyTable",
            "MY TABLE",
            "my table",
            "A/B",
            "A\\B",
            "A:B",
            "a<b>|?*\"",
            "50%^",
            "CON",
            "AUX",
            "COM1",
            "ÄRZTE",
            "Ärzte",
            "STRAßE",
            "TAB\tX",
            "X.DDL",
            "CON.X",
        ];
        let mut seen = std::collections::HashMap::new();
        for name in names {
            let path = relative_path(ObjectType::Table, name).unwrap();
            let file = path.file_name().unwrap().to_str().unwrap();
            assert_eq!(path.parent(), Some(Path::new("tables")), "{name}: {path:?}");
            assert!(
                !file.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*'])
                    && !file.chars().any(char::is_control),
                "{name}: {file}"
            );
            let stem = file.strip_suffix(".sql").unwrap();
            let device = stem.split('.').next().unwrap().to_uppercase();
            assert!(
                !["CON", "PRN", "AUX", "NUL", "COM1", "LPT1"].contains(&device.as_str()),
                "{name}: {file}"
            );
            if let Some(other) = seen.insert(file.to_lowercase(), name) {
                panic!("{name} and {other} both go to {file}");
            }
            let back = object_of_file(&Path::new("x").join(&path), "");
            assert_eq!(back, Some((None, name.to_owned(), ObjectType::Table)));
        }
        // Readable: plain names as before, case and other characters marked.
        for (name, stem) in [
            ("DEMO_EMPLOYEES", "demo_employees"),
            ("Foo", "f^oo"),
            ("foo", "^foo"),
            ("MyTable", "m^y^t^able"),
            ("MY TABLE", "my table"),
            ("A/B", "a%2Fb"),
            ("CON", "co%4E"),
            ("ÄRZTE", "ärzte"),
            // Not the DDL file of X, `x.ddl.sql`.
            ("X.DDL", "x%2Eddl"),
        ] {
            assert_eq!(file_stem(name), stem);
        }
        // Files of earlier exports, and files named by hand, read as before.
        assert_eq!(object_name("billing"), Some("BILLING".into()));
        assert_eq!(object_name("Billing"), Some("BILLING".into()));
        assert_eq!(object_name("a%2"), None);
        assert_eq!(object_name("x.ddl"), Some("X.DDL".into()));
    }

    #[test]
    fn exports_a_schema() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        conn.execute(
            "create or replace procedure pythia_export_p is begin null; end;",
            &[],
        )
        .unwrap();
        let owner = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        let dir = std::env::temp_dir().join(format!("pythia-export-{}", std::process::id()));
        let progress = ExportProgress::default();
        let n = export_schema(&conn, &owner, &dir, &progress).unwrap();
        assert!(n > 0);
        assert_eq!(
            progress.done.load(Ordering::Relaxed),
            progress.total.load(Ordering::Relaxed)
        );
        let text = std::fs::read_to_string(dir.join("procedures/pythia_export_p.prc")).unwrap();
        assert!(
            text.starts_with("CREATE OR REPLACE procedure pythia_export_p is"),
            "{text}"
        );
        assert!(text.ends_with("\n/\n"));
        // Reading it back finds the object and the same text.
        let (_, name, t) =
            object_of_file(&dir.join("procedures/pythia_export_p.prc"), &text).unwrap();
        assert_eq!(object_text(&conn, &owner, &name, t).unwrap(), text);
        conn.execute("drop procedure pythia_export_p", &[]).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Tables whose names differ only in case, or hold a `/` or a `:`, are
    /// exported each to a file of its own in `tables`, which reads back as
    /// the table.
    #[test]
    fn exports_each_object_to_a_file_of_its_own() {
        let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
            return;
        };
        let names = [
            "R54_SCH_FOO",
            "R54_SCH_Foo",
            "r54_sch_foo",
            "R54_SCH_A/B",
            "R54_SCH_A:B",
        ];
        for name in names {
            let _ = conn.execute(&format!("drop table \"{name}\" purge"), &[]);
            conn.execute(&format!("create table \"{name}\" (id number)"), &[])
                .unwrap();
        }
        let owner = conn
            .query_row_as::<String>("select user from dual", &[])
            .unwrap();
        let dir = std::env::temp_dir().join(format!("pythia-r54-schema-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let exported = export_schema(&conn, &owner, &dir, &ExportProgress::default());
        let mut files = Vec::new();
        let mut folders = vec![dir.clone()];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(&folder).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    folders.push(path);
                } else {
                    let text = std::fs::read_to_string(&path).unwrap();
                    files.push((path.strip_prefix(&dir).unwrap().to_owned(), text));
                }
            }
        }
        for name in names {
            let _ = conn.execute(&format!("drop table \"{name}\" purge"), &[]);
        }
        std::fs::remove_dir_all(&dir).unwrap();
        exported.unwrap();
        let ours: Vec<_> = files
            .iter()
            .filter(|(_, text)| text.to_uppercase().contains("CREATE TABLE \"R54_SCH_"))
            .collect();
        assert_eq!(ours.len(), names.len(), "{ours:?}");
        for name in names {
            let ddl = format!("CREATE TABLE \"{name}\"");
            let (path, text) = ours
                .iter()
                .find(|(_, text)| text.contains(&ddl))
                .unwrap_or_else(|| panic!("{name}: {ours:?}"));
            assert_eq!(path.parent(), Some(Path::new("tables")), "{name}");
            let back = object_of_file(&dir.join(path), text);
            assert_eq!(back, Some((None, name.to_owned(), ObjectType::Table)));
        }
    }
}
