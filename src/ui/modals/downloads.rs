use std::path::PathBuf;

use anyhow::Result;
use bon::vec;
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    macros::{constraint, constraints},
    style::Style,
    symbols::border,
    widgets::{Block, Borders, Cell, Clear, ListItem, Row, Table, TableState},
};

use crate::{
    config::{
        keys::CommonAction,
        theme::properties::{Property, SongProperty},
    },
    shared::macros::{status_info, status_warn},
    ctx::Ctx,
    shared::{
        ext::rect::RectExt,
        id::{self, Id},
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::MpdClientExt,
        ytdlp::{DownloadId, DownloadState},
    },
    ui::{
        UiEvent,
        dirstack::{Dir, DirStackItem},
        modal,
        modals::{Modal, info_modal::InfoModal, menu::modal::MenuModal},
    },
};

#[derive(Debug)]
pub struct DownloadsModal {
    id: Id,
    queue: Dir<DownloadId, TableState>,
    table_area: Rect,
    /// Round 56.5: the daemon torrent jobs whose rows are appended to the
    /// unified `Id | Source | State` table (the yt-dlp rows first, then the
    /// torrent rows). One cursor spans the whole list; this id prefix is
    /// what tells the context menu that a selected row is a torrent row
    /// (and which daemon job it points at when the menu opens).
    torrent_jobs: Vec<String>,
}

impl Modal for DownloadsModal {
    fn id(&self) -> Id {
        self.id
    }

    // Right-click on a download row opens its context menu.
    fn right_click_closes(&self) -> bool {
        false
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        let popup_area = frame.area().centered(constraint!(==90), constraint!(==20));
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(Block::default().style(Style::default().bg(bg_color)), popup_area);
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(ctx.config.as_border_style())
            .title_alignment(ratatui::prelude::Alignment::Center)
            .title("Downloads");

        let table_area = block.inner(popup_area);

        // Round 56.5: ONE list for all downloads — the daemon torrent jobs
        // are ordinary rows of the same `Id | Source | State` table,
        // appended after the yt-dlp rows (`Source` = "Torrent").
        let mut rows: Vec<Row> = ctx.ytdlp_manager.map_values(|item| {
            Row::new([
                Cell::from(""), // marker
                Cell::from(item.inner.id.clone()),
                Cell::from(item.inner.kind.to_string()),
                Cell::from(item.state.to_string()).style(item.state.as_style(ctx)),
            ])
        });
        // Torrent rows come from `~/.cache/s2udio/downloads.json` (the
        // shared state the daemon writes; `Ctx.dl_state` is refreshed by
        // the 1 s `DlStatePoll`).
        self.torrent_jobs = ctx
            .dl_state
            .borrow()
            .as_ref()
            .map(|state| state.jobs.iter().map(|job| job.job_id.clone()).collect())
            .unwrap_or_default();
        {
            let jobs = ctx.dl_state.borrow();
            if let Some(state) = jobs.as_ref() {
                let daemon_offline = !crate::core::dlctl::daemon_running(state);
                for job in &state.jobs {
                    let name = if job.torrent_name.len() > 40 {
                        let (head, _) = job.torrent_name.split_at(40);
                        format!("{head}…")
                    } else {
                        job.torrent_name.clone()
                    };
                    // The round-54 status rendering, moved into the State
                    // column (round 56 (56-1): a finished download shows a
                    // distinct done row while the daemon keeps it in the
                    // grace window).
                    let progress = if job.status.active() {
                        format!("{:.0}%", job.progress_percent)
                    } else if job.status == crate::core::dlctl::DlStatus::Completed {
                        "done ✓ 100%".to_owned()
                    } else {
                        job.status.to_string()
                    };
                    let style = Self::torrent_status_style(job.status, ctx);
                    let status_text = if daemon_offline && job.status.active() {
                        format!("offline ({})", job.status)
                    } else {
                        progress
                    };
                    rows.push(Row::new([
                        Cell::from(""),
                        Cell::from(name),
                        Cell::from("Torrent"),
                        Cell::from(status_text).style(style),
                    ]));
                }
            }
        }

        let item_count = rows.len();
        // Round 56.7 (host fix): the modal must come up with a usable
        // selection whenever rows exist — with only terminal rows (no
        // active jobs, so the 1 s `DlStatePoll` guard never runs) the
        // `DownloadsUpdated` auto-select never fires, and Enter / the
        // context-menu key would do nothing until the user navigates.
        if item_count > 0 && self.queue.state.get_selected().is_none() {
            self.queue.state.select(Some(0), 0);
        }
        let table = Table::new(rows, constraints![==1, ==33%, ==33%, ==34%])
            .row_highlight_style(ctx.config.theme.current_item_style)
            .header(Row::new(["", "Id", "Source", "State"]));

        self.queue
            .state
            .set_content_and_viewport_len(item_count, table_area.height as usize);
        frame.render_stateful_widget(table, table_area, self.queue.state.as_render_state_ref());

        frame.render_widget(block, popup_area);
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && item_count > table_area.height.saturating_sub(1) as usize
        {
            frame.render_stateful_widget(
                scrollbar,
                popup_area.inner(Margin { horizontal: 0, vertical: 1 }),
                self.queue.state.as_scrollbar_state_ref(),
            );
        }

        self.table_area = table_area.shrink_from_top(1); // Subtract header height

        Ok(())
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::Down => {
                    self.queue.next(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    ctx.render()?;
                }
                CommonAction::Up => {
                    self.queue.prev(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    ctx.render()?;
                }
                CommonAction::Close => {
                    self.hide(ctx)?;
                }
                CommonAction::Confirm => {
                    if self.selected_is_torrent(ctx) {
                        self.create_torrent_menu(ctx);
                    } else {
                        self.create_menu(ctx);
                    }
                }
                CommonAction::DownHalf => {
                    self.queue.next_half_viewport(ctx.config.scrolloff);
                    ctx.render()?;
                }
                CommonAction::UpHalf => {
                    self.queue.prev_half_viewport(ctx.config.scrolloff);
                    ctx.render()?;
                }
                CommonAction::PageUp => {
                    self.queue.prev_viewport(ctx.config.scrolloff);
                    ctx.render()?;
                }
                CommonAction::PageDown => {
                    self.queue.next_viewport(ctx.config.scrolloff);
                    ctx.render()?;
                }
                CommonAction::Top => {
                    self.queue.first();
                    ctx.render()?;
                }
                CommonAction::Bottom => {
                    self.queue.last();
                    ctx.render()?;
                }
                CommonAction::Select => {}
                CommonAction::ShowInfo => {}

                _ => {}
            }
        }
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        if !self.table_area.contains(event.into()) {
            return Ok(());
        }

        let clicked_row: usize = event.y.saturating_sub(self.table_area.y).into();
        let Some(idx) = self.queue.state.get_at_rendered_row(clicked_row) else {
            return Ok(());
        };

        match event.kind {
            MouseEventKind::LeftClick => {
                self.queue.select_idx(idx, ctx.config.scrolloff);
                ctx.render()?;
            }
            MouseEventKind::DoubleClick | MouseEventKind::MiddleClick | MouseEventKind::RightClick => {
                self.queue.select_idx(idx, ctx.config.scrolloff);
                if idx >= ctx.ytdlp_manager.len() {
                    self.create_torrent_menu(ctx);
                } else {
                    self.create_menu(ctx);
                }
                ctx.render()?;
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                self.queue.scroll_up(ctx.config.scroll_amount, ctx.config.scrolloff);
                ctx.render()?;
            }
            MouseEventKind::Drag { drag_start_position: _ } => {}
            MouseEventKind::LeftRelease => {}
            MouseEventKind::Moved => {}
        }
        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::DownloadsUpdated => {
                self.queue.items = ctx.ytdlp_manager.ids();
                let total = self.queue.items.len() + self.torrent_jobs.len();
                if total == 0 {
                    self.queue.state.select(None, 0);
                } else if self
                    .queue
                    .state
                    .get_selected()
                    .is_none_or(|selected| selected >= total)
                {
                    self.queue.state.select(Some(0), 0);
                }
                ctx.render()?;
            }
            _ => {}
        }
        Ok(())
    }
}

impl DownloadsModal {
    pub fn new(ctx: &Ctx) -> Self {
        let mut queue = Dir::new(ctx.ytdlp_manager.ids());
        if !queue.items.is_empty() {
            queue.state.select(Some(0), 0);
        }

        Self {
            id: id::new(),
            queue,
            table_area: Rect::default(),
            torrent_jobs: Vec::new(),
        }
    }

    /// Whether the unified cursor is on a torrent row (the yt-dlp rows
    /// come first, so any index at or past their count is a torrent row).
    fn selected_is_torrent(&self, ctx: &Ctx) -> bool {
        let yt_count = ctx.ytdlp_manager.len();
        self.queue.state.get_selected().is_some_and(|idx| idx >= yt_count)
    }

    /// The torrent row's per-status style (round 54 mapping: error for
    /// failed, debug/success for completed, warn for active states, info
    /// for stopped).
    fn torrent_status_style(status: crate::core::dlctl::DlStatus, ctx: &Ctx) -> Style {
        match status {
            crate::core::dlctl::DlStatus::Failed => ctx.config.theme.level_styles.error,
            crate::core::dlctl::DlStatus::Completed => ctx.config.theme.level_styles.debug,
            crate::core::dlctl::DlStatus::Downloading
            | crate::core::dlctl::DlStatus::Adding
            | crate::core::dlctl::DlStatus::Moving
            | crate::core::dlctl::DlStatus::Queued => ctx.config.theme.level_styles.warn,
            crate::core::dlctl::DlStatus::Stopped => ctx.config.theme.level_styles.info,
        }
    }

    /// The torrent row's context menu (Confirm / double-click / right-click
    /// on a daemon job row): ACTIVE jobs get "Stop download" (the daemon
    /// forgets the torrent — partials stay — and drops the job); terminal
    /// jobs (`Completed`/`Stopped`/`Failed`) get "Remove from list"
    /// (round 56.6-2: the row leaves `downloads.json` + the modal; the
    /// downloaded files are never touched — a spooled `Remove` when the
    /// daemon runs, a direct state edit when it is dead). The selected row
    /// is mapped back to a daemon job via the yt-dlp row-count prefix; the
    /// job must still exist in `ctx.dl_state` when the menu opens.
    pub fn create_torrent_menu(&self, ctx: &mut Ctx) {
        let Some(selected) = self.queue.state.get_selected() else {
            return;
        };
        let yt_count = ctx.ytdlp_manager.len();
        let Some(job_id) = self.torrent_jobs.get(selected.saturating_sub(yt_count)).cloned() else {
            return;
        };
        let state_ref = ctx.dl_state.borrow();
        let Some(job) = state_ref
            .as_ref()
            .and_then(|state| state.jobs.iter().find(|job| job.job_id == job_id))
        else {
            return;
        };
        let active = job.status.active();
        drop(state_ref);
        let remove_job_id = job_id.clone();
        let modal = MenuModal::new(ctx)
            .list_section(ctx, |mut section| {
                if active {
                    section.add_item("Stop download", move |_| {
                        if let Err(err) =
                            crate::core::dlctl::write_stop_request(Some(&job_id), None, None)
                        {
                            status_warn!("{err}");
                        } else {
                            status_info!("Stopping download of {job_id}… (partials stay in the torrent cache)");
                        }
                        Ok(())
                    });
                } else {
                    // Round 56.6-2: remove the terminal row. The daemon
                    // path spools a `Remove`; with no daemon the TUI edits
                    // `downloads.json` directly (both atomic; the files are
                    // kept). Re-poll the state immediately so the row
                    // disappears from the modal, plus one shot 600 ms later
                    // — long enough for the daemon (spool scan ~300 ms) to
                    // consume a `Remove` request.
                    section.add_item("Remove from list", move |ctx| {
                        let daemon_running = ctx
                            .dl_state
                            .borrow()
                            .as_ref()
                            .is_some_and(crate::core::dlctl::daemon_running);
                        let result = if daemon_running {
                            crate::core::dlctl::write_remove_request(Some(&remove_job_id), None, None)
                        } else {
                            crate::core::dlctl::remove_job_offline(&remove_job_id)
                        };
                        if let Err(err) = result {
                            status_warn!("{err}");
                        } else {
                            status_info!("Removed {remove_job_id} from the downloads list (files kept)");
                            let _ = ctx
                                .app_event_sender
                                .send(crate::AppEvent::DlStatePoll);
                            ctx.scheduler.schedule(
                                std::time::Duration::from_millis(600),
                                move |(tx, _)| {
                                    let _ = tx.send(crate::AppEvent::DlStatePoll);
                                    Ok(())
                                },
                            );
                        }
                        Ok(())
                    });
                }
                Some(section)
            })
            .build();
        modal!(ctx, modal);
    }

    pub fn create_menu(&self, ctx: &mut Ctx) {
        if let Some((id, current)) =
            self.queue.selected().and_then(|id| ctx.ytdlp_manager.get(*id).map(|item| (id, item)))
        {
            // Round 56.6-3: EVERY state has a context menu —
            // Downloading can be cancelled; Completed / Failed / Canceled
            // / AlreadyDownloaded rows can be removed from the list (the
            // files are kept). Add/Retry/Logs/Requeue unchanged.
            let actions = match &current.state {
                DownloadState::Queued => vec![ContextAction::Cancel(*id)],
                DownloadState::Downloading => vec![ContextAction::Cancel(*id)],
                DownloadState::Completed { logs, path } => vec![
                    ContextAction::Remove(*id),
                    ContextAction::Add(path.clone()),
                    ContextAction::Logs(logs.clone()),
                ],
                DownloadState::Failed { logs } => vec![
                    ContextAction::Remove(*id),
                    ContextAction::Retry(*id),
                    ContextAction::Logs(logs.clone()),
                ],
                DownloadState::Canceled => {
                    vec![ContextAction::Remove(*id), ContextAction::Requeue(*id)]
                }
                DownloadState::AlreadyDownloaded { path } => {
                    vec![ContextAction::Remove(*id), ContextAction::Add(path.clone())]
                }
            };

            if actions.is_empty() {
                return;
            }

            let modal = MenuModal::new(ctx)
                .list_section(ctx, |mut section| {
                    for mut action in actions {
                        match action {
                            ContextAction::Cancel(id) => {
                                section.add_item(action.to_string(), move |ctx| {
                                    ctx.ytdlp_manager.cancel_download(id);
                                    Ok(())
                                });
                            }
                            ContextAction::Add(ref mut path) => {
                                let path = std::mem::take(path);
                                section.add_item(action.to_string(), move |ctx| {
                                    let cache_dir = ctx.config.cache_dir.clone();
                                    ctx.command(move |client| {
                                        client.add_downloaded_file_to_queue(
                                            path,
                                            cache_dir.as_deref(),
                                            None,
                                        )?;
                                        Ok(())
                                    });
                                    Ok(())
                                });
                            }
                            ContextAction::Requeue(id) => {
                                section.add_item(action.to_string(), move |ctx| {
                                    ctx.ytdlp_manager.redownload(id);
                                    Ok(())
                                });
                            }
                            ContextAction::Logs(ref mut logs) => {
                                let logs = std::mem::take(logs);
                                section.add_item(action.to_string(), move |ctx| {
                                    let modal = InfoModal::builder()
                                        .ctx(ctx)
                                        .title("Logs")
                                        .percent_width(80.0_f32)
                                        .message(logs)
                                        .replacement_id("download_logs")
                                        .build();
                                    modal!(ctx, modal);
                                    Ok(())
                                });
                            }
                            ContextAction::Retry(id) => {
                                section.add_item(action.to_string(), move |ctx| {
                                    ctx.ytdlp_manager.redownload(id);
                                    Ok(())
                                });
                            }
                            // Round 56.6-3: drop the entry from the list
                            // (files kept) and send the refresh so the
                            // modal's rows + cursor update in place.
                            ContextAction::Remove(id) => {
                                section.add_item(action.to_string(), move |ctx| {
                                    ctx.ytdlp_manager.remove(id);
                                    status_info!("Removed download from the list (files kept)");
                                    let _ = ctx
                                        .app_event_sender
                                        .send(crate::AppEvent::YtDlpDownloadsUpdated);
                                    Ok(())
                                });
                            }
                        }
                    }

                    Some(section)
                })
                .list_section(ctx, |section| Some(section.item("Cancel", |_ctx| Ok(()))))
                .build();

            modal!(ctx, modal);
        }
    }
}

#[derive(strum::Display)]
enum ContextAction {
    #[strum(to_string = "Cancel download")]
    Cancel(DownloadId),
    #[strum(to_string = "Add to queue")]
    Add(PathBuf),
    #[strum(to_string = "Download")]
    Requeue(DownloadId),
    #[strum(to_string = "Show logs")]
    Logs(Vec<String>),
    #[strum(to_string = "Retry")]
    Retry(DownloadId),
    #[strum(to_string = "Remove from list")]
    Remove(DownloadId),
}

impl DownloadState {
    fn as_style(&self, ctx: &Ctx) -> ratatui::style::Style {
        match self {
            DownloadState::Queued => ctx.config.theme.level_styles.info,
            DownloadState::Downloading => ctx.config.theme.level_styles.warn,
            DownloadState::Completed { .. } => ctx.config.theme.level_styles.info,
            DownloadState::AlreadyDownloaded { .. } => ctx.config.theme.level_styles.info,
            DownloadState::Failed { .. } => ctx.config.theme.level_styles.error,
            DownloadState::Canceled => ctx.config.theme.level_styles.error,
        }
    }
}

impl DirStackItem for DownloadId {
    fn as_path(&self) -> &'static str {
        ""
    }

    fn is_file(&self) -> bool {
        true
    }

    fn to_file_preview(&self, _ctx: &Ctx) -> Vec<crate::shared::mpd_query::PreviewGroup> {
        Vec::new()
    }

    fn matches(&self, _song_format: &[Property<SongProperty>], _ctx: &Ctx, _filter: &str) -> bool {
        true
    }

    fn to_list_item<'a>(
        &self,
        _ctx: &Ctx,
        _is_marked: bool,
        _matches_filter: bool,
        _additional_content: Option<String>,
    ) -> ListItem<'a> {
        ListItem::new("")
    }
}
