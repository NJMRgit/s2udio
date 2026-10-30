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
/// The s2udio-only state dir (`~/.local/state/s2udio`, XDG
/// `XDG_STATE_HOME`): things that must **outlive a restart** and are not
/// regenerable cache — today the log file. The log used to live in
/// `std::env::temp_dir()`, which is tmpfs on most systems and is truncated
/// on each launch, so a crash left no artifact at all (the round-111 report
/// "possible unbounded memory growth / silent death in long sessions" could
/// not be root-caused for exactly that reason). `S2UDIO_LOG_DIR` overrides
/// the whole directory so tests and the dev harness can point it at a
/// scratch dir without touching the real HOME.
pub fn s2udio_state_dir() -> Option<PathBuf> {
    if let Some(dir) = ENV.var_os("S2UDIO_LOG_DIR").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    ENV.var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".local/state")))
        .map(|p| p.join(S2UDIO_CONFIG_NAME))
}
/// The s2udio log file path (`~/.local/state/s2udio/s2udio_<uid>.log`).
/// `S2UDIO_LOG_FILE` overrides the path (takes precedence over
/// `S2UDIO_LOG_DIR`, so a single-file override wins over the directory).
/// Returns `None` only when neither an override nor a home dir can be
/// determined.
///
/// Note on the name on disk: with rotation enabled flexi_logger writes the
/// live file as `<stem>_rCURRENT.log` and renames it to `<stem>_<n>.log`
/// when it rotates. So `S2UDIO_LOG_FILE=/tmp/x/s2udio.log` produces
/// `/tmp/x/s2udio_rCURRENT.log` — the *stem* is what the override controls,
/// not the literal filename. Anything that needs to read the log after the
/// fact must account for that suffix.
pub fn s2udio_log_path() -> Option<PathBuf> {
    if let Some(path) = ENV.var_os("S2UDIO_LOG_FILE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let uid = rustix::process::geteuid();
    s2udio_state_dir().map(|dir| dir.join(format!("s2udio_{}.log", uid.as_raw())))
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

/// One s2udio cache **file**, honoring an explicit cache dir and the legacy
/// `~/.cache/rmpc` location when the new dir does not hold the file yet.
///
/// The paste pipeline had a private copy of this rule for `yt-info.json`;
/// the daemon's caretaker must read and write the *same* file, so the rule
/// lives here now. The legacy dir is a read-only fallback: a file missing
/// from both resolves to the new dir, which every writer then creates.
pub fn s2udio_cache_path(cache_dir: Option<&Path>, file: &str) -> PathBuf {
    if let Some(dir) = cache_dir {
        return dir.join(file);
    }
    let new = s2udio_cache_dir()
        .unwrap_or_else(|| tilde_expand("~/.cache/s2udio").into_owned().into())
        .join(file);
    let legacy: PathBuf = tilde_expand("~/.cache/rmpc").into_owned().into();
    if new.exists() {
        new
    } else if legacy.join(file).exists() {
        legacy.join(file)
    } else {
        new
    }
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

    // Round 111 — the report could not attach a crash report because the log
    // lived in `std::env::temp_dir()` (tmpfs: gone on reboot) and was
    // truncated on every launch. These three tests pin the replacement
    // contract: a persistent, XDG-derived location that a test or the dev
    // harness can redirect.

    #[test]
    fn state_dir_is_under_xdg_state_home_not_tmp() {
        let _home_guard = crate::tests::fixtures::HOME_LOCK.lock().unwrap();
        use crate::shared::env::ENV;
        // A home that is deliberately NOT under temp_dir(), so the
        // assertion below can tell "derived from HOME" apart from
        // "hardcoded to temp_dir()" — the bug the report hit.
        let home = std::path::PathBuf::from("/s2u-fake-home");
        ENV.set("HOME".to_string(), home.to_string_lossy().into_owned());
        ENV.remove("XDG_STATE_HOME");
        ENV.remove("S2UDIO_LOG_DIR");

        let dir = s2udio_state_dir().expect("state dir resolves from HOME");

        assert_eq!(dir, home.join(".local/state/s2udio"));
        assert!(
            !dir.starts_with(std::env::temp_dir()),
            "the log must be derived from HOME/XDG_STATE_HOME, not dropped in \
             temp_dir(): tmpfs means a crash artefact would not survive a \
             reboot — got {dir:?}"
        );

        let log = s2udio_log_path().expect("log path resolves");
        assert!(
            log.starts_with(home.join(".local/state/s2udio")),
            "the log file lives in the state dir, got {log:?}"
        );

        ENV.remove("HOME");
    }

    #[test]
    fn xdg_state_home_overrides_home() {
        let _home_guard = crate::tests::fixtures::HOME_LOCK.lock().unwrap();
        use crate::shared::env::ENV;
        ENV.set("HOME".to_string(), "/home/should-not-be-used".to_string());
        ENV.set("XDG_STATE_HOME".to_string(), "/xdg/state".to_string());
        ENV.remove("S2UDIO_LOG_DIR");

        assert_eq!(s2udio_state_dir(), Some(std::path::PathBuf::from("/xdg/state/s2udio")));

        // A relative XDG_STATE_HOME is not a valid XDG dir; fall back to HOME.
        ENV.set("XDG_STATE_HOME".to_string(), "relative/path".to_string());
        assert_eq!(s2udio_state_dir(), Some(std::path::PathBuf::from("/home/should-not-be-used/.local/state/s2udio")));

        ENV.remove("HOME");
        ENV.remove("XDG_STATE_HOME");
    }

    #[test]
    fn log_dir_and_file_overrides_win() {
        let _home_guard = crate::tests::fixtures::HOME_LOCK.lock().unwrap();
        use crate::shared::env::ENV;
        let scratch = std::env::temp_dir().join(format!("s2u-logdir-test-{}", std::process::id()));
        ENV.set("HOME".to_string(), "/home/ignored".to_string());

        // S2UDIO_LOG_DIR redirects the directory (the dev harness uses this
        // so it never writes into the operator's real state dir).
        ENV.set("S2UDIO_LOG_DIR".to_string(), scratch.to_string_lossy().into_owned());
        ENV.remove("S2UDIO_LOG_FILE");
        assert_eq!(s2udio_state_dir(), Some(scratch.clone()));
        let path = s2udio_log_path().expect("log path resolves");
        assert_eq!(path.parent(), Some(scratch.as_path()));
        assert!(
            path.file_name().unwrap().to_string_lossy().starts_with("s2udio_"),
            "log file name is uid-scoped, got {path:?}"
        );

        // S2UDIO_LOG_FILE is the more specific override, so it wins outright
        // over S2UDIO_LOG_DIR rather than being joined onto it.
        let explicit = scratch.join("exact.log");
        ENV.set("S2UDIO_LOG_FILE".to_string(), explicit.to_string_lossy().into_owned());
        assert_eq!(s2udio_log_path(), Some(explicit));

        ENV.remove("HOME");
        ENV.remove("S2UDIO_LOG_DIR");
        ENV.remove("S2UDIO_LOG_FILE");
    }
}
