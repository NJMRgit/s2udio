//! The daemon's QAT (kitty quick-access terminal) controller.
//!
//! The QAT is a kitty layer surface: when the compositor removes the display
//! output (a screen blank on this host) the surface is closed and kitty tears
//! the whole session down. Nothing inside the QAT survives that, and a TUI
//! cannot relaunch its own terminal. The daemon can: it owns a global shortcut
//! (registered in KWin by a script, so no desktop-file shortcut is needed) and
//! launches/toggles the QAT with the TUI as its command. The QAT is therefore
//! disposable: closed by the compositor, reopened by the daemon with the
//! player state (MPD queue, daemon-owned streams) intact.
//!
//! The shortcut calls the daemon's D-Bus method (`org.s2udio.Daemon`), because
//! a KWin script has no other way to reach a process. The method runs the same
//! `toggle()` a user gets from `s2udio daemon qat`.

use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    time::Duration,
};

use anyhow::{Context, Result};
use parking_lot::Mutex;

/// Everything the QAT controller needs, resolved once at daemon startup.
pub(crate) struct QatConfig {
    /// The `kitten` launcher (it builds the panel argv and handles
    /// `--single-instance`, so a second call toggles the existing panel).
    pub kitten: PathBuf,
    /// The TUI binary the QAT runs. The daemon's own executable, so the QAT
    /// is the same build as the daemon.
    pub tui: PathBuf,
    /// The live global shortcut, e.g. `Meta+T`. The Settings panel changes
    /// this at runtime, so it is not a plain field.
    hotkey: Mutex<String>,
    /// Channel to the hotkey guard thread; `None` before `install_hotkey`.
    hotkey_tx: Mutex<Option<mpsc::Sender<String>>>,
}

impl QatConfig {
    pub fn from_env() -> Self {
        let kitten = crate::shared::paths::resolve_bin("kitten");
        let tui = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("s2udio"));
        // Precedence: the explicit environment override (tests, units), then
        // the Settings-panel sidecar, then the stable default.
        let file = crate::config::daemon::DaemonConfigFile::load();
        let configured = crate::config::daemon::normalize_hotkey(&file.qat_hotkey)
            .unwrap_or_else(|| crate::config::daemon::DEFAULT_QAT_HOTKEY.to_owned());
        let hotkey = crate::shared::env::ENV
            .var_os("S2U_QAT_HOTKEY")
            .and_then(|value| crate::config::daemon::normalize_hotkey(&value.to_string_lossy()))
            .unwrap_or(configured);
        Self {
            kitten,
            tui,
            hotkey: Mutex::new(hotkey),
            hotkey_tx: Mutex::new(None),
        }
    }

    /// The chord currently configured (what the daemon reports and registers).
    pub(crate) fn hotkey(&self) -> String {
        self.hotkey.lock().clone()
    }

    #[cfg(test)]
    fn for_test(kitten: &str, tui: &str, hotkey: &str) -> Self {
        Self {
            kitten: PathBuf::from(kitten),
            tui: PathBuf::from(tui),
            hotkey: Mutex::new(hotkey.to_owned()),
            hotkey_tx: Mutex::new(None),
        }
    }

    /// The argv of one toggle: the kitten launches the panel on the first call
    /// and toggles its visibility on every later one (kitty's
    /// `--single-instance` + `--toggle-visibility`, which the QAT config
    /// already carries). The TUI runs inside the panel.
    fn toggle_argv(&self) -> Vec<String> {
        vec![
            self.kitten.to_string_lossy().into_owned(),
            "quick-access-terminal".to_owned(),
            self.tui.to_string_lossy().into_owned(),
        ]
    }
}

/// Launch or toggle the QAT. Fire-and-forget: the kitten process becomes the
/// panel (or hands the toggle to the running one) and is reaped on a side
/// thread, so neither the IPC handler nor the D-Bus method waits for it.
pub(crate) fn toggle(cfg: &QatConfig) -> Result<()> {
    let argv = cfg.toggle_argv();
    let (program, args) = argv.split_first().context("empty QAT argv")?;
    let mut command = Command::new(program);
    command.args(args);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe; detaching keeps the panel alive
    // when the daemon restarts and keeps the daemon from waiting on it.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().with_context(|| format!("cannot run {program}"))?;
    log::info!(program = program.as_str(), args:? = args; "daemon: toggling the QAT");
    std::thread::Builder::new()
        .name("s2udio-qat-reap".to_owned())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}

/// Raise the player without ever hiding it.
///
/// A live TUI raises its own window: its kitty session knows the QAT panel and
/// `resize-os-window --action=show` remaps a hidden one, which no external
/// process can do. Only when no TUI is running does the daemon fall back to
/// the kitten (which launches the QAT with a fresh TUI, or shows an existing
/// hidden panel).
pub(crate) fn raise(cfg: &QatConfig) -> Result<()> {
    if crate::shared::ipc::raise_running_tui() {
        log::info!("daemon: asked the running TUI to raise its window");
        return Ok(());
    }
    toggle(cfg)
}

/// The JavaScript KWin loads to register the shortcut. Kept as a function so
/// the generated text (and the escaping) is testable.
pub(crate) fn hotkey_script(hotkey: &str) -> String {
    format!(
        "// s2udio: the daemon owns this shortcut; no desktop-file shortcut is needed.\n\
         function s2udioToggleQat() {{\n\
         \x20   console.info(\"S2U-QAT hotkey pressed\");\n\
         \x20   callDBus(\"org.s2udio.Daemon\", \"/org/s2udio/Daemon\", \"org.s2udio.Daemon\", \"ToggleQat\");\n\
         }}\n\
         registerShortcut(\"s2udio-qat\", \"s2udio — show/hide the player\", {hotkey:?}, s2udioToggleQat);\n\
         console.info(\"S2U-QAT hotkey registered: {hotkey}\");\n"
    )
}

/// Change the live hotkey and ask the guard thread to re-register it.
pub(crate) fn set_hotkey(cfg: &QatConfig, chord: &str) -> Result<()> {
    let chord = crate::config::daemon::normalize_hotkey(chord)
        .context("the QAT hotkey cannot be empty or contain quotes/newlines")?;
    *cfg.hotkey.lock() = chord.clone();
    if let Some(tx) = cfg.hotkey_tx.lock().as_ref() {
        let _ = tx.send(chord.clone());
    }
    log::info!(hotkey = chord.as_str(); "daemon: QAT hotkey changed");
    Ok(())
}

/// Register the shortcut in the running KWin (once) and keep it registered
/// across a KWin restart. A missing KWin/qdbus6 is not fatal: the daemon and
/// `daemon qat` still work, there is just no global shortcut.
///
/// The thread also owns hotkey changes: `set_hotkey` sends the new chord over
/// a channel, and the thread rewrites the script, unloads the old plugin,
/// drops KGlobalAccel's remembered action (it keeps the first active chord)
/// and loads the script again.
pub(crate) fn install_hotkey(cfg: Arc<QatConfig>) {
    let path = match hotkey_script_path() {
        Ok(path) => path,
        Err(err) => {
            log::warn!(error:? = err; "daemon: cannot write the QAT hotkey script");
            return;
        }
    };
    let (tx, rx) = mpsc::channel();
    *cfg.hotkey_tx.lock() = Some(tx);
    std::thread::Builder::new()
        .name("s2udio-qat-hotkey".to_owned())
        .spawn(move || {
            let mut loaded_name: Option<String> = None;
            loop {
                match rx.recv_timeout(Duration::from_secs(20)) {
                    Ok(chord) => {
                        if let Err(err) = write_hotkey_script(&path, &chord) {
                            log::warn!(error:? = err; "daemon: cannot write the new QAT hotkey script");
                        }
                        if let Some(name) = loaded_name.take() {
                            if let Err(err) = kwin_unload_script(&name) {
                                log::debug!(error:? = err; "daemon: cannot unload the old QAT hotkey script");
                            }
                        }
                        kglobalaccel_unregister();
                        log::info!(hotkey = chord.as_str(); "daemon: QAT hotkey updated");
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                let wanted = loaded_name
                    .as_deref()
                    .is_some_and(|name| kwin_script_loaded(name));
                if !wanted {
                    let chord = cfg.hotkey();
                    if let Err(err) = write_hotkey_script(&path, &chord) {
                        log::warn!(error:? = err; "daemon: cannot write the QAT hotkey script");
                        continue;
                    }
                    let name = format!("s2udio-qat-{}", std::process::id());
                    match kwin_load_script(&path, &name) {
                        Ok(()) => {
                            log::info!(hotkey = chord.as_str(), script:? = path; "daemon: QAT hotkey registered");
                            loaded_name = Some(name);
                        }
                        Err(err) => {
                            log::debug!(error:? = err; "daemon: QAT hotkey not registered (is KWin running?)");
                            loaded_name = None;
                        }
                    }
                }
            }
        })
        .ok();
}

fn write_hotkey_script(path: &Path, chord: &str) -> Result<()> {
    std::fs::write(path, hotkey_script(chord))
        .with_context(|| format!("cannot write {}", path.display()))?;
    #[allow(unused_imports)]
    use std::os::unix::fs::PermissionsExt as _;
    if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)) {
        log::debug!(error:? = err; "daemon: cannot chmod the QAT hotkey script");
    }
    Ok(())
}

fn kwin_unload_script(name: &str) -> Result<()> {
    let out = Command::new(qdbus())
        .args([
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting.unloadScript",
            name,
        ])
        .output()
        .context("cannot run qdbus6")?;
    if !out.status.success() {
        anyhow::bail!(
            "qdbus6 unloadScript {name} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// KGlobalAccel remembers the action's first active chord; a re-registered
/// script alone does not move it. Dropping the action before the reload lets
/// the new chord become the action's default again. Best effort: a missing
/// KGlobalAccel service is not a daemon failure.
fn kglobalaccel_unregister() {
    match Command::new(qdbus())
        .args([
            "org.kde.kglobalaccel",
            "/kglobalaccel",
            "org.kde.KGlobalAccel.unregister",
            "kwin",
            "s2udio-qat",
        ])
        .output()
    {
        Ok(out) if out.status.success() => {
            log::debug!(
                result = String::from_utf8_lossy(&out.stdout).trim();
                "daemon: KGlobalAccel action reset for the new chord"
            );
        }
        Ok(out) => log::debug!(
            stderr = String::from_utf8_lossy(&out.stderr).trim();
            "daemon: KGlobalAccel unregister failed"
        ),
        Err(err) => log::debug!(error:? = err; "daemon: cannot reset KGlobalAccel"),
    }
}

fn hotkey_script_path() -> Result<PathBuf> {
    let dir = crate::shared::paths::s2udio_cache_dir()
        .or_else(|| crate::shared::paths::s2udio_state_dir())
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("s2udio-qat-hotkey.js"))
}

fn qdbus() -> PathBuf {
    crate::shared::paths::resolve_bin("qdbus6")
}

/// Whether KWin already has the script under this plugin name.
fn kwin_script_loaded(name: &str) -> bool {
    matches!(
        Command::new(qdbus())
            .args([
                "org.kde.KWin",
                "/Scripting",
                "org.kde.kwin.Scripting.isScriptLoaded",
                name,
            ])
            .output(),
        Ok(out) if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true"
    )
}

/// Load the script and run it (a loaded script runs once, so `run` is what
/// makes `registerShortcut` execute).
fn kwin_load_script(path: &Path, name: &str) -> Result<()> {
    let out = Command::new(qdbus())
        .args([
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting.loadScript",
            &path.to_string_lossy(),
            name,
        ])
        .output()
        .context("cannot run qdbus6")?;
    if !out.status.success() {
        anyhow::bail!(
            "loadScript failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("loadScript returned no script id: {id:?}");
    }
    let out = Command::new(qdbus())
        .args([
            "org.kde.KWin",
            &format!("/Scripting/Script{id}"),
            "org.kde.kwin.Script.run",
        ])
        .output()
        .context("cannot run qdbus6")?;
    if !out.status.success() {
        anyhow::bail!(
            "Script.run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The daemon's D-Bus object: what the KWin script's `callDBus` reaches.
pub(crate) struct QatService {
    cfg: Arc<QatConfig>,
}

#[zbus::interface(name = "org.s2udio.Daemon")]
impl QatService {
    /// Launch the QAT, or toggle the running one.
    fn toggle_qat(&self) {
        if let Err(err) = toggle(&self.cfg) {
            log::warn!(error:? = err; "daemon: QAT toggle from D-Bus failed");
        }
    }

    /// Alias kept explicit for callers that want the launch/toggle action by
    /// name; the QAT's own semantics are a toggle.
    fn show_qat(&self) {
        self.toggle_qat();
    }

    fn hide_qat(&self) {
        self.toggle_qat();
    }

    /// Raise the player (MPRIS Raise): a live TUI raises itself, and with no
    /// TUI the daemon launches the QAT with a fresh one.
    fn raise_qat(&self) {
        if let Err(err) = raise(&self.cfg) {
            log::warn!(error:? = err; "daemon: QAT raise from D-Bus failed");
        }
    }

    fn qat_hotkey(&self) -> String {
        self.cfg.hotkey()
    }
}

/// Publish the D-Bus service the KWin shortcut calls. The connection is held
/// for the process lifetime by the spawned thread.
pub(crate) fn start_dbus(cfg: Arc<QatConfig>) -> Result<()> {
    let connection = zbus::blocking::connection::Builder::session()
        .context("no session bus")?
        .name("org.s2udio.Daemon")?
        .serve_at("/org/s2udio/Daemon", QatService { cfg })?
        .build()
        .context("cannot publish org.s2udio.Daemon")?;
    std::thread::Builder::new()
        .name("s2udio-qat-dbus".to_owned())
        .spawn(move || {
            log::info!("daemon: org.s2udio.Daemon is on the session bus");
            let _connection = connection;
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hotkey_script_registers_and_calls_the_daemon() {
        let script = hotkey_script("Meta+Shift+F12");
        assert!(script.contains("registerShortcut(\"s2udio-qat\""));
        assert!(
            script.contains("\"Meta+Shift+F12\""),
            "the configured chord is quoted into the script: {script}"
        );
        assert!(
            script.contains(
                "callDBus(\"org.s2udio.Daemon\", \"/org/s2udio/Daemon\", \"org.s2udio.Daemon\", \"ToggleQat\")"
            ),
            "the shortcut must call the daemon method: {script}"
        );
        // A chord with a quote must not break the JavaScript string.
        let quoted = hotkey_script("Meta+\"");
        assert!(!quoted.contains("\"Meta+\""), "the chord is escaped, got {quoted}");
    }

    #[test]
    fn a_toggle_runs_the_kitten_with_the_tui_as_its_command() {
        let cfg = QatConfig::for_test("/usr/bin/kitten", "/usr/local/bin/s2udio", "Meta+T");
        assert_eq!(
            cfg.toggle_argv(),
            vec![
                "/usr/bin/kitten".to_owned(),
                "quick-access-terminal".to_owned(),
                "/usr/local/bin/s2udio".to_owned(),
            ],
            "the panel must run the TUI, and a later call must toggle (kitty's single instance)"
        );
    }

    #[test]
    fn raise_asks_a_live_tui_before_launching_the_kitten() {
        use std::io::{BufRead, BufReader, Write};

        // A fake TUI socket in the real temp dir: `raise` must talk to it and
        // never reach the (nonexistent) kitten binary.
        let path = std::env::temp_dir().join(format!("s2u-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).expect("read");
            stream.write_all(b"ok\n").expect("reply");
            line
        });
        let cfg = QatConfig::for_test("/nonexistent/kitten", "/nonexistent/s2udio", "Meta+T");

        raise(&cfg).expect("the live TUI is asked to raise");
        let line = handle.join().expect("thread");
        assert!(line.contains("\"Raise\""), "got {line}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_hotkey_comes_from_the_environment_with_a_stable_default() {
        crate::shared::env::ENV.remove("S2U_QAT_HOTKEY");
        assert_eq!(QatConfig::from_env().hotkey(), "Meta+T");
        crate::shared::env::ENV.set("S2U_QAT_HOTKEY".to_owned(), "Meta+Shift+F12".to_owned());
        assert_eq!(QatConfig::from_env().hotkey(), "Meta+Shift+F12");
        crate::shared::env::ENV.set("S2U_QAT_HOTKEY".to_owned(), "   ".to_owned());
        assert_eq!(QatConfig::from_env().hotkey(), "Meta+T", "blank means default");
        crate::shared::env::ENV.remove("S2U_QAT_HOTKEY");
    }

    #[test]
    fn setting_the_hotkey_updates_the_live_value_and_rejects_blank() {
        let cfg = QatConfig::for_test("/usr/bin/kitten", "/usr/local/bin/s2udio", "Meta+T");
        set_hotkey(&cfg, " Meta+Shift+T ").expect("a valid chord");
        assert_eq!(cfg.hotkey(), "Meta+Shift+T");
        assert!(set_hotkey(&cfg, "   ").is_err(), "blank chords are refused");
        assert_eq!(cfg.hotkey(), "Meta+Shift+T", "an invalid chord keeps the old one");
    }
}
