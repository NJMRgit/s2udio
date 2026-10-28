use std::process::Command;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use super::{
    model::{
        AdvancedRow, Click, ContentRow, GeneralRow, JellyfinRow, MpvLanguageTarget, MpvRow, MpdRow,
        PendingRemap, TorrentRow, FPS_MAX, FPS_MIN, FREQ_MAX_LIMIT, FREQ_MIN_LIMIT,
    },
    SettingsModal,
};
use crate::{
    config::{keys::{Key, KeySequence}, state::AppStateFile},
    ctx::Ctx,
    mpd::{
        commands::replay_gain::ReplayGain,
        mpd_client::MpdClient,
    },
    shared::{
        macros::{modal, status_error, status_info, status_warn},
        mpd_client_ext::MpdClientExt,
    },
    ui::{
        modals::{input_modal::InputModal, mpv_conf_editor::MpvConfEditorModal},
        OPEN_OUTPUTS_MODAL,
    },
};

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
                "--file-selection".into(),
                "--directory".into(),
                "--filename".into(),
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

/// The Jellyfin server URL currently in use (Settings sidecar, then
/// jellytui's config) for the settings form.
pub(super) fn jellyfin_current_url(ctx: &Ctx) -> String {
    let sidecar = crate::config::jellyfin::jellyfin_sidecar_path();
    let url = std::fs::read_to_string(&sidecar)
        .ok()
        .and_then(|c| {
            ron::de::from_str::<crate::config::jellyfin::JellyfinCredentialsFile>(&c).ok()
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
    /// Stage a show/hide toggle: the live UI is untouched until the panel is
    /// closed with Save.
    pub(super) fn toggle_ui(&mut self, ctx: &mut Ctx, row: GeneralRow, value: bool) -> Result<()> {
        match row {
            GeneralRow::AlbumArt => self.ui_pending.show_album_art = value,
            GeneralRow::Lyrics => self.ui_pending.show_lyrics = value,
            GeneralRow::Cava => self.ui_pending.show_cava = value,
            GeneralRow::Jellyfin => self.ui_pending.show_jellyfin_tab = value,
            GeneralRow::AutoChapters => self.ui_pending.auto_show_chapters = value,
            GeneralRow::Mpdis2Notifications => {
                self.ui_pending.mpdris2_notifications = value;
            }
            _ => unreachable!("toggle_ui called with a non-toggle row"),
        }
        ctx.render()?;
        Ok(())
    }

    /// Stage the memory watchdog toggle (Settings > Advanced).
    ///
    /// Staged like every other settings row: the running watchdog is not
    /// started or stopped until the panel is saved, and the value persists to
    /// the config sidecar with the rest of the UI settings.
    pub(super) fn toggle_advanced(&mut self, ctx: &mut Ctx, row: AdvancedRow) -> Result<()> {
        match row {
            AdvancedRow::MemoryWatchdog => {
                self.ui_pending.memory_watchdog = !self.ui_pending.memory_watchdog;
            }
            // The interval is adjusted, not toggled; the header is not
            // interactive. Reaching either here is a wiring bug, not user
            // input.
            AdvancedRow::DiagnosticsHeader | AdvancedRow::MemoryWatchdogInterval => {
                unreachable!("toggle_advanced called with a non-toggle row")
            }
        }
        ctx.render()?;
        Ok(())
    }

    /// Cycle the staged auto-theme source by `delta` (off -> auto -> sttm ->
    /// matugen -> kde -> off). Staged like every other row: the watcher is
    /// restarted with the new source when the panel is saved.
    pub(super) fn step_auto_theme(&mut self, ctx: &mut Ctx, delta: i64) -> Result<()> {
        use crate::config::auto_theme::AutoThemeSource;
        let all = AutoThemeSource::ALL;
        let current = all
            .iter()
            .position(|source| *source == self.auto_theme_pending)
            .unwrap_or(0) as i64;
        let next = all[(current + delta).rem_euclid(all.len() as i64) as usize];
        self.auto_theme_pending = next;
        ctx.render()?;
        Ok(())
    }

    /// Step the selected row by `delta` (-1 or +1, scaled per row).
    pub(super) fn adjust(&mut self, ctx: &mut Ctx, delta: i64) -> Result<()> {
        match &self.rows[self.selected] {
            ContentRow::General(g) => match g {
                GeneralRow::VideoPlayback => {
                    let all = crate::config::video::VideoPlaybackMode::ALL;
                    let idx = all
                        .iter()
                        .position(|m| *m == self.video_pending)
                        .unwrap_or(0) as i64;
                    self.video_pending = all[(idx + delta).rem_euclid(all.len() as i64) as usize];
                    ctx.render()?;
                    Ok(())
                }
                GeneralRow::Fps => {
                    let v = (i64::from(self.fps()) + delta * 5)
                        .clamp(i64::from(FPS_MIN), i64::from(FPS_MAX)) as u16;
                    self.set_cava(ctx, |c| c.framerate = v)
                }
                GeneralRow::Sensitivity => {
                    let v = (i64::from(self.sensitivity()) + delta * 5).clamp(1, 500) as u16;
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
                    self.ui_pending.show_virtual_devices = !self.ui_pending.show_virtual_devices;
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
            },
            ContentRow::Mpv(row) => match row {
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
                    use crate::config::mpv::MpvSubtitleMode::{Custom, Hidden, SystemLanguage};
                    let idx = match &self.mpv_subtitles_pending {
                        Hidden => 0,
                        SystemLanguage => 1,
                        Custom { .. } => 2,
                    };
                    self.mpv_subtitles_pending = match (idx as i64 + delta).rem_euclid(3) {
                        0 => Hidden,
                        1 => SystemLanguage,
                        _ => Custom {
                            lang: self.mpv_custom_lang.clone(),
                        },
                    };
                    ctx.render()?;
                    Ok(())
                }
                MpvRow::Svp => {
                    self.mpv_svp_pending = !self.mpv_svp_pending;
                    ctx.render()?;
                    Ok(())
                }
                MpvRow::EditConf => {
                    Self::open_mpv_conf_editor(ctx);
                    Ok(())
                }
                MpvRow::Header => Ok(()),
            },
            ContentRow::Appearance(_) => Ok(()),
            ContentRow::AutoTheme => self.step_auto_theme(ctx, delta),
            ContentRow::Jellyfin(_) => Ok(()),
            ContentRow::Torrent(_) => Ok(()),
            ContentRow::Mpd(m) => match m {
                MpdRow::Crossfade => {
                    let v = (ctx.status.xfade.unwrap_or(0) as i64 + delta).clamp(0, 60) as u32;
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
                MpdRow::ReplayGain => {
                    let idx = ReplayGain::ALL
                        .iter()
                        .position(|m| *m == self.replay_gain_pending)
                        .unwrap_or(0) as i64;
                    let next =
                        ReplayGain::ALL[(idx + delta).rem_euclid(ReplayGain::ALL.len() as i64) as usize];
                    self.replay_gain_pending = next;
                    self.replay_gain_cycled = true;
                    // Round 53: act on the live server immediately; the
                    // staged value is persisted on Save.
                    ctx.command(move |client| {
                        client.replay_gain(next)?;
                        Ok(())
                    });
                    ctx.render()?;
                    Ok(())
                }
                _ => Ok(()),
            },
            ContentRow::Advanced(AdvancedRow::MemoryWatchdogInterval) => {
                // Step through a small set of sane intervals rather than
                // free-typing a number: the useful range is narrow, and a
                // typed value would need clamping anyway (the watchdog
                // clamps to MIN_INTERVAL_SECS as a backstop).
                const STEPS: [u64; 6] = [10, 30, 60, 300, 900, 3600];
                let current = self.ui_pending.memory_watchdog_interval_secs;
                let idx = STEPS.iter().position(|s| *s == current).unwrap_or(3) as i64;
                let next = STEPS[(idx + delta).rem_euclid(STEPS.len() as i64) as usize];
                self.ui_pending.memory_watchdog_interval_secs = next;
                ctx.render()?;
                Ok(())
            }
            ContentRow::Advanced(_) => Ok(()),
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

    /// Begin typing a Jellyfin text field (server URL / username /
    /// password); committed with Enter, cancelled with Esc.
    pub(super) fn start_edit_jellyfin(&mut self, row: JellyfinRow) {
        let current = match row {
            JellyfinRow::ServerUrl => self.jellyfin_url.clone(),
            JellyfinRow::Username => self.jellyfin_username.clone(),
            JellyfinRow::Password => self.jellyfin_password.clone(),
            _ => String::new(),
        };
        self.edit_buffer = current;
        self.editing_jellyfin_row = Some(row);
    }

    pub(super) fn commit_edit_jellyfin(&mut self, ctx: &mut Ctx) -> Result<()> {
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

    pub(super) fn activate(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some(row) = self.rows.get(self.selected).cloned() else { return Ok(()) };
        match &row {
            ContentRow::General(g) => match g {
                GeneralRow::AlbumArt
                | GeneralRow::Lyrics
                | GeneralRow::Cava
                | GeneralRow::Jellyfin
                | GeneralRow::AutoChapters
                | GeneralRow::Mpdis2Notifications => {
                    let value = match g {
                        GeneralRow::AlbumArt => !self.ui_pending.show_album_art,
                        GeneralRow::Lyrics => !self.ui_pending.show_lyrics,
                        GeneralRow::Cava => !self.ui_pending.show_cava,
                        GeneralRow::Jellyfin => !self.ui_pending.show_jellyfin_tab,
                        GeneralRow::AutoChapters => !self.ui_pending.auto_show_chapters,
                        GeneralRow::Mpdis2Notifications => !self.ui_pending.mpdris2_notifications,
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
                            log::error!(error:? = err; "Failed to request radio directory")
                        });
                    status_warn!("Reloading radio stations…");
                    ctx.render()?;
                    Ok(())
                }
                _ => Ok(()),
            },
            ContentRow::Mpv(row) => match row {
                MpvRow::AudioLang
                    if matches!(
                        self.mpv_audio_pending,
                        crate::config::mpv::MpvAudioLang::Custom { .. }
                    ) =>
                {
                    Self::open_language_picker(ctx, MpvLanguageTarget::Audio);
                    Ok(())
                }
                MpvRow::Subtitles
                    if matches!(
                        self.mpv_subtitles_pending,
                        crate::config::mpv::MpvSubtitleMode::Custom { .. }
                    ) =>
                {
                    Self::open_language_picker(ctx, MpvLanguageTarget::Subtitles);
                    Ok(())
                }
                _ => self.adjust(ctx, 1),
            },
            ContentRow::Appearance(target) => {
                self.start_edit_color(*target, ctx);
                Ok(())
            }
            ContentRow::AutoTheme => self.step_auto_theme(ctx, 1),
            ContentRow::KeyItem(_) => {
                self.capturing = true;
                ctx.render()?;
                Ok(())
            }
            ContentRow::Jellyfin(m) => match m {
                JellyfinRow::ServerUrl | JellyfinRow::Username | JellyfinRow::Password => {
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
                        status_warn!("Fill in the server URL, username and password first");
                        return Ok(());
                    }
                    match crate::jellyfin::Jellyfin::authenticate(&url, &username, &password) {
                        Ok((token, user_id)) => {
                            self.jellyfin_credentials =
                                Some(crate::config::jellyfin::JellyfinCredentialsFile {
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
            },
            ContentRow::Torrent(m) => match m {
                TorrentRow::WebUi => {
                    let needs_start = match ctx.torrent_webui_engine.borrow_mut().as_mut() {
                        Some(engine) => !engine.is_running(),
                        None => true,
                    };
                    if needs_start {
                        if let Some(reg) = crate::core::rqctl::registered_running() {
                            if let Err(err) = crate::core::rqctl::open_web_ui(&reg.web_url) {
                                status_warn!("{err}");
                            }
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
                    if let Err(err) = crate::core::rqctl::open_web_ui(&url) {
                        status_warn!("{err}");
                    }
                    status_info!("rqbit web UI opened at {url}");
                    ctx.render()?;
                    Ok(())
                }
                TorrentRow::StopEngine => {
                    let stopped = ctx.torrent_webui_engine.borrow_mut().take().is_some();
                    let stopped_registered =
                        crate::core::rqctl::stop_registered().unwrap_or(false);
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
                        .initial_value(current).on_confirm(|ctx, value| {
                            *ctx.torrent_socks_proxy_input.borrow_mut() = Some(value.to_string());
                            Ok(())
                        })
                    );
                    Ok(())
                }
                // A plain toggle: stage it, apply on Save.
                TorrentRow::StreamDownloads => {
                    self.torrent_stream_downloads_pending =
                        !self.torrent_stream_downloads_pending;
                    ctx.render()?;
                    Ok(())
                }
                TorrentRow::Header => Ok(()),
            },
            ContentRow::Mpd(m) => match m {
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
                                .input_label("path").initial_value(current).on_confirm(|ctx, value| {
                                    let mut state = AppStateFile::load();
                                    state.last_tab = ctx.config.tabs.names.first().map(|t| t.to_string());
                                    state.mpd_library_path = Some(value.to_string());
                                    if let Err(err) = state.save() {
                                        status_warn!("Failed to save state: {err}");
                                    }
                                    ctx.render()?;
                                    Ok(())
                                })
                            );
                        }
                    }
                    Ok(())
                }
                MpdRow::LibraryPlaylists => {
                    self.ui_pending.library_playlist_files = !self.ui_pending.library_playlist_files;
                    ctx.render()?;
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
                        "{}",
                        if rescan { "Rescanning library…" } else { "Updating library…" }
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
                            let outputs = client.list_partitioned_outputs(&current_partition)?;
                            Ok(crate::MpdQueryResult::Outputs(outputs))
                        });
                    Ok(())
                }
                MpdRow::Crossfade
                | MpdRow::Repeat
                | MpdRow::Random
                | MpdRow::Single
                | MpdRow::Consume
                | MpdRow::ReplayGain => self.adjust(ctx, 1),
                _ => Ok(()),
            },
            ContentRow::Advanced(AdvancedRow::MemoryWatchdog) => {
                self.toggle_advanced(ctx, AdvancedRow::MemoryWatchdog)
            }
            ContentRow::Advanced(_) => Ok(()),
            ContentRow::KeyTableHeader | ContentRow::KeyHeader(_) => Ok(()),
        }
    }

    pub(super) fn capture_remap(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Result<bool> {
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
        let old_keys = crate::ui::modals::remap_keys::apply_remap_runtime(
            ctx, section, &action, key,
        )?;
        self.pending_remaps.push(PendingRemap {
            section,
            action: action.clone(),
            new_key: KeySequence(vec![key]),
            old_keys,
        });
        status_info!(
            "Remapped '{}' to {}",
            crate::ui::modals::remap_keys::action_display(&action),
            KeySequence(vec![key])
        );
        self.capturing = false;
        self.refresh_rows(ctx);
        ctx.render()?;
        Ok(true)
    }

    pub(super) fn do_click(&mut self, click: Click, ctx: &mut Ctx) -> Result<()> {
        match click {
            Click::Toggle | Click::Activate | Click::Edit => self.activate(ctx),
            Click::Inc | Click::Next => self.adjust(ctx, 1),
            Click::Dec | Click::Prev => self.adjust(ctx, -1),
        }
    }

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

    /// Open the in-TUI nano-style editor for the user's mpv.conf (the
    /// Settings -> mpv "Edit mpv.conf" row, Round 48).
    fn open_mpv_conf_editor(ctx: &mut Ctx) {
        modal!(ctx, MpvConfEditorModal::new());
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
            .title(match target {
                MpvLanguageTarget::Audio => "Audio language",
                MpvLanguageTarget::Subtitles => "Subtitle language",
            }).on_confirm(move |ctx, selected, _idx| {
                let lang = Self::subtitle_language_options()
                    .iter()
                    .find(|(name, _)| **name == selected)
                    .map(|(_, code)| (*code).to_owned())
                    .unwrap_or_else(|| selected);
                match target {
                    MpvLanguageTarget::Audio => {
                        *ctx.mpv_custom_audio_lang.borrow_mut() = Some(lang);
                    }
                    MpvLanguageTarget::Subtitles => {
                        *ctx.mpv_custom_subtitle_lang.borrow_mut() = Some(lang);
                    }
                }
                Ok(())
            }).build()
        );
    }
}

/// The language names the settings + the controls' audio/subtitle buttons
/// offer (name, ISO 639-1 code).
pub(crate) fn language_options() -> &'static [(&'static str, &'static str)] {
    SettingsModal::subtitle_language_options()
}
