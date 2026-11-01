//! Files several Pythias share (#196). Each one has them in memory from its
//! start; writing them from there undid what another one saved since. A
//! change is applied to the file as it is now, settings write only what
//! changed, and the open windows are the first Pythia's.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use pythia_config::{Settings, load_toml, save_toml};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{error, info, warn};

/// Applies `change` to the file as it is now: reads it again, changes it,
/// writes it, and `mine` becomes what came of it. When the file can't be
/// read, `change` applies to `mine` as this Pythia has it.
pub fn change_toml<T>(path: Option<&Path>, mine: &mut T, change: impl FnOnce(&mut T))
where
    T: Serialize + DeserializeOwned + Default + Clone,
{
    let Some(path) = path else {
        change(mine);
        return;
    };
    let mut current = load_toml::<T>(path).unwrap_or_else(|e| {
        error!("{e:#}");
        mine.clone()
    });
    change(&mut current);
    if let Err(e) = save_toml(path, &current) {
        error!("saving {}: {e:#}", path.display());
    }
    *mine = current;
}

/// The settings to write: the file's (`file`), with the ones this Pythia
/// changed since `base` (what it read or wrote last) as `mine` has them.
pub fn merge_settings(base: &Settings, mine: &Settings, file: Option<toml::Table>) -> Settings {
    let (Ok(base), Ok(ours)) = (toml::Table::try_from(base), toml::Table::try_from(mine)) else {
        return mine.clone();
    };
    let Some(mut file) = file else {
        return mine.clone();
    };
    for (key, value) in &ours {
        if base.get(key) != Some(value) {
            file.insert(key.clone(), value.clone());
        }
    }
    for key in base.keys() {
        if !ours.contains_key(key) {
            file.remove(key);
        }
    }
    file.try_into().unwrap_or_else(|e| {
        warn!("merging the settings with the file's: {e}");
        mine.clone()
    })
}

/// Where the open windows are kept, and the lock that makes them this
/// Pythia's: the first one holds `pythia.lock` and keeps them in
/// `workspace.ron`; one started while it runs keeps its own as long as it
/// runs (`temporary`), so that it doesn't write over them.
pub struct Workspace {
    pub path: PathBuf,
    pub temporary: bool,
    _lock: Option<File>,
}

impl Workspace {
    pub fn claim(data_dir: &Path) -> Self {
        let shared = |lock| Self {
            path: data_dir.join("workspace.ron"),
            temporary: false,
            _lock: lock,
        };
        let _ = fs::create_dir_all(data_dir);
        let file = match File::create(data_dir.join("pythia.lock")) {
            Ok(file) => file,
            Err(e) => {
                warn!("no lock for the open windows: {e}");
                return shared(None);
            }
        };
        match file.try_lock() {
            Ok(()) => shared(Some(file)),
            Err(TryLockError::WouldBlock) => {
                info!("another Pythia is running: this one's windows are kept only while it runs");
                Self {
                    path: std::env::temp_dir()
                        .join(format!("pythia-workspace-{}.ron", std::process::id())),
                    temporary: true,
                    _lock: None,
                }
            }
            Err(TryLockError::Error(e)) => {
                warn!("no lock for the open windows: {e}");
                shared(None)
            }
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if self.temporary {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pythia_config::{ConnectionProfile, Connections};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pythia-shared-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn profile(name: &str) -> ConnectionProfile {
        ConnectionProfile {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Two Pythias add a connection each: both stay.
    #[test]
    fn changes_apply_to_the_file() {
        let dir = temp_dir("change");
        let path = dir.join("connections.toml");
        let mut one = Connections::default();
        let mut two = Connections::default();
        change_toml(Some(&path), &mut one, |c| c.upsert(profile("a")));
        change_toml(Some(&path), &mut two, |c| c.upsert(profile("b")));
        let names = |c: &Connections| {
            c.profiles
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&two), ["a", "b"]);
        change_toml(Some(&path), &mut one, |c| {
            c.profiles.retain(|p| p.name != "a");
        });
        assert_eq!(names(&one), ["b"]);
        assert_eq!(names(&load_toml::<Connections>(&path).unwrap()), ["b"]);
        // An unreadable file isn't replaced by just the change.
        fs::write(&path, "connection = 7").unwrap();
        change_toml(Some(&path), &mut one, |c| c.upsert(profile("c")));
        assert_eq!(names(&one), ["b", "c"]);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Only what this Pythia changed goes over the file's settings.
    #[test]
    fn settings_merge_what_changed() {
        let base = Settings::default();
        let mut mine = base.clone();
        mine.editor.tab_width = 2;
        let mut theirs = base.clone();
        theirs.ui_scale = 1.5;
        theirs.editor.tab_width = 8;
        let file = toml::Table::try_from(&theirs).unwrap();
        let merged = merge_settings(&base, &mine, Some(file.clone()));
        assert_eq!(merged.editor.tab_width, 2);
        assert_eq!(merged.ui_scale, 1.5);
        // Unchanged, the file's stay.
        assert_eq!(merge_settings(&base, &base, Some(file)), theirs);
        assert_eq!(merge_settings(&base, &mine, None), mine);
    }

    /// The first Pythia keeps the open windows; one started while it runs
    /// keeps its own, and removes them when it ends.
    #[test]
    fn the_first_pythia_keeps_the_windows() {
        let dir = temp_dir("workspace");
        let first = Workspace::claim(&dir);
        assert_eq!(first.path, dir.join("workspace.ron"));
        assert!(!first.temporary);
        let second = Workspace::claim(&dir);
        assert!(second.temporary);
        assert_ne!(second.path, first.path);
        fs::write(&second.path, "()").unwrap();
        let temporary = second.path.clone();
        drop(second);
        assert!(!temporary.exists());
        drop(first);
        assert!(!Workspace::claim(&dir).temporary);
        fs::remove_dir_all(dir).unwrap();
    }
}
