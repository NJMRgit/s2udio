use std::{collections::HashMap, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use address::MpdPassword;
use album_art::{AlbumArtConfig, AlbumArtConfigFile, ImageMethodFile};
use anyhow::Result;
use artists::{Artists, ArtistsFile};
use cava::{Cava, CavaFile};
use jellyfin::{Jellyfin, JellyfinFile};
use radio::{Radio, RadioFile};
use video::{Video, VideoFile};
use crate::config::mpv::{Mpv, MpvFile};
use clap::Parser;
use cli::{Args, OnOff, OnOffOneshot};
use itertools::Itertools;
use search::SearchFile;
use serde::{Deserialize, Serialize};
use sort_mode::{SortMode, SortModeFile, SortOptions};
use tabs::{PaneType, PaneTypeDiscriminants, Tabs, TabsFile, TreeBrowserArgs, validate_tabs};
use theme::properties::{SongProperty, SongPropertyFile};
use torrent::{Torrent, TorrentFile};
use utils::tilde_expand;

pub mod address;
pub mod album_art;
pub mod artists;
pub mod cava;
pub mod jellyfin;
pub mod radio;
pub mod mpv;
pub mod video;
pub mod cli;
pub mod cli_config;
mod defaults;
pub mod keys;
mod search;
pub mod sort_mode;
pub mod state;
pub mod tabs;
pub mod theme;
pub mod torrent;

pub use address::MpdAddress;
pub use search::{FilterKindFile, Search};

/// Runtime UI visibility settings, toggled from the Settings panel. These
/// are runtime-only in the sense that they are not part of the config file
/// schema; the app re-applies them on config reloads so they survive within
/// a session (restarting resets them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSettings {
    pub show_album_art: bool,
    pub show_lyrics: bool,
    pub show_cava: bool,
    pub show_radio_tab: bool,
    pub show_jellyfin_tab: bool,
    /// When a video starts (or is added while one plays), the Queue tab
    /// auto-switches to its Chapters list when it has chapter markers.
    /// Disabling it keeps the Audio/Video auto-switch but never lands on
    /// the Chapters list.
    pub auto_show_chapters: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            show_album_art: true,
            show_lyrics: true,
            show_cava: true,
            show_radio_tab: true,
            show_jellyfin_tab: true,
            auto_show_chapters: true,
        }
    }
}

use self::{
    keys::{KeyConfig, KeyConfigFile},
    theme::{ConfigColor, UiConfig},
};
use crate::{
    config::{
        tabs::{SizedPaneOrSplit, Tab, TabName},
        utils::tilde_expand_path,
    },
    shared::{duration_format::DurationFormat, lrc::LrcOffset, terminal::TERMINAL},
    tmux,
};

use ratatui::{prelude::IntoCrossterm, style::{Color, Style}};

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct Config {
    pub address: MpdAddress,
    pub password: Option<MpdPassword>,
    pub cache_dir: Option<PathBuf>,
    pub lyrics_dir: Option<String>,
    pub lyrics_offset: LrcOffset,
    pub enable_lyrics_index: bool,
    pub enable_lyrics_hot_reload: bool,
    pub volume_step: u8,
    pub max_fps: u32,
    pub scrolloff: usize,
    pub wrap_navigation: bool,
    pub keybinds: KeyConfig,
    pub normal_timeout_ms: u64,
    pub insert_timeout_ms: u64,
    pub enable_mouse: bool,
    pub scroll_amount: usize,
    pub enable_config_hot_reload: bool,
    pub status_update_interval_ms: Option<u64>,
    pub select_current_song_on_change: bool,
    pub center_current_song_on_change: bool,
    pub reflect_changes_to_playlist: bool,
    pub rewind_to_start_sec: Option<u64>,
    pub keep_state_on_song_change: bool,
    pub mpd_read_timeout: Duration,
    pub mpd_write_timeout: Duration,
    pub mpd_idle_read_timeout_ms: Option<Duration>,
    pub theme: UiConfig,
    pub theme_name: Option<String>,
    pub album_art: AlbumArtConfig,
    pub on_song_change: Option<Arc<Vec<String>>>,
    pub exec_on_song_change_at_start: bool,
    pub on_resize: Option<Arc<Vec<String>>>,
    pub search: Search,
    pub artists: Artists,
    pub tabs: Tabs,
    pub original_tabs_definition: TabsFile,
    pub active_panes: Vec<PaneType>,
    pub browser_song_sort: Arc<SortOptions>,
    pub show_playlists_in_browser: ShowPlaylistsMode,
    pub directories_sort: Arc<SortOptions>,
    pub cava: Cava,
    pub radio: Radio,
    pub jellyfin: Jellyfin,
    pub video: Video,
    pub mpv: Mpv,
    /// Consumed by the M2+ torrent classification/popup; unread until then.
    #[allow(dead_code)]
    pub torrent: Torrent,
    pub auto_open_downloads: bool,
    pub duration_format: DurationFormat,
    /// Runtime show/hide toggles from the Settings panel.
    pub ui: UiSettings,
}

impl Default for Config {
    fn default() -> Self {
        ConfigFile::default()
            .into_config(UiConfig::default(), None, None, true)
            .expect("Default config should be valid")
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum ShowPlaylistsMode {
    All,
    None,
    #[default]
    NonRoot,
}

#[allow(clippy::struct_excessive_bools)]
/// Round 19 heritage: the s2udio-only config overlay. Since round 23 the
/// full config lives at `~/.config/s2udio/config.ron` and this overlay
/// type is only consumed by the one-time migration (legacy base +
/// overlay -> merged full config). Holds exactly the s2udio feature
/// sections — everything upstream rmpc's `ConfigFile` does not define
/// (`radio`, `jellyfin`, `video`, `mpv`, `torrent`). Every section is
/// optional; absent sections fall back to the base file and then to
/// defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct S2udioConfigFile {
    pub radio: Option<RadioFile>,
    pub jellyfin: Option<JellyfinFile>,
    pub video: Option<VideoFile>,
    pub mpv: Option<MpvFile>,
    pub torrent: Option<TorrentFile>,
}

impl S2udioConfigFile {
    /// Overlay the s2udio sections onto a base rmpc `ConfigFile`: each
    /// present section replaces the base file's (which may carry the
    /// section from a pre-round-19 mixed config, or the default).
    pub fn merge_into(self, base: &mut ConfigFile) {
        if let Some(radio) = self.radio {
            base.radio = radio;
        }
        if let Some(jellyfin) = self.jellyfin {
            base.jellyfin = jellyfin;
        }
        if let Some(video) = self.video {
            base.video = video;
        }
        if let Some(mpv) = self.mpv {
            base.mpv = mpv;
        }
        if let Some(torrent) = self.torrent {
            base.torrent = torrent;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ConfigFile {
    pub address: String,
    password: Option<String>,
    cache_dir: Option<PathBuf>,
    lyrics_dir: Option<String>,
    lyrics_offset_ms: i64,
    enable_lyrics_index: bool,
    enable_lyrics_hot_reload: bool,
    pub theme: Option<String>,
    volume_step: u8,
    pub max_fps: u32,
    scrolloff: usize,
    wrap_navigation: bool,
    status_update_interval_ms: Option<u64>,
    select_current_song_on_change: bool,
    center_current_song_on_change: bool,
    reflect_changes_to_playlist: bool,
    rewind_to_start_sec: Option<u64>,
    keep_state_on_song_change: bool,
    mpd_read_timeout_ms: u64,
    mpd_write_timeout_ms: u64,
    mpd_idle_read_timeout_ms: Option<u64>,
    enable_mouse: bool,
    scroll_amount: usize,
    pub enable_config_hot_reload: bool,
    keybinds: KeyConfigFile,
    pub normal_timeout_ms: u64,
    pub insert_timeout_ms: u64,
    // Deprecated
    image_method: Option<ImageMethodFile>,
    album_art_max_size_px: Size,
    pub album_art: AlbumArtConfigFile,
    on_song_change: Option<Vec<String>>,
    exec_on_song_change_at_start: bool,
    on_resize: Option<Vec<String>>,
    search: SearchFile,
    artists: ArtistsFile,
    tabs: TabsFile,
    pub ignore_leading_the: bool,
    pub browser_song_sort: Vec<SongPropertyFile>,
    pub show_playlists_in_browser: ShowPlaylistsMode,
    pub directories_sort: SortModeFile,
    pub cava: CavaFile,
    pub radio: RadioFile,
    pub jellyfin: JellyfinFile,
    pub video: VideoFile,
    pub mpv: MpvFile,
    pub torrent: TorrentFile,
    pub auto_open_downloads: bool,
    pub duration_format: String,
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq)]
pub struct Size {
    pub width: u16,
    pub height: u16,
}

impl Default for Size {
    fn default() -> Self {
        Self { width: 1200, height: 1200 }
    }
}

impl From<(u16, u16)> for Size {
    fn from(value: (u16, u16)) -> Self {
        Self { width: value.0, height: value.1 }
    }
}

impl ConfigFile {
    fn stock_default() -> Self {
        Self {
            address: String::from("127.0.0.1:6600"),
            keybinds: KeyConfigFile::default(),
            normal_timeout_ms: 1000,
            insert_timeout_ms: 1000,
            volume_step: 5,
            scrolloff: 0,
            status_update_interval_ms: Some(1000),
            mpd_write_timeout_ms: 5_000,
            mpd_read_timeout_ms: 10_000,
            mpd_idle_read_timeout_ms: None,
            max_fps: 30,
            theme: None,
            cache_dir: None,
            lyrics_dir: None,
            lyrics_offset_ms: 0,
            enable_lyrics_index: true,
            enable_lyrics_hot_reload: false,
            image_method: None,
            select_current_song_on_change: false,
            center_current_song_on_change: false,
            album_art_max_size_px: Size::default(),
            album_art: AlbumArtConfigFile {
                disabled_protocols: vec!["http://".to_string(), "https://".to_string()],
                ..Default::default()
            },
            on_song_change: None,
            exec_on_song_change_at_start: false,
            on_resize: None,
            search: SearchFile::default(),
            tabs: TabsFile::default(),
            enable_mouse: true,
            scroll_amount: 1,
            enable_config_hot_reload: true,
            wrap_navigation: false,
            password: None,
            artists: ArtistsFile::default(),
            ignore_leading_the: false,
            browser_song_sort: defaults::default_song_sort(),
            directories_sort: SortModeFile::SortFormat { group_by_type: true, reverse: false },
            rewind_to_start_sec: None,
            keep_state_on_song_change: true,
            reflect_changes_to_playlist: false,
            cava: CavaFile::default(),
            radio: RadioFile::default(),
            jellyfin: JellyfinFile::default(),
            video: VideoFile::default(),
            mpv: MpvFile::default(),
            torrent: TorrentFile::default(),
            show_playlists_in_browser: ShowPlaylistsMode::default(),
            auto_open_downloads: true,
            duration_format: "%m:%S".to_string(),
        }
    }
}

impl Default for ConfigFile {
    fn default() -> Self {
        thread_local! {
            static PARSING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        PARSING.with(|parsing| {
            if parsing.get() {
                // Re-entrant call from serde's container-level default while we
                // are deserializing our own embedded default config.
                return Self::stock_default();
            }
            parsing.set(true);
            let mut result =
                ron::from_str::<ConfigFile>(include_str!("../../assets/example_config.ron"))
                    .expect("Failed to parse default config");
            // The example config's keybinds section omits the debug-only logs
            // map; restore it so the debug build's default keybinds match.
            #[cfg(debug_assertions)]
            {
                result.keybinds.logs = KeyConfigFile::default().logs;
            }
            parsing.set(false);
            result
        })
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        validate_tabs(&self.theme.layout, &self.tabs)
    }

    /// Whether a pane should be skipped entirely because the Settings panel
    /// has its show/hide toggle off.
    pub fn is_pane_hidden(&self, pane: &PaneType) -> bool {
        match pane {
            PaneType::AlbumArt => !self.ui.show_album_art,
            PaneType::Lyrics => !self.ui.show_lyrics,
            PaneType::Cava => !self.ui.show_cava,
            _ => false,
        }
    }

    /// The tree-browser args of the first `Directories` / `Playlists` /
    /// `Jellyfin` / `Radio` pane in the tab + theme layouts. The pane
    /// container keeps one pane instance per type, so the first
    /// occurrence's args drive that singleton; a config without the pane
    /// falls back to the defaults (50 / 120 / Some(15) — today's
    /// constants).
    pub fn tree_browser_args(&self, pane: PaneTypeDiscriminants) -> TreeBrowserArgs {
        self.tabs
            .tabs
            .values()
            .flat_map(|tab| tab.panes.panes_iter())
            .chain(self.theme.layout.panes_iter())
            .filter_map(|p| match &p.pane {
                PaneType::Directories { tree }
                | PaneType::Playlists { tree }
                | PaneType::Jellyfin { tree }
                | PaneType::Radio { tree } => {
                    Some((PaneTypeDiscriminants::from(&p.pane), tree.clone()))
                }
                _ => None,
            })
            .find(|(disc, _)| *disc == pane)
            .map(|(_, args)| args)
            .unwrap_or_default()
    }

    /// Whether a tab should be hidden from the tab bar and tab cycling. The
    /// Radio/Jellyfin tabs can be disabled from the Settings panel; they stay
    /// in the config so they can be re-enabled without losing their
    /// definitions.
    ///
    /// Round 28: the Search tab folded into the MPD tab (a Library/Search
    /// toggle inside it), so any leftover config entry named "Search" is
    /// hidden too — the tab bar never shows it again.
    pub fn is_tab_hidden(&self, tab: &TabName) -> bool {
        (!self.ui.show_radio_tab && tab.as_str().eq_ignore_ascii_case("Radio"))
            || (!self.ui.show_jellyfin_tab && tab.as_str().eq_ignore_ascii_case("Jellyfin"))
            || tab.as_str().eq_ignore_ascii_case("Search")
    }

    /// Merge key remaps persisted by the Settings panel (`keybinds.ron`
    /// sidecar) on top of the configured keybinds. Called at startup and on
    /// config reloads.
    pub fn apply_keybinds_override(&mut self) {
        if let Some(overrides) = self::keys::KeybindsOverrides::load() {
            overrides.apply_to(&mut self.keybinds);
        }
    }

    /// Merge cava settings persisted by the Settings panel (`cava.ron`
    /// sidecar) on top of the configured cava settings.
    pub fn apply_cava_override(&mut self) {
        if let Some(overrides) = self::cava::CavaOverridesFile::load() {
            overrides.apply_to(&mut self.cava);
        }
    }

    pub fn calc_active_panes(
        tabs: &HashMap<TabName, Tab>,
        layout: &SizedPaneOrSplit,
    ) -> Vec<PaneType> {
        tabs.iter()
            .flat_map(|(_, tab)| tab.panes.panes_iter().map(|pane| pane.pane.clone()))
            .chain(layout.panes_iter().map(|pane| pane.pane.clone()))
            .unique()
            .collect_vec()
    }

    pub fn default_cli(args: &mut Args) -> Config {
        ConfigFile::default()
            .into_config(
                UiConfig::default(),
                std::mem::take(&mut args.address),
                std::mem::take(&mut args.password),
                false,
            )
            .expect("Default config should always convert")
    }

    pub fn default_with_album_art_check() -> Result<Config> {
        ConfigFile::default().into_config(UiConfig::default(), None, None, false)
    }
}

impl ConfigFile {
    pub fn into_config(
        self,
        theme: UiConfig,
        address_cli: Option<String>,
        password_cli: Option<String>,
        skip_album_art_check: bool,
    ) -> Result<Config> {
        let original_tabs_definition = self.tabs.clone();
        let tabs: Tabs = self.tabs.convert(&theme.components, &theme.border_symbol_sets)?;
        let active_panes = Config::calc_active_panes(&tabs.tabs, &theme.layout);

        let (address, password) =
            MpdAddress::resolve(address_cli, password_cli, self.address, self.password);
        let album_art_method = self.album_art.method;
        let mut config = Config {
            theme_name: self.theme,
            cache_dir: self.cache_dir.map(|v| tilde_expand_path(&v)),
            lyrics_dir: self.lyrics_dir.map(|v| {
                let v = tilde_expand(&v);
                if v.ends_with('/') { v.into_owned() } else { format!("{v}/") }
            }),
            lyrics_offset: LrcOffset::from_millis(self.lyrics_offset_ms),
            enable_lyrics_index: self.enable_lyrics_index,
            enable_lyrics_hot_reload: self.enable_lyrics_hot_reload,
            tabs,
            original_tabs_definition,
            active_panes,
            address,
            password,
            volume_step: self.volume_step,
            max_fps: self.max_fps,
            scrolloff: self.scrolloff,
            wrap_navigation: self.wrap_navigation,
            status_update_interval_ms: self.status_update_interval_ms.map(|v| v.max(16)),
            mpd_read_timeout: Duration::from_millis(self.mpd_read_timeout_ms),
            mpd_write_timeout: Duration::from_millis(self.mpd_write_timeout_ms),
            mpd_idle_read_timeout_ms: self.mpd_idle_read_timeout_ms.map(Duration::from_millis),
            enable_mouse: self.enable_mouse,
            scroll_amount: self.scroll_amount,
            enable_config_hot_reload: self.enable_config_hot_reload,
            normal_timeout_ms: self.normal_timeout_ms,
            insert_timeout_ms: self.insert_timeout_ms,
            keybinds: self.keybinds.try_into()?,
            select_current_song_on_change: self.select_current_song_on_change,
            center_current_song_on_change: self.center_current_song_on_change,
            search: self.search.try_into()?,
            artists: self.artists.into(),
            album_art: self.album_art.into(),
            on_song_change: self.on_song_change.map(|arr| {
                Arc::new(arr.into_iter().map(|v| tilde_expand(&v).into_owned()).collect_vec())
            }),
            exec_on_song_change_at_start: self.exec_on_song_change_at_start,
            on_resize: self.on_resize.map(|arr| {
                Arc::new(arr.into_iter().map(|v| tilde_expand(&v).into_owned()).collect_vec())
            }),
            show_playlists_in_browser: self.show_playlists_in_browser,
            browser_song_sort: Arc::new(SortOptions {
                mode: SortMode::Format(
                    self.browser_song_sort.iter().cloned().map(SongProperty::from).collect_vec(),
                ),
                group_by_type: true,
                reverse: false,
                ignore_leading_the: self.ignore_leading_the,
                fold_case: true,
            }),
            directories_sort: Arc::new(match self.directories_sort {
                SortModeFile::Format { group_by_type, reverse } => SortOptions {
                    mode: SortMode::Format(
                        theme
                            .browser_song_format
                            .0
                            .iter()
                            .flat_map(|prop| prop.kind.collect_properties())
                            .collect_vec(),
                    ),
                    group_by_type,
                    reverse,
                    ignore_leading_the: self.ignore_leading_the,
                    fold_case: true,
                },
                SortModeFile::SortFormat { group_by_type, reverse } => SortOptions {
                    mode: SortMode::Format(
                        self.browser_song_sort.into_iter().map(SongProperty::from).collect_vec(),
                    ),
                    group_by_type,
                    reverse,
                    ignore_leading_the: self.ignore_leading_the,
                    fold_case: true,
                },
                SortModeFile::ModifiedTime { group_by_type, reverse } => SortOptions {
                    mode: SortMode::ModifiedTime,
                    group_by_type,
                    reverse,
                    ignore_leading_the: self.ignore_leading_the,
                    fold_case: true,
                },
            }),
            theme,
            rewind_to_start_sec: self.rewind_to_start_sec,
            keep_state_on_song_change: self.keep_state_on_song_change,
            reflect_changes_to_playlist: self.reflect_changes_to_playlist,
            cava: self.cava.into(),
            radio: self.radio.into(),
            jellyfin: self.jellyfin.into(),
            video: self.video.into(),
            mpv: self.mpv.into(),
            torrent: self.torrent.into(),
            auto_open_downloads: self.auto_open_downloads,
            duration_format: DurationFormat::parse(&self.duration_format)?,
            ui: UiSettings::default(),
        };

        // Derive the border / focused-border / selection colors from the text
        // color so external theme edits (sttm/blsw) keep the accents in sync
        // automatically.
        derive_theme_accents(&mut config.theme);

        if skip_album_art_check {
            return Ok(config);
        }

        let is_tmux = tmux::is_inside_tmux();
        if is_tmux && !tmux::is_passthrough_enabled()? {
            tmux::enable_passthrough()?;
        }

        config.album_art.method =
            TERMINAL.resolve_image_backend(self.image_method.unwrap_or(album_art_method));

        Ok(config)
    }
}

impl FromStr for Args {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Args::try_parse_from(std::iter::once("").chain(s.split_whitespace()))?)
    }
}

impl From<OnOff> for bool {
    fn from(value: OnOff) -> Self {
        match value {
            OnOff::On => true,
            OnOff::Off => false,
        }
    }
}

impl From<OnOffOneshot> for crate::mpd::commands::status::OnOffOneshot {
    fn from(value: OnOffOneshot) -> Self {
        match value {
            OnOffOneshot::On => crate::mpd::commands::status::OnOffOneshot::On,
            OnOffOneshot::Off => crate::mpd::commands::status::OnOffOneshot::Off,
            OnOffOneshot::Oneshot => crate::mpd::commands::status::OnOffOneshot::Oneshot,
        }
    }
}

/// Scale an RGB color by `f` (0..1) for derived accents.
pub(crate) fn scale_color(color: Color, f: f64) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            (f64::from(r) * f) as u8,
            (f64::from(g) * f) as u8,
            (f64::from(b) * f) as u8,
        ),
        other => other,
    }
}

/// Hover color for clickable text/buttons: the base color blended 35%
/// toward white — lighter **and** less saturated at the same time. Named
/// colors (white/black/yellow…) can't be lightened reliably, so they pass
/// through unchanged.
pub(crate) fn hover_color(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => {
            let lighten = |c: u8| {
                let c = f64::from(c);
                (c + (255.0 - c) * 0.35).round() as u8
            };
            Color::Rgb(lighten(r), lighten(g), lighten(b))
        }
        other => other,
    }
}

/// The hover style of a clickable element: its foreground lightened and
/// desaturated (named colors pass through). Text without an explicit color
/// (rendered in the terminal's default foreground) becomes white on hover,
/// which is always lighter than the default.
pub(crate) fn hover_style(style: Style) -> Style {
    let mut style = style;
    style.fg = style.fg.map(hover_color).or(Some(Color::White));
    style.bg = style.bg.map(hover_color);
    style
}

/// Apply the hover style to every span of a line (lightening each span's
/// own color so the row keeps its per-span color relationships).
pub(crate) fn hover_line(line: &mut ratatui::text::Line) {
    for span in line.spans.iter_mut() {
        span.style = hover_style(span.style);
    }
}

/// Derive the dependent colors from the theme's text color so they track
/// external theme changes (blur modes) automatically: the pane outlines
/// (borders) match the cava bars' accent color, the selection highlight and
/// the active tab highlight share the same accent, and the seekbar follows.
pub(crate) fn derive_theme_accents(theme: &mut UiConfig) {
    let Some(base) = theme.text_color else {
        // No accent color: the hover highlight just mirrors the selection.
        theme.hovered_item_style = theme.current_item_style;
        return;
    };
    // The cava bars are the accent; the pane outlines use the very same
    // color so the whole frame matches the visualizer.
    theme.borders_style.fg = Some(base);
    theme.highlight_border_style.fg = Some(base);
    theme.current_item_style.bg = Some(scale_color(base, 0.50));
    // Multi-selected (marked) rows: the same highlight effect with a
    // lighter background (0.65 vs the selection's 0.50).
    theme.marked_item_style = theme.current_item_style;
    theme.marked_item_style.bg = Some(scale_color(base, 0.65));
    // Mouse-over rows: the same highlight effect, slightly brighter than
    // the selection (0.58) but not as bright as marked rows (0.65).
    theme.hovered_item_style = theme.current_item_style;
    theme.hovered_item_style.bg = Some(scale_color(base, 0.58));
    theme.cava.bar_color =
        crate::config::theme::cava::CavaColor::Single(base.into_crossterm());
    // The active tab highlight reuses the selection highlight used
    // everywhere else (current_item_style), with the text color on top.
    theme.tab_bar.active_style.fg = Some(base);
    theme.tab_bar.active_style.bg = theme.current_item_style.bg;
    theme.progress_bar.elapsed_style.fg = Some(base);
    theme.progress_bar.thumb_style.fg = Some(base);
    theme.progress_bar.track_style.fg = Some(scale_color(base, 0.4));
}

pub mod utils {
    use std::{
        borrow::Cow,
        path::{MAIN_SEPARATOR, Path, PathBuf},
    };

    use crate::shared::env::ENV;

    pub fn tilde_expand_path(inp: &Path) -> PathBuf {
        let Ok(home) = ENV.var("HOME") else {
            return inp.to_owned();
        };
        let home = home.strip_suffix(MAIN_SEPARATOR).unwrap_or(home.as_ref());

        if let Ok(inp) = inp.strip_prefix("~") {
            if inp.as_os_str().is_empty() {
                return home.into();
            }

            return PathBuf::from(home.to_owned()).join(inp);
        }

        inp.to_path_buf()
    }

    pub fn tilde_expand(inp: &str) -> Cow<'_, str> {
        let Ok(home) = ENV.var("HOME") else {
            return Cow::Borrowed(inp);
        };
        let home = home.strip_suffix("/").unwrap_or(home.as_ref());

        if let Some(inp) = inp.strip_prefix('~') {
            if inp.is_empty() {
                return Cow::Owned(home.to_owned());
            }

            if inp.starts_with(MAIN_SEPARATOR) {
                return Cow::Owned(format!("{home}{inp}"));
            }
        }

        Cow::Borrowed(inp)
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    mod tests {
        use std::{
            path::PathBuf,
            sync::{LazyLock, Mutex},
        };

        use test_case::test_case;

        use super::tilde_expand;
        use crate::{config::utils::tilde_expand_path, shared::env::ENV};

        static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

        #[test_case("~", "/home/some_user")]
        #[test_case("~enene", "~enene")]
        #[test_case("~nope/", "~nope/")]
        #[test_case("~/yes", "/home/some_user/yes")]
        #[test_case("no/~/no", "no/~/no")]
        #[test_case("basic/path", "basic/path")]
        fn home_dir_present(input: &str, expected: &str) {
            let _guard = TEST_LOCK.lock().unwrap();

            ENV.clear();
            ENV.set("HOME".to_string(), "/home/some_user/".to_string());
            assert_eq!(tilde_expand(input), expected);
        }

        #[test_case("~", "~")]
        #[test_case("~enene", "~enene")]
        #[test_case("~nope/", "~nope/")]
        #[test_case("~/yes", "~/yes")]
        #[test_case("no/~/no", "no/~/no")]
        #[test_case("basic/path", "basic/path")]
        fn home_dir_not_present(input: &str, expected: &str) {
            let _guard = TEST_LOCK.lock().unwrap();

            ENV.clear();
            ENV.remove("HOME");
            assert_eq!(tilde_expand(input), expected);
        }

        #[test_case("~", "/home/some_user")]
        #[test_case("~enene", "~enene")]
        #[test_case("~nope/", "~nope/")]
        #[test_case("~/yes", "/home/some_user/yes")]
        #[test_case("no/~/no", "no/~/no")]
        #[test_case("basic/path", "basic/path")]
        fn home_dir_present_path(input: &str, expected: &str) {
            let _guard = TEST_LOCK.lock().unwrap();

            ENV.clear();
            ENV.set("HOME".to_string(), "/home/some_user/".to_string());

            let got = tilde_expand_path(&PathBuf::from(input));
            assert_eq!(got, PathBuf::from(expected));
        }

        #[test_case("~", "~")]
        #[test_case("~enene", "~enene")]
        #[test_case("~nope/", "~nope/")]
        #[test_case("~/yes", "~/yes")]
        #[test_case("no/~/no", "no/~/no")]
        #[test_case("basic/path", "basic/path")]
        fn home_dir_not_present_path(input: &str, expected: &str) {
            let _guard = TEST_LOCK.lock().unwrap();

            ENV.clear();
            ENV.remove("HOME");

            let got = tilde_expand_path(&PathBuf::from(input));
            assert_eq!(got, PathBuf::from(expected));
        }

        #[test]
        fn torrent_cache_dir_default_is_expanded() {
        let _home_guard = crate::tests::fixtures::HOME_LOCK.lock().unwrap();
            use crate::config::{ConfigFile, UiConfig};

            let _guard = TEST_LOCK.lock().unwrap();
            // The runtime torrent config expands the `~` in the default
            // cache dir (a config-file value is expanded too): without a
            // `torrent:` section the engine and the "Play and Download"
            // move must not address a literal `~/…` path.
            ENV.clear();
            ENV.set("HOME".to_string(), "/home/some_user".to_string());
            let config = ConfigFile::default()
                .into_config(UiConfig::default(), None, None, true)
                .expect("Default config should be valid");
            assert_eq!(
                config.torrent.cache_dir.to_string_lossy(),
                "/home/some_user/.cache/s2udio/torrents"
            );
            // A config-file value is expanded the same way.
            let mut file = ConfigFile::default();
            file.torrent.cache_dir = Some("~/custom/torrents".into());
            let config = file
                .into_config(UiConfig::default(), None, None, true)
                .expect("config should be valid");
            assert_eq!(
                config.torrent.cache_dir.to_string_lossy(),
                "/home/some_user/custom/torrents"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{UiConfig, derive_theme_accents, scale_color};
    #[cfg(debug_assertions)]
    use crate::config::keys::KeyConfigFile;
    use crate::config::{
        Config, ConfigFile, S2udioConfigFile, UiSettings, theme::UiConfigFile,
    };
    use crate::config::tabs::{PaneType, TabName};

    #[test]
    fn s2udio_overlay_overrides_the_base_sections_and_leaves_the_rest() {
        // Round 19: the ~/.config/s2udio/config.ron overlay only carries
        // the s2udio sections; merging it onto a base rmpc config must
        // replace exactly those sections and leave the base fields alone.
        let mut base = ConfigFile::default();
        base.address = "192.0.2.1:6600".to_owned();
        base.torrent.enabled = Some(false); // base file: disabled

        let overlay = S2udioConfigFile {
            torrent: Some(crate::config::torrent::TorrentFile {
                enabled: Some(true),
                ..Default::default()
            }),
            radio: None,
            jellyfin: None,
            video: None,
            mpv: None,
        };
        overlay.merge_into(&mut base);

        // The s2udio section came from the overlay…
        assert_eq!(base.torrent.enabled, Some(true), "overlay torrent.enabled wins");
        // …and the base fields are untouched.
        assert_eq!(base.address, "192.0.2.1:6600");
    }

    #[test]
    fn example_s2udio_overlay_parses() {
        // The shipped ~/.config/s2udio/config.ron template must deserialize
        // as the overlay (all sections commented out = empty overlay) and
        // merge as a no-op over a base config.
        let overlay: S2udioConfigFile = ron::de::from_str(include_str!(
            "../../assets/example_s2udio_config.ron"
        ))
        .expect("example overlay parses");
        let mut base = ConfigFile::default();
        overlay.merge_into(&mut base);
        // Torrent stays enabled-by-default (no overlay section).
        assert_eq!(base.torrent.enabled, Some(true));
    }

    #[test]
    fn s2udio_overlay_defaults_are_all_optional() {
        // An empty (or absent) overlay must deserialize and merge to a
        // no-op — the base file's sections (or defaults) apply.
        let overlay: S2udioConfigFile = ron::de::from_str("()").expect("empty overlay parses");
        let mut base = ConfigFile::default();
        base.torrent.enabled = Some(false);
        overlay.merge_into(&mut base);
        assert_eq!(
            base.torrent.enabled,
            Some(false),
            "absent overlay leaves the base section"
        );
    }

    #[test]
    fn ui_settings_hide_panes_and_tabs() {
        let mut config = Config::default();
        assert!(!config.is_pane_hidden(&PaneType::AlbumArt));
        assert!(!config.is_pane_hidden(&PaneType::Lyrics));
        assert!(!config.is_pane_hidden(&PaneType::Cava));
        assert!(!config.is_tab_hidden(&TabName::from("Radio")));
        assert!(!config.is_tab_hidden(&TabName::from("Jellyfin")));

        config.ui = UiSettings {
            show_album_art: false,
            show_lyrics: false,
            show_cava: false,
            show_radio_tab: false,
            show_jellyfin_tab: false,
            auto_show_chapters: true,
        };
        assert!(config.is_pane_hidden(&PaneType::AlbumArt));
        assert!(config.is_pane_hidden(&PaneType::Lyrics));
        assert!(config.is_pane_hidden(&PaneType::Cava));
        assert!(!config.is_pane_hidden(&PaneType::Queue));
        assert!(config.is_tab_hidden(&TabName::from("Radio")));
        assert!(config.is_tab_hidden(&TabName::from("Jellyfin")));
        assert!(config.is_tab_hidden(&TabName::from("radio"))); // case-insensitive
        assert!(!config.is_tab_hidden(&TabName::from("Local")));
    }

    #[test]
    fn derive_theme_accents_follows_text_color() {
        use ratatui::prelude::IntoCrossterm as _;

        let mut theme = UiConfig::default();
        let base = ratatui::style::Color::Rgb(0xff, 0xb4, 0x54);
        theme.text_color = Some(base);
        derive_theme_accents(&mut theme);
        // Pane outlines match the cava bars (the text color itself).
        assert_eq!(theme.borders_style.fg, Some(base));
        assert_eq!(theme.highlight_border_style.fg, Some(base));
        match theme.cava.bar_color {
            super::theme::cava::CavaColor::Single(c) => {
                assert_eq!(c, base.into_crossterm())
            }
            _ => panic!("expected a single cava bar color"),
        }
        // Selection + active tab share the same highlight accent.
        assert_eq!(theme.current_item_style.bg, Some(scale_color(base, 0.50)));
        assert_eq!(theme.tab_bar.active_style.bg, theme.current_item_style.bg);
        assert_eq!(theme.tab_bar.active_style.fg, Some(base));
        // Multi-selected rows: the same highlight effect with a lighter bg.
        assert_eq!(theme.marked_item_style.bg, Some(scale_color(base, 0.65)));
        let (mut current, mut marked) = (theme.current_item_style, theme.marked_item_style);
        current.bg = None;
        marked.bg = None;
        assert_eq!(marked, current, "marked copies the selection effect, only the bg lightens");
        // Mouse-over rows: the same highlight effect, brighter than the
        // selection but dimmer than marked rows.
        assert_eq!(theme.hovered_item_style.bg, Some(scale_color(base, 0.58)));
        let (mut current, mut hovered) = (theme.current_item_style, theme.hovered_item_style);
        current.bg = None;
        hovered.bg = None;
        assert_eq!(
            hovered, current,
            "hover copies the selection effect, only the bg lightens"
        );
    }

    #[test]
    fn hover_color_lightens_and_desaturates() {
        use ratatui::style::Color;

        // The grey text color lightens toward white.
        let c = super::hover_color(Color::Rgb(0x8f, 0x8f, 0x8f));
        assert_eq!(c, Color::Rgb(0xb6, 0xb6, 0xb6));
        // A saturated accent moves toward white: lighter *and* less
        // saturated (channels converge toward each other).
        let c = super::hover_color(Color::Rgb(0xff, 0x00, 0x00));
        assert_eq!(c, Color::Rgb(0xff, 0x59, 0x59));
        assert!(f64::from(0xff) > f64::from(0x59));
        // Named colors pass through unchanged.
        assert_eq!(super::hover_color(Color::Yellow), Color::Yellow);
    }

    #[test]
    fn hover_style_unstyled_text_becomes_white() {
        use ratatui::style::{Color, Style};

        let s = super::hover_style(Style::default());
        assert_eq!(s.fg, Some(Color::White));
        assert_eq!(s.bg, None);
        let s = super::hover_style(Style::default().fg(Color::Rgb(0x40, 0x40, 0x40)));
        assert_eq!(s.fg, Some(Color::Rgb(0x83, 0x83, 0x83)));
        let s = super::hover_style(Style::default().bg(Color::Rgb(0x40, 0x40, 0x40)));
        assert_eq!(s.bg, Some(Color::Rgb(0x83, 0x83, 0x83)));
    }

    #[test]
    fn derive_theme_accents_does_not_recolor_lists() {
        let mut theme = UiConfig::default();
        let static_color = ratatui::style::Color::Rgb(0x8f, 0x8f, 0x8f);
        theme.list_text_color = Some(static_color);
        theme.text_color = Some(ratatui::style::Color::Rgb(0xff, 0xb4, 0x54));
        derive_theme_accents(&mut theme);
        assert_eq!(
            theme.list_text_color,
            Some(static_color),
            "the list text color is untouched by the accent derivation"
        );
    }

    #[test]
    fn derive_theme_accents_is_noop_without_text_color() {
        let mut theme = UiConfig::default();
        theme.text_color = None;
        let before = theme.borders_style.fg;
        derive_theme_accents(&mut theme);
        assert_eq!(theme.borders_style.fg, before);
        // Without an accent the hover highlight mirrors the selection.
        assert_eq!(theme.hovered_item_style, theme.current_item_style);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn example_config_equals_default() {
        let config = ConfigFile::default();
        let path =
            format!("{}/assets/example_config.ron", std::env::var("CARGO_MANIFEST_DIR").unwrap());

        let mut f: ConfigFile = ron::de::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        f.keybinds.logs = KeyConfigFile::default().logs;

        assert_eq!(config, f);
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn example_config_equals_default() {
        let config = ConfigFile::default();
        let path =
            format!("{}/assets/example_config.ron", std::env::var("CARGO_MANIFEST_DIR").unwrap());

        let f: ConfigFile = ron::de::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();

        assert_eq!(config, f);
    }

    #[test]
    fn example_theme_equals_default() {
        let theme = UiConfigFile::default();
        let path =
            format!("{}/assets/example_theme.ron", std::env::var("CARGO_MANIFEST_DIR").unwrap());

        let file = ron::de::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();

        assert_eq!(theme, file);
    }
}
