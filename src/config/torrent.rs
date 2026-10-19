use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{defaults, utils::tilde_expand_path};

/// Torrent streaming configuration (drag&drop `.torrent` / `magnet:`).
///
/// Streaming runs through the **rqbit** engine (a single static binary,
/// subprocess + localhost HTTP server, exactly like the s2u-yt wrapper): a
/// pasted torrent opens the picker, the engine fetches pieces on demand and
/// mpv plays the stream URL.
///
/// Design: `docs/design/Backend/torrent-streaming.md`.
///
/// **Removed 2026-10-19** — five keys that no code ever read: the
/// never-built bandwidth gate (`min_download_speed_kbps`, `warmup_secs`,
/// `max_wait_secs`, `no_peers_timeout_secs` — the metainfo wait is
/// open-ended by design, and rqbit's own stats are the only speed signal),
/// plus `auto_pick_file` (the picker lists every media file, so picking for
/// the user is gone) and `keep_after_play` (no cache-cleanup policy was ever
/// implemented — the cache dir simply persists). Older `config.ron` files
/// that still carry them keep parsing: unknown keys are ignored.
#[derive(Debug, Clone, PartialEq)]
pub struct Torrent {
    /// Master switch: `false` still classifies torrents/magnets in the
    /// paste pipeline but hides the `[Torrent]` popup action.
    pub enabled: bool,
    /// Preferred HTTP API port for the rqbit server. When the port is
    /// already in use the engine scans `port+1 ..= port+20` for a free one.
    pub port: u16,
    /// Where rqbit stores the partial/streaming files. Created on first
    /// use; it persists (there is no cache-cleanup policy).
    pub cache_dir: PathBuf,
    /// **Streaming a torrent also downloads it** (`true` — default): the
    /// picker's `Stream` runs as a committed download, so the file lands in
    /// `~/Downloads/s2udio-downloads` and the torrent shows up in the
    /// Downloads modal like any other download. `false` = a plain stream that
    /// dies with the session and leaves nothing behind.
    ///
    /// Playback always runs through the **downloader daemon's** engine when
    /// this is on: the TUI engine and the daemon engine cannot share one
    /// cache dir (round 54, §2.2), so the stream is served by the engine that
    /// is also doing the downloading.
    pub stream_downloads: bool,
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
            cache_dir: defaults::default_torrent_cache_dir(),
            stream_downloads: true,
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
    pub cache_dir: Option<PathBuf>,
    pub stream_downloads: Option<bool>,
    pub socks_proxy: Option<String>,
}

impl From<TorrentFile> for Torrent {
    fn from(value: TorrentFile) -> Self {
        Self {
            enabled: value.enabled.unwrap_or(true),
            port: value.port.unwrap_or_else(defaults::default_torrent_port),
            cache_dir: value
                .cache_dir
                .map(|v| tilde_expand_path(&v))
                .unwrap_or_else(|| tilde_expand_path(&defaults::default_torrent_cache_dir())),
            stream_downloads: value.stream_downloads.unwrap_or(true),
            socks_proxy: value.socks_proxy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_downloads_is_on_by_default() {
        // The user's default: a streamed torrent always shows up in
        // Downloads, so a stream keeps the file unless deliberately turned
        // off.
        assert!(Torrent::default().stream_downloads);
    }

    /// The shipped example config is a real `config.ron`: parsing it is the
    /// same path a user's file goes through (RON only accepts the implicit
    /// `Some` form inside a full document, so fragments are not used here).
    fn parse_example(mutate: impl Fn(&str) -> String) -> TorrentFile {
        let example = include_str!("../../assets/example_config.ron");
        let text = mutate(example);
        ron::from_str::<crate::config::ConfigFile>(&text)
            .expect("the example config parses")
            .torrent
    }

    #[test]
    fn a_config_without_the_key_keeps_the_default() {
        // Every older `config.ron` predates the field: its line is dropped,
        // and the runtime value must still be the default (on).
        let file = parse_example(|text| {
            text.lines()
                .filter(|line| !line.contains("stream_downloads"))
                .collect::<Vec<_>>()
                .join("\n")
        });
        assert_eq!(file.stream_downloads, None, "the key is genuinely absent");
        assert!(
            Torrent::from(file).stream_downloads,
            "an absent key means the default (on)"
        );
    }

    #[test]
    fn a_config_still_carrying_the_removed_keys_keeps_parsing() {
        // Every config.ron written before 2026-10-19 carries the bandwidth
        // gate, auto-pick and keep-after-play keys. Removing them from the
        // schema must not make s2udio reject the file (serde ignores unknown
        // fields, and this pins that: a `deny_unknown_fields` added later
        // would break every existing install).
        let file = parse_example(|text| {
            text.replace(
                "cache_dir: \"~/.cache/s2udio/torrents\",",
                "cache_dir: \"~/.cache/s2udio/torrents\",\n\
                 min_download_speed_kbps: 500,\n\
                 warmup_secs: 5,\n\
                 max_wait_secs: 15,\n\
                 no_peers_timeout_secs: 30,\n\
                 auto_pick_file: true,\n\
                 keep_after_play: false,",
            )
        });
        let torrent = Torrent::from(file);
        assert_eq!(torrent.port, 3030, "the live keys still apply");
        assert!(torrent.stream_downloads, "and the new default survives");
    }

    #[test]
    fn a_pre_picker_config_file_still_loads() {
        // The exact shape a config.ron had before 2026-10-19 (taken from the
        // host's live file): every key this change removed, and neither of
        // the two keys added since. It must parse and fall back to the
        // defaults for the new keys — a parse failure would silently discard
        // the whole file, since s2udio falls back to stock defaults.
        let file = parse_example(|text| {
            let start = text.find("    torrent: (").expect("the example has a torrent block");
            let end = text.find("    keybinds: (").expect("and a keybinds block after it");
            let old_block = "    torrent: (\n\
                 \x20       enabled: true,\n\
                 \x20       port: 3030,\n\
                 \x20       min_download_speed_kbps: 500,\n\
                 \x20       warmup_secs: 5,\n\
                 \x20       max_wait_secs: 15,\n\
                 \x20       no_peers_timeout_secs: 30,\n\
                 \x20       cache_dir: \"~/.cache/s2udio/torrents\",\n\
                 \x20       auto_pick_file: true,\n\
                 \x20       keep_after_play: false,\n\
                 \x20   ),\n";
            format!("{}{old_block}{}", &text[..start], &text[end..])
        });
        let torrent = Torrent::from(file);
        assert_eq!(torrent.port, 3030);
        assert!(torrent.enabled);
        assert!(torrent.stream_downloads, "the new key defaults to on");
        assert_eq!(torrent.socks_proxy, None, "and the other new key to none");
    }

    #[test]
    fn the_key_can_turn_it_off() {
        let file = parse_example(|text| text.replace("stream_downloads: true", "stream_downloads: false"));
        assert_eq!(file.stream_downloads, Some(false));
        assert!(!Torrent::from(file).stream_downloads, "the user opted out");
    }

    #[test]
    fn the_shipped_example_config_carries_the_key() {
        // `assets/example_config.ron` documents every option and is compiled
        // in — a new key must appear there too.
        let example = include_str!("../../assets/example_config.ron");
        assert!(
            example.contains("stream_downloads"),
            "the example config must document the new torrent option"
        );
    }
}
