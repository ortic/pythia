//! Text files as their bytes: UTF-8 (with or without BOM), UTF-16 with its
//! BOM, or else Windows-1252, as script files and CSV files come.

/// How the bytes of a text were written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    /// Bytes that aren't UTF-8: legacy Windows files.
    Windows1252,
}

/// Windows-1252 0x80..=0x9F; the rest of the code page equals Latin-1.
pub const CP1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

/// The text of `bytes`, without a byte order mark, and how they hold it.
pub fn decode(bytes: &[u8]) -> (String, Detected) {
    let utf16 = |rest: &[u8], unit: fn([u8; 2]) -> u16| {
        // An odd byte at the end is no character.
        let units = rest.chunks(2).map(|c| match c {
            &[a, b] => unit([a, b]),
            _ => 0xfffd,
        });
        char::decode_utf16(units)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect::<String>()
    };
    if let Some(rest) = bytes.strip_prefix(b"\xff\xfe") {
        (utf16(rest, u16::from_le_bytes), Detected::Utf16Le)
    } else if let Some(rest) = bytes.strip_prefix(b"\xfe\xff") {
        (utf16(rest, u16::from_be_bytes), Detected::Utf16Be)
    } else if let Some(rest) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        (
            String::from_utf8_lossy(rest).into_owned(),
            Detected::Utf8Bom,
        )
    } else {
        match std::str::from_utf8(bytes) {
            Ok(text) => (text.to_owned(), Detected::Utf8),
            Err(_) => (windows_1252(bytes), Detected::Windows1252),
        }
    }
}

/// `bytes` as Windows-1252, which takes every byte.
pub fn windows_1252(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9f => CP1252_HIGH[usize::from(b - 0x80)],
            b => char::from(b),
        })
        .collect()
}
