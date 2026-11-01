//! "Execute to file": streams a query's rows straight into a file, for
//! results too large for the grid. On Oracle a task on the session reads
//! them; on the other databases the window fetches the result batch by
//! batch, as for the grid but with binary values whole (#204), and hands
//! each to a [`Sink`].

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use pythia_data::{Format, Options, StreamWriter};
use pythia_db::oracle::Connection;
use pythia_db::{Cell, Column, DbError, Dialect};

/// Rows fetched per round trip and written per batch.
pub const BATCH: usize = 1000;

/// A running export, polled by the window for progress.
pub struct ToFile {
    pub id: u64,
    pub path: PathBuf,
    pub sql: String,
    pub rows: Arc<AtomicU64>,
    pub started: Instant,
    /// Where the window writes the rows it fetches (databases other than
    /// Oracle); `None` while a task writes them.
    pub sink: Option<Sink>,
}

/// Writes a query's rows into a file as they come: its columns first, then
/// batches of rows. The file is removed unless it is finished.
pub struct Sink {
    path: PathBuf,
    format: Format,
    options: Options,
    out: Option<BufWriter<File>>,
    writer: Option<StreamWriter<BufWriter<File>>>,
}

impl Sink {
    /// Creates the file for the rows of `sql`; INSERT statements go into
    /// the table a one-table query reads (#158), in `dialect`'s SQL, and set
    /// the session's `time_zone` on MySQL (#213).
    pub fn create(
        path: &Path,
        sql: &str,
        dialect: Dialect,
        time_zone: Option<String>,
    ) -> Result<Self, DbError> {
        let file =
            File::create(path).map_err(|e| DbError::new(format!("{}: {e}", path.display())))?;
        let defaults = Options::default();
        Ok(Self {
            path: path.to_owned(),
            format: format_for(path),
            options: Options {
                table: crate::data_edit::single_table(sql).unwrap_or(defaults.table.clone()),
                dialect,
                time_zone,
                ..defaults
            },
            out: Some(BufWriter::with_capacity(1 << 20, file)),
            writer: None,
        })
    }

    fn io(&self, e: pythia_data::Error) -> DbError {
        DbError::new(format!("{}: {e}", self.path.display()))
    }

    /// The result's columns: writes the header.
    pub fn columns(&mut self, columns: &[Column]) -> Result<(), DbError> {
        if let Some(out) = self.out.take() {
            let writer =
                StreamWriter::new(self.format, columns.to_vec(), self.options.clone(), out)
                    .map_err(|e| self.io(e))?;
            self.writer = Some(writer);
        }
        Ok(())
    }

    /// Writes rows; returns how many were written so far.
    pub fn rows(&mut self, rows: &[Vec<Cell>]) -> Result<u64, DbError> {
        let Some(writer) = self.writer.as_mut() else {
            return Err(DbError::new("Rows before the columns"));
        };
        let written = writer.write(rows).map(|()| writer.rows());
        written.map_err(|e| self.io(e))
    }

    /// Completes the file (a JSON array's end, an Excel sheet).
    pub fn finish(mut self) -> Result<(), DbError> {
        let result = match self.writer.take() {
            Some(writer) => writer.finish().map(|_| ()).map_err(|e| self.io(e)),
            None => Ok(()),
        };
        if result.is_err() {
            let _ = std::fs::remove_file(&self.path);
        }
        result
    }

    /// Gives up: the incomplete file is removed.
    pub fn abandon(mut self) {
        self.writer = None;
        self.out = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The format for a file name; CSV when the extension isn't a streaming one.
pub fn format_for(path: &Path) -> Format {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    Format::STREAMING
        .iter()
        .copied()
        .find(|f| f.extension() == ext)
        .unwrap_or(Format::Csv)
}

/// Runs `sql` and writes every row to `path`; removes the file on failure
/// (including cancellation). Returns the number of rows written.
pub fn run(
    conn: &Connection,
    sql: &str,
    path: &Path,
    progress: &AtomicU64,
) -> Result<u64, DbError> {
    let mut sink = Sink::create(path, sql, Dialect::ORACLE, None)?;
    let mut started = false;
    let streamed = pythia_db::stream_query(conn, sql, BATCH, |columns, rows| {
        if !started {
            sink.columns(columns)?;
            started = true;
        }
        progress.store(sink.rows(rows)?, Ordering::Relaxed);
        Ok(())
    });
    match streamed {
        Ok(count) => sink.finish().map(|()| count),
        Err(e) => {
            sink.abandon();
            Err(e)
        }
    }
}

/// Asks for the target file on a background thread (the dialog blocks).
pub fn pick_file() -> std::sync::mpsc::Receiver<Option<PathBuf>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new().set_file_name("result.csv");
        for format in Format::STREAMING {
            dialog = dialog.add_filter(format.label(), &[format.extension()]);
        }
        let _ = tx.send(dialog.save_file());
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_format_from_extension() {
        assert_eq!(format_for(Path::new("a.json")), Format::Json);
        assert_eq!(format_for(Path::new("a.SQL")), Format::Insert);
        assert_eq!(format_for(Path::new("a.txt")), Format::Csv);
    }

    /// The rows as the window hands them over, batch by batch, in another
    /// database's SQL; a sink given up leaves no file.
    #[test]
    fn sink_writes_batches() {
        let path = std::env::temp_dir().join(format!("pythia-sink-{}.sql", std::process::id()));
        let columns = vec![
            Column {
                name: "id".into(),
                type_name: "INTEGER".into(),
                numeric: true,
            },
            Column {
                name: "note".into(),
                type_name: "TEXT".into(),
                numeric: false,
            },
        ];
        let mut sink =
            Sink::create(&path, "select id, note from notes", Dialect::SQLITE, None).unwrap();
        sink.columns(&columns).unwrap();
        assert_eq!(
            sink.rows(&[vec![Some("1".into()), Some("it's".into())]])
                .unwrap(),
            1
        );
        assert_eq!(sink.rows(&[vec![Some("2".into()), None]]).unwrap(), 2);
        sink.finish().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "insert into notes (id, note) values (1, 'it''s');",
                "insert into notes (id, note) values (2, NULL);"
            ]
        );
        std::fs::remove_file(&path).unwrap();

        let sink = Sink::create(&path, "select 1", Dialect::SQLITE, None).unwrap();
        assert!(path.exists());
        sink.abandon();
        assert!(!path.exists());
    }

    #[test]
    fn writes_query_to_file() {
        let Ok(spec) = std::env::var("PYTHIA_TEST_DB") else {
            return;
        };
        let (cred, db) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once('/').unwrap();
        let conn = Connection::connect(user, pw, db).unwrap();
        let path = std::env::temp_dir().join(format!("pythia-to-file-{}.csv", std::process::id()));
        let progress = AtomicU64::new(0);
        let n = run(
            &conn,
            "select level as n, 'row ' || level as label from dual connect by level <= 2500",
            &path,
            &progress,
        )
        .unwrap();
        assert_eq!(n, 2500);
        assert_eq!(progress.load(Ordering::Relaxed), 2500);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2501);
        assert!(text.starts_with("N,LABEL\n1,row 1\n"));
        std::fs::remove_file(&path).unwrap();

        // INSERTs go into the table the query reads (#158).
        let sql_path = path.with_extension("sql");
        run(&conn, "select * from dual", &sql_path, &progress).unwrap();
        let text = std::fs::read_to_string(&sql_path).unwrap();
        std::fs::remove_file(&sql_path).unwrap();
        assert!(
            text.starts_with("insert into dual (dummy) values ('X');"),
            "{text}"
        );

        // A `&` in a value comes back as it was, and the statement asks
        // for no substitution variable when it runs (#159).
        run(
            &conn,
            "select 'Missing (H&L) && &x.' as name from dual",
            &sql_path,
            &progress,
        )
        .unwrap();
        let text = std::fs::read_to_string(&sql_path).unwrap();
        std::fs::remove_file(&sql_path).unwrap();
        assert!(
            crate::sql_window::find_substitutions(&text, pythia_plsql::Syntax::Oracle).is_empty(),
            "{text}"
        );
        let value = text
            .split_once("values (")
            .and_then(|(_, rest)| rest.trim_end().strip_suffix(");"))
            .unwrap();
        let back: String = conn
            .query_row_as(&format!("select {value} from dual"), &[])
            .unwrap();
        assert_eq!(back, "Missing (H&L) && &x.");

        // A failing query leaves no file behind.
        assert!(run(&conn, "select * from no_such_table_xyz", &path, &progress).is_err());
        assert!(!path.exists());
    }

    /// The INSERTs of BLOBs over the 2000 bytes a `hextoraw` takes (#314),
    /// run back into a copy of their table, are the same values; they
    /// failed with ORA-01704.
    #[test]
    fn writes_long_binary_values() {
        let Some(conn) = crate::test_db::connect("PYTHIA_TEST_DB") else {
            return;
        };
        for table in ["r65_lv", "r65_lv_copy"] {
            let _ = conn.execute(&format!("drop table {table} purge"), &[]);
            conn.execute(
                &format!("create table {table} (id number primary key, b blob)"),
                &[],
            )
            .unwrap();
        }
        conn.execute(
            "declare
               v blob;
               w blob;
             begin
               dbms_lob.createtemporary(v, true);
               for i in 1 .. 50 loop
                 dbms_lob.writeappend(v, 2000,
                   utl_raw.copies(utl_raw.cast_from_binary_integer(i), 500));
               end loop;
               insert into r65_lv values (1, v);
               insert into r65_lv values (2, empty_blob()) returning b into w;
               dbms_lob.copy(w, v, 3000);
               insert into r65_lv values (3, hextoraw('CAFE'));
               insert into r65_lv values (4, null);
             end;",
            &[],
        )
        .unwrap();
        let path =
            std::env::temp_dir().join(format!("pythia-long-binary-{}.sql", std::process::id()));
        let progress = AtomicU64::new(0);
        let n = run(&conn, "select * from r65_lv order by id", &path, &progress).unwrap();
        assert_eq!(n, 4);
        let script = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let statements = pythia_plsql::split_statements(&script);
        assert_eq!(statements.len(), 4, "{script}");
        for statement in statements {
            let sql = statement.text(&script).replacen(
                "insert into r65_lv ",
                "insert into r65_lv_copy ",
                1,
            );
            conn.execute(&sql, &[])
                .unwrap_or_else(|e| panic!("{e}\n{sql}"));
        }
        let same: i64 = conn
            .query_row_as(
                "select count(*) from r65_lv a join r65_lv_copy c on c.id = a.id \
                  where dbms_lob.compare(a.b, c.b) = 0 or (a.b is null and c.b is null)",
                &[],
            )
            .unwrap();
        assert_eq!(same, 4);
        for table in ["r65_lv", "r65_lv_copy"] {
            conn.execute(&format!("drop table {table} purge"), &[])
                .unwrap();
        }
    }
}
