//! "Execute to file": streams a query's rows straight into a file, for
//! results too large for the grid.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use pythia_data::{Format, Options, StreamWriter};
use pythia_db::DbError;
use pythia_db::oracle::Connection;

/// Rows fetched per round trip and written per batch.
const BATCH: usize = 1000;

/// A running export, polled by the window for progress.
pub struct ToFile {
    pub id: u64,
    pub path: PathBuf,
    pub sql: String,
    pub rows: Arc<AtomicU64>,
    pub started: Instant,
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
    let format = format_for(path);
    let file = std::fs::File::create(path)
        .map_err(|e| DbError::new(format!("{}: {e}", path.display())))?;
    let out = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut out = Some(out);
    let mut writer: Option<StreamWriter<std::io::BufWriter<std::fs::File>>> = None;
    let io = |e: pythia_data::Error| DbError::new(format!("{}: {e}", path.display()));
    let result = pythia_db::stream_query(conn, sql, BATCH, |columns, rows| {
        if writer.is_none()
            && let Some(out) = out.take()
        {
            // INSERTs into the table a one-table query reads (#158).
            let defaults = Options::default();
            let options = Options {
                table: crate::data_edit::single_table(sql).unwrap_or(defaults.table.clone()),
                ..defaults
            };
            writer = Some(StreamWriter::new(format, columns.to_vec(), options, out).map_err(io)?);
        }
        if let Some(w) = writer.as_mut() {
            w.write(rows).map_err(io)?;
            progress.store(w.rows(), Ordering::Relaxed);
        }
        Ok(())
    })
    .and_then(|count| {
        if let Some(w) = writer.take() {
            w.finish().map_err(io)?;
        }
        Ok(count)
    });
    if result.is_err() {
        drop(writer);
        let _ = std::fs::remove_file(path);
    }
    result
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
            crate::sql_window::find_substitutions(&text).is_empty(),
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
}
