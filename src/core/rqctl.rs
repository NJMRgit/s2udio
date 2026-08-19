//! `s2udio rq start|stop|open` — shell control of the standalone rqbit
//! engine (round 43).
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
    os::unix::process::CommandExt,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::config::cli::RqCmd;

/// The registration file name inside the s2udio cache dir.
const STATE_FILE: &str = "rqbit.json";

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

/// SIGTERM, poll for exit (~2 s), then SIGKILL.
fn kill_pid(pid: u32) {
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
                false => Err(
                    "No rqbit engine is running (start one with `s2udio rq start` or Settings -> torrent)"
                        .to_owned(),
                ),
            }
        }
        RqCmd::Open => {
            let Some(reg) = registered_running() else {
                return Err(
                    "No rqbit engine is running (start one with `s2udio rq start` or Settings -> torrent)"
                        .to_owned(),
                );
            };
            open_url(&reg.web_url);
            println!("{}", reg.web_url);
            Ok(())
        }
        RqCmd::Check => check(),
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
    println!(
        "rqbit engine:   RUNNING (pid {}, web UI {})",
        reg.pid, reg.web_url
    );

    let mut ok = true;
    // Proxy base (the web URL minus the /web/ suffix).
    let proxy_base = reg
        .web_url
        .trim_end_matches("/web/")
        .trim_end_matches('/')
        .to_owned();

    // 1. Web UI through the proxy, no credentials.
    match http_status(&format!("{proxy_base}/web/")) {
        Ok(200) => println!("[PASS] web UI via proxy (no credentials)  -> 200"),
        other => {
            ok = false;
            println!("[FAIL] web UI via proxy (no credentials)  -> {other:?}");
        }
    }
    // 2. API through the proxy, no credentials.
    match http_status(&format!("{proxy_base}/stats")) {
        Ok(200) => println!("[PASS] API via proxy (no credentials)     -> 200"),
        other => {
            ok = false;
            println!("[FAIL] API via proxy (no credentials)     -> {other:?}");
        }
    }
    // 3. The engine port itself, no credentials -> must 401.
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

    if ok {
        println!("result: OK — the proxy is injecting auth correctly");
        Ok(())
    } else {
        Err("result: FAILED — see the [FAIL] lines above".to_owned())
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
        // New process group: terminal Ctrl+C / hangup never reaches the
        // daemon.
        .process_group(0)
        .spawn()
        .map_err(|err| format!("Failed to spawn the rqbit daemon: {err}"))?;

    // Wait for the daemon's READY line (bounded: 15 s covers the engine
    // spawn + readiness).
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
    // The daemon registered itself; print its URL.
    let reg = registered_running().ok_or_else(|| {
        "rqbit daemon started but did not register".to_owned()
    })?;
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
    // Signal readiness to the parent (it waits for this line).
    println!("READY {}", reg.web_url);
    loop {
        if SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if !engine.is_running() {
            // The rqbit child died on its own (crash): self-heal by
            // exiting and clearing the registration.
            let _ = unregister();
            return Err("rqbit engine exited unexpectedly".to_owned());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // Clean shutdown: dropping the engine kills rqbit + the proxy.
    drop(engine);
    let _ = unregister();
    Ok(())
}

static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Open `url` in the system browser (`xdg-open`); on failure the URL is
/// printed so it can be pasted manually.
fn open_url(url: &str) {
    use std::process::Stdio;
    match std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => {}
        Err(err) => {
            eprintln!("Could not open a browser (xdg-open): {err}");
            eprintln!("Open the URL manually: {url}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("rqctl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Point the state path at the temp dir via a write probe: we can't
        // easily redirect state_path(), so test the pieces it uses.
        let reg = RqEngineFile {
            pid: u32::MAX,
            web_url: "http://127.0.0.1:1/web/".to_owned(),
            engine_port: 3030,
            cache_dir: "/tmp/x".to_owned(),
            started_at: 1,
        };
        let json = serde_json::to_string(&reg).unwrap();
        let back: RqEngineFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, reg.pid);
        assert_eq!(back.web_url, reg.web_url);
        let _ = dir;
    }

    #[test]
    fn dead_pid_is_not_alive() {
        // 99_999_999 is beyond Linux's pid_max (~4M), so no process can
        // own it (and unlike -1/u32::MAX it is not the "signal everyone"
        // special case, which kill() reports as alive for root).
        assert!(!pid_alive(99_999_999));
        // Our own pid is alive.
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn stop_registered_without_state_is_a_noop() {
        // Never touch a live registration (a running engine's file must
        // survive tests) — skip when one exists.
        if read_state().is_some() {
            eprintln!("skipping: an rqbit engine is registered");
            return;
        }
        assert_eq!(stop_registered().expect("no state -> Ok(false)"), false);
    }
}
