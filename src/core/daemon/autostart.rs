//! Install/remove the systemd user unit that starts the daemon at login.
//!
//! The Advanced settings toggle drives this. The unit is deliberately
//! `Type=oneshot` around `s2udio daemon start` / `daemon stop`: the daemon is
//! detached (it must outlive the TUI and its own unit process), and `daemon
//! start` is idempotent, so enabling the unit while a TUI already started the
//! daemon is a no-op instead of a socket-bind failure. Disabling the toggle
//! only removes the login start — the running daemon is left alone; stop it
//! with `s2udio daemon stop`.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

pub(crate) const UNIT_NAME: &str = "s2udio-daemon.service";

/// Where the user unit lives: `$XDG_CONFIG_HOME/systemd/user/s2udio-daemon.service`
/// (default `~/.config/...`).
pub(crate) fn unit_path() -> PathBuf {
    let base = crate::shared::env::ENV
        .var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| crate::shared::paths::home_dir().map(|home| home.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    unit_path_in(&base)
}

fn unit_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("systemd").join("user").join(UNIT_NAME)
}

/// Whether the unit is installed — the settings row's initial value.
pub(crate) fn enabled() -> bool {
    unit_path().is_file()
}

/// Install + enable, or disable + remove, the unit. Disabling does not stop a
/// running daemon: the setting means "start at login".
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn set_enabled(enabled: bool) -> Result<()> {
    if enabled { install() } else { uninstall() }
}

#[cfg_attr(test, allow(dead_code))]
fn install() -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the s2udio executable")?;
    let contents = unit_contents(&exe)?;
    let path = unit_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let tmp = path.with_extension("service.tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot install {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    if let Err(err) = systemctl(&["enable", "--now", UNIT_NAME]) {
        // Leave no half-installed unit behind that would make the toggle
        // claim autostart is configured.
        let _ = std::fs::remove_file(&path);
        let _ = systemctl(&["daemon-reload"]);
        return Err(err);
    }
    log::info!(unit:? = path, exe:? = exe; "daemon autostart enabled");
    Ok(())
}

#[cfg_attr(test, allow(dead_code))]
fn uninstall() -> Result<()> {
    // `disable` first: if the unit was never enabled this is a harmless
    // error, and the running daemon must not be stopped by a settings toggle.
    let _ = systemctl(&["disable", UNIT_NAME]);
    let path = unit_path();
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("cannot remove {}", path.display()))?;
    }
    let _ = systemctl(&["daemon-reload"]);
    log::info!(unit:? = path; "daemon autostart disabled");
    Ok(())
}

#[cfg_attr(test, allow(dead_code))]
fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("cannot run systemctl --user (no systemd user session?)")?;
    if !output.status.success() {
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The unit text. Quoted `ExecStart`/`ExecStop` keep a path with spaces one
/// argument; a quote or newline in the path is refused rather than emitted as
/// a broken unit.
pub(crate) fn unit_contents(exe: &Path) -> Result<String> {
    let exe = exe.to_str().context("the s2udio executable path is not valid UTF-8")?;
    if exe.contains(['"', '\n', '\r']) {
        bail!("cannot write a unit for a path containing a quote or newline: {exe:?}");
    }
    Ok(format!(
        "[Unit]\n\
         Description=s2udio always-on daemon (stream proxy, web-stream caretaker, QAT)\n\
         PartOf=graphical-session.target\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart=\"{exe}\" daemon start\n\
         ExecStop=\"{exe}\" daemon stop\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_starts_and_stops_the_daemon_as_a_oneshot() {
        let unit = unit_contents(Path::new("/home/u/.local/bin/s2udio")).expect("unit");
        assert!(unit.contains("Type=oneshot"), "{unit}");
        assert!(
            unit.contains("ExecStart=\"/home/u/.local/bin/s2udio\" daemon start"),
            "the launcher is the same binary and start is idempotent: {unit}"
        );
        assert!(unit.contains("ExecStop=\"/home/u/.local/bin/s2udio\" daemon stop"), "{unit}");
        assert!(unit.contains("RemainAfterExit=yes"), "{unit}");
        assert!(unit.contains("WantedBy=graphical-session.target"), "{unit}");
        assert!(unit.contains("PartOf=graphical-session.target"), "{unit}");
    }

    #[test]
    fn a_path_with_a_quote_is_refused_instead_of_written_broken() {
        assert!(unit_contents(Path::new("/home/u/we\"ird/s2udio")).is_err());
        assert!(unit_contents(Path::new("/home/u/we\nird/s2udio")).is_err());
    }

    #[test]
    fn the_unit_lives_under_the_user_systemd_dir() {
        assert_eq!(
            unit_path_in(Path::new("/home/u/.config")),
            PathBuf::from("/home/u/.config/systemd/user/s2udio-daemon.service")
        );
    }
}
