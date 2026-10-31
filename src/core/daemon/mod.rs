//! The always-on s2udio daemon.
//!
//! One detached per-user process owns the stream buffer proxy and — while no
//! TUI is attached — the queue's web-stream caretaker, so closing the TUI no
//! longer kills the proxy or leaves the next tagged queue row unresolved. The
//! TUI keeps the UI policy and becomes a client of the proxy; a missing
//! daemon falls back to the in-process proxy (the pre-daemon behaviour).
//!
//! The split is intentional for this tranche: the caretaker only acts while
//! **no** TUI is attached (a held `tui_hello` connection marks one), so the
//! TUI's own look ahead is authoritative whenever it runs. Mpv, the torrent
//! engine and the scheduler move behind the same socket in follow-up slices.
//!
//! See `docs/design/Backend/daemon.md`.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    path::PathBuf,
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use rustix::process::{Pid, Signal, kill_process};

use crate::config::cli::{AutostartAction, DaemonCmd, QatAction};

pub(crate) mod autostart;
pub(crate) mod caretaker;
pub(crate) mod proto;
pub(crate) mod qat;
pub(crate) mod server;

/// Overrides the socket path (tests, a second daemon in a scratch session).
const SOCKET_ENV: &str = "S2U_DAEMON_SOCKET";
/// How long every short-lived request waits for its reply. Registration is a
/// local JSON line, so this only fires when the daemon is wedged.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
/// How long `daemon start` waits for the socket to answer.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `daemon stop` waits for a graceful exit before signalling.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// The connection of a TUI that has attached. Kept for the whole process so
/// the daemon sees `attached > 0`; the kernel closes it on a crash.
#[cfg_attr(test, allow(dead_code))]
static ATTACH: OnceLock<UnixStream> = OnceLock::new();

/// Where the daemon listens and the CLI looks first.
///
/// `$XDG_RUNTIME_DIR/s2udio/daemon.sock`, falling back to a uid-scoped temp
/// dir (the same reasoning as the TUI sockets: no root-owned world-writable
/// path is used). `S2U_DAEMON_SOCKET` wins outright.
pub(crate) fn socket_path() -> PathBuf {
    if let Some(path) = crate::shared::env::ENV.var_os(SOCKET_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    let base = crate::shared::env::ENV
        .var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let uid = rustix::process::geteuid();
            std::env::temp_dir().join(format!("s2udio-{}", uid.as_raw()))
        });
    base.join("s2udio").join("daemon.sock")
}

/// The CLI/config values the daemon must inherit from whoever starts it.
#[derive(Debug, Clone, Default)]
pub(crate) struct LaunchOptions {
    pub config: Option<PathBuf>,
    pub address: Option<String>,
    pub password: Option<String>,
    pub partition: Option<String>,
}

impl LaunchOptions {
    fn from_args(args_config: Option<PathBuf>, args_address: Option<String>, args_password: Option<String>, partition: Option<String>) -> Self {
        Self { config: args_config, address: args_address, password: args_password, partition }
    }
}

/// One request/reply on a fresh connection.
pub(crate) fn request(req: &proto::Request) -> Result<proto::Reply> {
    let mut stream = connect()?;
    exchange(&mut stream, req)
}

/// Connect to the daemon with the short request timeout applied.
pub(crate) fn connect() -> Result<UnixStream> {
    let stream = UnixStream::connect(socket_path()).context("the s2udio daemon is not running")?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    Ok(stream)
}

/// Write one request line and read one reply line.
pub(crate) fn exchange(stream: &mut UnixStream, req: &proto::Request) -> Result<proto::Reply> {
    let line = serde_json::to_string(req)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut reply = String::new();
    if reader.read_line(&mut reply)? == 0 {
        bail!("the daemon closed the connection without a reply");
    }
    let reply: proto::Reply = serde_json::from_str(reply.trim())?;
    match reply {
        proto::Reply::Error { message } => bail!("the daemon replied with an error: {message}"),
        other => Ok(other),
    }
}

/// The daemon's state, or an error when it is not running.
pub(crate) fn ping() -> Result<proto::DaemonInfo> {
    match request(&proto::Request::Ping)? {
        proto::Reply::Info(info) => Ok(info),
        other => Err(anyhow!("unexpected reply to ping: {other:?}")),
    }
}

/// Whether a daemon answers right now.
pub(crate) fn is_running() -> bool {
    ping().is_ok()
}

/// Ask the running daemon to change its QAT hotkey. The caller persists the
/// sidecar first; this makes the change live (the daemon rewrites the KWin
/// script and re-registers the shortcut).
pub(crate) fn set_qat_hotkey(chord: &str) -> Result<()> {
    match request(&proto::Request::QatHotkey { chord: chord.to_owned() })? {
        proto::Reply::Ok => Ok(()),
        other => Err(anyhow!("unexpected reply to the hotkey change: {other:?}")),
    }
}

/// The hotkey the daemon will use on its next start (the daemon.ron
/// sidecar, with the environment override applied by `QatConfig::from_env`).
pub(crate) fn configured_qat_hotkey() -> String {
    crate::config::daemon::DaemonConfigFile::load().qat_hotkey
}

/// Ask the daemon to raise the player. This is the no-live-TUI path for
/// `s2udio remote raise` and the MPRIS bridges: a live TUI raises itself over
/// its own socket (with its kitty/QAT session), and only when there is none
/// does the daemon launch/show the QAT.
pub(crate) fn raise() -> Result<()> {
    match request(&proto::Request::Qat { action: "raise".to_owned() })? {
        proto::Reply::Ok => Ok(()),
        other => Err(anyhow!("unexpected reply to raise: {other:?}")),
    }
}

/// Start the daemon if it is not already running, waiting until its socket
/// answers. Returns whether a daemon is up afterwards. Idempotent and safe to
/// call from every TUI start.
pub(crate) fn ensure_running(opts: &LaunchOptions) -> bool {
    if is_running() {
        return true;
    }
    if let Err(err) = spawn_serve(opts) {
        log::warn!(error:? = err; "Could not start the s2udio daemon");
        return false;
    }
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if is_running() {
            log::info!("s2udio daemon started");
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    log::warn!("s2udio daemon did not answer within {}s", START_TIMEOUT.as_secs());
    false
}

/// Spawn the detached hidden entry point (`s2udio daemon serve`).
fn spawn_serve(opts: &LaunchOptions) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the s2udio executable")?;
    let mut cmd = Command::new(exe);
    cmd.args(serve_argv(opts));
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    // The logger writes its own file; stderr only carries a panic or a
    // pre-logger failure, and losing it is exactly the round-110 lesson.
    if let Some(dir) = crate::shared::paths::s2udio_cache_dir()
        && std::fs::create_dir_all(&dir).is_ok()
        && let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("daemon.stderr.log"))
    {
        cmd.stderr(Stdio::from(file));
    } else {
        cmd.stderr(Stdio::null());
    }
    // SAFETY: `setsid` is async-signal-safe and the closure allocates
    // nothing; detaching the child from the TUI's session is the point.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("cannot spawn the s2udio daemon")?;
    log::debug!(pid = child.id(); "Spawned the s2udio daemon");
    Ok(())
}

/// Attach this TUI to the daemon. The stream is **held** for the process
/// lifetime: while it is open the daemon reports an attached TUI and stands
/// its caretaker down. Returns whether the attach succeeded.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn attach_tui() -> bool {
    if ATTACH.get().is_some() {
        return true;
    }
    let Ok(mut stream) = connect() else { return false };
    let request = proto::Request::TuiHello { pid: std::process::id() };
    match exchange(&mut stream, &request) {
        Ok(proto::Reply::Ok) => {
            // The stream is never read from; it only has to stay open.
            let _ = ATTACH.set(stream);
            log::info!("Attached to the s2udio daemon");
            true
        }
        Ok(other) => {
            log::warn!(reply:? = other; "Unexpected reply to tui_hello");
            false
        }
        Err(err) => {
            log::warn!(error:? = err; "Could not attach to the s2udio daemon");
            false
        }
    }
}

// ------------------------------------------------------------------ CLI ----

/// `s2udio daemon start|stop|status|serve`.
pub(crate) fn run(
    cmd: DaemonCmd,
    args_config: Option<PathBuf>,
    args_address: Option<String>,
    args_password: Option<String>,
    partition: Option<String>,
) -> Result<()> {
    let opts = LaunchOptions::from_args(args_config, args_address, args_password, partition);
    match cmd {
        DaemonCmd::Start => {
            if is_running() {
                println!("The s2udio daemon is already running.");
            } else if !ensure_running(&opts) {
                bail!("The s2udio daemon did not start within {}s.", START_TIMEOUT.as_secs());
            } else {
                println!("Started the s2udio daemon.");
            }
            print_status()?;
        }
        DaemonCmd::Status => {
            let info = ping().context("The s2udio daemon is not running")?;
            print_info(&info);
        }
        DaemonCmd::Stop => stop()?,
        DaemonCmd::Autostart { action } => autostart_action(action)?,
        DaemonCmd::Hotkey { chord } => hotkey_action(&chord)?,
        DaemonCmd::Qat { action } => qat_action(action)?,
        DaemonCmd::Serve { config, address, password, partition } => {
            let opts = LaunchOptions {
                config: config.or(opts.config),
                address: address.or(opts.address),
                password: password.or(opts.password),
                partition: partition.or(opts.partition),
            };
            server::serve(&opts)?;
        }
    }
    Ok(())
}

/// `s2udio daemon hotkey <chord>`: persist the new chord and make it live in
/// the running daemon (if there is one).
fn hotkey_action(chord: &str) -> Result<()> {
    let chord = crate::config::daemon::normalize_hotkey(chord)
        .context("The QAT hotkey cannot be empty or contain quotes/newlines")?;
    let mut file = crate::config::daemon::DaemonConfigFile::load();
    file.qat_hotkey = chord.clone();
    file.save().context("Cannot save the daemon settings")?;
    match set_qat_hotkey(&chord) {
        Ok(()) => println!("QAT hotkey: {chord} (daemon updated)"),
        Err(err) => println!("QAT hotkey: {chord} (saved; start the daemon to apply: {err})"),
    }
    Ok(())
}

/// `s2udio daemon autostart [on|off|status]`: what Settings > Advanced's
/// toggle does, without opening the panel.
fn autostart_action(action: AutostartAction) -> Result<()> {
    use crate::config::cli::AutostartAction as A;
    let state = |on: bool| if on { "enabled" } else { "disabled" };
    match action {
        A::Status => {
            println!(
                "daemon autostart: {} ({})",
                state(crate::core::daemon::autostart::enabled()),
                crate::core::daemon::autostart::unit_path().display()
            );
        }
        A::On | A::Off => {
            let on = matches!(action, A::On);
            crate::core::daemon::autostart::set_enabled(on)?;
            println!("daemon autostart {}", state(on));
            if !on {
                println!("(the running daemon was left alone; use `s2udio daemon stop` to stop it)");
            }
        }
    }
    Ok(())
}

/// `s2udio daemon qat [toggle|raise|show|hide|status]`: the same action the
/// registered hotkey fires, over the same socket.
fn qat_action(action: QatAction) -> Result<()> {
    if action == QatAction::Status {
        let info = ping().context("The s2udio daemon is not running")?;
        print_info(&info);
        return Ok(());
    }
    let action = match action {
        QatAction::Toggle => "toggle",
        QatAction::Raise => "raise",
        QatAction::Show => "show",
        QatAction::Hide => "hide",
        QatAction::Status => unreachable!("handled above"),
    };
    request(&proto::Request::Qat { action: action.to_owned() })
        .context("The s2udio daemon did not answer the QAT request")?;
    println!("QAT: {action}");
    Ok(())
}

fn stop() -> Result<()> {
    let info = ping().context("The s2udio daemon is not running")?;
    let _ = request(&proto::Request::Stop);
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if !is_running() {
            println!("Stopped the s2udio daemon.");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // The request was received but the daemon did not leave; the socket may
    // be wedged. Signal it, then give up loudly.
    let Some(pid) = i32::try_from(info.pid).ok().and_then(Pid::from_raw) else {
        bail!("The s2udio daemon (pid {}) did not stop and its pid is invalid.", info.pid);
    };
    let _ = kill_process(pid, Signal::TERM);
    std::thread::sleep(Duration::from_millis(1500));
    if !is_running() {
        println!("Stopped the s2udio daemon (SIGTERM).");
        return Ok(());
    }
    let _ = kill_process(pid, Signal::KILL);
    bail!("The s2udio daemon (pid {}) did not stop.", info.pid);
}

fn print_status() -> Result<()> {
    let info = ping()?;
    print_info(&info);
    Ok(())
}

fn print_info(info: &proto::DaemonInfo) {
    let uptime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|now| now.as_secs().saturating_sub(info.started_at))
        .unwrap_or_default();
    let proxy = info
        .proxy_port
        .map_or_else(|| "unavailable".to_owned(), |port| format!("127.0.0.1:{port}"));
    println!(
        "s2udio daemon: pid {} · up {}s · proxy {} · MPD {} · attached {} · queue {} · warmed {}",
        info.pid,
        uptime,
        proxy,
        if info.mpd_connected { "connected" } else { "disconnected" },
        info.attached,
        info.queue_len,
        info.warmed,
    );
    if let Some(hotkey) = &info.hotkey {
        println!("QAT hotkey: {hotkey}");
    }
    if let Some(err) = &info.last_error {
        println!("last error: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_honours_the_override() {
        // The override is the test seam: both the daemon and the client must
        // resolve the same path or `ensure_running` can never see a test
        // daemon.
        crate::shared::env::ENV.set(SOCKET_ENV.to_owned(), "/tmp/s2u-daemon-test.sock".to_owned());
        assert_eq!(socket_path(), PathBuf::from("/tmp/s2u-daemon-test.sock"));
        crate::shared::env::ENV.remove(SOCKET_ENV);
        let fallback = socket_path();
        assert!(fallback.ends_with("s2udio/daemon.sock"), "got {fallback:?}");
    }

    #[test]
    fn daemon_launch_args_are_forwarded_verbatim() {
        // `daemon serve` is spawned by us, so its argv shape is a contract:
        // losing --partition would make the daemon watch the default MPD
        // partition while the TUI watches another one.
        let opts = LaunchOptions {
            config: Some(PathBuf::from("/tmp/c.ron")),
            address: Some("127.0.0.1:6601".to_owned()),
            password: Some("secret".to_owned()),
            partition: Some("work".to_owned()),
        };
        let argv = serve_argv(&opts);
        assert_eq!(argv[0], "daemon");
        assert_eq!(argv[1], "serve");
        assert!(argv.windows(2).any(|w| w == ["--config", "/tmp/c.ron"]));
        assert!(argv.windows(2).any(|w| w == ["--address", "127.0.0.1:6601"]));
        assert!(argv.windows(2).any(|w| w == ["--password", "secret"]));
        assert!(argv.windows(2).any(|w| w == ["--partition", "work"]));
    }

    #[test]
    fn stop_timeout_is_longer_than_the_request_timeout() {
        // A stop request must be able to arrive and be answered before the
        // client gives up on the socket.
        assert!(STOP_TIMEOUT > REQUEST_TIMEOUT);
        assert!(START_TIMEOUT > Duration::from_secs(0));
    }
}

/// The exact argv of the detached entry point. One function so the spawn and
/// the test cannot drift (a lost `--partition` would make the daemon watch a
/// different MPD queue than the TUI).
fn serve_argv(opts: &LaunchOptions) -> Vec<String> {
    let mut argv = vec!["daemon".to_owned(), "serve".to_owned()];
    if let Some(config) = &opts.config {
        argv.push("--config".to_owned());
        argv.push(config.to_string_lossy().into_owned());
    }
    if let Some(address) = &opts.address {
        argv.push("--address".to_owned());
        argv.push(address.clone());
    }
    if let Some(password) = &opts.password {
        argv.push("--password".to_owned());
        argv.push(password.clone());
    }
    if let Some(partition) = &opts.partition {
        argv.push("--partition".to_owned());
        argv.push(partition.clone());
    }
    argv
}
