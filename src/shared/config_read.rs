use std::{
    io::Read,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    config::{
        Config,
        ConfigFile,
        S2udioConfigFile,
        cli::Args,
        cli_config::{CliConfig, CliConfigFile},
        theme::{UiConfig, UiConfigFile},
    },
    shared::paths::{
        config_paths,
        legacy_config_dir,
        s2udio_config_dir,
        s2udio_config_path,
        theme_paths,
    },
};

#[derive(Error, Debug)]
pub enum ConfigReadError {
    #[error("Deserialization error, {0}")]
    Deserialization(#[from] serde_path_to_error::Error<ron::Error>),
    #[error("Failed to deserialize ron config file, {0}")]
    Ron(#[from] ron::error::SpannedError),
    #[error("Configuration file not found at any of the possible paths")]
    ConfigNotFound,
    #[error("Theme file not found at any of the possible paths")]
    ThemeNotFound,
    #[error("IO error, {0}")]
    Io(#[from] std::io::Error),
    #[error("No configuration paths available")]
    NoConfigPaths,
    #[error("{0:?}")]
    Conversion(#[from] anyhow::Error),
}

pub fn read_cli_config(
    cli_arg_config_path: Option<&Path>,
    cli_arg_address: Option<String>,
    cli_arg_password: Option<String>,
) -> Result<CliConfig, ConfigReadError> {
    let chosen_config_path = resolve_config_path(cli_arg_config_path)?;

    let config = read_cli_config_file(&chosen_config_path)?;

    Ok(config.into_config(cli_arg_address, cli_arg_password))
}

pub fn read_config_for_debuginfo(
    cli_arg_config_path: Option<&Path>,
    cli_arg_address: Option<String>,
    cli_arg_password: Option<String>,
) -> Result<(ConfigFile, Config, PathBuf), ConfigReadError> {
    let chosen_config_path = resolve_config_path(cli_arg_config_path)?;

    let config = read_config_file(&chosen_config_path)?;

    Ok((
        config.clone(),
        config.into_config(UiConfig::default(), cli_arg_address, cli_arg_password, false)?,
        chosen_config_path,
    ))
}

pub struct ConfigResult {
    pub config: Config,
    pub config_path: Option<PathBuf>,
}

pub fn read_config_and_theme(args: &mut Args) -> Result<ConfigResult, ConfigReadError> {
    if args.clean {
        return Ok(ConfigResult {
            config: Config::default_with_album_art_check()?,
            config_path: None,
        });
    }

    let chosen_config_path = resolve_config_path(args.config.as_deref())?;

    let config = read_config_file(&chosen_config_path)?;

    let theme = match &config.theme {
        Some(theme_name) => {
            let theme_paths = theme_paths(args.theme.as_deref(), &chosen_config_path, theme_name);
            let chosen_theme_path = find_first_existing_path(theme_paths);

            if let Some(theme_path) = chosen_theme_path {
                read_theme_file(&theme_path)?
            } else {
                return Err(ConfigReadError::ThemeNotFound);
            }
        }
        // No theme set in the config file, this is OK, use the default theme
        None => UiConfigFile::default(),
    };

    let theme = theme.try_into().map_err(ConfigReadError::Conversion)?;

    Ok(ConfigResult {
        config: config
            .into_config(theme, args.address.take(), args.password.take(), false)
            .map_err(ConfigReadError::Conversion)?,
        config_path: Some(chosen_config_path),
    })
}

pub fn read_cli_config_file(path: &Path) -> Result<CliConfigFile, ConfigReadError> {
    let file = std::fs::File::open(path)?;
    let mut read = std::io::BufReader::new(file);
    let mut buf = Vec::new();
    read.read_to_end(&mut buf)?;

    Ok(serde_path_to_error::deserialize(&mut ron::de::Deserializer::from_bytes(&buf)?)?)
}

pub fn read_config_file(path: &Path) -> Result<ConfigFile, ConfigReadError> {
    let file = std::fs::File::open(path)?;
    let mut read = std::io::BufReader::new(file);
    let mut buf = Vec::new();
    read.read_to_end(&mut buf)?;

    Ok(serde_path_to_error::deserialize(&mut ron::de::Deserializer::from_bytes(&buf)?)?)
}

/// Read the s2udio-only config overlay (`~/.config/s2udio/config.ron`),
/// if it exists. The overlay carries the s2udio feature sections
/// (`radio`/`jellyfin`/`video`/`mpv`/`torrent`); when absent, `None` is
/// returned and the base rmpc config's sections (or defaults) apply
/// (round 19).
pub fn read_s2udio_config_overlay() -> Result<Option<S2udioConfigFile>, ConfigReadError> {
    let Some(path) = s2udio_config_path() else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let file = std::fs::File::open(&path)?;
    let mut read = std::io::BufReader::new(file);
    let mut buf = Vec::new();
    read.read_to_end(&mut buf)?;
    let overlay: S2udioConfigFile =
        serde_path_to_error::deserialize(&mut ron::de::Deserializer::from_bytes(&buf)?)?;
    log::debug!(path:?; "Loaded the s2udio config overlay");
    Ok(Some(overlay))
}

/// True when the file at `path` looks like a round-19 overlay: its top
/// level carries only the five s2udio sections (`radio`, `jellyfin`,
/// `video`, `mpv`, `torrent`) and no base-config field. A genuine full
/// config always has at least `address` and more keys.
fn looks_like_overlay(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else { return false };
    let Ok(ron::Value::Map(map)) = ron::from_str::<ron::Value>(&content) else {
        return false;
    };
    !map.is_empty()
        && map.keys().all(|k| {
            matches!(k, ron::Value::String(s) if matches!(s.as_str(), "radio" | "jellyfin" | "video" | "mpv" | "torrent"))
        })
}

/// Resolve the config path to use, running the one-time round-23
/// migration from the legacy `~/.config/rmpc` layout when needed.
///
/// The target layout is a single full config at
/// `~/.config/s2udio/config.ron`. When only the legacy base config
/// (and/or the round-19 `~/.config/s2udio/config.ron` overlay) exists,
/// the merged full config is written to the new path and the legacy
/// sidecars/themes are copied over.
pub fn resolve_config_path(
    cli_arg_config_path: Option<&Path>,
) -> Result<PathBuf, ConfigReadError> {
    if let Some(path) = cli_arg_config_path {
        return Ok(path.to_path_buf());
    }

    let candidates = config_paths(None);
    if candidates.is_empty() {
        return Err(ConfigReadError::NoConfigPaths);
    }
    let s2udio_path = candidates[0].clone();

    // A round-19 overlay file parses fine as a full ConfigFile (every
    // field defaults), so classify by the file's top-level keys: an
    // overlay carries only the five s2udio sections.
    let s2udio_is_overlay = looks_like_overlay(&s2udio_path);
    let s2udio_is_full =
        s2udio_path.exists() && !s2udio_is_overlay && read_config_file(&s2udio_path).is_ok();

    // Fast path: the full config already lives in ~/.config/s2udio.
    if s2udio_is_full {
        return Ok(s2udio_path);
    }

    // Migration: legacy base (+ optional round-19 overlay) -> full config.
    let legacy_base = candidates.iter().skip(1).find(|p| p.exists()).cloned();
    // A broken overlay (e.g. `#` comments or missing `Some(...)`, which ron
    // rejects) must not block startup: it was warned-and-ignored before,
    // so the migration treats it the same way.
    let overlay = if s2udio_is_overlay {
        match read_s2udio_config_overlay() {
            Ok(overlay) => overlay,
            Err(err) => {
                log::warn!(error:? = err; "Failed to read the s2udio config overlay (round-23 migration)");
                None
            }
        }
    } else {
        None
    };
    if legacy_base.is_none() && overlay.is_none() {
        return Err(ConfigReadError::ConfigNotFound);
    }

    let mut config = match &legacy_base {
        Some(path) => read_config_file(path)?,
        None => ConfigFile::default(),
    };
    if let Some(overlay) = overlay {
        overlay.merge_into(&mut config);
    }

    if let Some(parent) = s2udio_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = ron::ser::to_string_pretty(&config, ron::ser::PrettyConfig::default())
        .map_err(|err| ConfigReadError::Conversion(anyhow::anyhow!("{err}")))?;
    std::fs::write(&s2udio_path, content)?;
    log::info!(path:? = s2udio_path; "Migrated the config to ~/.config/s2udio");

    // Copy legacy sidecars and themes that the new dir does not have yet.
    migrate_sidecars_and_themes();

    // The legacy base file is deliberately LEFT in place (upstream rmpc
    // may still read ~/.config/rmpc/config.ron). The merge is idempotent:
    // once the s2udio file is a full config (not overlay-only) the fast
    // path above wins and the legacy file is never touched again.
    Ok(s2udio_path)
}

/// Copy the legacy `~/.config/rmpc` sidecars (`state.ron`,
/// `keybinds.ron`, `cava.ron`, `jellyfin.ron`) and `themes/` into
/// `~/.config/s2udio` when the new location does not have them yet
/// (one-time migration; legacy files are left untouched and are only
/// read as a fallback when the new file is absent).
fn migrate_sidecars_and_themes() {
    let Some(new_dir) = s2udio_config_dir() else { return };
    let Some(legacy_dir) = legacy_config_dir() else { return };
    if !legacy_dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&legacy_dir) {
        for entry in entries.flatten() {
            let from = entry.path();
            let name = from.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if !matches!(name.as_str(), "state.ron" | "keybinds.ron" | "cava.ron" | "jellyfin.ron" | "themes") {
                continue;
            }
            let to = new_dir.join(&name);
            if to.exists() {
                continue;
            }
            let copied = if from.is_dir() {
                let mut ok = std::fs::create_dir_all(&to).is_ok();
                if ok && let Ok(inner) = std::fs::read_dir(&from) {
                    for inner_entry in inner.flatten() {
                        let dst = to.join(inner_entry.file_name());
                        if inner_entry.path().is_dir() {
                            // shallow copy for the themes dir (one level)
                            ok = std::fs::create_dir_all(&dst).is_ok();
                            if ok && let Ok(deep) = std::fs::read_dir(inner_entry.path()) {
                                for file in deep.flatten() {
                                    if std::fs::copy(file.path(), dst.join(file.file_name())).is_err() {
                                        ok = false;
                                    }
                                }
                            }
                        } else if std::fs::copy(inner_entry.path(), dst).is_err() {
                            ok = false;
                        }
                    }
                }
                ok
            } else {
                std::fs::copy(&from, &to).is_ok()
            };
            if copied {
                log::info!(from:? = from; "Copied legacy sidecar/theme into ~/.config/s2udio");
            }
        }
    }
}

pub fn read_theme_file(path: &Path) -> Result<UiConfigFile, ConfigReadError> {
    let file = std::fs::File::open(path)?;
    let mut read = std::io::BufReader::new(file);
    let mut buf = Vec::new();
    read.read_to_end(&mut buf)?;

    Ok(serde_path_to_error::deserialize(&mut ron::de::Deserializer::from_bytes(&buf)?)?)
}

pub fn find_first_existing_path(paths: Vec<PathBuf>) -> Option<PathBuf> {
    paths.into_iter().find(|path| path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::env::ENV;

    /// Round 23 one-time migration: a legacy `~/.config/rmpc/config.ron`
    /// base plus the round-19 `~/.config/s2udio/config.ron` overlay merge
    /// into a single full `~/.config/s2udio/config.ron`; the legacy base
    /// is left in place (upstream rmpc), and the sidecars are copied over.
    #[test]
    fn migration_merges_legacy_base_and_overlay_into_s2udio_config() {
        let _home_guard = crate::tests::fixtures::HOME_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("s2u-migrate-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        ENV.set("HOME".to_string(), home.to_string_lossy().into_owned());

        let rmpc_dir = home.join(".config/rmpc");
        let s2udio_dir = home.join(".config/s2udio");
        std::fs::create_dir_all(&rmpc_dir).unwrap();
        std::fs::create_dir_all(&s2udio_dir).unwrap();

        // Legacy base: a base-only setting (address) so the merge is
        // observable.
        std::fs::write(
            rmpc_dir.join("config.ron"),
            "(address: \"1.2.3.4:9999\",)\n",
        )
        .unwrap();
        // Round-19 overlay: the s2udio-only sections at the old overlay
        // path (same filename as the future full config).
        std::fs::write(
            s2udio_dir.join("config.ron"),
            "(radio: Some((playlist: Some(\"radio\"), location: None, max_favourites: Some(10))),)\n",
        )
        .unwrap();
        // A legacy sidecar that the new dir lacks.
        std::fs::write(rmpc_dir.join("keybinds.ron"), "(global: {\"q\": Quit},)\n").unwrap();

        let resolved = resolve_config_path(None).expect("migration resolves a config");
        assert_eq!(resolved, s2udio_dir.join("config.ron"));

        let config = read_config_file(&resolved).unwrap();
        assert_eq!(config.address, "1.2.3.4:9999", "base setting survives");
        assert_eq!(
            config.radio.playlist.as_deref(),
            Some("radio"),
            "overlay section merged"
        );

        // The legacy base is left for upstream rmpc; the sidecar was
        // copied into the new dir.
        assert!(rmpc_dir.join("config.ron").exists());
        assert!(s2udio_dir.join("keybinds.ron").exists());

        // A second resolve is the idempotent fast path.
        assert_eq!(resolve_config_path(None).unwrap(), s2udio_dir.join("config.ron"));

        ENV.remove("HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
