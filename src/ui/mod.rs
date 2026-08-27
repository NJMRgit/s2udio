use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use itertools::Itertools;
use modals::{
    add_random_modal::AddRandomModal, decoders::DecodersModal, info_list_modal::InfoListModal,
    input_modal::InputModal, menu::modal::MenuModal, outputs::OutputsModal, tab_help::TabHelpModal,
};
use panes::{PaneContainer, Panes, pane_call};
use ratatui::{
    Frame,
    layout::{Alignment, Position, Rect},
    style::{Color, Style},
    widgets::{Block, Clear},
};
use tab_screen::TabScreen;

use self::{modals::Modal, panes::Pane};
use crate::{
    MpdQueryResult,
    config::{
        Config, UiSettings,
        cli::{Args, Command},
        keys::{CommonAction, GlobalAction, Key, KeyConfig, actions::RateKind},
        tabs::{PaneType, SizedPaneOrSplit, TabName, TreeBrowserArgs},
        theme::level_styles::LevelStyles,
    },
    core::{
        command::{create_env, run_external},
        config_watcher::ERROR_CONFIG_MODAL_ID,
    },
    ctx::{Ctx, FETCH_SONG_STICKERS, LIKE_STICKER, RATING_STICKER},
    mpd::{
        commands::{State, idle::IdleEvent},
        errors::{ErrorCode, MpdError, MpdFailureResponse},
        mpd_client::{MpdClient, MpdCommand, ValueChange},
        proto_client::ProtoClient,
        version::Version,
    },
    shared::{
        events::{Level, WorkRequest},
        id::Id,
        keys::{ActionEvent, Actions, KeyResolver},
        macros::{modal, status_error, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::{Enqueue, MpdClientExt},
        ytdlp::YtDlpHost,
    },
    ui::{
        image::facade::EncodeData,
        input::{InputEvent, InputResultEvent},
        modals::{downloads::DownloadsModal, menu::create_rating_modal},
    },
};
use unicode_width::UnicodeWidthStr;

pub mod band;
pub mod browser;
pub mod dir_or_song;
pub mod dirstack;
pub mod image;
pub mod input;
pub mod modals;
pub mod panes;
pub mod seekbar;
pub mod song_list;
pub mod tab_screen;
pub mod tree_browser;
pub mod widgets;

#[derive(Debug)]
pub struct StatusMessage {
    pub message: String,
    pub level: Level,
    pub created: std::time::Instant,
    pub timeout: std::time::Duration,
}

#[derive(Debug)]
pub struct Ui {
    panes: PaneContainer,
    modals: Vec<Box<dyn Modal>>,
    tabs: HashMap<TabName, TabScreen>,
    layout: SizedPaneOrSplit,
    area: Rect,
    resizing: bool,
    overlays_hidden: bool,
    cava_hidden: bool,
    /// Set when the cava row was removed (hidden): the event loop clears
    /// the terminal and repaints fully, so the visualizer's terminal-side
    /// bars can never leave stale cells behind.
    cava_refresh_pending: bool,
    /// Runtime show/hide toggles from the Settings panel; re-applied to the
    /// config on every config reload so they survive within the session.
    ui_settings: UiSettings,
}

const OPEN_DECODERS_MODAL: &str = "open_decoders_modal";
pub(crate) const OPEN_OUTPUTS_MODAL: &str = "open_outputs_modal";

pub const FILTER_PREFIX: &str = "[FILTER]:";

macro_rules! active_tab_call {
    ($self:ident, $ctx:ident, $fn:ident($($param:expr),+)) => {
        $self.tabs
            .get_mut(&$ctx.active_tab)
            .context(anyhow!("Expected tab '{}' to be defined. Please report this along with your config.", $ctx.active_tab))?
            .$fn(&mut $self.panes, $($param),+)
    }
}

impl Ui {
    pub fn new(ctx: &Ctx) -> Result<Ui> {
        Ok(Self {
            panes: PaneContainer::new(ctx)?,
            layout: ctx.config.theme.layout.clone(),
            modals: Vec::default(),
            area: Rect::default(),
            tabs: Self::init_tabs(ctx)?,
            resizing: false,
            overlays_hidden: false,
            cava_hidden: false,
            cava_refresh_pending: false,
            ui_settings: ctx.config.ui,
        })
    }

    fn init_tabs(ctx: &Ctx) -> Result<HashMap<TabName, TabScreen>> {
        ctx.config
            .tabs
            .tabs
            .iter()
            .map(|(name, screen)| -> Result<_> {
                Ok((name.clone(), TabScreen::new(screen.panes.clone())?))
            })
            .try_collect()
    }

    fn calc_areas(&mut self, area: Rect, _ctx: &Ctx) {
        self.area = area;
    }

    /// Set by the event loop while the terminal is being resized; while set,
    /// render() shows only the pixel size and skips the normal UI.
    pub fn set_resizing(&mut self, resizing: bool, ctx: &Ctx) {
        self.resizing = resizing;
        // The cava pane gates its geometry restarts on this flag, so an
        // in-progress resize cannot respawn the visualizer (and drop the
        // audio) on every frame; the settled resize re-initializes it.
        ctx.resizing.set(resizing);
    }

    pub fn change_tab(&mut self, new_tab: TabName, ctx: &mut Ctx) -> Result<()> {
        // The cava visualizer is hidden on tabs where it would go flat (the
        // Jellyfin tab always, the Queue tab while a video plays in mpv) and
        // restarted when leaving them.
        let entering_cava_hidden = ctx.cava_hidden_on(new_tab.as_str());
        let leaving_cava_hidden = ctx.cava_hidden_on(ctx.active_tab.as_str());

        self.layout.for_each_pane(
            self.area,
            self.area.height,
            &mut |pane, _, _, _, _| {
                match self.panes.get_mut(&pane.pane, ctx)? {
                    Panes::TabContent => {
                        active_tab_call!(self, ctx, on_hide(ctx))?;
                    }
                    _ => {}
                }
                Ok(())
            },
            ctx,
        )?;
        if entering_cava_hidden {
            self.panes.cava.on_hide(ctx)?;
            // The cava row collapsed: clear the whole window and repaint
            // so the visualizer's overlay leaves no stale cells.
            self.request_cava_refresh();
        } else if leaving_cava_hidden && !self.cava_hidden {
            self.panes.cava.before_show(ctx)?;
        }

        ctx.active_tab = new_tab.clone();
        // Leaving the Queue tab hands the keyboard back to the new tab's
        // panes: drop the seekbar's control.
        seekbar::clear(ctx);
        // Remember the tab for the next start (restored when nothing is
        // playing). Errors are logged, never fatal.
        let mut state = crate::config::state::AppStateFile::load();
        state.last_tab = Some(new_tab.to_string());
        if let Err(err) = state.save() {
            log::error!(error:? = err; "Failed to save state file");
        }
        self.on_event(UiEvent::TabChanged(new_tab), ctx)?;
        self.layout.for_each_pane(
            self.area,
            self.area.height,
            &mut |pane, pane_area, _, _, _| {
                match self.panes.get_mut(&pane.pane, ctx)? {
                    Panes::TabContent => {
                        active_tab_call!(self, ctx, before_show(pane_area, ctx))?;
                    }
                    _ => {}
                }
                Ok(())
            },
            ctx,
        )
    }

    pub fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        self.area = frame.area();

        // The controls/info and seekbar rows need this many rows (5 + 3).
        // Below that there is nothing useful to render, so show an error
        // until the terminal is resized large enough.
        const MIN_CONTENT_HEIGHT: u16 = 8;
        // The terminal window must be at least this wide in pixels. Only
        // terminals that report real pixel sizes via TIOCGWINSZ (kitty et
        // al.) are measured; when pixels are not reported the check is
        // skipped rather than guessing from the column count.
        const MIN_WIDTH_PX: u16 = 500;
        let size_px =
            crossterm::terminal::window_size().map(|s| (s.width, s.height)).unwrap_or((0, 0));
        let too_small =
            frame.area().height < MIN_CONTENT_HEIGHT || (size_px.0 > 0 && size_px.0 < MIN_WIDTH_PX);
        let degraded = self.resizing || too_small;
        if degraded {
            if !self.overlays_hidden {
                // Terminal-side overlays (kitty/iTerm/sixel album art and the
                // cava bars) draw outside the ratatui buffer, so hide them
                // while the window is in a transient state.
                self.overlays_hidden = true;
                self.panes.album_art.on_hide(ctx)?;
            }
            // The Jellyfin tab's poster is a terminal-side overlay too;
            // hide it while the window is in a transient size.
            let jellyfin_hidden = self.tabs.get(&ctx.active_tab).is_some_and(|tab| {
                tab.panes
                    .panes_iter()
                    .any(|p| p.pane == PaneType::Jellyfin { tree: TreeBrowserArgs::default() })
            });
            if jellyfin_hidden {
                let id = PaneType::Jellyfin { tree: TreeBrowserArgs::default() };
                if let Ok(Panes::Jellyfin(jellyfin)) = self.panes.get_mut(&id, ctx) {
                    jellyfin.hide_pending_poster(ctx);
                }
            }
            if !self.cava_hidden {
                self.cava_hidden = true;
                self.panes.cava.on_hide(ctx)?;
            }
            let area = frame.area();
            if self.resizing {
                // During a resize the window is kept completely clear:
                // nothing is drawn until the resize settles (ResizedDebounced
                // flips resizing off and the normal UI returns).
                frame.render_widget(Clear, area);
                return Ok(());
            }
            // Too small (not resizing): show the hint message.
            let msg = if size_px.0 > 0 && size_px.0 < MIN_WIDTH_PX {
                format!(
                    "Terminal too small: need at least {MIN_WIDTH_PX}px width (current: {}px)",
                    size_px.0
                )
            } else {
                format!(
                    "Terminal too small: need at least {MIN_CONTENT_HEIGHT} rows to show controls and seekbar"
                )
            };
            let style =
                ctx.config.theme.text_color.map_or_else(Style::default, |c| Style::default().fg(c));
            let bg = ctx
                .config
                .theme
                .background_color
                .map_or_else(Style::default, |c| Style::default().bg(c));
            // Wipe stale cells and the background so only the message shows
            // while the terminal is in a transient size.
            frame.render_widget(Clear, area);
            frame.render_widget(Block::default().style(bg), area);
            frame.render_widget(
                ratatui::widgets::Paragraph::new(msg).alignment(Alignment::Center).style(style),
                Rect { x: area.x, y: area.y + area.height / 2, width: area.width, height: 1 },
            );
            return Ok(());
        }
        if self.overlays_hidden || self.cava_hidden {
            // Terminal is usable again: bring the overlays back. The album
            // art overlay only belongs to the tab that contains it — never
            // redraw it (e.g. after a resize) while another tab is active,
            // or it paints over that tab's panes at the stale area.
            let album_art_visible =
                self.tabs.get(&ctx.active_tab).is_some_and(|tab| {
                    tab.panes.panes_iter().any(|pane| pane.pane == PaneType::AlbumArt)
                }) || self.layout.panes_iter().any(|pane| pane.pane == PaneType::AlbumArt);
            if self.overlays_hidden && album_art_visible && !ctx.is_pane_hidden(&PaneType::AlbumArt)
            {
                self.overlays_hidden = false;
                self.panes.album_art.before_show(ctx)?;
            } else {
                self.overlays_hidden = false;
            }
            if self.cava_hidden && !ctx.is_pane_hidden(&PaneType::Cava) {
                self.cava_hidden = false;
                self.panes.cava.before_show(ctx)?;
            } else {
                self.cava_hidden = false;
            }
        }

        if let Some(bg_color) = ctx.config.theme.background_color {
            frame
                .render_widget(Block::default().style(Style::default().bg(bg_color)), frame.area());
        }

        self.layout.for_each_pane_custom_data(
            self.area,
            self.area.height,
            &mut *frame,
            &mut |pane, pane_area, block, block_area, bg_color, frame| {
                match self.panes.get_mut(&pane.pane, ctx)? {
                    Panes::TabContent => {
                        active_tab_call!(
                            self,
                            ctx,
                            render(frame, pane_area, self.area.height, ctx)
                        )?;
                    }
                    mut pane_instance => {
                        pane_call!(pane_instance, render(frame, pane_area, ctx))?;
                    }
                }
                if let Some(bg_color) = bg_color {
                    frame.render_widget(
                        Block::default().style(Style::default().bg(bg_color)),
                        pane_area,
                    );
                }
                let border_style =
                    pane.border_style.unwrap_or_else(|| ctx.config.as_border_style());
                frame.render_widget(block.border_style(border_style), block_area);
                Ok(())
            },
            &mut |block, block_area, background_color, frame| {
                if let Some(bg_color) = background_color {
                    frame.render_widget(
                        Block::default().style(Style::default().bg(bg_color)),
                        block.inner(block_area),
                    );
                }
                frame.render_widget(block, block_area);
                Ok(())
            },
            ctx,
        )?;

        if ctx.config.theme.modal_backdrop && !self.modals.is_empty() {
            let buffer = frame.buffer_mut();
            buffer.set_style(*buffer.area(), Style::default().fg(Color::DarkGray));
        }

        for modal in &mut self.modals {
            modal.render(frame, ctx)?;
        }

        Ok(())
    }

    pub fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        // Track the pointer for the mouseover effects. Terminals report the
        // pointer leaving the window as a move to (65535, 65535): treat any
        // far out-of-bounds coordinate as "no hover". While a modal is open
        // the *pane* position is suppressed (the popup must not highlight
        // the UI behind it) and the popup gets its own position for its own
        // hover effects.
        let outside = event.x >= u16::MAX - 100 || event.y >= u16::MAX - 100;
        let pos = (!outside).then_some(Position { x: event.x, y: event.y });
        if self.modals.is_empty() {
            ctx.set_mouse_pos(pos);
            ctx.set_modal_mouse_pos(None);
        } else {
            ctx.set_mouse_pos(None);
            ctx.set_modal_mouse_pos(pos);
        }

        // A pure pointer move carries no interaction: update the hover
        // position and re-render so the mouseover effects follow the cursor.
        if matches!(event.kind, MouseEventKind::Moved) {
            ctx.render()?;
            return Ok(());
        }

        // Right-click backs out of the top modal, like Esc (the Close
        // action). Modals that use right-click for their own actions opt out
        // via Modal::right_click_closes.
        if matches!(event.kind, MouseEventKind::RightClick) {
            if let Some(modal) = self.modals.last() {
                if modal.right_click_closes() {
                    let id = modal.id();
                    ctx.app_event_sender
                        .send(crate::AppEvent::UiEvent(UiAppEvent::PopModal(id)))?;
                    return Ok(());
                }
            }
        }

        if let Some(ref mut modal) = self.modals.last_mut() {
            modal.handle_mouse_event(event, ctx)?;
            return Ok(());
        }

        self.layout.for_each_pane(
            self.area,
            self.area.height,
            &mut |pane, _, _, _, _| {
                match self.panes.get_mut(&pane.pane, ctx)? {
                    Panes::TabContent => {
                        active_tab_call!(self, ctx, handle_mouse_event(event, ctx))?;
                    }
                    mut pane_instance => {
                        pane_call!(pane_instance, handle_mouse_event(event, ctx))?;
                    }
                }
                Ok(())
            },
            ctx,
        )?;

        ctx.render()?;

        Ok(())
    }

    pub fn handle_action(
        &mut self,
        key: &mut ActionEvent,
        ctx: &mut Ctx,
    ) -> Result<KeyHandleResult> {
        if let Some(ref mut modal) = self.modals.last_mut() {
            modal.handle_key(key, ctx)?;
            return Ok(KeyHandleResult::None);
        }

        active_tab_call!(self, ctx, handle_action(key, ctx))?;

        // Esc is bound to both Close (menu) and ShowSettings. When no modal
        // is open the focused pane may have consumed the Close half (e.g.
        // clearing selection marks), which would block the global half, so
        // fire the settings half explicitly — unless the pane fully
        // consumed the keypress (first Esc clears the selection; a second
        // Esc, with nothing selected, opens the settings).
        if self.modals.is_empty()
            && !key.is_consumed()
            && key.actions.iter().any(|a| matches!(a, Actions::Global(GlobalAction::ShowSettings)))
        {
            let modal = modals::settings::SettingsModal::new(&*ctx);
            modal!(ctx, modal);
            return Ok(KeyHandleResult::None);
        }

        if let Some(action) = key.claim_global() {
            // While an mpv video is the UI source, the transport keys drive
            // mpv (when MPD playback took over, they drive MPD again).
            if crate::core::mpv::mpv_is_ui_source(ctx)
                && let Some(socket) = ctx.mpv.socket.clone()
            {
                let routed = match action {
                    GlobalAction::TogglePause => {
                        crate::core::mpv::mpv_toggle_pause(&socket);
                        true
                    }
                    GlobalAction::Stop => {
                        crate::core::mpv::mpv_quit(&socket);
                        true
                    }
                    GlobalAction::SeekForward => {
                        crate::core::mpv::mpv_seek(&socket, ctx.mpv.position + 5.0);
                        true
                    }
                    GlobalAction::SeekBack => {
                        crate::core::mpv::mpv_seek(&socket, (ctx.mpv.position - 5.0).max(0.0));
                        true
                    }
                    GlobalAction::SeekToStart => {
                        crate::core::mpv::mpv_seek(&socket, 0.0);
                        true
                    }
                    _ => false,
                };
                if routed {
                    ctx.render()?;
                    return Ok(KeyHandleResult::None);
                }
            }
            match action {
                GlobalAction::Partition { name: Some(name), autocreate } => {
                    let name = name.clone();
                    let autocreate = *autocreate;
                    ctx.command(move |client| {
                        match client.switch_to_partition(&name) {
                            Ok(()) => {}
                            Err(MpdError::Mpd(MpdFailureResponse {
                                code: ErrorCode::NoExist,
                                ..
                            })) if autocreate => {
                                client.new_partition(&name)?;
                                client.switch_to_partition(&name)?;
                            }
                            err @ Err(_) => err?,
                        }
                        Ok(())
                    });
                }
                GlobalAction::Partition { name: None, .. } => {
                    let result = ctx.query_sync(move |client| {
                        let partitions = client.list_partitions()?;
                        Ok(partitions.0)
                    })?;
                    let modal = MenuModal::new(ctx)
                        .width(60)
                        .list_section(ctx, |section| {
                            if ctx.status.partition == "default" {
                                None
                            } else {
                                let section = section.item("Switch to default partition", |ctx| {
                                    ctx.command(move |client| {
                                        client.switch_to_partition("default")?;
                                        Ok(())
                                    });
                                    Ok(())
                                });

                                Some(section)
                            }
                        })
                        .multi_section(ctx, |section| {
                            let mut section = section
                                .add_action("Switch", |ctx, label| {
                                    ctx.command(move |client| {
                                        client.switch_to_partition(&label)?;
                                        Ok(())
                                    });
                                })
                                .add_action("Delete", |ctx, label| {
                                    ctx.command(move |client| {
                                        client.delete_partition(&label)?;
                                        Ok(())
                                    });
                                });
                            let mut any_non_default = false;
                            for partition in result
                                .iter()
                                .filter(|p| *p != "default" && **p != ctx.status.partition)
                            {
                                section = section.add_item(partition);
                                any_non_default = true;
                            }

                            if any_non_default { Some(section) } else { None }
                        })
                        .input_section(ctx, "New partition:", |section| {
                            let section = section.action(|ctx, value| {
                                if !value.is_empty() {
                                    ctx.command(move |client| {
                                        client.send_start_cmd_list()?;
                                        client.send_new_partition(&value)?;
                                        client.send_switch_to_partition(&value)?;
                                        client.send_execute_cmd_list()?;
                                        client.read_ok()?;
                                        Ok(())
                                    });
                                }
                            });
                            Some(section)
                        })
                        .list_section(ctx, |section| Some(section.item("Cancel", |_ctx| Ok(()))))
                        .build();

                    modal!(ctx, modal);
                }
                GlobalAction::Command { command, .. } => {
                    let cmd = command.parse();
                    log::debug!("executing {cmd:?}");

                    if let Ok(Args { command: Some(cmd), .. }) = cmd
                        && ctx.work_sender.send(WorkRequest::Command(cmd)).is_err()
                    {
                        log::error!("Failed to send command");
                    }
                }
                GlobalAction::CommandMode => {
                    let modal =
                        InputModal::new(ctx).title("Execute a command").on_confirm(|ctx, value| {
                            match Args::parse_cli_line(value) {
                                Ok(Args {
                                    command:
                                        Some(Command::SearchYt {
                                            query,
                                            provider,
                                            interactive,
                                            limit,
                                            position,
                                        }),
                                    ..
                                }) => {
                                    let kind: YtDlpHost = provider.into();

                                    let info_msg = format!("Searching '{query}' on {kind}");
                                    let send_result = ctx.work_sender.send(WorkRequest::SearchYt {
                                        query,
                                        kind,
                                        limit,
                                        interactive,
                                        position,
                                    });

                                    match send_result {
                                        Ok(()) => {
                                            status_info!("{info_msg}");
                                        }
                                        Err(err) => {
                                            log::error!("Failed to send SearchYt work: {err}");
                                        }
                                    }

                                    Ok(())
                                }

                                Ok(Args {
                                    command: Some(Command::AddYt { url, position }),
                                    ..
                                }) => {
                                    let send_result =
                                        ctx.ytdlp_manager.download_url(&url, position);
                                    match send_result {
                                        Ok(()) => {
                                            if ctx.config.auto_open_downloads {
                                                modal!(ctx, DownloadsModal::new(ctx));
                                            }
                                        }
                                        Err(err) => {
                                            status_error!(err:?; "Failed to queue yt-dlp download");
                                        }
                                    }
                                    Ok(())
                                }

                                Ok(Args { command: Some(cmd), .. }) => {
                                    if ctx.work_sender.send(WorkRequest::Command(cmd)).is_err() {
                                        log::error!("Failed to send command");
                                    }
                                    Ok(())
                                }

                                Ok(_) => {
                                    log::warn!("No subcommand provided");
                                    Ok(())
                                }

                                Err(e) => {
                                    log::error!("Parse error: {e}");
                                    Ok(())
                                }
                            }
                        });
                    modal!(ctx, modal);
                }
                GlobalAction::NextTrack if ctx.status.state != State::Stop => {
                    let keep_state = ctx.config.keep_state_on_song_change;
                    let state = ctx.status.state;
                    ctx.command(move |client| {
                        client.next_keep_state(keep_state, state)?;
                        Ok(())
                    });
                }
                GlobalAction::PreviousTrack if ctx.status.state != State::Stop => {
                    let rewind_to_start = ctx.config.rewind_to_start_sec;
                    let elapsed_sec = ctx.status.elapsed.as_secs();
                    let keep_state = ctx.config.keep_state_on_song_change;
                    let state = ctx.status.state;
                    ctx.command(move |client| {
                        match rewind_to_start {
                            Some(value) => {
                                if elapsed_sec >= value {
                                    client.seek_current(ValueChange::Set(0))?;
                                } else {
                                    client.prev_keep_state(keep_state, state)?;
                                }
                            }
                            None => {
                                client.prev_keep_state(keep_state, state)?;
                            }
                        }
                        Ok(())
                    });
                }
                GlobalAction::Stop if matches!(ctx.status.state, State::Play | State::Pause) => {
                    ctx.command(move |client| {
                        client.stop()?;
                        Ok(())
                    });
                }
                GlobalAction::ToggleRepeat => {
                    let repeat = !ctx.status.repeat;
                    ctx.command(move |client| {
                        client.repeat(repeat)?;
                        Ok(())
                    });
                }
                GlobalAction::ToggleRandom => {
                    let random = !ctx.status.random;
                    ctx.command(move |client| {
                        client.random(random)?;
                        Ok(())
                    });
                }
                GlobalAction::ToggleSingle => {
                    let single = ctx.status.single;
                    ctx.command(move |client| {
                        if client.version() < Version::new(0, 21, 0) {
                            client.single(single.cycle_skip_oneshot())?;
                        } else {
                            client.single(single.cycle())?;
                        }
                        Ok(())
                    });
                }
                GlobalAction::ToggleConsume => {
                    let consume = ctx.status.consume;
                    ctx.command(move |client| {
                        if client.version() < Version::new(0, 24, 0) {
                            client.consume(consume.cycle_skip_oneshot())?;
                        } else {
                            client.consume(consume.cycle())?;
                        }
                        Ok(())
                    });
                }
                GlobalAction::ToggleSingleOnOff => {
                    let single = ctx.status.single;
                    ctx.command(move |client| {
                        client.single(single.cycle_skip_oneshot())?;
                        Ok(())
                    });
                }
                GlobalAction::ToggleConsumeOnOff => {
                    let consume = ctx.status.consume;
                    ctx.command(move |client| {
                        client.consume(consume.cycle_skip_oneshot())?;
                        Ok(())
                    });
                }
                GlobalAction::TogglePause => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.pause_toggle()?;
                            Ok(())
                        });
                    } else {
                        ctx.command(move |client| {
                            client.play()?;
                            Ok(())
                        });
                    }
                }
                GlobalAction::VolumeUp => {
                    let step = ctx.config.volume_step;
                    ctx.command(move |client| {
                        client.volume(ValueChange::Increase(step.into()))?;
                        Ok(())
                    });
                }
                GlobalAction::VolumeDown => {
                    let step = ctx.config.volume_step;
                    ctx.command(move |client| {
                        client.volume(ValueChange::Decrease(step.into()))?;
                        Ok(())
                    });
                }
                GlobalAction::CrossfadeUp => {
                    let current_xfade = ctx.status.xfade.unwrap_or(0);
                    let new_xfade = current_xfade.saturating_add(1);
                    ctx.command(move |client| {
                        client.crossfade(new_xfade)?;
                        Ok(())
                    });
                }
                GlobalAction::CrossfadeDown => {
                    let current_xfade = ctx.status.xfade.unwrap_or(0);
                    let new_xfade = current_xfade.saturating_sub(1);
                    ctx.command(move |client| {
                        client.crossfade(new_xfade)?;
                        Ok(())
                    });
                }
                GlobalAction::SeekForward
                    if matches!(ctx.status.state, State::Play | State::Pause) =>
                {
                    ctx.command(move |client| {
                        client.seek_current(ValueChange::Increase(5))?;
                        Ok(())
                    });
                }
                GlobalAction::SeekBack
                    if matches!(ctx.status.state, State::Play | State::Pause) =>
                {
                    ctx.command(move |client| {
                        client.seek_current(ValueChange::Decrease(5))?;
                        Ok(())
                    });
                }
                GlobalAction::SeekToStart
                    if matches!(ctx.status.state, State::Play | State::Pause) =>
                {
                    ctx.command(move |client| {
                        client.seek_current(ValueChange::Set(0))?;
                        Ok(())
                    });
                }
                GlobalAction::Update => {
                    ctx.command(move |client| {
                        client.update(None)?;
                        Ok(())
                    });
                }
                GlobalAction::Rescan => {
                    ctx.command(move |client| {
                        client.rescan(None)?;
                        Ok(())
                    });
                }
                GlobalAction::NextTab => {
                    self.change_tab(ctx.config.next_screen(&ctx.active_tab), ctx)?;
                    ctx.render()?;
                }
                GlobalAction::PreviousTab => {
                    self.change_tab(ctx.config.prev_screen(&ctx.active_tab), ctx)?;
                    ctx.render()?;
                }
                // Round 28b: Shift+Tab toggles the MPD tab's Library/Search
                // mode — the Directories pane claims it while focused;
                // anywhere else it is a no-op (Tab/E/Q cycle tabs).
                GlobalAction::ToggleMpdMode => {}
                GlobalAction::SwitchToTab(name) => {
                    if ctx.config.tabs.names.contains(name) && !ctx.config.is_tab_hidden(name) {
                        self.change_tab(name.clone(), ctx)?;
                        ctx.render()?;
                    } else {
                        status_error!(
                            "Tab with name '{}' does not exist. Check your configuration.",
                            name
                        );
                    }
                }
                GlobalAction::NextTrack => {}
                GlobalAction::PreviousTrack => {}
                GlobalAction::Stop => {}
                GlobalAction::SeekBack => {}
                GlobalAction::SeekForward => {}
                GlobalAction::SeekToStart => {}
                GlobalAction::ExternalCommand { command, .. } => {
                    run_external(command.clone(), create_env(ctx, std::iter::empty::<&str>()));
                }
                GlobalAction::Quit => return Ok(KeyHandleResult::Quit),
                GlobalAction::ShowHelp => {
                    let modal = TabHelpModal::new(&*ctx);
                    modal!(ctx, modal);
                }
                GlobalAction::ShowSettings => {
                    let modal = modals::settings::SettingsModal::new(&*ctx);
                    modal!(ctx, modal);
                }
                GlobalAction::ShowOutputs => {
                    let current_partition = ctx.status.partition.clone();
                    ctx.query().id(OPEN_OUTPUTS_MODAL).replace_id(OPEN_OUTPUTS_MODAL).query(
                        move |client| {
                            let outputs = client.list_partitioned_outputs(&current_partition)?;
                            Ok(MpdQueryResult::Outputs(outputs))
                        },
                    );
                }
                GlobalAction::ShowDecoders => {
                    ctx.query()
                        .id(OPEN_DECODERS_MODAL)
                        .replace_id(OPEN_DECODERS_MODAL)
                        .query(|client| Ok(MpdQueryResult::Decoders(client.decoders()?.0)));
                }
                GlobalAction::ShowCurrentSongInfo => {
                    if let Some((_, current_song)) = ctx.find_current_song_in_queue() {
                        modal!(
                            ctx,
                            InfoListModal::builder()
                                .rows(current_song)
                                .title("Song info")
                                .column_widths(&[30, 70])
                                .build()
                        );
                    } else {
                        status_info!("No song is currently playing");
                    }
                }
                GlobalAction::AddRandom => {
                    modal!(ctx, AddRandomModal::new(ctx));
                }
                GlobalAction::ShowDownloads => {
                    modal!(ctx, DownloadsModal::new(ctx));
                }
            }
        } else if let Some(action) = key.claim_common() {
            #[allow(
                clippy::collapsible_match,
                reason = "Future expansion, remove when adding other actions"
            )]
            match action {
                CommonAction::Rate { kind, current: true, min_rating, max_rating } => {
                    if let Some((_, song)) = ctx.find_current_song_in_queue() {
                        match kind {
                            RateKind::Modal { values, custom, like } => {
                                let items = vec![Enqueue::File { path: song.file.clone() }];
                                modal!(
                                    ctx,
                                    create_rating_modal(
                                        items,
                                        values.as_slice(),
                                        *min_rating,
                                        *max_rating,
                                        *custom,
                                        *like,
                                        ctx
                                    )
                                );
                            }
                            RateKind::Value(value) => {
                                let uri = song.file.clone();
                                let value = value.to_string();
                                ctx.command(move |client| {
                                    client.set_sticker(&uri, RATING_STICKER, &value)?;
                                    Ok(())
                                });
                            }
                            RateKind::Like() => {
                                let uri = song.file.clone();
                                ctx.command(move |client| {
                                    client.set_sticker(&uri, LIKE_STICKER, "2")?;
                                    Ok(())
                                });
                            }
                            RateKind::Dislike() => {
                                let uri = song.file.clone();
                                ctx.command(move |client| {
                                    client.set_sticker(&uri, LIKE_STICKER, "0")?;
                                    Ok(())
                                });
                            }
                            RateKind::Neutral() => {
                                let uri = song.file.clone();
                                ctx.command(move |client| {
                                    client.set_sticker(&uri, LIKE_STICKER, "1")?;
                                    Ok(())
                                });
                            }
                        }
                    } else {
                        status_error!("No song is currently playing");
                    }
                }
                _ => {}
            }
        }

        Ok(KeyHandleResult::None)
    }

    /// Forward a raw key event to the top modal (used for key-capture while
    /// remapping). Returns true when the key was consumed.
    pub fn handle_raw_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        ctx: &mut Ctx,
    ) -> Result<bool> {
        // While the seekbar owns the keyboard (Ctrl+Tab on the Queue tab)
        // it consumes the navigation / seek keys before the resolver sees
        // them; everything else falls through to the normal keybinds. It
        // needs the real Release events to resolve tap vs hold.
        if seekbar::is_focused(ctx) {
            if seekbar::handle_key(ctx, key) {
                return Ok(true);
            }
            // An unhandled Release (e.g. a modifier key) must never reach
            // the resolver: it would double-trigger the press.
            if key.kind == crossterm::event::KeyEventKind::Release {
                return Ok(true);
            }
        }
        // With the kitty keyboard protocol the terminal reports key
        // releases; they never carry an action outside the seekbar. Swallow
        // them before the modal / resolver dispatch so nothing
        // double-triggers on press + release (e.g. the settings key-capture
        // view must not bind a key twice).
        if key.kind == crossterm::event::KeyEventKind::Release {
            return Ok(true);
        }
        if let Some(ref mut modal) = self.modals.last_mut() {
            return modal.handle_raw_key(key, ctx);
        }
        Ok(false)
    }

    pub fn handle_insert_mode(
        &mut self,
        action: Option<&mut ActionEvent>,
        buf: &[Key],
        ctx: &mut Ctx,
    ) -> Result<()> {
        if let Some(action) = action {
            // We got some resolved keybind in insert mode. Currently only Confirm and Close
            // are possible to be bound there so this is fine.
            let kind = match action.claim_common() {
                Some(CommonAction::Confirm) => InputResultEvent::Confirm,
                Some(CommonAction::Close) => InputResultEvent::Cancel,
                other => {
                    log::error!(other:?; "Expected Confirm or Close action in insert mode");
                    return Ok(());
                }
            };

            if let Some(ref mut modal) = self.modals.last_mut() {
                modal.handle_insert_mode(kind, ctx)?;
            } else {
                active_tab_call!(self, ctx, handle_insert_mode(kind, ctx))?;
            }

            ctx.input.normal_mode();
        } else {
            // Resolve each buffered key individually
            for key in buf {
                if let Some(kind) = ctx.input.handle_input(InputEvent::from_key_event(*key)) {
                    if let Some(ref mut modal) = self.modals.last_mut() {
                        modal.handle_insert_mode(kind, ctx)?;
                    } else {
                        active_tab_call!(self, ctx, handle_insert_mode(kind, ctx))?;
                    }
                }
            }
        }

        ctx.render()?;
        Ok(())
    }

    pub fn before_show(&mut self, area: Rect, ctx: &mut Ctx) -> Result<()> {
        self.calc_areas(area, ctx);

        self.layout.for_each_pane(
            self.area,
            self.area.height,
            &mut |pane, pane_area, _, _, _| {
                // Panes hidden by the Settings toggles (or the cava rules:
                // the Jellyfin tab always, the Queue tab while a video
                // plays) stay hidden — re-showing them here would restart
                // their terminal-side overlays (the cava bars) over the UI.
                if ctx.is_pane_hidden(&pane.pane) {
                    return Ok(());
                }
                match self.panes.get_mut(&pane.pane, ctx)? {
                    Panes::TabContent => {
                        active_tab_call!(self, ctx, before_show(pane_area, ctx))?;
                    }
                    mut pane_instance => {
                        pane_call!(pane_instance, calculate_areas(pane_area, ctx))?;
                        pane_call!(pane_instance, before_show(ctx))?;
                    }
                }
                Ok(())
            },
            ctx,
        )
    }

    pub fn on_ui_app_event(&mut self, event: UiAppEvent, ctx: &mut Ctx) -> Result<()> {
        match event {
            UiAppEvent::Modal(modal) => {
                let existing_modal = modal.replacement_id().and_then(|id| {
                    self.modals
                        .iter_mut()
                        .find(|m| m.replacement_id().as_ref().is_some_and(|m_id| *m_id == id))
                });

                if let Some(existing_modal) = existing_modal {
                    *existing_modal = modal;
                } else {
                    self.modals.push(modal);
                }

                // A modal owns the keyboard now; the seekbar must not keep
                // intercepting keys.
                seekbar::clear(ctx);

                // A modal is now on top: the panes behind it must not show
                // a hover highlight from the position that opened it (e.g.
                // the right-click that raised the context menu).
                ctx.set_mouse_pos(None);
                ctx.set_modal_mouse_pos(None);

                self.on_event(UiEvent::ModalOpened, ctx)?;
                ctx.render()?;
            }
            UiAppEvent::PopConfigErrorModal => {
                let original_len = self.modals.len();
                self.modals
                    .retain(|m| m.replacement_id().is_none_or(|id| id != ERROR_CONFIG_MODAL_ID));
                let new_len = self.modals.len();
                if new_len == 0 {
                    self.on_event(UiEvent::ModalClosed, ctx)?;
                }
                if original_len != new_len {
                    ctx.render()?;
                }
            }
            UiAppEvent::PopModal(id) => {
                let original_len = self.modals.len();
                self.modals.retain(|m| m.id() != id);
                let new_len = self.modals.len();
                if new_len == 0 {
                    self.on_event(UiEvent::ModalClosed, ctx)?;
                }
                if original_len != new_len {
                    ctx.render()?;
                }
            }
            UiAppEvent::ChangeTab(tab_name) => {
                self.change_tab(tab_name, ctx)?;
                ctx.render()?;
            }
            UiAppEvent::ApplySettings(staged) => {
                let mut config = ctx.config.as_ref().clone();

                // UI show/hide toggles (album art, lyrics, cava, radio tab).
                config.ui = staged.ui;

                // Video playback mode: apply + persist to state.ron.
                config.video.playback = staged.video_playback;
                // mpv audio language + subtitle preference + SVP support:
                // apply + persist.
                config.mpv.audio_lang = staged.mpv_audio_lang.clone();
                config.mpv.subtitles = staged.mpv_subtitles.clone();
                config.mpv.svp = staged.mpv_svp;
                let mut state = crate::config::state::AppStateFile::load();
                state.video_playback = Some(staged.video_playback.as_str().to_owned());
                state.mpv_audio_lang = Some(staged.mpv_audio_lang.as_str().to_owned());
                state.mpv_subtitles = Some(staged.mpv_subtitles.as_str());
                state.mpv_svp = Some(staged.mpv_svp);
                // The UI toggles (incl. auto chapters + library playlist
                // files) and the appearance colors are not part of the
                // config.ron schema; persist them here so a restart keeps
                // them (startup re-applies state.ui onto the config).
                state.ui = Some(staged.ui);
                // The mpDris2 track-change notification toggle: sync it to
                // the bridge state file so the s2u-mpdris2 shim applies it
                // live (no mpDris2 service restart).
                crate::shared::mpdris2::write_notify_state(
                    ctx.config.cache_dir.as_deref(),
                    staged.ui.mpdris2_notifications,
                );
                state.appearance = Some(modals::settings::persisted_appearance(&config));
                // rqbit SOCKS5 proxy (Settings -> torrent): applied to the
                // engine config (the next engine spawn routes through it)
                // and persisted to state.ron.
                config.torrent.socks_proxy = if staged.torrent_socks_proxy.trim().is_empty() {
                    None
                } else {
                    Some(staged.torrent_socks_proxy.clone())
                };
                state.torrent_socks_proxy = Some(staged.torrent_socks_proxy.clone());
                if let Err(err) = state.save() {
                    status_warn!("Failed to save state: {err}");
                }

                // Jellyfin credentials from a successful sign-in: persist to
                // the sidecar (preferred over jellytui's config).
                if let Some(creds) = &staged.jellyfin {
                    let path = crate::config::jellyfin::jellyfin_sidecar_write_path();
                    let content =
                        ron::ser::to_string_pretty(creds, ron::ser::PrettyConfig::default());
                    match content {
                        Ok(content) => {
                            if let Some(parent) = path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            if let Err(err) = std::fs::write(&path, content) {
                                status_warn!("Failed to save jellyfin credentials: {err}");
                            } else {
                                status_info!("Jellyfin credentials saved to {}", path.display());
                            }
                        }
                        Err(err) => {
                            status_warn!("Failed to serialize jellyfin credentials: {err}");
                        }
                    }
                }

                // Cava settings: persist the sidecar, then apply to the
                // runtime config.
                if let Err(err) = staged.cava.save() {
                    status_warn!("Failed to save cava settings: {err}");
                }
                staged.cava.apply_to(&mut config.cava);

                // Staged appearance colors.
                for (target, staged_color) in
                    modals::settings::AppearanceTarget::all().iter().zip(staged.appearance)
                {
                    let color = match staged_color {
                        modals::settings::StagedColor::Unchanged => continue,
                        modals::settings::StagedColor::Transparent => None,
                        modals::settings::StagedColor::Set(color) => Some(color),
                    };
                    modals::settings::set_appearance_color(&mut config.theme, *target, color);
                }

                // A "UI colors" edit drives the dependent accents (borders,
                // selection, cava bars, seekbar, controls) the same way the
                // blur watcher derives them from text_color.
                if !matches!(
                    staged.appearance[modals::settings::AppearanceTarget::Ui as usize],
                    modals::settings::StagedColor::Unchanged
                ) {
                    crate::config::derive_theme_accents(&mut config.theme);
                }

                ctx.config = std::sync::Arc::new(config);
                self.ui_settings = ctx.config.ui;

                // Key remaps applied while the panel was open: persist them
                // to the sidecar (the runtime keybinds were already updated
                // live so the table showed the new keys).
                for remap in &staged.remaps {
                    if let Err(err) = modals::remap_keys::save_override(
                        remap.section,
                        &remap.action,
                        &remap.new_key,
                        &remap.old_keys,
                    ) {
                        log::error!(error:? = err; "Failed to persist key remap");
                    }
                }

                // If the Radio tab was just disabled and it was the active
                // tab, switch away so the UI does not land on a hidden tab.
                if !ctx.config.ui.show_radio_tab
                    && ctx.active_tab.as_str().eq_ignore_ascii_case("Radio")
                {
                    let fallback = ctx
                        .config
                        .tabs
                        .names
                        .iter()
                        .find(|name| !ctx.config.is_tab_hidden(name))
                        .cloned()
                        .or_else(|| ctx.config.tabs.names.first().cloned());
                    if let Some(fallback) = fallback {
                        self.change_tab(fallback, ctx)?;
                    }
                }
                // Same for the Jellyfin tab.
                if !ctx.config.ui.show_jellyfin_tab
                    && ctx.active_tab.as_str().eq_ignore_ascii_case("Jellyfin")
                {
                    let fallback = ctx
                        .config
                        .tabs
                        .names
                        .iter()
                        .find(|name| !ctx.config.is_tab_hidden(name))
                        .cloned()
                        .or_else(|| ctx.config.tabs.names.first().cloned());
                    if let Some(fallback) = fallback {
                        self.change_tab(fallback, ctx)?;
                    }
                }

                status_info!("Settings saved");
                self.on_event(UiEvent::ConfigChanged, ctx)?;
                ctx.render()?;
            }
            UiAppEvent::DiscardSettings { keybinds } => {
                // The panel mutates the runtime keybinds live (so the table
                // shows new keys); restore the snapshot taken when it opened.
                let mut config = ctx.config.as_ref().clone();
                config.keybinds = keybinds;
                ctx.config = std::sync::Arc::new(config);
                ctx.key_resolver = KeyResolver::new(&ctx.config);
                ctx.render()?;
            }
            UiAppEvent::MpvItemChanged { item_id, title } => {
                ctx.mpv.item_id = (!item_id.is_empty()).then_some(item_id.clone());
                ctx.mpv.item = None;
                ctx.mpv.title = title;
                ctx.mpv.pending_seek = None;
                ctx.mpv.art_path = None;
                // A new video must never show the previous one's poster in
                // the media controls until its own art is fetched.
                crate::ui::modals::paste::clear_mpv_mpris_art(&ctx);
                // The new item's chapters (Queue Chapters view) and MPRIS
                // metadata/poster are fetched in the background; the album
                // art (Jellyfin image or resolved YouTube thumbnail) is
                // refreshed below.
                if !item_id.is_empty() {
                    let _ = ctx.work_sender.send(
                        crate::shared::events::WorkRequest::FetchJellyfinMpris {
                            item_id: item_id.clone(),
                        },
                    );
                    let _ = ctx.work_sender.send(
                        crate::shared::events::WorkRequest::FetchJellyfinChapters {
                            item_id: item_id.clone(),
                        },
                    );
                }
                // The Queue tab follows the switched-to video (Chapters /
                // Video list).
                if let Err(err) = self.follow_video_session(ctx) {
                    log::error!(error:? = err; "Failed to follow the video session in the queue tab");
                }
                self.refresh_album_art(ctx)?;
                ctx.render()?;
            }
            UiAppEvent::ClearQueueMarked => {
                match self.panes.get_mut(&PaneType::Queue, ctx)? {
                    Panes::Queue(queue) => queue.clear_marked(),
                    _ => {}
                }
                ctx.render()?;
            }
            UiAppEvent::SeekbarReleaseCheck => {
                seekbar::on_release_check(ctx);
            }
            UiAppEvent::LyricsReleaseCheck => match self.panes.get_mut(&PaneType::Lyrics, ctx)? {
                Panes::Lyrics(lyrics) => lyrics.release_btn(ctx)?,
                _ => {}
            },
        }
        Ok(())
    }

    pub fn resize(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        log::trace!(area:?; "Terminal was resized");
        self.calc_areas(area, ctx);

        // Everything except cava first (the active tab's album art is
        // redrawn at the new size inside TabContent), cava last: the cava
        // bars are a terminal-side overlay drawn outside the ratatui
        // buffer, so restarting them last keeps them on top of the album
        // art instead of being covered by it.
        let mut cava_area = None;
        self.layout.for_each_pane(
            self.area,
            self.area.height,
            &mut |pane, pane_area, _, _, _| {
                if pane.pane == PaneType::Cava {
                    cava_area = Some(pane_area);
                    return Ok(());
                }
                match self.panes.get_mut(&pane.pane, ctx)? {
                    Panes::TabContent => {
                        active_tab_call!(self, ctx, resize(pane_area, ctx))?;
                    }
                    mut pane_instance => {
                        pane_call!(pane_instance, calculate_areas(pane_area, ctx))?;
                        pane_call!(pane_instance, resize(pane_area, ctx))?;
                    }
                }
                Ok(())
            },
            ctx,
        )?;
        if let Some(pane_area) = cava_area {
            self.panes.cava.calculate_areas(pane_area, ctx)?;
            self.panes.cava.resize(pane_area, ctx)?;
        }
        Ok(())
    }

    /// The Queue tab follows the playing video: its list switches to the
    /// Chapters view when the video has markers (and the setting allows
    /// it), else to the mpv playlist (Video view). Called when a video
    /// session starts (launch, reattach) and when the video's chapters
    /// arrive. A no-op while the video is not the active UI source (e.g.
    /// MPD playback has taken over and paused it).
    pub fn follow_video_session(&mut self, ctx: &Ctx) -> Result<()> {
        if !crate::core::mpv::mpv_is_ui_source(ctx) {
            return Ok(());
        }
        match self.panes.get_mut(&PaneType::Queue, ctx)? {
            Panes::Queue(queue) => queue.follow_playing_video(ctx),
            _ => {}
        }
        Ok(())
    }

    /// Draw any album art queued by the last `ImageResized` event, or heal
    /// the last drawn image when the frame's diff rewrote its placeholder
    /// cells. Called by the event loop *after* the frame's buffer flush
    /// (the flush would otherwise overwrite the overlay's terminal-side
    /// cells) with the flushed frame's buffer, so the facade can tell
    /// whether the art pane area actually changed.
    pub fn flush_album_art(&mut self, buffer: &ratatui::buffer::Buffer, ctx: &Ctx) -> Result<()> {
        self.panes.album_art.flush_pending_display(buffer, ctx)
    }

    /// Display any overlay queued by the last frame (the Jellyfin tab's
    /// poster). Called after the buffer flush so the flush cannot overwrite
    /// the overlay's terminal-side placeholder cells.
    pub fn flush_pending_overlays(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(tab) = self.tabs.get_mut(&ctx.active_tab) else {
            return Ok(());
        };
        let Some(pane) = tab
            .panes
            .panes_iter()
            .find(|p| p.pane == PaneType::Jellyfin { tree: TreeBrowserArgs::default() })
        else {
            return Ok(());
        };
        let mut pane = self.panes.get_mut(&pane.pane, ctx)?;
        if let Panes::Jellyfin(jellyfin) = &mut pane {
            jellyfin.flush_pending_poster(ctx);
        }
        Ok(())
    }

    /// Re-evaluate the album art source: the video session started/ended or
    /// switched, so the overlay should show the video's thumbnail (Jellyfin
    /// item image / resolved YouTube thumbnail) or fall back to the audio
    /// source (the current song's art, or the default). Only when the album
    /// art pane is actually visible — `show_default` draws synchronously
    /// and must never paint over another tab.
    pub fn refresh_album_art(&mut self, ctx: &Ctx) -> Result<()> {
        let visible =
            self.tabs.get(&ctx.active_tab).is_some_and(|tab| {
                tab.panes.panes_iter().any(|pane| pane.pane == PaneType::AlbumArt)
            }) || self.layout.panes_iter().any(|pane| pane.pane == PaneType::AlbumArt);
        if !visible || ctx.is_pane_hidden(&PaneType::AlbumArt) {
            return Ok(());
        }
        self.panes.album_art.before_show(ctx)
    }

    /// Hide the cava overlay (a video is playing and the tab's layout drops
    /// the cava row).
    pub(crate) fn hide_cava(&mut self, ctx: &Ctx) -> Result<()> {
        self.panes.cava.on_hide(ctx)?;
        // The visualizer's bars are a terminal-side overlay: clear the
        // whole window and repaint so no stale cells remain where the row
        // collapsed.
        self.request_cava_refresh();
        Ok(())
    }

    /// Bring the cava overlay back (the video ended and the tab shows the
    /// visualizer again).
    pub(crate) fn show_cava(&mut self, ctx: &Ctx) -> Result<()> {
        self.panes.cava.before_show(ctx)
    }

    /// Ask the event loop to clear the terminal and redraw the full UI
    /// (used when the cava row is removed so its overlay can't leave stale
    /// cells).
    pub(crate) fn request_cava_refresh(&mut self) {
        self.cava_refresh_pending = true;
    }

    /// Consume the full-refresh request (the event loop clears the
    /// terminal and forces two render passes).
    pub(crate) fn take_cava_refresh(&mut self) -> bool {
        std::mem::take(&mut self.cava_refresh_pending)
    }

    /// Fire any cava Start that was deferred so the bars paint *after* the
    /// UI frame (never before it). Called by the event loop right after a
    /// flushed frame.
    pub(crate) fn maybe_start_cava(&mut self, ctx: &Ctx) -> Result<()> {
        self.panes.cava.maybe_start(ctx)
    }

    pub fn on_event(&mut self, mut event: UiEvent, ctx: &mut Ctx) -> Result<()> {
        match event {
            UiEvent::Database => {
                ctx.input.clear_all_buffers();
                status_warn!(
                    "The music database has been updated. Some parts of the UI may have been reinitialized to prevent inconsistent behaviours."
                );
            }
            UiEvent::ConfigChanged => {
                // Re-apply the runtime show/hide toggles on top of the newly
                // loaded config.
                let mut config = ctx.config.as_ref().clone();
                config.ui = self.ui_settings;
                ctx.config = std::sync::Arc::new(config);

                // Call on_hide for all panes in the current tab and current layout because they
                // might not be visible after the change
                self.layout.for_each_pane(
                    self.area,
                    self.area.height,
                    &mut |pane, _, _, _, _| {
                        match self.panes.get_mut(&pane.pane, ctx)? {
                            Panes::TabContent => {
                                active_tab_call!(self, ctx, on_hide(ctx))?;
                            }
                            mut pane_instance => {
                                pane_call!(pane_instance, on_hide(ctx))?;
                            }
                        }
                        Ok(())
                    },
                    ctx,
                )?;

                self.layout = ctx.config.theme.layout.clone();
                let new_active_tab = ctx
                    .config
                    .tabs
                    .names
                    .iter()
                    .find(|tab| tab == &&ctx.active_tab)
                    .or(ctx.config.tabs.names.first())
                    .context("Expected at least one tab")?;

                let mut old_other_panes = std::mem::take(&mut self.panes.others);
                for (key, new_other_pane) in PaneContainer::init_other_panes(ctx) {
                    let old = old_other_panes.remove(&key);
                    self.panes.others.insert(key, old.unwrap_or(new_other_pane));
                }
                // We have to be careful about the order of operations here as they might cause
                // a panic if done incorrectly
                self.tabs = Self::init_tabs(ctx)?;
                ctx.active_tab = new_active_tab.clone();
                self.on_event(UiEvent::TabChanged(new_active_tab.clone()), ctx)?;

                // Call before_show here, because we have "hidden" all the panes before and this
                // will force them to reinitialize
                self.before_show(self.area, ctx)?;
            }
            _ => {}
        }

        for pane_type in &ctx.config.active_panes {
            let visible =
                !ctx.is_pane_hidden(pane_type)
                    && (self.tabs.get(&ctx.active_tab).is_some_and(|tab| {
                        tab.panes.panes_iter().any(|pane| pane.pane == *pane_type)
                    }) || self.layout.panes_iter().any(|pane| pane.pane == *pane_type));

            match self.panes.get_mut(pane_type, ctx)? {
                #[cfg(debug_assertions)]
                Panes::Logs(p) => p.on_event(&mut event, visible, ctx),
                Panes::Queue(p) => p.on_event(&mut event, visible, ctx),
                Panes::QueueHeader(p) => p.on_event(&mut event, visible, ctx),
                Panes::Directories(p) => p.on_event(&mut event, visible, ctx),
                Panes::Albums(p) => p.on_event(&mut event, visible, ctx),
                Panes::Artists(p) => p.on_event(&mut event, visible, ctx),
                Panes::Playlists(p) => p.on_event(&mut event, visible, ctx),
                Panes::Search(p) => p.on_event(&mut event, visible, ctx),
                Panes::Radio(p) => p.on_event(&mut event, visible, ctx),
                Panes::Jellyfin(p) => p.on_event(&mut event, visible, ctx),
                Panes::AlbumArtists(p) => p.on_event(&mut event, visible, ctx),
                Panes::AlbumArt(p) => p.on_event(&mut event, visible, ctx),
                Panes::Lyrics(p) => p.on_event(&mut event, visible, ctx),
                Panes::ProgressBar(p) => p.on_event(&mut event, visible, ctx),
                Panes::Header(p) => p.on_event(&mut event, visible, ctx),
                Panes::Tabs(p) => p.on_event(&mut event, visible, ctx),
                #[cfg(debug_assertions)]
                Panes::FrameCount(p) => p.on_event(&mut event, visible, ctx),
                Panes::Others(p) => p.on_event(&mut event, visible, ctx),
                Panes::Cava(p) => p.on_event(&mut event, visible, ctx),
                // Property and the dummy TabContent pane do not need to receive events
                Panes::Property(_) | Panes::TabContent => Ok(()),
                // Empty pane is a noop, no events
                Panes::Empty(_) => Ok(()),
            }?;
        }

        for modal in &mut self.modals {
            modal.on_event(&mut event, ctx)?;
        }

        Ok(())
    }

    pub(crate) fn on_command_finished(
        &mut self,
        id: &'static str,
        pane: Option<PaneType>,
        data: MpdQueryResult,
        ctx: &mut Ctx,
    ) -> Result<()> {
        match pane {
            Some(pane_type) => {
                let visible = !ctx.is_pane_hidden(&pane_type)
                    && (self.tabs.get(&ctx.active_tab).is_some_and(|tab| {
                        tab.panes.panes_iter().any(|pane| pane.pane == pane_type)
                    }) || self.layout.panes_iter().any(|pane| pane.pane == pane_type));

                match self.panes.get_mut(&pane_type, ctx)? {
                    #[cfg(debug_assertions)]
                    Panes::Logs(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Queue(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::QueueHeader(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Directories(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Albums(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Artists(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Playlists(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Search(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Radio(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Jellyfin(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::AlbumArtists(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::AlbumArt(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Lyrics(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::ProgressBar(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Header(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Tabs(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Others(p) => p.on_query_finished(id, data, visible, ctx),
                    #[cfg(debug_assertions)]
                    Panes::FrameCount(p) => p.on_query_finished(id, data, visible, ctx),
                    Panes::Cava(p) => p.on_query_finished(id, data, visible, ctx),
                    // Property and the dummy TabContent pane do not need to receive command
                    // notifications
                    Panes::Property(_) | Panes::TabContent => Ok(()),
                    // Empty pane is a noop, no commands
                    Panes::Empty(_) => Ok(()),
                }?;
            }
            None => match (id, data) {
                (OPEN_OUTPUTS_MODAL, MpdQueryResult::Outputs(outputs)) => {
                    modal!(ctx, OutputsModal::new(outputs));
                }
                (OPEN_DECODERS_MODAL, MpdQueryResult::Decoders(decoders)) => {
                    modal!(ctx, DecodersModal::new(decoders));
                }
                (FETCH_SONG_STICKERS, MpdQueryResult::SongStickers(stickers)) => {
                    for (k, v) in stickers {
                        // Assume all stickers were fetched for each song so simple replace is
                        // enough
                        ctx.set_song_stickers(k, v);
                    }
                    ctx.render()?;
                }
                (id, mut data) => {
                    // TODO a proper modal target
                    for modal in &mut self.modals {
                        modal.on_query_finished(id, &mut data, ctx)?;
                    }
                }
            },
        }

        Ok(())
    }
}

#[derive(Debug)]
pub enum UiAppEvent {
    Modal(Box<dyn Modal + Send + Sync>),
    PopModal(Id),
    PopConfigErrorModal,
    ChangeTab(TabName),
    /// The Settings panel was closed with Save: apply the staged UI
    /// toggles, cava overrides, appearance colors and key remaps, then
    /// re-init the layout.
    ApplySettings(modals::settings::StagedSettings),
    /// The Settings panel was closed with Discard: restore the runtime
    /// keybinds to the snapshot taken when the panel opened (the only thing
    /// the panel mutates live while open).
    DiscardSettings {
        keybinds: KeyConfig,
    },
    /// The queue's context-menu Remove deleted the marked items; drop the
    /// (now stale) marked selection.
    ClearQueueMarked,
    /// The running mpv session was redirected to another Jellyfin item (the
    /// play-another-file prompt): update the session's item id + title.
    MpvItemChanged {
        item_id: String,
        title: String,
    },
    /// The seekbar's release-check one-shot fired: treat the held Space as
    /// released (tap vs hold resolution on terminals without release
    /// events).
    SeekbarReleaseCheck,
    /// The lyrics header buttons' release-check one-shot fired: treat the
    /// held button as released (the pressed `⭘` marker reverts to `●` on
    /// terminals without mouse release events).
    LyricsReleaseCheck,
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum UiEvent {
    Player,
    QueueChanged,
    Database,
    Output,
    StoredPlaylist,
    LogAdded(Vec<u8>),
    ModalOpened,
    ModalClosed,
    Exit,
    LyricsIndexed,
    SongChanged,
    Reconnected,
    TabChanged(TabName),
    Displayed,
    Hidden,
    ConfigChanged,
    PlaybackStateChanged,
    ImageEncoded { data: EncodeData },
    ImageEncodeFailed { err: anyhow::Error },
    DownloadsUpdated,
}

impl TryFrom<IdleEvent> for UiEvent {
    type Error = ();

    fn try_from(event: IdleEvent) -> Result<Self, ()> {
        Ok(match event {
            IdleEvent::Player => UiEvent::Player,
            IdleEvent::Database => UiEvent::Database,
            IdleEvent::StoredPlaylist => UiEvent::StoredPlaylist,
            IdleEvent::Output => UiEvent::Output,
            _ => return Err(()),
        })
    }
}

pub enum KeyHandleResult {
    None,
    Quit,
}

impl From<&Level> for Color {
    fn from(value: &Level) -> Self {
        match value {
            Level::Info => Color::Blue,
            Level::Warn => Color::Yellow,
            Level::Error => Color::Red,
            Level::Debug => Color::LightGreen,
            Level::Trace => Color::Magenta,
        }
    }
}

impl Level {
    pub fn into_style(self, config: &LevelStyles) -> Style {
        match self {
            Level::Trace => config.trace,
            Level::Debug => config.debug,
            Level::Warn => config.warn,
            Level::Error => config.error,
            Level::Info => config.info,
        }
    }
}

impl Config {
    fn next_screen(&self, current_screen: &TabName) -> TabName {
        let visible: Vec<&TabName> =
            self.tabs.names.iter().filter(|name| !self.is_tab_hidden(name)).collect();
        if visible.is_empty() {
            return current_screen.clone();
        }
        let idx = visible.iter().position(|t| *t == current_screen).unwrap_or(0);
        visible[(idx + 1) % visible.len()].clone()
    }

    fn prev_screen(&self, current_screen: &TabName) -> TabName {
        let visible: Vec<&TabName> =
            self.tabs.names.iter().filter(|name| !self.is_tab_hidden(name)).collect();
        if visible.is_empty() {
            return current_screen.clone();
        }
        let idx = visible.iter().position(|t| *t == current_screen).unwrap_or(0);
        visible[(if idx == 0 { visible.len() - 1 } else { idx - 1 }) % visible.len()].clone()
    }

    fn as_border_style(&self) -> ratatui::style::Style {
        self.theme.borders_style
    }

    fn as_focused_border_style(&self) -> ratatui::style::Style {
        self.theme.highlight_border_style
    }

    fn as_text_style(&self) -> ratatui::style::Style {
        self.theme.text_color.map(|color| Style::default().fg(color)).unwrap_or_default()
    }

    /// The static text style for secondary list text (the queue's
    /// non-table text, the tab lists' sublines/details): the theme's
    /// configured text color, not the blur accent, so it keeps the
    /// configured look.
    fn as_list_text_style(&self) -> ratatui::style::Style {
        self.theme
            .list_text_color
            .or(self.theme.text_color)
            .map(|color| Style::default().fg(color))
            .unwrap_or_default()
    }

    /// The primary list text style (item names / rows) shared with the
    /// queue: explicit ANSI white, immune to the blur accent — the same
    /// white/grey palette as the queue's columns and info-box values, with
    /// the accent reserved for the selection highlight
    /// (`current_item_style`). Secondary text uses `as_list_text_style`.
    fn as_list_name_style(&self) -> ratatui::style::Style {
        Style::default().fg(Color::White)
    }

    /// The list text style for stream entries inside a playlist: the
    /// queue's grey text tinted dark blue, so online streams stand apart
    /// from local files (which keep the white rows).
    fn as_stream_text_style(&self) -> ratatui::style::Style {
        self.as_list_text_style().fg(Color::Blue)
    }

    fn as_styled_scrollbar(&self) -> Option<ratatui::widgets::Scrollbar<'_>> {
        let scrollbar = self.theme.scrollbar.as_ref()?;
        let symbols = &scrollbar.symbols;
        Some(
            ratatui::widgets::Scrollbar::default()
                .orientation(ratatui::widgets::ScrollbarOrientation::VerticalRight)
                .track_symbol(if symbols[0].is_empty() { None } else { Some(&symbols[0]) })
                .thumb_symbol(&scrollbar.symbols[1])
                .begin_symbol(if symbols[2].is_empty() { None } else { Some(&symbols[2]) })
                .end_symbol(if symbols[3].is_empty() { None } else { Some(&symbols[3]) })
                .track_style(scrollbar.track_style)
                .begin_style(scrollbar.ends_style)
                .end_style(scrollbar.ends_style)
                .thumb_style(scrollbar.thumb_style),
        )
    }

    /// Display widths of the begin/end arrow symbols (0 when disabled), so
    /// scrollbar mouse geometry lines up with the rendered widget.
    pub(crate) fn scrollbar_ends_width(&self) -> (u16, u16) {
        let Some(symbols) = self.theme.scrollbar.as_ref().map(|s| &s.symbols) else {
            return (0, 0);
        };
        (
            if symbols[2].is_empty() { 0 } else { symbols[2].width() as u16 },
            if symbols[3].is_empty() { 0 } else { symbols[3].width() as u16 },
        )
    }
}
