//! Reading and writing script files the way they were: UTF-8 (with or
//! without BOM), UTF-16 with its BOM or Windows-1252, LF or CRLF. The editor always works with
//! `\n` and Unicode text.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Encoding {
    #[default]
    Utf8,
    Utf8Bom,
    /// Legacy Windows files (what PL/SQL Developer writes by default).
    Windows1252,
    /// UTF-16 with its byte order mark, as SQL Server Management Studio's
    /// scripts and Windows PowerShell's `>` write them.
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    pub const ALL: [Encoding; 5] = [
        Encoding::Utf8,
        Encoding::Utf8Bom,
        Encoding::Utf16Le,
        Encoding::Utf16Be,
        Encoding::Windows1252,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 BOM",
            Encoding::Windows1252 => "Windows-1252",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFile {
    pub text: String,
    pub encoding: Encoding,
    pub line_ending: LineEnding,
}

/// Windows-1252 0x80..=0x9F; the rest of the code page equals Latin-1.
const CP1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

pub fn decode(bytes: &[u8]) -> TextFile {
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
    let (text, encoding) = if let Some(rest) = bytes.strip_prefix(b"\xff\xfe") {
        (utf16(rest, u16::from_le_bytes), Encoding::Utf16Le)
    } else if let Some(rest) = bytes.strip_prefix(b"\xfe\xff") {
        (utf16(rest, u16::from_be_bytes), Encoding::Utf16Be)
    } else if let Some(rest) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        (
            String::from_utf8_lossy(rest).into_owned(),
            Encoding::Utf8Bom,
        )
    } else {
        match std::str::from_utf8(bytes) {
            Ok(text) => (text.to_owned(), Encoding::Utf8),
            Err(_) => (
                bytes
                    .iter()
                    .map(|&b| match b {
                        0x80..=0x9f => CP1252_HIGH[usize::from(b - 0x80)],
                        b => char::from(b),
                    })
                    .collect(),
                Encoding::Windows1252,
            ),
        }
    };
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    // Mostly CRLF counts as CRLF; stray CRs are dropped either way.
    let line_ending = if crlf > 0 && crlf * 2 >= lf {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    TextFile {
        text: text.replace("\r\n", "\n"),
        encoding,
        line_ending,
    }
}

/// The bytes for `text`; fails for characters Windows-1252 can't hold.
pub fn encode(text: &str, encoding: Encoding, line_ending: LineEnding) -> Result<Vec<u8>, String> {
    let text = match line_ending {
        LineEnding::Lf => text.to_owned(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
    };
    match encoding {
        Encoding::Utf8 => Ok(text.into_bytes()),
        Encoding::Utf8Bom => {
            let mut out = b"\xef\xbb\xbf".to_vec();
            out.extend(text.into_bytes());
            Ok(out)
        }
        Encoding::Utf16Le => Ok([0xff, 0xfe]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect()),
        Encoding::Utf16Be => Ok([0xfe, 0xff]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_be_bytes))
            .collect()),
        Encoding::Windows1252 => text
            .chars()
            .map(|c| match u32::from(c) {
                n @ (0..=0x7f | 0xa0..=0xff) => Ok(n as u8),
                _ => CP1252_HIGH
                    .iter()
                    .position(|&h| h == c)
                    .map(|i| 0x80 + i as u8)
                    .ok_or_else(|| {
                        format!(
                            "'{c}' can't be saved as Windows-1252; save it as UTF-8 (click the \
                             encoding in the status bar)"
                        )
                    }),
            })
            .collect(),
    }
}

pub fn read(path: &Path) -> std::io::Result<TextFile> {
    std::fs::read(path).map(|b| decode(&b))
}

pub fn write(
    path: &Path,
    text: &str,
    encoding: Encoding,
    line_ending: LineEnding,
) -> std::io::Result<()> {
    let bytes = encode(text, encoding, line_ending).map_err(std::io::Error::other)?;
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_round_trips() {
        let utf8 = decode("select 'é' from dual;\n".as_bytes());
        assert_eq!(
            (utf8.encoding, utf8.line_ending),
            (Encoding::Utf8, LineEnding::Lf)
        );

        let legacy = b"-- Gr\xfc\xdfe \x80 5\r\nselect 1 from dual;\r\n";
        let file = decode(legacy);
        assert_eq!(file.encoding, Encoding::Windows1252);
        assert_eq!(file.line_ending, LineEnding::CrLf);
        assert_eq!(file.text, "-- Grüße € 5\nselect 1 from dual;\n");
        assert_eq!(
            encode(&file.text, file.encoding, file.line_ending).unwrap(),
            legacy
        );

        let bom = decode(b"\xef\xbb\xbfselect 1;\n");
        assert_eq!(bom.encoding, Encoding::Utf8Bom);
        assert_eq!(bom.text, "select 1;\n");
        assert_eq!(
            encode(&bom.text, bom.encoding, bom.line_ending).unwrap(),
            b"\xef\xbb\xbfselect 1;\n"
        );

        assert!(encode("日本", Encoding::Windows1252, LineEnding::Lf).is_err());
    }

    /// UTF-16 with its BOM, as SQL Server Management Studio writes
    /// scripts: it was read as Windows-1252 (`ÿþs\0e\0l\0…`), and an edit
    /// saved single bytes into it.
    #[test]
    fn utf16_round_trips() {
        let text = "select N'Grüße 𝄞';\r\ngo\r\n";
        let le: Vec<u8> = [0xff, 0xfe]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let file = decode(&le);
        assert_eq!(
            (file.encoding, file.line_ending),
            (Encoding::Utf16Le, LineEnding::CrLf)
        );
        assert_eq!(file.text, "select N'Grüße 𝄞';\ngo\n");
        assert_eq!(
            encode(&file.text, file.encoding, file.line_ending).unwrap(),
            le
        );
        let be: Vec<u8> = [0xfe, 0xff]
            .into_iter()
            .chain("x\n".encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        let file = decode(&be);
        assert_eq!(
            (file.encoding, file.text.as_str()),
            (Encoding::Utf16Be, "x\n")
        );
        assert_eq!(
            encode(&file.text, file.encoding, file.line_ending).unwrap(),
            be
        );
        // An odd byte or a lone surrogate doesn't stop it.
        assert_eq!(decode(b"\xff\xfex\x00\x00\xd8y").text, "x\u{fffd}\u{fffd}");
    }
}
