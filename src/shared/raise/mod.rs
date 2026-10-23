//! **Bring the running TUI's terminal window to the front.**
//!
//! A terminal TUI cannot raise itself with a portable API, so this module
//! probes the session once per request and tries the mechanisms that exist, in
//! order: tmux first (it can at least select the window/pane that holds the
//! TUI), then the terminal's own control channel (kitty), the window manager
//! (KWin on KDE Wayland/X11, the only one this repo has proven), other
//! compositors (Hyprland, Sway), X11 tools, and finally wezterm.
//!
//! The KWin backend is a generalisation of the rqbit web-UI activation in
//! `core/rqctl.rs`: KWin has no plain "activate window" D-Bus call, so a tiny
//! JavaScript program is loaded into KWin's scripting engine and run. Two
//! properties of that engine shape the code here:
//!
//! - a loaded script **runs exactly once** (`run()` never resets), so every
//!   raise loads a fresh script under a fresh plugin name;
//! - the engine has no file I/O, so the script reports through the journal
//!   (`console.info`), which is only read when `journalctl` is available.
//!
//! `ws.activeWindow = w` raises, un-minimises and focuses; KWin's
//! focus-stealing prevention only gates newly mapped windows, not scripting
//! activation.
use std::{
    path::Path,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::shared::{env::ENV, paths::s2udio_cache_dir};

/// Every process command in this module gets a deadline: a wedged window
/// manager or a stuck `qdbus6` must never hang the caller.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the KWin backend waits for its script's journal line.
const KWIN_RESULT_TIMEOUT: Duration = Duration::from_millis(900);

/// Terminal process names we recognise as "the window this TUI lives in".
const TERMINAL_PROGRAMS: &[&str] = &[
    "kitty",
    "konsole",
    "foot",
    "footclient",
    "wezterm-gui",
    "alacritty",
    "xterm",
    "gnome-terminal-server",
    "gnome-terminal",
    "yakuake",
    "tilix",
    "terminator",
    "urxvt",
    "st",
];

static SERIAL: AtomicU64 = AtomicU64::new(0);

/// A mechanism that can bring the terminal window forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Kitty,
    KWin,
    Hyprland,
    Sway,
    X11,
    Wezterm,
}

impl Backend {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Backend::Kitty => "kitty",
            Backend::KWin => "kwin",
            Backend::Hyprland => "hyprland",
            Backend::Sway => "sway",
            Backend::X11 => "x11",
            Backend::Wezterm => "wezterm",
        }
    }
}

/// The session facts the ladder needs. Built from the environment and
/// `/proc`; constructed literally in tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Session {
    /// The terminal emulator's pid (what window managers expose as `pid`).
    pub(crate) terminal_pid: Option<u32>,
    /// Best-effort WM class of the terminal (`kitty`, `konsole`, …).
    pub(crate) terminal_class: String,
    /// `$TMUX_PANE`, when inside tmux.
    pub(crate) tmux_pane: Option<String>,
    /// `$KITTY_LISTEN_ON`, present only with kitty remote control enabled.
    pub(crate) kitty_listen_on: Option<String>,
    /// `$KITTY_WINDOW_ID`.
    pub(crate) kitty_window_id: Option<String>,
    /// `$WEZTERM_PANE`.
    pub(crate) wezterm_pane: Option<String>,
    /// KDE/Plasma session.
    pub(crate) kde: bool,
    /// Hyprland session.
    pub(crate) hyprland: bool,
    /// Sway/i3 session.
    pub(crate) sway: bool,
    /// X11 session (an XWayland DISPLAY alone is not enough).
    pub(crate) x11: bool,
    /// `$DISPLAY`, needed by the X11 backend.
    pub(crate) display: Option<String>,
}

impl Session {
    /// Probe the current session. Called per request: env reads are cheap and
    /// the `/proc` walk is at most a dozen small reads.
    pub(crate) fn detect() -> Self {
        let desktop = ENV.var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
        let session_type = ENV.var("XDG_SESSION_TYPE").unwrap_or_default().to_ascii_lowercase();
        let display = non_empty(ENV.var("DISPLAY").ok());
        let wayland = non_empty(ENV.var("WAYLAND_DISPLAY").ok());
        let terminal_pid =
            ENV.var("KITTY_PID").ok().and_then(|pid| pid.parse().ok()).or_else(ancestor_terminal_pid);
        let terminal_class = terminal_pid
            .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/comm")).ok())
            .map_or_else(|| class_from_term(ENV.var("TERM").ok().as_deref()), |comm| class_for(&comm));

        Self {
            terminal_pid,
            terminal_class,
            tmux_pane: non_empty(ENV.var("TMUX_PANE").ok()),
            kitty_listen_on: non_empty(ENV.var("KITTY_LISTEN_ON").ok()),
            kitty_window_id: non_empty(ENV.var("KITTY_WINDOW_ID").ok()),
            wezterm_pane: non_empty(ENV.var("WEZTERM_PANE").ok()),
            kde: desktop.contains("kde")
                || desktop.contains("plasma")
                || ENV.var("KDE_FULL_SESSION").is_ok_and(|v| v == "true"),
            hyprland: ENV.var("HYPRLAND_INSTANCE_SIGNATURE").is_ok(),
            sway: ENV.var("SWAYSOCK").is_ok() || ENV.var("I3SOCK").is_ok(),
            x11: session_type == "x11" || (wayland.is_none() && display.is_some()),
            display,
        }
    }
}

/// The terminal backends that apply to `session`, in attempt order. tmux is
/// handled separately because selecting a tmux window does not raise the
/// terminal window around it.
fn plan(session: &Session) -> Vec<Backend> {
    let mut backends = Vec::new();
    if session.kitty_listen_on.is_some() && session.kitty_window_id.is_some() {
        backends.push(Backend::Kitty);
    }
    if session.kde {
        backends.push(Backend::KWin);
    }
    if session.hyprland {
        backends.push(Backend::Hyprland);
    }
    if session.sway {
        backends.push(Backend::Sway);
    }
    if session.x11 && session.display.is_some() {
        backends.push(Backend::X11);
    }
    if session.wezterm_pane.is_some() {
        backends.push(Backend::Wezterm);
    }
    backends
}

/// Raise the window that holds this process. Returns the backend that worked.
///
/// # Errors
///
/// Fails with an explanation naming every attempted backend when none of them
/// could bring a window forward.
pub(crate) fn run() -> Result<&'static str> {
    let session = Session::detect();
    log::debug!(session:?; "Raising the s2udio window");

    let tmux_ok = session.tmux_pane.as_deref().is_some_and(|pane| {
        match tmux_focus(pane) {
            Ok(()) => {
                log::debug!(pane:? = pane; "Selected the tmux pane that holds s2udio");
                true
            }
            Err(err) => {
                log::debug!(pane:? = pane, error:% = err; "tmux could not select the pane");
                false
            }
        }
    });

    let mut failures = Vec::new();
    for backend in plan(&session) {
        match attempt(backend, &session) {
            Ok(()) => {
                log::info!(backend:% = backend.name(); "Raised the s2udio window");
                return Ok(backend.name());
            }
            Err(err) => {
                log::debug!(backend:% = backend.name(), error:% = err; "Raise backend failed");
                failures.push(format!("{}: {err}", backend.name()));
            }
        }
    }

    // A tmux selection is not a window raise, but it is still progress: the
    // user sees the pane when they switch back. Report it rather than failing
    // outright when it was the only thing that worked.
    if tmux_ok {
        log::info!("Selected the tmux pane; no window-manager backend was available");
        return Ok("tmux");
    }

    if failures.is_empty() {
        bail!(
            "no raise backend applies to this session (terminal pid {:?}, class {:?})",
            session.terminal_pid,
            session.terminal_class
        );
    }
    Err(anyhow!("all raise backends failed: {}", failures.join("; ")))
}

fn attempt(backend: Backend, session: &Session) -> Result<()> {
    match backend {
        Backend::Kitty => kitty_focus(session),
        Backend::KWin => kwin_focus(session),
        Backend::Hyprland => compositor_focus(session, "hyprctl"),
        Backend::Sway => sway_focus(session),
        Backend::X11 => x11_focus(session),
        Backend::Wezterm => wezterm_focus(session),
    }
}

/// `tmux select-window`/`switch-client`: makes the TUI's pane the one the
/// client shows. Returns Ok when either command succeeded.
fn tmux_focus(pane: &str) -> Result<()> {
    let tmux = which::which("tmux").context("tmux is not installed")?;
    let mut selected = false;
    for args in [vec!["select-window".to_owned(), "-t".to_owned(), pane.to_owned()], vec![
        "switch-client".to_owned(),
        "-t".to_owned(),
        pane.to_owned(),
    ]] {
        match run_with_timeout(&tmux, &args, COMMAND_TIMEOUT) {
            Ok(output) if output.status.success() => selected = true,
            Ok(output) => log::trace!(code:? = output.status.code(); "tmux command refused"),
            Err(err) => log::trace!(error:% = err; "tmux command failed"),
        }
    }
    if selected { Ok(()) } else { bail!("tmux did not select {pane}") }
}

fn kitty_focus(session: &Session) -> Result<()> {
    let listen_on = session.kitty_listen_on.as_deref().context("kitty remote control is off")?;
    let window_id = session.kitty_window_id.as_deref().context("no kitty window id")?;
    let kitty = which::which("kitty").context("kitty is not on PATH")?;
    let output = run_with_timeout(
        &kitty,
        &[
            "@".to_owned(),
            "--to".to_owned(),
            listen_on.to_owned(),
            "focus-window".to_owned(),
            "--match".to_owned(),
            format!("window_id:{window_id}"),
        ],
        COMMAND_TIMEOUT,
    )?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("kitty focus-window exited {}", output.status)
    }
}

fn wezterm_focus(session: &Session) -> Result<()> {
    let pane = session.wezterm_pane.as_deref().context("no WEZTERM_PANE")?;
    let wezterm = which::which("wezterm").context("wezterm is not on PATH")?;
    let output = run_with_timeout(
        &wezterm,
        &["cli".to_owned(), "activate-pane".to_owned(), "--pane-id".to_owned(), pane.to_owned()],
        COMMAND_TIMEOUT,
    )?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("wezterm activate-pane exited {}", output.status)
    }
}

fn compositor_focus(session: &Session, binary: &str) -> Result<()> {
    let pid = session.terminal_pid.context("no terminal pid to match")?;
    let tool = which::which(binary).with_context(|| format!("{binary} is not on PATH"))?;
    let by_pid = run_with_timeout(
        &tool,
        &["dispatch".to_owned(), "focuswindow".to_owned(), format!("pid:{pid}")],
        COMMAND_TIMEOUT,
    )?;
    if by_pid.status.success() {
        return Ok(());
    }
    if session.terminal_class.is_empty() {
        bail!("{binary} rejected pid:{pid}");
    }
    let by_class = run_with_timeout(
        &tool,
        &[
            "dispatch".to_owned(),
            "focuswindow".to_owned(),
            format!("class:{}", session.terminal_class),
        ],
        COMMAND_TIMEOUT,
    )?;
    if by_class.status.success() {
        Ok(())
    } else {
        bail!("{binary} rejected pid:{pid} and class:{}", session.terminal_class)
    }
}

fn sway_focus(session: &Session) -> Result<()> {
    let pid = session.terminal_pid.context("no terminal pid to match")?;
    let swaymsg = which::which("swaymsg").context("swaymsg is not on PATH")?;
    let output = run_with_timeout(
        &swaymsg,
        &[format!("[pid={pid}] focus")],
        COMMAND_TIMEOUT,
    )?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("swaymsg rejected [pid={pid}] focus")
    }
}

fn x11_focus(session: &Session) -> Result<()> {
    let pid = session.terminal_pid.context("no terminal pid to match")?;
    if let Ok(xdotool) = which::which("xdotool") {
        let output = run_with_timeout(
            &xdotool,
            &[
                "search".to_owned(),
                "--pid".to_owned(),
                pid.to_string(),
                "windowactivate".to_owned(),
            ],
            COMMAND_TIMEOUT,
        )?;
        if output.status.success() {
            return Ok(());
        }
    }
    let wmctrl = which::which("wmctrl").context("neither xdotool nor wmctrl is on PATH")?;
    let list = run_with_timeout(&wmctrl, &["-lp".to_owned()], COMMAND_TIMEOUT)?;
    let wanted = pid.to_string();
    for line in String::from_utf8_lossy(&list.stdout).lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        // wmctrl -lp: <id> <desktop> <pid> <host> <title...>
        if columns.len() > 2 && columns[2] == wanted {
            let activate = run_with_timeout(
                &wmctrl,
                &["-i".to_owned(), "-a".to_owned(), columns[0].to_owned()],
                COMMAND_TIMEOUT,
            )?;
            if activate.status.success() {
                return Ok(());
            }
        }
    }
    bail!("no X11 window is owned by pid {pid}")
}

/// KWin: generate a one-shot script, load it under a unique plugin name, run
/// it, unload it, and read its `console.info` marker from the journal.
fn kwin_focus(session: &Session) -> Result<()> {
    let qdbus = which::which("qdbus6").or_else(|_| which::which("qdbus")).context(
        "qdbus6/qdbus is not on PATH (KWin scripting is unavailable)",
    )?;
    let cache_dir = s2udio_cache_dir().context("cannot determine the cache directory")?;
    std::fs::create_dir_all(&cache_dir).context("cannot create the cache directory")?;
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    let unique = format!("{}-{serial}", std::process::id());
    let nonce = format!("S2URAISE{unique}");
    let script_path = cache_dir.join(format!("raise-{unique}.js"));
    std::fs::write(&script_path, kwin_script(session, &nonce))
        .with_context(|| format!("cannot write {}", script_path.display()))?;

    let plugin_name = format!("s2udio-raise-{unique}");
    let load = run_kwin(
        &qdbus,
        &[
            "org.kde.KWin".to_owned(),
            "/Scripting".to_owned(),
            "org.kde.kwin.Scripting.loadScript".to_owned(),
            script_path.to_string_lossy().into_owned(),
            plugin_name.clone(),
        ],
    )?;
    let index: u32 = String::from_utf8_lossy(&load.stdout)
        .trim()
        .parse()
        .context("KWin did not return a script index")?;

    let run_result = run_kwin(
        &qdbus,
        &[
            "org.kde.KWin".to_owned(),
            format!("/Scripting/Script{index}"),
            "org.kde.kwin.Script.run".to_owned(),
        ],
    );
    // Best effort: a script can only run once, and KWin keeps the object for
    // the session otherwise. Unloading is asynchronous, so the name is never
    // reused.
    let _ = run_kwin(
        &qdbus,
        &[
            "org.kde.KWin".to_owned(),
            "/Scripting".to_owned(),
            "org.kde.kwin.Scripting.unloadScript".to_owned(),
            plugin_name,
        ],
    );
    let _ = std::fs::remove_file(&script_path);
    run_result.context("KWin could not run the raise script")?;

    let Some(journalctl) = which::which("journalctl").ok() else {
        // Without the journal the script's result cannot be read; the run
        // call itself already succeeded.
        return Ok(());
    };
    let deadline = Instant::now() + KWIN_RESULT_TIMEOUT;
    loop {
        if let Some(line) = journal_line_with(&journalctl, &nonce) {
            if line.contains("HIT") {
                return Ok(());
            }
            if line.contains("MISS") {
                bail!(
                    "KWin found no window for pid {:?} or class {:?}",
                    session.terminal_pid,
                    session.terminal_class
                );
            }
            if line.contains("ERR") {
                bail!("KWin script reported: {line}");
            }
        }
        if Instant::now() >= deadline {
            // The script ran (KWin replied) but wrote no marker: the journal
            // may be rate-limited or unavailable. Do not call it a failure.
            log::debug!(nonce:% = nonce; "KWin raise marker not seen in the journal");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn run_kwin(qdbus: &Path, args: &[String]) -> Result<Output> {
    run_with_timeout(qdbus, args, COMMAND_TIMEOUT).context("qdbus6 call failed")
}

/// The one-shot KWin script: pick the best matching normal window (pid first,
/// class as fallback, caption/stacking as tie-breakers), un-minimise it and
/// activate it. `pid` is injected as a number and the strings are quoted.
fn kwin_script(session: &Session, nonce: &str) -> String {
    let target = session.terminal_pid.unwrap_or(0);
    let class = js_string(&session.terminal_class);
    let nonce = js_string(nonce);
    format!(
        r#"// s2udio: bring the TUI's terminal window to the front.
var target = {target};
var cls = "{class}";
var nonce = "{nonce}";
var list = workspace.windowList();
var best = null;
var bestScore = -1;
for (var i = 0; i < list.length; i++) {{
    var w = list[i];
    if (w.normalWindow === false) {{ continue; }}
    var byPid = target > 0 && w.pid === target;
    var byClass = cls !== "" && String(w.resourceClass || "").indexOf(cls) === 0;
    if (!byPid && !byClass) {{ continue; }}
    var score = 0;
    if (String(w.caption || "").indexOf("s2udio") >= 0) {{ score += 4; }}
    if (workspace.activeWindow === w) {{ score += 2; }}
    score = score * 1000 + (typeof w.stackingOrder === "number" ? w.stackingOrder : 0);
    if (score > bestScore) {{ bestScore = score; best = w; }}
}}
if (best === null) {{
    console.info(nonce + " MISS");
}} else {{
    try {{
        if (best.minimized) {{ best.minimized = false; }}
        workspace.activeWindow = best;
        console.info(nonce + " HIT " + best.pid);
    }} catch (e) {{
        console.info(nonce + " ERR " + e);
    }}
}}
"#
    )
}

/// Quote a string for inclusion in the generated JS.
fn js_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// The first journal line containing `marker`, if any. KWin scripts have no
/// file I/O, so `console.info` into the user journal is the result channel.
fn journal_line_with(journalctl: &Path, marker: &str) -> Option<String> {
    let output = run_with_timeout(
        journalctl,
        &[
            "--user".to_owned(),
            "--since".to_owned(),
            "-5 s".to_owned(),
            "--no-pager".to_owned(),
        ],
        Duration::from_secs(2),
    )
    .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(marker))
        .next_back()
        .map(str::to_owned)
}

/// Run a command, killing it at the deadline. `Child::output()` has no
/// timeout; a wedged WM tool must not hang the worker thread.
fn run_with_timeout(program: &Path, args: &[String], timeout: Duration) -> Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to run {}", program.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("{} timed out after {timeout:?}", program.display());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                let _ = child.kill();
                return Err(err).with_context(|| format!("Failed to poll {}", program.display()));
            }
        }
    }
    child.wait_with_output().with_context(|| format!("Failed to read {}", program.display()))
}

/// The pid of the terminal emulator this process runs under, by walking
/// `/proc/<pid>/stat` parents until a known terminal shows up.
fn ancestor_terminal_pid() -> Option<u32> {
    let mut pid = std::process::id();
    for _ in 0..12 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let parent = parse_ppid(&stat)?;
        if parent <= 1 {
            return None;
        }
        let comm = std::fs::read_to_string(format!("/proc/{parent}/comm")).ok()?.trim().to_owned();
        if TERMINAL_PROGRAMS.iter().any(|name| comm == *name) {
            return Some(parent);
        }
        pid = parent;
    }
    None
}

/// `pid (comm) state ppid …` — the comm may contain spaces and parentheses, so
/// the fields after the *last* `)` are the reliable ones.
fn parse_ppid(stat: &str) -> Option<u32> {
    let tail = stat.get(stat.rfind(')')? + 1..)?.trim_start();
    let mut fields = tail.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

/// WM class for a terminal process name.
fn class_for(comm: &str) -> String {
    let comm = comm.trim();
    match comm {
        "wezterm-gui" => "wezterm".to_owned(),
        "gnome-terminal-server" => "gnome-terminal".to_owned(),
        other => other.to_owned(),
    }
}

/// Fallback class from `$TERM` when no terminal process was identified.
fn class_from_term(term: Option<&str>) -> String {
    match term.unwrap_or_default() {
        "xterm-kitty" => "kitty".to_owned(),
        term if term.contains("konsole") => "konsole".to_owned(),
        term if term.contains("alacritty") => "alacritty".to_owned(),
        term if term.contains("wezterm") => "wezterm".to_owned(),
        term if term.contains("foot") => "foot".to_owned(),
        _ => String::new(),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_session() -> Session {
        Session {
            terminal_pid: Some(4242),
            terminal_class: "kitty".to_owned(),
            tmux_pane: Some("%1".to_owned()),
            kitty_listen_on: Some("unix:/tmp/kitty.sock".to_owned()),
            kitty_window_id: Some("7".to_owned()),
            wezterm_pane: Some("3".to_owned()),
            kde: true,
            hyprland: true,
            sway: true,
            x11: true,
            display: Some(":0".to_owned()),
        }
    }

    #[test]
    fn the_ladder_is_ordered_and_prerequisite_gated() {
        let backends = plan(&full_session());
        assert_eq!(
            backends,
            vec![
                Backend::Kitty,
                Backend::KWin,
                Backend::Hyprland,
                Backend::Sway,
                Backend::X11,
                Backend::Wezterm,
            ]
        );

        // A bare KDE session with no terminal control channel still raises.
        let kde = Session { kde: true, ..Session::default() };
        assert_eq!(plan(&kde), vec![Backend::KWin]);

        // kitty needs both the socket and the window id.
        let no_listen = Session {
            kitty_window_id: Some("7".to_owned()),
            ..Session::default()
        };
        assert!(plan(&no_listen).is_empty());

        // X11 needs an actual X11 session, not just a DISPLAY (XWayland).
        let wayland = Session { display: Some(":0".to_owned()), ..Session::default() };
        assert!(plan(&wayland).is_empty());
    }

    #[test]
    fn the_kwin_script_targets_the_terminal_and_activates_it() {
        let script = kwin_script(&full_session(), "S2URAISE99");
        assert!(script.contains("var target = 4242;"), "{script}");
        assert!(script.contains(r#"var cls = "kitty";"#), "{script}");
        assert!(script.contains(r#"var nonce = "S2URAISE99";"#), "{script}");
        assert!(script.contains("workspace.activeWindow = best;"), "{script}");
        assert!(script.contains("best.minimized = false;"), "{script}");
        assert!(script.contains("if (w.normalWindow === false)"), "{script}");
    }

    #[test]
    fn the_script_quotes_are_escaped() {
        assert_eq!(js_string(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(js_string("line\nbreak"), "line\\nbreak");
        assert_eq!(js_string("bell\u{7}char"), "bellchar");
    }

    #[test]
    fn ppid_is_read_after_the_last_parenthesis() {
        // The comm contains spaces and parentheses: fields before the last ')'
        // are not trustworthy.
        assert_eq!(parse_ppid("123 (my (odd) proc) S 456 789 0 -1"), Some(456));
        assert_eq!(parse_ppid("not a stat line"), None);
    }

    #[test]
    fn terminal_classes_are_normalised_for_window_matching() {
        assert_eq!(class_for("kitty\n"), "kitty");
        assert_eq!(class_for("wezterm-gui"), "wezterm");
        assert_eq!(class_for("gnome-terminal-server"), "gnome-terminal");
        assert_eq!(class_from_term(Some("xterm-kitty")), "kitty");
        assert_eq!(class_from_term(Some("screen-256color")), "");
    }

    #[test]
    fn a_command_that_never_exits_is_killed_at_the_deadline() {
        let sleep = which::which("sleep").expect("sleep on PATH");
        let started = Instant::now();
        let result = run_with_timeout(&sleep, &["5".to_owned()], Duration::from_millis(80));
        assert!(result.is_err(), "a 5 s sleep under an 80 ms deadline must fail");
        assert!(started.elapsed() < Duration::from_secs(2), "the deadline was not enforced");
    }
}
