//! Reading statement text, for the backends that aren't Oracle: what a
//! statement is, its bind variables, and values rendered as text.

use pythia_plsql::{TokenKind, tokenize};

use crate::compat::Release;

/// The first one or two words of a statement, uppercase, comments skipped.
pub(crate) fn leading_words(sql: &str) -> Vec<String> {
    tokenize(sql)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .take(2)
        .map(|t| sql[t.span].to_ascii_uppercase())
        .collect()
}

/// The statement's first word, e.g. `INSERT`, for "INSERT: 3 rows affected".
pub(crate) fn statement_kind(sql: &str) -> String {
    leading_words(sql)
        .into_iter()
        .next()
        .unwrap_or_else(|| "Statement".into())
}

/// Whether any word of the statement (not in strings or comments) is one of
/// `words` (uppercase).
pub(crate) fn has_word(sql: &str, words: &[&str]) -> bool {
    tokenize(sql).iter().any(|t| {
        t.kind.is_word()
            && words
                .iter()
                .any(|w| sql[t.span.clone()].eq_ignore_ascii_case(w))
    })
}

/// Whether the word `word` is directly followed by one of `next` (words of
/// the statement, not in strings or comments).
pub(crate) fn word_followed_by(sql: &str, word: &str, next: &[&str]) -> bool {
    let words: Vec<&str> = tokenize(sql)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| &sql[t.span])
        .collect();
    words.windows(2).any(|pair| {
        pair[0].eq_ignore_ascii_case(word) && next.iter().any(|n| pair[1].eq_ignore_ascii_case(n))
    })
}

/// Whether the statement has a `;` before its end, i.e. is more than one.
pub(crate) fn has_inner_semicolon(sql: &str) -> bool {
    let tokens = tokenize(sql);
    let last = tokens.iter().rposition(|t| !t.kind.is_trivia());
    tokens
        .iter()
        .enumerate()
        .any(|(i, t)| t.kind == TokenKind::Semicolon && Some(i) != last)
}

/// Replaces the `:name` bind variables that have a value (names match
/// without case, as Oracle's do) by what `replace` makes of the value's
/// position (from 1) and value.
pub(crate) fn replace_binds(
    sql: &str,
    binds: &[(String, Option<String>)],
    mut replace: impl FnMut(usize, &Option<String>) -> String,
) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    let mut position = 0;
    for token in tokenize(sql) {
        if token.kind != TokenKind::BindVar {
            continue;
        }
        let name = &sql[token.span.start + 1..token.span.end];
        let Some((_, value)) = binds.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) else {
            continue;
        };
        position += 1;
        out.push_str(&sql[last..token.span.start]);
        out.push_str(&replace(position, value));
        last = token.span.end;
    }
    out.push_str(&sql[last..]);
    out
}

/// The bytes of a binary value a result holds, like a RAW column; a longer
/// value is cut short (see [`hex`]).
pub const SHOWN_BYTES: usize = 4000;

/// How much text a batch of whole values holds (a row past it, at least
/// one): rows of large blobs come a few at a time, not 1000 (#204).
pub(crate) const WHOLE_BATCH_BYTES: usize = 64 << 20;

/// Uppercase hex: of the first [`SHOWN_BYTES`] with the value's size after
/// them, `89504E47…… (10485760 bytes)`, or all of it when the value is
/// wanted `whole` (#204).
pub(crate) fn hex(bytes: &[u8], whole: bool) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let shown = if whole {
        bytes.len()
    } else {
        bytes.len().min(SHOWN_BYTES)
    };
    let mut out = String::with_capacity(shown * 2 + 24);
    for &b in &bytes[..shown] {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    if shown < bytes.len() {
        out.push_str(&format!("… ({} bytes)", bytes.len()));
    }
    out
}

/// The size of a binary value a result holds cut short: the number in
/// `… (10485760 bytes)` after the hex of its first [`SHOWN_BYTES`] (also
/// after SQL Server's `0x`). `None` for any other text.
pub fn cut_size(value: &str) -> Option<u64> {
    if !value.ends_with(" bytes)") {
        return None;
    }
    let hex = value.strip_prefix("0x").unwrap_or(value);
    let (head, tail) = hex.split_at_checked(SHOWN_BYTES * 2)?;
    let size: u64 = tail
        .strip_prefix("… (")?
        .strip_suffix(" bytes)")?
        .parse()
        .ok()?;
    (size > SHOWN_BYTES as u64 && head.bytes().all(|b| b.is_ascii_hexdigit())).then_some(size)
}

/// The text of a row's values, for [`WHOLE_BATCH_BYTES`].
pub(crate) fn row_bytes(row: &[Option<String>]) -> usize {
    row.iter().flatten().map(String::len).sum()
}

/// `9.4.0` → `(9, 4)`, `10.11.6-MariaDB` → `(10, 11)`, `18.1 (Debian …)` →
/// `(18, 1)`.
pub(crate) fn release(version: &str) -> Release {
    let mut parts = version
        .split(|c: char| !c.is_ascii_digit())
        .map(|p| p.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// The byte offset of a 1-based character position, as PostgreSQL reports
/// where an error is.
pub(crate) fn char_offset(sql: &str, position: usize) -> Option<u32> {
    let offset = match position.checked_sub(1)? {
        0 => 0,
        n => sql.char_indices().nth(n).map_or(sql.len(), |(i, _)| i),
    };
    u32::try_from(offset).ok()
}

/// The byte offset where line `line` (from 1) of the statement starts, as
/// SQL Server reports where an error is.
pub(crate) fn line_offset(sql: &str, line: usize) -> Option<u32> {
    let start: usize = sql
        .split_inclusive('\n')
        .take(line.checked_sub(1)?)
        .map(str::len)
        .sum();
    // Point at the line's first word rather than its indentation.
    let indent = sql[start..].len() - sql[start..].trim_start_matches([' ', '\t']).len();
    u32::try_from(start + indent).ok()
}

/// Days since 1970-01-01 → (year, month, day), for databases that send dates
/// as day counts.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_binds() {
        assert_eq!(leading_words("/* x */ update t set a = 1"), ["UPDATE", "T"]);
        assert_eq!(statement_kind("  "), "Statement");
        assert!(has_word(
            "with d as (delete from t returning *) select 1",
            &["DELETE"]
        ));
        assert!(!has_word("select 'delete' from t -- delete", &["DELETE"]));
        assert!(has_inner_semicolon("select 1; select 2"));
        assert!(!has_inner_semicolon("select ';' from t;"));
        let binds = vec![
            ("id".to_owned(), Some("7".to_owned())),
            ("n".to_owned(), None),
        ];
        let sql = replace_binds(
            "select * from t where id = :ID or :n is null and x = ':id' and y = :other",
            &binds,
            |i, v| format!("${i}{}", if v.is_some() { "v" } else { "" }),
        );
        assert_eq!(
            sql,
            "select * from t where id = $1v or $2 is null and x = ':id' and y = :other"
        );
    }

    /// Binary values past 4000 bytes are cut short with their size, which
    /// reads back, unless wanted whole (#204).
    #[test]
    fn binary_values() {
        assert_eq!(hex(&[0xca, 0xfe, 0x01], false), "CAFE01");
        let big = vec![0xab; 10_000];
        let cut = hex(&big, false);
        assert_eq!(cut, format!("{}… (10000 bytes)", "AB".repeat(4000)));
        assert_eq!(cut_size(&cut), Some(10_000));
        assert_eq!(cut_size(&format!("0x{cut}")), Some(10_000));
        let whole = hex(&big, true);
        assert_eq!(whole, "AB".repeat(10_000));
        assert_eq!(cut_size(&whole), None);
        assert_eq!(cut_size(&hex(&[0; 4000], false)), None);
        assert_eq!(cut_size("ABCD… (10000 bytes)"), None);
        assert_eq!(row_bytes(&[Some("AB".into()), None, Some("é".into())]), 4);
    }

    #[test]
    fn positions() {
        assert_eq!(char_offset("select ä from", 8), Some(7));
        assert_eq!(char_offset("select ä frm", 10), Some(10));
        assert_eq!(char_offset("x", 0), None);
        assert_eq!(line_offset("select 1\n  frm t", 2), Some(11));
        assert_eq!(line_offset("select 1", 1), Some(0));
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-719_162), (1, 1, 1));
        assert_eq!(civil_from_days(-25_567), (1900, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
        assert_eq!(release("18.1 (Debian 18.1-1.pgdg13+2)"), (18, 1));
        assert_eq!(release("16.0.4305.5"), (16, 0));
    }
}
