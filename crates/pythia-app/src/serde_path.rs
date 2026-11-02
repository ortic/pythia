//! Paths in the workspace. A path that isn't UTF-8 (a Linux file name in
//! Latin-1) is no RON string: one such path made the whole workspace fail
//! to save, every window's unsaved text with it (#430). Such a path is kept
//! as its bytes; one that is UTF-8 as before, a string.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Stored {
    Text(String),
    Bytes(Vec<u8>),
}

fn stored(path: &Path) -> Stored {
    match path.to_str() {
        Some(text) => Stored::Text(text.to_owned()),
        None => Stored::Bytes(bytes(path)),
    }
}

fn path(stored: Stored) -> PathBuf {
    match stored {
        Stored::Text(text) => text.into(),
        Stored::Bytes(bytes) => from_bytes(bytes),
    }
}

#[cfg(unix)]
fn bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn from_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes).into()
}

#[cfg(not(unix))]
fn from_bytes(bytes: Vec<u8>) -> PathBuf {
    String::from_utf8_lossy(&bytes).into_owned().into()
}

/// `#[serde(with = "crate::serde_path::option")]` for an `Option<PathBuf>`.
pub mod option {
    use super::*;

    pub fn serialize<S: Serializer>(path: &Option<PathBuf>, s: S) -> Result<S::Ok, S::Error> {
        path.as_deref().map(stored).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
        Ok(Option::<Stored>::deserialize(d)?.map(path))
    }
}

#[cfg(test)]
mod tests {
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Window {
        #[serde(default, with = "super::option")]
        path: Option<std::path::PathBuf>,
    }

    /// As eframe saves the workspace, in RON: a path that isn't UTF-8
    /// comes back as it was, and one written before as a string reads.
    #[test]
    fn paths_that_are_no_text_are_kept() {
        #[cfg(unix)]
        let odd: std::path::PathBuf = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(b"/tmp/caf\xe9.sql".to_vec()).into()
        };
        #[cfg(not(unix))]
        let odd = std::path::PathBuf::from("C:/tmp/café.sql");
        for path in [Some(odd), Some("/tmp/café.sql".into()), None] {
            let window = Window { path };
            let text = ron::ser::to_string_pretty(&window, Default::default()).unwrap();
            let back: Window = ron::from_str(&text).unwrap();
            assert_eq!(back, window, "{text}");
        }
        let before: Window = ron::from_str("(path: Some(\"/tmp/x.sql\"))").unwrap();
        assert_eq!(
            before.path.as_deref(),
            Some(std::path::Path::new("/tmp/x.sql"))
        );
    }
}
