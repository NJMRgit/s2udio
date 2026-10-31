//! Wire types for the s2udio daemon socket.
//!
//! One JSON object per line: the request, then the reply, except for a TUI
//! attach connection, which sends `tui_hello` and then holds the stream open
//! (holding it is how the daemon knows a TUI is running; the kernel closes it
//! on a crash, so no heartbeat is needed).

use serde::{Deserialize, Serialize};

/// A request from a client (the TUI, `s2udio daemon ...`, the hotkey bridge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Liveness + state; also what `daemon start` waits for.
    Ping,
    /// Ask the daemon to exit. Answered before the daemon stops.
    Stop,
    /// Register a stream with the buffer proxy and return the port it is
    /// served on (`None` when the proxy could not bind).
    Register { class: String, original: String, upstream: String },
    /// Register + begin filling the buffer for a row that is about to play.
    Preload { class: String, original: String, upstream: String },
    /// The original link a buffer key was registered from.
    OriginalLink { key: String },
    /// Whether a buffered URL needs to be re-resolved (expired, failed, or
    /// pointing at a previous run's port).
    NeedsRefresh { url: String },
    /// Announce a TUI; the connection is kept open and counts as "attached".
    TuiHello { pid: u32 },
    /// QAT actions (`"toggle"`, `"raise"`, `"show"`, `"hide"`). The
    /// daemon's hotkey reaches toggle through D-Bus; MPRIS uses raise.
    Qat { action: String },
    /// Change the QAT chord of the running daemon. The Settings panel saves
    /// the daemon.ron sidecar first and then makes the change live.
    QatHotkey { chord: String },
}

/// A reply to one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Ok,
    Info(DaemonInfo),
    Port { port: Option<u16> },
    Link { link: Option<String> },
    Refresh { needed: bool },
    Error { message: String },
}

/// What `daemon status` and the ensure/start handshake report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    /// Epoch seconds.
    pub started_at: u64,
    /// The stream buffer proxy's port; `None` when it failed to bind.
    pub proxy_port: Option<u16>,
    pub mpd_connected: bool,
    /// Open TUI attach connections. The caretaker stands down while this is
    /// non-zero (the TUI's own look ahead is authoritative then).
    pub attached: usize,
    pub queue_len: usize,
    /// Rows the caretaker has resolved since the daemon started.
    pub warmed: u64,
    pub last_error: Option<String>,
    /// The registered QAT shortcut (`None` when the daemon has no QAT
    /// controller, e.g. a headless session).
    pub hotkey: Option<String>,
}

impl Reply {
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self::Error { message: message.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_through_the_line_protocol() {
        for request in [
            Request::Ping,
            Request::Stop,
            Request::Register {
                class: "audio".to_owned(),
                original: "https://example.invalid/watch?v=x".to_owned(),
                upstream: "https://cdn.invalid/a".to_owned(),
            },
            Request::Preload {
                class: "audio".to_owned(),
                original: "https://example.invalid/watch?v=x".to_owned(),
                upstream: "https://cdn.invalid/a".to_owned(),
            },
            Request::OriginalLink { key: "abc123".to_owned() },
            Request::NeedsRefresh {
                url: "http://127.0.0.1:47123/s2u/audio/abc123".to_owned(),
            },
            Request::TuiHello { pid: 42 },
            Request::Qat { action: "toggle".to_owned() },
            Request::Qat { action: "raise".to_owned() },
            Request::QatHotkey { chord: "Meta+Shift+T".to_owned() },
        ] {
            let line = serde_json::to_string(&request).expect("request serializes");
            assert!(!line.contains('\n'), "a request must stay on one line: {line}");
            let parsed: Request = serde_json::from_str(&line).expect("request parses");
            assert_eq!(parsed, request);
        }
    }

    #[test]
    fn replies_keep_their_tag_and_fields_across_the_wire() {
        let info = DaemonInfo {
            pid: 7,
            started_at: 1,
            proxy_port: Some(47123),
            mpd_connected: true,
            attached: 0,
            queue_len: 12,
            warmed: 3,
            last_error: Some("boom".to_owned()),
            hotkey: Some("Meta+T".to_owned()),
        };
        for reply in [
            Reply::Ok,
            Reply::Info(info.clone()),
            Reply::Port { port: Some(47123) },
            Reply::Port { port: None },
            Reply::Link { link: Some("https://example.invalid/watch?v=x".to_owned()) },
            Reply::Link { link: None },
            Reply::Refresh { needed: true },
            Reply::error("bad request"),
        ] {
            let line = serde_json::to_string(&reply).expect("reply serializes");
            let parsed: Reply = serde_json::from_str(&line).expect("reply parses");
            assert_eq!(parsed, reply);
        }
        // The internally tagged shape is part of the contract: a new client
        // must recognise an old daemon by `kind`, not by a bare object.
        let line = serde_json::to_string(&Reply::Info(info)).expect("serializes");
        assert!(line.contains("\"kind\":\"info\""), "got {line}");
        assert!(line.contains("\"proxy_port\":47123"), "got {line}");
    }
}
