//! Export of query results: CSV, TSV, JSON, XLSX, INSERT statements,
//! Markdown and HTML.
//!
//! Cell values arrive as text (see `pythia_db::Cell`); column types decide how
//! they are written: numbers stay numbers in JSON/XLSX/INSERT (in XLSX
//! while their whole part fits the 15 digits Excel holds), INSERT scripts
//! write values as literals of the database they came from (`TO_DATE` on
//! Oracle, `X'…'` binary on MySQL, …).

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::io::Write;

pub mod import;
pub mod text;

use pythia_db::{Cell, Column, DbKind, Dialect};
use serde_json::value::{RawValue, to_raw_value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Csv,
    Tsv,
    Json,
    Xlsx,
    Insert,
    Markdown,
    Html,
}

impl Format {
    pub const ALL: &[Format] = &[
        Format::Csv,
        Format::Tsv,
        Format::Json,
        Format::Xlsx,
        Format::Insert,
        Format::Markdown,
        Format::Html,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Format::Csv => "CSV",
            Format::Tsv => "Tab separated",
            Format::Json => "JSON",
            Format::Xlsx => "Excel (xlsx)",
            Format::Insert => "INSERT statements",
            Format::Markdown => "Markdown table",
            Format::Html => "HTML table",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
            Format::Xlsx => "xlsx",
            Format::Insert => "sql",
            Format::Markdown => "md",
            Format::Html => "html",
        }
    }

    /// Text formats can go to the clipboard.
    pub fn is_text(self) -> bool {
        self != Format::Xlsx
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub header: bool,
    /// Target table for INSERT statements.
    pub table: String,
    /// Whose SQL INSERT statements are written in.
    pub dialect: Dialect,
    /// Sheet name for XLSX.
    pub sheet: String,
    /// CSV field separator (default `,`).
    pub delimiter: Option<u8>,
    /// How CSV/TSV write NULL (default: empty).
    pub null_text: String,
    /// The time zone a MySQL session showed TIMESTAMP values in: INSERT
    /// statements in MySQL's SQL set it while they run, as mysqldump does
    /// (#213).
    pub time_zone: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            header: true,
            table: "my_table".into(),
            dialect: Dialect::ORACLE,
            sheet: "Result".into(),
            delimiter: None,
            null_text: String::new(),
            time_zone: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Csv(#[from] csv::Error),
    #[error(transparent)]
    Xlsx(#[from] rust_xlsxwriter::XlsxError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A binary value the rows hold cut short (#204).
    #[error(
        "{column} holds a binary value of {size} bytes cut short to its first {} for the \
         grid; Execute to file writes such values whole",
        pythia_db::SHOWN_BYTES
    )]
    Cut { column: String, size: u64 },
    /// More than an Excel sheet or cell holds (#434): the library's own
    /// error named neither the column nor the row.
    #[error("{0}: export as CSV or JSON, which hold any size")]
    TooLarge(String),
}

/// Excel's limits: rows of a sheet, characters of a cell.
const XLSX_ROWS: usize = 1_048_576;
const XLSX_CELL_CHARS: usize = 32_767;

pub type Result<T> = std::result::Result<T, Error>;

/// Writes `rows` in `format`.
pub fn export(
    format: Format,
    columns: &[Column],
    rows: &[Vec<Cell>],
    options: &Options,
    out: &mut dyn Write,
) -> Result<()> {
    check_whole(columns, rows)?;
    match format {
        Format::Csv => delimited(
            options.delimiter.unwrap_or(b','),
            columns,
            rows,
            options,
            out,
        ),
        Format::Tsv => delimited(b'\t', columns, rows, options, out),
        Format::Json => json(columns, rows, options.dialect, out),
        Format::Xlsx => xlsx(columns, rows, options, out),
        Format::Insert => {
            let zone = insert_time_zone(columns, options);
            if let Some(zone) = zone {
                writeln!(out, "{}", set_time_zone(zone))?;
            }
            inserts(columns, rows, options, out)?;
            if zone.is_some() {
                writeln!(out, "{RESTORE_TIME_ZONE}")?;
            }
            Ok(())
        }
        Format::Markdown => markdown(columns, rows, out),
        Format::Html => html(columns, rows, out),
    }
}

/// Fails on the first binary value `rows` hold cut short (as results do
/// past [`pythia_db::SHOWN_BYTES`]) instead of writing part of it (#204).
pub fn check_whole(columns: &[Column], rows: &[Vec<Cell>]) -> Result<()> {
    for row in rows {
        for (col, cell) in columns.iter().zip(row) {
            if let Some(size) = cell.as_deref().and_then(pythia_db::cut_size) {
                return Err(Error::Cut {
                    column: col.name.clone(),
                    size,
                });
            }
        }
    }
    Ok(())
}

/// Convenience wrapper returning the exported bytes.
pub fn export_to_vec(
    format: Format,
    columns: &[Column],
    rows: &[Vec<Cell>],
    options: &Options,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    export(format, columns, rows, options, &mut out)?;
    Ok(out)
}

/// Writes rows batch by batch, for results too large to hold in memory.
/// Formats that need all rows up front (Excel, HTML) can't stream.
pub struct StreamWriter<W: Write> {
    format: Format,
    columns: Vec<Column>,
    options: Options,
    out: W,
    rows: u64,
}

impl Format {
    /// Formats [`StreamWriter`] supports.
    pub const STREAMING: &[Format] = &[
        Format::Csv,
        Format::Tsv,
        Format::Json,
        Format::Insert,
        Format::Markdown,
    ];
}

impl<W: Write> StreamWriter<W> {
    pub fn new(format: Format, columns: Vec<Column>, options: Options, mut out: W) -> Result<Self> {
        assert!(
            Format::STREAMING.contains(&format),
            "{format:?} can't stream"
        );
        match format {
            Format::Csv | Format::Tsv if options.header => {
                let delimiter = if format == Format::Csv {
                    options.delimiter.unwrap_or(b',')
                } else {
                    b'\t'
                };
                let mut writer = csv::WriterBuilder::new()
                    .delimiter(delimiter)
                    .from_writer(&mut out);
                writer.write_record(columns.iter().map(|c| c.name.as_str()))?;
                writer.flush()?;
            }
            Format::Json => write!(out, "[")?,
            Format::Markdown => markdown_header(&columns, &mut out)?,
            Format::Insert => {
                if let Some(zone) = insert_time_zone(&columns, &options) {
                    writeln!(out, "{}", set_time_zone(zone))?;
                }
            }
            _ => {}
        }
        Ok(Self {
            format,
            columns,
            options,
            out,
            rows: 0,
        })
    }

    pub fn write(&mut self, rows: &[Vec<Cell>]) -> Result<()> {
        check_whole(&self.columns, rows)?;
        let options = Options {
            header: false,
            ..self.options.clone()
        };
        match self.format {
            Format::Csv => delimited(
                options.delimiter.unwrap_or(b','),
                &self.columns,
                rows,
                &options,
                &mut self.out,
            )?,
            Format::Tsv => delimited(b'\t', &self.columns, rows, &options, &mut self.out)?,
            Format::Insert => inserts(&self.columns, rows, &options, &mut self.out)?,
            Format::Markdown => markdown_rows(rows, &mut self.out)?,
            Format::Json => {
                let keys = json_keys(&self.columns);
                for (i, row) in rows.iter().enumerate() {
                    let sep = if self.rows == 0 && i == 0 {
                        "\n"
                    } else {
                        ",\n"
                    };
                    let object = json_object(&keys, &self.columns, row, self.options.dialect)?;
                    let object = serde_json::to_string_pretty(&object)?;
                    write!(self.out, "{sep}{}", indent(&object))?;
                }
            }
            Format::Xlsx | Format::Html => unreachable!(),
        }
        self.rows += rows.len() as u64;
        Ok(())
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn finish(mut self) -> Result<W> {
        if self.format == Format::Json {
            let close = if self.rows == 0 { "]\n" } else { "\n]\n" };
            write!(self.out, "{close}")?;
        }
        if self.format == Format::Insert && insert_time_zone(&self.columns, &self.options).is_some()
        {
            writeln!(self.out, "{RESTORE_TIME_ZONE}")?;
        }
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Indents a pretty-printed JSON object by two spaces (array element).
fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn delimited(
    delimiter: u8,
    columns: &[Column],
    rows: &[Vec<Cell>],
    options: &Options,
    out: &mut dyn Write,
) -> Result<()> {
    let mut writer = csv::WriterBuilder::new()
        .delimiter(delimiter)
        .from_writer(out);
    if options.header {
        writer.write_record(columns.iter().map(|c| c.name.as_str()))?;
    }
    for row in rows {
        writer.write_record(
            row.iter()
                .map(|c| c.as_deref().unwrap_or(&options.null_text)),
        )?;
    }
    writer.flush()?;
    Ok(())
}

fn json(
    columns: &[Column],
    rows: &[Vec<Cell>],
    dialect: Dialect,
    out: &mut dyn Write,
) -> Result<()> {
    let keys = json_keys(columns);
    let array = rows
        .iter()
        .map(|row| json_object(&keys, columns, row, dialect))
        .collect::<serde_json::Result<Vec<_>>>()?;
    serde_json::to_writer_pretty(&mut *out, &array)?;
    writeln!(out)?;
    Ok(())
}

/// A row as a JSON object: its members in the result's order.
struct JsonObject<'a>(Vec<(&'a str, Box<RawValue>)>);

impl serde::Serialize for JsonObject<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(key, value)| (key, value)))
    }
}

/// The keys of the columns in a row's JSON object: their names, in the
/// result's order. A name that comes again takes the first `_2`, `_3`, …
/// that no column is called (#297): as one map, `select a.id, b.id` kept
/// one `id`, and the keys came sorted.
fn json_keys(columns: &[Column]) -> Vec<String> {
    let names: HashSet<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    let mut taken: HashSet<String> = HashSet::new();
    columns
        .iter()
        .map(|col| {
            let name = col.name.as_str();
            let key = if taken.contains(name) {
                (2..)
                    .map(|n| format!("{name}_{n}"))
                    .find(|k| !names.contains(k.as_str()) && !taken.contains(k))
                    .unwrap_or_default()
            } else {
                name.to_owned()
            };
            taken.insert(key.clone());
            key
        })
        .collect()
}

/// A row as a JSON object under `keys` (see [`json_keys`]). Numbers go in
/// as raw JSON, every digit (#267): as an i64 or a double,
/// 18446744073709551615 was written 1.8446744073709552e+19.
fn json_object<'a>(
    keys: &'a [String],
    columns: &[Column],
    row: &[Cell],
    dialect: Dialect,
) -> serde_json::Result<JsonObject<'a>> {
    let members = keys
        .iter()
        .zip(columns)
        .zip(row)
        .map(|((key, col), cell)| {
            let typed = match cell {
                Some(v) if col.numeric => json_number(v).map(RawValue::from_string),
                Some(v) if is_boolean(dialect, col) => boolean(v).map(|b| to_raw_value(&b)),
                _ => None,
            };
            // NULL as null, the rest as strings.
            let value = typed.unwrap_or_else(|| to_raw_value(cell))?;
            Ok((key.as_str(), value))
        })
        .collect::<serde_json::Result<_>>()?;
    Ok(JsonObject(members))
}

/// Columns of booleans (#217): PostgreSQL's and Oracle's `boolean`, SQL
/// Server's `bit` (PostgreSQL's is a string of bits). MySQL has none.
fn is_boolean(dialect: Dialect, col: &Column) -> bool {
    let t = col.type_name.as_str();
    t.eq_ignore_ascii_case("boolean")
        || t.eq_ignore_ascii_case("bool")
        || (dialect.kind() == DbKind::SqlServer && t == "bit")
}

/// A boolean as the sessions show it: `t`/`f`, `TRUE`/`FALSE`, `1`/`0`.
fn boolean(text: &str) -> Option<bool> {
    match text {
        "t" | "1" => Some(true),
        "f" | "0" => Some(false),
        _ if text.eq_ignore_ascii_case("true") => Some(true),
        _ if text.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

/// A number as JSON writes it, every digit kept (#267): without a `+`,
/// with a 0 before a leading `.` (`-.5` as `-0.5`) and none before other
/// digits. `None` for what isn't a decimal number (`NaN`, `Infinity`).
fn json_number(text: &str) -> Option<String> {
    let (sign, unsigned) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text.strip_prefix('+').unwrap_or(text)),
    };
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => unsigned.split_at(at),
        None => (unsigned, ""),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let power = exponent.get(1..).unwrap_or_default();
    let power = power.strip_prefix(['+', '-']).unwrap_or(power);
    if (whole.is_empty() && fraction.is_empty())
        || !digits(whole)
        || !digits(fraction)
        || (!exponent.is_empty() && (power.is_empty() || !digits(power)))
    {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    let point = if fraction.is_empty() { "" } else { "." };
    Some(format!("{sign}{whole}{point}{fraction}{exponent}"))
}

/// The significant digits of the whole part of a number written in
/// decimal, from its first to its last that isn't 0: 19 in
/// `9223372036854775807`, 1 in `1e20`, none in `0.333…`.
fn whole_digits(text: &str) -> usize {
    let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits: String = whole
        .chars()
        .chain(fraction.chars())
        .filter(char::is_ascii_digit)
        .collect();
    let shift = exponent.parse::<i64>().unwrap_or(0);
    let len = whole.chars().filter(char::is_ascii_digit).count() as i64 + shift;
    let len = len.clamp(0, digits.len() as i64) as usize;
    digits[..len].trim_matches('0').len()
}

/// A number for an Excel cell, unless Excel can't hold its whole part:
/// Excel keeps 15 significant digits, so a number whose whole part has
/// more stays text with all of them (#267); 9223372036854775807 would be
/// 9223372036854775808. A fraction rounds as Excel's own numbers do, so
/// an average of 40 digits stays a number.
fn excel_number(text: &str) -> Option<f64> {
    let n = text.parse::<f64>().ok()?;
    (whole_digits(text) <= 15).then_some(n)
}

fn xlsx(
    columns: &[Column],
    rows: &[Vec<Cell>],
    options: &Options,
    out: &mut dyn Write,
) -> Result<()> {
    use rust_xlsxwriter::{Format as XFormat, Workbook};
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet();
    sheet.set_name(sheet_name(&options.sheet))?;
    let bold = XFormat::new().set_bold();
    let date = XFormat::new().set_num_format("yyyy-mm-dd");
    let datetime = XFormat::new().set_num_format("yyyy-mm-dd hh:mm:ss");
    // Excel shows milliseconds; the cell holds what its double can (#192).
    let fraction = XFormat::new().set_num_format("yyyy-mm-dd hh:mm:ss.000");
    let first_row = u32::from(options.header);
    if rows.len() + first_row as usize > XLSX_ROWS {
        return Err(Error::TooLarge(format!(
            "An Excel sheet holds {XLSX_ROWS} rows; the result has {}",
            rows.len()
        )));
    }
    if let Some((r, col)) = rows.iter().enumerate().find_map(|(r, row)| {
        columns.iter().zip(row).find_map(|(col, cell)| {
            cell.as_deref()
                .filter(|v| v.chars().nth(XLSX_CELL_CHARS).is_some())
                .map(|_| (r, col))
        })
    }) {
        return Err(Error::TooLarge(format!(
            "Row {}'s {} is longer than the {XLSX_CELL_CHARS} characters an Excel cell holds",
            r + 1,
            col.name
        )));
    }
    if options.header {
        for (c, col) in columns.iter().enumerate() {
            sheet.write_string_with_format(0, c as u16, &col.name, &bold)?;
        }
    }
    for (r, row) in rows.iter().enumerate() {
        let r = r as u32 + first_row;
        for (c, (col, cell)) in columns.iter().zip(row).enumerate() {
            let Some(value) = cell else { continue };
            let c = c as u16;
            let as_date = if excel_date(col) {
                excel_datetime(value)
            } else {
                None
            };
            match (col.numeric, as_date) {
                (true, _) => match excel_number(value) {
                    Some(n) => sheet.write_number(r, c, n)?,
                    None => sheet.write_string(r, c, value)?,
                },
                (
                    false,
                    Some(ExcelValue {
                        date: dt,
                        time,
                        fraction: f,
                    }),
                ) => {
                    let format = match (time, f) {
                        (_, true) => &fraction,
                        (true, false) => &datetime,
                        (false, false) => &date,
                    };
                    sheet.write_datetime_with_format(r, c, &dt, format)?
                }
                (false, None) => sheet.write_string(r, c, value)?,
            };
        }
    }
    if options.header && !columns.is_empty() {
        sheet.set_freeze_panes(1, 0)?;
        sheet.autofilter(0, 0, rows.len() as u32, columns.len() as u16 - 1)?;
    }
    sheet.autofit();
    out.write_all(&workbook.save_to_buffer()?)?;
    Ok(())
}

/// Excel sheet names: max 31 chars, no `[]:*?/\`.
fn sheet_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !"[]:*?/\\".contains(*c))
        .take(31)
        .collect();
    if cleaned.is_empty() {
        "Result".into()
    } else {
        cleaned
    }
}

/// Columns written as Excel dates: the dates and timestamps of every
/// database (#193), but those with a time zone, which an Excel date can't
/// hold.
fn excel_date(col: &Column) -> bool {
    let t = col.type_name.to_ascii_uppercase();
    let zone = t.contains("TIME ZONE") || t.starts_with("DATETIMEOFFSET");
    !zone
        && (t == "DATE"
            || t == "SMALLDATETIME"
            || t.starts_with("TIMESTAMP")
            || t.starts_with("DATETIME"))
}

/// A date as an Excel date, and what of it to show.
struct ExcelValue {
    date: rust_xlsxwriter::ExcelDateTime,
    time: bool,
    fraction: bool,
}

/// A date as the sessions show it, `YYYY-MM-DD[ HH:MM:SS[.f…]]`, with its
/// fraction of a second (#192); anything after a space (a zone) is
/// dropped. Other text, and dates Excel doesn't have (before 1900), stay
/// text.
fn excel_datetime(text: &str) -> Option<ExcelValue> {
    let digits = |s: &str, at: &[usize], sep: u8| {
        s.bytes().enumerate().all(|(i, b)| {
            if at.contains(&i) {
                b == sep
            } else {
                b.is_ascii_digit()
            }
        })
    };
    let mut parts = text.split(' ');
    let date = parts.next()?;
    // Excel's days start with 1900.
    if date.len() != 10 || !digits(date, &[4, 7], b'-') || &date[..4] < "1900" {
        return None;
    }
    let Some(time) = parts.next() else {
        let date = rust_xlsxwriter::ExcelDateTime::parse_from_str(date).ok()?;
        return Some(ExcelValue {
            date,
            time: false,
            fraction: false,
        });
    };
    let (hms, fraction) = match time.split_once('.') {
        Some((hms, f)) => (hms, Some(f)),
        None => (time, None),
    };
    let fraction_ok =
        fraction.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()));
    if hms.len() != 8 || !digits(hms, &[2, 5], b':') || !fraction_ok {
        return None;
    }
    let date = rust_xlsxwriter::ExcelDateTime::parse_from_str(&format!("{date}T{time}")).ok()?;
    Some(ExcelValue {
        date,
        time: true,
        fraction: fraction.is_some(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Date,
    Timestamp,
    TimestampTz,
    Binary,
}

fn kind(col: &Column) -> Kind {
    let t = col.type_name.to_ascii_uppercase();
    if t == "DATE" {
        Kind::Date
    } else if t.starts_with("TIMESTAMP") && t.contains("TIME ZONE") {
        Kind::TimestampTz
    } else if t.starts_with("TIMESTAMP") {
        Kind::Timestamp
    } else if t.starts_with("RAW") || t == "BLOB" || t == "LONG RAW" {
        Kind::Binary
    } else {
        Kind::Text
    }
}

/// A numeric column's value that can go into SQL as it is: `inf` or `NaN`
/// would be read as names.
fn is_number(col: &Column, v: &str) -> bool {
    col.numeric && v.parse::<f64>().is_ok_and(f64::is_finite)
}

/// A BINARY_DOUBLE's or BINARY_FLOAT's value as a literal of its type
/// (#339): `1e300d`, `2.5f`, `binary_double_infinity`,
/// `-binary_float_infinity`, `binary_double_nan`. As a NUMBER, 1e300
/// overflows (ORA-01426) and 2e-300 is 0; as text, `'inf'` goes in but
/// compares with `Inf` as text. `None` for other columns and for text
/// that is no number.
fn binary_float(col: &Column, v: &str) -> Option<String> {
    let (suffix, name) = match col.type_name.as_str() {
        "BINARY_DOUBLE" => ('d', "binary_double"),
        "BINARY_FLOAT" => ('f', "binary_float"),
        _ => return None,
    };
    let number = v.parse::<f64>().ok()?;
    Some(if number.is_nan() {
        format!("{name}_nan")
    } else if number.is_infinite() && !v.bytes().any(|b| b.is_ascii_digit()) {
        let sign = if number < 0.0 { "-" } else { "" };
        format!("{sign}{name}_infinity")
    } else {
        format!("{v}{suffix}")
    })
}

/// Renders a value as a SQL literal for `col` (Oracle's SQL).
pub fn sql_literal(col: &Column, value: Option<&str>) -> String {
    let Some(v) = value else {
        return "NULL".into();
    };
    if let Some(binary) = binary_float(col, v) {
        return binary;
    }
    if is_number(col, v) {
        return v.to_owned();
    }
    let quoted = format!("'{}'", v.replace('\'', "''"));
    match kind(col) {
        Kind::Date => format!("to_date({quoted}, 'YYYY-MM-DD HH24:MI:SS')"),
        Kind::Timestamp if v.contains('.') => {
            format!("to_timestamp({quoted}, 'YYYY-MM-DD HH24:MI:SS.FF')")
        }
        Kind::Timestamp => format!("to_timestamp({quoted}, 'YYYY-MM-DD HH24:MI:SS')"),
        // TZR takes an offset (`+02:00`) or a region (`Europe/Zurich`).
        Kind::TimestampTz if v.contains('.') => {
            format!("to_timestamp_tz({quoted}, 'YYYY-MM-DD HH24:MI:SS.FF TZR')")
        }
        Kind::TimestampTz => format!("to_timestamp_tz({quoted}, 'YYYY-MM-DD HH24:MI:SS TZR')"),
        // Typed as the grid shows it, `0x…`, a value is the bytes of its
        // digits (#256, #271).
        Kind::Binary => match typed_hex(v) {
            Some(hex) => format!("hextoraw('{hex}')"),
            None => format!("hextoraw({quoted})"),
        },
        Kind::Text => {
            let text = text_value(col, v);
            oracle_json(col, &text, v, None).unwrap_or(text)
        }
    }
}

/// [`sql_literal`] of `value` for column `col`, `name` (quoted), which
/// held `held`: a JSON's scalars that are where they were and as they
/// were are taken from the column, with their types (see [`oracle_json`]).
pub fn sql_literal_replacing(col: &Column, value: &str, name: &str, held: &str) -> String {
    if kind(col) != Kind::Text || binary_float(col, value).is_some() || is_number(col, value) {
        return sql_literal(col, Some(value));
    }
    let text = text_value(col, value);
    oracle_json(col, &text, value, Some((name, held))).unwrap_or(text)
}

/// A text's literal. One holds 4000 bytes: a CLOB's longer text is a CLOB
/// of parts (#317), each a CLOB of its own: Oracle joins the plain
/// literals after a CLOB into one VARCHAR2 first, which with many `&`,
/// each a literal apart, failed with ORA-01489 (#342). Oracle parses many
/// parts slowly: INSERT statements append them in a PL/SQL block instead
/// (see `oracle_block`).
fn text_value(col: &Column, v: &str) -> String {
    if !long_text(col, v) {
        return text_literal(v);
    }
    text_parts(v)
        .into_iter()
        .map(|part| format!("to_clob({})", text_literal(part)))
        .collect::<Vec<_>>()
        .join(" || ")
}

/// The keys of the extended objects that Oracle JSON's scalars of types
/// JSON text has none of show as (#370), as Oracle writes them but a
/// binary value's, in hex as the grid shows binary values. Other keys
/// Oracle reads so, as MongoDB's `$oid` and `$date`, are an object's.
const JSON_EXTENDED: &[&str] = &[
    "$oracleDate",
    "$oracleTimestamp",
    "$oracleTimestampTZ",
    "$intervalDaySecond",
    "$intervalYearMonth",
    "$numberDouble",
    "$numberFloat",
    "$rawhex",
    "$rawid",
];

/// Whether `value` is one of those objects: one of their keys with text,
/// a binary number's with a number too. MongoDB writes `{"$numberDouble":
/// "1.5"}`.
fn json_extended(value: &serde_json::Value) -> bool {
    let Some(map) = value.as_object().filter(|map| map.len() == 1) else {
        return false;
    };
    map.iter().all(|(key, v)| match key.as_str() {
        "$numberDouble" | "$numberFloat" => {
            v.is_number() || matches!(v.as_str(), Some("Nan" | "Inf" | "-Inf"))
        }
        key => JSON_EXTENDED.contains(&key) && v.is_string(),
    })
}

/// A step of a path in a JSON value.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

/// The extended objects in `value` (see [`json_extended`]), by their paths.
fn extended_objects<'a>(
    value: &'a serde_json::Value,
    path: &mut Vec<Step<'a>>,
    out: &mut Vec<(Vec<Step<'a>>, &'a serde_json::Value)>,
) {
    if json_extended(value) {
        out.push((path.clone(), value));
        return;
    }
    let steps: Vec<(Step, &serde_json::Value)> = match value {
        serde_json::Value::Object(map) => map.iter().map(|(k, v)| (Step::Key(k), v)).collect(),
        serde_json::Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (Step::Index(i), v))
            .collect(),
        _ => return,
    };
    for (step, v) in steps {
        path.push(step);
        extended_objects(v, path, out);
        path.pop();
    }
}

/// The value at `path` in `value`.
fn json_at<'v>(value: &'v serde_json::Value, path: &[Step]) -> Option<&'v serde_json::Value> {
    path.iter().try_fold(value, |v, step| match step {
        Step::Key(key) => v.get(*key),
        Step::Index(i) => v.get(*i),
    })
}

/// `path` as a literal of Oracle's JSON path: `'$."a"[2]'`, a `&` as
/// `\u0026`, which a script doesn't read as a substitution variable (#159).
fn json_path(path: &[Step]) -> String {
    let mut out = String::from("'$");
    for step in path {
        match step {
            Step::Index(i) => out.push_str(&format!("[{i}]")),
            Step::Key(key) => {
                out.push_str(".\"");
                for c in key.chars() {
                    match c {
                        '"' | '\\' => {
                            out.push('\\');
                            out.push(c);
                        }
                        '\'' => out.push_str("''"),
                        c if c == '&' || c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
        }
    }
    out.push('\'');
    out
}

/// The literal of `v` for Oracle JSON column `col`, `text` its literal as
/// text, with its extended objects (see [`JSON_EXTENDED`]) the scalars they
/// stand for (#370): `json_transform(json('…'), set '$."a"' =
/// json('{"$oracleDate":"…"}' extended))`, or `json('…' extended)` where
/// `v` is one. Only those: `json('…' extended)` of all of `v` would read
/// MongoDB's `{"$oid":…}` as an ID too. Where `held` names the column and
/// the value it holds, a scalar there as it is at its place in `v` is taken
/// from the column, keeping its type: the driver reads a DATE, a TIMESTAMP
/// and a TIMESTAMP WITH TIME ZONE in a JSON alike. `None` for a JSON
/// without them, and for other columns.
fn oracle_json(col: &Column, text: &str, v: &str, held: Option<(&str, &str)>) -> Option<String> {
    if col.type_name != "JSON" || !v.contains("\"$") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(v).ok()?;
    let mut objects = Vec::new();
    extended_objects(&value, &mut Vec::new(), &mut objects);
    if objects.is_empty() {
        return None;
    }
    let held = held.and_then(|(name, held)| {
        Some((name, serde_json::from_str::<serde_json::Value>(held).ok()?))
    });
    let scalar = |path: &[Step], object: &serde_json::Value| match &held {
        Some((name, held)) if json_at(held, path) == Some(object) => {
            format!("json_query({name}, {} returning json)", json_path(path))
        }
        _ => format!("json({} extended)", text_literal(&object.to_string())),
    };
    if let [(path, object)] = objects.as_slice()
        && path.is_empty()
    {
        return Some(scalar(path, object));
    }
    let sets: Vec<String> = objects
        .iter()
        .map(|(path, object)| format!("set {} = {}", json_path(path), scalar(path, object)))
        .collect();
    Some(format!("json_transform(json({text}), {})", sets.join(", ")))
}

/// Text as an Oracle literal, each `&` apart (see [`apart_from_ampersands`]).
fn text_literal(v: &str) -> String {
    apart_from_ampersands(
        v,
        |p| format!("'{}'", p.replace('\'', "''")),
        "chr(38)",
        " || ",
    )
}

/// The most bytes a text literal holds (ORA-01704).
const TEXT_LITERAL_BYTES: usize = 4000;

/// The UTF-16 code units, which a CLOB's length counts, of each part of a
/// text too long for one literal: no database character set makes 1000
/// of them more than 4000 bytes.
const TEXT_PART_UNITS: usize = 1000;

/// Whether `v` is a CLOB's, NCLOB's or LONG's text too long for one
/// literal (#317); the pure-Rust driver types CLOB columns LONG. A JSON's
/// or VECTOR's too, which Oracle reads from a CLOB (#342).
fn long_text(col: &Column, v: &str) -> bool {
    matches!(
        col.type_name.as_str(),
        "CLOB" | "NCLOB" | "LONG" | "JSON" | "VECTOR"
    ) && v.len() > TEXT_LITERAL_BYTES
}

/// `text` in parts of at most [`TEXT_PART_UNITS`] UTF-16 code units.
fn text_parts(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut units) = (0, 0);
    for (at, c) in text.char_indices() {
        if units + c.len_utf16() > TEXT_PART_UNITS {
            parts.push(&text[start..at]);
            (start, units) = (at, 0);
        }
        units += c.len_utf16();
    }
    parts.push(&text[start..]);
    parts
}

/// A text literal with each `&` outside the quotes, as `amp` joined by
/// `concat`. Where scripts run, in SQL*Plus and Pythia's SQL windows, a
/// `&name` inside a literal is a substitution variable: `'Missing (H&L)'`
/// would ask for `L` (#159).
fn apart_from_ampersands(
    v: &str,
    quote: impl Fn(&str) -> String,
    amp: &str,
    concat: &str,
) -> String {
    if !v.contains('&') {
        return quote(v);
    }
    let mut parts = Vec::new();
    for (i, piece) in v.split('&').enumerate() {
        if i > 0 {
            parts.push(amp.to_owned());
        }
        if !piece.is_empty() {
            parts.push(quote(piece));
        }
    }
    parts.join(concat)
}

/// Renders a value as a SQL literal for `col` in `dialect`'s SQL.
pub fn literal(dialect: Dialect, col: &Column, value: Option<&str>) -> String {
    match dialect.kind() {
        DbKind::Oracle => sql_literal(col, value),
        DbKind::MySql => mysql_literal(col, value),
        DbKind::Postgres => postgres_literal(col, value),
        DbKind::SqlServer => sqlserver_literal(col, value),
        DbKind::Sqlite => sqlite_literal(col, value),
    }
}

/// The hex digits of a binary value typed as the grid shows it, `0x…`
/// (#256); an odd number of them with a 0 in front, as MySQL reads `0x…`.
fn typed_hex(v: &str) -> Option<String> {
    let digits = v.strip_prefix("0x")?;
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let pad = if digits.len() % 2 == 1 { "0" } else { "" };
    Some(format!("{pad}{digits}"))
}

/// MySQL converts text to dates and times itself; binary values (also
/// GEOMETRY) are shown as hex, or typed as `0x…`, and written as `X'…'`.
/// A BIT, shown as hex too, is written as the number it stands for (#253):
/// `X'1'` is a syntax error, and MariaDB compares a BIT with `X'…'` as a
/// decimal. Text with a backslash goes as hex too: a quoted `\` means one
/// thing with `NO_BACKSLASH_ESCAPES` and another without, `_utf8mb4 X'…'`
/// the same either way. So does text with a `&`, which Pythia's SQL
/// windows read as a substitution variable even in a literal (#159).
fn mysql_literal(col: &Column, value: Option<&str>) -> String {
    let Some(v) = value else {
        return "NULL".into();
    };
    if is_number(col, v) {
        return v.to_owned();
    }
    let binary = matches!(
        col.type_name.as_str(),
        "BINARY" | "VARBINARY" | "BLOB" | "BIT" | "GEOMETRY" | "VECTOR"
    );
    let typed = typed_hex(v).filter(|_| binary);
    let v = typed.as_deref().unwrap_or(v);
    let hex = v.chars().all(|c| c.is_ascii_hexdigit());
    if col.type_name == "BIT"
        && hex
        && let Ok(n) = u64::from_str_radix(v, 16)
    {
        return n.to_string();
    }
    if binary && hex {
        format!("X'{v}'")
    } else if v.contains(['\\', '\0', '&']) {
        let hex: String = v.bytes().map(|b| format!("{b:02X}")).collect();
        format!("_utf8mb4 X'{hex}'")
    } else {
        format!("'{}'", v.replace('\'', "''"))
    }
}

/// PostgreSQL takes back the text it shows (`\x00ff` bytea, `t`, dates) as
/// a quoted literal; a bytea typed as `0x…` goes as `\x…`. With a
/// backslash, `E'…'` says how to read it whatever
/// `standard_conforming_strings` is; a `&` goes in as `\x26`, as Pythia's
/// SQL windows read `&name` as a substitution variable even in a literal
/// (#159).
fn postgres_literal(col: &Column, value: Option<&str>) -> String {
    let Some(v) = value else {
        return "NULL".into();
    };
    if is_number(col, v) {
        return v.to_owned();
    }
    let typed = typed_hex(v)
        .filter(|_| col.type_name == "bytea")
        .map(|hex| format!("\\x{hex}"));
    let v = typed.as_deref().unwrap_or(v);
    let quoted = v.replace('\'', "''");
    if v.contains(['\\', '&']) {
        format!("E'{}'", quoted.replace('\\', "\\\\").replace('&', "\\x26"))
    } else {
        format!("'{quoted}'")
    }
}

/// SQL Server: Unicode text as `N'…'`, binary (also CLR types such as
/// `geometry`) as the `0x…` it is shown as. `datetime` text with a `T` (ISO
/// 8601) is read the same under every `DATEFORMAT`; with a space, `dmy`
/// swaps month and day.
///
/// ASCII text for a `varchar` or `char` column goes without the `N`: it
/// means the same in every code page, and compared with `N'…'` SQL Server
/// converts the column, which under a SQL collation scans its index
/// instead of seeking. A number past the 38 digits of a decimal literal (a
/// `float` like 1e300, shown in full) is written with an exponent.
fn sqlserver_literal(col: &Column, value: Option<&str>) -> String {
    let Some(v) = value else {
        return "NULL".into();
    };
    if is_number(col, v) {
        if v.chars().filter(char::is_ascii_digit).count() > 38
            && let Ok(n) = v.parse::<f64>()
        {
            return format!("{n:e}");
        }
        return v.to_owned();
    }
    let hex = v
        .strip_prefix("0x")
        .is_some_and(|h| h.chars().all(|c| c.is_ascii_hexdigit()));
    match col.type_name.as_str() {
        "binary" | "varbinary" | "image" | "udt" if hex => v.to_owned(),
        "datetime" | "smalldatetime" => format!("'{}'", v.replacen(' ', "T", 1)),
        "varchar" | "char" if v.is_ascii() => apart_from_ampersands(
            v,
            |p| format!("'{}'", p.replace('\'', "''")),
            "CHAR(38)",
            " + ",
        ),
        _ => apart_from_ampersands(
            v,
            |p| format!("N'{}'", p.replace('\'', "''")),
            "NCHAR(38)",
            " + ",
        ),
    }
}

/// Whether SQLite would write a number as `text`: `7`, `-12`, `1.5`, not
/// `007` or `1.50`. Such a number goes into SQLite as one: a column without
/// a type keeps what it is given, and one with a type converts it as it
/// needs.
pub fn sqlite_number(text: &str) -> bool {
    text.parse::<i64>().is_ok_and(|i| i.to_string() == text)
        || text
            .parse::<f64>()
            .is_ok_and(|f| f.is_finite() && format!("{f:?}") == text)
}

/// An amount as a plain decimal, written with a currency and digit groups
/// as PostgreSQL's `money` is, by the server's `lc_monetary`: `$1,234.56`
/// is `1234.56`, `-1.234,56 €` is `-1234.56`, `($5.00)` is `-5.00`,
/// `CHF 1'234.50` is `1234.50`. Of `.` and `,`, the last is the decimal
/// point, unless it is the only one and three digits follow it (a group:
/// `¥1,234`). `None` without digits.
pub fn amount(text: &str) -> Option<String> {
    let text = text.trim();
    let negative = text.contains('-') || text.starts_with('(') && text.ends_with(')');
    let kept: Vec<char> = text
        .chars()
        .filter(|c| c.is_ascii_digit() || matches!(c, '.' | ','))
        .collect();
    if !kept.iter().any(char::is_ascii_digit) {
        return None;
    }
    let point = kept
        .iter()
        .rposition(|c| matches!(c, '.' | ','))
        .filter(|&at| {
            let both = kept.contains(&'.') && kept.contains(&',');
            let alone = kept.iter().filter(|&&c| c == kept[at]).count() == 1;
            both || alone && kept.len() - at - 1 != 3
        });
    let mut out = String::from(if negative { "-" } else { "" });
    for (i, c) in kept.iter().enumerate() {
        if c.is_ascii_digit() {
            out.push(*c);
        } else if Some(i) == point {
            out.push('.');
        }
    }
    Some(out)
}

/// Compares two numbers written as plain decimals (`-12.50`, `.5`, `007`)
/// exactly, digit by digit, whatever their size; `None` if one isn't such
/// a number (`1e300`, `NaN`, text). A double holds 15 to 17 of their
/// digits (#238).
pub fn compare_decimals(a: &str, b: &str) -> Option<Ordering> {
    let (a, b) = (PlainDecimal::parse(a)?, PlainDecimal::parse(b)?);
    Some(match (a.negative, b.negative) {
        (false, false) => a.magnitude(&b),
        (true, true) => b.magnitude(&a),
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
    })
}

/// A plain decimal's sign and digits, without the zeros before its first
/// and after its last significant digit; zero isn't negative.
struct PlainDecimal<'a> {
    negative: bool,
    whole: &'a str,
    fraction: &'a str,
}

impl<'a> PlainDecimal<'a> {
    fn parse(text: &'a str) -> Option<Self> {
        let (negative, digits) = match text.strip_prefix('-') {
            Some(digits) => (true, digits),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        let decimal = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        if (whole.is_empty() && fraction.is_empty()) || !decimal(whole) || !decimal(fraction) {
            return None;
        }
        let whole = whole.trim_start_matches('0');
        let fraction = fraction.trim_end_matches('0');
        Some(Self {
            negative: negative && !(whole.is_empty() && fraction.is_empty()),
            whole,
            fraction,
        })
    }

    /// Compares the absolute values.
    fn magnitude(&self, other: &Self) -> Ordering {
        self.whole
            .len()
            .cmp(&other.whole.len())
            .then_with(|| self.whole.cmp(other.whole))
            .then_with(|| self.fraction.cmp(other.fraction))
    }
}

/// SQLite: a blob as the `X'…'` of the hex it is shown as, numbers as
/// SQLite writes them unquoted (see [`sqlite_number`]), text quoted (no
/// backslash escapes), a `&` apart as `char(38)` since Pythia's SQL windows
/// read `&name` as a substitution variable even in a literal (#159). The
/// session types a column of blobs BLOB (#209); there hex is a blob, also
/// when it is all digits (`x'313233'`), and so is `0x…` typed.
fn sqlite_literal(col: &Column, value: Option<&str>) -> String {
    let Some(v) = value else {
        return "NULL".into();
    };
    let blob = col.type_name.contains("BLOB");
    if let Some(hex) = typed_hex(v).filter(|_| blob) {
        return format!("X'{hex}'");
    }
    // An empty blob is shown empty too.
    let hex = v.len() % 2 == 0 && v.chars().all(|c| c.is_ascii_hexdigit());
    if hex && blob {
        return format!("X'{v}'");
    }
    if is_number(col, v) || sqlite_number(v) {
        return v.to_owned();
    }
    let text = |piece: &str| {
        apart_from_ampersands(
            piece,
            |p| format!("'{}'", p.replace('\'', "''")),
            "char(38)",
            " || ",
        )
    };
    // SQLite reads a statement up to a NUL, so one in a string ended it
    // there (#425): it is char(0) between the pieces.
    if !v.contains('\0') {
        return text(v);
    }
    let mut parts = Vec::new();
    for (i, piece) in v.split('\0').enumerate() {
        if i > 0 {
            parts.push("char(0)".to_owned());
        }
        if !piece.is_empty() {
            parts.push(text(piece));
        }
    }
    parts.join(" || ")
}

/// The zone INSERT statements of `columns` set while they run: in MySQL's
/// SQL with a TIMESTAMP column, the one its values were shown in (#213). A
/// TIMESTAMP is an instant shown in the session's zone; written as shown,
/// it would move in a session of another zone.
fn insert_time_zone<'a>(columns: &[Column], options: &'a Options) -> Option<&'a str> {
    let zoned = options.dialect.kind() == DbKind::MySql
        && columns.iter().any(|c| c.type_name == "TIMESTAMP");
    options.time_zone.as_deref().filter(|_| zoned)
}

fn set_time_zone(zone: &str) -> String {
    format!(
        "set @old_time_zone = @@time_zone, time_zone = '{}';",
        zone.replace('\'', "''")
    )
}

const RESTORE_TIME_ZONE: &str = "set time_zone = @old_time_zone;";

fn inserts(
    columns: &[Column],
    rows: &[Vec<Cell>],
    options: &Options,
    out: &mut dyn Write,
) -> Result<()> {
    let dialect = options.dialect;
    let names: Vec<String> = columns
        .iter()
        .map(|c| match dialect.kind() {
            DbKind::Oracle => identifier(&c.name),
            _ => dialect.quote_ident(&c.name),
        })
        .collect();
    let target = if options.table.trim().is_empty() {
        "my_table"
    } else {
        options.table.trim()
    };
    let head = format!("insert into {target} ({}) values (", names.join(", "));
    for row in rows {
        if dialect.kind() == DbKind::Oracle
            && let Some(block) = oracle_block(&head, columns, row)
        {
            write!(out, "{block}")?;
            continue;
        }
        let values: Vec<String> = columns
            .iter()
            .zip(row)
            .map(|(c, v)| literal(dialect, c, v.as_deref()))
            .collect();
        writeln!(out, "{head}{});", values.join(", "))?;
    }
    Ok(())
}

/// The most bytes one `hextoraw('…')` takes: a literal holds 4000
/// characters (ORA-01704).
const HEX_LITERAL_BYTES: usize = 2000;

/// The bytes of each part an INSERT's PL/SQL block appends: the block's
/// compile time grows faster than the length of its literals.
const BLOCK_CHUNK_BYTES: usize = 1000;

/// The hex digits of a BLOB's value too long for one `hextoraw` (#314), as
/// shown or typed as `0x…`; a LONG RAW's too, as the pure-Rust driver
/// types BLOB columns so. A RAW holds 2000 bytes, unless MAX_STRING_SIZE
/// is EXTENDED, where a literal takes 32767. Its hex is never longer than
/// the value, so a shorter one isn't looked at, and the value's own digits
/// aren't copied (#343).
fn long_binary<'a>(col: &Column, value: &'a str) -> Option<Cow<'a, str>> {
    if !matches!(col.type_name.as_str(), "BLOB" | "LONG RAW")
        || value.len() <= 2 * HEX_LITERAL_BYTES
    {
        return None;
    }
    // Typed as `0x…`, an odd number of digits with a 0 in front, as
    // `typed_hex` reads them.
    let hex = match value.strip_prefix("0x") {
        Some(digits) if digits.len() % 2 == 1 => Cow::Owned(format!("0{digits}")),
        Some(digits) => Cow::Borrowed(digits),
        None => Cow::Borrowed(value),
    };
    let digits = hex.len().is_multiple_of(2) && hex.bytes().all(|b| b.is_ascii_hexdigit());
    (digits && hex.len() > 2 * HEX_LITERAL_BYTES).then_some(hex)
}

/// An Oracle INSERT of a row with a binary value or a CLOB's text too
/// long for a literal (#314, #317), as a PL/SQL block that makes each such
/// value in a temporary LOB from parts and inserts the row with it. No
/// SQL makes a BLOB over 2000 bytes from literals: `to_blob(…) ||
/// to_blob(…)` is an empty one, `utl_raw.concat` stops at 2000 bytes; a
/// CLOB's parts compile here in a fraction of the time `||` takes, and
/// as many as there are. `None` for a row without such a value, which is
/// left to its INSERT before any literal is made (#343).
fn oracle_block(head: &str, columns: &[Column], row: &[Cell]) -> Option<String> {
    let long = |(col, cell): (&Column, &Cell)| {
        cell.as_deref()
            .is_some_and(|v| long_binary(col, v).is_some() || long_text(col, v))
    };
    if !columns.iter().zip(row).any(long) {
        return None;
    }
    let mut declare = String::new();
    let mut body = String::new();
    let mut values = Vec::with_capacity(columns.len());
    for (col, cell) in columns.iter().zip(row) {
        let var = format!("pythia${}", values.len() + 1);
        let value = cell.as_deref();
        if let Some(hex) = value.and_then(|v| long_binary(col, v)) {
            declare.push_str(&format!("  {var} blob;\n"));
            body.push_str(&format!("  dbms_lob.createtemporary({var}, true);\n"));
            // The hex goes to writeappend's RAW as hextoraw would take it,
            // which compiles in half the time; its digits are ASCII, so
            // each chunk is whole characters.
            for chunk in hex.as_bytes().chunks(2 * BLOCK_CHUNK_BYTES) {
                let digits = std::str::from_utf8(chunk).unwrap_or_default();
                body.push_str(&format!(
                    "  dbms_lob.writeappend({var}, {}, '{digits}');\n",
                    chunk.len() / 2
                ));
            }
        } else if let Some(text) = value.filter(|v| long_text(col, v)) {
            // An NCLOB takes the CLOB (#317), and so do a JSON and a
            // VECTOR (#342).
            declare.push_str(&format!("  {var} clob;\n"));
            body.push_str(&format!("  dbms_lob.createtemporary({var}, true);\n"));
            for part in text_parts(text) {
                body.push_str(&format!(
                    "  dbms_lob.writeappend({var}, {}, {});\n",
                    part.encode_utf16().count(),
                    text_literal(part)
                ));
            }
            // A JSON with its extended objects' scalars (#370).
            values.push(oracle_json(col, &var, text, None).unwrap_or(var));
            continue;
        } else {
            values.push(sql_literal(col, value));
            continue;
        }
        values.push(var);
    }
    Some(format!(
        "declare\n{declare}begin\n{body}  {head}{});\nend;\n/\n",
        values.join(", ")
    ))
}

/// Lowercase plain identifiers; quote anything that needs it (Oracle), a
/// reserved word too: `level` is no name, `"LEVEL"` is (#274).
pub fn identifier(name: &str) -> String {
    let quoted = Dialect::ORACLE.quote_ident(name);
    if quoted == name {
        name.to_lowercase()
    } else {
        quoted
    }
}

fn markdown(columns: &[Column], rows: &[Vec<Cell>], out: &mut dyn Write) -> Result<()> {
    markdown_header(columns, out)?;
    markdown_rows(rows, out)
}

fn markdown_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn markdown_header(columns: &[Column], out: &mut dyn Write) -> Result<()> {
    let header: Vec<String> = columns.iter().map(|c| markdown_escape(&c.name)).collect();
    writeln!(out, "| {} |", header.join(" | "))?;
    let rule: Vec<&str> = columns
        .iter()
        .map(|c| if c.numeric { "---:" } else { "---" })
        .collect();
    writeln!(out, "| {} |", rule.join(" | "))?;
    Ok(())
}

fn markdown_rows(rows: &[Vec<Cell>], out: &mut dyn Write) -> Result<()> {
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|c| markdown_escape(c.as_deref().unwrap_or("")))
            .collect();
        writeln!(out, "| {} |", cells.join(" | "))?;
    }
    Ok(())
}

fn html(columns: &[Column], rows: &[Vec<Cell>], out: &mut dyn Write) -> Result<()> {
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    writeln!(out, "<table>")?;
    writeln!(
        out,
        "  <thead><tr>{}</tr></thead>",
        columns
            .iter()
            .map(|c| format!("<th>{}</th>", escape(&c.name)))
            .collect::<String>()
    )?;
    writeln!(out, "  <tbody>")?;
    for row in rows {
        let cells: String = columns
            .iter()
            .zip(row)
            .map(|(c, v)| {
                let align = if c.numeric {
                    " style=\"text-align:right\""
                } else {
                    ""
                };
                format!("<td{align}>{}</td>", escape(v.as_deref().unwrap_or("")))
            })
            .collect();
        writeln!(out, "    <tr>{cells}</tr>")?;
    }
    writeln!(out, "  </tbody>\n</table>")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell as Counter;

    use super::*;

    /// PostgreSQL's money as `lc_monetary` writes it, as a plain decimal.
    #[test]
    fn amounts() {
        for (text, plain) in [
            ("$1,234.56", "1234.56"),
            ("-$5.00", "-5.00"),
            ("($5.00)", "-5.00"),
            ("-1.234.567,89 €", "-1234567.89"),
            ("CHF 1'234.50", "1234.50"),
            ("¥1,234", "1234"),
            ("1,234,567", "1234567"),
            ("12.5", "12.5"),
            ("$0.00", "0.00"),
        ] {
            assert_eq!(amount(text).as_deref(), Some(plain), "{text}");
        }
        assert_eq!(amount("n/a"), None);
    }

    /// The bytes each thread allocates, for the tests of what a write
    /// copies.
    struct Counting;

    thread_local! {
        static ALLOCATED: Counter<usize> = const { Counter::new(0) };
    }

    // SAFETY: System's allocator, with the bytes counted.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = ALLOCATED.try_with(|a| a.set(a.get() + layout.size()));
            // SAFETY: as the caller's.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: as the caller's.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let _ = ALLOCATED.try_with(|a| a.set(a.get() + size));
            // SAFETY: as the caller's.
            unsafe { System.realloc(ptr, layout, size) }
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;

    /// The bytes `f` allocates on this thread.
    fn allocated(f: impl FnOnce()) -> usize {
        let before = ALLOCATED.with(Counter::get);
        f();
        ALLOCATED.with(Counter::get) - before
    }

    fn sample() -> (Vec<Column>, Vec<Vec<Cell>>) {
        let col = |name: &str, ty: &str, numeric| Column {
            name: name.into(),
            type_name: ty.into(),
            numeric,
        };
        (
            vec![
                col("ID", "NUMBER(10)", true),
                col("NAME", "VARCHAR2(30)", false),
                col("HIRED", "DATE", false),
                col("Mixed Case", "VARCHAR2(5)", false),
            ],
            vec![
                vec![
                    Some("1".into()),
                    Some("O'Brien, \"Pat\"".into()),
                    Some("2020-01-02 03:04:05".into()),
                    None,
                ],
                vec![Some("2.5".into()), None, None, Some("a|b".into())],
            ],
        )
    }

    fn text(format: Format) -> String {
        let (c, r) = sample();
        String::from_utf8(
            export_to_vec(
                format,
                &c,
                &r,
                &Options {
                    table: "emp".into(),
                    ..Options::default()
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn csv_quotes() {
        assert_eq!(
            text(Format::Csv),
            "ID,NAME,HIRED,Mixed Case\n1,\"O'Brien, \"\"Pat\"\"\",2020-01-02 03:04:05,\n2.5,,,a|b\n"
        );
    }

    #[test]
    fn insert_literals() {
        assert_eq!(
            text(Format::Insert),
            "insert into emp (id, name, hired, \"Mixed Case\") values (1, 'O''Brien, \"Pat\"', \
             to_date('2020-01-02 03:04:05', 'YYYY-MM-DD HH24:MI:SS'), NULL);\n\
             insert into emp (id, name, hired, \"Mixed Case\") values (2.5, NULL, NULL, 'a|b');\n"
        );
    }

    #[test]
    fn time_zone_literals() {
        let tz = Column {
            name: "AT".into(),
            type_name: "TIMESTAMP(6) WITH TIME ZONE".into(),
            numeric: false,
        };
        assert_eq!(
            sql_literal(&tz, Some("2026-07-01 12:00:00.000000 Europe/Zurich")),
            "to_timestamp_tz('2026-07-01 12:00:00.000000 Europe/Zurich', \
             'YYYY-MM-DD HH24:MI:SS.FF TZR')"
        );
        assert_eq!(
            sql_literal(&tz, Some("2026-07-01 12:00:00 +02:00")),
            "to_timestamp_tz('2026-07-01 12:00:00 +02:00', 'YYYY-MM-DD HH24:MI:SS TZR')"
        );
    }

    #[test]
    fn mysql_literals() {
        let col = |t: &str, numeric| Column {
            name: "C".into(),
            type_name: t.into(),
            numeric,
        };
        let my = |c: &Column, v| literal(Dialect::MYSQL, c, v);
        assert_eq!(my(&col("INT", true), Some("-12")), "-12");
        assert_eq!(my(&col("DOUBLE", true), Some("inf")), "'inf'");
        assert_eq!(my(&col("VARCHAR", false), Some("O'Brien")), "'O''Brien'");
        assert_eq!(my(&col("VARCHAR", false), None), "NULL");
        assert_eq!(
            my(&col("TEXT", false), Some(r"C:\new")),
            "_utf8mb4 X'433A5C6E6577'"
        );
        assert_eq!(
            my(&col("DATETIME", false), Some("2026-01-02 03:04:05.5")),
            "'2026-01-02 03:04:05.5'"
        );
        assert_eq!(my(&col("DATE", false), Some("2026-01-02")), "'2026-01-02'");
        assert_eq!(my(&col("VARBINARY", false), Some("00ff")), "X'00ff'");
        assert_eq!(my(&col("BLOB", false), Some("")), "X''");
        assert_eq!(my(&col("BLOB", false), Some("no hex")), "'no hex'");
        // A BIT is shown as hex and written as its number (#253): X'1' is
        // a syntax error, and MariaDB compares X'…' with a BIT as a decimal.
        let bit = col("BIT", false);
        assert_eq!(my(&bit, Some("05")), "5");
        assert_eq!(my(&bit, Some("1")), "1");
        assert_eq!(my(&bit, Some("00")), "0");
        assert_eq!(my(&bit, Some("81")), "129");
        assert_eq!(my(&bit, Some("FFFFFFFFFFFFFFFF")), "18446744073709551615");
        assert_eq!(my(&bit, Some("2x")), "'2x'");
        // Oracle's stay as they were.
        assert_eq!(
            literal(
                Dialect::ORACLE,
                &col("DATE", false),
                Some("2026-01-02 00:00:00")
            ),
            "to_date('2026-01-02 00:00:00', 'YYYY-MM-DD HH24:MI:SS')"
        );
    }

    /// The grid shows binary values as `0x…`, and a value typed so is its
    /// bytes on every database (#256), as on SQL Server; a text column
    /// keeps it as text. An odd number of digits reads as MySQL's `0x…`
    /// does, with a 0 in front.
    #[test]
    fn binary_values_typed_as_shown() {
        let col = |t: &str| Column {
            name: "C".into(),
            type_name: t.into(),
            numeric: false,
        };
        let typed = |d: Dialect, t: &str, v: &str| literal(d, &col(t), Some(v));
        let my = |t, v| typed(Dialect::MYSQL, t, v);
        assert_eq!(my("VARBINARY", "0x171066e1"), "X'171066e1'");
        assert_eq!(my("BLOB", "0xABC"), "X'0ABC'");
        assert_eq!(my("BINARY", "0x"), "X''");
        assert_eq!(my("BIT", "0x81"), "129");
        assert_eq!(my("VARCHAR", "0x41"), "'0x41'");
        assert_eq!(my("VARBINARY", "0x4G"), "'0x4G'");
        let pg = |t, v| typed(Dialect::POSTGRES, t, v);
        assert_eq!(pg("bytea", "0x171066e1"), r"E'\\x171066e1'");
        assert_eq!(pg("bytea", "0xabc"), r"E'\\x0abc'");
        assert_eq!(pg("text", "0x41"), "'0x41'");
        let lite = |t, v| typed(Dialect::SQLITE, t, v);
        assert_eq!(lite("BLOB", "0x171066E1"), "X'171066E1'");
        assert_eq!(lite("BLOB", "0xABC"), "X'0ABC'");
        assert_eq!(lite("TEXT", "0x41"), "'0x41'");
        assert_eq!(
            typed(Dialect::SQLSERVER, "varbinary", "0x171066E1"),
            "0x171066E1"
        );
        let ora = |t, v| typed(Dialect::ORACLE, t, v);
        assert_eq!(ora("RAW(16)", "0x171066E1"), "hextoraw('171066E1')");
        assert_eq!(ora("BLOB", "0xABC"), "hextoraw('0ABC')");
        assert_eq!(ora("LONG RAW", "0x00ff"), "hextoraw('00ff')");
        assert_eq!(ora("RAW(16)", "171066E1"), "hextoraw('171066E1')");
        assert_eq!(ora("VARCHAR2(10)", "0x41"), "'0x41'");
    }

    /// Oracle's reserved words, also those the keyword list lacks, are
    /// quoted where INSERTs name a table or a column; so are names stored
    /// in lowercase or mixed case.
    #[test]
    fn oracle_inserts_quote_reserved_words() {
        let col = |name: &str| Column {
            name: name.into(),
            type_name: "NUMBER".into(),
            numeric: true,
        };
        let columns = [
            col("ID"),
            col("LEVEL"),
            col("SIZE"),
            col("RESOURCE"),
            col("level"),
            col("Size"),
        ];
        let rows = [(1..=6).map(|n| Some(n.to_string())).collect::<Vec<_>>()];
        let options = Options {
            table: identifier("SIZE"),
            ..Options::default()
        };
        assert_eq!(
            String::from_utf8(export_to_vec(Format::Insert, &columns, &rows, &options).unwrap())
                .unwrap(),
            "insert into \"SIZE\" (id, \"LEVEL\", \"SIZE\", \"RESOURCE\", \"level\", \"Size\") \
             values (1, 2, 3, 4, 5, 6);\n"
        );
        assert_eq!(identifier("DEMO_EMPLOYEES"), "demo_employees");
        assert_eq!(identifier("Mixed Case"), "\"Mixed Case\"");
    }

    /// No `&` in a literal: where scripts run it would be a substitution
    /// variable (#159).
    #[test]
    fn ampersands_outside_literals() {
        let col = |t: &str| Column {
            name: "C".into(),
            type_name: t.into(),
            numeric: false,
        };
        let ora = |v| literal(Dialect::ORACLE, &col("VARCHAR2"), Some(v));
        assert_eq!(ora("Missing (H&L)"), "'Missing (H' || chr(38) || 'L)'");
        assert_eq!(ora("&"), "chr(38)");
        assert_eq!(ora("a&&b"), "'a' || chr(38) || chr(38) || 'b'");
        assert_eq!(ora("&x'"), "chr(38) || 'x'''");
        assert_eq!(
            literal(Dialect::MYSQL, &col("VARCHAR"), Some("H&L")),
            "_utf8mb4 X'48264C'"
        );
        assert_eq!(
            literal(Dialect::POSTGRES, &col("text"), Some(r"H&L\x")),
            r"E'H\x26L\\x'"
        );
        assert_eq!(
            literal(Dialect::SQLSERVER, &col("nvarchar"), Some("H&L")),
            "N'H' + NCHAR(38) + N'L'"
        );
        assert_eq!(
            literal(Dialect::SQLITE, &col("TEXT"), Some("H&L")),
            "'H' || char(38) || 'L'"
        );
    }

    #[test]
    fn literals_of_each_database() {
        let col = |t: &str, numeric| Column {
            name: "C".into(),
            type_name: t.into(),
            numeric,
        };
        let pg = |c: &Column, v| literal(Dialect::POSTGRES, c, v);
        assert_eq!(pg(&col("integer", true), Some("7")), "7");
        assert_eq!(pg(&col("double precision", true), Some("NaN")), "'NaN'");
        assert_eq!(pg(&col("bytea", false), Some(r"\x00ff")), r"E'\\x00ff'");
        assert_eq!(pg(&col("text", false), Some("it's")), "'it''s'");
        assert_eq!(
            pg(
                &col("timestamp with time zone", false),
                Some("2026-01-02 03:04:05+01")
            ),
            "'2026-01-02 03:04:05+01'"
        );
        let lite = |c: &Column, v| literal(Dialect::SQLITE, c, v);
        assert_eq!(lite(&col("INTEGER", true), Some("7")), "7");
        assert_eq!(lite(&col("BLOB", false), Some("00FF")), "X'00FF'");
        assert_eq!(lite(&col("TEXT", false), Some("00FF")), "'00FF'");
        // A blob's hex may be all digits: x'313233' and x'10' are no numbers.
        assert_eq!(lite(&col("BLOB", false), Some("313233")), "X'313233'");
        assert_eq!(lite(&col("BLOB", false), Some("10")), "X'10'");
        assert_eq!(lite(&col("BLOB", false), Some("hello")), "'hello'");
        // Numbers as SQLite writes them go in as numbers, others as text.
        assert_eq!(lite(&col("", false), Some("7")), "7");
        assert_eq!(lite(&col("TEXT", false), Some("007")), "'007'");
        assert_eq!(lite(&col("TEXT", false), Some("1.50")), "'1.50'");
        assert_eq!(lite(&col("TEXT", false), Some(r"it's \n")), r"'it''s \n'");
        assert_eq!(lite(&col("DATE", false), None), "NULL");
        let ms = |c: &Column, v| literal(Dialect::SQLSERVER, c, v);
        assert_eq!(
            ms(&col("nvarchar", false), Some("grüße 'x'")),
            "N'grüße ''x'''"
        );
        assert_eq!(ms(&col("varbinary", false), Some("0x00FF")), "0x00FF");
        assert_eq!(
            ms(&col("varbinary", false), Some("0x; drop")),
            "N'0x; drop'"
        );
        assert_eq!(
            ms(&col("datetime", false), Some("2026-01-02 03:04:05.123")),
            "'2026-01-02T03:04:05.123'"
        );
        assert_eq!(ms(&col("date", false), Some("2026-01-02")), "N'2026-01-02'");
        // ASCII needs no N for varchar, which keeps an index on it usable.
        assert_eq!(ms(&col("varchar", false), Some("it's")), "'it''s'");
        assert_eq!(ms(&col("char", false), Some("H&L")), "'H' + CHAR(38) + 'L'");
        assert_eq!(ms(&col("varchar", false), Some("grüße")), "N'grüße'");
        assert_eq!(ms(&col("udt", false), Some("0x0101")), "0x0101");
        // Past a decimal literal's 38 digits, a float takes an exponent.
        let big = format!("1{}", "0".repeat(300));
        assert_eq!(ms(&col("float", true), Some(&big)), "1e300");
        let tiny = format!("0.{}25", "0".repeat(40));
        assert_eq!(ms(&col("float", true), Some(&tiny)), "2.5e-41");
        assert_eq!(ms(&col("float", true), Some("0.1")), "0.1");
        // Oracle too: a name isn't a number.
        assert_eq!(sql_literal(&col("NUMBER", true), Some("Inf")), "'Inf'");
    }

    /// BINARY_DOUBLE and BINARY_FLOAT values are literals of their type
    /// (#339): as NUMBERs, 1e300 overflowed (ORA-01426), 2e-300 was 0, and
    /// 'inf' and 'NaN' compared as text with Inf and Nan.
    #[test]
    fn oracle_binary_floats_typed() {
        let col = |t: &str| Column {
            name: "C".into(),
            type_name: t.into(),
            numeric: true,
        };
        let double = |v| sql_literal(&col("BINARY_DOUBLE"), Some(v));
        let float = |v| sql_literal(&col("BINARY_FLOAT"), Some(v));
        assert_eq!(double("1e300"), "1e300d");
        assert_eq!(double("-2e-300"), "-2e-300d");
        assert_eq!(double("0.1"), "0.1d");
        assert_eq!(double("100"), "100d");
        assert_eq!(double("inf"), "binary_double_infinity");
        assert_eq!(double("-inf"), "-binary_double_infinity");
        assert_eq!(double("NaN"), "binary_double_nan");
        assert_eq!(float("3.4028235e38"), "3.4028235e38f");
        assert_eq!(float("1e-40"), "1e-40f");
        assert_eq!(float("inf"), "binary_float_infinity");
        assert_eq!(float("-inf"), "-binary_float_infinity");
        assert_eq!(float("NaN"), "binary_float_nan");
        // Typed as Oracle writes them, or in a form it reads.
        assert_eq!(double("-Inf"), "-binary_double_infinity");
        assert_eq!(float("Nan"), "binary_float_nan");
        assert_eq!(double(".5E3"), ".5E3d");
        // Anything else as text, for Oracle to read or reject.
        assert_eq!(double(" 1.5"), "' 1.5'");
        assert_eq!(double("1,5"), "'1,5'");
        assert_eq!(sql_literal(&col("BINARY_DOUBLE"), None), "NULL");
        // In INSERT statements too.
        let columns = [col("BINARY_DOUBLE"), col("BINARY_FLOAT")];
        let rows = vec![vec![Some("1e300".into()), Some("NaN".into())]];
        let options = Options {
            table: "t".into(),
            ..Options::default()
        };
        let text =
            String::from_utf8(export_to_vec(Format::Insert, &columns, &rows, &options).unwrap())
                .unwrap();
        assert_eq!(
            text,
            "insert into t (c, c) values (1e300d, binary_float_nan);\n"
        );
    }

    #[test]
    fn inserts_in_the_connections_sql() {
        let col = |name: &str, ty: &str, numeric| Column {
            name: name.into(),
            type_name: ty.into(),
            numeric,
        };
        let columns = [
            col("id", "INT", true),
            col("order", "VARCHAR", false),
            col("born", "DATE", false),
        ];
        let rows = [vec![
            Some("1".into()),
            Some(r"C:\x".into()),
            Some("2026-01-02".into()),
        ]];
        let insert = |dialect| {
            let options = Options {
                table: "t".into(),
                dialect,
                ..Options::default()
            };
            String::from_utf8(export_to_vec(Format::Insert, &columns, &rows, &options).unwrap())
                .unwrap()
        };
        assert_eq!(
            insert(Dialect::MYSQL),
            "insert into t (id, `order`, born) values (1, _utf8mb4 X'433A5C78', '2026-01-02');\n"
        );
        assert_eq!(
            insert(Dialect::SQLSERVER),
            "insert into t (id, [order], born) values (1, N'C:\\x', N'2026-01-02');\n"
        );
    }

    /// MySQL's INSERTs of TIMESTAMP values set the zone they were shown in
    /// while they run, streamed too (#213); without a TIMESTAMP, or in
    /// another database's SQL, they don't.
    #[test]
    fn mysql_timestamps_in_their_zone() {
        let col = |name: &str, ty: &str| Column {
            name: name.into(),
            type_name: ty.into(),
            numeric: false,
        };
        let columns = vec![col("ts", "TIMESTAMP"), col("dt", "DATETIME")];
        let rows = vec![vec![
            Some("2021-10-07 09:00:00".to_owned()),
            Some("2021-10-07 09:00:00".to_owned()),
        ]];
        let options = |dialect, zone: Option<&str>| Options {
            table: "t".into(),
            dialect,
            time_zone: zone.map(Into::into),
            ..Options::default()
        };
        let insert =
            "insert into t (ts, dt) values ('2021-10-07 09:00:00', '2021-10-07 09:00:00');";
        let zoned = format!(
            "set @old_time_zone = @@time_zone, time_zone = '+11:00';\n{insert}\n\
             set time_zone = @old_time_zone;\n"
        );
        let mysql = options(Dialect::MYSQL, Some("+11:00"));
        let text = |columns: &[Column], options: &Options| {
            String::from_utf8(export_to_vec(Format::Insert, columns, &rows, options).unwrap())
                .unwrap()
        };
        assert_eq!(text(&columns, &mysql), zoned);
        let mut stream =
            StreamWriter::new(Format::Insert, columns.clone(), mysql.clone(), Vec::new()).unwrap();
        stream.write(&rows).unwrap();
        assert_eq!(String::from_utf8(stream.finish().unwrap()).unwrap(), zoned);
        // A DATETIME has no zone; no zone known, none set.
        let datetimes = [col("ts", "DATETIME"), col("dt", "DATETIME")];
        assert_eq!(text(&datetimes, &mysql), format!("{insert}\n"));
        assert_eq!(
            text(&columns, &options(Dialect::MYSQL, None)),
            format!("{insert}\n")
        );
        assert!(!text(&columns, &options(Dialect::POSTGRES, Some("+11:00"))).contains("set"));
    }

    /// A binary value fetched cut short for the grid isn't written as it
    /// is, in any format: the export fails and says what writes it whole.
    #[test]
    fn cut_values_are_not_exported() {
        let columns = [
            Column {
                name: "ID".into(),
                type_name: "INT".into(),
                numeric: true,
            },
            Column {
                name: "DATA".into(),
                type_name: "BLOB".into(),
                numeric: false,
            },
        ];
        let cut = format!("{}… (10485760 bytes)", "89".repeat(4000));
        let rows = |value: &str| vec![vec![Some("1".to_owned()), Some(value.to_owned())]];
        for format in Format::ALL {
            for value in [cut.clone(), format!("0x{cut}")] {
                let error = export_to_vec(*format, &columns, &rows(&value), &Options::default())
                    .expect_err("a cut value");
                let text = error.to_string();
                assert!(
                    text.contains("DATA") && text.contains("Execute to file"),
                    "{format:?}: {text}"
                );
            }
        }
        let mut stream = StreamWriter::new(
            Format::Csv,
            columns.to_vec(),
            Options::default(),
            Vec::new(),
        )
        .unwrap();
        assert!(stream.write(&rows(&cut)).is_err());
        // Whole values, and text that only looks like the end of one.
        let whole = "89".repeat(5000);
        for value in [whole.as_str(), "it ends… (12 bytes)"] {
            let csv = export_to_vec(Format::Csv, &columns, &rows(value), &Options::default());
            assert!(csv.is_ok(), "{value}");
        }
    }

    /// Plain decimals compare digit by digit, past what a double holds
    /// (#238).
    #[test]
    fn decimals_compare_exactly() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        for (a, b, order) in [
            ("9223372036854775807", "9223372036854775806", Some(Greater)),
            (
                "12345678901234567890.25",
                "12345678901234567890.5",
                Some(Less),
            ),
            ("007", "7", Some(Equal)),
            (".5", "0.50", Some(Equal)),
            ("-0", "0", Some(Equal)),
            ("-1.5", "-1.25", Some(Less)),
            ("-2", "1", Some(Less)),
            ("+3", "2.999", Some(Greater)),
            ("1e3", "1000", None),
            ("NaN", "1", None),
            ("-", "1", None),
            (".", "1", None),
        ] {
            assert_eq!(compare_decimals(a, b), order, "{a} {b}");
        }
    }

    #[test]
    fn json_types() {
        let v: serde_json::Value = serde_json::from_str(&text(Format::Json)).unwrap();
        assert_eq!(v[0]["ID"], 1);
        assert_eq!(v[1]["ID"], 2.5);
        assert_eq!(v[1]["NAME"], serde_json::Value::Null);
    }

    /// JSON writes a number with every digit the database shows, past an
    /// i64 and a double, streamed too; `.5` as `0.5`, as JSON has it.
    /// Excel keeps 15 significant digits: a number whose whole part has
    /// more is text, all of it; a longer fraction rounds.
    #[test]
    fn numbers_keep_every_digit() {
        use calamine::{Data, Reader};
        let columns = [Column {
            name: "N".into(),
            type_name: "NUMBER".into(),
            numeric: true,
        }];
        let cases = [
            ("9223372036854775807", "9223372036854775807", false),
            ("18446744073709551615", "18446744073709551615", false),
            ("12345678901234567890123", "12345678901234567890123", false),
            ("1234567890123456.5", "1234567890123456.5", false),
            ("1.2345678901234567E+20", "1.2345678901234567E+20", false),
            ("-0.123456789012345678", "-0.123456789012345678", true),
            ("0.33333333333333333333", "0.33333333333333333333", true),
            ("123456789012345.678", "123456789012345.678", true),
            ("123456789012345", "123456789012345", true),
            ("100000000000000000000", "100000000000000000000", true),
            ("0.00000000000000000002", "0.00000000000000000002", true),
            ("2.5", "2.5", true),
            (".5", "0.5", true),
            ("-.5", "-0.5", true),
            ("1.5E-7", "1.5E-7", true),
        ];
        let rows: Vec<Vec<Cell>> = cases
            .iter()
            .map(|(v, _, _)| vec![Some(v.to_string())])
            .collect();
        let options = Options::default();
        let json = export_to_vec(Format::Json, &columns, &rows, &options).unwrap();
        let mut stream =
            StreamWriter::new(Format::Json, columns.to_vec(), options.clone(), Vec::new()).unwrap();
        stream.write(&rows).unwrap();
        let streamed = stream.finish().unwrap();
        for bytes in [json, streamed] {
            let text = String::from_utf8(bytes).unwrap();
            assert!(
                serde_json::from_str::<serde_json::Value>(&text).is_ok(),
                "{text}"
            );
            let written: Vec<&str> = text
                .lines()
                .filter_map(|l| l.trim().strip_prefix("\"N\": "))
                .collect();
            let expected: Vec<&str> = cases.iter().map(|(_, j, _)| *j).collect();
            assert_eq!(written, expected);
        }
        let bytes = export_to_vec(Format::Xlsx, &columns, &rows, &options).unwrap();
        let mut book = calamine::open_workbook_auto_from_rs(std::io::Cursor::new(bytes)).unwrap();
        let sheet = book.worksheet_range("Result").unwrap();
        for (i, (value, _, number)) in cases.iter().enumerate() {
            let cell = sheet.get((i + 1, 0)).unwrap();
            if *number {
                assert_eq!(cell, &Data::Float(value.parse().unwrap()), "{value}");
            } else {
                assert_eq!(cell, &Data::String(value.to_string()), "{value}");
            }
        }
    }

    /// Booleans are JSON's true and false, not the text shown (#217): a
    /// JSON reader takes "f" (and "FALSE", "0") for true.
    #[test]
    fn json_booleans() {
        let col = |ty: &str| Column {
            name: "b".into(),
            type_name: ty.into(),
            numeric: false,
        };
        let json = |dialect, ty: &str, values: &[Option<&str>]| {
            let rows: Vec<Vec<Cell>> = values.iter().map(|v| vec![v.map(Into::into)]).collect();
            let options = Options {
                dialect,
                ..Options::default()
            };
            let bytes = export_to_vec(Format::Json, &[col(ty)], &rows, &options).unwrap();
            let mut stream =
                StreamWriter::new(Format::Json, vec![col(ty)], options, Vec::new()).unwrap();
            stream.write(&rows).unwrap();
            let streamed: serde_json::Value =
                serde_json::from_slice(&stream.finish().unwrap()).unwrap();
            let whole: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(whole, streamed);
            whole
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["b"].clone())
                .collect::<Vec<_>>()
        };
        use serde_json::Value::{Bool, Null, String as Text};
        let pg = json(Dialect::POSTGRES, "boolean", &[Some("t"), Some("f"), None]);
        assert_eq!(pg, [Bool(true), Bool(false), Null]);
        let ora = json(Dialect::ORACLE, "BOOLEAN", &[Some("TRUE"), Some("FALSE")]);
        assert_eq!(ora, [Bool(true), Bool(false)]);
        let ms = json(Dialect::SQLSERVER, "bit", &[Some("1"), Some("0")]);
        assert_eq!(ms, [Bool(true), Bool(false)]);
        // PostgreSQL's bit is a string of bits.
        let bits = json(Dialect::POSTGRES, "bit", &[Some("101"), Some("1")]);
        assert_eq!(bits, [Text("101".into()), Text("1".into())]);
    }

    /// A row's object has a key for each column, in the result's order,
    /// whole and streamed: a second `id` (`select a.id, b.id …`) is
    /// `id_2`, or the next number no column takes; `ID` is another key.
    #[test]
    fn json_keeps_every_column() {
        let col = |name: &str| Column {
            name: name.into(),
            type_name: "NUMBER".into(),
            numeric: true,
        };
        let columns: Vec<Column> = ["ZIP", "id", "NAME", "id", "id_2", "id", "ID"]
            .into_iter()
            .map(col)
            .collect();
        let rows: Vec<Vec<Cell>> = vec![(1..=7).map(|n| Some(n.to_string())).collect()];
        let options = Options::default();
        let whole = export_to_vec(Format::Json, &columns, &rows, &options).unwrap();
        let mut stream =
            StreamWriter::new(Format::Json, columns.clone(), options, Vec::new()).unwrap();
        stream.write(&rows).unwrap();
        let streamed = stream.finish().unwrap();
        for bytes in [whole, streamed] {
            let text = String::from_utf8(bytes).unwrap();
            let members: Vec<(&str, &str)> = text
                .lines()
                .filter_map(|l| l.trim().trim_end_matches(',').split_once(": "))
                .collect();
            let expected = [
                ("\"ZIP\"", "1"),
                ("\"id\"", "2"),
                ("\"NAME\"", "3"),
                ("\"id_3\"", "4"),
                ("\"id_2\"", "5"),
                ("\"id_4\"", "6"),
                ("\"ID\"", "7"),
            ];
            assert_eq!(members, expected, "{text}");
        }
    }

    /// An Oracle INSERT of a binary value over the 2000 bytes a `hextoraw`
    /// literal takes is a PL/SQL block that makes it in 1000-byte parts
    /// (#314); other rows, and a value of 2000 bytes, stay INSERTs.
    #[test]
    fn oracle_long_binary_values() {
        let col = |name: &str, ty: &str, numeric| Column {
            name: name.into(),
            type_name: ty.into(),
            numeric,
        };
        let columns = [
            col("ID", "NUMBER", true),
            col("B", "BLOB", false),
            col("L", "LONG RAW", false),
            col("R", "RAW(16)", false),
        ];
        let long = "AB".repeat(2001);
        let rows = vec![
            vec![
                Some("1".into()),
                Some(long.clone()),
                None,
                Some("CAFE".into()),
            ],
            vec![Some("2".into()), Some("CD".repeat(2000)), None, None],
            vec![
                Some("3".into()),
                Some(format!("0x{long}")),
                Some(long.clone()),
                None,
            ],
        ];
        let options = Options {
            table: "t".into(),
            ..Options::default()
        };
        let text =
            String::from_utf8(export_to_vec(Format::Insert, &columns, &rows, &options).unwrap())
                .unwrap();
        let part = |var: &str, bytes: usize| {
            format!(
                "  dbms_lob.writeappend({var}, {bytes}, '{}');\n",
                "AB".repeat(bytes)
            )
        };
        let parts = |var: &str| {
            format!(
                "  dbms_lob.createtemporary({var}, true);\n{}{}{}",
                part(var, 1000),
                part(var, 1000),
                part(var, 1)
            )
        };
        let expected = format!(
            "declare\n  pythia$2 blob;\nbegin\n{}  insert into t (id, b, l, r) values \
             (1, pythia$2, NULL, hextoraw('CAFE'));\nend;\n/\n\
             insert into t (id, b, l, r) values (2, hextoraw('{}'), NULL, NULL);\n\
             declare\n  pythia$2 blob;\n  pythia$3 blob;\nbegin\n{}{}  insert into t \
             (id, b, l, r) values (3, pythia$2, pythia$3, NULL);\nend;\n/\n",
            parts("pythia$2"),
            "CD".repeat(2000),
            parts("pythia$2"),
            parts("pythia$3"),
        );
        assert_eq!(text, expected);
    }

    /// An Oracle INSERT export looks at a row's values without copying
    /// them, and leaves a row without a long one to its INSERT before
    /// making any literal (#343): `oracle_block` copied each BLOB's hex to
    /// see whether it was long, and made every value's literal, which the
    /// INSERT then made again.
    #[test]
    fn oracle_rows_without_long_values_cost_nothing() {
        let col = |name: &str, ty: &str| Column {
            name: name.into(),
            type_name: ty.into(),
            numeric: false,
        };
        let columns = [col("B", "BLOB"), col("L", "LONG RAW"), col("C", "CLOB")];
        let row = vec![
            Some("AB".repeat(2000)),
            Some(format!("0x{}", "CD".repeat(1999))),
            Some("x".repeat(4000)),
        ];
        let head = "insert into t (b, l, c) values (";
        let bytes = allocated(|| assert_eq!(oracle_block(head, &columns, &row), None));
        assert_eq!(bytes, 0);
        // A long value's hex isn't copied either, unless it needs a 0.
        let long = "AB".repeat(2001);
        let bytes = allocated(|| assert!(long_binary(&columns[0], &long).is_some()));
        assert_eq!(bytes, 0);
        let typed = format!("0x{}", "A".repeat(4001));
        assert_eq!(
            long_binary(&columns[1], &typed).as_deref(),
            Some(&*format!("0{}", &typed[2..]))
        );
    }

    /// A CLOB's text over the 4000 bytes a literal holds is a CLOB of
    /// parts of at most 1000 UTF-16 code units, a pair of surrogates kept
    /// whole and each `&` apart (#317); INSERT statements append the parts
    /// in a PL/SQL block. VARCHAR2 and shorter CLOBs stay one literal.
    #[test]
    fn oracle_long_text_values() {
        let col = |name: &str, ty: &str| Column {
            name: name.into(),
            type_name: ty.into(),
            numeric: false,
        };
        let x = |n: usize| "x".repeat(n);
        let long = x(4001);
        let clob = col("C", "CLOB");
        assert_eq!(
            sql_literal(&clob, Some(&long)),
            format!(
                "to_clob('{0}') || to_clob('{0}') || to_clob('{0}') || to_clob('{0}') || \
                 to_clob('x')",
                x(1000)
            )
        );
        for ty in ["NCLOB", "LONG"] {
            assert!(sql_literal(&col("C", ty), Some(&long)).starts_with("to_clob('x"));
        }
        assert_eq!(sql_literal(&clob, Some(&x(4000))), format!("'{}'", x(4000)));
        let varchar = col("V", "VARCHAR2(32767)");
        assert_eq!(sql_literal(&varchar, Some(&long)), format!("'{long}'"));
        let wide = format!("{}\u{1F600}{}", x(999), "y".repeat(3100));
        let parts = text_parts(&wide);
        assert_eq!(parts[0], x(999));
        assert_eq!(parts[1], format!("\u{1F600}{}", "y".repeat(998)));
        assert_eq!(parts.concat(), wide);
        assert!(parts.iter().all(|p| p.encode_utf16().count() <= 1000));
        let amp = format!("it's & {}", x(4000));
        assert!(
            sql_literal(&clob, Some(&amp)).starts_with("to_clob('it''s ' || chr(38) || ' xxx"),
            "{}",
            &sql_literal(&clob, Some(&amp))[..40]
        );

        let columns = [
            Column {
                name: "ID".into(),
                type_name: "NUMBER".into(),
                numeric: true,
            },
            clob.clone(),
        ];
        let rows = vec![vec![Some("1".into()), Some(amp.clone())]];
        let options = Options {
            table: "t".into(),
            ..Options::default()
        };
        let text =
            String::from_utf8(export_to_vec(Format::Insert, &columns, &rows, &options).unwrap())
                .unwrap();
        let expected = format!(
            "declare\n  pythia$2 clob;\nbegin\n  dbms_lob.createtemporary(pythia$2, true);\n  \
             dbms_lob.writeappend(pythia$2, 1000, 'it''s ' || chr(38) || ' {}');\n{}  \
             dbms_lob.writeappend(pythia$2, 7, '{}');\n  insert into t (id, c) values (1, \
             pythia$2);\nend;\n/\n",
            x(993),
            format!("  dbms_lob.writeappend(pythia$2, 1000, '{}');\n", x(1000)).repeat(3),
            x(7)
        );
        assert_eq!(text, expected);
    }

    /// A JSON's and a VECTOR's text over 4000 bytes is written as a
    /// CLOB's (#342), which Oracle reads into either; a shorter one stays
    /// one literal.
    #[test]
    fn oracle_long_json_and_vectors() {
        let col = |name: &str, ty: &str| Column {
            name: name.into(),
            type_name: ty.into(),
            numeric: false,
        };
        let (json, vector) = (col("J", "JSON"), col("V", "VECTOR"));
        let long_json = format!(r#"["{}"]"#, "x".repeat(4000));
        let long_vector = format!("[{}]", ["0.5"; 1000].join(","));
        assert!(sql_literal(&json, Some(&long_json)).starts_with(r#"to_clob('["xxx"#));
        assert!(sql_literal(&vector, Some(&long_vector)).starts_with("to_clob('[0.5,0.5"));
        assert_eq!(sql_literal(&vector, Some("[1,2]")), "'[1,2]'");
        // Each part a CLOB of its own, a text with many `&` too, whose
        // literals in between Oracle joined into one VARCHAR2 (ORA-01489).
        let amps = format!(r#"["{}"]"#, "a&".repeat(3000));
        let literal = sql_literal(&json, Some(&amps));
        assert_eq!(literal.matches("to_clob(").count(), 7);
        assert!(literal.starts_with(r#"to_clob('["a' || chr(38) || 'a'"#));
        assert!(literal.contains(") || to_clob("));
        assert!(literal.ends_with(r#"chr(38) || '"]')"#), "{literal}");
        let rows = vec![vec![Some(long_json), Some(long_vector)]];
        let options = Options {
            table: "t".into(),
            ..Options::default()
        };
        let text = String::from_utf8(
            export_to_vec(Format::Insert, &[json, vector], &rows, &options).unwrap(),
        )
        .unwrap();
        assert!(
            text.starts_with("declare\n  pythia$1 clob;\n  pythia$2 clob;\nbegin\n"),
            "{}",
            &text[..100]
        );
        assert!(
            text.ends_with("  insert into t (j, v) values (pythia$1, pythia$2);\nend;\n/\n"),
            "{text}"
        );
    }

    /// A JSON's extended objects as Pythia shows its scalars of types JSON
    /// text has none of are those scalars (#370), each set at its path;
    /// others, as MongoDB's, stay objects. In an UPDATE, a scalar where and
    /// as it was is taken from the column.
    #[test]
    fn oracle_json_extended_scalars() {
        let json = Column {
            name: "J".into(),
            type_name: "JSON".into(),
            numeric: false,
        };
        let lit = |v: &str| sql_literal(&json, Some(v));
        assert_eq!(
            lit(r#"{"$oracleDate":"2020-01-01T00:00:00"}"#),
            r#"json('{"$oracleDate":"2020-01-01T00:00:00"}' extended)"#
        );
        assert_eq!(
            lit(r#"{"a":[1,{"$numberDouble":"Nan"}],"it's \"&\\":{"$rawhex":"AB"}}"#),
            concat!(
                r#"json_transform(json('{"a":[1,{"$numberDouble":"Nan"}],"it''s \"' || chr(38) || '\\":{"$rawhex":"AB"}}'), "#,
                r#"set '$."a"[1]' = json('{"$numberDouble":"Nan"}' extended), "#,
                r#"set '$."it''s \"\u0026\\"' = json('{"$rawhex":"AB"}' extended))"#
            )
        );
        // MongoDB's objects, Oracle's own `$binary` and objects of more
        // keys stay objects; text that isn't JSON is left to Oracle.
        for plain in [
            r#"{"_id":{"$oid":"5f1b2c3d4e5f6a7b8c9d0e1f"}}"#,
            r#"{"n":{"$numberDouble":"1.5"},"b":{"$binary":"q80="}}"#,
            r#"{"d":{"$oracleDate":"2020-01-01T00:00:00","x":1}}"#,
            r#"{"d":{"$oracleDate":1}}"#,
            r#"{"$oracleDate":"2020-01-01T00:00:00""#,
            r#""$oracleDate""#,
        ] {
            assert_eq!(lit(plain), text_literal(plain), "{plain}");
        }
        // Other columns.
        let clob = Column {
            type_name: "CLOB".into(),
            ..json.clone()
        };
        let date = r#"{"$oracleDate":"2020-01-01T00:00:00"}"#;
        assert_eq!(sql_literal(&clob, Some(date)), format!("'{date}'"));
        // A long one is JSON of CLOB parts.
        let long = format!(r#"{{"d":{date},"s":"{}"}}"#, "x".repeat(4000));
        let literal = lit(&long);
        assert!(
            literal.starts_with(r#"json_transform(json(to_clob('{"d":"#),
            "{literal}"
        );
        assert!(literal.ends_with(&format!(r#"')), set '$."d"' = json('{date}' extended))"#)));

        // An UPDATE takes the scalars where and as they were from the column.
        let held =
            r#"{"a":{"$oracleDate":"2020-01-01T00:00:00"},"b":[{"$numberFloat":1.5}],"c":1}"#;
        let new = r#"{"a":{"$oracleDate":"2020-01-01T00:00:00"},"b":[{"$numberFloat":2.5}],"c":2}"#;
        assert_eq!(
            sql_literal_replacing(&json, new, "J", held),
            format!(
                "json_transform(json('{new}'), \
                 set '$.\"a\"' = json_query(J, '$.\"a\"' returning json), \
                 set '$.\"b\"[0]' = json('{{\"$numberFloat\":2.5}}' extended))"
            )
        );
        assert_eq!(
            sql_literal_replacing(&json, date, "J", date),
            "json_query(J, '$' returning json)"
        );
        assert_eq!(sql_literal_replacing(&json, "[1]", "J", held), "'[1]'");
        let number = Column {
            type_name: "NUMBER".into(),
            numeric: true,
            ..json.clone()
        };
        assert_eq!(sql_literal_replacing(&number, "1.5", "N", "1"), "1.5");

        // INSERT statements, a long one in a block.
        let rows = vec![
            vec![Some(
                r#"{"t":{"$oracleTimestamp":"2020-01-01T10:00:00.5"}}"#.into(),
            )],
            vec![Some(long.clone())],
        ];
        let options = Options {
            table: "t".into(),
            ..Options::default()
        };
        let text = String::from_utf8(
            export_to_vec(Format::Insert, std::slice::from_ref(&json), &rows, &options).unwrap(),
        )
        .unwrap();
        assert!(
            text.starts_with(concat!(
                r#"insert into t (j) values (json_transform(json('{"t":{"$oracleTimestamp":"2020-01-01T10:00:00.5"}}'), "#,
                r#"set '$."t"' = json('{"$oracleTimestamp":"2020-01-01T10:00:00.5"}' extended)));"#,
                "\ndeclare\n  pythia$1 clob;\n"
            )),
            "{text}"
        );
        assert!(
            text.ends_with(&format!(
                "  insert into t (j) values (json_transform(json(pythia$1), set '$.\"d\"' = json('{date}' extended)));\nend;\n/\n"
            )),
            "{text}"
        );
    }

    #[test]
    fn markdown_and_html_escape() {
        assert!(text(Format::Markdown).contains("| 2.5 |  |  | a\\|b |"));
        assert!(
            text(Format::Markdown)
                .starts_with("| ID | NAME | HIRED | Mixed Case |\n| ---: | --- | --- | --- |\n")
        );
        assert!(text(Format::Html).contains("<td>O'Brien, &quot;Pat&quot;</td>"));
    }

    #[test]
    fn xlsx_is_zip() {
        let (c, r) = sample();
        let bytes = export_to_vec(Format::Xlsx, &c, &r, &Options::default()).unwrap();
        assert_eq!(&bytes[..2], b"PK");
        // A text longer than a cell holds names its row and column.
        let mut long = r.clone();
        long[1][1] = Some("x".repeat(XLSX_CELL_CHARS + 1));
        let e = export_to_vec(Format::Xlsx, &c, &long, &Options::default()).unwrap_err();
        assert!(
            e.to_string().starts_with(&format!("Row 2's {}", c[1].name)),
            "{e}"
        );
    }

    /// Every database's dates and timestamps become Excel dates (#193),
    /// with their fractions of seconds (#192); those with a zone, and text
    /// that isn't a date, stay text.
    #[test]
    fn xlsx_dates() {
        use calamine::{Data, Reader};
        let cases = [
            ("DATE", "2020-03-04 23:09:48", Some(48.0)),
            ("TIMESTAMP(6)", "2020-03-04 23:09:48.167000", Some(48.167)),
            ("DATETIME", "2020-03-04 23:09:48.167", Some(48.167)),
            ("timestamp", "2020-03-04 23:09:48.167", Some(48.167)),
            ("datetime2", "2020-03-04 23:09:48.1670000", Some(48.167)),
            ("datetime", "2020-03-04 23:09:48.167", Some(48.167)),
            ("smalldatetime", "2020-03-04 23:09:00", Some(0.0)),
            ("date", "2020-03-04", Some(-83388.0)),
            (
                "TIMESTAMP(6) WITH TIME ZONE",
                "2020-03-04 23:09:48.167000 +01:00",
                None,
            ),
            (
                "timestamp with time zone",
                "2020-03-04 23:09:48.167+01",
                None,
            ),
            ("datetimeoffset", "2020-03-04 23:09:48.1670000 +01:00", None),
            ("DATETIME", "yesterday", None),
            ("DATETIME", "1899-12-31 10:00:00", None),
        ];
        let columns: Vec<Column> = cases
            .iter()
            .map(|(t, _, _)| Column {
                name: "D".into(),
                type_name: (*t).into(),
                numeric: false,
            })
            .collect();
        let rows = vec![cases.iter().map(|(_, v, _)| Some(v.to_string())).collect()];
        let bytes = export_to_vec(Format::Xlsx, &columns, &rows, &Options::default()).unwrap();
        let mut book = calamine::open_workbook_auto_from_rs(std::io::Cursor::new(bytes)).unwrap();
        let sheet = book.worksheet_range("Result").unwrap();
        // 2020-03-04 as an Excel day, and 23:09 as its fraction.
        let day = 43894.0;
        let hm = (23.0 * 3600.0 + 9.0 * 60.0) / 86400.0;
        for (i, (t, value, seconds)) in cases.iter().enumerate() {
            let cell = sheet.get((1, i)).unwrap();
            match seconds {
                Some(s) => {
                    let Data::DateTime(dt) = cell else {
                        panic!("{t} {value}: {cell:?}")
                    };
                    let expected = if *s < 0.0 {
                        day
                    } else {
                        day + hm + s / 86400.0
                    };
                    assert!(
                        (dt.as_f64() - expected).abs() < 1e-9,
                        "{t}: {}",
                        dt.as_f64()
                    );
                }
                None => assert_eq!(cell, &Data::String(value.to_string()), "{t}"),
            }
        }
    }

    #[test]
    fn streams_like_export() {
        let columns = vec![
            Column {
                name: "ID".into(),
                type_name: "NUMBER".into(),
                numeric: true,
            },
            Column {
                name: "NAME".into(),
                type_name: "VARCHAR2".into(),
                numeric: false,
            },
        ];
        let rows: Vec<Vec<Cell>> = (0..5)
            .map(|i| vec![Some(i.to_string()), Some(format!("n|{i},\"x\""))])
            .collect();
        for &format in Format::STREAMING {
            let options = Options::default();
            let whole = export_to_vec(format, &columns, &rows, &options).unwrap();
            let mut stream =
                StreamWriter::new(format, columns.clone(), options, Vec::new()).unwrap();
            stream.write(&rows[..2]).unwrap();
            stream.write(&rows[2..]).unwrap();
            assert_eq!(stream.rows(), 5);
            let streamed = stream.finish().unwrap();
            if format == Format::Json {
                let a: serde_json::Value = serde_json::from_slice(&whole).unwrap();
                let b: serde_json::Value = serde_json::from_slice(&streamed).unwrap();
                assert_eq!(a, b);
            } else {
                assert_eq!(
                    String::from_utf8(whole).unwrap(),
                    String::from_utf8(streamed).unwrap(),
                    "{format:?}"
                );
            }
        }
        let empty = StreamWriter::new(Format::Json, columns, Options::default(), Vec::new())
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&empty).unwrap(),
            serde_json::json!([])
        );
    }
}
