//! Reading CSV / Excel files for import, with type inference.
//!
//! Files are read into a [`Sheet`] of text cells. [`infer`] then decides per
//! column whether it holds numbers, dates (and in which format) or text, so
//! the importer can convert values exactly and suggest a CREATE TABLE.

use std::io::Cursor;
use std::path::Path;

use chrono::{NaiveDate, NaiveDateTime};
use pythia_db::Dialect;

use crate::{Error, Result};

/// A table of text cells read from a file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sheet {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CsvOptions {
    /// Field separator; detected when `None`.
    pub delimiter: Option<u8>,
    /// Whether the first row holds column names; detected when `None`.
    pub header: Option<bool>,
}

/// Reads delimited text. Empty fields are NULL.
pub fn read_csv(data: &[u8], options: CsvOptions) -> Result<Sheet> {
    let text = String::from_utf8_lossy(data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data));
    let delimiter = options.delimiter.unwrap_or_else(|| sniff_delimiter(&text));
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());
    let mut records: Vec<Vec<Option<String>>> = Vec::new();
    for record in reader.records() {
        let record = record?;
        records.push(
            record
                .iter()
                .map(|f| Some(f.to_owned()).filter(|v| !v.is_empty()))
                .collect(),
        );
    }
    Ok(into_sheet(records, options.header))
}

/// Picks the delimiter that splits the first lines most consistently.
fn sniff_delimiter(text: &str) -> u8 {
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(20)
        .collect();
    let mut best = (b',', 0usize);
    for &d in b",;\t|" {
        let counts: Vec<usize> = lines.iter().map(|l| l.matches(d as char).count()).collect();
        let Some(&first) = counts.first() else {
            continue;
        };
        if first > 0 && counts.iter().all(|c| *c == first) && first > best.1 {
            best = (d, first);
        }
    }
    best.0
}

/// Sheet names of a workbook (xlsx, xls, xlsb, ods).
pub fn workbook_sheets(path: &Path) -> Result<Vec<String>> {
    let workbook = calamine::open_workbook_auto(path).map_err(xl_error)?;
    Ok(calamine::Reader::sheet_names(&workbook))
}

/// Reads one sheet of a workbook. Dates become `YYYY-MM-DD HH24:MI:SS`.
pub fn read_workbook(path: &Path, sheet: &str, header: Option<bool>) -> Result<Sheet> {
    let mut workbook = calamine::open_workbook_auto(path).map_err(xl_error)?;
    let range = calamine::Reader::worksheet_range(&mut workbook, sheet).map_err(xl_error)?;
    Ok(into_sheet(
        range
            .rows()
            .map(|row| row.iter().map(cell_text).collect())
            .collect(),
        header,
    ))
}

/// Same as [`read_workbook`] for workbook bytes.
pub fn read_workbook_bytes(
    data: &[u8],
    sheet: Option<&str>,
    header: Option<bool>,
) -> Result<Sheet> {
    let mut workbook =
        calamine::open_workbook_auto_from_rs(Cursor::new(data.to_vec())).map_err(xl_error)?;
    let names = calamine::Reader::sheet_names(&workbook);
    let name = sheet
        .map(str::to_owned)
        .or_else(|| names.first().cloned())
        .unwrap_or_default();
    let range = calamine::Reader::worksheet_range(&mut workbook, &name).map_err(xl_error)?;
    Ok(into_sheet(
        range
            .rows()
            .map(|row| row.iter().map(cell_text).collect())
            .collect(),
        header,
    ))
}

fn xl_error(e: impl std::fmt::Display) -> Error {
    Error::Io(std::io::Error::other(e.to_string()))
}

fn cell_text(cell: &calamine::Data) -> Option<String> {
    use calamine::Data;
    match cell {
        Data::Empty => None,
        Data::String(s) if s.is_empty() => None,
        Data::String(s) => Some(s.clone()),
        Data::Int(i) => Some(i.to_string()),
        Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => Some(format!("{f:.0}")),
        Data::Float(f) => Some(f.to_string()),
        Data::Bool(b) => Some(if *b { "1" } else { "0" }.into()),
        Data::DateTime(dt) => {
            let (y, mo, d, h, mi, sec, _) = dt.to_ymd_hms_milli();
            Some(format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{sec:02}"))
        }
        Data::DateTimeIso(s) | Data::DurationIso(s) => Some(s.clone()),
        Data::Error(e) => Some(format!("#{e:?}")),
    }
}

/// Splits off the header row (detected if `header` is `None`) and pads rows.
fn into_sheet(mut records: Vec<Vec<Option<String>>>, header: Option<bool>) -> Sheet {
    records.retain(|r| r.iter().any(Option::is_some));
    let width = records.iter().map(Vec::len).max().unwrap_or(0);
    for r in &mut records {
        r.resize(width, None);
    }
    let has_header = header.unwrap_or_else(|| looks_like_header(&records));
    let headers = if has_header && !records.is_empty() {
        let first = records.remove(0);
        first
            .into_iter()
            .enumerate()
            .map(|(i, h)| h.unwrap_or_else(|| format!("COLUMN_{}", i + 1)))
            .collect()
    } else {
        (1..=width).map(|i| format!("COLUMN_{i}")).collect()
    };
    Sheet {
        headers,
        rows: records,
    }
}

/// A first row of non-numeric, distinct labels over data that has numbers
/// or dates in those columns is a header.
fn looks_like_header(records: &[Vec<Option<String>>]) -> bool {
    let Some(first) = records.first() else {
        return false;
    };
    let labels = first.iter().all(|c| {
        c.as_deref()
            .is_some_and(|v| parse_number(v).is_none() && !looks_like_date(v))
    });
    if !labels {
        return false;
    }
    if records.len() == 1 {
        return true;
    }
    // Header if any column's first row is text while the rest is typed.
    (0..first.len()).any(|c| {
        let rest: Vec<&str> = records[1..]
            .iter()
            .filter_map(|r| r.get(c)?.as_deref())
            .collect();
        !rest.is_empty()
            && rest
                .iter()
                .all(|v| parse_number(v).is_some() || looks_like_date(v))
    }) || records[1..].iter().all(|r| r != first)
}

/// What a column holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Numbers; `decimal_comma` if written like `1.234,5`.
    Number {
        decimal_comma: bool,
        scale: u32,
        digits: u32,
    },
    /// Dates; the Oracle format to read them with.
    Date {
        oracle_format: &'static str,
    },
    Text {
        max_chars: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub kind: Kind,
    pub nullable: bool,
}

/// (chrono format, Oracle format) pairs tried for dates, most specific first.
const DATE_FORMATS: &[(&str, &str, bool)] = &[
    ("%Y-%m-%d %H:%M:%S", "YYYY-MM-DD HH24:MI:SS", true),
    ("%Y-%m-%dT%H:%M:%S", "YYYY-MM-DD\"T\"HH24:MI:SS", true),
    ("%Y-%m-%d %H:%M", "YYYY-MM-DD HH24:MI", true),
    ("%Y-%m-%d", "YYYY-MM-DD", false),
    ("%d.%m.%Y %H:%M:%S", "DD.MM.YYYY HH24:MI:SS", true),
    ("%d.%m.%Y %H:%M", "DD.MM.YYYY HH24:MI", true),
    ("%d.%m.%Y", "DD.MM.YYYY", false),
    ("%m/%d/%Y", "MM/DD/YYYY", false),
    ("%d/%m/%Y", "DD/MM/YYYY", false),
];

fn date_format_for(value: &str) -> Option<usize> {
    DATE_FORMATS.iter().position(|(fmt, _, time)| {
        if *time {
            NaiveDateTime::parse_from_str(value, fmt).is_ok()
        } else {
            NaiveDate::parse_from_str(value, fmt).is_ok()
        }
    })
}

fn looks_like_date(value: &str) -> bool {
    date_format_for(value.trim()).is_some()
}

/// Parses `1234`, `-1.5`, `1'234.50`, `1,234.50`, `1.234,50`, `1e3`.
/// Returns the normalized text (dot decimal) and whether a decimal comma was used.
pub fn parse_number(value: &str) -> Option<(String, bool)> {
    let v = value.trim().replace(['\'', ' ', '\u{a0}'], "");
    if v.is_empty() {
        return None;
    }
    let (dots, commas) = (v.matches('.').count(), v.matches(',').count());
    let (normalized, decimal_comma) = match (dots, commas) {
        (_, 0) => (v.clone(), false),
        (0, 1) => (v.replace(',', "."), true),
        (0, _) => (v.replace(',', ""), false),
        _ => {
            // Both: the last one is the decimal separator.
            if v.rfind(',') > v.rfind('.') {
                (v.replace('.', "").replace(',', "."), true)
            } else {
                (v.replace(',', ""), false)
            }
        }
    };
    normalized
        .parse::<f64>()
        .ok()
        .filter(|f| f.is_finite())
        .map(|_| (normalized, decimal_comma))
}

/// Infers the kind of every column from its values.
pub fn infer(sheet: &Sheet) -> Vec<ColumnInfo> {
    (0..sheet.headers.len())
        .map(|c| {
            let values: Vec<&str> = sheet
                .rows
                .iter()
                .filter_map(|r| r.get(c)?.as_deref())
                .map(str::trim)
                .collect();
            let nullable = values.len() < sheet.rows.len();
            let max_chars = values
                .iter()
                .map(|v| v.chars().count())
                .max()
                .unwrap_or(1)
                .max(1);
            if values.is_empty() {
                return ColumnInfo {
                    kind: Kind::Text { max_chars },
                    nullable: true,
                };
            }
            // Leading zeros (zip codes, ids) must stay text.
            let leading_zero = values.iter().any(|v| {
                v.len() > 1 && v.starts_with('0') && !v.starts_with("0.") && !v.starts_with("0,")
            });
            let numbers: Vec<(String, bool)> =
                values.iter().filter_map(|v| parse_number(v)).collect();
            if !leading_zero && numbers.len() == values.len() {
                let decimal_comma = numbers.iter().any(|(_, dc)| *dc);
                let scale = numbers
                    .iter()
                    .map(|(n, _)| n.split_once('.').map_or(0, |(_, f)| f.len() as u32))
                    .max()
                    .unwrap_or(0);
                let digits = numbers
                    .iter()
                    .map(|(n, _)| {
                        n.trim_start_matches('-')
                            .split('.')
                            .next()
                            .unwrap_or("")
                            .len() as u32
                    })
                    .max()
                    .unwrap_or(1);
                return ColumnInfo {
                    kind: Kind::Number {
                        decimal_comma,
                        scale,
                        digits,
                    },
                    nullable,
                };
            }
            for (fmt_index, (fmt, oracle, time)) in DATE_FORMATS.iter().enumerate() {
                let all = values.iter().all(|v| {
                    if *time {
                        NaiveDateTime::parse_from_str(v, fmt).is_ok()
                    } else {
                        NaiveDate::parse_from_str(v, fmt).is_ok()
                    }
                });
                if all {
                    let _ = fmt_index;
                    return ColumnInfo {
                        kind: Kind::Date {
                            oracle_format: oracle,
                        },
                        nullable,
                    };
                }
            }
            ColumnInfo {
                kind: Kind::Text { max_chars },
                nullable,
            }
        })
        .collect()
}

/// Oracle column type for an inferred column.
pub fn oracle_type(info: &ColumnInfo) -> String {
    match &info.kind {
        Kind::Number {
            scale: 0, digits, ..
        } if *digits <= 18 => {
            // Leave headroom for values larger than the sample.
            format!("NUMBER({})", (*digits + 2).clamp(10, 38))
        }
        Kind::Number { .. } => "NUMBER".into(),
        Kind::Date { .. } => "DATE".into(),
        Kind::Text { max_chars } if *max_chars > 4000 => "CLOB".into(),
        // Leave some room: round up to a friendly size.
        Kind::Text { max_chars } => {
            let size = [10, 20, 30, 50, 100, 200, 500, 1000, 2000, 4000]
                .into_iter()
                .find(|s| *s >= *max_chars)
                .unwrap_or(4000);
            format!("VARCHAR2({size} CHAR)")
        }
    }
}

/// Turns a header into an Oracle column name: `Order date` → `ORDER_DATE`,
/// `Café` → `CAFÉ` as Oracle folds `café` (#380).
pub fn column_name(header: &str) -> String {
    let name = header
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    let mut name = Dialect::ORACLE.dictionary_name(&name);
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        name = format!("C_{name}");
    }
    // Identifiers are limited to 128 bytes (12.2+), not characters.
    let mut end = name.len().min(128);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name.truncate(end);
    name
}

/// The SQL expression that converts the bind `:n` for a column.
pub fn bind_expr(n: usize, info: Option<&ColumnInfo>) -> String {
    match info.map(|i| &i.kind) {
        Some(Kind::Date { oracle_format }) => {
            format!("to_date(:{n}, '{}')", oracle_format.replace('\'', "''"))
        }
        // Values are normalized to a dot decimal; the importer sets
        // NLS_NUMERIC_CHARACTERS accordingly on its session.
        Some(Kind::Number { .. }) => format!("to_number(:{n})"),
        _ => format!(":{n}"),
    }
}

/// Converts a cell for binding (numbers to dot-decimal text).
pub fn bind_value(value: Option<&str>, info: Option<&ColumnInfo>) -> Option<String> {
    let v = value?.trim();
    if v.is_empty() {
        return None;
    }
    match info.map(|i| &i.kind) {
        Some(Kind::Number { .. }) => Some(parse_number(v).map_or_else(|| v.to_owned(), |(n, _)| n)),
        _ => Some(value?.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_sniffing_and_header() {
        let sheet = read_csv(
            b"\xef\xbb\xbfid;name;amount;day\n1;Anna;1.234,50;31.12.2024\n2;;7;01.01.2025\n",
            CsvOptions::default(),
        )
        .unwrap();
        assert_eq!(sheet.headers, ["id", "name", "amount", "day"]);
        assert_eq!(sheet.rows[1][1], None);
        let info = infer(&sheet);
        assert_eq!(
            info[0].kind,
            Kind::Number {
                decimal_comma: false,
                scale: 0,
                digits: 1
            }
        );
        assert!(matches!(
            info[2].kind,
            Kind::Number {
                decimal_comma: true,
                scale: 2,
                ..
            }
        ));
        assert_eq!(
            info[3].kind,
            Kind::Date {
                oracle_format: "DD.MM.YYYY"
            }
        );
        assert!(info[1].nullable);
        assert_eq!(oracle_type(&info[1]), "VARCHAR2(10 CHAR)");
        assert_eq!(
            bind_value(Some("1.234,50"), Some(&info[2])).as_deref(),
            Some("1234.50")
        );
    }

    #[test]
    fn headerless_and_leading_zeros() {
        let sheet = read_csv(
            b"0042,2024-01-02 10:00:00\n0100,2024-02-03 11:30:00\n",
            CsvOptions::default(),
        )
        .unwrap();
        assert_eq!(sheet.headers, ["COLUMN_1", "COLUMN_2"]);
        let info = infer(&sheet);
        assert!(matches!(info[0].kind, Kind::Text { .. }));
        assert_eq!(
            info[1].kind,
            Kind::Date {
                oracle_format: "YYYY-MM-DD HH24:MI:SS"
            }
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_number("1'234.5"), Some(("1234.5".into(), false)));
        assert_eq!(parse_number("1,234.5"), Some(("1234.5".into(), false)));
        assert_eq!(parse_number("-3,5"), Some(("-3.5".into(), true)));
        assert_eq!(parse_number("abc"), None);
    }

    #[test]
    fn names() {
        assert_eq!(column_name("Order date"), "ORDER_DATE");
        assert_eq!(column_name(" 2nd value! "), "C_2ND_VALUE");
        assert_eq!(column_name(&"Ä".repeat(100)).len(), 128);
        // As Oracle folds café and straße (#380).
        assert_eq!(column_name("Café"), "CAFÉ");
        assert_eq!(column_name("Straße"), "STRAßE");
    }

    #[test]
    fn xlsx_roundtrip() {
        let columns = vec![
            pythia_db::Column {
                name: "ID".into(),
                type_name: "NUMBER".into(),
                numeric: true,
            },
            pythia_db::Column {
                name: "HIRED".into(),
                type_name: "DATE".into(),
                numeric: false,
            },
        ];
        let rows = vec![vec![Some("1".into()), Some("2020-01-02 03:04:05".into())]];
        let bytes = crate::export_to_vec(
            crate::Format::Xlsx,
            &columns,
            &rows,
            &crate::Options::default(),
        )
        .unwrap();
        let sheet = read_workbook_bytes(&bytes, None, None).unwrap();
        assert_eq!(sheet.headers, ["ID", "HIRED"]);
        assert_eq!(
            sheet.rows,
            vec![vec![Some("1".into()), Some("2020-01-02 03:04:05".into())]]
        );
    }
}
