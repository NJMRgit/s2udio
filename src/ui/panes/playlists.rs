use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};
use anyhow::{Context, Result};
use enum_map::EnumMap;
use itertools::Itertools;
use ratatui::{
    Frame, layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{CommonAction, DirectoriesActions},
        tabs::{PaneType, PaneTypeDiscriminants, TreeBrowserArgs},
    },
    ctx::Ctx,
    mpd::{client::Client, commands::Song, mpd_client::{MpdClient, SingleOrRange}},
    shared::{
        cmp::StringCompare, ext::btreeset_ranges::BTreeSetRanges, keys::ActionEvent,
        macros::{modal, status_info},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::{Enqueue, MpdClientExt, MpdDelete},
    },
    status_warn,
    ui::{
        UiEvent, browser::{BrowserPane, MoveDirection},
        dir_or_song::DirOrSong, dirstack::{Dir, DirStack, DirStackItem},
        input::InputResultEvent,
        modals::{
            confirm_modal::{Action, ConfirmModal},
            info_list_modal::InfoListModal, input_modal::InputModal,
            menu::{delete_from_playlist_or_show_confirmation, modal::MenuModal},
            select_modal::SelectModal,
        },
        song_list::SongListCore, widgets::browser::{Browser, BrowserArea},
    },
};
/// The list items of the Playlists tab: playlist rows carry the ♪ / ▶
/// prefix (from the background classification), stream songs inside a
/// playlist show their cached title in the stream color, and everything
/// else renders like the other tabs (local files stay white). The rows
/// are fully owned (`'static`) so the caller can snapshot the stack
/// without fighting the borrow checker.
fn playlist_items(
    items: &[DirOrSong],
    marked: &BTreeSet<usize>,
    hovered: Option<usize>,
    ctx: &Ctx,
    kinds: &HashMap<String, PlaylistKind>,
) -> Vec<ListItem<'static>> {
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let config = &ctx.config;
            let list_item = match item {
                DirOrSong::Dir { name, playlist: true, .. } => {
                    let kind = kinds
                        .get(name.as_str())
                        .copied()
                        .unwrap_or(PlaylistKind::Audio);
                    ListItem::from(
                        Line::from(
                            vec![
                                Span::from(kind.prefix()), Span::from(if name.is_empty() {
                                "Untitled".to_owned() } else { name.clone() }),
                            ],
                        ),
                    )
                }
                DirOrSong::Song(
                    song,
                ) if crate::ui::panes::radio::is_stream_url(&song.file) => {
                    let title = stream_display_title(ctx, &song.file)
                        .unwrap_or_else(|| song.file.clone());
                    ListItem::from(
                            Line::from(
                                vec![
                                    Span::styled(config.theme.symbols.song.clone(), config.theme
                                    .symbols.song_style.unwrap_or_default(),), Span::from(" "),
                                    Span::styled(title, config.as_stream_text_style()),
                                ],
                            ),
                        )
                        .style(config.as_stream_text_style())
                }
                DirOrSong::Song(song) => {
                    let mut spans = vec![
                        Span::styled(config.theme.symbols.song.clone(), config.theme
                        .symbols.song_style.unwrap_or_default(),), Span::from(" "),
                    ];
                    spans
                        .extend(
                            config
                                .theme
                                .browser_song_format
                                .0
                                .iter()
                                .map(|prop| {
                                    Span::from(
                                        prop
                                            .as_string(
                                                Some(song),
                                                &config.theme.format_tag_separator,
                                                config.theme.multiple_tag_resolution_strategy,
                                                ctx,
                                            )
                                            .unwrap_or_default(),
                                    )
                                }),
                        );
                    ListItem::from(Line::from(spans))
                }
                DirOrSong::Dir { name, .. } => {
                    ListItem::from(
                        Line::from(
                            vec![
                                Span::from(if name.is_empty() { "Untitled".to_owned() } else
                                { name.clone() })
                            ],
                        ),
                    )
                }
            };
            if marked.contains(&idx) {
                list_item.style(config.theme.marked_item_style)
            } else if hovered == Some(idx) {
                list_item.style(config.theme.hovered_item_style)
            } else {
                list_item
            }
        })
        .collect()
}
#[derive(Debug)]
pub struct PlaylistsPane {
    stack: DirStack<DirOrSong, ListState>,
    browser: Browser<DirOrSong>,
    playlists_area: Rect,
    songs_area: Rect,
    /// Area of the playlists (left) list's scrollbar column (Round 48: the
    /// songs list's scrollbar is recorded in `browser.areas[Scrollbar]`,
    /// which the shared `SongListCore` handler reads).
    playlists_scrollbar_area: Rect,
    initialized: bool,
    /// Playlist name -> whether it holds audio or video content (from the
    /// background classification query), driving the ♪ / ▶ prefixes.
    playlist_kinds: HashMap<String, PlaylistKind>,
    /// Scroll state of the info box (mouse wheel / scrollbar).
    info_state: ListState,
    /// Area of the info box (for mouse scrolling).
    info_area: Rect,
    /// Number of rows in the info box (for the scroll bounds).
    info_items_len: usize,
    /// The item (playlist name / song file) whose info is shown; the
    /// scroll resets when it changes.
    info_key: Option<String>,
    /// Tree-browser layout args from the config (defaults = today's
    /// constants: 50-col minimum tree, hidden <= 120, info cap 15).
    tree_args: TreeBrowserArgs,
    /// Area of the info box's scrollbar (for click/drag scrolling).
    info_scrollbar_area: Rect,
    /// Drag state of the info box's scrollbar (thumb follows the pointer).
    info_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag,
}
const INIT: &str = "init";
const REINIT: &str = "reinit";
const FETCH_DATA: &str = "fetch_data";
const PLAYLIST_INFO: &str = "preview";
/// Result id of the background query classifying every playlist (audio /
/// video) from its first entry; the prefix icons in the playlist list are
/// drawn from it.
const PLAYLIST_KINDS: &str = "playlist_kinds";
/// Whether a stored playlist holds audio or video content. Playlists are
/// created audio-only or video-only, so a single video entry marks the
/// whole playlist as video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaylistKind {
    Audio,
    Video,
}
impl PlaylistKind {
    /// The prefix shown before the playlist's name in the Playlists tab:
    /// `♪ ` for audio, `▶  ` for video.
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            PlaylistKind::Audio => "♪ ",
            PlaylistKind::Video => "▶  ",
        }
    }
    /// Classify a playlist from its entries: any video file/URL makes it a
    /// video playlist (playlists are created type-pure, so the first
    /// entry usually suffices; a mixed legacy playlist is still shown as
    /// video).
    pub(crate) fn of(songs: &[Song]) -> Self {
        if songs.iter().any(|s| is_video_uri(&s.file)) {
            PlaylistKind::Video
        } else {
            PlaylistKind::Audio
        }
    }
}
/// Whether a URI is a video (local video file or a direct video URL),
/// from the paste popup's video extension list.
pub(crate) fn is_video_uri(uri: &str) -> bool {
    crate::ui::modals::paste::is_video_extension(uri)
}
/// The cached info of a stream entry (looked up by its resolved URL, or
/// matched by a cached entry's `original_url`), for the video-style info
/// box and the row titles.
pub(crate) fn stream_info(
    ctx: &Ctx,
    uri: &str,
) -> Option<crate::shared::ytdlp::YtStreamInfo> {
    let info = ctx.yt_info.borrow();
    info.get(uri)
        .cloned()
        .or_else(|| info.values().find(|e| e.original_url == uri).cloned())
}
/// The cached title of a stream (a resolved YouTube-style URL, or the
/// original link) so playlist rows show the video name instead of a long
/// random URL. `None` for local files / uncached streams.
pub(crate) fn stream_display_title(ctx: &Ctx, uri: &str) -> Option<String> {
    stream_info(ctx, uri)
        .filter(|entry| !entry.title.is_empty())
        .map(|entry| entry.title)
}
impl PlaylistsPane {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            stack: DirStack::default(),
            browser: Browser::new(),
            playlists_area: Rect::default(),
            songs_area: Rect::default(),
            playlists_scrollbar_area: Rect::default(),
            initialized: false,
            playlist_kinds: HashMap::new(),
            info_state: ListState::default(),
            info_area: Rect::default(),
            info_items_len: 0,
            info_key: None,
            tree_args: ctx.config.tree_browser_args(PaneTypeDiscriminants::Playlists),
            info_scrollbar_area: Rect::default(),
            info_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
        }
    }
    fn render_playlists(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let focused = self.stack.path().is_empty();
        let items_snapshot = self.stack.root().items.clone();
        let marked = self.stack.root().marked().clone();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(" Playlists ");
        let inner = block.inner(area);
        let (list_area, scrollbar_area) = Self::split_scrollbar(inner, ctx);
        let Dir { state, .. } = self.stack.root_mut();
        state.set_content_and_viewport_len(items_snapshot.len(), list_area.height.into());
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            list_area,
            state.offset(),
            items_snapshot.len(),
            1,
        );
        let items = playlist_items(
            &items_snapshot,
            &marked,
            hover_idx,
            ctx,
            &self.playlist_kinds,
        );
        ratatui::widgets::StatefulWidget::render(
            crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
                .highlight_style(
                    if hover_idx == state.get_selected() || focused {
                        ctx.config.theme.hovered_item_style
                    } else {
                        ctx.config.theme.current_item_style
                    },
                )
                .style(ctx.config.as_list_name_style()),
            list_area,
            frame.buffer_mut(),
            state.as_render_state_ref(),
        );
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && scrollbar_area.width > 0
        {
            frame.render_stateful_widget(
                scrollbar,
                scrollbar_area,
                state.as_scrollbar_state_ref(),
            );
        }
        ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
        self.playlists_area = list_area;
        self.playlists_scrollbar_area = scrollbar_area;
    }
    /// Split `inner` into the list area and a 1-column scrollbar area when
    /// the theme has a scrollbar (the same split the queue table uses).
    fn split_scrollbar(inner: Rect, ctx: &Ctx) -> (Rect, Rect) {
        if ctx.config.theme.scrollbar.is_some() && inner.width > 1 {
            let [list, scrollbar] = ratatui::layout::Layout::horizontal([
                ratatui::layout::Constraint::Percentage(100),
                ratatui::layout::Constraint::Length(1),
            ])
            .areas(inner);
            (list, scrollbar)
        } else {
            (inner, Rect::default())
        }
    }
    fn render_songs(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let at_root = self.stack.path().is_empty();
        let (items_snapshot, marked, title) = if at_root {
            let items_snapshot = self.stack.root().items.clone();
            let marked = self.stack.root().marked().clone();
            (items_snapshot, marked, " Playlists ")
        } else {
            let items_snapshot = self.stack.current().items.clone();
            let marked = self.stack.current().marked().clone();
            (items_snapshot, marked, " Songs ")
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(title);
        let inner = block.inner(area);
        let (list_area, scrollbar_area) = Self::split_scrollbar(inner, ctx);
        if at_root {
            let Dir { state, .. } = self.stack.root_mut();
            state
                .set_content_and_viewport_len(items_snapshot.len(), list_area.height.into());
            let hover_idx = crate::ui::panes::hovered_item(
                ctx.mouse_pos(),
                list_area,
                state.offset(),
                items_snapshot.len(),
                1,
            );
            let items = playlist_items(
                &items_snapshot,
                &marked,
                hover_idx,
                ctx,
                &self.playlist_kinds,
            );
            ratatui::widgets::StatefulWidget::render(
                crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
                    .highlight_style(
                        if hover_idx == state.get_selected() {
                            ctx.config.theme.hovered_item_style
                        } else {
                            ctx.config.theme.current_item_style
                        },
                    )
                    .style(ctx.config.as_list_name_style()),
                list_area,
                frame.buffer_mut(),
                state.as_render_state_ref(),
            );
            if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
                && scrollbar_area.width > 0
            {
                frame.render_stateful_widget(
                    scrollbar,
                    scrollbar_area,
                    state.as_scrollbar_state_ref(),
                );
            }
            ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
        } else {
            let Dir { state, .. } = self.stack.current_mut();
            state
                .set_content_and_viewport_len(items_snapshot.len(), list_area.height.into());
            let hover_idx = crate::ui::panes::hovered_item(
                ctx.mouse_pos(),
                list_area,
                state.offset(),
                items_snapshot.len(),
                1,
            );
            let items = playlist_items(
                &items_snapshot,
                &marked,
                hover_idx,
                ctx,
                &self.playlist_kinds,
            );
            ratatui::widgets::StatefulWidget::render(
                crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
                    .highlight_style(
                        if hover_idx == state.get_selected() || !at_root {
                            ctx.config.theme.hovered_item_style
                        } else {
                            ctx.config.theme.current_item_style
                        },
                    )
                    .style(ctx.config.as_list_name_style()),
                list_area,
                frame.buffer_mut(),
                state.as_render_state_ref(),
            );
            if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
                && scrollbar_area.width > 0
            {
                frame.render_stateful_widget(
                    scrollbar,
                    scrollbar_area,
                    state.as_scrollbar_state_ref(),
                );
            }
            ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
        }
        self.songs_area = list_area;
        self.browser.areas[BrowserArea::Current] = list_area;
        self.browser.areas[BrowserArea::Scrollbar] = scrollbar_area;
    }
    fn render_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let key = if self.stack.path().is_empty() {
            self.stack
                .root()
                .selected()
                .and_then(|d| match d {
                    DirOrSong::Dir { name, .. } => Some(name.clone()),
                    _ => None,
                })
        } else {
            self.stack.current().selected().map(|d| d.as_path().to_owned())
        };
        let mut items: Vec<ListItem> = Vec::new();
        if self.stack.path().is_empty() {
            if let Some(playlist) = self.stack.root().selected() {
                if let DirOrSong::Dir { name, .. } = playlist {
                    let key_style = ctx.config.theme.preview_label_style;
                    let group = ctx.config.theme.preview_metadata_group_style;
                    items.push(ListItem::new(Line::styled(" --- [Playlist]", group)));
                    items
                        .push(
                            ListItem::new(
                                Line::from(
                                    vec![
                                        Span::styled("Name", key_style), Span::raw(": "),
                                        Span::raw(name.clone()),
                                    ],
                                ),
                            ),
                        );
                }
            }
        } else if let Some(DirOrSong::Song(song)) = self.stack.current().selected() {
            if let Some(yt) = stream_info(ctx, &song.file) {
                let key_style = ctx.config.theme.preview_label_style;
                let base = ctx.config.as_text_style();
                let white = ratatui::style::Style::default()
                    .fg(ratatui::style::Color::White);
                let list_style = ctx.config.as_list_text_style();
                let body_width = (area.width.saturating_sub(4).max(10)) as usize;
                let title = if yt.title.is_empty() {
                    song.file.clone()
                } else {
                    yt.title.clone()
                };
                items.push(ListItem::new(Line::from(Span::styled(title, white))));
                if let Some(duration) = song.duration {
                    items
                        .push(
                            ListItem::new(
                                Line::from(
                                    vec![
                                        Span::styled("Time: ", key_style), Span::styled(crate
                                        ::ui::panes::lyrics::format_clock(duration.as_secs()),
                                        white,),
                                    ],
                                ),
                            ),
                        );
                }
                let (left, right, body) = crate::ui::panes::lyrics::yt_stream_info_parts(
                    &yt,
                    base,
                    list_style,
                    body_width,
                );
                if !left.is_empty() || !right.is_empty() {
                    let mut spans = left;
                    if !spans.is_empty() && !right.is_empty() {
                        spans.push(Span::raw("   "));
                    }
                    spans.extend(right);
                    items.push(ListItem::new(Line::from(spans)));
                }
                if !body.is_empty() {
                    items
                        .push(
                            ListItem::new(
                                Line::from(
                                    vec![
                                        Span::styled("Description", key_style), Span::styled(" ↴",
                                        white),
                                    ],
                                ),
                            ),
                        );
                }
                items.extend(body.into_iter().map(|row| ListItem::new(row.line)));
            } else {
                for group in song.to_file_preview(ctx) {
                    if let Some(name) = group.name {
                        items
                            .push(
                                ListItem::new(
                                    Line::styled(name, group.header_style.unwrap_or_default()),
                                ),
                            );
                    }
                    items.extend(group.items);
                    items.push(ListItem::new(""));
                }
            }
        }
        self.info_items_len = items.len();
        self.info_area = area;
        if self.info_key.as_deref() != key.as_deref() {
            self.info_key = key;
            self.info_state = ListState::default();
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(area);
        let overflow = items.len() > inner.height as usize;
        let (list_area, scrollbar_area) = if overflow
            && ctx.config.as_styled_scrollbar().is_some()
        {
            let [a, b] = Layout::horizontal([
                    Constraint::Percentage(100),
                    Constraint::Length(1),
                ])
                .areas(inner);
            (a, b)
        } else {
            (inner, Rect::default())
        };
        let list = List::new(items).style(ctx.config.as_list_name_style());
        ratatui::widgets::StatefulWidget::render(
            list,
            list_area,
            frame.buffer_mut(),
            &mut self.info_state,
        );
        ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
        if scrollbar_area.width > 0
            && let Some(scrollbar) = ctx.config.as_styled_scrollbar()
        {
            let max_offset = self
                .info_items_len
                .saturating_sub(list_area.height as usize);
            let position = self.info_state.offset().min(max_offset);
            ratatui::widgets::StatefulWidget::render(
                scrollbar,
                scrollbar_area,
                frame.buffer_mut(),
                &mut ratatui::widgets::ScrollbarState::new(max_offset + 1)
                    .position(position)
                    .viewport_content_length(list_area.height as usize),
            );
        }
        self.info_scrollbar_area = scrollbar_area;
    }
    /// Whether the songs pane has an armed/active rubber band (only
    /// meaningful inside a playlist: at the root the songs area shows the
    /// playlist-name list, which has no band, Round 46).
    fn songs_band_active(&self) -> bool {
        self.stack.current().state.band.is_active()
    }
    /// Select the cell under the pointer in the songs pane.
    fn select_song_at(
        &mut self,
        row: usize,
        select_fn: impl FnOnce(&mut Dir<DirOrSong, ListState>, usize),
    ) {
        if self.stack.path().is_empty() {
            if let Some(dir) = self.stack.next_mut()
                && let Some(idx) = dir.state.get_at_rendered_row(row)
            {
                select_fn(dir, idx);
            }
        } else if let Some(idx) = self.stack.current().state.get_at_rendered_row(row) {
            let dir = self.stack.current_mut();
            select_fn(dir, idx);
        }
    }
    /// The dir shown in the songs pane (current when inside a playlist, the
    /// next/preview dir at the root).
    fn songs_dir_mut(&mut self) -> Option<&mut Dir<DirOrSong, ListState>> {
        if self.stack.path().is_empty() {
            self.stack.next_mut()
        } else {
            Some(self.stack.current_mut())
        }
    }
    /// w/s: move the highlight up/down the playlist list (preview updates).
    fn playlist_move(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let root = self.stack.root_mut();
        if dir < 0 {
            root.prev(ctx.config.scrolloff, ctx.config.wrap_navigation);
        } else {
            root.next(ctx.config.scrolloff, ctx.config.wrap_navigation);
        }
        SongListCore::fetch_data_internal(self, ctx)?;
        ctx.render()?;
        Ok(())
    }
    /// ↑/↓: move the songs pane.
    fn songs_move(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        if let Some(dir_item) = self.songs_dir_mut() {
            if dir < 0 {
                dir_item.prev(ctx.config.scrolloff, ctx.config.wrap_navigation);
            } else {
                dir_item.next(ctx.config.scrolloff, ctx.config.wrap_navigation);
            }
            ctx.render()?;
        }
        Ok(())
    }
    /// Move the highlight in the list the cursor is on: the playlist list
    /// at the root, the songs inside a playlist (the MPD / Jellyfin
    /// single-cursor scheme).
    fn current_move(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        if self.stack.path().is_empty() {
            self.playlist_move(dir, ctx)
        } else {
            self.songs_move(dir, ctx)
        }
    }
    /// Open the highlighted playlist (enter it) or play the highlighted
    /// song — the `d`/`→`/Enter action of the MPD / Jellyfin scheme.
    fn open_or_play(&mut self, ctx: &Ctx) -> Result<()> {
        if self.stack.path().is_empty() {
            return self.open_selected_playlist(ctx);
        }
        SongListCore::open(self, true, ctx)?;
        ctx.render()?;
        Ok(())
    }
    /// Back out one level to the playlist list (no-op at the root).
    fn back_out(&mut self, ctx: &Ctx) -> Result<()> {
        self.stack_mut().leave();
        SongListCore::fetch_data_internal(self, ctx)?;
        ctx.render()?;
        Ok(())
    }
    /// Right-click / Enter menu for the highlighted item: the playlist
    /// menu at the root, the song menu inside a playlist (exactly what
    /// right-click opens). Mouse right-clicks anchor the menu at the
    /// cursor; keyboard-open stays centered (Round 46).
    fn open_context_menu(
        &mut self,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        if self.stack.path().is_empty() {
            let name = self
                .stack
                .root()
                .selected()
                .and_then(|d| match d {
                    DirOrSong::Dir { name, .. } => Some(name.clone()),
                    _ => None,
                });
            if let Some(name) = name {
                return self.open_playlist_menu(&name, ctx, anchor);
            }
            return Ok(());
        }
        self.open_song_menu(ctx, anchor)
    }
    /// Right-click / Enter menu for a playlist (anchored at the cursor
    /// when opened with the mouse).
    fn open_playlist_menu(
        &mut self,
        name: &str,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        let playlist = name.to_owned();
        let menu = MenuModal::new(ctx)
            .anchor(anchor)
            .list_section(
                ctx,
                |mut section| {
                    section
                        .add_item(
                            "Add to Queue",
                            {
                                let playlist = playlist.clone();
                                move |ctx| {
                                    ctx.command(move |client| {
                                        let songs = client.list_playlist_info(&playlist, None)?;
                                        let items: Vec<Enqueue> = songs
                                            .into_iter()
                                            .map(|s| Enqueue::File { path: s.file })
                                            .collect();
                                        client.enqueue_multiple(items, None, None, false)?;
                                        Ok(())
                                    });
                                    Ok(())
                                }
                            },
                        );
                    section
                        .add_item(
                            "Replace Queue",
                            {
                                let playlist = playlist.clone();
                                move |ctx| {
                                    ctx.command(move |client| {
                                        let songs = client.list_playlist_info(&playlist, None)?;
                                        let items: Vec<Enqueue> = songs
                                            .into_iter()
                                            .map(|s| Enqueue::File { path: s.file })
                                            .collect();
                                        client.enqueue_multiple(items, None, None, true)?;
                                        Ok(())
                                    });
                                    Ok(())
                                }
                            },
                        );
                    Some(section)
                },
            )
            .list_section(
                ctx,
                |mut section| {
                    section
                        .add_item(
                            "Rename Playlist",
                            {
                                let playlist = playlist.clone();
                                move |ctx| {
                                    let current_name = playlist.clone();
                                    modal!(
                                        ctx, InputModal::new(ctx).title("Rename playlist")
                                        .confirm_label("Rename").input_label("New name:")
                                        .initial_value(current_name.clone()).on_confirm(move | ctx,
                                        new_value | { if current_name != new_value { let
                                        current_name = current_name.clone(); let new_value =
                                        new_value.to_owned(); ctx.command(move | client | { client
                                        .rename_playlist(& current_name, & new_value) ?;
                                        status_info!("Playlist '{}' renamed to '{}'", current_name,
                                        new_value); Ok(()) }); } Ok(()) })
                                    );
                                    Ok(())
                                }
                            },
                        );
                    section
                        .add_item(
                            "Delete Playlist",
                            {
                                let playlist = playlist.clone();
                                move |ctx| {
                                    modal!(
                                        ctx, ConfirmModal::builder().ctx(ctx)
                                        .message(vec![format!("Delete playlist '{}'?", playlist),
                                        "This cannot be undone.".to_owned(),]).action(Action::Single
                                        { confirm_label : Some("Delete"), cancel_label : None,
                                        on_confirm : Box::new(move | ctx | { let playlist = playlist
                                        .clone(); ctx.command(move | client | { client
                                        .delete_multiple(vec![MpdDelete::Playlist { name : playlist,
                                        }]) ?; Ok(()) }); status_info!("Playlist deleted"); Ok(())
                                        }), }).size((45, 6)).build()
                                    );
                                    Ok(())
                                }
                            },
                        );
                    Some(section)
                },
            );
        crate::shared::macros::modal!(ctx, menu);
        Ok(())
    }
    /// Right-click / Enter menu for a song in the songs pane (anchored
    /// at the cursor when opened with the mouse).
    fn open_song_menu(
        &mut self,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        let Some(DirOrSong::Song(highlighted)) = self
            .songs_dir_mut()
            .and_then(|d| d.selected().cloned()) else {
            return Ok(());
        };
        let highlighted_file = highlighted.file.clone();
        let songs: Vec<Song> = match self.songs_dir_mut() {
            Some(dir) if !dir.marked().is_empty() => {
                dir.marked_items()
                    .filter_map(|item| match item {
                        DirOrSong::Song(song) => Some(song.clone()),
                        _ => None,
                    })
                    .collect()
            }
            _ => vec![highlighted],
        };
        let files: Vec<String> = songs.iter().map(|s| s.file.clone()).collect();
        let remove_paths: HashSet<String> = files.iter().cloned().collect();
        let playlist_name = self
            .stack
            .path()
            .as_slice()
            .first()
            .cloned()
            .unwrap_or_default();
        let download_ctx = {
            let info = ctx.yt_info.borrow();
            info.get(&highlighted_file)
                .cloned()
                .or_else(|| {
                    info.values().find(|e| e.original_url == highlighted_file).cloned()
                })
        }
            .map(|info| (info, playlist_name.clone(), highlighted_file.clone()));
        let menu = MenuModal::new(ctx)
            .anchor(anchor)
            .list_section(
                ctx,
                |mut section| {
                    let files = files.clone();
                    section
                        .add_item(
                            "Add to queue",
                            {
                                let files = files.clone();
                                move |ctx| {
                                    ctx.command(move |client| {
                                        client
                                            .enqueue_multiple(
                                                files
                                                    .iter()
                                                    .cloned()
                                                    .map(|f| Enqueue::File { path: f })
                                                    .collect_vec(),
                                                None,
                                                None,
                                                false,
                                            )?;
                                        Ok(())
                                    });
                                    Ok(())
                                }
                            },
                        );
                    section
                        .add_item(
                            "Replace queue",
                            {
                                let files = files.clone();
                                move |ctx| {
                                    ctx.command(move |client| {
                                        client
                                            .enqueue_multiple(
                                                files
                                                    .iter()
                                                    .cloned()
                                                    .map(|f| Enqueue::File { path: f })
                                                    .collect_vec(),
                                                None,
                                                None,
                                                true,
                                            )?;
                                        Ok(())
                                    });
                                    Ok(())
                                }
                            },
                        );
                    if let Some((info, playlist_name, uri)) = download_ctx {
                        section
                            .add_item(
                                "Download",
                                move |ctx| {
                                    crate::ui::modals::paste::open_stream_download_menu(
                                        ctx,
                                        &info,
                                        &crate::shared::ytdlp::ReplaceAction::Playlist {
                                            name: playlist_name,
                                            uri,
                                        },
                                    );
                                    Ok(())
                                },
                            );
                    }
                    Some(section)
                },
            )
            .list_section(
                ctx,
                |mut section| {
                    let files = files.clone();
                    section
                        .add_item(
                            "Create playlist",
                            {
                                let files = files.clone();
                                move |ctx| {
                                    let files = files.clone();
                                    let initial = files
                                        .first()
                                        .and_then(|f| f.rsplit('/').next())
                                        .unwrap_or_default()
                                        .to_owned();
                                    modal!(
                                        ctx, InputModal::new(ctx).title("Create new playlist")
                                        .confirm_label("Save").input_label("Playlist name:")
                                        .initial_value(initial).on_confirm(move | ctx, value | { let
                                        value = value.to_owned(); let files = files.clone(); ctx
                                        .command(move | client | { client.create_playlist(& value,
                                        files) ?; Ok(()) }); Ok(()) })
                                    );
                                    Ok(())
                                }
                            },
                        );
                    section
                        .add_item(
                            "Add to playlist",
                            {
                                let files = files.clone();
                                move |ctx| {
                                    let files = files.clone();
                                    let radio_playlist = ctx.config.radio.playlist.clone();
                                    let (files, playlists) = ctx
                                        .query_sync(move |client| {
                                            let playlists = client
                                                .picker_playlists(&radio_playlist)?
                                                .into_iter()
                                                .map(|p| p.name)
                                                .collect_vec();
                                            Ok((files, playlists))
                                        })?;
                                    modal!(
                                        ctx, SelectModal::builder().ctx(ctx).options(playlists)
                                        .confirm_label("Add").title("Select a playlist")
                                        .on_confirm(move | ctx, selected, _idx | { let files = files
                                        .clone(); ctx.command(move | client | { client
                                        .add_to_playlist_multiple(& selected, files) ?; Ok(()) });
                                        Ok(()) }).build()
                                    );
                                    Ok(())
                                }
                            },
                        );
                    Some(section)
                },
            )
            .list_section(
                ctx,
                |mut section| {
                    section
                        .add_item(
                            "Remove from playlist",
                            {
                                let playlist_name = playlist_name.clone();
                                let remove_paths = remove_paths.clone();
                                move |ctx| {
                                    delete_from_playlist_or_show_confirmation(
                                        playlist_name,
                                        &remove_paths,
                                        true,
                                        ctx,
                                    )?;
                                    Ok(())
                                }
                            },
                        );
                    Some(section)
                },
            );
        crate::shared::macros::modal!(ctx, menu);
        Ok(())
    }
    fn open_selected_playlist(&mut self, ctx: &Ctx) -> Result<()> {
        self.stack_mut().enter();
        SongListCore::fetch_data_internal(self, ctx)?;
        ctx.render()?;
        Ok(())
    }
    /// Fire the background query that classifies every stored playlist
    /// (audio / video) from its first entry. The result lands in
    /// `playlist_kinds` and drives the ♪ / ▶ prefixes in the list.
    fn query_playlist_kinds(&self, ctx: &Ctx) {
        let names: Vec<String> = self
            .stack
            .root()
            .items
            .iter()
            .filter_map(|item| match item {
                DirOrSong::Dir { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        ctx.query()
            .id(PLAYLIST_KINDS)
            .replace_id(PLAYLIST_KINDS)
            .target(PaneType::Playlists {
                tree: TreeBrowserArgs::default(),
            })
            .query(move |client| {
                let ranged = client.version()
                    >= crate::mpd::version::Version::new(0, 24, 0);
                let mut kinds = HashMap::new();
                for name in &names {
                    let range = ranged.then(|| SingleOrRange::single(0));
                    if let Ok(songs) = client.list_playlist_info(name, range) {
                        kinds.insert(name.clone(), PlaylistKind::of(&songs));
                    }
                }
                Ok(MpdQueryResult::Any(Box::new(kinds)))
            });
    }
}
impl Pane for PlaylistsPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        let tree_w = self.tree_args.tree_width(area.width);
        let (playlists_area, right) = if tree_w == 0 {
            (Rect::default(), area)
        } else {
            let [playlists_area, right] = Layout::horizontal([
                    Constraint::Length(tree_w),
                    Constraint::Length(area.width - tree_w),
                ])
                .areas(area);
            (playlists_area, right)
        };
        let tips_h = 3;
        let info_h = self
            .tree_args
            .info_box_height(right.height.saturating_sub(tips_h) * 2 / 3);
        let songs_h = right.height.saturating_sub(tips_h + info_h);
        let [songs_area, tips_area, info_area] = Layout::vertical([
                Constraint::Length(songs_h),
                Constraint::Length(tips_h),
                Constraint::Length(info_h),
            ])
            .areas(right);
        self.browser.areas[BrowserArea::Previous] = Rect::default();
        self.browser.areas[BrowserArea::Preview] = Rect::default();
        self.render_playlists(frame, playlists_area, ctx);
        self.render_songs(frame, songs_area, ctx);
        self.render_info(frame, info_area, ctx);
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        let tip_lines = vec![
            Line::from(vec![Span::styled("w/s · ↑/↓", base),
            Span::styled("  playlists · songs", dim),]),
            Line::from(vec![Span::styled("d / a", base),
            Span::styled("  open · back out", dim)]),
            Line::from(vec![Span::styled("Enter · →", base),
            Span::styled("  menu · play track", dim),]),
        ];
        frame
            .render_widget(
                Paragraph::new(tip_lines).style(dim),
                tips_area
                    .inner(ratatui::layout::Margin {
                        horizontal: 1,
                        vertical: 0,
                    }),
            );
        Ok(())
    }
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        let id = if self.initialized { REINIT } else { INIT };
        let compare = StringCompare::from(ctx.config.browser_song_sort.as_ref());
        let radio_playlist = ctx.config.radio.playlist.clone();
        ctx.query()
            .id(id)
            .target(PaneType::Playlists {
                tree: TreeBrowserArgs::default(),
            })
            .replace_id(id)
            .query(move |client| {
                let result: Vec<_> = client
                    .list_playlists()
                    .context("Cannot list playlists")?
                    .into_iter()
                    .filter(|playlist| playlist.name != radio_playlist)
                    .sorted_by(|a, b| compare.compare(&a.name, &b.name))
                    .map(|playlist| DirOrSong::playlist_name_only(playlist.name))
                    .collect();
                Ok(MpdQueryResult::DirOrSong {
                    data: result,
                    path: None,
                })
            });
        self.initialized = true;
        Ok(())
    }
    fn on_event(
        &mut self,
        event: &mut UiEvent,
        _is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match event {
            UiEvent::Database | UiEvent::StoredPlaylist => {
                let id = match event {
                    UiEvent::Database => INIT,
                    UiEvent::StoredPlaylist => REINIT,
                    _ => return Ok(()),
                };
                let sort_opts = ctx.config.browser_song_sort.clone();
                let radio_playlist = ctx.config.radio.playlist.clone();
                ctx.query()
                    .id(id)
                    .replace_id(id)
                    .target(PaneType::Playlists {
                        tree: TreeBrowserArgs::default(),
                    })
                    .query(move |client| {
                        let result: Vec<_> = client
                            .list_playlists()
                            .context("Cannot list playlists")?
                            .into_iter()
                            .filter(|playlist| playlist.name != radio_playlist)
                            .sorted_by(|a, b| {
                                StringCompare::from(sort_opts.as_ref())
                                    .compare(&a.name, &b.name)
                            })
                            .map(|playlist| DirOrSong::playlist_name_only(playlist.name))
                            .collect();
                        Ok(MpdQueryResult::DirOrSong {
                            data: result,
                            path: None,
                        })
                    });
            }
            UiEvent::Reconnected => {
                self.initialized = false;
                self.before_show(ctx)?;
            }
            _ => {}
        }
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let position = event.into();
        let at_root = self.stack.path().is_empty();
        // Band capture (Round 46): once a band is armed in the songs list,
        // drags and releases are accepted even when the pointer left the
        // list area (the row clamps to the visible list).
        if !at_root {
            match event.kind {
                MouseEventKind::Drag { .. } if self.songs_band_active() => {
                    return SongListCore::update_band_drag(self, event, ctx);
                }
                MouseEventKind::LeftRelease if self.songs_band_active() => {
                    return SongListCore::finish_band_drag(self, ctx);
                }
                _ => {}
            }
        }
        // Round 48 scrollbars: the songs list (right) goes through the
        // shared SongListCore handler (its scrollbar area is recorded in
        // `browser.areas[Scrollbar]` during render); the playlists list
        // (left) drives the root dir directly. Handled before the area
        // gates so an armed drag follows the pointer anywhere.
        if SongListCore::handle_scrollbar_interaction(self, event, ctx)? {
            return Ok(());
        }
        if self.playlists_scrollbar_area.width > 0
            && matches!(
                event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }
            )
        {
            let dir = self.stack.root_mut();
            let viewport_len = dir
                .state
                .viewport_len()
                .unwrap_or(self.playlists_scrollbar_area.height as usize);
            let content_len = dir
                .items
                .len()
                .saturating_sub(viewport_len)
                .saturating_add(1)
                .max(1);
            let offset = dir.state.inner.offset();
            let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
            if let Some(perc) = dir.state.scrollbar_drag.handle(
                event,
                self.playlists_scrollbar_area,
                content_len,
                viewport_len,
                offset,
                begin_len,
                end_len,
            ) {
                dir.scroll_to(perc, ctx.config.scrolloff);
                ctx.render()?;
                return Ok(());
            }
        }
        if self.playlists_area.contains(position)
            || (at_root && self.songs_area.contains(position))
        {
            let list_area = if self.playlists_area.contains(position) {
                self.playlists_area
            } else {
                self.songs_area
            };
            match event.kind {
                MouseEventKind::RightClick => {
                    self.stack.current_mut().state.band.cancel();
                    let row = usize::from(event.y.saturating_sub(list_area.y));
                    if let Some(idx) = self.stack.root().state.get_at_rendered_row(row) {
                        let dir = self.stack.root_mut();
                        dir.select_idx(idx, ctx.config.scrolloff);
                        let name = dir
                            .selected()
                            .and_then(|d| match d {
                                DirOrSong::Dir { name, .. } => Some(name.clone()),
                                _ => None,
                            });
                        if let Some(name) = name {
                            return self.open_playlist_menu(&name, ctx, Some(position));
                        }
                    }
                    return Ok(());
                }
                MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                    let row = usize::from(event.y.saturating_sub(list_area.y));
                    if let Some(idx) = self.stack.root().state.get_at_rendered_row(row) {
                        let dir = self.stack.root_mut();
                        dir.select_idx(idx, ctx.config.scrolloff);
                        let is_double = matches!(
                            event.kind, MouseEventKind::DoubleClick
                        );
                        SongListCore::fetch_data_internal(self, ctx)?;
                        if is_double {
                            self.stack_mut().enter();
                            SongListCore::fetch_data_internal(self, ctx)?;
                        }
                        ctx.render()?;
                    }
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                        -1
                    } else {
                        1
                    };
                    self.stack
                        .root_mut()
                        .scroll_viewport(dir, ctx.config.scroll_amount.max(1));
                    ctx.render()?;
                }
                _ => {}
            }
            return Ok(());
        }
        if self.songs_area.contains(position) {
            match event.kind {
                MouseEventKind::RightClick => {
                    self.stack.current_mut().state.band.cancel();
                    let row = usize::from(event.y.saturating_sub(self.songs_area.y));
                    self.select_song_at(
                        row,
                        |dir, idx| {
                            dir.state.band.cancel();
                            dir.select_idx(idx, 0);
                        },
                    );
                    return self.open_song_menu(ctx, Some(position));
                }
                MouseEventKind::LeftClick if event
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) => {
                    let row = usize::from(event.y.saturating_sub(self.songs_area.y));
                    self.select_song_at(
                        row,
                        |dir, idx| {
                            if dir.state.marked.is_empty() {
                                if let Some(sel) = dir.state.get_selected() {
                                    dir.state.mark(sel);
                                }
                            }
                            dir.state.mark(idx);
                            // Arm the band so a ctrl+drag from here adds a
                            // range (ctrl semantics keep existing marks).
                            dir.state.band.arm(idx, false);
                            dir.select_idx(idx, 0);
                        },
                    );
                    ctx.render()?;
                }
                MouseEventKind::LeftClick if event
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::ALT) => {
                    let row = usize::from(event.y.saturating_sub(self.songs_area.y));
                    self.select_song_at(
                        row,
                        |dir, idx| {
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
                            let (lo, hi) = (anchor.min(idx), anchor.max(idx));
                            if lo < hi {
                                dir.state.mark_range(lo, hi);
                                dir.state.set_range_mark(lo, hi);
                            }
                            dir.select_idx(idx, 0);
                        },
                    );
                    ctx.render()?;
                }
                MouseEventKind::DoubleClick => {
                    self.stack.current_mut().state.band.cancel();
                    let row = usize::from(event.y.saturating_sub(self.songs_area.y));
                    if let Some(idx) = self
                        .stack
                        .current()
                        .state
                        .get_at_rendered_row(row)
                    {
                        let dir = self.stack.current_mut();
                        dir.select_idx(idx, ctx.config.scrolloff);
                        SongListCore::open(self, true, ctx)?;
                        SongListCore::fetch_data_internal(self, ctx)?;
                    }
                }
                MouseEventKind::LeftClick => {
                    let row = usize::from(event.y.saturating_sub(self.songs_area.y));
                    self.select_song_at(
                        row,
                        |dir, idx| {
                            // A plain press arms the band and defers the
                            // multi-selection drop (click ≠ drag); the
                            // release resolves it (Round 46).
                            let click_on_different_row = !dir.state.marked.is_empty()
                                && Some(idx) != dir.state.get_selected();
                            dir.state.band.arm(idx, click_on_different_row);
                            dir.select_idx(idx, 0);
                            dir.state.set_mark_anchor(idx);
                            dir.state.clear_range_mark();
                        },
                    );
                    ctx.render()?;
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    self.stack.current_mut().state.band.cancel();
                    let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                        -1
                    } else {
                        1
                    };
                    self.stack
                        .current_mut()
                        .scroll_viewport(dir, ctx.config.scroll_amount.max(1));
                    ctx.render()?;
                }
                _ => {}
            }
            return Ok(());
        }
        if self.info_scrollbar_area.height > 0
            && matches!(
                event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }
            ) && self.info_scrollbar_area.contains(event.into())
        {
            let max = self.info_items_len.saturating_sub(self.info_area.height as usize);
            if max > 0 {
                let viewport_len = self.info_area.height as usize;
                let position = self.info_state.offset();
                let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
                if let Some(perc) = self
                    .info_scrollbar_drag
                    .handle(
                        event,
                        self.info_scrollbar_area,
                        max + 1,
                        viewport_len,
                        position,
                        begin_len,
                        end_len,
                    )
                {
                    let new = ((perc.clamp(0.0, 1.0)) * max as f64).floor() as usize;
                    if new != self.info_state.offset() {
                        *self.info_state.offset_mut() = new;
                        ctx.render()?;
                    }
                    return Ok(());
                }
            }
            return Ok(());
        }
        if self.info_area.contains(event.into()) {
            let dir = match event.kind {
                MouseEventKind::ScrollUp => -1,
                MouseEventKind::ScrollDown => 1,
                _ => return Ok(()),
            };
            let max = self.info_items_len.saturating_sub(self.info_area.height as usize)
                as i64;
            let current = self.info_state.offset() as i64;
            let new = (current + dir).clamp(0, max.max(0)) as usize;
            if new != self.info_state.offset() {
                *self.info_state.offset_mut() = new;
                ctx.render()?;
            }
        }
        Ok(())
    }
    fn handle_insert_mode(
        &mut self,
        kind: InputResultEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        SongListCore::handle_insert_mode(self, kind, ctx)?;
        Ok(())
    }
    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_common() {
            match action {
                CommonAction::Up | CommonAction::Down => {
                    let dir = if matches!(action, CommonAction::Up) { -1 } else { 1 };
                    return self.current_move(dir, ctx);
                }
                CommonAction::Left => {
                    return self.back_out(ctx);
                }
                CommonAction::Confirm => return self.open_context_menu(ctx, None),
                CommonAction::ContextMenu => return self.open_context_menu(ctx, None),
                CommonAction::SelectAll => {
                    if !self.stack.path().is_empty()
                        && let Some(dir) = self.songs_dir_mut() && dir.len() > 0
                    {
                        dir.state.mark_range(0, dir.len() - 1);
                        ctx.render()?;
                    }
                    return Ok(());
                }
                _ => event.abandon(),
            }
        }
        if let Some(action) = event.claim_directories() {
            return match action {
                DirectoriesActions::FolderUp | DirectoriesActions::FolderDown => {
                    let dir = if matches!(action, DirectoriesActions::FolderUp) {
                        -1
                    } else {
                        1
                    };
                    self.current_move(dir, ctx)
                }
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    self.open_or_play(ctx)
                }
                DirectoriesActions::FolderCollapse => self.back_out(ctx),
            };
        }
        self.handle_common_action(event, ctx)?;
        self.handle_global_action(event, ctx)?;
        Ok(())
    }
    fn on_query_finished(
        &mut self,
        id: &'static str,
        mpd_command: MpdQueryResult,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match (id, mpd_command) {
            (PLAYLIST_INFO, MpdQueryResult::SongsList { data, .. }) => {
                modal!(
                    ctx, InfoListModal::builder().column_widths(& [30, 70])
                    .title("Playlist info").rows(data).size((40, 20)).build()
                );
                ctx.render()?;
            }
            (PLAYLIST_KINDS, MpdQueryResult::Any(any)) => {
                if let Ok(kinds) = any.downcast::<HashMap<String, PlaylistKind>>() {
                    self.playlist_kinds = *kinds;
                    if is_visible {
                        ctx.render()?;
                    }
                }
            }
            (FETCH_DATA, MpdQueryResult::DirOrSong { data, path }) => {
                let Some(path) = path else {
                    log::error!(
                        path:?, current_path:? = self.stack().path();
                        "Cannot insert data because path is not provided"
                    );
                    return Ok(());
                };
                self.stack_mut().insert(path, data);
                SongListCore::fetch_data_internal(self, ctx)?;
                ctx.render()?;
            }
            (INIT, MpdQueryResult::DirOrSong { data, path: _ }) => {
                self.stack = DirStack::new(data);
                if let Some(sel) = self.stack.current().selected() {
                    self.fetch_data(sel, ctx)?;
                }
                self.query_playlist_kinds(ctx);
                ctx.render()?;
            }
            (REINIT, MpdQueryResult::DirOrSong { data, .. }) if !is_visible => {
                self.stack = DirStack::new(data);
                if let Some(sel) = self.stack.current().selected() {
                    self.fetch_data(sel, ctx)?;
                }
                self.query_playlist_kinds(ctx);
                ctx.render()?;
            }
            (REINIT, MpdQueryResult::DirOrSong { data, .. }) => {
                let mut new_stack = DirStack::new(data);
                let old_viewport_len = self.stack.current().state.viewport_len();
                let old_content_len = self.stack.current().state.content_len();
                let old_marked = self.stack.current().marked().clone();
                match self.stack.path().as_slice() {
                    [playlist_name] => {
                        let Some((selected_idx, selected_playlist)) = self
                            .stack()
                            .previous()
                            .map(|prev| {
                                prev.selected_with_idx()
                                    .map_or(
                                        (0, playlist_name.as_str()),
                                        |(idx, playlist)| { (idx, playlist.as_path()) },
                                    )
                            }) else {
                            log::error!(
                                stack:? = self.stack();
                                "Reinitializing playlists. Current path sugsests that we are inside a playlist but previous is None"
                            );
                            return Ok(());
                        };
                        let idx_to_select = new_stack
                            .current()
                            .items
                            .iter()
                            .find_position(|item| item.as_path() == selected_playlist)
                            .map_or(selected_idx, |(idx, _)| idx);
                        new_stack.current_mut().state.set_viewport_len(old_viewport_len);
                        new_stack
                            .current_mut()
                            .state
                            .select(Some(idx_to_select), ctx.config.scrolloff);
                        log::debug!(
                            stack:? = new_stack; "Reinitializing playlist stack"
                        );
                        let selected_song = self.stack().current().selected_with_idx();
                        let Some(new_playlist) = new_stack.current().selected() else {
                            return Ok(());
                        };
                        let playlist = new_playlist.as_path().to_owned();
                        new_stack.current_mut().state.set_content_len(old_content_len);
                        new_stack.current_mut().state.set_viewport_len(old_viewport_len);
                        let songs = ctx
                            .query_sync(move |client| {
                                Ok(client.list_playlist_info(&playlist, None)?)
                            })?;
                        let Some(next_path) = new_stack.next_path() else {
                            log::debug!(
                                stack:? = new_stack;
                                "No playlist selected after reinit, not entering"
                            );
                            return Ok(());
                        };
                        new_stack
                            .insert(
                                next_path,
                                songs.into_iter().map(DirOrSong::Song).collect(),
                            );
                        new_stack.enter();
                        if let Some((idx, song)) = selected_song {
                            let idx_to_select = new_stack
                                .current()
                                .items
                                .iter()
                                .find_position(|item| item.as_path() == song.as_path())
                                .map_or(idx, |(idx, _)| idx);
                            new_stack
                                .current_mut()
                                .state
                                .set_viewport_len(old_viewport_len);
                            new_stack
                                .current_mut()
                                .state
                                .select(Some(idx_to_select), ctx.config.scrolloff);
                        }
                        *new_stack.current_mut().marked_mut() = old_marked;
                        self.stack = new_stack;
                        self.query_playlist_kinds(ctx);
                        ctx.render()?;
                    }
                    [] => {
                        let Some((selected_idx, selected_playlist)) = self
                            .stack()
                            .current()
                            .selected_with_idx()
                            .map(|(idx, playlist)| (idx, playlist.as_path())) else {
                            self.stack = new_stack;
                            if let Some(sel) = self.stack.current().selected() {
                                self.fetch_data(sel, ctx)?;
                            }
                            self.query_playlist_kinds(ctx);
                            ctx.render()?;
                            return Ok(());
                        };
                        let idx_to_select = new_stack
                            .current()
                            .items
                            .iter()
                            .find_position(|item| item.as_path() == selected_playlist)
                            .map_or(selected_idx, |(idx, _)| idx);
                        new_stack.current_mut().state.set_viewport_len(old_viewport_len);
                        new_stack
                            .current_mut()
                            .state
                            .select(Some(idx_to_select), ctx.config.scrolloff);
                        self.stack = new_stack;
                        if let Some(sel) = self.stack.current().selected() {
                            self.fetch_data(sel, ctx)?;
                        }
                        self.query_playlist_kinds(ctx);
                    }
                    _ => {
                        log::error!(
                            stack:? = self.stack; "Invalid playlist stack state"
                        );
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
impl SongListCore<DirOrSong, ListState> for PlaylistsPane {
    fn list(&self) -> &Dir<DirOrSong, ListState> {
        self.stack().current()
    }
    fn list_mut(&mut self) -> &mut Dir<DirOrSong, ListState> {
        self.stack_mut().current_mut()
    }
    fn scrollbar_area(&self) -> Option<Rect> {
        BrowserPane::scrollbar_area(self)
    }
    fn list_area(&self) -> Option<Rect> {
        BrowserPane::list_area(self)
    }
    fn open(&mut self, autoplay: bool, ctx: &Ctx) -> Result<()> {
        BrowserPane::open(self, autoplay, ctx)
    }
    fn leave(&mut self, ctx: &Ctx) -> Result<()> {
        BrowserPane::leave(self, ctx)
    }
    fn fetch_data_internal(&mut self, ctx: &Ctx) -> Result<()> {
        BrowserPane::fetch_data_internal(self, ctx)
    }
    fn enqueue<'a>(
        &self,
        items: impl Iterator<Item = &'a DirOrSong>,
    ) -> (Vec<Enqueue>, Option<usize>) {
        BrowserPane::enqueue(self, items)
    }
    fn initial_playlist_name(&self, all: bool) -> Option<String> {
        BrowserPane::initial_playlist_name(self, all)
    }
    fn list_songs_in_item(
        &self,
        item: DirOrSong,
    ) -> impl FnOnce(&mut Client<'_>) -> Result<Vec<Song>> + Clone + 'static {
        move |client| {
            Ok(
                match item {
                    DirOrSong::Dir { name, .. } => {
                        client.list_playlist_info(&name, None)?
                    }
                    DirOrSong::Song(song) => vec![song.clone()],
                },
            )
        }
    }
    fn fetch_data(&self, selected: &DirOrSong, ctx: &Ctx) -> Result<()> {
        match self.stack.path().as_slice() {
            [] => {
                let DirOrSong::Dir { name: playlist, .. } = selected else {
                    log::error!(
                        selected:? = selected; "Expected playlist to be selected"
                    );
                    return Ok(());
                };
                let path = self.stack.next_path();
                let playlist = playlist.to_owned();
                ctx.query()
                    .id(FETCH_DATA)
                    .replace_id("playlists_data")
                    .target(PaneType::Playlists {
                        tree: TreeBrowserArgs::default(),
                    })
                    .query(move |client| {
                        let data = client
                            .list_playlist_info(&playlist, None)?
                            .into_iter()
                            .map(DirOrSong::Song)
                            .collect_vec();
                        Ok(MpdQueryResult::DirOrSong {
                            data,
                            path,
                        })
                    });
            }
            _ => {}
        }
        Ok(())
    }
    fn show_info(&self, item: &DirOrSong, ctx: &Ctx) -> Result<()> {
        match item {
            DirOrSong::Dir { name, .. } => {
                let playlist = name.clone();
                ctx.query()
                    .target(PaneType::Playlists {
                        tree: TreeBrowserArgs::default(),
                    })
                    .replace_id(PLAYLIST_INFO)
                    .id(PLAYLIST_INFO)
                    .query(move |client| {
                        let playlist = client.list_playlist_info(&playlist, None)?;
                        Ok(MpdQueryResult::SongsList {
                            data: playlist,
                            path: None,
                        })
                    });
            }
            DirOrSong::Song(_) => {}
        }
        Ok(())
    }
    fn delete<'a>(
        &self,
        items: impl Iterator<Item = (usize, &'a DirOrSong)>,
    ) -> Vec<MpdDelete> {
        match self.stack().path().as_slice() {
            [playlist] => {
                let playlist: Arc<str> = Arc::from(playlist.as_str());
                items
                    .filter_map(|(idx, item)| match item {
                        DirOrSong::Dir { .. } => None,
                        DirOrSong::Song(_) => {
                            Some(MpdDelete::SongInPlaylist {
                                playlist: Arc::clone(&playlist),
                                range: SingleOrRange::single(idx),
                            })
                        }
                    })
                    .collect_vec()
            }
            [] => {
                items
                    .filter_map(|(_, item)| match item {
                        DirOrSong::Dir { name, .. } => {
                            Some(MpdDelete::Playlist {
                                name: name.clone(),
                            })
                        }
                        DirOrSong::Song(_) => None,
                    })
                    .collect_vec()
            }
            _ => Vec::new(),
        }
    }
    fn can_rename(&self, item: &DirOrSong) -> bool {
        matches!(item, DirOrSong::Dir { .. })
    }
    fn rename(item: &DirOrSong, ctx: &Ctx) -> Result<()> {
        match item {
            DirOrSong::Dir { name: d, .. } => {
                let current_name = d.clone();
                modal!(
                    ctx, InputModal::new(ctx).title("Rename playlist")
                    .confirm_label("Rename").input_label("New name:")
                    .initial_value(current_name.clone()).on_confirm(move | ctx, new_value
                    | { if current_name != new_value { let current_name = current_name
                    .clone(); let new_value = new_value.to_owned(); ctx.command(move |
                    client | { client.rename_playlist(& current_name, & new_value) ?;
                    status_info!("Playlist '{}' renamed to '{}'", current_name,
                    new_value); Ok(()) }); } Ok(()) })
                );
            }
            DirOrSong::Song(_) => {}
        }
        Ok(())
    }
    fn move_selected(&mut self, direction: MoveDirection, ctx: &Ctx) -> Result<()> {
        let Some(DirOrSong::Dir { name: playlist, .. }) = self
            .stack
            .previous()
            .and_then(|p| p.selected()) else {
            return Ok(());
        };
        if self.stack().current().marked().is_empty() {
            let Some(idx) = self
                .stack()
                .current()
                .selected_with_idx()
                .map(|(idx, _)| idx) else {
                status_warn!("Cannot move because no item is selected");
                return Ok(());
            };
            let new_idx = match direction {
                MoveDirection::Up => idx.saturating_sub(1),
                MoveDirection::Down => {
                    (idx + 1).min(self.stack().current().items.len() - 1)
                }
            };
            let playlist = playlist.clone();
            ctx.query_sync(move |client| {
                client
                    .move_in_playlist(&playlist, &SingleOrRange::single(idx), new_idx)?;
                Ok(())
            })?;
            self.stack_mut().current_mut().items.swap(idx, new_idx);
            self.stack_mut().current_mut().select_idx(new_idx, ctx.config.scrolloff);
        } else {
            match direction {
                MoveDirection::Up => {
                    if let Some(0) = self.stack().current().marked().first() {
                        return Ok(());
                    }
                }
                MoveDirection::Down => {
                    if let Some(last_idx) = self.stack().current().marked().last()
                        && *last_idx == self.stack().current().items.len() - 1
                    {
                        return Ok(());
                    }
                }
            }
            let playlist = playlist.clone();
            let ranges = self.stack().current().marked().ranges().collect_vec();
            ctx.query_sync(move |client| {
                for range in ranges {
                    let idx = range.start();
                    let new_idx = match direction {
                        MoveDirection::Up => idx.saturating_sub(1),
                        MoveDirection::Down => idx + 1,
                    };
                    client.move_in_playlist(&playlist, &(range.into()), new_idx)?;
                }
                Ok(())
            })?;
            let mut new_marked = BTreeSet::new();
            for marked in self.stack().current().marked() {
                match direction {
                    MoveDirection::Up => {
                        new_marked.insert(marked.saturating_sub(1));
                    }
                    MoveDirection::Down => {
                        new_marked.insert(*marked + 1);
                    }
                }
            }
            *self.stack_mut().current_mut().marked_mut() = new_marked;
            return Ok(());
        }
        ctx.render()?;
        Ok(())
    }
}
impl BrowserPane<DirOrSong> for PlaylistsPane {
    fn stack(&self) -> &DirStack<DirOrSong, ListState> {
        &self.stack
    }
    fn stack_mut(&mut self) -> &mut DirStack<DirOrSong, ListState> {
        &mut self.stack
    }
    fn browser_areas(&self) -> EnumMap<BrowserArea, Rect> {
        self.browser.areas
    }
}
