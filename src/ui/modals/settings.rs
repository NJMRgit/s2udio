//! The settings panel: a two-pane navigator. The left sidebar lists the
//! sections (general / keybinds / mpd); the right pane shows the selected
//! section's rows. Mouse navigation is fully supported (sidebar clicks,
//! row clicks, [+]/[-]/[<]/[>] buttons, scroll).
use std::process::Command;
use anyhow::Result;
use ratatui::{
    Frame, layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols, text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use super::{
    Modal, confirm_modal::{Action, ConfirmModal},
    input_modal::InputModal, remap_keys,
};
use crate::{
    AppEvent,
    config::{
        Config, UiSettings, cava::CavaOverridesFile, keys::{Key, KeyConfig, KeySequence},
        scale_color, state::AppStateFile, theme::UiConfig,
    },
    ctx::Ctx, mpd::{commands::status::OnOffOneshot, mpd_client::MpdClient},
    shared::{
        id::{self, Id},
        keys::{ActionEvent, Actions},
        macros::{modal, status_error, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::MpdClientExt,
    },
    ui::{OPEN_OUTPUTS_MODAL, UiAppEvent},
};
#[derive(Debug, Clone)]
struct PwNode {
    /// The cava `source` value (the pw `node.name`).
    name: String,
    /// Human-readable description shown in the row.
    description: String,
    /// `node.virtual == "true"` (Easy Effects etc.), hidden unless requested.
    is_virtual: bool,
}
/// Every capture target for the cava device row: pactl's source list
/// (sink monitors + mics + virtual sources) when available, falling back to
/// the wpctl sink list (offered as `X.monitor`) on systems without
/// pipewire-pulse. `wpctl status` alone is not enough: it only lists sinks
/// (and omits their monitors), while cava must capture a sink's *monitor*
/// to visualize what it plays.
fn pipewire_sources() -> Vec<PwNode> {
    if let Some(mut nodes) = pipewire_sources_pactl() {
        nodes.sort_by(|a, b| (a.is_virtual, &a.name).cmp(&(b.is_virtual, &b.name)));
        return nodes;
    }
    pipewire_sinks_wpctl()
}
/// `pactl list sources`: lists *every* source — including the `X.monitor`
/// monitors of sinks, which are exactly the cava `source` values wanted.
/// `None` when pactl (pipewire-pulse) is not installed or the server is
/// unreachable.
fn pipewire_sources_pactl() -> Option<Vec<PwNode>> {
    let out = Command::new("pactl").arg("list").arg("sources").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_pactl_sources(&String::from_utf8_lossy(&out.stdout)))
}
/// Parse `pactl list sources`: each `Source #N` block's `Name:` (the cava
/// `source` value, e.g. `Media.monitor`), `Description:` (human-readable)
/// and the `node.virtual` property.
fn parse_pactl_sources(stdout: &str) -> Vec<PwNode> {
    let mut result = Vec::new();
    for block in stdout.split("Source #").skip(1) {
        let mut name = None;
        let mut description = None;
        let mut is_virtual = false;
        for line in block.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("Name:") {
                name = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("Description:") {
                description = Some(v.trim().to_string());
            } else if line.contains("node.virtual = \"true\"") {
                is_virtual = true;
            }
        }
        if let (Some(name), Some(description)) = (name, description) {
            result
                .push(PwNode {
                    name,
                    description,
                    is_virtual,
                });
        }
    }
    result
}
/// Fallback when pactl is unavailable: the wpctl sink list, offered as
/// `X.monitor` capture sources (the form cava needs to visualize a sink —
/// a raw sink name makes cava use `target.object`, which PipeWire does not
/// feed on every setup).
fn pipewire_sinks_wpctl() -> Vec<PwNode> {
    let mut result = Vec::new();
    let Ok(out) = Command::new("wpctl").arg("status").output() else {
        return result;
    };
    if !out.status.success() {
        return result;
    }
    for (id, description) in parse_wpctl_status(&String::from_utf8_lossy(&out.stdout)) {
        let Some((name, is_virtual)) = inspect_sink(id) else { continue };
        result
            .push(PwNode {
                name: format!("{name}.monitor"),
                description: format!("Monitor of {description}"),
                is_virtual,
            });
    }
    result
}
/// Parse `wpctl status`: the `Audio -> Sinks` section's `id. description`
/// lines.
fn parse_wpctl_status(stdout: &str) -> Vec<(u32, String)> {
    let mut result = Vec::new();
    let mut section = String::new();
    for raw in stdout.lines() {
        let line = raw.trim();
        if line.starts_with("├─") || line.starts_with("└─") {
            let name = line
                .trim_start_matches(['├', '└'])
                .trim_start_matches('─')
                .trim()
                .trim_end_matches(':')
                .to_string();
            section = name;
            continue;
        }
        if section != "Sinks" {
            continue;
        }
        let Some(rest) = line.strip_prefix('│') else { continue };
        let rest = rest.trim();
        let Some(dot) = rest.find(". ") else { continue };
        let Ok(id) = rest[..dot].trim().parse::<u32>() else { continue };
        let description = rest[dot + 2..]
            .split(" [vol:")
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        if !description.is_empty() {
            result.push((id, description));
        }
    }
    result
}
fn parse_wpctl_inspect(stdout: &str) -> Option<(String, bool)> {
    let mut name = None;
    let mut is_virtual = false;
    for raw in stdout.lines() {
        let line = raw.trim().trim_start_matches("* ").trim();
        if let Some(v) = line.strip_prefix("node.name = ") {
            name = Some(v.trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("node.virtual = ") {
            is_virtual = v.trim_matches('"') == "true";
        }
    }
    Some((name?, is_virtual))
}
fn inspect_sink(id: u32) -> Option<(String, bool)> {
    let out = Command::new("wpctl").arg("inspect").arg(id.to_string()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_wpctl_inspect(&String::from_utf8_lossy(&out.stdout))
}
/// Open a directory picker so the user can browse to the library location:
/// KDE's kdialog first (the KDE default chooser), then GNOME's zenity.
/// Returns `Ok(Some(path))` when a folder was picked, `Ok(None)` when the
/// user cancelled, and `Err(())` when no picker is installed.
fn pick_directory(start: &str) -> Result<Option<String>, ()> {
    let pickers: [(&str, Vec<String>); 2] = [
        ("kdialog", vec!["--getexistingdirectory".into(), start.into()]),
        (
            "zenity",
            vec![
                "--file-selection".into(), "--directory".into(), "--filename".into(),
                start.into(),
            ],
        ),
    ];
    for (cmd, args) in pickers {
        match Command::new(cmd).args(&args).output() {
            Ok(out) if out.status.success() => {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if path.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(path));
            }
            Ok(_) => return Ok(None),
            Err(_) => continue,
        }
    }
    Err(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    Keybinds,
    Mpv,
    Mpd,
    Jellyfin,
    Torrent,
}
/// Which settings pane owns keyboard navigation. Mirrors the tab-pane
/// scheme: w/s/↑/↓ move the focused pane, d/→/Enter activate, a/← steps
/// back to the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsFocus {
    Sidebar,
    Content,
}
impl Section {
    fn all() -> [Section; 6] {
        [
            Section::General,
            Section::Keybinds,
            Section::Mpv,
            Section::Mpd,
            Section::Jellyfin,
            Section::Torrent,
        ]
    }
    fn name(self) -> &'static str {
        match self {
            Section::General => "general",
            Section::Keybinds => "keybinds",
            Section::Mpv => "mpv",
            Section::Mpd => "mpd",
            Section::Jellyfin => "jellyfin",
            Section::Torrent => "torrent",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeneralRow {
    FeaturesHeader,
    AlbumArt,
    Lyrics,
    Cava,
    Radio,
    RadioReload,
    Jellyfin,
    VideoPlayback,
    AutoChapters,
    Mpdris2Notifications,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MpvRow {
    Header,
    AudioLang,
    Subtitles,
    /// The "svp support" toggle: pass `--input-ipc-server=/tmp/mpvsocket`
    /// so SVP4's manager can drive frame interpolation (and s2udio tracks
    /// playback over the same socket).
    Svp,
}
/// Which mpv preference the language picker is choosing for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MpvLanguageTarget {
    Audio,
    Subtitles,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MpdRow {
    LibraryHeader,
    LibraryPath,
    Update,
    Rescan,
    PlaybackHeader,
    Crossfade,
    Repeat,
    Random,
    Single,
    Consume,
    DevicesHeader,
    Outputs,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JellyfinRow {
    Header,
    ServerUrl,
    Username,
    Password,
    SignIn,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TorrentRow {
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
/// A staged appearance change: not applied until the panel is closed with
/// Save. `Transparent` maps to `None` (no color / default background).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagedColor {
    Unchanged,
    Transparent,
    Set(Color),
}
/// The cava values the panel manages, staged while the panel is open and
/// applied (runtime config + `cava.ron` sidecar) only when it is closed
/// with Save. Values are resolved (defaults applied) so the rows always
/// show what would be saved.
#[derive(Debug, Clone, PartialEq)]
struct StagedCava {
    framerate: u16,
    autosens: bool,
    sensitivity: u16,
    lower_cutoff_freq: u16,
    higher_cutoff_freq: u32,
    channels: u32,
    source: String,
    noise_reduction: u8,
    monstercat: bool,
    waves: bool,
    /// Round 29: the configured PipeWire node name for the cava stream.
    /// Not editable from the panel (set it in `cava.ron`), but carried
    /// through so a panel Save never drops it.
    node_name: Option<String>,
}
impl StagedCava {
    fn from_config(config: &Config) -> Self {
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
    fn to_overrides(&self) -> CavaOverridesFile {
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
struct AdjustSnapshot {
    cava: StagedCava,
    /// The live MPD crossfade seconds at session start (the crossfade row
    /// adjusts MPD live, so cancel restores it).
    crossfade: u32,
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
}
/// Which theme color an appearance row edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppearanceTarget {
    /// Content text color (the queue / tab-list text). Never touched by the
    /// blur watcher.
    Text,
    Borders,
    FocusBorder,
    Selection,
    Accent,
    Background,
    /// The UI chrome accent: `text_color`, which the accent derivation
    /// (borders, cava bars, selection, seekbar, controls) follows. The blur
    /// watcher owns it while a mode is active.
    Ui,
}
impl AppearanceTarget {
    fn name(self) -> &'static str {
        match self {
            AppearanceTarget::Text => "text color",
            AppearanceTarget::Ui => "UI colors",
            AppearanceTarget::Borders => "border color",
            AppearanceTarget::FocusBorder => "focused border",
            AppearanceTarget::Selection => "selection highlight",
            AppearanceTarget::Accent => "highlighted item",
            AppearanceTarget::Background => "background color",
        }
    }
    /// Persistence order (matches the enum discriminant order, so
    /// `AppearanceTarget as usize` indices stay stable). `Ui` is appended at
    /// the end so legacy 6-value `state.ron` appearance arrays still align
    /// with the original targets.
    pub(crate) fn all() -> [AppearanceTarget; 7] {
        [
            AppearanceTarget::Text,
            AppearanceTarget::Borders,
            AppearanceTarget::FocusBorder,
            AppearanceTarget::Selection,
            AppearanceTarget::Accent,
            AppearanceTarget::Background,
            AppearanceTarget::Ui,
        ]
    }
    /// Display order in the settings panel (text color, UI colors, border
    /// color, …).
    fn display_order() -> [AppearanceTarget; 7] {
        [
            AppearanceTarget::Text,
            AppearanceTarget::Ui,
            AppearanceTarget::Borders,
            AppearanceTarget::FocusBorder,
            AppearanceTarget::Selection,
            AppearanceTarget::Accent,
            AppearanceTarget::Background,
        ]
    }
}
/// Parse a 3- or 6-digit hex color (with or without '#').
fn parse_hex_color(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    let expanded: String = match s.len() {
        3 => s.chars().flat_map(|c| std::iter::repeat(c).take(2)).collect(),
        6 => s.to_string(),
        _ => return None,
    };
    if !expanded.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(&expanded, 16).ok()?;
    Some(Color::Rgb(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8))
}
fn color_hex(color: &Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}"),
    }
}
/// The current theme color of an appearance target. `blur_active` tells
/// whether the blur watcher currently owns `text_color`: while a mode is
/// active the persisted value must be the *configured* color, never the
/// transient mode accent (a save while a mode runs would otherwise freeze a
/// stale mode color into state.ron).
fn theme_color(
    theme: &crate::config::theme::UiConfig,
    target: AppearanceTarget,
    blur_active: bool,
) -> Option<Color> {
    match target {
        AppearanceTarget::Text => theme.list_text_color.or(theme.text_color),
        AppearanceTarget::Ui => {
            if blur_active {
                theme.list_text_color.or(theme.text_color)
            } else {
                theme.text_color
            }
        }
        AppearanceTarget::Borders => theme.borders_style.fg,
        AppearanceTarget::FocusBorder => theme.highlight_border_style.fg,
        AppearanceTarget::Selection => theme.current_item_style.bg,
        AppearanceTarget::Accent => theme.highlighted_item_style.fg,
        AppearanceTarget::Background => theme.background_color,
    }
}
/// The appearance values to persist to state.ron after a settings save:
/// each target's resolved color as a hex string, "" for transparent, and
/// `None` when the theme's value is not an RGB color (left to the theme on
/// restore). While a blur mode is scheduled the UI-accent target persists
/// the configured color (see `theme_color`).
pub(crate) fn persisted_appearance(
    config: &crate::config::Config,
) -> Vec<Option<String>> {
    let blur_active = crate::core::blur::read_schedule_mode().is_some();
    persisted_appearance_with(config, blur_active)
}
/// The persisted values with an explicit blur-mode signal (deterministic in
/// tests).
fn persisted_appearance_with(
    config: &crate::config::Config,
    blur_active: bool,
) -> Vec<Option<String>> {
    AppearanceTarget::all()
        .iter()
        .map(|target| match theme_color(&config.theme, *target, blur_active) {
            Some(Color::Rgb(r, g, b)) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
            Some(_) => None,
            None => Some(String::new()),
        })
        .collect()
}
/// Apply persisted appearance values to a config's theme: a hex string sets
/// the color, "" makes it transparent, `None` leaves the theme default.
///
/// When a blur mode is scheduled (`blur_mode_active`), the targets the blur
/// watcher derives from the mode's color (the UI accent / Borders /
/// FocusBorder / Selection) are skipped: the persisted values only reflect
/// the theme at the time of the last settings save and would otherwise
/// flash stale colors (e.g. a previous mode's accent) until the watcher
/// re-derives them on its first tick. The content text color (Text) and the
/// highlighted-item / background colors are not blur-managed and always
/// restore.
pub(crate) fn apply_persisted_appearance(
    config: &mut crate::config::Config,
    values: &[Option<String>],
    blur_mode_active: bool,
) {
    for (target, value) in AppearanceTarget::all().iter().zip(values) {
        if blur_mode_active
            && matches!(
                target, AppearanceTarget::Ui | AppearanceTarget::Borders |
                AppearanceTarget::FocusBorder | AppearanceTarget::Selection
            )
        {
            continue;
        }
        match value.as_deref() {
            Some(hex) if hex.is_empty() => {
                set_appearance_color(&mut config.theme, *target, None);
            }
            Some(hex) => {
                if let Some(color) = parse_hex_color(hex) {
                    set_appearance_color(&mut config.theme, *target, Some(color));
                }
            }
            None => {}
        }
    }
}
#[derive(Debug, Clone)]
enum ContentRow {
    General(GeneralRow),
    KeyTableHeader,
    KeyHeader(remap_keys::Section),
    KeyItem(remap_keys::RemapRow),
    Appearance(AppearanceTarget),
    Mpd(MpdRow),
    Mpv(MpvRow),
    Jellyfin(JellyfinRow),
    Torrent(TorrentRow),
}
impl ContentRow {
    fn is_header(&self) -> bool {
        matches!(
            self, ContentRow::General(GeneralRow::FeaturesHeader | GeneralRow::CavaHeader
            | GeneralRow::AppearanceHeader,) | ContentRow::KeyTableHeader |
            ContentRow::KeyHeader(_) | ContentRow::Mpd(MpdRow::LibraryHeader |
            MpdRow::PlaybackHeader | MpdRow::DevicesHeader) |
            ContentRow::Mpv(MpvRow::Header) | ContentRow::Jellyfin(JellyfinRow::Header) |
            ContentRow::Torrent(TorrentRow::Header)
        )
    }
}
/// Mouse click targets within a row (button columns), resolved per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Click {
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
struct DeferredButton {
    offset: usize,
    label: &'static str,
    click: Click,
}
/// The current color of an appearance target in the theme (`None` =
/// transparent / unset, e.g. the background).
fn appearance_color(theme: &UiConfig, target: AppearanceTarget) -> Option<Color> {
    match target {
        AppearanceTarget::Text => theme.list_text_color.or(theme.text_color),
        AppearanceTarget::Ui => theme.text_color,
        AppearanceTarget::Borders => theme.borders_style.fg,
        AppearanceTarget::FocusBorder => theme.highlight_border_style.fg,
        AppearanceTarget::Selection => theme.current_item_style.bg,
        AppearanceTarget::Accent => theme.highlighted_item_style.fg,
        AppearanceTarget::Background => theme.background_color,
    }
}
pub(crate) fn set_appearance_color(
    theme: &mut UiConfig,
    target: AppearanceTarget,
    color: Option<Color>,
) {
    match target {
        AppearanceTarget::Text => theme.list_text_color = color,
        AppearanceTarget::Ui => theme.text_color = color,
        AppearanceTarget::Borders => theme.borders_style.fg = color,
        AppearanceTarget::FocusBorder => theme.highlight_border_style.fg = color,
        AppearanceTarget::Selection => {
            theme.current_item_style.bg = color;
            theme.marked_item_style.bg = color.map(|c| scale_color(c, 1.3));
        }
        AppearanceTarget::Accent => theme.highlighted_item_style.fg = color,
        AppearanceTarget::Background => theme.background_color = color,
    }
}
/// Truncate `s` to at most `width` columns, appending an ellipsis when cut.
fn truncate_col(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}
const FREQ_MIN_LIMIT: i64 = 20;
const FREQ_MAX_LIMIT: i64 = 22_000;
const DEFAULT_FREQ_MIN: u16 = 50;
const DEFAULT_FREQ_MAX: u32 = 15_000;
const FPS_MIN: u16 = 15;
const FPS_MAX: u16 = 120;
#[derive(Debug)]
pub struct SettingsModal {
    id: Id,
    section: Section,
    rows: Vec<ContentRow>,
    selected: usize,
    /// Highlighted sidebar item (w/s moves it, d populates the right pane).
    sidebar_selected: usize,
    /// Which pane owns keyboard navigation: the sidebar (w/s move the
    /// section highlight, d/→/Enter open it) or the content rows (w/s/↑/↓
    /// move the highlight, d/→/Enter toggle, a/← back to the sidebar).
    /// Matches the tab-pane navigation scheme (browser panes).
    focus: SettingsFocus,
    /// PipeWire capture sources (sink monitors + mics + virtual sources,
    /// with an "auto" pseudo-node first) for the device row.
    nodes: Vec<PwNode>,
    /// MPD update/rescan scope path (persisted in state.ron).
    library_path: String,
    /// Jellyfin server credentials, staged until a successful sign-in.
    jellyfin_url: String,
    jellyfin_username: String,
    jellyfin_password: String,
    /// Set by a successful sign-in; persisted to the jellyfin.ron sidecar
    /// on Save.
    jellyfin_credentials: Option<crate::config::jellyfin::JellyfinCredentialsFile>,
    /// The Jellyfin text row currently being typed into (inline edit, like
    /// the appearance colors).
    editing_jellyfin_row: Option<JellyfinRow>,
    /// The rqbit SOCKS5 proxy when the panel opened (staging baseline).
    torrent_socks_proxy_initial: String,
    /// Staged rqbit SOCKS5 proxy ("" = no proxy; applied to the engine
    /// config + persisted to state.ron on Save).
    torrent_socks_proxy_pending: String,
    /// UI show/hide toggles when the panel opened (staging baseline).
    ui_initial: UiSettings,
    /// Staged UI show/hide toggles (the live config is untouched until Save).
    ui_pending: UiSettings,
    /// Video playback mode when the panel opened.
    video_initial: crate::config::video::VideoPlaybackMode,
    /// Staged video playback mode (applied + persisted on Save).
    video_pending: crate::config::video::VideoPlaybackMode,
    /// mpv audio language when the panel opened.
    mpv_audio_initial: crate::config::mpv::MpvAudioLang,
    /// Staged mpv audio language (applied + persisted on Save).
    mpv_audio_pending: crate::config::mpv::MpvAudioLang,
    /// mpv subtitle mode when the panel opened.
    mpv_subtitles_initial: crate::config::mpv::MpvSubtitleMode,
    /// Staged mpv subtitle mode (applied + persisted on Save).
    mpv_subtitles_pending: crate::config::mpv::MpvSubtitleMode,
    /// SVP support when the panel opened.
    mpv_svp_initial: bool,
    /// Staged SVP support (applied + persisted on Save).
    mpv_svp_pending: bool,
    /// The last custom language code picked in the language window (the
    /// fallback when the audio/subtitle cycle lands on "custom"); seeded
    /// from the current setting or the OS language.
    mpv_custom_lang: String,
    /// Cava values when the panel opened.
    cava_initial: StagedCava,
    /// Staged cava values (live config + sidecar untouched until Save).
    cava_pending: StagedCava,
    /// Runtime keybinds when the panel opened; restored when the panel is
    /// closed with Discard.
    keybinds_snapshot: KeyConfig,
    /// Key remaps applied while the panel was open; persisted on Save.
    pending_remaps: Vec<PendingRemap>,
    /// Keybinds section is waiting for the new key.
    capturing: bool,
    /// Staged appearance changes, applied only when the panel is closed with
    /// Save (indexed by AppearanceTarget).
    appearance_pending: [StagedColor; 7],
    /// The appearance row currently being typed into.
    editing_color: Option<AppearanceTarget>,
    /// Hex digits typed so far (no '#').
    edit_buffer: String,
    /// The stepper row ([-] [+]) whose controls are focused for keyboard
    /// adjustment: Space/Enter enters, Space/Enter commits, Esc cancels
    /// (reverting the staged value). The selection is locked while set.
    adjusting: Option<usize>,
    /// The staged values when adjust mode started, so Esc can cancel the
    /// whole adjustment session.
    adjust_snapshot: Option<AdjustSnapshot>,
    /// First visible content row (the panel is compact, so long sections
    /// scroll).
    scroll: usize,
    /// Number of content rows visible in the current render.
    visible_rows: usize,
    /// Keybinds table column widths (computed each render from the rows).
    key_col_w: usize,
    action_col_w: usize,
    desc_col_w: usize,
    row_areas: Vec<(usize, Rect)>,
    click_targets: Vec<(Rect, Click)>,
    sidebar_areas: Vec<Rect>,
}
/// The Jellyfin server URL currently in use (Settings sidecar, then
/// jellytui's config) for the settings form.
fn jellyfin_current_url(ctx: &Ctx) -> String {
    let sidecar = crate::config::jellyfin::jellyfin_sidecar_path();
    let url = std::fs::read_to_string(&sidecar)
        .ok()
        .and_then(|c| {
            ron::de::from_str::<crate::config::jellyfin::JellyfinCredentialsFile>(&c)
                .ok()
        })
        .map(|c| c.server_url)
        .or_else(|| {
            crate::jellyfin::Jellyfin::from_config_file(&ctx.config.jellyfin.config_file)
                .map(|jf| jf.base)
        })
        .unwrap_or_default();
    url.trim_end_matches('/').to_owned()
}
impl SettingsModal {
    pub fn new(ctx: &Ctx) -> Self {
        let ui_initial = ctx.config.ui;
        let video_initial = ctx.config.video.playback;
        let mpv_audio_initial = ctx.config.mpv.audio_lang.clone();
        let mpv_subtitles_initial = ctx.config.mpv.subtitles.clone();
        let mpv_svp_initial = ctx.config.mpv.svp;
        let mpv_custom_lang = match (&mpv_audio_initial, &mpv_subtitles_initial) {
            (crate::config::mpv::MpvAudioLang::Custom { lang }, _)
            | (_, crate::config::mpv::MpvSubtitleMode::Custom { lang }) => lang.clone(),
            _ => {
                crate::config::mpv::os_language_code().unwrap_or_else(|| "en".to_owned())
            }
        };
        let cava_initial = StagedCava::from_config(&ctx.config);
        let torrent_socks_proxy = ctx
            .config
            .torrent
            .socks_proxy
            .clone()
            .unwrap_or_default();
        let mut modal = Self {
            id: id::new(),
            section: Section::General,
            rows: Vec::new(),
            selected: 0,
            sidebar_selected: 0,
            focus: SettingsFocus::Sidebar,
            nodes: Vec::new(),
            library_path: AppStateFile::load().mpd_library_path.unwrap_or_default(),
            jellyfin_url: jellyfin_current_url(ctx),
            jellyfin_username: String::new(),
            jellyfin_password: String::new(),
            jellyfin_credentials: None,
            editing_jellyfin_row: None,
            torrent_socks_proxy_initial: torrent_socks_proxy.clone(),
            torrent_socks_proxy_pending: torrent_socks_proxy,
            adjusting: None,
            adjust_snapshot: None,
            ui_initial,
            ui_pending: ui_initial,
            video_initial,
            video_pending: video_initial,
            mpv_audio_initial: mpv_audio_initial.clone(),
            mpv_audio_pending: mpv_audio_initial,
            mpv_subtitles_initial: mpv_subtitles_initial.clone(),
            mpv_subtitles_pending: mpv_subtitles_initial,
            mpv_svp_initial,
            mpv_svp_pending: mpv_svp_initial,
            mpv_custom_lang,
            cava_pending: cava_initial.clone(),
            cava_initial,
            keybinds_snapshot: ctx.config.keybinds.clone(),
            pending_remaps: Vec::new(),
            capturing: false,
            appearance_pending: [StagedColor::Unchanged; 7],
            editing_color: None,
            edit_buffer: String::new(),
            scroll: 0,
            visible_rows: 0,
            key_col_w: 0,
            action_col_w: 0,
            desc_col_w: 0,
            row_areas: Vec::new(),
            click_targets: Vec::new(),
            sidebar_areas: Vec::new(),
        };
        modal.refresh_nodes();
        modal.rows = modal.build_rows(ctx);
        modal
    }
    /// Refresh the PipeWire capture-source list (with the "auto" entry
    /// first): sink monitors (`X.monitor`) plus mics/virtual sources.
    /// Round 30: cava is PipeWire-only, so the list is always shown.
    fn refresh_nodes(&mut self) {
        let mut nodes = vec![
            PwNode { name : "auto".to_string(), description : "auto (default output)"
            .to_string(), is_virtual : false, }
        ];
        nodes.extend(pipewire_sources());
        self.nodes = nodes;
    }
    fn build_rows(&self, ctx: &Ctx) -> Vec<ContentRow> {
        match self.section {
            Section::General => {
                let mut rows = vec![
                    ContentRow::General(GeneralRow::FeaturesHeader),
                    ContentRow::General(GeneralRow::AlbumArt),
                    ContentRow::General(GeneralRow::Lyrics),
                    ContentRow::General(GeneralRow::Cava),
                    ContentRow::General(GeneralRow::Radio),
                    ContentRow::General(GeneralRow::RadioReload),
                    ContentRow::General(GeneralRow::Jellyfin),
                    ContentRow::General(GeneralRow::VideoPlayback),
                    ContentRow::General(GeneralRow::AutoChapters),
                    ContentRow::General(GeneralRow::Mpdris2Notifications),
                    ContentRow::General(GeneralRow::CavaHeader),
                    ContentRow::General(GeneralRow::AutoSens),
                    ContentRow::General(GeneralRow::Sensitivity),
                    ContentRow::General(GeneralRow::Fps),
                    ContentRow::General(GeneralRow::FreqMin),
                    ContentRow::General(GeneralRow::FreqMax),
                    ContentRow::General(GeneralRow::Channels),
                ];
                rows.push(ContentRow::General(GeneralRow::Device));
                rows.push(ContentRow::General(GeneralRow::VirtualDevices));
                rows.push(ContentRow::General(GeneralRow::NoiseReduction));
                rows.push(ContentRow::General(GeneralRow::Monstercat));
                rows.push(ContentRow::General(GeneralRow::Waves));
                rows.push(ContentRow::General(GeneralRow::AppearanceHeader));
                rows.extend(
                    AppearanceTarget::display_order()
                        .into_iter()
                        .map(ContentRow::Appearance),
                );
                rows
            }
            Section::Keybinds => {
                let mut rows: Vec<ContentRow> = vec![ContentRow::KeyTableHeader];
                let mut last_section = None;
                for item in remap_keys::build_remap_rows(ctx) {
                    if last_section != Some(item.section) {
                        rows.push(ContentRow::KeyHeader(item.section));
                        last_section = Some(item.section);
                    }
                    rows.push(ContentRow::KeyItem(item));
                }
                rows
            }
            Section::Mpv => {
                vec![
                    ContentRow::Mpv(MpvRow::Header), ContentRow::Mpv(MpvRow::AudioLang),
                    ContentRow::Mpv(MpvRow::Subtitles), ContentRow::Mpv(MpvRow::Svp),
                ]
            }
            Section::Mpd => {
                vec![
                    ContentRow::Mpd(MpdRow::LibraryHeader),
                    ContentRow::Mpd(MpdRow::LibraryPath),
                    ContentRow::Mpd(MpdRow::Update), ContentRow::Mpd(MpdRow::Rescan),
                    ContentRow::Mpd(MpdRow::PlaybackHeader),
                    ContentRow::Mpd(MpdRow::Crossfade), ContentRow::Mpd(MpdRow::Repeat),
                    ContentRow::Mpd(MpdRow::Random), ContentRow::Mpd(MpdRow::Single),
                    ContentRow::Mpd(MpdRow::Consume),
                    ContentRow::Mpd(MpdRow::DevicesHeader),
                    ContentRow::Mpd(MpdRow::Outputs),
                ]
            }
            Section::Jellyfin => {
                vec![
                    ContentRow::Jellyfin(JellyfinRow::Header),
                    ContentRow::Jellyfin(JellyfinRow::ServerUrl),
                    ContentRow::Jellyfin(JellyfinRow::Username),
                    ContentRow::Jellyfin(JellyfinRow::Password),
                    ContentRow::Jellyfin(JellyfinRow::SignIn),
                ]
            }
            Section::Torrent => {
                vec![
                    ContentRow::Torrent(TorrentRow::Header),
                    ContentRow::Torrent(TorrentRow::WebUi),
                    ContentRow::Torrent(TorrentRow::StopEngine),
                    ContentRow::Torrent(TorrentRow::SocksProxy),
                ]
            }
        }
    }
    /// Refresh the rows for the current section (the PipeWire device rows
    /// are always part of the cava block — round 30).
    fn refresh_rows(&mut self, ctx: &Ctx) {
        self.rows = self.build_rows(ctx);
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }
    fn source(&self) -> String {
        self.cava_pending.source.clone()
    }
    fn fps(&self) -> u16 {
        self.cava_pending.framerate
    }
    fn freq_min(&self) -> u16 {
        self.cava_pending.lower_cutoff_freq
    }
    fn freq_max(&self) -> u32 {
        self.cava_pending.higher_cutoff_freq
    }
    fn channels(&self) -> u32 {
        self.cava_pending.channels
    }
    fn noise_reduction(&self) -> u8 {
        self.cava_pending.noise_reduction
    }
    fn sensitivity(&self) -> u16 {
        self.cava_pending.sensitivity
    }
    fn monstercat(&self) -> bool {
        self.cava_pending.monstercat
    }
    fn waves(&self) -> bool {
        self.cava_pending.waves
    }
    /// The nodes currently offered to the device row. The "show virtual
    /// devices" toggle gates every virtual PipeWire node — the KDE
    /// split-sink monitors (`Media.monitor` etc.) and Easy Effects (sink
    /// monitor + processing source) — so off offers only real hardware
    /// capture points (plus "auto"); on offers everything. Non-virtual
    /// nodes are always offered.
    fn visible_nodes(&self) -> Vec<PwNode> {
        self.nodes
            .iter()
            .filter(|n| self.ui_pending.show_virtual_devices || !n.is_virtual)
            .cloned()
            .collect()
    }
    /// Stage a cava change: the live config and the sidecar are untouched
    /// until the panel is closed with Save.
    fn set_cava(
        &mut self,
        ctx: &mut Ctx,
        f: impl FnOnce(&mut StagedCava),
    ) -> Result<()> {
        f(&mut self.cava_pending);
        self.refresh_nodes();
        self.refresh_rows(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Stage a show/hide toggle: the live UI is untouched until the panel is
    /// closed with Save.
    fn toggle_ui(&mut self, ctx: &mut Ctx, row: GeneralRow, value: bool) -> Result<()> {
        match row {
            GeneralRow::AlbumArt => self.ui_pending.show_album_art = value,
            GeneralRow::Lyrics => self.ui_pending.show_lyrics = value,
            GeneralRow::Cava => self.ui_pending.show_cava = value,
            GeneralRow::Radio => self.ui_pending.show_radio_tab = value,
            GeneralRow::Jellyfin => self.ui_pending.show_jellyfin_tab = value,
            GeneralRow::AutoChapters => self.ui_pending.auto_show_chapters = value,
            GeneralRow::Mpdris2Notifications => {
                self.ui_pending.mpdris2_notifications = value;
            }
            _ => unreachable!("toggle_ui called with a non-toggle row"),
        }
        ctx.render()?;
        Ok(())
    }
    /// Step the selected row by `delta` (-1 or +1, scaled per row).
    fn adjust(&mut self, ctx: &mut Ctx, delta: i64) -> Result<()> {
        match &self.rows[self.selected] {
            ContentRow::General(g) => {
                match g {
                    GeneralRow::VideoPlayback => {
                        let all = crate::config::video::VideoPlaybackMode::ALL;
                        let idx = all
                            .iter()
                            .position(|m| *m == self.video_pending)
                            .unwrap_or(0) as i64;
                        self.video_pending = all[(idx + delta)
                            .rem_euclid(all.len() as i64) as usize];
                        ctx.render()?;
                        Ok(())
                    }
                    GeneralRow::Fps => {
                        let v = (i64::from(self.fps()) + delta * 5)
                            .clamp(i64::from(FPS_MIN), i64::from(FPS_MAX)) as u16;
                        self.set_cava(ctx, |c| c.framerate = v)
                    }
                    GeneralRow::Sensitivity => {
                        let v = (i64::from(self.sensitivity()) + delta * 5).clamp(1, 500)
                            as u16;
                        self.set_cava(ctx, |c| c.sensitivity = v)
                    }
                    GeneralRow::FreqMin => {
                        let max = i64::from(self.freq_max());
                        let v = (i64::from(self.freq_min()) + delta * 100)
                            .clamp(FREQ_MIN_LIMIT, FREQ_MAX_LIMIT)
                            .min(max) as u16;
                        self.set_cava(ctx, |c| c.lower_cutoff_freq = v)
                    }
                    GeneralRow::FreqMax => {
                        let min = i64::from(self.freq_min());
                        let v = (i64::from(self.freq_max()) + delta * 100)
                            .clamp(FREQ_MIN_LIMIT, FREQ_MAX_LIMIT)
                            .max(min) as u32;
                        self.set_cava(ctx, |c| c.higher_cutoff_freq = v)
                    }
                    GeneralRow::Channels => {
                        let v = if self.channels() == 2 { 1 } else { 2 };
                        self.set_cava(ctx, |c| c.channels = v)
                    }
                    GeneralRow::Device => self.cycle_device(ctx, delta),
                    GeneralRow::VirtualDevices => {
                        self.ui_pending.show_virtual_devices = !self
                            .ui_pending
                            .show_virtual_devices;
                        ctx.render()?;
                        Ok(())
                    }
                    GeneralRow::NoiseReduction => {
                        let v = (i64::from(self.noise_reduction()) + delta * 5)
                            .clamp(0, 100) as u8;
                        self.set_cava(ctx, |c| c.noise_reduction = v)
                    }
                    GeneralRow::Monstercat => {
                        let v = !self.monstercat();
                        self.set_cava(ctx, |c| c.monstercat = v)
                    }
                    GeneralRow::Waves => {
                        let v = !self.waves();
                        self.set_cava(ctx, |c| c.waves = v)
                    }
                    _ => Ok(()),
                }
            }
            ContentRow::Mpv(row) => {
                match row {
                    MpvRow::AudioLang => {
                        self.mpv_audio_pending = match &self.mpv_audio_pending {
                            crate::config::mpv::MpvAudioLang::System => {
                                crate::config::mpv::MpvAudioLang::Custom {
                                    lang: self.mpv_custom_lang.clone(),
                                }
                            }
                            crate::config::mpv::MpvAudioLang::Custom { .. } => {
                                crate::config::mpv::MpvAudioLang::System
                            }
                        };
                        ctx.render()?;
                        Ok(())
                    }
                    MpvRow::Subtitles => {
                        use crate::config::mpv::MpvSubtitleMode::{
                            Custom, Hidden, SystemLanguage,
                        };
                        let idx = match &self.mpv_subtitles_pending {
                            Hidden => 0,
                            SystemLanguage => 1,
                            Custom { .. } => 2,
                        };
                        self.mpv_subtitles_pending = match (idx as i64 + delta)
                            .rem_euclid(3)
                        {
                            0 => Hidden,
                            1 => SystemLanguage,
                            _ => {
                                Custom {
                                    lang: self.mpv_custom_lang.clone(),
                                }
                            }
                        };
                        ctx.render()?;
                        Ok(())
                    }
                    MpvRow::Svp => {
                        self.mpv_svp_pending = !self.mpv_svp_pending;
                        ctx.render()?;
                        Ok(())
                    }
                    MpvRow::Header => Ok(()),
                }
            }
            ContentRow::Appearance(_) => Ok(()),
            ContentRow::Jellyfin(_) => Ok(()),
            ContentRow::Torrent(_) => Ok(()),
            ContentRow::Mpd(m) => {
                match m {
                    MpdRow::Crossfade => {
                        let v = (ctx.status.xfade.unwrap_or(0) as i64 + delta)
                            .clamp(0, 60) as u32;
                        ctx.command(move |client| {
                            client.crossfade(v)?;
                            Ok(())
                        });
                        ctx.render()?;
                        Ok(())
                    }
                    MpdRow::Repeat => {
                        let v = !ctx.status.repeat;
                        ctx.command(move |client| {
                            client.repeat(v)?;
                            Ok(())
                        });
                        ctx.render()?;
                        Ok(())
                    }
                    MpdRow::Random => {
                        let v = !ctx.status.random;
                        ctx.command(move |client| {
                            client.random(v)?;
                            Ok(())
                        });
                        ctx.render()?;
                        Ok(())
                    }
                    MpdRow::Single => Self::cycle_single_consume(ctx, true),
                    MpdRow::Consume => Self::cycle_single_consume(ctx, false),
                    _ => Ok(()),
                }
            }
            ContentRow::KeyTableHeader
            | ContentRow::KeyHeader(_)
            | ContentRow::KeyItem(_) => Ok(()),
        }
    }
    fn cycle_single_consume(ctx: &mut Ctx, single: bool) -> Result<()> {
        if single {
            let mode = ctx.status.single;
            ctx.command(move |client| {
                client.single(mode.cycle_single())?;
                Ok(())
            });
        } else {
            let mode = ctx.status.consume;
            ctx.command(move |client| {
                client.consume(mode.cycle())?;
                Ok(())
            });
        }
        ctx.render()?;
        Ok(())
    }
    /// Cycle the PipeWire monitor node by `delta` (-1 or +1).
    fn cycle_device(&mut self, ctx: &mut Ctx, delta: i64) -> Result<()> {
        let nodes = self.visible_nodes();
        if nodes.is_empty() {
            return Ok(());
        }
        let current = self.source();
        let idx = nodes.iter().position(|n| n.name == current).unwrap_or(0);
        let next = (idx as i64 + delta).rem_euclid(nodes.len() as i64) as usize;
        let node = nodes[next].name.clone();
        self.set_cava(ctx, |c| c.source = node)
    }
    /// The color currently displayed for an appearance row: a staged value
    /// wins over the live theme value (`None` = transparent).
    fn displayed_color(&self, ctx: &Ctx, target: AppearanceTarget) -> Option<Color> {
        match self.appearance_pending[target as usize] {
            StagedColor::Unchanged => appearance_color(&ctx.config.theme, target),
            StagedColor::Transparent => None,
            StagedColor::Set(color) => Some(color),
        }
    }
    /// Begin typing a Jellyfin text field (server URL / username /
    /// password); committed with Enter, cancelled with Esc.
    fn start_edit_jellyfin(&mut self, row: JellyfinRow) {
        let current = match row {
            JellyfinRow::ServerUrl => self.jellyfin_url.clone(),
            JellyfinRow::Username => self.jellyfin_username.clone(),
            JellyfinRow::Password => self.jellyfin_password.clone(),
            _ => String::new(),
        };
        self.edit_buffer = current;
        self.editing_jellyfin_row = Some(row);
    }
    fn commit_edit_jellyfin(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some(row) = self.editing_jellyfin_row else { return Ok(()) };
        let value = std::mem::take(&mut self.edit_buffer);
        let label = match row {
            JellyfinRow::ServerUrl => {
                self.jellyfin_url = value.trim().to_owned();
                "server url"
            }
            JellyfinRow::Username => {
                self.jellyfin_username = value.trim().to_owned();
                "username"
            }
            JellyfinRow::Password => {
                self.jellyfin_password = value;
                "password"
            }
            _ => return Ok(()),
        };
        self.editing_jellyfin_row = None;
        status_info!("{label} staged — sign in, then save when leaving settings");
        ctx.render()?;
        Ok(())
    }
    fn start_edit_color(&mut self, target: AppearanceTarget, ctx: &mut Ctx) {
        let _ = self.displayed_color(ctx, target);
        self.edit_buffer.clear();
        self.editing_color = Some(target);
        ctx.render().ok();
    }
    /// Commit the edit buffer: validate, stage the color and leave edit mode.
    /// `transparent` / `none` / `0` clears the color (e.g. a transparent
    /// background).
    fn commit_edit_color(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some(target) = self.editing_color else { return Ok(()) };
        let value = self.edit_buffer.trim().to_ascii_lowercase();
        let staged = if matches!(value.as_str(), "transparent" | "none" | "0" | "") {
            Some(StagedColor::Transparent)
        } else {
            parse_hex_color(&value).map(StagedColor::Set)
        };
        match staged {
            Some(staged) => {
                self.appearance_pending[target as usize] = staged;
                self.editing_color = None;
                status_info!("{} staged — save when leaving settings", target.name());
                ctx.render()?;
            }
            None => {
                status_warn!(
                    "Invalid color '{}' (hex like 8f8f8f, or transparent)", self
                    .edit_buffer
                );
                ctx.render()?;
            }
        }
        Ok(())
    }
    /// w/s: move the sidebar highlight (no pane population until `d`).
    fn move_sidebar(&mut self, dir: i64, ctx: &mut Ctx) {
        let n = Section::all().len();
        self.sidebar_selected = (self.sidebar_selected as i64 + dir).rem_euclid(n as i64)
            as usize;
        ctx.render().ok();
    }
    /// d / sidebar click: populate the right pane with the highlighted
    /// section.
    fn populate(&mut self, ctx: &mut Ctx) {
        self.section = Section::all()[self.sidebar_selected];
        self.selected = 0;
        self.scroll = 0;
        self.refresh_rows(ctx);
        ctx.render().ok();
    }
    /// ↑/↓: move the content highlight; stops at the section boundaries (the
    /// section is chosen with `d`, not by scrolling).
    fn move_selection(&mut self, dir: i64, ctx: &mut Ctx) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len();
        let mut pos = self.selected;
        loop {
            let next = pos as i64 + dir;
            if next < 0 || next >= len as i64 {
                break;
            }
            pos = next as usize;
            if !self.rows[pos].is_header() {
                break;
            }
        }
        if pos != self.selected && !self.rows[pos].is_header() {
            self.selected = pos;
            let visible = self.visible_rows.max(1);
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected >= self.scroll + visible {
                self.scroll = self.selected + 1 - visible;
            }
        }
        ctx.render().ok();
    }
    /// The [-] [+] stepper rows whose controls can be focused with
    /// Space/Enter (a/← and d/→ then adjust, Esc cancels). The cava
    /// device row joins them: Enter (or d/→) focuses its [<] [>] cycle
    /// controls, then a/← and d/→ walk the PipeWire capture list.
    fn is_stepper_row(&self, idx: usize) -> bool {
        matches!(
            self.rows.get(idx), Some(ContentRow::General(GeneralRow::Sensitivity |
            GeneralRow::Fps | GeneralRow::FreqMin | GeneralRow::FreqMax |
            GeneralRow::NoiseReduction | GeneralRow::Device)) |
            Some(ContentRow::Mpd(MpdRow::Crossfade))
        )
    }
    /// Enter adjust mode on the selected stepper row: focus its controls and
    /// snapshot the staged values so Esc can cancel the whole session.
    fn start_adjust(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.adjust_snapshot = Some(AdjustSnapshot {
            cava: self.cava_pending.clone(),
            crossfade: ctx.status.xfade.unwrap_or(0),
        });
        self.adjusting = Some(self.selected);
        ctx.render()?;
        Ok(())
    }
    /// Leave adjust mode, keeping the adjusted value (Space/Enter).
    fn commit_adjust(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.adjusting = None;
        self.adjust_snapshot = None;
        ctx.render()?;
        Ok(())
    }
    /// Leave adjust mode, reverting the staged value to the snapshot (Esc).
    fn cancel_adjust(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some(snapshot) = self.adjust_snapshot.take() else { return Ok(()) };
        self.cava_pending = snapshot.cava;
        ctx.command(move |client| {
            client.crossfade(snapshot.crossfade)?;
            Ok(())
        });
        self.adjusting = None;
        self.refresh_rows(ctx);
        ctx.render()?;
        Ok(())
    }
    fn activate(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some(row) = self.rows.get(self.selected).cloned() else { return Ok(()) };
        match &row {
            ContentRow::General(g) => {
                match g {
                    GeneralRow::AlbumArt
                    | GeneralRow::Lyrics
                    | GeneralRow::Cava
                    | GeneralRow::Radio
                    | GeneralRow::Jellyfin
                    | GeneralRow::AutoChapters
                    | GeneralRow::Mpdris2Notifications => {
                        let value = match g {
                            GeneralRow::AlbumArt => !self.ui_pending.show_album_art,
                            GeneralRow::Lyrics => !self.ui_pending.show_lyrics,
                            GeneralRow::Cava => !self.ui_pending.show_cava,
                            GeneralRow::Radio => !self.ui_pending.show_radio_tab,
                            GeneralRow::Jellyfin => !self.ui_pending.show_jellyfin_tab,
                            GeneralRow::AutoChapters => {
                                !self.ui_pending.auto_show_chapters
                            }
                            GeneralRow::Mpdris2Notifications => {
                                !self.ui_pending.mpdris2_notifications
                            }
                            _ => unreachable!(),
                        };
                        self.toggle_ui(ctx, *g, value)
                    }
                    GeneralRow::Fps
                    | GeneralRow::Sensitivity
                    | GeneralRow::FreqMin
                    | GeneralRow::FreqMax
                    | GeneralRow::Channels
                    | GeneralRow::Device
                    | GeneralRow::VirtualDevices
                    | GeneralRow::NoiseReduction
                    | GeneralRow::Monstercat
                    | GeneralRow::Waves => self.adjust(ctx, 1),
                    GeneralRow::VideoPlayback => self.adjust(ctx, 1),
                    GeneralRow::AutoSens => {
                        let v = !self.cava_pending.autosens;
                        self.set_cava(ctx, |c| c.autosens = v)
                    }
                    GeneralRow::RadioReload => {
                        let location = ctx.config.radio.location.clone();
                        let cache_dir = ctx.config.cache_dir.clone();
                        let _ = ctx
                            .work_sender
                            .send(crate::shared::events::WorkRequest::FetchRadioDirectory {
                                location,
                                cache_dir,
                            })
                            .map_err(|err| {
                                log::error!(
                                    error:? = err; "Failed to request radio directory"
                                )
                            });
                        status_warn!("Reloading radio stations…");
                        ctx.render()?;
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            ContentRow::Mpv(row) => {
                match row {
                    MpvRow::AudioLang if matches!(
                        self.mpv_audio_pending, crate ::config::mpv::MpvAudioLang::Custom
                        { .. }
                    ) => {
                        Self::open_language_picker(ctx, MpvLanguageTarget::Audio);
                        Ok(())
                    }
                    MpvRow::Subtitles if matches!(
                        self.mpv_subtitles_pending, crate
                        ::config::mpv::MpvSubtitleMode::Custom { .. }
                    ) => {
                        Self::open_language_picker(ctx, MpvLanguageTarget::Subtitles);
                        Ok(())
                    }
                    _ => self.adjust(ctx, 1),
                }
            }
            ContentRow::Appearance(target) => {
                self.start_edit_color(*target, ctx);
                Ok(())
            }
            ContentRow::KeyItem(_) => {
                self.capturing = true;
                ctx.render()?;
                Ok(())
            }
            ContentRow::Jellyfin(m) => {
                match m {
                    JellyfinRow::ServerUrl
                    | JellyfinRow::Username
                    | JellyfinRow::Password => {
                        self.start_edit_jellyfin(*m);
                        ctx.render()?;
                        Ok(())
                    }
                    JellyfinRow::SignIn => {
                        let url = self
                            .jellyfin_url
                            .trim()
                            .trim_end_matches('/')
                            .to_owned();
                        let username = self.jellyfin_username.trim().to_owned();
                        let password = self.jellyfin_password.clone();
                        if url.is_empty() || username.is_empty() || password.is_empty() {
                            status_warn!(
                                "Fill in the server URL, username and password first"
                            );
                            return Ok(());
                        }
                        match crate::jellyfin::Jellyfin::authenticate(
                            &url,
                            &username,
                            &password,
                        ) {
                            Ok((token, user_id)) => {
                                self.jellyfin_credentials = Some(crate::config::jellyfin::JellyfinCredentialsFile {
                                    server_url: url,
                                    access_token: token,
                                    user_id,
                                });
                                status_info!("Signed in to Jellyfin as '{username}'");
                            }
                            Err(err) => {
                                status_error!("Jellyfin login failed: {err}");
                            }
                        }
                        ctx.render()?;
                        Ok(())
                    }
                    JellyfinRow::Header => Ok(()),
                }
            }
            ContentRow::Torrent(m) => {
                match m {
                    TorrentRow::WebUi => {
                        let needs_start = match ctx
                            .torrent_webui_engine
                            .borrow_mut()
                            .as_mut()
                        {
                            Some(engine) => !engine.is_running(),
                            None => true,
                        };
                        if needs_start {
                            if let Some(reg) = crate::core::rqctl::registered_running() {
                                Self::open_url(&reg.web_url);
                                status_info!("rqbit web UI opened at {}", reg.web_url);
                                ctx.render()?;
                                return Ok(());
                            }
                            let config = ctx.config.torrent.clone();
                            match crate::core::torrent::start_engine(&config) {
                                Ok(engine) => {
                                    if let Err(err) = crate::core::rqctl::register(&engine) {
                                        log::warn!(
                                            error:? = err; "Failed to register the rqbit engine"
                                        );
                                    }
                                    *ctx.torrent_webui_engine.borrow_mut() = Some(engine);
                                }
                                Err(err) => {
                                    status_error!("rqbit web UI failed to start: {err}");
                                    ctx.render()?;
                                    return Ok(());
                                }
                            }
                        }
                        let url = ctx
                            .torrent_webui_engine
                            .borrow()
                            .as_ref()
                            .expect("engine started just above")
                            .web_url();
                        Self::open_url(&url);
                        status_info!("rqbit web UI opened at {url}");
                        ctx.render()?;
                        Ok(())
                    }
                    TorrentRow::StopEngine => {
                        let stopped = ctx
                            .torrent_webui_engine
                            .borrow_mut()
                            .take()
                            .is_some();
                        let stopped_registered = crate::core::rqctl::stop_registered()
                            .unwrap_or(false);
                        if stopped || stopped_registered {
                            status_info!("rqbit engine stopped");
                        } else {
                            status_info!("No rqbit engine is running");
                        }
                        ctx.render()?;
                        Ok(())
                    }
                    TorrentRow::SocksProxy => {
                        let current = self.torrent_socks_proxy_pending.clone();
                        modal!(
                            ctx, InputModal::new(ctx)
                            .title("rqbit SOCKS5 proxy (VPN route)")
                            .input_label("socks5://[user:pass@]host:port")
                            .initial_value(current).on_confirm(| ctx, value | { * ctx
                            .torrent_socks_proxy_input.borrow_mut() = Some(value
                            .to_string()); Ok(()) })
                        );
                        Ok(())
                    }
                    TorrentRow::Header => Ok(()),
                }
            }
            ContentRow::Mpd(m) => {
                match m {
                    MpdRow::LibraryPath => {
                        let current = self.library_path.clone();
                        match pick_directory(&current) {
                            Ok(Some(path)) => {
                                let mut state = AppStateFile::load();
                                state.last_tab = ctx
                                    .config
                                    .tabs
                                    .names
                                    .first()
                                    .map(|t| t.to_string());
                                state.mpd_library_path = Some(path.clone());
                                if let Err(err) = state.save() {
                                    status_warn!("Failed to save state: {err}");
                                }
                                self.library_path = path;
                                ctx.render()?;
                            }
                            Ok(None) => {}
                            Err(()) => {
                                modal!(
                                    ctx, InputModal::new(ctx).title("MPD library path")
                                    .input_label("path").initial_value(current).on_confirm(|
                                    ctx, value | { let mut state = AppStateFile::load(); state
                                    .last_tab = ctx.config.tabs.names.first().map(| t | t
                                    .to_string()); state.mpd_library_path = Some(value
                                    .to_string()); if let Err(err) = state.save() {
                                    status_warn!("Failed to save state: {err}"); } ctx.render()
                                    ?; Ok(()) })
                                );
                            }
                        }
                        Ok(())
                    }
                    MpdRow::Update | MpdRow::Rescan => {
                        let rescan = matches!(m, MpdRow::Rescan);
                        let scope = if self.library_path.is_empty() {
                            None
                        } else {
                            Some(self.library_path.clone())
                        };
                        ctx.command(move |client| {
                            if rescan {
                                client.rescan(scope.as_deref())?;
                            } else {
                                client.update(scope.as_deref())?;
                            }
                            Ok(())
                        });
                        status_warn!(
                            "{}", if rescan { "Rescanning library…" } else {
                            "Updating library…" }
                        );
                        ctx.render()?;
                        Ok(())
                    }
                    MpdRow::Outputs => {
                        let current_partition = ctx.status.partition.clone();
                        ctx.query()
                            .id(OPEN_OUTPUTS_MODAL)
                            .replace_id(OPEN_OUTPUTS_MODAL)
                            .query(move |client| {
                                let outputs = client
                                    .list_partitioned_outputs(&current_partition)?;
                                Ok(crate::MpdQueryResult::Outputs(outputs))
                            });
                        Ok(())
                    }
                    MpdRow::Crossfade
                    | MpdRow::Repeat
                    | MpdRow::Random
                    | MpdRow::Single
                    | MpdRow::Consume => self.adjust(ctx, 1),
                    _ => Ok(()),
                }
            }
            ContentRow::KeyTableHeader | ContentRow::KeyHeader(_) => Ok(()),
        }
    }
    /// Ask whether to apply the staged settings or drop them when the panel
    /// is closed with unsaved edits.
    fn prompt_save_or_discard(&self, ctx: &mut Ctx) -> Result<()> {
        let settings_id = self.id;
        let staged = self.staged_settings();
        let keybinds_snapshot = self.keybinds_snapshot.clone();
        modal!(
            ctx, ConfirmModal::builder().ctx(ctx)
            .message(vec!["You have unsaved settings changes.",
            "Save them, or discard them?",]).action(Action::CustomButtons { buttons :
            vec![("Save", Box::new(move | ctx | { ctx.app_event_sender
            .send(AppEvent::UiEvent(UiAppEvent::ApplySettings(staged))) ?; ctx
            .app_event_sender.send(AppEvent::UiEvent(UiAppEvent::PopModal(settings_id)))
            ?; Ok(()) }),), ("Discard", Box::new(move | ctx | { ctx.app_event_sender
            .send(AppEvent::UiEvent(UiAppEvent::DiscardSettings { keybinds :
            keybinds_snapshot },)) ?; ctx.app_event_sender
            .send(AppEvent::UiEvent(UiAppEvent::PopModal(settings_id))) ?; Ok(()) }),),],
            }).size((52, 8)).build()
        );
        Ok(())
    }
    /// True when anything editable was changed while the panel was open.
    fn has_changes(&self) -> bool {
        self.ui_pending != self.ui_initial || self.video_pending != self.video_initial
            || self.mpv_audio_pending != self.mpv_audio_initial
            || self.mpv_subtitles_pending != self.mpv_subtitles_initial
            || self.mpv_svp_pending != self.mpv_svp_initial
            || self.cava_pending != self.cava_initial
            || self
                .appearance_pending
                .iter()
                .any(|c| !matches!(c, StagedColor::Unchanged))
            || !self.pending_remaps.is_empty() || self.jellyfin_credentials.is_some()
            || self.torrent_socks_proxy_pending != self.torrent_socks_proxy_initial
    }
    /// The staged changes, handed to the UI to apply on Save.
    fn staged_settings(&self) -> StagedSettings {
        StagedSettings {
            ui: self.ui_pending,
            cava: self.cava_pending.to_overrides(),
            appearance: self.appearance_pending,
            remaps: self.pending_remaps.clone(),
            video_playback: self.video_pending,
            mpv_audio_lang: self.mpv_audio_pending.clone(),
            mpv_subtitles: self.mpv_subtitles_pending.clone(),
            mpv_svp: self.mpv_svp_pending,
            jellyfin: self.jellyfin_credentials.clone(),
            torrent_socks_proxy: self.torrent_socks_proxy_pending.clone(),
        }
    }
    fn do_click(&mut self, click: Click, ctx: &mut Ctx) -> Result<()> {
        match click {
            Click::Toggle | Click::Activate | Click::Edit => self.activate(ctx),
            Click::Inc | Click::Next => self.adjust(ctx, 1),
            Click::Dec | Click::Prev => self.adjust(ctx, -1),
        }
    }
    fn styles(&self, ctx: &Ctx) -> (Style, Style, Style) {
        let base = ctx
            .config
            .theme
            .text_color
            .map_or_else(Style::default, |c| Style::default().fg(c));
        let dim = base.add_modifier(Modifier::DIM);
        let active = ctx.config.theme.current_item_style;
        (base, dim, active)
    }
    /// The row-wide click target covering the whole row.
    fn row_click(
        targets: &mut Vec<(Rect, Click)>,
        x: u16,
        y: u16,
        width: u16,
        click: Click,
    ) {
        targets.push((Rect { x, y, width, height: 1 }, click));
    }
    /// Render a row's label + control parts. The control is right-aligned
    /// by the caller; buttons inside it are deferred (their click rects are
    /// placed once the control's x is known). The fourth element is the
    /// row-wide click action (covering the whole row, including the label).
    #[allow(clippy::too_many_arguments)]
    fn row_main(
        &self,
        row: &ContentRow,
        ctx: &Ctx,
        style: Style,
        dim: Style,
        avail: usize,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<DeferredButton>, Option<Click>) {
        match row {
            ContentRow::Mpv(m) => {
                match m {
                    MpvRow::Header => {
                        (
                            vec![Span::styled("[ mpv ]", dim)],
                            Vec::new(),
                            Vec::new(),
                            None,
                        )
                    }
                    MpvRow::AudioLang => {
                        let active_label = self.mpv_audio_pending.label();
                        (
                            vec![Span::styled(" audio", style)],
                            vec![
                                Span::styled(format!(" {active_label} "), style.bold()),
                                Span::styled(" > ", dim), Span::styled(" original ", dim),
                            ],
                            Vec::new(),
                            Some(Click::Toggle),
                        )
                    }
                    MpvRow::Subtitles => {
                        let second = self.mpv_subtitles_pending.label();
                        (
                            vec![Span::styled(" subtitles", style)],
                            vec![
                                Span::styled(" signs ", dim), Span::styled(" > ", dim),
                                Span::styled(format!(" {second} "), style.bold()),
                            ],
                            Vec::new(),
                            Some(Click::Toggle),
                        )
                    }
                    MpvRow::Svp => {
                        Self::toggle_row("svp support", self.mpv_svp_pending, style)
                    }
                }
            }
            ContentRow::General(g) => {
                match g {
                    GeneralRow::AlbumArt
                    | GeneralRow::Lyrics
                    | GeneralRow::Cava
                    | GeneralRow::Radio
                    | GeneralRow::Jellyfin
                    | GeneralRow::AutoChapters
                    | GeneralRow::Mpdris2Notifications
                    | GeneralRow::AutoSens
                    | GeneralRow::VirtualDevices
                    | GeneralRow::Monstercat
                    | GeneralRow::Waves => {
                        let (label, enabled) = match g {
                            GeneralRow::AlbumArt => {
                                ("show album art", self.ui_pending.show_album_art)
                            }
                            GeneralRow::Lyrics => {
                                ("show lyrics", self.ui_pending.show_lyrics)
                            }
                            GeneralRow::Cava => ("show cava", self.ui_pending.show_cava),
                            GeneralRow::Radio => {
                                ("show radio tab", self.ui_pending.show_radio_tab)
                            }
                            GeneralRow::Jellyfin => {
                                ("show jellyfin tab", self.ui_pending.show_jellyfin_tab)
                            }
                            GeneralRow::AutoChapters => {
                                (
                                    "If media contains chapters open to chapters list",
                                    self.ui_pending.auto_show_chapters,
                                )
                            }
                            GeneralRow::Mpdris2Notifications => {
                                (
                                    "mpdris2 desktop notifications",
                                    self.ui_pending.mpdris2_notifications,
                                )
                            }
                            GeneralRow::AutoSens => {
                                ("auto-sens", self.cava_pending.autosens)
                            }
                            GeneralRow::VirtualDevices => {
                                (
                                    "show virtual devices",
                                    self.ui_pending.show_virtual_devices,
                                )
                            }
                            GeneralRow::Monstercat => {
                                ("monstercat smoothing", self.monstercat())
                            }
                            GeneralRow::Waves => ("waves smoothing", self.waves()),
                            _ => unreachable!(),
                        };
                        Self::toggle_row(label, enabled, style)
                    }
                    GeneralRow::RadioReload => {
                        let control = vec![Span::styled("[Reload]", style.bold())];
                        let buttons = vec![
                            DeferredButton { offset : 0, label : "[Reload]", click :
                            Click::Activate, }
                        ];
                        (
                            vec![Span::styled(" reload radio stations", style)],
                            control,
                            buttons,
                            Some(Click::Activate),
                        )
                    }
                    GeneralRow::VideoPlayback => {
                        let current = self.video_pending;
                        let mut control = Vec::new();
                        for (i, mode) in crate::config::video::VideoPlaybackMode::ALL
                            .iter()
                            .enumerate()
                        {
                            if i > 0 {
                                control.push(Span::styled("|", dim));
                            }
                            let active = *mode == current;
                            control
                                .push(
                                    Span::styled(
                                        format!(" {} ", mode.as_str()),
                                        if active { style.bold() } else { dim },
                                    ),
                                );
                        }
                        (
                            vec![
                                Span::styled(" Jellyfin media playback preference", style)
                            ],
                            control,
                            Vec::new(),
                            Some(Click::Toggle),
                        )
                    }
                    GeneralRow::Sensitivity => {
                        self.stepper_row(
                            "sensitivity",
                            self.sensitivity().to_string(),
                            style,
                        )
                    }
                    GeneralRow::Fps => {
                        self.stepper_row(
                            "frame rate 15-120",
                            self.fps().to_string(),
                            style,
                        )
                    }
                    GeneralRow::FreqMin => {
                        self.stepper_row(
                            "min sampling frequency",
                            self.freq_min().to_string(),
                            style,
                        )
                    }
                    GeneralRow::FreqMax => {
                        self.stepper_row(
                            "max sampling frequency",
                            self.freq_max().to_string(),
                            style,
                        )
                    }
                    GeneralRow::Channels => {
                        let current = self.channels();
                        Self::option_row(
                            "channels",
                            style,
                            dim,
                            &[("1", current == 1), ("2", current == 2)],
                        )
                    }
                    GeneralRow::Device => {
                        let source = self.source();
                        let description = self
                            .nodes
                            .iter()
                            .find(|n| n.name == source)
                            .map(|n| n.description.clone())
                            .unwrap_or_else(|| {
                                if source == "auto" {
                                    "auto (default output)".to_string()
                                } else {
                                    source
                                }
                            });
                        let max_desc = avail
                            .saturating_sub(
                                1 + "device".len() + 2 + "[<] [>]".len() + 2,
                            );
                        let description = truncate_col(&description, max_desc);
                        let d = description.chars().count();
                        (
                            vec![Span::styled(" device", style)],
                            vec![
                                Span::styled(description, style.bold()), Span::raw("  "),
                                Span::styled("[<]", style.bold()), Span::raw(" "),
                                Span::styled("[>]", style.bold()),
                            ],
                            vec![
                                DeferredButton { offset : d + 2, label : "[<]", click :
                                Click::Prev }, DeferredButton { offset : d + 6, label :
                                "[>]", click : Click::Next },
                            ],
                            Some(Click::Next),
                        )
                    }
                    GeneralRow::NoiseReduction => {
                        self.stepper_row(
                            "noise reduction",
                            self.noise_reduction().to_string(),
                            style,
                        )
                    }
                    _ => (Vec::new(), Vec::new(), Vec::new(), None),
                }
            }
            ContentRow::Appearance(_) => (Vec::new(), Vec::new(), Vec::new(), None),
            ContentRow::Mpd(m) => {
                match m {
                    MpdRow::LibraryPath => {
                        let path = if self.library_path.is_empty() {
                            "(whole library)".to_string()
                        } else {
                            self.library_path.clone()
                        };
                        let max_path = avail
                            .saturating_sub(
                                1 + "library location".len() + 2 + "[edit]".len() + 2,
                            );
                        let path = truncate_col(&path, max_path);
                        let p = path.chars().count();
                        (
                            vec![Span::styled(" library location", style)],
                            vec![
                                Span::styled(path, style.bold()), Span::raw("  "),
                                Span::styled("[edit]", style.bold()),
                            ],
                            vec![
                                DeferredButton { offset : p + 2, label : "[edit]", click :
                                Click::Edit }
                            ],
                            Some(Click::Edit),
                        )
                    }
                    MpdRow::Update => {
                        Self::action_row(
                            "update library",
                            "scan for new / changed files",
                            style,
                            dim,
                        )
                    }
                    MpdRow::Rescan => {
                        Self::action_row(
                            "rescan library",
                            "force re-read of all files",
                            style,
                            dim,
                        )
                    }
                    MpdRow::Outputs => {
                        Self::action_row(
                            "outputs",
                            "list / toggle MPD outputs",
                            style,
                            dim,
                        )
                    }
                    MpdRow::Crossfade => {
                        self.stepper_row(
                            "crossfade",
                            ctx.status.xfade.unwrap_or(0).to_string(),
                            style,
                        )
                    }
                    MpdRow::Repeat
                    | MpdRow::Random
                    | MpdRow::Single
                    | MpdRow::Consume => {
                        let (label, enabled) = match m {
                            MpdRow::Repeat => ("repeat", ctx.status.repeat),
                            MpdRow::Random => ("random", ctx.status.random),
                            MpdRow::Single => {
                                ("single", !matches!(ctx.status.single, OnOffOneshot::Off))
                            }
                            MpdRow::Consume => {
                                (
                                    "consume",
                                    !matches!(ctx.status.consume, OnOffOneshot::Off),
                                )
                            }
                            _ => unreachable!(),
                        };
                        Self::toggle_row(label, enabled, style)
                    }
                    _ => (Vec::new(), Vec::new(), Vec::new(), None),
                }
            }
            ContentRow::Jellyfin(m) => {
                match m {
                    JellyfinRow::ServerUrl
                    | JellyfinRow::Username
                    | JellyfinRow::Password => {
                        let (label, value) = match m {
                            JellyfinRow::ServerUrl => {
                                ("server url", self.jellyfin_url.clone())
                            }
                            JellyfinRow::Username => {
                                ("username", self.jellyfin_username.clone())
                            }
                            JellyfinRow::Password => {
                                (
                                    "password",
                                    "•".repeat(self.jellyfin_password.chars().count().min(8)),
                                )
                            }
                            _ => unreachable!(),
                        };
                        let display = if self.editing_jellyfin_row == Some(*m) {
                            format!("{}_", self.edit_buffer)
                        } else if value.is_empty() {
                            "(not set)".to_string()
                        } else {
                            value
                        };
                        let max_value = avail
                            .saturating_sub(1 + label.len() + 2 + "[edit]".len() + 2);
                        let display = truncate_col(&display, max_value);
                        let d = display.chars().count();
                        (
                            vec![Span::styled(format!(" {label}"), style)],
                            vec![
                                Span::styled(display, style.bold()), Span::raw("  "),
                                Span::styled("[edit]", style.bold()),
                            ],
                            vec![
                                DeferredButton { offset : d + 2, label : "[edit]", click :
                                Click::Edit }
                            ],
                            Some(Click::Edit),
                        )
                    }
                    JellyfinRow::SignIn => {
                        let signed_in = self.jellyfin_credentials.is_some();
                        let label = if signed_in { "sign in ✓" } else { "sign in" };
                        (
                            vec![
                                Span::styled(format!(" {label}"), if signed_in { style
                                .bold() } else { style },)
                            ],
                            vec![Span::styled("fetch a session token for the app", dim)],
                            Vec::new(),
                            Some(Click::Activate),
                        )
                    }
                    _ => (Vec::new(), Vec::new(), Vec::new(), None),
                }
            }
            ContentRow::Torrent(m) => {
                match m {
                    TorrentRow::WebUi => {
                        let running = self.torrent_webui_running(ctx);
                        let desc = if running {
                            "engine running — open the browser"
                        } else {
                            "start the engine + open the browser"
                        };
                        let button = if running { "[open]" } else { "[start]" };
                        let d = desc.chars().count();
                        (
                            vec![Span::styled(" web ui", style)],
                            vec![
                                Span::styled(desc, dim), Span::raw("  "),
                                Span::styled(button, style.bold()),
                            ],
                            vec![
                                DeferredButton { offset : d + 2, label : button, click :
                                Click::Activate, }
                            ],
                            Some(Click::Activate),
                        )
                    }
                    TorrentRow::StopEngine => {
                        let running = self.torrent_webui_running(ctx);
                        let desc = if running {
                            "kill the standalone engine"
                        } else {
                            "no engine running"
                        };
                        let d = desc.chars().count();
                        (
                            vec![Span::styled(" stop engine", style)],
                            vec![
                                Span::styled(desc, dim), Span::raw("  "),
                                Span::styled("[stop]", style.bold()),
                            ],
                            vec![
                                DeferredButton { offset : d + 2, label : "[stop]", click :
                                Click::Activate, }
                            ],
                            Some(Click::Activate),
                        )
                    }
                    TorrentRow::SocksProxy => {
                        let value = self.torrent_socks_proxy_pending.clone();
                        let display = if value.is_empty() {
                            "(not set)".to_string()
                        } else {
                            value
                        };
                        let max_value = avail
                            .saturating_sub(
                                1 + "socks proxy".len() + 2 + "[edit]".len() + 2,
                            );
                        let display = truncate_col(&display, max_value);
                        let d = display.chars().count();
                        (
                            vec![Span::styled(" socks proxy", style)],
                            vec![
                                Span::styled(display, style.bold()), Span::raw("  "),
                                Span::styled("[edit]", style.bold()),
                            ],
                            vec![
                                DeferredButton { offset : d + 2, label : "[edit]", click :
                                Click::Edit }
                            ],
                            Some(Click::Edit),
                        )
                    }
                    _ => (Vec::new(), Vec::new(), Vec::new(), None),
                }
            }
            _ => (Vec::new(), Vec::new(), Vec::new(), None),
        }
    }
    /// A `[x]` / `[ ]` toggle row: label left, checkbox right-aligned.
    fn toggle_row(
        label: &'static str,
        enabled: bool,
        style: Style,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<DeferredButton>, Option<Click>) {
        (
            vec![Span::styled(format!(" {label}"), style)],
            vec![
                Span::styled(format!("[{}]", if enabled { "x" } else { " " }), style
                .bold())
            ],
            Vec::new(),
            Some(Click::Toggle),
        )
    }
    /// A `a | b | c` choice row: the current option is bold, the others dim.
    fn option_row(
        label: &str,
        style: Style,
        dim: Style,
        options: &[(&str, bool)],
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<DeferredButton>, Option<Click>) {
        let mut control = Vec::new();
        for (i, (text, active)) in options.iter().enumerate() {
            if i > 0 {
                control.push(Span::styled("|", dim));
            }
            control
                .push(
                    Span::styled(
                        format!(" {text} "),
                        if *active { style.bold() } else { dim },
                    ),
                );
        }
        (
            vec![Span::styled(format!(" {label}"), style)],
            control,
            Vec::new(),
            Some(Click::Toggle),
        )
    }
    /// A stepper row: label left, `value [-] [+]` right-aligned. The
    /// buttons are deferred so the row-wide click (Inc) can't shadow them.
    fn stepper_row(
        &self,
        label: &str,
        value: String,
        style: Style,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<DeferredButton>, Option<Click>) {
        let vw = value.chars().count();
        let ctl = if self.adjusting == Some(self.selected) {
            style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            style.add_modifier(Modifier::BOLD)
        };
        let control = vec![
            Span::styled(value, ctl), Span::raw(" "), Span::styled("[-]", ctl),
            Span::raw(" "), Span::styled("[+]", ctl),
        ];
        let buttons = vec![
            DeferredButton { offset : vw + 1, label : "[-]", click : Click::Dec },
            DeferredButton { offset : vw + 4, label : "[+]", click : Click::Inc },
        ];
        (
            vec![Span::styled(format!(" {label}"), style)],
            control,
            buttons,
            Some(Click::Inc),
        )
    }
    /// A plain action row: label left, a dim description right-aligned.
    fn action_row(
        label: &'static str,
        hint: &'static str,
        style: Style,
        dim: Style,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<DeferredButton>, Option<Click>) {
        (
            vec![Span::styled(format!(" {label}"), style)],
            vec![Span::styled(hint, dim)],
            Vec::new(),
            Some(Click::Activate),
        )
    }
    /// The description shown right-aligned on a section header row ("" =
    /// none).
    fn section_hint(&self, row: &ContentRow) -> String {
        match row {
            ContentRow::General(GeneralRow::FeaturesHeader) => {
                "enable or disable features globally".into()
            }
            ContentRow::General(GeneralRow::CavaHeader) => {
                "cava visualization settings".into()
            }
            ContentRow::General(GeneralRow::AppearanceHeader) => String::new(),
            ContentRow::Mpv(MpvRow::Header) => "mpv video playback".into(),
            ContentRow::Mpd(MpdRow::LibraryHeader) => "MPD database".into(),
            ContentRow::Mpd(MpdRow::PlaybackHeader) => "transport modes".into(),
            ContentRow::Mpd(MpdRow::DevicesHeader) => "outputs & sessions".into(),
            ContentRow::Jellyfin(JellyfinRow::Header) => "Jellyfin server".into(),
            ContentRow::Torrent(TorrentRow::Header) => "rqbit torrent engine".into(),
            _ => String::new(),
        }
    }
    fn render_row(
        &self,
        row: &ContentRow,
        selected: bool,
        hovered: bool,
        ctx: &Ctx,
        base: Style,
        dim: Style,
        active: Style,
        x: u16,
        y: u16,
        width: u16,
        targets: &mut Vec<(Rect, Click)>,
    ) -> Line<'static> {
        let style = if selected { active } else { base };
        let hovered = hovered && Self::is_clickable_row(row);
        let hover = |mut line: Line<'static>| -> Line<'static> {
            if hovered {
                crate::config::hover_line(&mut line);
            }
            line
        };
        match row {
            ContentRow::KeyTableHeader => {
                let d = self.desc_col_w.max(10);
                let a = self.action_col_w.max(6);
                let k = self.key_col_w.max(4);
                Line::from(
                    vec![
                        Span::styled(format!(" {:<d$}", "Description"), dim),
                        Span::styled(format!("{:<a$}", "Action"), dim),
                        Span::styled(format!("{:<k$}", "Key"), dim),
                    ],
                )
            }
            ContentRow::KeyHeader(section) => {
                Line::from(vec![Span::styled(format!(" {}", section.name()), dim)])
            }
            ContentRow::KeyItem(item) => {
                Self::row_click(targets, x, y, width, Click::Activate);
                let d = self.desc_col_w;
                let a = self.action_col_w;
                let k = self.key_col_w;
                let desc = truncate_col(
                    &remap_keys::remap_description(&item.action),
                    d.max(10),
                );
                let action = truncate_col(&item.display, a.max(6));
                let keys = truncate_col(&remap_keys::key_display(&item.keys), k.max(4));
                hover(
                    Line::from(
                        vec![
                            Span::styled(format!(" {desc:<d$}"), active),
                            Span::styled(format!("{action:<a$}"), style),
                            Span::styled(keys, dim),
                        ],
                    ),
                )
            }
            ContentRow::General(
                GeneralRow::FeaturesHeader
                | GeneralRow::CavaHeader
                | GeneralRow::AppearanceHeader,
            )
            | ContentRow::Mpv(MpvRow::Header)
            | ContentRow::Mpd(
                MpdRow::LibraryHeader | MpdRow::PlaybackHeader | MpdRow::DevicesHeader,
            )
            | ContentRow::Jellyfin(JellyfinRow::Header)
            | ContentRow::Torrent(TorrentRow::Header) => {
                let title = match row {
                    ContentRow::General(GeneralRow::FeaturesHeader) => "[ features ]",
                    ContentRow::General(GeneralRow::CavaHeader) => "[ cava ]",
                    ContentRow::General(GeneralRow::AppearanceHeader) => "[ appearance ]",
                    ContentRow::Mpv(MpvRow::Header) => "[ mpv ]",
                    ContentRow::Mpd(MpdRow::LibraryHeader) => "[ library ]",
                    ContentRow::Mpd(MpdRow::PlaybackHeader) => "[ playback ]",
                    ContentRow::Mpd(MpdRow::DevicesHeader) => "[ devices ]",
                    ContentRow::Jellyfin(JellyfinRow::Header) => "[ server ]",
                    ContentRow::Torrent(TorrentRow::Header) => "[ rqbit ]",
                    _ => unreachable!(),
                };
                let hint = self.section_hint(row);
                let available = (width as usize).saturating_sub(2);
                let hint_keep = available.saturating_sub(title.len()).saturating_sub(1);
                let hint = truncate_col(&hint, hint_keep);
                let pad = available
                    .saturating_sub(title.len())
                    .saturating_sub(hint.len());
                let mut spans = vec![Span::styled(title, dim)];
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                if !hint.is_empty() {
                    spans.push(Span::styled(hint, dim));
                }
                Line::from(spans)
            }
            ContentRow::Appearance(target) => {
                Self::row_click(targets, x, y, width, Click::Toggle);
                let color = self.displayed_color(ctx, *target);
                let is_editing = self.editing_color == Some(*target);
                let field = if is_editing {
                    format!("#{}_", self.edit_buffer)
                } else {
                    color.as_ref().map_or_else(|| "transparent".to_string(), color_hex)
                };
                let swatch = match color {
                    Some(color) => Span::styled("  ", Style::default().bg(color)),
                    None => Span::styled("▢▢", dim),
                };
                let field_s = format!("[{field}]");
                let label_w = 1 + target.name().len() + 2 + 2;
                let pad = (width as usize)
                    .saturating_sub(2)
                    .saturating_sub(label_w)
                    .saturating_sub(field_s.chars().count());
                let mut spans = vec![
                    Span::styled(format!(" {}", target.name()), style), Span::raw("  "),
                    swatch,
                ];
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                spans.push(Span::styled(field_s, style.bold()));
                hover(Line::from(spans))
            }
            ContentRow::General(_)
            | ContentRow::Mpd(_)
            | ContentRow::Mpv(_)
            | ContentRow::Jellyfin(_)
            | ContentRow::Torrent(_) => {
                let (label, control, buttons, row_action) = self
                    .row_main(row, ctx, style, dim, width as usize);
                if let Some(click) = row_action {
                    Self::row_click(targets, x, y, width, click);
                }
                let control_w: usize = control.iter().map(|s| s.width()).sum();
                let available = (width as usize).saturating_sub(2);
                let (label, label_w, pad) = {
                    let label_w: usize = label.iter().map(|s| s.width()).sum();
                    let pad = available.saturating_sub(label_w + control_w);
                    if pad > 0 {
                        (label, label_w, pad)
                    } else {
                        let keep = available.saturating_sub(control_w).saturating_sub(1);
                        let mut shrunk = Vec::new();
                        if let Some(span) = label.into_iter().next() {
                            let text = span.content.to_string();
                            shrunk
                                .push(Span::styled(truncate_col(&text, keep), span.style));
                        }
                        let shrunk_w: usize = shrunk.iter().map(|s| s.width()).sum();
                        (shrunk, shrunk_w, 1)
                    }
                };
                let control_x = x + label_w as u16 + pad as u16;
                for b in buttons {
                    targets
                        .push((
                            Rect {
                                x: control_x + b.offset as u16,
                                y,
                                width: b.label.chars().count() as u16,
                                height: 1,
                            },
                            b.click,
                        ));
                }
                let mut spans = label;
                spans.push(Span::raw(" ".repeat(pad)));
                spans.extend(control);
                if row_action.is_some() {
                    hover(Line::from(spans))
                } else {
                    Line::from(spans)
                }
            }
        }
    }
    /// Whether the standalone web-UI engine is running: the GUI's
    /// in-memory engine OR an engine the `s2udio rq` CLI registered (the
    /// two share one registration file).
    fn torrent_webui_running(&self, ctx: &Ctx) -> bool {
        ctx
            .torrent_webui_engine
            .borrow_mut()
            .as_mut()
            .map(|e| e.is_running())
            .unwrap_or(false) || crate::core::rqctl::registered_running().is_some()
    }
    /// Open `url` in the system browser (`xdg-open`); on failure the URL
    /// stays in the status bar so the user can paste it manually.
    fn open_url(url: &str) {
        use std::process::Stdio;
        match Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => {}
            Err(err) => {
                status_warn!("Could not open a browser (xdg-open): {err}");
            }
        }
    }
    /// Whether the row registers a row-wide click (only those lighten on
    /// hover).
    fn is_clickable_row(row: &ContentRow) -> bool {
        matches!(
            row, ContentRow::KeyItem(_) | ContentRow::Appearance(_) |
            ContentRow::General(_) | ContentRow::Mpd(_) | ContentRow::Mpv(_) |
            ContentRow::Jellyfin(_) | ContentRow::Torrent(_)
        )
    }
}
impl SettingsModal {
    /// Common subtitle languages offered by the custom-language picker.
    fn subtitle_language_options() -> &'static [(&'static str, &'static str)] {
        &[
            ("English", "en"),
            ("Spanish", "es"),
            ("French", "fr"),
            ("German", "de"),
            ("Italian", "it"),
            ("Portuguese", "pt"),
            ("Japanese", "ja"),
            ("Korean", "ko"),
            ("Chinese (Simplified)", "zh"),
            ("Russian", "ru"),
            ("Dutch", "nl"),
            ("Polish", "pl"),
            ("Turkish", "tr"),
            ("Swedish", "sv"),
            ("Arabic", "ar"),
            ("Hindi", "hi"),
        ]
    }
    /// Open the language picker (audio or subtitles); the chosen code is
    /// stored in `ctx.mpv_custom_audio_lang` / `ctx.mpv_custom_subtitle_lang`
    /// and applied on the next render.
    fn open_language_picker(ctx: &mut Ctx, target: MpvLanguageTarget) {
        use crate::ui::modals::select_modal::SelectModal;
        let options: Vec<String> = Self::subtitle_language_options()
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect();
        modal!(
            ctx, SelectModal::builder().ctx(ctx).options(options).confirm_label("Select")
            .title(match target { MpvLanguageTarget::Audio => "Audio language",
            MpvLanguageTarget::Subtitles => "Subtitle language", }).on_confirm(move |
            ctx, selected, _idx | { let lang = Self::subtitle_language_options().iter()
            .find(| (name, _) | ** name == selected).map(| (_, code) | (* code)
            .to_owned()).unwrap_or_else(|| selected); match target {
            MpvLanguageTarget::Audio => { * ctx.mpv_custom_audio_lang.borrow_mut() =
            Some(lang); } MpvLanguageTarget::Subtitles => { * ctx
            .mpv_custom_subtitle_lang.borrow_mut() = Some(lang); } } Ok(()) }).build()
        );
    }
}
/// The language names the settings + the controls' audio/subtitle buttons
/// offer (name, ISO 639-1 code).
pub(crate) fn language_options() -> &'static [(&'static str, &'static str)] {
    SettingsModal::subtitle_language_options()
}
impl Modal for SettingsModal {
    fn id(&self) -> Id {
        self.id
    }
    /// Right-click is routed through handle_mouse_event so staged changes
    /// get the save/discard prompt (the generic close would drop them).
    fn right_click_closes(&self) -> bool {
        false
    }
    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        if let Some(lang) = ctx.mpv_custom_subtitle_lang.borrow_mut().take() {
            self.mpv_subtitles_pending = crate::config::mpv::MpvSubtitleMode::Custom {
                lang: lang.clone(),
            };
            self.mpv_custom_lang = lang;
        }
        if let Some(lang) = ctx.mpv_custom_audio_lang.borrow_mut().take() {
            self.mpv_audio_pending = crate::config::mpv::MpvAudioLang::Custom {
                lang: lang.clone(),
            };
            self.mpv_custom_lang = lang;
        }
        if let Some(proxy) = ctx.torrent_socks_proxy_input.borrow_mut().take() {
            self.torrent_socks_proxy_pending = proxy;
        }
        self.refresh_rows(ctx);
        self.library_path = AppStateFile::load().mpd_library_path.unwrap_or_default();
        let popup_area = frame.area();
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame
                .render_widget(
                    Block::default().style(Style::default().bg(bg_color)),
                    popup_area,
                );
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(symbols::border::ROUNDED)
            .border_style(ctx.config.as_border_style())
            .title_alignment(ratatui::prelude::Alignment::Center)
            .title(
                if self.capturing {
                    " Settings — keybinds: press a new key (Esc cancel) ".bold()
                } else if let Some(target) = self.editing_color {
                    format!(
                        " Settings — {}: type a hex color (Enter save, Esc cancel) ",
                        target.name()
                    )
                        .bold()
                } else {
                    " Settings ".bold()
                },
            );
        let inner = block
            .inner(popup_area)
            .inner(Margin {
                horizontal: 1,
                vertical: 0,
            });
        let (base, dim, active) = self.styles(ctx);
        let [body_area, spacer_area, footer_area] = Layout::vertical([
                Constraint::Percentage(100),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(inner);
        let [top_pad, body_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Percentage(100),
            ])
            .areas(body_area);
        let _ = (spacer_area, top_pad);
        let [sidebar_area, divider_area, gap_area, content_area] = Layout::horizontal([
                Constraint::Length(20),
                Constraint::Length(1),
                Constraint::Length(2),
                Constraint::Percentage(100),
            ])
            .areas(body_area);
        let _ = gap_area;
        self.sidebar_areas = Vec::new();
        let mouse = ctx.modal_mouse_pos();
        let sidebar_focused = self.focus == SettingsFocus::Sidebar;
        let mut side_lines: Vec<Line> = Vec::new();
        for (idx, section) in Section::all().iter().enumerate() {
            let is_active = idx == self.sidebar_selected;
            let mut style = if is_active && sidebar_focused {
                active
            } else if is_active {
                base.add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            let area = Rect {
                x: sidebar_area.x,
                y: sidebar_area.y + idx as u16,
                width: sidebar_area.width,
                height: 1,
            };
            if mouse.is_some_and(|p| area.contains(p)) {
                style = crate::config::hover_style(style);
            }
            side_lines
                .push(
                    Line::from(
                        Span::styled(
                            format!(
                                " {} {}", if is_active { ">" } else { " " }, section.name()
                            ),
                            style,
                        ),
                    ),
                );
            self.sidebar_areas.push(area);
        }
        frame.render_widget(Paragraph::new(side_lines), sidebar_area);
        let visible = (content_area.height as usize).min(self.rows.len());
        self.visible_rows = visible;
        let max_scroll = self.rows.len().saturating_sub(visible);
        self.scroll = self.scroll.min(max_scroll);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected.saturating_add(1).saturating_sub(visible);
        }
        self.key_col_w = 0;
        self.action_col_w = 0;
        self.desc_col_w = 0;
        if self.section == Section::Keybinds {
            for row in &self.rows {
                if let ContentRow::KeyItem(item) = row {
                    self.key_col_w = self
                        .key_col_w
                        .max(remap_keys::key_display(&item.keys).chars().count());
                    self.action_col_w = self
                        .action_col_w
                        .max(item.display.chars().count());
                    self.desc_col_w = self
                        .desc_col_w
                        .max(
                            remap_keys::remap_description(&item.action).chars().count(),
                        );
                }
            }
            let content_w = content_area.width as usize;
            self.key_col_w = (self.key_col_w + 1).min(15).max(5);
            self.action_col_w = (self.action_col_w + 1).min(21).max(7);
            self.desc_col_w = (self.desc_col_w + 1)
                .min(
                    content_w
                        .saturating_sub(self.key_col_w + self.action_col_w + 16)
                        .max(10),
                )
                .max(10);
        }
        self.row_areas = Vec::new();
        self.click_targets = Vec::new();
        let mut lines: Vec<Line> = Vec::new();
        for idx in self.scroll..self.scroll + visible {
            let row = &self.rows[idx];
            let y = content_area.y + (idx - self.scroll) as u16;
            let row_area = Rect {
                x: content_area.x,
                y,
                width: content_area.width,
                height: 1,
            };
            self.row_areas.push((idx, row_area));
            let hovered = mouse.is_some_and(|p| row_area.contains(p));
            let mut targets = Vec::new();
            let content_selected = idx == self.selected
                && self.focus == SettingsFocus::Content;
            let line = self
                .render_row(
                    row,
                    content_selected,
                    hovered,
                    ctx,
                    base,
                    dim,
                    active,
                    content_area.x,
                    y,
                    content_area.width,
                    &mut targets,
                );
            self.click_targets.extend(targets);
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines), content_area);
        if self.rows.len() > visible {
            let thumb_h = ((visible * visible) / self.rows.len()).max(1) as u16;
            let travel = (visible as u16).saturating_sub(thumb_h);
            let thumb_y = content_area.y
                + travel * self.scroll as u16 / max_scroll.max(1) as u16;
            let buf = frame.buffer_mut();
            for y in content_area.y..content_area.bottom() {
                let c = if y >= thumb_y && y < thumb_y + thumb_h {
                    "█"
                } else {
                    "│"
                };
                buf.set_string(content_area.right() - 1, y, c, dim);
            }
        }
        let footer = if self.capturing {
            " press any key to assign · Esc to cancel "
        } else if self.adjusting.is_some() {
            " a/← d/→  adjust · Space/Enter  commit · Esc  cancel "
        } else if self.focus == SettingsFocus::Sidebar {
            " w/s ↑/↓  sidebar · d/→/Enter  open · Esc  close "
        } else {
            " w/s ↑/↓  options · d/→/Enter  toggle · a/←  back to sidebar · Esc  close "
        };
        frame
            .render_widget(
                Paragraph::new(Line::from(Span::styled(footer, dim))),
                footer_area,
            );
        {
            let buf = frame.buffer_mut();
            for y in divider_area.y..divider_area.bottom() {
                buf.set_string(divider_area.x, y, "│", dim);
            }
        }
        frame.render_widget(block, popup_area);
        Ok(())
    }
    fn handle_key(&mut self, _key: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
    fn handle_raw_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        ctx: &mut Ctx,
    ) -> Result<bool> {
        use crossterm::event::{KeyCode, KeyModifiers};
        if self.capturing {
            let key = Key::from(key);
            if key.key == KeyCode::Esc {
                self.capturing = false;
                ctx.render()?;
                return Ok(true);
            }
            let (section, action) = match self.rows.get(self.selected) {
                Some(ContentRow::KeyItem(item)) => (item.section, item.action.clone()),
                _ => {
                    self.capturing = false;
                    return Ok(true);
                }
            };
            let old_keys = remap_keys::apply_remap_runtime(ctx, section, &action, key)?;
            self.pending_remaps
                .push(PendingRemap {
                    section,
                    action: action.clone(),
                    new_key: KeySequence(vec![key]),
                    old_keys,
                });
            status_info!(
                "Remapped '{}' to {}", remap_keys::action_display(& action),
                KeySequence(vec![key])
            );
            self.capturing = false;
            self.refresh_rows(ctx);
            ctx.render()?;
            return Ok(true);
        }
        if self.editing_jellyfin_row.is_some() {
            use crossterm::event::KeyModifiers as KM;
            match key.code {
                KeyCode::Char(c) if !c.is_control() && key.modifiers == KM::NONE => {
                    self.edit_buffer.push(c);
                    ctx.render()?;
                }
                KeyCode::Backspace => {
                    self.edit_buffer.pop();
                    ctx.render()?;
                }
                KeyCode::Enter => {
                    self.commit_edit_jellyfin(ctx)?;
                }
                KeyCode::Esc => {
                    self.editing_jellyfin_row = None;
                    self.edit_buffer.clear();
                    ctx.render()?;
                }
                _ => {}
            }
            return Ok(true);
        }
        if self.editing_color.is_some() {
            use crossterm::event::KeyModifiers as KM;
            match key.code {
                KeyCode::Char(
                    c,
                ) if c.is_ascii_hexdigit() && key.modifiers == KM::NONE => {
                    if self.edit_buffer.chars().count() < 6 {
                        self.edit_buffer.push(c);
                        ctx.render()?;
                    }
                }
                KeyCode::Backspace => {
                    self.edit_buffer.pop();
                    ctx.render()?;
                }
                KeyCode::Enter => {
                    self.commit_edit_color(ctx)?;
                }
                KeyCode::Esc => {
                    self.editing_color = None;
                    self.edit_buffer.clear();
                    ctx.render()?;
                }
                _ => {}
            }
            return Ok(true);
        }
        if self.adjusting.is_some() {
            match key.code {
                KeyCode::Char('a')
                | KeyCode::Left if key.modifiers.is_empty()
                    || matches!(key.code, KeyCode::Left) => {
                    self.adjust(ctx, -1)?;
                }
                KeyCode::Char('d')
                | KeyCode::Right if key.modifiers.is_empty()
                    || matches!(key.code, KeyCode::Right) => {
                    self.adjust(ctx, 1)?;
                }
                KeyCode::Enter
                | KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => {
                    self.commit_adjust(ctx)?;
                }
                KeyCode::Esc => {
                    self.cancel_adjust(ctx)?;
                }
                _ => {}
            }
            return Ok(true);
        }
        match key.code {
            KeyCode::Char('w')
            | KeyCode::Char('s')
            | KeyCode::Up
            | KeyCode::Down if key.modifiers.is_empty()
                || matches!(key.code, KeyCode::Up | KeyCode::Down) => {
                let dir = match key.code {
                    KeyCode::Char('w') | KeyCode::Up => -1,
                    _ => 1,
                };
                match self.focus {
                    SettingsFocus::Sidebar => self.move_sidebar(dir, ctx),
                    SettingsFocus::Content => self.move_selection(dir, ctx),
                }
            }
            KeyCode::Char('a')
            | KeyCode::Left if key.modifiers.is_empty()
                || matches!(key.code, KeyCode::Left) => {
                if self.focus == SettingsFocus::Content {
                    self.focus = SettingsFocus::Sidebar;
                    ctx.render()?;
                }
            }
            KeyCode::Char('d')
            | KeyCode::Right if key.modifiers.is_empty()
                || matches!(key.code, KeyCode::Right) => {
                match self.focus {
                    SettingsFocus::Sidebar => {
                        self.populate(ctx);
                        self.focus = SettingsFocus::Content;
                    }
                    SettingsFocus::Content => {
                        if self.is_stepper_row(self.selected) {
                            self.start_adjust(ctx)?;
                        } else {
                            self.activate(ctx)?;
                        }
                    }
                }
            }
            KeyCode::Enter
            | KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => {
                match self.focus {
                    SettingsFocus::Sidebar => {
                        self.populate(ctx);
                        self.focus = SettingsFocus::Content;
                    }
                    SettingsFocus::Content => {
                        if self.is_stepper_row(self.selected) {
                            self.start_adjust(ctx)?;
                        } else {
                            self.activate(ctx)?;
                        }
                    }
                }
            }
            KeyCode::Esc => {
                if self.has_changes() {
                    self.prompt_save_or_discard(ctx)?;
                } else {
                    self.hide(ctx)?;
                }
            }
            _ => {}
        }
        Ok(true)
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        if matches!(event.kind, MouseEventKind::RightClick) {
            if self.has_changes() {
                self.prompt_save_or_discard(ctx)?;
            } else {
                self.hide(ctx)?;
            }
            return Ok(());
        }
        if matches!(
            event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick
        ) {
            if let Some(idx) = self
                .sidebar_areas
                .iter()
                .position(|area| area.contains(event.into()))
            {
                self.adjusting = None;
                self.adjust_snapshot = None;
                self.sidebar_selected = idx;
                self.populate(ctx);
                self.focus = SettingsFocus::Content;
                return Ok(());
            }
            if let Some((_, click)) = self
                .click_targets
                .iter()
                .rev()
                .find(|(area, _)| area.contains(event.into()))
            {
                if let Some((idx, _)) = self
                    .row_areas
                    .iter()
                    .find(|(_, area)| area.contains(event.into()))
                {
                    self.selected = *idx;
                }
                self.adjusting = None;
                self.adjust_snapshot = None;
                self.focus = SettingsFocus::Content;
                return self.do_click(*click, ctx);
            }
        }
        if matches!(event.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
            let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                -1
            } else {
                1
            };
            let position: ratatui::layout::Position = event.into();
            if self.sidebar_areas.iter().any(|area| area.contains(position)) {
                self.move_sidebar(dir, ctx);
            } else if self.row_areas.iter().any(|(_, area)| area.contains(position)) {
                self.move_selection(dir, ctx);
            }
        }
        Ok(())
    }
}
