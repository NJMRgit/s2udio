pub mod accent;
pub mod client;
pub mod command;
pub mod config_watcher;
/// The always-on daemon: owns the stream buffer proxy and, while no TUI is
/// attached, the queue's web-stream caretaker. See
/// `docs/design/Backend/daemon.md`.
pub mod daemon;
pub mod event_loop;
pub mod input;
/// One running instance per user: the lock file that refuses a second TUI
/// (two of them resolve the same queue rows and fight over playback).
pub mod instance_lock;
pub mod lyrics_watcher;
pub mod mpv;
/// `s2udio rq start|stop|open` — shell control of the standalone rqbit
/// engine, sharing its registration with the Settings panel.
pub mod rqctl;
/// `s2udio dl start|status|stop` — the durable torrent downloader daemon
/// (round 54): committed torrent downloads run in a detached process that
/// survives the TUI; see `dlctl`'s module docs for the full design.
pub mod dlctl;
pub mod scheduler;
pub mod socket;
/// Torrent streaming engine (rqbit) — M1 bootstrap; the entry points
/// (`start_engine`, `add_torrent`, …) are wired into the app in M2+.
#[allow(dead_code)]
pub mod torrent;
/// Auth-injecting loopback proxy for the rqbit web UI (round 42 fix).
pub mod torrent_proxy;
/// Localhost caching proxy for web audio/video streams (round 107): a
/// stable local URL handed to MPD/mpv, with a 1 GiB audio / 2 GiB video
/// on-disk LRU so an already-played item replays instantly.
pub mod stream_cache;
/// Local resume positions for every video played in mpv (Jellyfin keeps
/// its own server-side copy; this is the store for local files, torrents
/// and resolved YouTube links, and the fallback when the server has none).
pub mod video_resume;
pub mod work;
