use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    config::UiSettings,
    shared::paths::{legacy_config_dir, s2udio_config_dir},
};

/// Small runtime state persisted to `~/.config/s2udio/state.ron` (round 19
/// — s2udio-only runtime state moved out of `~/.config/rmpc/` so the base
/// rmpc config stays pure) so the app can restore things like the last
/// active tab across restarts. The main config file is never rewritten.
pub const STATE_FILE: &str = "state.ron";

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppStateFile {
    /// The last tab that was shown (written on every tab change and on
    /// startup after the initial tab is chosen).
    pub last_tab: Option<String>,
    /// MPD update/rescan scope path set from the settings panel (empty =
    /// the whole library).
    pub mpd_library_path: Option<String>,
    /// The video playback preference ("ask" / "mpv" / "mpd") chosen in the
    /// settings panel; overrides the config default when present.
    pub video_playback: Option<String>,
    /// mpv audio language preference ("en" / "") chosen in the settings
    /// panel; overrides the config default when present.
    pub mpv_audio_lang: Option<String>,
    /// mpv subtitle preference ("signs" / "off") chosen in the settings
    /// panel; overrides the config default when present.
    pub mpv_subtitles: Option<String>,
    /// The "svp support" toggle (Settings -> mpv): mpv gets
    /// `--input-ipc-server=/tmp/mpvsocket`; overrides the config default
    /// when present.
    pub mpv_svp: Option<bool>,
    /// The settings panel's UI toggles (album art / lyrics / cava / radio /
    /// jellyfin tabs + auto-chapters): runtime-only in the config schema,
    /// persisted here so a restart keeps them.
    pub ui: Option<UiSettings>,
    /// Appearance colors from the settings panel, in
    /// `AppearanceTarget::all()` order: a hex string, "" for transparent,
    /// absent = leave the theme's default.
    pub appearance: Option<Vec<Option<String>>>,
    /// The rqbit SOCKS5 proxy URL (Settings -> torrent), e.g.
    /// `socks5://127.0.0.1:1080`; "" = explicitly no proxy, absent = keep
    /// the config default.
    pub torrent_socks_proxy: Option<String>,
}

impl AppStateFile {
    /// The s2udio state path (`~/.config/s2udio/state.ron`). Round 19:
    /// s2udio's runtime state lives in its own config dir; a pre-round-19
    /// file at `~/.config/rmpc/state.ron` is still read (migration) until
    /// the new location has one.
    pub fn path() -> Option<PathBuf> {
        s2udio_config_dir().map(|dir| dir.join(STATE_FILE))
    }

    /// The legacy pre-round-23 state path (`~/.config/rmpc/state.ron`).
    fn legacy_path() -> Option<PathBuf> {
        legacy_config_dir().map(|dir| dir.join(STATE_FILE))
    }

    /// Read the state file; missing or unparsable state falls back to
    /// defaults (never an error). Falls back to the legacy
    /// `~/.config/rmpc/state.ron` when the s2udio file is absent.
    pub fn load() -> Self {
        let path =
            Self::path().filter(|p| p.exists()).or_else(Self::legacy_path).or_else(Self::path);
        let Some(path) = path else { return Self::default() };
        match std::fs::read_to_string(path) {
            Ok(content) => ron::de::from_str(&content).unwrap_or_else(|err| {
                log::warn!("Failed to parse state file: {err}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Persist the state file (ignores missing config dir gracefully).
    pub fn save(&self) -> Result<()> {
        let Some(path) = Self::path() else {
            bail!("Could not determine config directory");
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())?;
        std::fs::write(&path, content)?;
        Ok(())
    }
}
