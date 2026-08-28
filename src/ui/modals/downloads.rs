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
    /// Torrent-section cursor: whether the keyboard selection is on the
    /// round-54 downloader-daemon rows (the modal's second section) and
    /// which row. The yt-dlp table keeps its own `queue` cursor; Down at
    /// the bottom of it enters the torrent section, Up at its top leaves.
    torrent_focus: bool,
    torrent_selected: usize,
    /// The torrent rows as rendered (job ids from `ctx.dl_state`), so the
    /// context menu knows what the selection points at.
    torrent_jobs: Vec<String>,
    /// The area of the torrent section (mouse hit-testing).
    torrent_area: Rect,
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

        let rows = ctx.ytdlp_manager.map_values(|item| {
            Row::new([
                Cell::from(""), // marker
                Cell::from(item.inner.id.clone()),
                Cell::from(item.inner.kind.to_string()),
                Cell::from(item.state.to_string()).style(item.state.as_style(ctx)),
            ])
        });
        let item_count = rows.len();
        let table = Table::new(rows, constraints![==1, ==33%, ==33%, ==34%])
            .row_highlight_style(ctx.config.theme.current_item_style)
            .header(Row::new(["", "Id", "Source", "State"]));

        self.queue
            .state
            .set_content_and_viewport_len(ctx.ytdlp_manager.len(), table_area.height as usize);
        frame.render_stateful_widget(table, table_area, self.queue.state.as_render_state_ref());

        // Round 54: the downloader-daemon (torrent) section below the
        // yt-dlp table — fed by `~/.cache/s2udio/downloads.json` (the
        // shared state the daemon writes; `Ctx.dl_state` is refreshed by
        // the 1 s `DlStatePoll`). Rows: name | status | progress; active
        // jobs offer "Stop download" (the daemon forgets the torrent,
        // partials stay).
        let job_count = ctx
            .dl_state
            .borrow()
            .as_ref()
            .map(|state| state.jobs.len())
            .unwrap_or(0);
        self.torrent_jobs = ctx
            .dl_state
            .borrow()
            .as_ref()
            .map(|state| state.jobs.iter().map(|job| job.job_id.clone()).collect())
            .unwrap_or_default();
        if self.torrent_selected >= job_count {
            self.torrent_selected = job_count.saturating_sub(1);
        }
        // Reserve 1 header row + 1 separator row; the torrent table shares
        // the popup's lower half.
        let yt_height = table_area
            .height
            .saturating_sub(if job_count > 0 { job_count as u16 + 2 } else { 0 });
        let torrent_area = ratatui::layout::Rect {
            x: table_area.x,
            y: table_area.y + yt_height.min(table_area.height),
            width: table_area.width,
            height: table_area.height.saturating_sub(yt_height.min(table_area.height)),
        };
        if job_count > 0 {
            frame.render_widget(
                Block::default()
                    .borders(ratatui::widgets::Borders::TOP)
                    .border_set(border::PLAIN)
                    .border_style(ctx.config.as_border_style())
                    .title_alignment(ratatui::prelude::Alignment::Left)
                    .title("Torrent downloads"),
                torrent_area,
            );
            let inner = torrent_area.inner(Margin {
                horizontal: 0,
                vertical: 1,
            });
            let jobs = ctx.dl_state.borrow();
            let rows: Vec<Row> = jobs
                .as_ref()
                .map(|state| {
                    state
                        .jobs
                        .iter()
                        .map(|job| {
                            let name = if job.torrent_name.len() > 40 {
                                let (head, _) = job.torrent_name.split_at(40);
                                format!("{head}…")
                            } else {
                                job.torrent_name.clone()
                            };
                            let progress = if job.status.active() {
                                format!("{:.0}%", job.progress_percent)
                            } else {
                                job.status.to_string()
                            };
                            let style = match job.status {
                                crate::core::dlctl::DlStatus::Failed => {
                                    ctx.config.theme.level_styles.error
                                }
                                crate::core::dlctl::DlStatus::Downloading
                                | crate::core::dlctl::DlStatus::Adding
                                | crate::core::dlctl::DlStatus::Moving
                                | crate::core::dlctl::DlStatus::Queued => {
                                    ctx.config.theme.level_styles.warn
                                }
                                crate::core::dlctl::DlStatus::Stopped => {
                                    ctx.config.theme.level_styles.info
                                }
                            };
                            let daemon_offline = jobs
                                .as_ref()
                                .is_some_and(|s| !crate::core::dlctl::daemon_running(s));
                            let status_text = if daemon_offline && job.status.active() {
                                format!("offline ({})", job.status)
                            } else {
                                progress
                            };
                            Row::new([
                                Cell::from(""),
                                Cell::from(name),
                                Cell::from(status_text).style(style),
                            ])
                        })
                        .collect()
                })
                .unwrap_or_default();
            let torrent_table = Table::new(rows, constraints![==1, ==60%, ==39%])
                .row_highlight_style(ctx.config.theme.current_item_style)
                .header(Row::new(["", "Torrent", "Status"]));
            let mut torrent_state = ratatui::widgets::TableState::default();
            if self.torrent_focus {
                torrent_state.select(Some(self.torrent_selected.min(job_count.saturating_sub(1))));
            }
            frame.render_stateful_widget(
                torrent_table,
                inner,
                &mut torrent_state,
            );
            self.torrent_area = inner;
        } else {
            self.torrent_area = ratatui::layout::Rect::default();
            self.torrent_focus = false;
        }

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
                    let job_count = self.torrent_jobs.len();
                    if job_count > 0 && !self.torrent_focus && self.at_yt_bottom() {
                        // Move from the yt table into the torrent section.
                        self.torrent_focus = true;
                        self.torrent_selected = 0;
                    } else if self.torrent_focus {
                        if self.torrent_selected + 1 < job_count {
                            self.torrent_selected += 1;
                        } else if ctx.config.wrap_navigation && job_count > 0 {
                            self.torrent_selected = 0;
                        }
                    } else {
                        self.queue.next(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    }
                    ctx.render()?;
                }
                CommonAction::Up => {
                    if self.torrent_focus {
                        if self.torrent_selected == 0 {
                            self.torrent_focus = false;
                        } else {
                            self.torrent_selected -= 1;
                        }
                    } else {
                        self.queue.prev(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    }
                    ctx.render()?;
                }
                CommonAction::Close => {
                    self.hide(ctx)?;
                }
                CommonAction::Confirm => {
                    if self.torrent_focus {
                        self.create_torrent_menu(ctx);
                    } else {
                        self.create_menu(ctx);
                    }
                }
                CommonAction::DownHalf => {
                    if self.torrent_focus {
                        let step = (self.torrent_area.height / 2).max(1) as usize;
                        for _ in 0..step {
                            if self.torrent_selected + 1 < self.torrent_jobs.len() {
                                self.torrent_selected += 1;
                            }
                        }
                    } else {
                        self.queue.next_half_viewport(ctx.config.scrolloff);
                    }
                    ctx.render()?;
                }
                CommonAction::UpHalf => {
                    if self.torrent_focus {
                        let step = (self.torrent_area.height / 2).max(1) as usize;
                        for _ in 0..step {
                            if self.torrent_selected > 0 {
                                self.torrent_selected -= 1;
                            }
                        }
                    } else {
                        self.queue.prev_viewport(ctx.config.scrolloff);
                    }
                    ctx.render()?;
                }
                CommonAction::PageUp => {
                    if self.torrent_focus {
                        self.torrent_selected = 0;
                    } else {
                        self.queue.prev_viewport(ctx.config.scrolloff);
                    }
                    ctx.render()?;
                }
                CommonAction::PageDown => {
                    if self.torrent_focus {
                        self.torrent_selected = self.torrent_jobs.len().saturating_sub(1);
                    } else {
                        self.queue.next_viewport(ctx.config.scrolloff);
                    }
                    ctx.render()?;
                }
                CommonAction::Top => {
                    if self.torrent_focus && self.torrent_selected == 0 {
                        self.torrent_focus = false;
                    } else if self.torrent_focus {
                        self.torrent_selected = 0;
                    } else {
                        self.queue.first();
                    }
                    ctx.render()?;
                }
                CommonAction::Bottom => {
                    if self.torrent_focus {
                        self.torrent_selected = self.torrent_jobs.len().saturating_sub(1);
                    } else {
                        self.queue.last();
                    }
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

        // Round 54: clicks in the torrent section select torrent rows.
        if !self.torrent_area.is_empty() && self.torrent_area.contains(event.into()) {
            let clicked_row: usize = event.y.saturating_sub(self.torrent_area.y).into();
            let Some(idx) = clicked_row.checked_sub(1) else {
                // The header line itself.
                return Ok(());
            };
            if idx >= self.torrent_jobs.len() {
                return Ok(());
            }
            match event.kind {
                MouseEventKind::LeftClick => {
                    self.torrent_focus = true;
                    self.torrent_selected = idx;
                    ctx.render()?;
                }
                MouseEventKind::DoubleClick | MouseEventKind::MiddleClick | MouseEventKind::RightClick => {
                    self.torrent_focus = true;
                    self.torrent_selected = idx;
                    self.create_torrent_menu(ctx);
                    ctx.render()?;
                }
                MouseEventKind::ScrollDown => {
                    if self.torrent_selected + 1 < self.torrent_jobs.len() {
                        self.torrent_selected += 1;
                    }
                    ctx.render()?;
                }
                MouseEventKind::ScrollUp => {
                    if self.torrent_selected > 0 {
                        self.torrent_selected -= 1;
                    }
                    ctx.render()?;
                }
                _ => {}
            }
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
            MouseEventKind::DoubleClick => {
                self.queue.select_idx(idx, ctx.config.scrolloff);
                self.create_menu(ctx);
                ctx.render()?;
            }
            MouseEventKind::MiddleClick => {
                self.queue.select_idx(idx, ctx.config.scrolloff);
                self.create_menu(ctx);
                ctx.render()?;
            }
            MouseEventKind::RightClick => {
                self.queue.select_idx(idx, ctx.config.scrolloff);
                self.create_menu(ctx);
                ctx.render()?;
            }
            MouseEventKind::ScrollDown => {
                self.queue.scroll_up(ctx.config.scroll_amount, ctx.config.scrolloff);
                ctx.render()?;
            }
            MouseEventKind::ScrollUp => {
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
                if !self.queue.items.is_empty() && self.queue.selected().is_none() {
                    self.queue.state.select(Some(0), 0);
                }
                if self.torrent_selected >= ctx
                    .dl_state
                    .borrow()
                    .as_ref()
                    .map(|state| state.jobs.len())
                    .unwrap_or(0)
                {
                    self.torrent_selected = 0;
                    self.torrent_focus = false;
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
            torrent_focus: false,
            torrent_selected: 0,
            torrent_jobs: Vec::new(),
            torrent_area: Rect::default(),
        }
    }

    /// Whether the yt-dlp cursor is on its last row (Down moves into the
    /// torrent section).
    fn at_yt_bottom(&self) -> bool {
        let selected = self
            .queue
            .selected_with_idx()
            .map(|(idx, _)| idx);
        selected == Some(self.queue.len().saturating_sub(1))
    }

    /// The torrent section's context menu (Confirm / double-click /
    /// right-click on a daemon job row): active jobs get "Stop download"
    /// (the daemon forgets the torrent — partials stay — and drops the
    /// job). Terminal jobs have no actions.
    pub fn create_torrent_menu(&self, ctx: &mut Ctx) {
        let Some(job_id) = self.torrent_jobs.get(self.torrent_selected).cloned() else {
            return;
        };
        let state_ref = ctx.dl_state.borrow();
        let Some(job) = state_ref
            .as_ref()
            .and_then(|state| state.jobs.iter().find(|job| job.job_id == job_id))
        else {
            return;
        };
        if !job.status.active() {
            return;
        }
        drop(state_ref);
        let modal = MenuModal::new(ctx)
            .list_section(ctx, |mut section| {
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
                Some(section)
            })
            .build();
        modal!(ctx, modal);
    }

    pub fn create_menu(&self, ctx: &mut Ctx) {
        if let Some((id, current)) =
            self.queue.selected().and_then(|id| ctx.ytdlp_manager.get(*id).map(|item| (id, item)))
        {
            let actions = match &current.state {
                DownloadState::Queued => vec![ContextAction::Cancel(*id)],
                DownloadState::Downloading => vec![],
                DownloadState::Completed { logs, path } => {
                    vec![ContextAction::Add(path.clone()), ContextAction::Logs(logs.clone())]
                }
                DownloadState::Failed { logs } => {
                    vec![ContextAction::Retry(*id), ContextAction::Logs(logs.clone())]
                }
                DownloadState::Canceled => vec![ContextAction::Requeue(*id)],
                DownloadState::AlreadyDownloaded { path } => {
                    vec![ContextAction::Add(path.clone())]
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
