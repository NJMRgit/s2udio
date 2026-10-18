use super::appearance::{AppearanceTarget, StagedColor};
use crate::{
    config::{Config, UiSettings, cava::CavaOverridesFile, keys::KeySequence},
    mpd::commands::replay_gain::ReplayGain,
    shared::keys::Actions,
    ui::modals::remap_keys,
};

/// A PipeWire capture source offered by the settings panel.
#[derive(Debug, Clone)]
pub(super) struct PwNode {
    /// The cava `source` value (the pw `node.name`).
    pub(super) name: String,
    /// Human-readable description shown in the row.
    pub(super) description: String,
    /// `node.virtual == "true"` (Easy Effects etc.), hidden unless requested.
    pub(super) is_virtual: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Section {
    General,
    Keybinds,
    Mpv,
    Mpd,
    Jellyfin,
    Torrent,
    /// Diagnostics that exist for when something is already suspected:
    /// currently the in-app memory watchdog. Kept separate from `General` so
    /// the everyday toggles are not diluted by a debugging switch.
    Advanced,
}

/// Which settings pane owns keyboard navigation. Mirrors the tab-pane
/// scheme: w/s/↑/↓ move the focused pane, d/→/Enter activate, a/← steps
/// back to the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsFocus {
    Sidebar,
    Content,
}

impl Section {
    pub(super) fn all() -> [Section; 7] {
        [
            Section::General,
            Section::Keybinds,
            Section::Mpv,
            Section::Mpd,
            Section::Jellyfin,
            Section::Torrent,
            Section::Advanced,
        ]
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Section::General => "general",
            Section::Keybinds => "keybinds",
            Section::Mpv => "mpv",
            Section::Mpd => "mpd",
            Section::Jellyfin => "jellyfin",
            Section::Torrent => "torrent",
            Section::Advanced => "advanced",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GeneralRow {
    FeaturesHeader,
    AlbumArt,
    Lyrics,
    Cava,
    // Round 62 (Q4): the "show radio tab" toggle is gone — Radio is not a
    // tab anymore (it lives on the Queue page), so there is nothing to
    // show/hide. The radio favourites reload row stays (RadioReload).
    RadioReload,
    Jellyfin,
    VideoPlayback,
    AutoChapters,
    Mpdis2Notifications,
    CavaHeader,
    AutoSens,
    Sensitivity,
    Fps,
    FreqMin,
    FreqMax,
    Channels,
    Device,
    VirtualDevices,
    NoiseReduction,
    Monstercat,
    Waves,
    AppearanceHeader,
}

/// Settings > Advanced rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdvancedRow {
    DiagnosticsHeader,
    /// Log this process's RSS on a timer, so a long session leaves a memory
    /// trail in the log. Off by default; see `shared::mem_watch`.
    MemoryWatchdog,
    /// How often the watchdog samples. Only meaningful while the toggle above
    /// is on.
    MemoryWatchdogInterval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MpvRow {
    Header,
    AudioLang,
    Subtitles,
    /// The "svp support" toggle: pass `--input-ipc-server=/tmp/mpvsocket`
    /// so SVP4's manager can drive frame interpolation (and s2udio tracks
    /// playback over the same socket).
    Svp,
    /// Opens the in-TUI nano-style editor for `~/.config/mpv/mpv.conf`
    /// (Round 48).
    EditConf,
}

/// Which mpv preference the language picker is choosing for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MpvLanguageTarget {
    Audio,
    Subtitles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MpdRow {
    LibraryHeader,
    LibraryPath,
    /// Settings > MPD: show the `.m3u` / `.pls` / `.xspf` playlist files
    /// found in the music library in the Playlists tab (read-only).
    LibraryPlaylists,
    Update,
    Rescan,
    PlaybackHeader,
    Crossfade,
    Repeat,
    Random,
    Single,
    Consume,
    /// Replay gain mode cycle (off / track / album) — acts immediately on
    /// the live server, persisted to state.ron on Save (round 53).
    ReplayGain,
    DevicesHeader,
    Outputs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JellyfinRow {
    Header,
    ServerUrl,
    Username,
    Password,
    SignIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TorrentRow {
    Header,
    /// Launch the rqbit web UI in the browser (starts the standalone
    /// engine first when none is running).
    WebUi,
    /// Stop the standalone web-UI engine (next "web ui" start spawns a
    /// fresh one — needed to pick up a changed SOCKS proxy).
    StopEngine,
    /// SOCKS5 proxy URL for all rqbit traffic (the VPN route); "" = none.
    SocksProxy,
}

/// The cava values the panel manages, staged while the panel is open and
/// applied (runtime config + `cava.ron` sidecar) only when it is closed
/// with Save. Values are resolved (defaults applied) so the rows always
/// show what would be saved.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StagedCava {
    pub(super) framerate: u16,
    pub(super) autosens: bool,
    pub(super) sensitivity: u16,
    pub(super) lower_cutoff_freq: u16,
    pub(super) higher_cutoff_freq: u32,
    pub(super) channels: u32,
    pub(super) source: String,
    pub(super) noise_reduction: u8,
    pub(super) monstercat: bool,
    pub(super) waves: bool,
    /// Round 29: the configured PipeWire node name for the cava stream.
    /// Not editable from the panel (set it in `cava.ron`), but carried
    /// through so a panel Save never drops it.
    pub(super) node_name: Option<String>,
}

impl StagedCava {
    pub(super) fn from_config(config: &Config) -> Self {
        Self {
            framerate: config.cava.framerate,
            autosens: config.cava.autosens,
            sensitivity: config.cava.sensitivity,
            lower_cutoff_freq: config.cava.lower_cutoff_freq.unwrap_or(DEFAULT_FREQ_MIN),
            higher_cutoff_freq: config
                .cava
                .higher_cutoff_freq
                .unwrap_or(DEFAULT_FREQ_MAX),
            channels: config.cava.input.channels.unwrap_or(2),
            source: config.cava.input.source.clone(),
            noise_reduction: config.cava.smoothing.noise_reduction,
            monstercat: config.cava.smoothing.monstercat,
            waves: config.cava.smoothing.waves,
            node_name: config.cava.input.node_name.clone(),
        }
    }

    /// The sidecar values that would be persisted on Save.
    pub(super) fn to_overrides(&self) -> CavaOverridesFile {
        CavaOverridesFile {
            framerate: Some(self.framerate),
            autosens: Some(self.autosens),
            sensitivity: Some(self.sensitivity),
            lower_cutoff_freq: Some(self.lower_cutoff_freq),
            higher_cutoff_freq: Some(self.higher_cutoff_freq),
            channels: Some(self.channels),
            source: Some(self.source.clone()),
            noise_reduction: Some(self.noise_reduction),
            monstercat: Some(self.monstercat),
            waves: Some(self.waves),
            node_name: self.node_name.clone(),
        }
    }
}

/// Values staged before a [-] [+] adjustment session; restored when the
/// session is cancelled with Esc.
#[derive(Debug, Clone)]
pub(super) struct AdjustSnapshot {
    pub(super) cava: StagedCava,
    /// The live MPD crossfade seconds at session start (the crossfade row
    /// adjusts MPD live, so cancel restores it).
    pub(super) crossfade: u32,
}

/// A key remap applied while the panel was open. The runtime keybinds are
/// updated immediately (the table shows the new key); the change is written
/// to `keybinds.ron` only when the panel is closed with Save.
#[derive(Debug, Clone)]
pub(crate) struct PendingRemap {
    pub section: remap_keys::Section,
    pub action: Actions,
    pub new_key: KeySequence,
    pub old_keys: Vec<KeySequence>,
}

/// Everything the panel staged, handed to the UI to apply when it is closed
/// with Save.
#[derive(Debug, Clone)]
pub(crate) struct StagedSettings {
    pub ui: UiSettings,
    pub cava: CavaOverridesFile,
    pub appearance: [StagedColor; 7],
    pub remaps: Vec<PendingRemap>,
    /// Video playback mode chosen in the settings panel; applied + persisted
    /// to state.ron on Save.
    pub video_playback: crate::config::video::VideoPlaybackMode,
    /// mpv audio language + subtitle preference + SVP support chosen in
    /// the settings panel; applied + persisted to state.ron on Save.
    pub mpv_audio_lang: crate::config::mpv::MpvAudioLang,
    pub mpv_subtitles: crate::config::mpv::MpvSubtitleMode,
    pub mpv_svp: bool,
    /// Jellyfin credentials from a successful sign-in; persisted to the
    /// jellyfin.ron sidecar on Save.
    pub jellyfin: Option<crate::config::jellyfin::JellyfinCredentialsFile>,
    /// rqbit SOCKS5 proxy URL staged in the settings panel; applied to the
    /// engine config + persisted to state.ron on Save ("" = no proxy).
    pub torrent_socks_proxy: String,
    /// The MPD replay gain mode cycled in the settings panel; `None` = the
    /// user never touched the row (legacy state.ron stays without the
    /// field and the server mode is left untouched).
    pub replay_gain: Option<ReplayGain>,
}

#[derive(Debug, Clone)]
pub(super) enum ContentRow {
    General(GeneralRow),
    KeyTableHeader,
    KeyHeader(remap_keys::Section),
    KeyItem(remap_keys::RemapRow),
    Appearance(AppearanceTarget),
    Mpd(MpdRow),
    Mpv(MpvRow),
    Jellyfin(JellyfinRow),
    Torrent(TorrentRow),
    Advanced(AdvancedRow),
}

impl ContentRow {
    pub(super) fn is_header(&self) -> bool {
        matches!(
            self, ContentRow::General(GeneralRow::FeaturesHeader | GeneralRow::CavaHeader
            | GeneralRow::AppearanceHeader,) | ContentRow::KeyTableHeader |
            ContentRow::KeyHeader(_) | ContentRow::Mpd(MpdRow::LibraryHeader |
            MpdRow::PlaybackHeader | MpdRow::DevicesHeader) |
            ContentRow::Mpv(MpvRow::Header) | ContentRow::Jellyfin(JellyfinRow::Header) |
            ContentRow::Torrent(TorrentRow::Header) |
            ContentRow::Advanced(AdvancedRow::DiagnosticsHeader)
        )
    }
}

/// Mouse click targets within a row (button columns), resolved per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Click {
    Toggle,
    Inc,
    Dec,
    Prev,
    Next,
    Edit,
    Activate,
}

/// A button placed inside a row's right-aligned control part: `offset` is
/// the column relative to the control's start. The click rect is placed by
/// `render_row` once the control's x is known (deferred so the row-wide
/// click target — pushed first — can never shadow the button).
pub(super) struct DeferredButton {
    pub(super) offset: usize,
    pub(super) label: &'static str,
    pub(super) click: Click,
}

pub(super) const FREQ_MIN_LIMIT: i64 = 20;
pub(super) const FREQ_MAX_LIMIT: i64 = 22_000;
/// Query id for the live `replay_gain_status` snapshot fetched when the
/// panel opens (round 53; MPD `status` does not carry the mode).
pub(super) const REPLAY_GAIN_SNAPSHOT: &str = "open_settings_replay_gain";
pub(super) const DEFAULT_FREQ_MIN: u16 = 50;
pub(super) const DEFAULT_FREQ_MAX: u32 = 15_000;
pub(super) const FPS_MIN: u16 = 15;
pub(super) const FPS_MAX: u16 = 120;
