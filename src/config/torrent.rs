use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{defaults, utils::tilde_expand_path};

/// Torrent streaming configuration (drag&drop `.torrent` / `magnet:`).
///
/// Streaming runs through the **rqbit** engine (a single static binary,
/// subprocess + localhost HTTP server, exactly like the s2u-yt wrapper):
/// dropping a torrent offers **Play (stream)** in the paste popup; the
/// engine downloads pieces on demand and mpv plays the stream URL, with an
/// explicit bandwidth gate before playback starts (no play when the peer
/// speed is below `min_download_speed_kbps`).
///
/// Full plan: `docs/design/Backend/torrent-streaming.md` (M1 = this
/// engine bootstrap + config; M2+ wire the classification, popup, gate,
/// playback and lifecycle).
#[derive(Debug, Clone, PartialEq)]
pub struct Torrent {
    /// Master switch: `false` still classifies torrents/magnets in the
    /// paste pipeline but hides the `[Torrent]` popup action.
    pub enabled: bool,
    /// Preferred HTTP API port for the rqbit server. When the port is
    /// already in use the engine scans `port+1 ..= port+20` for a free one.
    pub port: u16,
    /// The bandwidth gate threshold: playback starts only once the median
    /// measured download speed reaches this many KB/s.
    pub min_download_speed_kbps: u32,
    /// Minimum sampling window before the gate can pass (avoids a lucky
    /// instant burst).
    pub warmup_secs: u64,
    /// Hard deadline of the gate; when it expires without the speed
    /// threshold, the user gets Retry / Play anyway / Cancel.
    pub max_wait_secs: u64,
    /// How long to wait for any peer before aborting a dead magnet with
    /// "No peers found".
    pub no_peers_timeout_secs: u64,
    /// Where rqbit stores the partial/streaming files. Created on first
    /// use; cleared on app exit unless `keep_after_play` is true.
    pub cache_dir: PathBuf,
    /// Multi-file torrents: pick the single largest playable file silently
    /// (`true`) or always ask (`false`).
    pub auto_pick_file: bool,
    /// Keep the engine + partial files after playback ends (`true`) or kill
    /// it immediately (`false` — default; minimizes P2P seeding).
    pub keep_after_play: bool,
    /// SOCKS5 proxy URL (e.g. `socks5://127.0.0.1:1080`) for ALL outgoing
    /// rqbit connections — the VPN route for torrent traffic. Passed to
    /// rqbit as `--socks-url`; when set, incoming connections are disabled
    /// too (rqbit's own recommendation when proxying: the proxy only
    /// handles outgoing, so listening would leak the real IP). `None` =
    /// direct connections.
    pub socks_proxy: Option<String>,
}

impl Default for Torrent {
    fn default() -> Self {
        Self {
            enabled: true,
            port: defaults::default_torrent_port(),
            min_download_speed_kbps: defaults::default_torrent_min_speed_kbps(),
            warmup_secs: defaults::default_torrent_warmup_secs(),
            max_wait_secs: defaults::default_torrent_max_wait_secs(),
            no_peers_timeout_secs: defaults::default_torrent_no_peers_timeout_secs(),
            cache_dir: defaults::default_torrent_cache_dir(),
            auto_pick_file: true,
            keep_after_play: false,
            socks_proxy: None,
        }
    }
}

/// The `torrent: (…)` section of `config.ron`. All fields optional; the
/// runtime [`Torrent`] fills unset ones with the defaults.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct TorrentFile {
    pub enabled: Option<bool>,
    pub port: Option<u16>,
    pub min_download_speed_kbps: Option<u32>,
    pub warmup_secs: Option<u64>,
    pub max_wait_secs: Option<u64>,
    pub no_peers_timeout_secs: Option<u64>,
    pub cache_dir: Option<PathBuf>,
    pub auto_pick_file: Option<bool>,
    pub keep_after_play: Option<bool>,
    pub socks_proxy: Option<String>,
}

impl From<TorrentFile> for Torrent {
    fn from(value: TorrentFile) -> Self {
        Self {
            enabled: value.enabled.unwrap_or(true),
            port: value.port.unwrap_or_else(defaults::default_torrent_port),
            min_download_speed_kbps: value
                .min_download_speed_kbps
                .unwrap_or_else(defaults::default_torrent_min_speed_kbps),
            warmup_secs: value.warmup_secs.unwrap_or_else(defaults::default_torrent_warmup_secs),
            max_wait_secs: value
                .max_wait_secs
                .unwrap_or_else(defaults::default_torrent_max_wait_secs),
            no_peers_timeout_secs: value
                .no_peers_timeout_secs
                .unwrap_or_else(defaults::default_torrent_no_peers_timeout_secs),
            cache_dir: value
                .cache_dir
                .map(|v| tilde_expand_path(&v))
                .unwrap_or_else(|| tilde_expand_path(&defaults::default_torrent_cache_dir())),
            auto_pick_file: value.auto_pick_file.unwrap_or(true),
            keep_after_play: value.keep_after_play.unwrap_or(false),
            socks_proxy: value.socks_proxy,
        }
    }
}

