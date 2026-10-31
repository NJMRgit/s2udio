//! `~/.config/s2udio/daemon.ron` — the settings that belong to the always-on
//! daemon rather than to the TUI.
//!
//! The daemon starts before (or without) a TUI, so it cannot read the TUI's
//! in-memory settings. The one daemon-side preference today is the QAT
//! hotkey; the Settings panel writes this sidecar, the daemon reads it at
//! startup, and the running daemon is updated over its socket.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The chord used when neither the file nor the environment says otherwise.
pub const DEFAULT_QAT_HOTKEY: &str = "Meta+T";

/// Daemon preferences, persisted as RON. Missing fields fall back to their
/// defaults so a hand-written or older file still loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfigFile {
    /// The global shortcut that toggles the QAT, in KWin chord syntax
    /// (`Meta+T`, `Meta+Shift+F12`, …).
    pub qat_hotkey: String,
}

impl Default for DaemonConfigFile {
    fn default() -> Self {
        Self {
            qat_hotkey: DEFAULT_QAT_HOTKEY.to_owned(),
        }
    }
}

/// Path of the daemon sidecar (`~/.config/s2udio/daemon.ron`).
pub fn daemon_config_path() -> PathBuf {
    crate::shared::paths::s2udio_config_dir()
        .unwrap_or_else(|| crate::config::utils::tilde_expand_path(Path::new("~/.config/s2udio")))
        .join("daemon.ron")
}

impl DaemonConfigFile {
    /// Read the sidecar; a missing or unparsable file is the default (the
    /// daemon must start even if the user hand-edited it badly).
    pub fn load() -> Self {
        load_from(&daemon_config_path())
    }

    /// Write the sidecar, creating `~/.config/s2udio` when needed.
    pub fn save(&self) -> Result<()> {
        save_to(self, &daemon_config_path())
    }
}

fn load_from(path: &Path) -> DaemonConfigFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| ron::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_to(config: &DaemonConfigFile, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let text = ron::ser::to_string_pretty(config, ron::ser::PrettyConfig::default())
        .context("cannot serialize the daemon settings")?;
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

/// Trim a chord and reject values that would break the generated KWin script
/// or are empty. Returns the normalized chord.
pub fn normalize_hotkey(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.contains(['\n', '\r', '"']) {
        return None;
    }
    Some(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_hotkey_is_meta_t() {
        assert_eq!(DaemonConfigFile::default().qat_hotkey, "Meta+T");
    }

    #[test]
    fn the_sidecar_round_trips() {
        let config = DaemonConfigFile {
            qat_hotkey: "Meta+Shift+F12".to_owned(),
        };
        let text = ron::ser::to_string_pretty(&config, ron::ser::PrettyConfig::default())
            .expect("serialize");
        let parsed: DaemonConfigFile = ron::from_str(&text).expect("parse");
        assert_eq!(parsed, config);
        assert!(
            ron::from_str::<DaemonConfigFile>("()").is_ok(),
            "an empty file must fall back to defaults, not fail"
        );
    }

    #[test]
    fn saving_and_loading_uses_the_file() {
        let dir = std::env::temp_dir().join(format!("s2u-daemon-conf-{}", std::process::id()));
        let path = dir.join("daemon.ron");
        let config = DaemonConfigFile {
            qat_hotkey: "Meta+Shift+T".to_owned(),
        };
        save_to(&config, &path).expect("save");
        assert_eq!(load_from(&path), config);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_chord_is_rejected() {
        assert_eq!(normalize_hotkey("  Meta+T  ").as_deref(), Some("Meta+T"));
        assert!(normalize_hotkey("").is_none());
        assert!(normalize_hotkey("   ").is_none());
        assert!(normalize_hotkey("Meta+\"T").is_none());
        assert!(normalize_hotkey("Meta+\nT").is_none());
    }
}
