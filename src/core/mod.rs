pub mod blur;
pub mod client;
pub mod command;
pub mod config_watcher;
pub mod event_loop;
pub mod input;
pub mod lyrics_watcher;
pub mod mpv;
pub mod scheduler;
pub mod socket;
/// Torrent streaming engine (rqbit) — M1 bootstrap; the entry points
/// (`start_engine`, `add_torrent`, …) are wired into the app in M2+.
#[allow(dead_code)]
pub mod torrent;
/// Auth-injecting loopback proxy for the rqbit web UI (round 42 fix).
pub mod torrent_proxy;
/// `s2udio rq start|stop|open` — shell control of the standalone rqbit
/// engine, sharing its registration with the Settings panel.
pub mod rqctl;
pub mod work;
