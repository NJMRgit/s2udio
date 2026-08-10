use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{defaults, utils::tilde_expand_path};

/// Jellyfin integration configuration.
///
/// The Jellyfin tab connects to the same server/account as the `jellytui`
/// TUI client and reads its credentials (`server_url`, `access_token`,
/// `user_id`) from jellytui's config file, so there is no second place to
/// type the password. The file location can be overridden here.
#[derive(Debug, Clone, PartialEq)]
pub struct Jellyfin {
    /// Path to jellytui's `config.toml` (server URL + token + user id).
    pub config_file: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct JellyfinFile {
    pub config_file: Option<String>,
}


/// Jellyfin credentials written by the Settings panel
/// (`~/.config/s2udio/jellyfin.ron`, round 19 — s2udio-only sidecar moved
/// out of `~/.config/rmpc/`). When present it overrides jellytui's
/// config file; the password is never stored, only the session token.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JellyfinCredentialsFile {
    pub server_url: String,
    pub access_token: String,
    pub user_id: String,
}

/// Path of the Settings-panel sidecar holding the Jellyfin credentials.
/// Writes go to `~/.config/s2udio/jellyfin.ron`; the legacy
/// `~/.config/rmpc/jellyfin.ron` is still read (migration) when the new
/// file is absent. This returns the path that EXISTS (new preferred,
/// legacy fallback, else the new location) so readers pick up a
/// pre-round-19 sidecar automatically.
pub fn jellyfin_sidecar_path() -> PathBuf {
    let new = crate::shared::paths::s2udio_config_dir()
        .unwrap_or_else(|| tilde_expand_path(std::path::Path::new("~/.config/s2udio")))
        .join("jellyfin.ron");
    if new.exists() {
        return new;
    }
    let legacy = legacy_jellyfin_sidecar_path();
    if legacy.exists() {
        return legacy;
    }
    new
}

/// The s2udio write path (`~/.config/s2udio/jellyfin.ron`): where the
/// Settings panel persists credentials (round 19).
pub fn jellyfin_sidecar_write_path() -> PathBuf {
    crate::shared::paths::s2udio_config_dir()
        .unwrap_or_else(|| tilde_expand_path(std::path::Path::new("~/.config/s2udio")))
        .join("jellyfin.ron")
}

/// The legacy pre-round-19 Jellyfin sidecar path
/// (`~/.config/rmpc/jellyfin.ron`).
pub fn legacy_jellyfin_sidecar_path() -> PathBuf {
    crate::shared::paths::legacy_config_dir()
        .unwrap_or_else(|| tilde_expand_path(std::path::Path::new("~/.config/rmpc")))
        .join("jellyfin.ron")
}

impl From<JellyfinFile> for Jellyfin {
    fn from(value: JellyfinFile) -> Self {
        let config_file = value
            .config_file
            .map(|p| tilde_expand_path(std::path::Path::new(&p)))
            .unwrap_or_else(|| {
                tilde_expand_path(std::path::Path::new(&defaults::default_jellyfin_config_file()))
            });
        Self { config_file }
    }
}
