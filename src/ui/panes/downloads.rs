use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    prelude::Position,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};
use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{CommonAction, DirectoriesActions},
        theme::properties::{Property, SongProperty},
    },
    ctx::Ctx,
    mpd::commands::Song,
    shared::{
        events::WorkRequest,
        keys::ActionEvent,
        macros::{status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_query::PreviewGroup,
        ytdlp::{DownloadId, DownloadState},
    },
    ui::{
        UiEvent, dir_or_song::DirOrSong,
        dirstack::{Dir, DirStackItem},
        modals::{menu::modal::MenuModal, paste::YtAction},
    },
};
/// The Downloads tab (round 60 amendment): lists every file in
/// `~/Downloads/s2udio-downloads`, typed by media kind (audio `S` rows,
/// video `▶` rows — the files may come from stream downloads, torrents,
/// …). Playback runs through mpv (the files live outside the MPD library,
/// whose queue cannot hold them); the queue actions operate on the mpv
/// video playlist, matching the video-list behavior.
/// Round 64: in-progress rows (yt-dlp queue entries + active daemon jobs)
/// are merged on top of the disk files; the list rescans while visible.
#[derive(Debug)]
pub struct DownloadsPane {
    /// The download rows (disk files + synthetic in-progress rows),
    /// rebuilt from the merged listing when the pane shows / a download
    /// completes / the render-time throttle elapses.
    list: Dir<DownloadRow, ListState>,
    /// Round 64: when the merged listing was last rebuilt from the
    /// render-time throttle (None = never; the first render rescans).
    last_rescan: Option<Instant>,
    /// Scroll state of the info box.
    info_state: ListState,
    /// Area of the info box (mouse scrolling).
    info_area: Rect,
    /// Number of rows in the info box (scroll bounds).
    info_items_len: usize,
    /// The file whose info is shown (reset the scroll on change).
    info_key: Option<String>,
    /// The items list area (mouse row math), refreshed every render.
    list_area: Rect,
    /// The 4-cell scrollbar strip of the items list (mouse drag area,
    /// round 63).
    scrollbar_area: Rect,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadKind {
    Audio,
    Video,
}
fn kind_of(path: &str) -> DownloadKind {
    if crate::ui::panes::playlists::is_video_uri(path) {
        DownloadKind::Video
    } else {
        DownloadKind::Audio
    }
}
/// The disk listing: every file of the downloads dir, sorted, mapped to
/// `DirOrSong::Song` with the stem as its title (audio vs video typed by
/// the extension; the row prefix shows the kind).
fn list_downloads() -> Vec<DirOrSong> {
    let Some(dir) = crate::ui::modals::paste::downloads_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut items: Vec<DirOrSong> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = std::path::Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone());
            let mut metadata = std::collections::HashMap::new();
            metadata.insert(
                "title".to_owned(),
                crate::mpd::commands::metadata_tag::MetadataTag::from(stem),
            );
            metadata.insert(
                "kind".to_owned(),
                crate::mpd::commands::metadata_tag::MetadataTag::from(
                    match kind_of(&path.to_string_lossy()) {
                        DownloadKind::Audio => "Audio".to_owned(),
                        DownloadKind::Video => "Video".to_owned(),
                    },
                ),
            );
            DirOrSong::Song(crate::mpd::commands::Song {
                file: path.to_string_lossy().into_owned(),
                duration: None,
                metadata,
                last_modified: e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| chrono::DateTime::from_timestamp(d.as_secs() as i64, 0))
                    .flatten()
                    .unwrap_or(chrono::Utc::now()),
                ..Default::default()
            })
        })
        .collect();
    items.sort_by(|a, b| a.as_path().cmp(b.as_path()));
    items
}
/// Round 64: a Downloads tab row — either a real file from the downloads
/// dir (audio/video, playable) or a synthetic in-progress row (a yt-dlp
/// manager item or an active daemon torrent job; no playable file yet —
/// its menu streams instead).
#[derive(Debug, Clone)]
enum DownloadRow {
    File(DirOrSong),
    Yt {
        /// Stable row key (`as_path`), e.g. `ytdlp:<manager id>`.
        key: String,
        /// The yt-dlp manager entry behind the row.
        id: DownloadId,
        /// Display label (the yt-dlp filename, or its id).
        label: String,
        /// `true`: state is Downloading, `false`: Queued.
        downloading: bool,
    },
    Torrent {
        key: String,
        /// The daemon job id behind the row.
        job_id: String,
        label: String,
        /// Overall progress of the kept files (0-100).
        progress: f64,
    },
}
impl DirStackItem for DownloadRow {
    fn as_path(&self) -> &str {
        match self {
            DownloadRow::File(DirOrSong::Song(song)) => &song.file,
            DownloadRow::File(DirOrSong::Dir { name, .. }) => name,
            DownloadRow::Yt { key, .. } | DownloadRow::Torrent { key, .. } => key,
        }
    }
    fn is_file(&self) -> bool {
        true
    }
    fn to_file_preview(&self, ctx: &Ctx) -> Vec<PreviewGroup> {
        match self {
            DownloadRow::File(file) => file.to_file_preview(ctx),
            DownloadRow::Yt { .. } | DownloadRow::Torrent { .. } => Vec::new(),
        }
    }
    fn matches(
        &self,
        _song_format: &[Property<SongProperty>],
        _ctx: &Ctx,
        _filter: &str,
    ) -> bool {
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
/// Round 64: the merged download rows — in-progress yt-dlp downloads and
/// active daemon torrent jobs first (synthetic rows on top), then the
/// completed files from disk. The synthetic rows carry a stable key so a
/// rescan can preserve the selection.
fn list_rows(ctx: &Ctx) -> Vec<DownloadRow> {
    let mut rows = Vec::new();
    for id in ctx.ytdlp_manager.ids() {
        let Some(item) = ctx.ytdlp_manager.get(id) else { continue };
        if matches!(item.state, DownloadState::Queued | DownloadState::Downloading) {
            let label = if item.inner.filename.is_empty() {
                item.inner.id.clone()
            } else {
                item.inner.filename.clone()
            };
            rows.push(DownloadRow::Yt {
                key: format!("ytdlp:{id:?}"),
                id,
                label,
                downloading: matches!(item.state, DownloadState::Downloading),
            });
        }
    }
    if let Some(state) = ctx.dl_state.borrow().as_ref() {
        for job in state.jobs.iter().filter(|job| job.status.active()) {
            rows.push(DownloadRow::Torrent {
                key: format!("torrent:{}", job.job_id),
                job_id: job.job_id.clone(),
                label: job.torrent_name.clone(),
                progress: job.progress_percent,
            });
        }
    }
    rows.extend(list_downloads().into_iter().map(DownloadRow::File));
    rows
}
/// The download rows, typed by media kind: `S` for audio, `▶` for video.
fn download_items(
    items: &[DownloadRow],
    marked: &std::collections::BTreeSet<usize>,
    hovered: Option<usize>,
    ctx: &Ctx,
) -> Vec<ListItem<'static>> {
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let config = &ctx.config;
            let (prefix, title, suffix): (Span<'static>, String, Option<Span<'static>>) =
                match item {
                    DownloadRow::File(DirOrSong::Song(song)) => {
                        let file = song.file.as_str();
                        let prefix = match kind_of(file) {
                            DownloadKind::Audio => Span::styled(
                                config.theme.symbols.song.clone(),
                                config.theme.symbols.song_style.unwrap_or_default(),
                            ),
                            DownloadKind::Video => Span::raw("▶ "),
                        };
                        let suffix = matches!(kind_of(file), DownloadKind::Video)
                            .then(|| Span::styled("  [video]", config.as_list_text_style()));
                        (prefix, song_title(song), suffix)
                    }
                    DownloadRow::File(DirOrSong::Dir { name, .. }) => ("".into(), name.clone(), None),
                    // Round 64: in-progress rows — a distinct ↓ marker and
                    // a state/kind subtitle.
                    DownloadRow::Yt { label, downloading, .. } => {
                        let prefix = Span::styled("↓", config.theme.level_styles.warn);
                        let subtitle = if *downloading { "  - downloading" } else { "  - queued" };
                        let suffix = Some(Span::styled(subtitle, config.as_list_text_style()));
                        (prefix, label.clone(), suffix)
                    }
                    DownloadRow::Torrent { label, progress, .. } => {
                        let prefix = Span::styled("↓", config.theme.level_styles.warn);
                        let suffix = Some(Span::styled(
                            format!("  torrent · {progress:.0}%"),
                            config.as_list_text_style(),
                        ));
                        (prefix, label.clone(), suffix)
                    }
                };
            let mut line = Line::from(vec![prefix, Span::from(" "), Span::from(title)]);
            if let Some(suffix) = suffix {
                line.push_span(suffix);
            }
            let mut item = ListItem::from(line);
            if marked.contains(&idx) {
                item = item.style(config.theme.marked_item_style);
            } else if hovered == Some(idx) {
                item = item.style(config.theme.hovered_item_style);
            }
            item
        })
        .collect()
}
fn song_title(song: &Song) -> String {
    song.metadata
        .get("title")
        .map(crate::mpd::commands::metadata_tag::MetadataTag::first)
        .unwrap_or("Untitled")
        .to_owned()
}
impl DownloadsPane {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            list: Dir::new(list_rows(ctx)),
            last_rescan: None,
            info_state: ListState::default(),
            info_area: Rect::default(),
            info_items_len: 0,
            info_key: None,
            list_area: Rect::default(),
            scrollbar_area: Rect::default(),
        }
    }
    fn open_menu(&mut self, ctx: &Ctx, anchor: Option<Position>) -> Result<()> {
        let Some(item) = self.list.selected().cloned() else { return Ok(()) };
        match item {
            // Round 64: an in-progress row streams (same actions as the
            // Downloads modal's "Stream (mpv)") — no playable file yet.
            DownloadRow::Yt { id, .. } => {
                let modal = MenuModal::new(ctx)
                    .anchor(anchor)
                    .list_section(ctx, |section| {
                        let stream_id = id;
                        Some(section.item("Stream (mpv)", move |ctx| {
                            let Some(item) = ctx.ytdlp_manager.get(stream_id) else {
                                return Ok(());
                            };
                            if let Err(err) = ctx.work_sender.send(
                                WorkRequest::ResolveYtStreams {
                                    urls: vec![item.inner.to_url()],
                                    action: YtAction::PlayVideo,
                                },
                            ) {
                                status_warn!("{err}");
                            }
                            Ok(())
                        }))
                    })
                    .list_section(ctx, |section| {
                        Some(section.item("Cancel", |_ctx| Ok(())))
                    })
                    .build();
                crate::shared::macros::modal!(ctx, modal);
                Ok(())
            }
            DownloadRow::Torrent { job_id, .. } => {
                let modal = MenuModal::new(ctx)
                    .anchor(anchor)
                    .list_section(ctx, |section| {
                        let stream_job_id = job_id.clone();
                        Some(section.item("Stream (mpv)", move |ctx| {
                            let job = ctx
                                .dl_state
                                .borrow()
                                .as_ref()
                                .and_then(|state| {
                                    state
                                        .jobs
                                        .iter()
                                        .find(|job| job.job_id == stream_job_id)
                                })
                                .cloned();
                            if let Some(job) = job {
                                crate::ui::modals::paste::stream_daemon_job(ctx, &job)?;
                            }
                            Ok(())
                        }))
                    })
                    .list_section(ctx, |section| {
                        Some(section.item("Cancel", |_ctx| Ok(())))
                    })
                    .build();
                crate::shared::macros::modal!(ctx, modal);
                Ok(())
            }
            DownloadRow::File(DirOrSong::Dir { .. }) => Ok(()),
            DownloadRow::File(DirOrSong::Song(song)) => {
                let file = song.file.clone();
                let title = song_title(&song);
                let entry =
                    crate::core::mpv::MpvPlaylistEntry::new(title.clone(), file.clone(), None);
                let modal = MenuModal::new(ctx)
                    .anchor(anchor)
                    .list_section(
                        ctx,
                        |section| {
                            let play_entry = entry.clone();
                            let play_title = title.clone();
                            let mut section = section.item("Play (mpv)", move |ctx| {
                                crate::core::mpv::play_video_entries(ctx, vec![play_entry.clone()]);
                                status_info!("Playing {}", play_title);
                                Ok(())
                            });
                            let add_entry = entry.clone();
                            section = section.item("Add to queue", move |ctx| {
                                crate::core::mpv::add_to_video_playlist(
                                    ctx,
                                    vec![add_entry.clone()],
                                    false,
                                );
                                Ok(())
                            });
                            let rep_entry = entry.clone();
                            section = section.item("Replace queue", move |ctx| {
                                {
                                    let mut playlist = ctx.video_playlist.borrow_mut();
                                    playlist.clear();
                                    playlist.push(rep_entry.clone());
                                }
                                crate::ui::modals::paste::save_video_playlist(ctx);
                                Ok(())
                            });
                            Some(section)
                        },
                    )
                    .list_section(
                        ctx,
                        |section| {
                            let section = section.item("Cancel", |_ctx| Ok(()));
                            Some(section)
                        },
                    )
                    .build();
                crate::shared::macros::modal!(ctx, modal);
                Ok(())
            }
        }
    }
}
impl Pane for DownloadsPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        // Round 64: rescan while visible, throttled to once per ~2.5 s.
        // A pane render only happens while the tab is on screen, so this
        // also covers the switch-to-this-tab case; the existing
        // `DownloadsUpdated`/`Displayed` rescans stay on top.
        if self.last_rescan.is_none_or(|t| t.elapsed() >= RESCAN_INTERVAL) {
            self.last_rescan = Some(Instant::now());
            self.rescan(ctx);
        }
        // Round 60c (S3): the legend/tips strip is gone — the list and
        // the info box reclaim the whole pane.
        let info_h = (area.height * 2 / 3).min(15);
        let [list_area_raw, info_area] = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(info_h),
            ])
            .areas(area);
        // The Item Box: title row + separator + the scrollable list.
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style());
        let inner = block.inner(list_area_raw);
        let title = format!(" Downloads ({}) ", self.list.items.len());
        let content = crate::ui::item_box_header(frame, inner, title.trim(), ctx);
        let (list_area, scrollbar_area) = if ctx.config.theme.scrollbar.is_some() {
            crate::ui::scrollbar_strip(content)
        } else {
            (content, Rect::default())
        };
        self.list_area = list_area;
        self.scrollbar_area = scrollbar_area;
        self.list
            .state
            .set_content_and_viewport_len(self.list.items.len(), list_area.height.into());
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            list_area,
            self.list.state.offset(),
            self.list.items.len(),
            1,
        );
        let items = download_items(&self.list.items, &self.list.state.marked, hover_idx, ctx);
        ratatui::widgets::StatefulWidget::render(
            crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
                .highlight_style(
                    if hover_idx == self.list.state.get_selected() {
                        ctx.config.theme.hovered_item_style
                    } else {
                        ctx.config.theme.current_item_style
                    },
                )
                .style(ctx.config.as_list_name_style()),
            list_area,
            frame.buffer_mut(),
            self.list.state.as_render_state_ref(),
        );
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar() && scrollbar_area.width > 0 {
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                scrollbar_area,
                self.list.state.as_scrollbar_state_ref(),
            );
        }
        frame.render_widget(block, list_area_raw);
        crate::ui::connect_box_divider(frame, list_area_raw, list_area_raw.y + 2, ctx);


        // The Info Box: the selected file's details (metadata first,
        // [Tags] (kind), [File] last — the A3 layout).
        self.render_info(frame, info_area, ctx);
        Ok(())
    }
    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_common() {
            match action {
                CommonAction::Up | CommonAction::Down => {
                    let dir = if matches!(action, CommonAction::Up) { -1 } else { 1 };
                    self.move_cursor(dir, ctx)?;
                    return Ok(());
                }
                CommonAction::Confirm => return self.open_menu(ctx, None),
                CommonAction::ContextMenu => return self.open_menu(ctx, None),
                CommonAction::Close => {
                    if !self.list.state.marked.is_empty() {
                        self.list.state.unmark_all();
                        ctx.render()?;
                        return Ok(());
                    }
                    event.abandon();
                }
                _ => event.abandon(),
            }
        }
        if event.claim_global().is_some() {
            event.abandon();
        }
        if let Some(action) = event.claim_directories() {
            match action {
                DirectoriesActions::FolderUp | DirectoriesActions::FolderDown => {
                    let dir = if matches!(action, DirectoriesActions::FolderUp) { -1 } else { 1 };
                    self.move_cursor(dir, ctx)?;
                }
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    self.play_selected(ctx)?;
                }
                DirectoriesActions::FolderCollapse => {}
            }
        }
        Ok(())
    }
    /// Round 63.1 (3): any release ends an armed scrollbar grab (the
    /// routed release may have landed on another pane).
    fn on_global_mouse_release(&mut self, _ctx: &Ctx) -> Result<()> {
        self.list.state.scrollbar_drag.disarm();
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let area = self.list_area;
        if area.height == 0 {
            return Ok(());
        }
        // Round 63: the downloads scrollbar strip drags/jumps like every
        // other list — a press anywhere in the 4-cell strip arms the grab
        // and the thumb then follows the pointer anywhere until release.
        if self.scrollbar_area.width > 0
            && matches!(
                event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }
            )
        {
            let viewport_len = self
                .list
                .state
                .viewport_len()
                .unwrap_or(self.scrollbar_area.height as usize);
            let content_len = self
                .list
                .items
                .len()
                .saturating_sub(viewport_len)
                .saturating_add(1)
                .max(1);
            let position = self.list.state.inner.offset();
            let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
            if let Some(perc) = self.list.state.scrollbar_drag.handle(
                event,
                self.scrollbar_area,
                content_len,
                viewport_len,
                position,
                begin_len,
                end_len,
            ) {
                self.list.state.scroll_to(perc, ctx.config.scrolloff);
                ctx.render()?;
                return Ok(());
            }
        }
        let position = event.into();
        let row = usize::from(event.y.saturating_sub(area.y));
        match event.kind {
            MouseEventKind::LeftClick if event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                if let Some(idx) = self.list.state.get_at_rendered_row(row) {
                    let dir = &mut self.list;
                    dir.select_idx(idx, ctx.config.scrolloff);
                    dir.state.toggle_mark(idx);
                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick if event.modifiers.contains(crossterm::event::KeyModifiers::ALT) => {
                if let Some(idx) = self.list.state.get_at_rendered_row(row) {
                    let dir = &mut self.list;
                    dir.state.band.cancel();
                    if dir.state.mark_anchor().is_none() {
                        dir.state.set_mark_anchor(idx);
                    }
                    let anchor = dir.state.mark_anchor().unwrap_or(idx);
                    if let Some((lo, hi)) = dir.state.take_range_mark() {
                        for i in lo..=hi {
                            dir.state.marked.remove(&i);
                        }
                    }
                    if anchor != idx {
                        dir.state.mark_range(anchor, idx);
                        dir.state.set_range_mark(anchor, idx);
                    }
                    dir.select_idx(idx, ctx.config.scrolloff);
                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick => {
                if let Some(idx) = self.list.state.get_at_rendered_row(row) {
                    let click_on_different = !self.list.state.marked.is_empty()
                        && Some(idx) != self.list.state.get_selected();
                    self.list.state.band.arm(idx, click_on_different);
                    self.list.select_idx(idx, ctx.config.scrolloff);
                    self.list.state.set_mark_anchor(idx);
                    self.list.state.clear_range_mark();
                    ctx.render()?;
                }
            }
            MouseEventKind::DoubleClick => {
                self.list.state.band.cancel();
                if self.list.state.get_at_rendered_row(row).is_some() {
                    self.play_selected(ctx)?;
                }
            }
            MouseEventKind::RightClick => {
                self.list.state.band.cancel();
                let Some(idx) = self.list.state.get_at_rendered_row(row) else {
                    return Ok(());
                };
                self.list.select_idx(idx, ctx.config.scrolloff);
                return self.open_menu(ctx, Some(position));
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                self.move_cursor(dir, ctx)?;
            }
            _ => {}
        }
        Ok(())
    }
    fn on_event(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            // A download completed / the tab shown: re-read the merged
            // listing so new files appear (selection preserved by path).
            UiEvent::DownloadsUpdated | UiEvent::Displayed => {
                self.rescan(ctx);
                ctx.render()?;
            }
            _ => {}
        }
        Ok(())
    }
    fn on_query_finished(
        &mut self,
        _id: &'static str,
        _data: MpdQueryResult,
        _is_visible: bool,
        _ctx: &Ctx,
    ) -> Result<()> {
        Ok(())
    }
}
const RESCAN_INTERVAL: Duration = Duration::from_millis(2500);

impl DownloadsPane {
    /// Round 64: rebuild the merged row list, preserving the selection by
    /// path when the selected row is still present (a file row keeps its
    /// file path, a synthetic row its stable key). With no previous
    /// selection and a previously-empty list, the first row is selected.
    fn rescan(&mut self, ctx: &Ctx) {
        let selected_path = self.list.selected().map(|item| item.as_path().to_owned());
        let len_old = self.list.items.len();
        self.list = Dir::new(list_rows(ctx));
        if let Some(path) = selected_path {
            if let Some(idx) = self
                .list
                .items
                .iter()
                .position(|item| item.as_path() == path)
            {
                self.list.select_idx(idx, ctx.config.scrolloff);
                return;
            }
        }
        if len_old == 0 && !self.list.items.is_empty() {
            self.list.state.select(Some(0), 0);
        }
    }
    fn move_cursor(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        if self.list.items.is_empty() {
            return Ok(());
        }
        let state = &mut self.list.state;
        if dir < 0 {
            state.prev(ctx.config.scrolloff, false);
        } else {
            state.next(ctx.config.scrolloff, false);
        }
        ctx.render()?;
        Ok(())
    }
    fn play_selected(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.list.selected().cloned() else { return Ok(()) };
        // Round 64: in-progress rows have no playable file — Enter /
        // double-click stays safe (their context menu streams instead).
        let DownloadRow::File(DirOrSong::Song(song)) = item else { return Ok(()) };
        let title = song_title(&song);
        let file = song.file.clone();
        crate::core::mpv::play_video_entries(
            ctx,
            vec![crate::core::mpv::MpvPlaylistEntry::new(title, file, None)],
        );
        Ok(())
    }
    fn render_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let key = self.list.selected().map(|item| item.as_path().to_owned());
        let mut items: Vec<ListItem> = Vec::new();
        // Round 64: only real file rows have an info box (the synthetic
        // in-progress rows leave it empty).
        if let Some(DownloadRow::File(DirOrSong::Song(song))) = self.list.selected() {
            let preview = song.to_file_preview(ctx);
            for group in preview {
                if let Some(name) = group.name {
                    items.push(
                        ListItem::new(Line::styled(name, group.header_style.unwrap_or_default())),
                    );
                }
                items.extend(group.items);
                items.push(ListItem::new(""));
            }
        }
        self.info_items_len = items.len();
        if self.info_key != key {
            self.info_key = key;
            self.info_state = ListState::default();
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(area);
        let list = List::new(items).style(ctx.config.as_list_name_style());
        ratatui::widgets::StatefulWidget::render(
            list,
            inner,
            frame.buffer_mut(),
            &mut self.info_state,
        );
        frame.render_widget(block, area);
        self.info_area = inner;
    }
}
