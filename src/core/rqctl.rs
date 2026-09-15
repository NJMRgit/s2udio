//! `s2udio rq start|stop|open|tray` — shell control of the standalone
//! rqbit engine (round 43; `tray` round 81).
//!
//! The engine behind the rqbit web UI can be started from the Settings
//! panel (Settings -> torrent -> web ui) OR from the shell. So that both
//! share ONE engine, every started standalone engine registers itself in
//! a small JSON file (`~/.cache/s2udio/rqbit.json`): the pid + the
//! browser URL. The CLI commands read/write that file; the Settings
//! panel does the same, so `s2udio rq stop` also stops an engine the GUI
//! started, and `s2udio rq start` reuses one a previous session left
//! running. The file stores only the proxy URL (no auth token) — the
//! auth-injecting loopback proxy keeps the engine itself protected.
//!
//! The spawned engine is identical to the GUI's: `torrent::start_engine`
//! (config `torrent.port` / `socks_proxy` / `cache_dir` + the state.ron
//! socks override), auth-injecting web-UI proxy, random basic-auth token.
use std::{
    os::unix::process::CommandExt, path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use serde::{Deserialize, Serialize};
use crate::config::cli::RqCmd;
/// The registration file name inside the s2udio cache dir.
const STATE_FILE: &str = "rqbit.json";
/// The tray's pid file (same dir): the tray is a singleton, so launching
/// it twice (the S2RQ desktop entry clicked again, a manual `rq tray`)
/// does not add a second icon.
const TRAY_PID_FILE: &str = "rqbit-tray.pid";
/// The tray's icon name: the monochrome, **light** torrent glyph s2udio
/// installs into the user's hicolor icon theme (`assets/icons/`). Light on
/// purpose: breeze's `torrents-symbolic` that this replaced is the
/// light-scheme variant (`#232629`), i.e. a dark glyph that nearly
/// disappeared on a dark panel. The same name is used by the S2RQ desktop
/// entry.
pub const TORRENT_ICON: &str = "s2rq-torrent";
/// The icon file itself, embedded so the tray can install it on demand.
const TORRENT_ICON_SVG: &str = include_str!("../../assets/icons/s2rq-torrent.svg");
/// Two `Activate` signals inside this window count as one double click.
/// The StatusNotifierItem spec has no double-click event: the panel sends
/// one `Activate` per left click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// A running standalone rqbit engine, as registered on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RqEngineFile {
    /// The rqbit child process id.
    pub pid: u32,
    /// The browser URL (`http://127.0.0.1:<proxy port>/web/` — the
    /// auth-injecting proxy; no credentials appear here or are needed).
    pub web_url: String,
    /// The rqbit HTTP API port (the engine itself, behind the proxy) —
    /// for the `rq check` auth-integrity probe. Missing in registrations
    /// written before this field existed.
    #[serde(default)]
    pub engine_port: u16,
    /// The engine's cache/download dir (informational).
    pub cache_dir: String,
    /// Unix seconds when the engine was started.
    pub started_at: u64,
}
impl RqEngineFile {
    /// Whether the registered pid is a live process.
    pub fn alive(&self) -> bool {
        pid_alive(self.pid)
    }
}
/// `~/.cache/s2udio/rqbit.json`.
pub fn state_path() -> Option<PathBuf> {
    crate::shared::paths::s2udio_cache_dir().map(|dir| dir.join(STATE_FILE))
}
/// Read the registration file (missing/unparsable -> None; stale entries
/// are left for the caller's `alive()` check so a crashed engine can be
/// detected).
pub fn read_state() -> Option<RqEngineFile> {
    let path = state_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}
/// The registered engine, but only when its pid is alive.
pub fn registered_running() -> Option<RqEngineFile> {
    read_state().filter(|reg| reg.alive())
}
/// Whether a pid is a live process (signal 0 probe; EPERM counts as
/// alive — the process exists, just not ours to signal).
pub fn pid_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
/// Register a running engine (overwrites any previous registration).
/// Used by the Settings panel (pid = the rqbit child; the GUI process
/// owns the engine) and by the CLI daemon (pid = the daemon itself).
pub fn register(engine: &crate::core::torrent::TorrentEngine) -> Result<(), String> {
    let engine_port = engine
        .base_url()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(0);
    let reg = RqEngineFile {
        pid: engine.pid(),
        web_url: engine.web_url(),
        engine_port,
        cache_dir: engine.cache_dir.display().to_string(),
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    write_registration(&reg)
}
/// Persist a registration (overwrites any previous one).
pub fn write_registration(reg: &RqEngineFile) -> Result<(), String> {
    let Some(path) = state_path() else {
        return Err("Could not determine the s2udio cache dir".to_owned());
    };
    let content = serde_json::to_string_pretty(reg)
        .map_err(|err| format!("Failed to serialize the rqbit registration: {err}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    }
    std::fs::write(&path, content)
        .map_err(|err| format!("Cannot write {}: {err}", path.display()))
}
/// Remove the registration file (missing file is not an error).
pub fn unregister() -> Result<(), String> {
    if let Some(path) = state_path() {
        match std::fs::remove_file(&path) {
            Ok(()) | Err(_) if !path.exists() => {}
            Err(err) => return Err(format!("Cannot remove {}: {err}", path.display())),
            _ => {}
        }
    }
    Ok(())
}
/// Kill the registered engine (SIGTERM, then SIGKILL after ~2 s) and
/// remove its registration. Returns whether an engine was killed.
pub fn stop_registered() -> Result<bool, String> {
    let Some(reg) = read_state() else {
        return Ok(false);
    };
    if !pid_alive(reg.pid) {
        let _ = unregister();
        return Ok(false);
    }
    kill_pid(reg.pid);
    let _ = unregister();
    Ok(true)
}
/// SIGTERM, poll for exit (~2 s), then SIGKILL. Pub for the round-54
/// downloader daemon (`s2udio dl stop` kills the same way).
pub fn kill_pid(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    for _ in 0..20 {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}
/// The torrent engine config the CLI uses: config.ron `torrent` section
/// (defaults when absent) + the Settings-panel socks override from
/// state.ron (same rule as `main.rs` startup).
pub fn torrent_config() -> crate::config::torrent::Torrent {
    let mut file = crate::config::torrent::TorrentFile::default();
    if let Ok(path) = crate::shared::config_read::resolve_config_path(None)
        && let Ok(config_file) = crate::shared::config_read::read_config_file(&path)
    {
        file = config_file.torrent;
    }
    let mut torrent = crate::config::torrent::Torrent::from(file);
    let state = crate::config::state::AppStateFile::load();
    if let Some(proxy) = state.torrent_socks_proxy {
        torrent.socks_proxy = if proxy.trim().is_empty() { None } else { Some(proxy) };
    }
    torrent
}
/// `s2udio rq start|stop|open`.
pub fn run(cmd: RqCmd) -> Result<(), String> {
    match cmd {
        RqCmd::Start => start(),
        RqCmd::Stop => {
            match stop_registered()? {
                true => {
                    println!("rqbit engine stopped");
                    Ok(())
                }
                false => {
                    Err(
                        "No rqbit engine is running (start one with `s2udio rq start` or Settings -> torrent)"
                            .to_owned(),
                    )
                }
            }
        }
        RqCmd::Open => {
            let Some(reg) = registered_running() else {
                return Err(
                    "No rqbit engine is running (start one with `s2udio rq start` or Settings -> torrent)"
                        .to_owned(),
                );
            };
            open_web_ui(&reg.web_url)?;
            println!("{}", reg.web_url);
            Ok(())
        }
        RqCmd::Check => check(),
        RqCmd::Tray => tray(),
        RqCmd::Serve => serve(),
    }
}
/// Verify the auth-injecting proxy end-to-end:
///   1. the proxy serves the web UI without credentials (200 text/html),
///   2. the proxy serves the API without credentials (200),
///   3. the engine port itself still rejects unauthenticated requests
///      (401) — the proxy is the only way in without the token.
/// Prints one PASS/FAIL line per probe; exit code 0 = all good.
fn check() -> Result<(), String> {
    let Some(reg) = registered_running() else {
        return Err(
            "No rqbit engine is running (start one with `s2udio rq start` or Settings -> torrent)"
                .to_owned(),
        );
    };
    println!("rqbit engine:   RUNNING (pid {}, web UI {})", reg.pid, reg.web_url);
    let mut ok = true;
    let proxy_base = reg
        .web_url
        .trim_end_matches("/web/")
        .trim_end_matches('/')
        .to_owned();
    match http_status(&format!("{proxy_base}/web/")) {
        Ok(200) => println!("[PASS] web UI via proxy (no credentials)  -> 200"),
        other => {
            ok = false;
            println!("[FAIL] web UI via proxy (no credentials)  -> {other:?}");
        }
    }
    match http_status(&format!("{proxy_base}/stats")) {
        Ok(200) => println!("[PASS] API via proxy (no credentials)     -> 200"),
        other => {
            ok = false;
            println!("[FAIL] API via proxy (no credentials)     -> {other:?}");
        }
    }
    if reg.engine_port == 0 {
        ok = false;
        println!(
            "[FAIL] engine port auth (no credentials)  -> unknown port in the registration              (restart the engine to refresh it)"
        );
    } else {
        match http_status(&format!("http://127.0.0.1:{}/stats", reg.engine_port)) {
            Ok(401) => {
                println!(
                    "[PASS] engine port auth (no credentials)  -> 401 (auth intact, port {})",
                    reg.engine_port
                )
            }
            other => {
                ok = false;
                println!(
                    "[FAIL] engine port auth (no credentials)  -> {other:?} (expected 401, port {})",
                    reg.engine_port
                );
            }
        }
    }
    // 4. A POST body must arrive byte-for-byte. `/torrents?is_url=true`
    //    echoes the URL it was handed in its 400, so the probe string
    //    coming back intact (and without a `\r\n` in front of it) proves
    //    the proxy did not shift the body. This is the probe that catches
    //    the round-79 defect: the head rewrite emitted an extra blank line,
    //    so the engine read every body two bytes early — the web UI's
    //    "Add Torrent" failed with `unsupported URL "\r\nmagnet:?xt=…"`
    //    and `.torrent` uploads arrived two bytes short. The three GET
    //    probes above cannot see it.
    let probe = "s2udio-proxy-body-probe";
    let probe_url = format!("{proxy_base}/torrents?&is_url=true");
    match http_post_text(&probe_url, probe) {
        Ok((400, body))
            if body.contains(probe) && !body.contains(&format!("\\r\\n{probe}")) =>
        {
            println!("[PASS] POST body via proxy (no credentials)-> 400, body intact")
        }
        Ok((status, body)) => {
            ok = false;
            println!(
                "[FAIL] POST body via proxy (no credentials)-> {status}, body: {}",
                body.chars().take(160).collect::<String>()
            );
        }
        Err(err) => {
            ok = false;
            println!("[FAIL] POST body via proxy (no credentials)-> {err}");
        }
    }
    if ok {
        println!("result: OK — the proxy is injecting auth correctly");
        Ok(())
    } else {
        Err("result: FAILED — see the [FAIL] lines above".to_owned())
    }
}
/// POST `body` to `url` and return the status plus the response text
/// (2xx/4xx both come back as values; only transport failures are Err).
fn http_post_text(url: &str, body: &str) -> Result<(u16, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build();
    let request = agent
        .post(url)
        .set("Content-Type", "text/plain;charset=UTF-8")
        .send_string(body);
    match request {
        Ok(resp) => {
            let status = resp.status();
            Ok((status, resp.into_string().unwrap_or_default()))
        }
        Err(ureq::Error::Status(code, resp)) => {
            Ok((code, resp.into_string().unwrap_or_default()))
        }
        Err(err) => Err(format!("{url}: {err}")),
    }
}
/// GET `url` and return the HTTP status (2xx/4xx both come back as
/// numbers; only transport-level failures are reported as Err).
fn http_status(url: &str) -> Result<u16, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build();
    match agent.get(url).call() {
        Ok(resp) => Ok(resp.status()),
        Err(ureq::Error::Status(code, _)) => Ok(code),
        Err(err) => Err(format!("{url}: {err}")),
    }
}
/// Start the standalone engine as a detached daemon (idempotent). The
/// engine + the auth-injecting proxy MUST outlive this process, so the
/// work is done by a hidden `s2udio rq __serve` child: it spawns rqbit,
/// registers itself in the state file, prints a `READY` line on its
/// stdout (piped to us), and then runs a shutdown loop until stopped.
fn start() -> Result<(), String> {
    if let Some(reg) = registered_running() {
        println!("rqbit web UI already running: {}", reg.web_url);
        return Ok(());
    }
    let exe = std::env::current_exe()
        .map_err(|err| format!("Cannot find the s2udio binary: {err}"))?;
    let mut child = std::process::Command::new(&exe)
        .arg("rq")
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|err| format!("Failed to spawn the rqbit daemon: {err}"))?;
    let mut stdout = child.stdout.take().expect("daemon stdout is piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        use std::io::BufRead;
        let _ = std::io::BufReader::new(&mut stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(line) if line.starts_with("READY") => {}
        Ok(other) => {
            let _ = child.kill();
            return Err(format!("rqbit daemon failed to start: {other}"));
        }
        Err(_) => {
            let _ = child.kill();
            return Err("rqbit daemon did not become ready within 15 s".to_owned());
        }
    }
    let reg = registered_running()
        .ok_or_else(|| "rqbit daemon started but did not register".to_owned())?;
    println!("rqbit web UI: {}", reg.web_url);
    println!("stop it with `s2udio rq stop`");
    Ok(())
}
/// The daemon: own the engine + proxy until SIGTERM/SIGINT (or until the
/// engine child dies), keeping the registration file current.
fn serve() -> Result<(), String> {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let config = torrent_config();
    let mut engine = crate::core::torrent::start_engine(&config)?;
    let engine_port = engine
        .base_url()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(0);
    let reg = RqEngineFile {
        pid: std::process::id(),
        web_url: engine.web_url(),
        engine_port,
        cache_dir: engine.cache_dir.display().to_string(),
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    write_registration(&reg)?;
    println!("READY {}", reg.web_url);
    loop {
        if SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if !engine.is_running() {
            let _ = unregister();
            return Err("rqbit engine exited unexpectedly".to_owned());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    drop(engine);
    let _ = unregister();
    Ok(())
}
static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(
    false,
);
extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
}
/// What the window check found, i.e. whether `open_web_ui` still has to
/// launch a window.
enum WebUiWindow {
    /// A window showing the current URL was found and focused.
    Focused,
    /// Window(s) for the web UI existed but showed an OLDER URL (an engine
    /// from before a restart, i.e. a dead proxy port): they were closed, a
    /// fresh window is needed.
    Replaced,
    /// No window for the web UI is open.
    Absent,
    /// The window manager cannot be asked (no KWin scripting / qdbus6 /
    /// journalctl): a duplicate window may result.
    Unknown,
}

/// The URL the open app window was created with
/// (`~/.cache/s2udio/rqbit-webui.json`). The proxy port changes on every
/// engine start, so the record is what tells a live window from a stale
/// one — the window itself is only identifiable by the chromium class,
/// which carries the host and path but not the port.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WebUiWindowState {
    #[serde(default)]
    url: String,
}

/// `~/.cache/s2udio/rqbit-webui.json`.
fn webui_state_path() -> Option<PathBuf> {
    crate::shared::paths::s2udio_cache_dir().map(|dir| dir.join("rqbit-webui.json"))
}

/// Remember which URL the opened window shows.
fn remember_webui_window(url: &str) {
    let Some(path) = webui_state_path() else {
        return;
    };
    let state = WebUiWindowState { url: url.to_owned() };
    match serde_json::to_string(&state) {
        Ok(content) => {
            let _ = std::fs::write(path, content);
        }
        Err(err) => log::warn!(error:? = err; "Failed to serialize the web UI window state"),
    }
}

/// The URL the recorded window was opened with ("" = unknown).
fn recorded_webui_url() -> String {
    webui_state_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|content| serde_json::from_str::<WebUiWindowState>(&content).ok())
        .map(|state| state.url)
        .unwrap_or_default()
}

/// The KWin script that finds the web-UI window: chromium's
/// `--class`/`--wayland-app-id` are ignored on Wayland, so the window is
/// matched by the class chromium derives from the URL
/// (`chrome-127.0.0.1__web_-Default` for `http://127.0.0.1:<port>/web/` —
/// the port is not part of it). `close` makes the script close the
/// (stale) window(s) instead of focusing the first one; `nonce` marks the
/// single `console.info` line this run writes to the journal.
fn kwin_webui_script(nonce: &str, close: bool) -> String {
    format!(
        r#"// s2udio: find the rqbit web-UI chromium app window (round 81).
var ws = workspace;
var list = ws.windowList();
var found = null;
var n = 0;
for (var i = 0; i < list.length; i++) {{
    var w = list[i];
    if (w.resourceName === 'chromium' && String(w.resourceClass).indexOf('chrome-127.0.0.1__web_') === 0) {{
        if (found === null) {{ found = w; }}
        n++;
    }}
}}
if (n === 0) {{
    console.info('{nonce} MISS');
}} else {{
    // Report the finding BEFORE acting: closing a window can throw and
    // would otherwise swallow the line the caller waits for.
    console.info('{nonce} FOUND ' + n);
    if ({close}) {{
        var closed = 0;
        for (var j = 0; j < list.length; j++) {{
            var c = list[j];
            if (c.resourceName === 'chromium' && String(c.resourceClass).indexOf('chrome-127.0.0.1__web_') === 0) {{
                // KWin 6's scripting API has no `close()`; the method is
                // `closeWindow()` (tried first, `close()` kept as a
                // fallback for other KWin versions).
                try {{
                    if (typeof c.closeWindow === 'function') {{ c.closeWindow(); }} else {{ c.close(); }}
                    closed++;
                }} catch (e) {{ console.info('{nonce} ERR ' + e); }}
            }}
        }}
        console.info('{nonce} CLOSED ' + closed);
    }} else {{
        try {{
            ws.activeWindow = found;
            console.info('{nonce} HIT');
        }} catch (e) {{
            console.info('{nonce} ERR ' + e);
        }}
    }}
}}
"#
    )
}

/// Run `bin` with `args` and return its trimmed stdout (None on failure).
fn run_capture(bin: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The first user-journal line containing `marker` (KWin scripts have no
/// file IO — `console.info` lands in the journal).
fn journal_line_with(marker: &str) -> Option<String> {
    let out = std::process::Command::new("journalctl")
        .args(["--user", "--since", "-5 s", "--no-pager"])
        .output()
        .ok()?;
    // The LAST matching line is the current state of the run (a script
    // writes FOUND first, then HIT/CLOSED/ERR).
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.contains(marker))
        .next_back()
        .map(str::to_owned)
}

/// Ask KWin to focus — or, when the window shows an older URL, to close —
/// an existing web-UI window. This is the only window-manager API on this
/// KDE/Wayland session: wmctrl/xdotool are X11-only and not installed, and
/// KWin's D-Bus has no plain "activate window" call, so a tiny script is
/// loaded, run and its journal line read back.
fn check_webui_window(url: &str) -> WebUiWindow {
    if which::which("qdbus6").is_err() || which::which("journalctl").is_err() {
        return WebUiWindow::Unknown;
    }
    let Some(script_path) =
        crate::shared::paths::s2udio_cache_dir().map(|dir| dir.join("rqbit-focus.js"))
    else {
        return WebUiWindow::Unknown;
    };
    let stale = !recorded_webui_url().is_empty() && recorded_webui_url() != url;
    let nonce = format!("S2UWEBUI{}", std::process::id());
    if std::fs::write(&script_path, kwin_webui_script(&nonce, stale)).is_err() {
        return WebUiWindow::Unknown;
    }
    let Some(script_path) = script_path.to_str() else {
        return WebUiWindow::Unknown;
    };
    // loadScript hands back the index of THIS script object; run() must be
    // called on that object (an older index silently does nothing).
    let Some(index) = run_capture(
        "qdbus6",
        &[
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting.loadScript",
            script_path,
        ],
    )
    .and_then(|out| out.trim().parse::<u32>().ok()) else {
        return WebUiWindow::Unknown;
    };
    let _ = run_capture("qdbus6", &["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.start"]);
    let script_object = format!("/Scripting/Script{index}");
    let _ = run_capture(
        "qdbus6",
        &["org.kde.KWin", &script_object, "org.kde.kwin.Script.run"],
    );
    // The script runs on KWin's thread: poll the journal briefly for this
    // run's marker. `FOUND` says a window exists (in close mode the close
    // request then follows, so keep polling for `CLOSED`), `HIT` that it
    // was activated, `MISS` that there is none.
    let mut found = false;
    for _ in 0..15 {
        if let Some(line) = journal_line_with(&nonce) {
            if line.contains("MISS") {
                return WebUiWindow::Absent;
            }
            if line.contains("ERR") {
                return WebUiWindow::Unknown;
            }
            if line.contains("HIT") {
                return WebUiWindow::Focused;
            }
            if line.contains("CLOSED") {
                return WebUiWindow::Replaced;
            }
            if line.contains("FOUND") {
                found = true;
                if !stale {
                    return WebUiWindow::Focused;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    if found {
        return if stale { WebUiWindow::Replaced } else { WebUiWindow::Focused };
    }
    WebUiWindow::Unknown
}

/// Open the rqbit web UI (round 81). An already-open window is **focused**
/// instead of adding a second one (chromium would happily open another:
/// the second launcher exits 0 and the running browser adds a window), a
/// window left over from an earlier engine (dead proxy port) is closed and
/// replaced, and a chromium-family browser gets the requested app-window
/// form — `chromium --app=<url> --frameless`; with no chromium installed
/// the system default browser (`xdg-open`) is used. `rq open`, the tray
/// and the Settings row all go through this, so the web UI always behaves
/// the same way.
pub fn open_web_ui(url: &str) -> Result<(), String> {
    match check_webui_window(url) {
        WebUiWindow::Focused => return Ok(()),
        WebUiWindow::Replaced | WebUiWindow::Absent | WebUiWindow::Unknown => {}
    }
    launch_web_ui(url)
}

/// Launch a fresh app window for `url` (`--app=<url> --frameless`, else the
/// default browser).
fn launch_web_ui(url: &str) -> Result<(), String> {
    use std::process::Stdio;
    // Any chromium-family binary serves the app window; the first one on
    // PATH wins (this machine has chromium).
    for bin in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ] {
        let Ok(path) = which::which(bin) else {
            continue;
        };
        // A dedicated profile: the app window keeps its own lifecycle
        // (quitting the user's own browser must not close it) and never
        // mixes with their browsing session. The window class chromium
        // derives from the URL is unaffected (it carries the profile
        // name, not the profile directory).
        let profile = crate::shared::paths::s2udio_cache_dir()
            .map(|dir| dir.join("chromium-webui"));
        let mut command = std::process::Command::new(path);
        if let Some(profile) = profile.as_deref() {
            command
                .arg(format!("--user-data-dir={}", profile.display()))
                .arg("--no-first-run")
                .arg("--no-default-browser-check");
        }
        match command
            .arg(format!("--app={url}"))
            .arg("--frameless")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => {
                remember_webui_window(url);
                return Ok(());
            }
            // A chromium that will not launch falls through to the next
            // candidate (and finally to the default browser).
            Err(err) => log::warn!(bin, error:? = err; "Failed to launch the chromium app window"),
        }
    }
    match std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => {
            remember_webui_window(url);
            Ok(())
        }
        Err(err) => {
            eprintln!("Could not open a browser (xdg-open): {err}");
            eprintln!("Open the URL manually: {url}");
            Err(format!("Could not open a browser: {err}"))
        }
    }
}

/// The tray icon's state (round 81, `s2udio rq tray`).
/// The colour the tray glyph is drawn in: the current KDE colour scheme's
/// window foreground (`kdeglobals`), so the monochrome icon matches the
/// panel it sits on (`252,252,252` on this dark scheme). Falls back to
/// near-white.
fn tray_icon_color() -> [u8; 3] {
    let Some(path) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config/kdeglobals"))
    else {
        return [252, 252, 252];
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return [252, 252, 252];
    };
    let mut in_window_section = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_window_section = line.eq_ignore_ascii_case("[Colors:Window]");
            continue;
        }
        if in_window_section
            && let Some(value) = line.strip_prefix("ForegroundNormal=")
        {
            let parts: Vec<u8> =
                value.split(',').filter_map(|part| part.trim().parse().ok()).collect();
            if parts.len() == 3 {
                return [parts[0], parts[1], parts[2]];
            }
        }
    }
    [252, 252, 252]
}

/// Point-in-polygon (ray casting) in the glyph's 22-unit space.
fn point_in_polygon(x: f64, y: f64, polygon: &[(f64, f64); 7]) -> bool {
    let mut inside = false;
    let mut j = polygon.len() - 1;
    for i in 0..polygon.len() {
        let (xi, yi) = polygon[i];
        let (xj, yj) = polygon[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The tray glyph's three shapes in the icon's 22-unit space — the same
/// geometry as `assets/icons/s2rq-torrent.svg`: a ring plus an up and a
/// down arrow.
const TRAY_RING_CENTER: f64 = 11.0;
const TRAY_RING_RADIUS: f64 = 8.4;
const TRAY_RING_HALF_STROKE: f64 = 0.75;
const TRAY_UP_ARROW: [(f64, f64); 7] = [
    (11.0, 5.6),
    (13.5, 8.3),
    (11.9, 8.3),
    (11.9, 10.6),
    (10.1, 10.6),
    (10.1, 8.3),
    (8.5, 8.3),
];
const TRAY_DOWN_ARROW: [(f64, f64); 7] = [
    (11.0, 16.4),
    (8.5, 13.7),
    (10.1, 13.7),
    (10.1, 11.4),
    (11.9, 11.4),
    (11.9, 13.7),
    (13.5, 13.7),
];

/// One tray pixmap at `size` px: ARGB32 in network byte order (the
/// StatusNotifierItem `IconPixmap` layout), 4x4 supersampled for smooth
/// edges.
///
/// A pixmap rather than only an icon name: a themed name depends on the
/// host's icon lookup (plasmashell drew a `?` placeholder for the
/// installed `s2rq-torrent.svg`), while a pixmap is drawn exactly as
/// given. The desktop entry keeps using the theme file — same geometry,
/// same colour.
fn tray_icon_pixmap(size: i32, [red, green, blue]: [u8; 3]) -> ksni::Icon {
    const SUBSAMPLES: i32 = 4;
    let size = size.max(1);
    let scale = 22.0 / f64::from(size);
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut hits = 0u32;
            for sy in 0..SUBSAMPLES {
                for sx in 0..SUBSAMPLES {
                    let u = (f64::from(x) + (f64::from(sx) + 0.5) / f64::from(SUBSAMPLES))
                        * scale;
                    let v = (f64::from(y) + (f64::from(sy) + 0.5) / f64::from(SUBSAMPLES))
                        * scale;
                    let distance = ((u - TRAY_RING_CENTER).powi(2)
                        + (v - TRAY_RING_CENTER).powi(2))
                    .sqrt();
                    let on_ring = (distance - TRAY_RING_RADIUS).abs() <= TRAY_RING_HALF_STROKE;
                    if on_ring
                        || point_in_polygon(u, v, &TRAY_UP_ARROW)
                        || point_in_polygon(u, v, &TRAY_DOWN_ARROW)
                    {
                        hits += 1;
                    }
                }
            }
            let alpha = (hits * 255 / (SUBSAMPLES * SUBSAMPLES) as u32) as u8;
            data.extend_from_slice(&[alpha, red, green, blue]);
        }
    }
    ksni::Icon { width: size, height: size, data }
}

/// The tray pixmaps (every size a panel may ask for).
fn tray_icon_pixmaps() -> Vec<ksni::Icon> {
    let color = tray_icon_color();
    [16, 22, 24, 32, 48].iter().map(|size| tray_icon_pixmap(*size, color)).collect()
}

/// Install the tray icon into the user's hicolor theme (both the `status`
/// and the `apps` section, so the tray and the desktop entry find it) when
/// it is missing or stale — the tray then works on a machine where
/// setup.sh has not run. Returns the XDG icon root for the
/// StatusNotifierItem's `IconThemePath`.
fn ensure_tray_icon() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("HOME")?)
        .join(".local/share/icons");
    for section in ["status", "apps"] {
        let dir = root.join("hicolor/scalable").join(section);
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let path = dir.join(format!("{TORRENT_ICON}.svg"));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(TORRENT_ICON_SVG) {
            let _ = std::fs::write(&path, TORRENT_ICON_SVG);
        }
    }
    Some(root)
}

struct RqTray {
    /// The engine's web UI URL (the auth-injecting proxy URL).
    web_url: String,
    /// The glyph, drawn in code (see `tray_icon_pixmaps`).
    pixmaps: Vec<ksni::Icon>,
    /// The XDG icon root that holds the tray icon (the SNI
    /// `IconThemePath`).
    icon_theme_path: String,
    /// When the last left click arrived, for the double-click detection
    /// (`activate` is the only click signal the spec offers).
    last_activate: Option<Instant>,
    /// Set by the menu's `Shutdown` entry: the tray process leaves (its
    /// icon disappears) once the engine is stopped.
    done: Arc<AtomicBool>,
}

impl ksni::Tray for RqTray {
    fn id(&self) -> String {
        "s2udio-rqbit".to_owned()
    }
    /// The panel tooltip title.
    fn title(&self) -> String {
        "s2udio rqbit".to_owned()
    }
    /// Empty on purpose: with an icon name set, hosts resolve that name
    /// instead of using the pixmap below — and plasmashell drew its
    /// missing-icon `?` for the name (its Qt lookup does not find the
    /// file the KDE icon loader resolves). The pixmap is self-contained,
    /// so no host has to look anything up.
    fn icon_name(&self) -> String {
        String::new()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.pixmaps.clone()
    }
    /// The icon lives in the user's own hicolor theme (installed by
    /// `ensure_tray_icon` / setup.sh) — hand the host that path too.
    fn icon_theme_path(&self) -> String {
        self.icon_theme_path.clone()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: TORRENT_ICON.to_owned(),
            title: "s2udio rqbit".to_owned(),
            description: self.web_url.clone(),
            ..Default::default()
        }
    }
    /// A **double** left click opens the web UI (a single click does
    /// nothing, as requested). The spec has no double-click event — the
    /// panel sends one `Activate` per click — so two signals inside
    /// `DOUBLE_CLICK` count as one double click. The menu's `Open` entry
    /// opens it directly.
    fn activate(&mut self, _x: i32, _y: i32) {
        let now = Instant::now();
        let double = self
            .last_activate
            .is_some_and(|last| now.duration_since(last) <= DOUBLE_CLICK);
        self.last_activate = (!double).then_some(now);
        if !double {
            return;
        }
        if let Err(err) = open_web_ui(&self.web_url) {
            log::warn!(error:? = err; "Failed to open the rqbit web UI from the tray");
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Open".to_owned(),
                activate: Box::new(|tray: &mut RqTray| {
                    if let Err(err) = open_web_ui(&tray.web_url) {
                        log::warn!(error:? = err; "Failed to open the rqbit web UI from the tray");
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Shutdown".to_owned(),
                activate: Box::new(|tray: &mut RqTray| {
                    match stop_registered() {
                        Ok(_) => {}
                        Err(err) => log::warn!(error:? = err; "Failed to stop the rqbit engine"),
                    }
                    tray.done.store(true, Ordering::Relaxed);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// `s2udio rq tray` — a StatusNotifierItem tray icon that means "the
/// rqbit web UI is running" (round 81). The icon registers on the session
/// bus (KDE's StatusNotifierWatcher hosts it) while this process lives,
/// and the process leaves as soon as the engine is gone, so the icon
/// tracks the engine instead of needing its own state. Menu: `Open`
/// (chromium app window when available, else the default browser) and
/// `Shutdown` (`rq stop`).
fn tray() -> Result<(), String> {
    use ksni::blocking::TrayMethods;

    // One tray per machine: a second launch (the desktop entry clicked
    // twice) would show a second, identical icon.
    let pid_path = crate::shared::paths::s2udio_cache_dir().map(|dir| dir.join(TRAY_PID_FILE));
    if let Some(path) = &pid_path
        && let Ok(content) = std::fs::read_to_string(path)
        && let Ok(pid) = content.trim().parse::<u32>()
        && pid_alive(pid)
    {
        println!("s2udio rqbit tray already running (pid {pid})");
        return Ok(());
    }
    // The tray is the engine's UI: bring an engine up when none is
    // running (the S2RQ desktop entry calls `rq start` first — this is
    // the safety net for a bare `s2udio rq tray`).
    if registered_running().is_none() {
        start()?;
    }
    let reg = registered_running()
        .ok_or_else(|| "No rqbit engine is running".to_owned())?;
    let done = Arc::new(AtomicBool::new(false));
    let icon_theme_path = ensure_tray_icon()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let handle = RqTray {
        web_url: reg.web_url.clone(),
        pixmaps: tray_icon_pixmaps(),
        icon_theme_path,
        last_activate: None,
        done: Arc::clone(&done),
    }
        .spawn()
        .map_err(|err| format!("Failed to register the tray icon: {err}"))?;
    if let Some(path) = &pid_path {
        let _ = std::fs::write(path, std::process::id().to_string());
    }
    println!("s2udio rqbit tray running (web UI {})", reg.web_url);
    // The icon means "the engine is running": leave when it is gone —
    // stopped from the menu, from the TUI's stop row, from `rq stop`, or
    // a crashed engine.
    while !done.load(Ordering::Relaxed) && registered_running().is_some() {
        std::thread::sleep(Duration::from_millis(500));
    }
    handle.shutdown().wait();
    if let Some(path) = &pid_path {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}
