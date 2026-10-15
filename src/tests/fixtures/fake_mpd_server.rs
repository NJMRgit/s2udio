//! A **scripted MPD server** over a real socket, so the production `Client`
//! can be driven end to end.
//!
//! The whole `▶▶` / resolve path decides what to do *inside* the closure handed
//! to `Ctx::command` — and that closure is typed `FnOnce(&mut Client<'_>)`
//! against the concrete socket client, so no `dyn MpdClient` fake can be
//! injected through the channel. Twelve fixes in this path were therefore each
//! verified in a pure function or at the event layer while the real defect sat
//! in the closure, which is why a green suite coexisted with a broken feature
//! for eight rounds.
//!
//! This rig closes that gap from the outside: it listens on a
//! `std::os::unix::net::UnixStream` pair and speaks enough of the MPD protocol
//! for the operations this path uses (`playlistinfo`, `playid`, `addid`,
//! `deleteid`, `status`, …). It keeps a **real queue with real song ids** and
//! hands new rows fresh ids exactly as MPD does — that id churn is the source
//! of most of the defects in this area.
//!
//! So a test can assert on the *end state MPD would have* — which row is
//! playing, under which id — rather than on the app's intention to play.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use crate::config::{MpdAddress, address::MpdPassword};
use crate::mpd::{client::Client, commands::Song};

/// A row of the fake playlist.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: u32,
    pub file: String,
}

/// The fake server's queue plus the record of what was asked of it.
#[derive(Debug, Default)]
pub struct FakeMpdState {
    pub rows: Vec<Row>,
    /// The id the next `addid` hands out.
    next_id: u32,
    /// The row MPD actually started (`playid` succeeded).
    pub playing: Option<u32>,
    /// Every command line received, in order — `playid 812`, `deleteid 814`, …
    pub commands: Vec<String>,
    /// A `playid` for an id the queue does not hold: the `No such song` ACK
    /// round `94dafaa` measured after an in-place swap. Recorded so a test can
    /// see the stale-id play that used to be swallowed.
    pub refused_playids: Vec<u32>,
}

impl FakeMpdState {
    pub fn new(rows: Vec<(u32, String)>) -> Self {
        let next_id = rows.iter().map(|(id, _)| *id).max().map_or(1, |max| max + 1);
        Self {
            rows: rows.into_iter().map(|(id, file)| Row { id, file }).collect(),
            next_id,
            playing: None,
            commands: Vec::new(),
            refused_playids: Vec::new(),
        }
    }

    pub fn file_of(&self, id: u32) -> Option<&str> {
        self.rows.iter().find(|row| row.id == id).map(|row| row.file.as_str())
    }

    pub fn position_of(&self, id: u32) -> Option<usize> {
        self.rows.iter().position(|row| row.id == id)
    }

    fn addid(&mut self, uri: &str, position: Option<usize>) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let position = position.map_or(self.rows.len(), |position| position.min(self.rows.len()));
        self.rows.insert(position, Row { id, file: uri.to_owned() });
        id
    }

    fn deleteid(&mut self, id: u32) -> bool {
        match self.position_of(id) {
            Some(position) => {
                self.rows.remove(position);
                true
            }
            None => false,
        }
    }

    /// Serialize the queue as `playlistinfo`'s response body.
    fn playlistinfo(&self) -> String {
        let mut out = String::new();
        for row in &self.rows {
            out.push_str(&format!("file: {}\n", row.file));
            out.push_str(&format!("Id: {}\n", row.id));
            out.push_str("Pos: 0\n");
        }
        out
    }
}

/// A running fake MPD server bound to a socket path the `Client` connects to.
pub struct FakeMpdServer {
    pub state: Arc<Mutex<FakeMpdState>>,
    pub socket_path: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for FakeMpdServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock the accept loop: `incoming()` parks until a connection
        // arrives, so the shutdown connection below is what lets the thread
        // notice `stop` and exit. Without this the join hangs a test run.
        if let Ok(stream) = UnixStream::connect(&self.socket_path) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl FakeMpdServer {
    /// Start a server holding `rows`, listening on a private socket.
    pub fn start(rows: Vec<(u32, String)>) -> Self {
        // A short, unique-enough path: `sun_path` is 108 bytes, so the
        // system temp dir plus a pid/counter suffix is the safe choice.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let socket_path = std::env::temp_dir().join(format!(
            "s2u-fakempd-{}-{n}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);

        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("the fake MPD socket must bind");

        let state = Arc::new(Mutex::new(FakeMpdState::new(rows)));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                // The path uses one connection at a time (the `Client` is the
                // only speaker), so serving inline keeps the model simple and
                // the command order exact.
                serve(stream, &thread_state, &thread_stop);
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
            }
        });

        Self { state, socket_path, stop, handle: Some(handle) }
    }

    /// Connect the production `Client` to this server.
    pub fn client(&self) -> Client<'static> {
        Client::init(
            MpdAddress::SocketPath(self.socket_path.to_string_lossy().into_owned()),
            None::<MpdPassword>,
            "fake-mpd",
            None,
            false,
        )
        .expect("the production Client must connect to the fake MPD server")
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, FakeMpdState> {
        // A test that panics mid-assert can leave the guard held; recovering the
        // state keeps a later failure readable instead of turning it into a
        // second, confusing panic (or a hang while the drop path contends).
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A copy of the queue, safe to hold across assertions.
    pub fn snapshot(&self) -> Vec<Row> {
        self.state().rows.clone()
    }

    /// The files in the fake queue, in order.
    pub fn files(&self) -> Vec<String> {
        self.snapshot().into_iter().map(|row| row.file).collect()
    }

    /// The file MPD is playing, if any.
    pub fn playing_file(&self) -> Option<String> {
        let state = self.state();
        state.playing.and_then(|id| state.file_of(id)).map(str::to_owned)
    }

    /// Every command line the fake MPD has received.
    pub fn commands(&self) -> Vec<String> {
        self.state().commands.clone()
    }
}

/// Serve one connection: read command lines, answer each with a valid MPD
/// response, and keep the queue in `state`.
fn serve(
    stream: UnixStream,
    state: &Arc<Mutex<FakeMpdState>>,
    stop: &Arc<AtomicBool>,
) {
    let reader_stream = match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    // A read timeout keeps the loop able to notice `stop` (and a client that
    // vanished), so `Drop`'s join always completes.
    let _ = reader_stream.set_read_timeout(Some(std::time::Duration::from_millis(50)));
    let mut writer = stream;
    let mut reader = BufReader::new(reader_stream);

    if writer.write_all(b"OK MPD 0.24.0\n").is_err() {
        return;
    }
    let _ = writer.flush();

    let mut line = String::new();
    while !stop.load(Ordering::SeqCst) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Nothing to read yet: loop so `stop` is re-checked.
                continue;
            }
            Err(_) => break,
        }
        let command = line.trim_end().to_owned();
        if command.is_empty() {
            continue;
        }
        if std::env::var_os("S2U_FAKE_MPD_TRACE").is_some() {
            eprintln!("[fake-mpd] <- {command}");
        }
        let response = handle_command(&command, state);
        if writer.write_all(response.as_bytes()).is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

/// One command → its wire response.
fn handle_command(command: &str, state: &Arc<Mutex<FakeMpdState>>) -> String {
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    state.commands.push(command.to_owned());

    let mut parts = command.split_whitespace();
    let verb = parts.next().unwrap_or_default();
    match verb {
        "playlistinfo" | "listallinfo" => {
            let body = state.playlistinfo();
            format!("{body}OK\n")
        }
        "playid" => {
            let id = parts.next().and_then(|raw| raw.parse::<u32>().ok());
            match id {
                // MPD refuses an id it does not hold — the `No such song` ACK
                // measured after an in-place swap. Modelling the refusal is
                // what makes a stale-id play fail here the way it fails live.
                Some(id) if state.position_of(id).is_some() => {
                    state.playing = Some(id);
                    "OK\n".to_owned()
                }
                Some(id) => {
                    state.refused_playids.push(id);
                    format!("ACK [50@0] {{playid}} No such song\n")
                }
                None => "ACK [2@0] {playid} Bad song id\n".to_owned(),
            }
        }
        "addid" => {
            // `addid "<uri>" [position]`, with the uri quoted and escaped.
            let rest = command.strip_prefix("addid").unwrap_or_default().trim_start();
            let (uri, tail) = parse_quoted(rest);
            let position =
                tail.split_whitespace().next().and_then(|raw| raw.parse::<usize>().ok());
            let id = state.addid(&uri, position);
            format!("Id: {id}\nOK\n")
        }
        "deleteid" => {
            let id = parts.next().and_then(|raw| raw.parse::<u32>().ok());
            match id {
                Some(id) if state.deleteid(id) => "OK\n".to_owned(),
                Some(_) => "ACK [50@0] {deleteid} No such song\n".to_owned(),
                None => "ACK [2@0] {deleteid} Bad song id\n".to_owned(),
            }
        }
        "status" => "volume: 100\nrepeat: 0\nrandom: 1\nsingle: 0\nconsume: 0\n\
                     playlist: 1\nplaylistlength: 3\nstate: play\nsong: 0\n\
                     songid: 1\nnextsong: 1\nnextsongid: 2\nOK\n"
            .to_owned(),
        "currentsong" => {
            let body = state
                .playing
                .and_then(|id| {
                    state
                        .rows
                        .iter()
                        .find(|row| row.id == id)
                        .map(|row| format!("file: {}\nId: {}\n", row.file, row.id))
                })
                .unwrap_or_default();
            format!("{body}OK\n")
        }
        "idle" => {
            // Block briefly rather than spin: the rig never needs an event.
            std::thread::sleep(std::time::Duration::from_millis(5));
            "OK\n".to_owned()
        }
        "noidle" | "ping" => "OK\n".to_owned(),
        "commands" => {
            // The `Client` probes the supported-command set on connect.
            "command: playid\ncommand: addid\ncommand: deleteid\n\
             command: playlistinfo\ncommand: status\nOK\n"
                .to_owned()
        }
        "close" => "OK\n".to_owned(),
        _ => "OK\n".to_owned(),
    }
}

/// Split a leading `"…"` (MPD's quoted form, `\"` escaped) from the remainder.
fn parse_quoted(input: &str) -> (String, String) {
    let Some(rest) = input.strip_prefix('"') else {
        return (input.to_owned(), String::new());
    };
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\\' => {
                if let Some((_, escaped)) = chars.next() {
                    out.push(escaped);
                }
            }
            '"' => return (out, rest[index + 1..].to_owned()),
            _ => out.push(ch),
        }
    }
    (out, String::new())
}

/// Build a `Song` list from `(id, file)` pairs, for the app-side `ctx.queue`.
pub fn songs(rows: &[(u32, String)]) -> Vec<Song> {
    rows.iter()
        .map(|(id, file)| Song { id: *id, file: file.clone(), ..Default::default() })
        .collect()
}
