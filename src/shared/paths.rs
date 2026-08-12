use std::path::{Path, PathBuf};

use crate::{config::utils::tilde_expand, shared::env::ENV};

#[cfg(debug_assertions)]
const CONFIG_NAME: &str = "config.debug.ron";
#[cfg(not(debug_assertions))]
const CONFIG_NAME: &str = "config.ron";
// The app is s2udio; all configs live in ~/.config/s2udio (round 23).
const S2UDIO_CONFIG_NAME: &str = "s2udio";
// Legacy pre-round-23 location (~/.config/rmpc): read-only fallback so a
// one-time migration on first run moves the base config, sidecars and
// themes into ~/.config/s2udio (never written from the new layout).
const LEGACY_CONFIG_NAME: &str = "rmpc";

pub fn home_dir() -> Option<PathBuf> {
    ENV.var_os("HOME")
        .and_then(|home| if home.is_empty() { None } else { Some(home) })
        .map(PathBuf::from)
}

/// The config dir (`~/.config/s2udio`): every s2udio config — the base
/// `config.ron`, the sidecars (`state.ron`, `keybinds.ron`, `cava.ron`,
/// `jellyfin.ron`) and `themes/` (round 23: nothing lives in
/// `~/.config/rmpc` anymore).
pub fn config_dir() -> Option<PathBuf> {
    ENV.var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .map(|p| p.join(S2UDIO_CONFIG_NAME))
}

/// Alias kept for callers that named the s2udio dir explicitly before the
/// round-23 unification (`config_dir()` now returns the same path).
pub fn s2udio_config_dir() -> Option<PathBuf> {
    config_dir()
}

/// The legacy pre-round-23 config dir (`~/.config/rmpc`): read-only
/// migration fallback (sidecars/themes) — the app never writes here.
pub fn legacy_config_dir() -> Option<PathBuf> {
    ENV.var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .map(|p| p.join(LEGACY_CONFIG_NAME))
}

/// The s2udio-only cache dir (`~/.cache/s2udio`): s2udio runtime data
/// (video playlist, mpv MPRIS state, MPRIS art) — separate from rmpc's
/// cache so stream/video playlists never collide with rmpc/MPD state
/// (round 19).
pub fn s2udio_cache_dir() -> Option<PathBuf> {
    ENV.var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".cache")))
        .map(|p| p.join(S2UDIO_CONFIG_NAME))
}

/// The s2udio config path: `~/.config/s2udio/config.ron` — the single,
/// full config file (round 23; the round-19 `~/.config/s2udio/config.ron`
/// overlay is consumed by the one-time migration and retired).
pub fn s2udio_config_path() -> Option<PathBuf> {
    s2udio_config_dir().map(|dir| dir.join(CONFIG_NAME))
}

pub fn config_paths(cli_arg_config_path: Option<&Path>) -> Vec<PathBuf> {
    if let Some(path) = cli_arg_config_path {
        return vec![path.to_path_buf()];
    }

    let mut result = Vec::new();
    match config_dir() {
        Some(config_dir) => result.push(config_dir.join(CONFIG_NAME)),
        None => log::warn!("Could not determine configuration directory"),
    }

    // Legacy pre-round-23 locations: read-only migration fallbacks.
    if let Some(legacy_dir) = legacy_config_dir() {
        result.push(legacy_dir.join(CONFIG_NAME));
    }
    if let Some(home) = home_dir() {
        result.push(home.join(LEGACY_CONFIG_NAME).join(CONFIG_NAME));
    }

    result
}

pub fn theme_paths(
    cli_arg_theme: Option<&Path>,
    config_path: &Path,
    theme_name: &str,
) -> Vec<PathBuf> {
    if let Some(path) = cli_arg_theme {
        return vec![path.to_path_buf()];
    }

    let config_dir = config_path.parent().unwrap_or_else(|| {
        panic!("Expected config path to have parent directory. Path: '{}'", config_path.display())
    });

    // Round 23: themes live in `~/.config/s2udio/themes/` and are resolved
    // FIRST; the legacy pre-round-23 dirs (`~/.config/rmpc/themes/…` and
    // `~/rmpc/themes/…`) are read-only fallbacks so a pre-migration theme
    // keeps working. The config path's own dir is kept as a candidate for
    // a theme colocated with a custom `--config` path.
    let mut paths = Vec::new();
    if let Some(s2udio_dir) = s2udio_config_dir() {
        paths.push(s2udio_dir.join("themes").join(format!("{theme_name}.ron")));
        paths.push(s2udio_dir.join("themes").join(theme_name));
    }
    if let Some(legacy_dir) = legacy_config_dir() {
        paths.push(legacy_dir.join("themes").join(format!("{theme_name}.ron")));
        paths.push(legacy_dir.join("themes").join(theme_name));
        paths.push(legacy_dir.join(format!("{theme_name}.ron")));
        paths.push(legacy_dir.join(theme_name));
    }
    paths.push(config_dir.join("themes").join(format!("{theme_name}.ron")));
    paths.push(config_dir.join("themes").join(theme_name));
    paths.push(config_dir.join(format!("{theme_name}.ron")));
    paths.push(config_dir.join(theme_name));
    paths.push(PathBuf::from(tilde_expand(theme_name).into_owned()));
    paths
}

/// Round 29: the LD_PRELOAD shim that renames cava's PipeWire node.
/// Installed by `setup.sh` into `~/.local/share/s2udio/libcavaname.so`
/// (built from `scripts/cava-node-name.c`); `S2UDIO_CAVA_NAME_SHIM`
/// overrides the location. Returns `None` when the shim does not exist.
pub fn cava_node_name_shim() -> Option<PathBuf> {
    let path = std::env::var_os("S2UDIO_CAVA_NAME_SHIM")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".local/share/s2udio/libcavaname.so")))?;
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_paths_prefer_the_s2udio_theme_dir() {
        let _home_guard = crate::tests::fixtures::HOME_LOCK.lock().unwrap();
        // Round 23: s2udio's own themes resolve from ~/.config/s2udio/
        // FIRST; the legacy ~/.config/rmpc/ dir is only a read-only
        // fallback for the one-time migration.
        use crate::shared::env::ENV;
        let home = std::env::temp_dir().join(format!("s2u-theme-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        ENV.set("HOME".to_string(), home.to_string_lossy().into_owned());

        let base_config = home.join(".config/s2udio/config.ron");
        let paths = theme_paths(None, &base_config, "default");
        assert_eq!(
            paths[0],
            home.join(".config/s2udio/themes/default.ron"),
            "s2udio theme dir is the first candidate"
        );
        assert_eq!(
            paths[2],
            home.join(".config/rmpc/themes/default.ron"),
            "legacy rmpc theme dir is the migration fallback"
        );
        assert_eq!(paths[0].to_string_lossy().contains(".config/s2udio"), true);

        ENV.remove("HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
