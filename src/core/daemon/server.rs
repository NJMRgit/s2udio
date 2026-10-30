//! The daemon process: IPC socket, MPD watcher and (while no TUI is
//! attached) the queue caretaker.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use parking_lot::Mutex;

use super::{
    LaunchOptions,
    caretaker::{self, WarmState},
    proto::{DaemonInfo, Reply, Request},
    qat,
    socket_path,
};
use crate::{
    config::cli_config::CliConfig,
    core::stream_cache::{self, StreamClass},
    mpd::{client::Client, mpd_client::MpdClient},
    shared::config_read::read_cli_config,
};

/// How often MPD is polled. The caretaker is cheap when nothing is warmable;
/// the TUI polls as fast as its status interval, but the daemon has no UI to
/// keep responsive and one resolve is seconds anyway.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Reconnect delay after MPD goes away (or was not there at startup).
const MPD_RETRY: Duration = Duration::from_secs(2);

/// Everything the IPC handlers and the watcher share.
pub(crate) struct State {
    pid: u32,
    started_at: u64,
    cache_dir: Option<PathBuf>,
    proxy_port: Option<u16>,
    attached: AtomicUsize,
    mpd_connected: AtomicBool,
    queue_len: AtomicUsize,
    warmed: AtomicU64,
    stop: AtomicBool,
    last_error: Mutex<String>,
    qat: Arc<qat::QatConfig>,
}

impl State {
    fn new(cache_dir: Option<PathBuf>, qat: Arc<qat::QatConfig>) -> Self {
        Self {
            pid: std::process::id(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            cache_dir,
            proxy_port: stream_cache::current_port(),
            attached: AtomicUsize::new(0),
            mpd_connected: AtomicBool::new(false),
            queue_len: AtomicUsize::new(0),
            warmed: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            last_error: Mutex::new(String::new()),
            qat,
        }
    }

    fn info(&self) -> DaemonInfo {
        let last_error = self.last_error.lock().clone();
        DaemonInfo {
            pid: self.pid,
            started_at: self.started_at,
            proxy_port: self.proxy_port,
            mpd_connected: self.mpd_connected.load(Ordering::Relaxed),
            attached: self.attached.load(Ordering::Relaxed),
            queue_len: self.queue_len.load(Ordering::Relaxed),
            warmed: self.warmed.load(Ordering::Relaxed),
            last_error: (!last_error.is_empty()).then_some(last_error),
            hotkey: Some(self.qat.hotkey.clone()),
        }
    }

    pub(crate) fn set_error(&self, message: impl Into<String>) {
        *self.last_error.lock() = message.into();
    }

    pub(crate) fn attached(&self) -> usize {
        self.attached.load(Ordering::Relaxed)
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub(crate) fn cache_dir(&self) -> Option<&std::path::Path> {
        self.cache_dir.as_deref()
    }

    pub(crate) fn note_warmed(&self) {
        self.warmed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_queue_len(&self, len: usize) {
        self.queue_len.store(len, Ordering::Relaxed);
    }
}

/// Run the daemon until a `stop` request arrives (or the process is killed).
pub(crate) fn serve(opts: &LaunchOptions) -> Result<()> {
    let cli = match read_cli_config(opts.config.as_deref(), opts.address.clone(), opts.password.clone()) {
        Ok(cli) => cli,
        Err(err) => {
            // The logger is not up yet; stderr is the panic log the starter
            // redirected, so this is not lost.
            eprintln!("s2udio daemon: could not read the config ({err}); using defaults");
            CliConfig::default()
        }
    };
    let _log = crate::shared::logging::init_s2udio_daemon().context("cannot start the daemon logger")?;
    // The proxy is the daemon's reason to exist; starting it before the
    // socket means a client that finds the socket also finds the port.
    stream_cache::init(cli.cache_dir.as_deref());
    let qat = Arc::new(qat::QatConfig::from_env());
    let state = Arc::new(State::new(cli.cache_dir.clone(), Arc::clone(&qat)));

    let path = socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }
    if path.exists() && UnixStream::connect(&path).is_err() {
        // A crashed daemon leaves the inode behind; nobody answers on it, so
        // it is safe to replace.
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));

    log::info!(
        pid = state.pid,
        socket:? = path,
        proxy_port:? = state.proxy_port,
        mpd:? = cli.address;
        "s2udio daemon started"
    );

    {
        let state = Arc::clone(&state);
        thread::Builder::new()
            .name("s2udio-daemon-ipc".to_owned())
            .spawn(move || accept_loop(listener, state))
            .context("cannot spawn the daemon IPC thread")?;
    }
    // The QAT controller and its shortcut are best-effort: a headless session
    // (no KWin, no session bus) logs and keeps the caretaker/proxy running.
    match qat::start_dbus(Arc::clone(&qat)) {
        Ok(()) => {}
        Err(err) => log::warn!(error:? = err; "daemon: QAT D-Bus service unavailable"),
    }
    qat::install_hotkey(Arc::clone(&qat));
    {
        let state = Arc::clone(&state);
        let cli = cli.clone();
        let partition = opts.partition.clone();
        thread::Builder::new()
            .name("s2udio-daemon-mpd".to_owned())
            .spawn(move || watcher_loop(state, cli, partition))
            .context("cannot spawn the daemon MPD thread")?;
    }

    while !state.stopping() {
        thread::sleep(Duration::from_millis(200));
    }
    let _ = std::fs::remove_file(&path);
    log::info!("s2udio daemon stopped");
    Ok(())
}

fn accept_loop(listener: UnixListener, state: Arc<State>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            log::warn!("daemon: failed to accept a socket connection");
            continue;
        };
        let state = Arc::clone(&state);
        let _ = thread::Builder::new().name("s2udio-daemon-conn".to_owned()).spawn(move || {
            if let Err(err) = handle_connection(stream, &state) {
                log::warn!(error:? = err; "daemon: connection ended with an error");
            }
        });
    }
}

/// One request/reply connection, except a `tui_hello`, whose connection is
/// kept open and counted until it closes.
fn handle_connection(mut stream: UnixStream, state: &Arc<State>) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone().context("cannot clone the IPC stream")?);
    let mut held_tui = false;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) => {
                log::debug!(error:? = err; "daemon: IPC read ended");
                break;
            }
        }
        let request: Request = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(err) => {
                let _ = write_reply(&mut stream, &Reply::error(format!("invalid request: {err}")));
                break;
            }
        };
        let reply = match request {
            Request::Ping => Reply::Info(state.info()),
            Request::Stop => {
                state.stop.store(true, Ordering::Relaxed);
                Reply::Ok
            }
            Request::TuiHello { pid } => {
                if !held_tui {
                    held_tui = true;
                    state.attached.fetch_add(1, Ordering::Relaxed);
                    log::info!(pid; "daemon: a TUI attached; the caretaker stands down");
                }
                Reply::Ok
            }
            Request::Register { class, original, upstream } => match stream_class(&class) {
                Some(class) => {
                    let _url = stream_cache::proxy_url(state.cache_dir(), class, &original, &upstream);
                    Reply::Port { port: stream_cache::current_port() }
                }
                None => Reply::error(format!("unknown stream class {class:?}")),
            },
            Request::Preload { class, original, upstream } => match stream_class(&class) {
                Some(class) => {
                    stream_cache::preload(state.cache_dir(), class, &original, &upstream);
                    Reply::Ok
                }
                None => Reply::error(format!("unknown stream class {class:?}")),
            },
            Request::OriginalLink { key } => Reply::Link { link: stream_cache::original_link_for_key(&key) },
            Request::NeedsRefresh { url } => Reply::Refresh { needed: stream_cache::needs_upstream_refresh(&url) },
            Request::Qat { action } => match action.as_str() {
                "toggle" | "show" | "hide" => match qat::toggle(&state.qat) {
                    Ok(()) => Reply::Ok,
                    Err(err) => Reply::error(format!("cannot toggle the QAT: {err}")),
                },
                "raise" => match qat::raise(&state.qat) {
                    Ok(()) => Reply::Ok,
                    Err(err) => Reply::error(format!("cannot raise the player: {err}")),
                },
                other => Reply::error(format!("unknown QAT action {other:?}")),
            },
        };
        if write_reply(&mut stream, &reply).is_err() {
            break;
        }
        if state.stopping() {
            break;
        }
    }
    if held_tui {
        state.attached.fetch_sub(1, Ordering::Relaxed);
        log::info!("daemon: the TUI detached");
    }
    Ok(())
}

fn write_reply(stream: &mut UnixStream, reply: &Reply) -> Result<()> {
    let mut line = serde_json::to_string(reply)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn stream_class(value: &str) -> Option<StreamClass> {
    StreamClass::from_str(value)
}

/// Poll MPD forever, reconnecting with a delay; run the caretaker while no
/// TUI is attached.
fn watcher_loop(state: Arc<State>, cli: CliConfig, partition: Option<String>) {
    let mut warm = WarmState::default();
    loop {
        if state.stopping() {
            return;
        }
        let client = Client::init(cli.address.clone(), cli.password.clone(), "s2udio-daemon", partition.clone(), false);
        let mut client = match client {
            Ok(client) => client,
            Err(err) => {
                state.set_error(format!("MPD connection failed: {err}"));
                log::warn!(error:? = err, address:? = cli.address; "daemon: cannot reach MPD; retrying");
                sleep_unless_stopping(&state, MPD_RETRY);
                continue;
            }
        };
        state.mpd_connected.store(true, Ordering::Relaxed);
        log::info!(address:? = cli.address; "daemon: connected to MPD");
        loop {
            if state.stopping() {
                return;
            }
            match poll_once(&mut client, &state, &cli, &mut warm) {
                Ok(()) => sleep_unless_stopping(&state, POLL_INTERVAL),
                Err(err) => {
                    state.set_error(format!("MPD poll failed: {err}"));
                    log::warn!(error:? = err; "daemon: MPD poll failed; reconnecting");
                    break;
                }
            }
        }
        state.mpd_connected.store(false, Ordering::Relaxed);
        sleep_unless_stopping(&state, MPD_RETRY);
    }
}

fn poll_once(
    client: &mut Client<'static>,
    state: &Arc<State>,
    cli: &CliConfig,
    warm: &mut WarmState,
) -> Result<()> {
    let status = client.get_status().context("MPD status")?;
    let queue = client.playlist_info().context("MPD playlist")?.unwrap_or_default();
    state.note_queue_len(queue.len());
    if state.attached() == 0 && !queue.is_empty() {
        caretaker::warm_queue(client, &status, &queue, cli.cache_dir.as_deref(), state, warm);
    }
    Ok(())
}

fn sleep_unless_stopping(state: &Arc<State>, duration: Duration) {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        if state.stopping() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::daemon::proto::Request;

    /// Two connected sockets, so the protocol can be driven without binding
    /// the real daemon path or touching the stream proxy.
    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
    }

    fn state() -> Arc<State> {
        Arc::new(State::new(None, Arc::new(qat::QatConfig::from_env())))
    }

    #[test]
    fn ping_reports_the_state_and_stop_sets_the_flag() {
        let state = state();
        let (mut client, server) = pair();
        let handle = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || handle_connection(server, &state).expect("connection"))
        };

        let info = match crate::core::daemon::exchange(&mut client, &Request::Ping).expect("ping reply") {
            Reply::Info(info) => info,
            other => panic!("expected info, got {other:?}"),
        };
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.attached, 0);
        assert!(!info.mpd_connected);

        let reply = crate::core::daemon::exchange(&mut client, &Request::Stop).expect("stop reply");
        assert_eq!(reply, Reply::Ok);
        handle.join().expect("handler");
        assert!(state.stopping());
    }

    #[test]
    fn a_held_tui_hello_counts_until_the_connection_closes() {
        let state = state();
        let (client, server) = pair();
        let handle = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || handle_connection(server, &state).expect("connection"))
        };
        let mut client = client;
        let reply = crate::core::daemon::exchange(&mut client, &Request::TuiHello { pid: 42 }).expect("hello reply");
        assert_eq!(reply, Reply::Ok);
        // Give the handler a moment to have bumped the counter after the reply.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while state.attached() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(state.attached(), 1, "the open connection is the attach");
        drop(client);
        handle.join().expect("handler");
        assert_eq!(state.attached(), 0, "closing the connection detaches");
    }

    #[test]
    fn a_malformed_request_gets_an_error_reply_not_a_panic() {
        let state = state();
        let (mut client, server) = pair();
        let handle = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || handle_connection(server, &state).expect("connection"))
        };
        client.write_all(b"{\"op\":\"nope\"}\n").expect("write");
        client.flush().expect("flush");
        let mut line = String::new();
        BufReader::new(&mut client).read_line(&mut line).expect("read reply");
        let reply: Reply = serde_json::from_str(line.trim()).expect("parse reply");
        assert!(matches!(reply, Reply::Error { .. }), "got {reply:?}");
        handle.join().expect("handler");
    }
}
