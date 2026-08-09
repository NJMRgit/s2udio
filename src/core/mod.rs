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
pub mod work;
