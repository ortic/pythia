//! Settings, saved connection profiles and on-disk locations.
//!
//! Everything is stored as TOML in the platform config directory
//! (`~/.config/pythia` on Linux). Passwords are never written here; they will
//! go to the OS keychain (CONCEPT.md §5.2).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};
use serde::{Serialize, de::DeserializeOwned};

mod connections;
mod settings;
pub mod tns;

pub use connections::{ConnectionProfile, Connections, DbKind, Privilege, SshTunnel};
pub use settings::{
    AiBackend, AiKind, AiSettings, AppearanceSettings, CursorShape, Density, EditorSettings,
    FormatTriggers, GridSettings, InvalidObjectSettings, InvalidScope, KeymapPreset, ScrollBars,
    Settings, ThemeChoice,
};

/// Platform directories used by Pythia.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn detect() -> Result<Self> {
        Self::for_app("pythia")
    }

    /// Carries over settings, connections and the workspace from before the
    /// app was renamed from orui to Pythia. Call before anything creates the
    /// new directories. Returns what couldn't be moved.
    pub fn adopt_legacy(&self) -> Vec<String> {
        let Ok(legacy) = Self::for_app("orui") else {
            return Vec::new();
        };
        [
            (&legacy.config_dir, &self.config_dir),
            (&legacy.data_dir, &self.data_dir),
        ]
        .into_iter()
        .filter_map(|(from, to)| move_if_absent(from, to).err())
        .collect()
    }

    fn for_app(name: &str) -> Result<Self> {
        let dirs = directories::ProjectDirs::from("com", "Ortic", name)
            .context("could not determine a home directory")?;
        Ok(Self {
            config_dir: dirs.config_dir().to_owned(),
            data_dir: dirs.data_dir().to_owned(),
        })
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.toml")
    }

    pub fn connections_file(&self) -> PathBuf {
        self.config_dir.join("connections.toml")
    }

    /// SSH host keys trusted in Pythia (besides `~/.ssh/known_hosts`).
    pub fn known_hosts_file(&self) -> PathBuf {
        self.config_dir.join("known_hosts")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    /// Autosaved editor content for crash recovery.
    pub fn recovery_dir(&self) -> PathBuf {
        self.data_dir.join("recovery")
    }
}

/// Moves the directory `from` to `to` unless `to` already exists. On
/// failure the app starts with fresh directories.
fn move_if_absent(from: &Path, to: &Path) -> Result<(), String> {
    if to.exists() || !from.is_dir() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::rename(from, to).map_err(|e| format!("moving {} to {}: {e}", from.display(), to.display()))
}

/// Loads a TOML file, returning the default value if it doesn't exist.
pub fn load_toml<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes a TOML file atomically (write to a temp file, then rename).
pub fn save_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = toml::to_string_pretty(value)?;
    // A temp file of its own: another Pythia, or another thread, may save
    // the same file at the same time (#196).
    static SAVES: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "toml.{}-{}.tmp",
        std::process::id(),
        SAVES.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("pythia-config-test-{}", std::process::id()));
        let path = dir.join("settings.toml");
        let mut settings = Settings::default();
        settings.editor.font_size = 15.0;
        save_toml(&path, &settings).unwrap();
        let loaded: Settings = load_toml(&path).unwrap();
        assert_eq!(loaded, settings);
        assert_eq!(
            load_toml::<Settings>(&dir.join("missing.toml")).unwrap(),
            Settings::default()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    /// Saves at the same time don't share a temp file (#196): with one,
    /// they wrote over each other's, and a rename failed.
    #[test]
    fn saves_at_once() {
        let dir = std::env::temp_dir().join(format!("pythia-config-saves-{}", std::process::id()));
        let path = dir.join("settings.toml");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut settings = Settings::default();
                    settings.editor.tab_width = i;
                    for _ in 0..50 {
                        save_toml(&path, &settings).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let back: Settings = load_toml(&path).unwrap();
        assert!(back.editor.tab_width < 8);
        let files: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1, "{files:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    /// Settings from before the appearance options keep today's look; the
    /// new ones survive a round trip.
    #[test]
    fn appearance_settings() {
        let old: Settings = toml::from_str("theme = \"dark\"\nui_scale = 1.25\n").unwrap();
        assert_eq!(old.appearance, AppearanceSettings::default());
        assert_eq!(old.appearance.density, Density::COMFORTABLE);
        assert_eq!(old.grid, GridSettings::default());
        assert_eq!(old.editor.cursor_shape, CursorShape::Line);

        let mut settings = Settings::default();
        settings.appearance.font_file = "/fonts/Inter.ttf".into();
        settings.appearance.density = Density::COMPACT;
        settings.appearance.accent = Some([0x10, 0x80, 0x40]);
        settings.appearance.scroll_bars = ScrollBars::Visible;
        settings.grid.null_text = "(null)".into();
        settings.grid.null_color = Some([1, 2, 3]);
        settings.editor.cursor_shape = CursorShape::Block;
        settings.editor.line_spacing = 2.0;
        let text = toml::to_string_pretty(&settings).unwrap();
        assert!(text.contains("scroll_bars = \"visible\""), "{text}");
        assert!(text.contains("cursor_shape = \"block\""), "{text}");
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);

        // A partial density keeps the other values.
        let partial: Settings = toml::from_str("[appearance.density]\npadding = 1.0\n").unwrap();
        assert_eq!(
            partial.appearance.density,
            Density {
                padding: 1.0,
                ..Density::COMFORTABLE
            }
        );
    }

    #[test]
    fn legacy_dir_moves_only_when_new_is_absent() {
        let dir = std::env::temp_dir().join(format!("pythia-move-test-{}", std::process::id()));
        let old = dir.join("orui");
        let new = dir.join("nested").join("pythia");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("settings.toml"), "theme = \"light\"\n").unwrap();

        move_if_absent(&old, &new).unwrap();
        assert!(!old.exists());
        assert!(new.join("settings.toml").exists());

        fs::create_dir_all(&old).unwrap();
        move_if_absent(&old, &new).unwrap();
        assert!(old.exists(), "an existing target is left alone");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ai_settings() {
        let settings: Settings = toml::from_str(
            "[ai]\nsend_bind_values = true\n\n[[ai.backends]]\nid = \"b1\"\nname = \"Ollama\"\n\
             kind = \"local-server\"\nmodel = \"llama3.1\"\n",
        )
        .unwrap();
        let ai = &settings.ai;
        assert!(ai.send_bind_values && !ai.send_result_data);
        assert_eq!(ai.timeout_secs, 300);
        let backend = ai.default_backend().unwrap();
        assert_eq!(
            (backend.kind, backend.model.as_str()),
            (AiKind::LocalServer, "llama3.1")
        );
        assert!(ai.backend("nope").is_none());

        let mut settings = Settings::default();
        settings.ai.backends = vec![
            AiBackend::new(AiKind::Anthropic, "a".into()),
            AiBackend::new(AiKind::Cli, "c".into()),
        ];
        settings.ai.default_backend = "c".into();
        let text = toml::to_string_pretty(&settings).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back, settings);
        assert_eq!(back.ai.default_backend().unwrap().kind, AiKind::Cli);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let settings: Settings =
            toml::from_str("theme = \"light\"\n[editor]\ntab_width = 2\n").unwrap();
        assert_eq!(settings.theme, ThemeChoice::Light);
        assert_eq!(settings.editor.tab_width, 2);
        assert_eq!(
            settings.editor.font_size,
            EditorSettings::default().font_size
        );
        assert_eq!(settings.editor.completion_rows, 15);
    }
}
