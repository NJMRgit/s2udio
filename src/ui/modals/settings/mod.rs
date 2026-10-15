//! The settings panel: a two-pane navigator. The left sidebar lists the
//! sections (general / keybinds / mpd); the right pane shows the selected
//! section's rows. Mouse navigation is fully supported (sidebar clicks,
//! row clicks, [+]/[-]/[<]/[>] buttons, scroll).
use anyhow::Result;
use ratatui::{Frame, layout::Rect};

use super::{
    Modal,
    confirm_modal::{Action, ConfirmModal},
};
use crate::{
    AppEvent, MpdQueryResult,
    config::{UiSettings, keys::KeyConfig, state::AppStateFile},
    ctx::Ctx,
    mpd::{commands::replay_gain::ReplayGain, mpd_client::MpdClient},
    shared::{
        id::{self, Id},
        keys::ActionEvent,
        macros::modal,
        mouse_event::MouseEvent,
    },
    ui::UiAppEvent,
};

mod actions;
mod appearance;
mod cava;
mod input;
mod model;
mod render;
mod render_rows;
#[cfg(test)]
mod tests;

pub(crate) use actions::language_options;
pub(crate) use appearance::{
    AppearanceTarget, StagedColor, apply_persisted_appearance, persisted_appearance,
    set_appearance_color,
};
pub(crate) use model::{PendingRemap, StagedSettings};

use model::{
    AdjustSnapshot, Click, ContentRow, GeneralRow, JellyfinRow, MpvRow, MpdRow, PwNode, Section,
    SettingsFocus, StagedCava, TorrentRow, REPLAY_GAIN_SNAPSHOT,
};

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
    /// MPD replay gain mode when the panel opened (the persisted value;
    /// replaced by the live `replay_gain_status` snapshot when it answers
    /// before any cycle).
    replay_gain_initial: ReplayGain,
    /// Staged MPD replay gain mode — cycling acts on the live server
    /// immediately and the value is persisted to state.ron on Save.
    replay_gain_pending: ReplayGain,
    /// The user cycled the replay gain row: a late live snapshot must not
    /// clobber the staged choice.
    replay_gain_cycled: bool,
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

impl SettingsModal {
    pub fn new(ctx: &Ctx) -> Self {
        Self::new_on_section(ctx, Section::General)
    }

    /// Round 51: open the panel straight to the Jellyfin sign-in section
    /// (the Jellyfin tab's unconfigured state routes Enter / click here).
    pub fn open_jellyfin(ctx: &Ctx) {
        if let Err(err) = ctx.app_event_sender.send(crate::AppEvent::UiEvent(
            crate::ui::UiAppEvent::Modal(Box::new(SettingsModal::new_on_section(
                ctx,
                Section::Jellyfin,
            ))),
        )) {
            log::error!("Failed to open a modal: {err}");
        }
    }

    /// Build the panel with a preselected section (default: General).
    fn new_on_section(ctx: &Ctx, section: Section) -> Self {
        let ui_initial = ctx.config.ui;
        let video_initial = ctx.config.video.playback;
        let mpv_audio_initial = ctx.config.mpv.audio_lang.clone();
        let mpv_subtitles_initial = ctx.config.mpv.subtitles.clone();
        let mpv_svp_initial = ctx.config.mpv.svp;
        let mpv_custom_lang = match (&mpv_audio_initial, &mpv_subtitles_initial) {
            (crate::config::mpv::MpvAudioLang::Custom { lang }, _)
            | (_, crate::config::mpv::MpvSubtitleMode::Custom { lang }) => lang.clone(),
            _ => crate::config::mpv::os_language_code().unwrap_or_else(|| "en".to_owned()),
        };
        let cava_initial = StagedCava::from_config(&ctx.config);
        // Round 53: replay gain baseline = the persisted value (fallback
        // "off"); the LIVE mode is snapshotted via `replay_gain_status`
        // below — an unparsable/hand-edited state value degrades to off.
        let replay_gain = AppStateFile::load()
            .mpd_replay_gain
            .and_then(|mode| mode.parse::<ReplayGain>().ok())
            .unwrap_or_default();
        let torrent_socks_proxy = ctx
            .config
            .torrent
            .socks_proxy
            .clone()
            .unwrap_or_default();
        let mut modal = Self {
            id: id::new(),
            section,
            rows: Vec::new(),
            selected: 0,
            sidebar_selected: Section::all()
                .iter()
                .position(|s| *s == section)
                .unwrap_or(0),
            focus: SettingsFocus::Sidebar,
            nodes: Vec::new(),
            library_path: AppStateFile::load().mpd_library_path.unwrap_or_default(),
            jellyfin_url: actions::jellyfin_current_url(ctx),
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
            replay_gain_initial: replay_gain,
            replay_gain_pending: replay_gain,
            replay_gain_cycled: false,
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
        // Round 53: snapshot the LIVE replay gain mode (`replay_gain_status`
        // — `status` does not carry it). A fetch error (dead server / too
        // old MPD) leaves the persisted value / "off" in place without
        // failing the panel.
        ctx.query()
            .id(REPLAY_GAIN_SNAPSHOT)
            .replace_id(REPLAY_GAIN_SNAPSHOT)
            .query(move |client| {
                Ok(crate::MpdQueryResult::Any(Box::new(client.get_replay_gain()?)))
            });
        modal
    }

    fn build_rows(&self, ctx: &Ctx) -> Vec<ContentRow> {
        match self.section {
            Section::General => {
                let mut rows = vec![
                    ContentRow::General(GeneralRow::FeaturesHeader),
                    ContentRow::General(GeneralRow::AlbumArt),
                    ContentRow::General(GeneralRow::Lyrics),
                    ContentRow::General(GeneralRow::Cava),
                    ContentRow::General(GeneralRow::RadioReload),
                    ContentRow::General(GeneralRow::Jellyfin),
                    ContentRow::General(GeneralRow::VideoPlayback),
                    ContentRow::General(GeneralRow::AutoChapters),
                    ContentRow::General(GeneralRow::Mpdis2Notifications),
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
                for item in crate::ui::modals::remap_keys::build_remap_rows(ctx) {
                    if last_section != Some(item.section) {
                        rows.push(ContentRow::KeyHeader(item.section));
                        last_section = Some(item.section);
                    }
                    rows.push(ContentRow::KeyItem(item));
                }
                rows
            }
            Section::Mpv => vec![
                ContentRow::Mpv(MpvRow::Header),
                ContentRow::Mpv(MpvRow::AudioLang),
                ContentRow::Mpv(MpvRow::Subtitles),
                ContentRow::Mpv(MpvRow::Svp),
                ContentRow::Mpv(MpvRow::EditConf),
            ],
            Section::Mpd => vec![
                ContentRow::Mpd(MpdRow::LibraryHeader),
                ContentRow::Mpd(MpdRow::LibraryPath),
                ContentRow::Mpd(MpdRow::LibraryPlaylists),
                ContentRow::Mpd(MpdRow::Update),
                ContentRow::Mpd(MpdRow::Rescan),
                ContentRow::Mpd(MpdRow::PlaybackHeader),
                ContentRow::Mpd(MpdRow::Crossfade),
                ContentRow::Mpd(MpdRow::Repeat),
                ContentRow::Mpd(MpdRow::Random),
                ContentRow::Mpd(MpdRow::Single),
                ContentRow::Mpd(MpdRow::Consume),
                ContentRow::Mpd(MpdRow::ReplayGain),
                ContentRow::Mpd(MpdRow::DevicesHeader),
                ContentRow::Mpd(MpdRow::Outputs),
            ],
            Section::Jellyfin => vec![
                ContentRow::Jellyfin(JellyfinRow::Header),
                ContentRow::Jellyfin(JellyfinRow::ServerUrl),
                ContentRow::Jellyfin(JellyfinRow::Username),
                ContentRow::Jellyfin(JellyfinRow::Password),
                ContentRow::Jellyfin(JellyfinRow::SignIn),
            ],
            Section::Torrent => vec![
                ContentRow::Torrent(TorrentRow::Header),
                ContentRow::Torrent(TorrentRow::WebUi),
                ContentRow::Torrent(TorrentRow::StopEngine),
                ContentRow::Torrent(TorrentRow::SocksProxy),
            ],
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
            || self.replay_gain_pending != self.replay_gain_initial
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
            replay_gain: self.replay_gain_cycled.then_some(self.replay_gain_pending),
        }
    }

    fn on_replay_gain_query(
        &mut self,
        id: &'static str,
        data: &mut MpdQueryResult,
        ctx: &Ctx,
    ) -> Result<()> {
        // Round 53: the live replay gain mode snapshot taken when the panel
        // opened. Applied to both the baseline and the staged value so a
        // fresh open with no edits never triggers a spurious save prompt.
        // A late snapshot after the user already cycled the row is ignored
        // (it must not clobber the staged choice).
        if id == REPLAY_GAIN_SNAPSHOT && !self.replay_gain_cycled {
            if let MpdQueryResult::Any(any) = data {
                let any: &dyn std::any::Any = &**any;
                if let Some(mode) = any.downcast_ref::<ReplayGain>() {
                    let mode = *mode;
                    self.replay_gain_initial = mode;
                    self.replay_gain_pending = mode;
                    ctx.render()?;
                }
            }
        }
        Ok(())
    }
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

    fn on_query_finished(
        &mut self,
        id: &'static str,
        data: &mut MpdQueryResult,
        ctx: &Ctx,
    ) -> Result<()> {
        self.on_replay_gain_query(id, data, ctx)
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        render::render(self, frame, ctx)
    }

    fn handle_key(&mut self, _key: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }

    fn handle_raw_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        ctx: &mut Ctx,
    ) -> Result<bool> {
        self.handle_raw_key_event(key, ctx)
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        self.handle_mouse_event_input(event, ctx)
    }
}
