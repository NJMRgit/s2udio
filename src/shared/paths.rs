use std::path::{Path, PathBuf};
use crate::{config::utils::tilde_expand, shared::env::ENV};
#[cfg(debug_assertions)]
const CONFIG_NAME: &str = "config.debug.ron";
#[cfg(not(debug_assertions))]
const CONFIG_NAME: &str = "config.ron";
const S2UDIO_CONFIG_NAME: &str = "s2udio";
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
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| {
            panic!(
                "Expected config path to have parent directory. Path: '{}'", config_path
                .display()
            )
        });
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
/// s2udio's own unmanaged prefix (`/opt/s2udio`; `S2UDIO_OPT_PREFIX`
/// overrides it). `setup.sh` installs the programs no package manager owns
/// there instead of `/usr/bin` or `/usr/local/bin`: the vendored upstream
/// python mpDris2 source and a cava built from source on the distros without
/// a patchable package (Alpine, NixOS), plus their python dependencies.
pub fn opt_prefix() -> PathBuf {
    ENV.var_os("S2UDIO_OPT_PREFIX")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/s2udio"))
}
/// A program in s2udio's own prefix (`/opt/s2udio/bin/<name>`).
pub fn opt_bin(name: &str) -> PathBuf {
    opt_prefix().join("bin").join(name)
}
/// Resolve a support program the way `setup.sh` installed it: PATH first (a
/// distro package, or the user-level `~/.local/bin` copy), then s2udio's own
/// prefix, else the bare name so the spawn error still names the program.
/// `S2UDIO_<PROGRAM>_BIN` (upper-cased, non-alphanumerics to `_`) overrides
/// the whole lookup — `S2UDIO_CAVA_BIN` for cava.
pub fn resolve_bin(name: &str) -> PathBuf {
    let override_var = format!(
        "S2UDIO_{}_BIN",
        name.to_uppercase().replace(|c: char| !c.is_alphanumeric(), "_")
    );
    if let Some(path) = ENV.var_os(&override_var).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Ok(path) = which::which(name) {
        return path;
    }
    let own = opt_bin(name);
    if own.is_file() {
        return own;
    }
    PathBuf::from(name)
}
