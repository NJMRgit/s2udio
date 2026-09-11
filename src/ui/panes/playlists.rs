use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};
use anyhow::{Context, Result, bail};
use enum_map::EnumMap;
use itertools::Itertools;
use ratatui::{
    Frame, layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};
use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{CommonAction, DirectoriesActions, GlobalAction},
        tabs::{PaneType, PaneTypeDiscriminants, TreeBrowserArgs},
    },
    ctx::Ctx,
    mpd::{
        client::Client,
        commands::{
            Song, lsinfo::LsInfoEntry, metadata_tag::MetadataTag,
        },
        mpd_client::{MpdClient, SingleOrRange},
    },
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
/// prefix (from the background classification) — or the ♫ prefix for
/// library playlist files (read-only) —, stream songs inside a
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
                DirOrSong::Dir { name, full_path, playlist: true, .. } => {
                    let library = !full_path.is_empty();
                    let prefix = if library {
                        "♫ "
                    } else {
                        kinds
                            .get(name.as_str())
                            .copied()
                            .unwrap_or(PlaylistKind::Audio)
                            .prefix()
                    };
                    ListItem::from(
                        Line::from(
                            vec![
                                Span::from(prefix), Span::from(if name.is_empty() {
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
    /// Library-relative path of the currently opened playlist when it is a
    /// library playlist file (`.m3u`/`.pls`/`.xspf`, read-only); `None`
    /// for stored playlists and at the root. Guards the stored-playlist-
    /// only operations (rename / delete / clear / move) and the song
    /// menu's remove-from-playlist.
    open_library_playlist: Option<String>,
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
    /// Round 60 (B1): the active mode (Playlists browser or Search). The
    /// search state lives for the session.
    mode: PlaylistsTabMode,
    /// Click areas of the toggle row's two labels (Playlists, Search).
    toggle_areas: [Rect; 2],
    /// Click area of the `↰ Back` button (round 60 B3), refreshed on every
    /// render; zero when hidden.
    back_area: Rect,
    /// Buffer id of the search query input (session-lived).
    search_buffer: crate::ui::input::BufferId,
    /// Keyboard phase of the search mode: true = the `Search:` input row
    /// is focused, false = the results list is focused. Round 73.2: starts
    /// false and is only set by the `S` jump — Shift+Tab / the toggle click
    /// no longer focus the input.
    search_input_focused: bool,
    /// Consecutive Left presses at the search bar (round 63.1): the first
    /// Left navigates the text cursor, the second consecutive Left exits
    /// the search exactly like Esc #2. Reset by any other input result.
    search_left_presses: u8,
    /// Set once the per-session playlist-songs snapshot was requested.
    search_songs_loaded: bool,
    /// Every playlist's songs, fetched once per session (used by the
    /// search; the cache makes re-searches instant).
    search_playlists: Vec<SearchPlaylist>,
    /// The current search results (playlist-name matches first, then
    /// song-inside-playlist matches).
    search_results: Dir<DirOrSong, ListState>,
    /// Area of the search results list (mouse math).
    search_results_area: Rect,
    /// A playlist opened from search while the root list was still
    /// loading: (name, library path), enacted once the root list lands.
    pending_open: Option<(String, String)>,
}
const SEARCH_DATA: &str = "playlists_search_data";
#[allow(dead_code)]
const _SEARCH_ORDER: () = ();
/// Whether a song matches the (lowercased) search query: the title, artist
/// or album tag, or the file path contains it (case-insensitive).
fn song_matches_query(song: &Song, query: &str) -> bool {
    if song.file.to_lowercase().contains(query) {
        return true;
    }
    fn tag_contains(tag: &crate::mpd::commands::metadata_tag::MetadataTag, query: &str) -> bool {
        match tag {
            crate::mpd::commands::metadata_tag::MetadataTag::Single(v) => {
                v.to_lowercase().contains(query)
            }
            crate::mpd::commands::metadata_tag::MetadataTag::Multiple(items) => {
                items.iter().any(|v| v.to_lowercase().contains(query))
            }
        }
    }
    ["title", "artist", "album"].iter().any(|key| {
        song.metadata.get(*key).is_some_and(|tag| tag_contains(tag, query))
    })
}
/// The rows of the search results: playlist-name matches render like the
/// playlist list (♪ / ▶ / ♫ prefix + name), song matches render like
/// playlist songs with a dim `· in <playlist>`-style suffix.
fn search_result_items(
    items: &[DirOrSong],
    marked: &BTreeSet<usize>,
    hovered: Option<usize>,
    ctx: &Ctx,
    kinds: &HashMap<String, PlaylistKind>,
    sources: &HashMap<String, String>,
) -> Vec<ListItem<'static>> {
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let config = &ctx.config;
            let mut list_item = match item {
                DirOrSong::Dir { name, full_path, playlist: true, .. } => {
                    let library = !full_path.is_empty();
                    let prefix = if library {
                        "♫ "
                    } else {
                        kinds
                            .get(name.as_str())
                            .copied()
                            .unwrap_or(PlaylistKind::Audio)
                            .prefix()
                    };
                    let mut line = Line::from(
                        vec![
                            Span::from(prefix),
                            Span::from(if name.is_empty() { "Untitled".to_owned() } else { name.clone() }),
                        ],
                    );
                    line.push_span(Span::styled("  [playlist]", config.as_list_text_style()));
                    ListItem::from(line)
                }
                DirOrSong::Song(song) => {
                    let config2 = &ctx.config;
                    let mut spans = vec![
                        Span::styled(
                            config2.theme.symbols.song.clone(),
                            config2.theme.symbols.song_style.unwrap_or_default(),
                        ),
                        Span::from(" "),
                    ];
                    spans.extend(
                        config2
                            .theme
                            .browser_song_format
                            .0
                            .iter()
                            .map(|prop| {
                                Span::from(
                                    prop.as_string(
                                        Some(song),
                                        &config2.theme.format_tag_separator,
                                        config2.theme.multiple_tag_resolution_strategy,
                                        ctx,
                                    )
                                    .unwrap_or_default(),
                                )
                            }),
                    );
                    // The match source (round 60 B1): a dim `in <playlist>`
                    // suffix so song-inside-playlist matches are explicit.
                    if let Some(source) = sources.get(&song.file) {
                        spans.push(Span::styled(
                            format!("  · in {source}"),
                            config2.as_list_text_style(),
                        ));
                    }
                    ListItem::from(Line::from(spans))
                }
                DirOrSong::Dir { name, .. } => ListItem::from(
                    Line::from(Span::raw(if name.is_empty() { "Untitled".to_owned() } else { name.clone() })),
                ),
            };
            if marked.contains(&idx) {
                list_item = list_item.style(config.theme.marked_item_style);
            } else if hovered == Some(idx) {
                list_item = list_item.style(config.theme.hovered_item_style);
            }
            list_item
        })
        .collect()
}
const INIT: &str = "init";
const REINIT: &str = "reinit";
const FETCH_DATA: &str = "fetch_data";
const PLAYLIST_INFO: &str = "preview";
/// Result id of the background query classifying every playlist (audio /
/// video) from its first entry; the prefix icons in the playlist list are
/// drawn from it.
const PLAYLIST_KINDS: &str = "playlist_kinds";
/// The tab's mode (round 60 B1): the playlist browser, or search across
/// playlist names + the songs inside playlists. Startup default:
/// Playlists; the state (query + results) lives for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaylistsTabMode {
    Playlists,
    Search,
}

/// One playlist's search snapshot (every playlist the tab lists, with its
/// songs fetched once per session): `full_path` is the library-relative
/// path for read-only library playlist files (empty for stored
/// playlists), `name` the display name.
#[derive(Debug, Clone)]
struct SearchPlaylist {
    name: String,
    full_path: String,
    songs: Vec<Song>,
}

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
            open_library_playlist: None,
            info_state: ListState::default(),
            info_area: Rect::default(),
            info_items_len: 0,
            info_key: None,
            tree_args: ctx.config.tree_browser_args(PaneTypeDiscriminants::Playlists),
            info_scrollbar_area: Rect::default(),
            info_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            mode: PlaylistsTabMode::Playlists,
            toggle_areas: [Rect::default(); 2],
            back_area: Rect::default(),
            search_buffer: crate::ui::input::BufferId::new(),
            // Round 73.2: the search page opens unfocused (Shift+Tab no
            // longer grabs the input; `s` does).
            search_input_focused: false,
            search_left_presses: 0,
            search_songs_loaded: false,
            search_playlists: Vec::new(),
            search_results: Dir::new(Vec::new()),
            search_results_area: Rect::default(),
            pending_open: None,
        }
    }
    fn render_playlists(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let focused = self.stack.path().is_empty();
        let items_snapshot = self.stack.root().items.clone();
        let marked = self.stack.root().marked().clone();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style());
        let inner = block.inner(area);
        let content = crate::ui::item_box_header(frame, inner, "Playlists", ctx);
        let (list_area, scrollbar_area) = Self::split_scrollbar(content, ctx);
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
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                scrollbar_area,
                state.as_scrollbar_state_ref(),
            );
        }
        ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
        crate::ui::connect_box_divider(frame, area, area.y + 2, ctx);
        self.playlists_area = list_area;
        self.playlists_scrollbar_area = scrollbar_area;
    }
    /// Split `inner` into the list area and the 4-cell scrollbar strip
    /// (glyph column + the one-cell left / two-cell right margins; the
    /// whole strip is the click/drag activation area, round 60 A6).
    fn split_scrollbar(inner: Rect, ctx: &Ctx) -> (Rect, Rect) {
        if ctx.config.theme.scrollbar.is_some() {
            crate::ui::scrollbar_strip(inner)
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
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style());
        let inner = block.inner(area);
        // Round 60 (A2/A7): the Item Box template — a plain title row (one
        // character margin), a connected separator, then the list.
        let content = crate::ui::item_box_header(frame, inner, title.trim(), ctx);
        let (list_area, scrollbar_area) = Self::split_scrollbar(content, ctx);
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
                crate::ui::render_scrollbar_strip(
                    frame,
                    scrollbar,
                    scrollbar_area,
                    state.as_scrollbar_state_ref(),
                );
            }
            ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
            crate::ui::connect_box_divider(frame, area, area.y + 2, ctx);
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
                crate::ui::render_scrollbar_strip(
                    frame,
                    scrollbar,
                    scrollbar_area,
                    state.as_scrollbar_state_ref(),
                );
            }
            ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
            crate::ui::connect_box_divider(frame, area, area.y + 2, ctx);
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
                if let DirOrSong::Dir { name, full_path, .. } = playlist {
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
                    // Library playlist file: show its library-relative path.
                    if !full_path.is_empty() {
                        items
                            .push(
                                ListItem::new(
                                    Line::from(
                                        vec![
                                            Span::styled("Path", key_style), Span::raw(": "),
                                            Span::raw(full_path.clone()),
                                        ],
                                    ),
                                ),
                            );
                    }
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
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(area);
        let overflow = items.len() > inner.height as usize;
        let (list_area, scrollbar_area) = if overflow
            && ctx.config.as_styled_scrollbar().is_some()
        {
            crate::ui::scrollbar_strip(inner)
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
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                scrollbar_area,
                &mut ratatui::widgets::ScrollbarState::new(max_offset + 1)
                    .position(position)
                    .viewport_content_length(list_area.height as usize),
            );
        }
        self.info_scrollbar_area = scrollbar_area;
    }
    /// Mouse handling of the search mode: clicks on the `Search:` input
    /// focus it, clicks on the results select/mark (ctrl additive, alt
    /// ranges), double-click activates, right-click opens the options
    /// menu, the wheel scrolls the results.
    fn handle_search_mouse(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let position: ratatui::layout::Position = event.into();
        // The input row: any click between the toggle row and the results
        // list focuses the input.
        if matches!(event.kind, MouseEventKind::LeftClick)
            && self.search_results_area.y > 0
            && self.toggle_areas[0].y > 0
            && position.y > self.toggle_areas[0].y
            && position.y < self.search_results_area.y
        {
            self.focus_search_input(ctx);
            ctx.render()?;
            return Ok(());
        }
        let area = self.search_results_area;
        if !area.contains(position) {
            return Ok(());
        }
        let row = usize::from(event.y.saturating_sub(area.y)) + self.search_results.state.offset();
        match event.kind {
            MouseEventKind::LeftClick
                if event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                if let Some(idx) = self.search_results.state.get_at_rendered_row(row) {
                    let dir = &mut self.search_results;
                    dir.select_idx(idx, ctx.config.scrolloff);
                    dir.state.toggle_mark(idx);
                    dir.state.band.arm(idx, false);
                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick
                if event.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
            {
                if let Some(idx) = self.search_results.state.get_at_rendered_row(row) {
                    let dir = &mut self.search_results;
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
                if let Some(idx) = self.search_results.state.get_at_rendered_row(row) {
                    let click_on_different = !self.search_results.state.marked.is_empty()
                        && Some(idx) != self.search_results.state.get_selected();
                    self.search_results.state.band.arm(idx, click_on_different);
                    self.search_results.select_idx(idx, ctx.config.scrolloff);
                    self.search_results.state.set_mark_anchor(idx);
                    self.search_results.state.clear_range_mark();
                    self.release_search_input(ctx);
                    ctx.render()?;
                }
            }
            MouseEventKind::DoubleClick => {
                self.search_results.state.band.cancel();
                self.search_activate(ctx)?;
            }
            MouseEventKind::RightClick => {
                self.search_results.state.band.cancel();
                let Some(idx) = self.search_results.state.get_at_rendered_row(row) else {
                    return Ok(());
                };
                self.search_results.select_idx(idx, ctx.config.scrolloff);
                self.release_search_input(ctx);
                return self.search_context_menu(ctx);
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                self.search_move(dir, ctx)?;
            }
            _ => {}
        }
        Ok(())
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
        self.open_library_playlist = None;
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
            let selected = self.stack.root().selected().cloned();
            if let Some(DirOrSong::Dir { name, full_path, .. }) = selected {
                let library_path = (!full_path.is_empty()).then_some(full_path);
                return self.open_playlist_menu(&name, library_path.as_deref(), ctx, anchor);
            }
            return Ok(());
        }
        self.open_song_menu(ctx, anchor)
    }
    /// Right-click / Enter menu for a playlist (anchored at the cursor
    /// when opened with the mouse). Library playlist files get only the
    /// queue actions — they are user-owned files on disk and read-only
    /// from the app (no rename / delete).
    fn open_playlist_menu(
        &mut self,
        name: &str,
        library_path: Option<&str>,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        let playlist = name.to_owned();
        let library_path = library_path.map(str::to_owned);
        let menu = MenuModal::new(ctx)
            .anchor(anchor)
            .list_section(
                ctx,
                |mut section| {
                    let playlist = playlist.clone();
                    let library_path = library_path.clone();
                    section
                        .add_item(
                            "Add to Queue",
                            {
                                let playlist = playlist.clone();
                                let library_path = library_path.clone();
                                move |ctx| {
                                    ctx.command(move |client| {
                                        match playlist_menu_items(
                                            client, &playlist, library_path.as_deref(),
                                        ) {
                                            Ok(items) => {
                                                client.enqueue_multiple(items, None, None, false)?;
                                            }
                                            Err(err) => status_warn!("{err:#}"),
                                        }
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
                                let library_path = library_path.clone();
                                move |ctx| {
                                    ctx.command(move |client| {
                                        match playlist_menu_items(
                                            client, &playlist, library_path.as_deref(),
                                        ) {
                                            Ok(items) => {
                                                client.enqueue_multiple(items, None, None, true)?;
                                            }
                                            Err(err) => status_warn!("{err:#}"),
                                        }
                                        Ok(())
                                    });
                                    Ok(())
                                }
                            },
                        );
                    Some(section)
                },
            );
        let menu = if library_path.is_none() {
            menu.list_section(
                ctx,
                |mut section| {
                    let playlist = playlist.clone();
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
            )
        } else {
            menu
        };
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
        // Library playlist files are read-only: no remove/clear/move via
        // MPD (they are user-owned files on disk, and a same-named stored
        // playlist must not be edited by mistake).
        let library = self.open_library_playlist.is_some();
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
                    let library = library;
                    if !library {
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
                    }
                    Some(section)
                },
            );
        crate::shared::macros::modal!(ctx, menu);
        Ok(())
    }
    fn open_selected_playlist(&mut self, ctx: &Ctx) -> Result<()> {
        // Record whether the entered playlist is a read-only library file
        // (guards the stored-playlist-only song ops while inside).
        self.open_library_playlist = self.stack.root().selected().and_then(|item| match item {
            DirOrSong::Dir { full_path, .. } if !full_path.is_empty() => {
                Some(full_path.clone())
            }
            _ => None,
        });
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
                // Library playlist files are read-only and never classify
                // as stored playlists (their ♫ marker replaces ♪ / ▶).
                DirOrSong::Dir { name, full_path, .. } if full_path.is_empty() => {
                    Some(name.clone())
                }
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

/// One library playlist file found by the scoped enumeration walk —
/// its library-relative MPD path plus the timestamp MPD reports.
struct LibraryPlaylistFile {
    full_path: String,
    last_modified: chrono::DateTime<chrono::Utc>,
}

/// All library playlist files (`.m3u` / `.pls` / `.xspf`) reachable from
/// the MPD database root, via a scoped per-directory `lsinfo` walk.
///
/// A bare `listall` only reports `playlist:` entries at the library root
/// on live MPD 0.24.14 — the 28 real library `.m3u` files are all nested
/// inside album folders and would be missed entirely (round-49 host
/// finding). `lsinfo <uri>` returns a directory's direct children
/// (including `playlist:` entries, verified live for nested files), so
/// walking every directory the tree reports finds them. Each call is
/// exact/non-recursive, so the walk does not depend on any server-side
/// recursion quirk; directories are visited once.
fn list_library_playlist_files(client: &mut Client<'_>) -> Result<Vec<LibraryPlaylistFile>> {
    let mut found = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    // None = the database root (bare `lsinfo`).
    let mut pending: Vec<Option<String>> = vec![None];
    while let Some(dir) = pending.pop() {
        if !visited.insert(dir.clone().unwrap_or_default()) {
            continue;
        }
        let entries = client
            .lsinfo(dir.as_deref())
            .with_context(|| format!("Cannot list library {}", dir.as_deref().unwrap_or("")))?;
        for entry in entries {
            match entry {
                LsInfoEntry::Dir(dir) => pending.push(Some(dir.full_path)),
                LsInfoEntry::Playlist(playlist) => found.push(LibraryPlaylistFile {
                    full_path: playlist.full_path,
                    last_modified: playlist.last_modified,
                }),
                LsInfoEntry::File(_) => {}
            }
        }
    }
    found.sort_by(|a, b| a.full_path.cmp(&b.full_path));
    found.dedup_by(|a, b| a.full_path == b.full_path);
    Ok(found)
}

/// Both Playlists-tab refresh paths (before_show + Database /
/// StoredPlaylist events) build the root list from the stored playlists
/// and — when the Settings > MPD toggle is on — the library playlist
/// files (`.m3u` / `.pls` / `.xspf`, the radio favourites name excluded),
/// sorted together by name (existing comparator). Library files are
/// listed read-only: `full_path` carries the library-relative path.
fn playlist_root_items(
    client: &mut Client<'_>,
    show_library_files: bool,
    radio_playlist: &str,
    compare: &StringCompare,
) -> Result<Vec<DirOrSong>> {
    let mut items: Vec<DirOrSong> = client
        .list_playlists()
        .context("Cannot list playlists")?
        .into_iter()
        .filter(|playlist| playlist.name != radio_playlist)
        .map(|playlist| DirOrSong::playlist_name_only(playlist.name))
        .collect();
    if show_library_files {
        items.extend(
            list_library_playlist_files(client)?.into_iter().filter_map(|file| {
                if !crate::shared::playlist_file::is_library_playlist_path(&file.full_path) {
                    return None;
                }
                let name = crate::shared::playlist_file::playlist_stem(&file.full_path);
                if name.is_empty() || name == radio_playlist {
                    return None;
                }
                Some(DirOrSong::Dir {
                    name,
                    full_path: file.full_path,
                    last_modified: file.last_modified,
                    playlist: true,
                })
            }),
        );
    }
    items.sort_by(|a, b| match (a, b) {
        (DirOrSong::Dir { name: an, .. }, DirOrSong::Dir { name: bn, .. }) => {
            compare.compare(an, bn)
        }
        _ => compare.compare(a.as_path(), b.as_path()),
    });
    Ok(items)
}

/// The music library root the pane reads library playlist files from:
/// the Settings > MPD "library location" choice (state.ron, current
/// session; the update/rescan scope), else the `music_directory` parsed
/// from the mpd.conf candidates; `None` when neither is available — the
/// pane then warns in the status bar and skips local parsing.
fn library_music_dir() -> Option<String> {
    let session = crate::config::state::AppStateFile::load()
        .mpd_library_path
        .filter(|path| !path.trim().is_empty());
    session.or_else(crate::ui::modals::paste::music_directory)
}

/// The opened entries of a library playlist file as `Song`s: the title /
/// artist come from the playlist metadata, the duration only when the
/// format carries one. Errors (unknown music directory, unreadable file)
/// show a status warning and yield an empty list — library playlist files
/// are best-effort read-only extras.
fn read_library_playlist_songs(full_path: &str) -> Vec<Song> {
    match library_music_dir() {
        Some(root) => {
            match crate::shared::playlist_file::read_library_playlist(&root, full_path) {
                Ok(entries) => entries.into_iter().map(library_entry_to_song).collect(),
                Err(err) => {
                    status_warn!("{err:#}");
                    Vec::new()
                }
            }
        }
        None => {
            status_warn!(
                "MPD music directory unknown — cannot read library playlist '{full_path}' (set it in Settings > MPD or mpd.conf)"
            );
            Vec::new()
        }
    }
}

/// One parsed library-playlist entry as a playable `Song` row/queue item.
fn library_entry_to_song(entry: crate::shared::playlist_file::PlaylistEntry) -> Song {
    let mut metadata = HashMap::new();
    if let Some(title) = entry.title.filter(|title| !title.is_empty()) {
        metadata.insert("title".to_owned(), MetadataTag::Single(title));
    }
    if let Some(artist) = entry.artist.filter(|artist| !artist.is_empty()) {
        metadata.insert("artist".to_owned(), MetadataTag::Single(artist));
    }
    Song {
        file: entry.uri,
        duration: entry.duration_ms.map(std::time::Duration::from_millis),
        metadata,
        ..Default::default()
    }
}

/// The queue items behind a root playlist row's "Add / Replace Queue":
/// stored playlists read MPD's playlist contents; library playlist files
/// are parsed locally (read-only — the app never edits or deletes them).
fn playlist_menu_items(
    client: &mut Client<'_>,
    playlist: &str,
    library_path: Option<&str>,
) -> Result<Vec<Enqueue>> {
    if let Some(path) = library_path {
        let Some(root) = library_music_dir() else {
            bail!(
                "MPD music directory unknown — cannot read '{path}' (set it in Settings > MPD or mpd.conf)"
            );
        };
        let entries = crate::shared::playlist_file::read_library_playlist(&root, path)?;
        return Ok(entries.into_iter().map(|entry| Enqueue::File { path: entry.uri }).collect());
    }
    let songs = client.list_playlist_info(playlist, None)?;
    Ok(songs.into_iter().map(|song| Enqueue::File { path: song.file }).collect())
}


impl PlaylistsPane {
    fn render_toggle(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        self.toggle_areas = [Rect::default(); 2];
        self.back_area = Rect::default();
        if area.height == 0 {
            return;
        }
        let segments = [
            crate::ui::widgets::sub_tab_bar::Segment {
                label: "Playlists",
                active: self.mode == PlaylistsTabMode::Playlists,
            },
            crate::ui::widgets::sub_tab_bar::Segment {
                label: "Search",
                active: self.mode == PlaylistsTabMode::Search,
            },
        ];
        let x = area.x.saturating_add(1);
        let bar = crate::ui::widgets::sub_tab_bar::SubTabBar::new(
            &segments,
            x,
            area.y,
            area.right().saturating_sub(1),
        );
        let areas = bar.render(frame, ctx);
        for (idx, seg_area) in areas.into_iter().take(2).enumerate() {
            self.toggle_areas[idx] = seg_area;
        }
        // Round 60 (B3): `↰ Back` at the right end of the row, visible
        // only inside a playlist (hidden at the playlist-list root and in
        // search mode).
        let visible = self.mode == PlaylistsTabMode::Playlists
            && !self.stack.path().is_empty();
        self.back_area = crate::ui::draw_back_button(frame, area, visible, ctx);
    }
    /// Library mode: the playlist tree/list + songs + info layout.
    fn render_library(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
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
        // Round 60c (S3): the 3-row legend strip is gone — the songs list
        // and the info box reclaim the space.
        let info_h = self
            .tree_args
            .info_box_height(right.height * 2 / 3);
        let songs_h = right.height.saturating_sub(info_h);
        let [songs_area, info_area] = Layout::vertical([
                Constraint::Length(songs_h),
                Constraint::Length(info_h),
            ])
            .areas(right);
        self.browser.areas[BrowserArea::Previous] = Rect::default();
        self.browser.areas[BrowserArea::Preview] = Rect::default();
        self.render_playlists(frame, playlists_area, ctx);
        self.render_songs(frame, songs_area, ctx);
        self.render_info(frame, info_area, ctx);
        Ok(())
    }

    // ── search mode (round 60 B1) ────────────────────────────────────

    /// Search mode: `Search:` input row, a connected separator, the
    /// scrollable ` Results ` list and the info box (the Item Box / Info
    /// Box templates). Results show the match source: plain playlist rows
    /// for playlist-name matches, song rows with a dim `in <playlist>`
    /// suffix for matches inside a playlist.
    fn render_search(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        self.ensure_search_songs(ctx);
        // Round 60c (S1): ONE combined frame — the `Search:` input row on
        // top, a connected `├───…───┤` divider, the results list filling
        // the same box, and the `Results` label on the bottom edge
        // (`╰─Results───…──╯`). The Info Box stays below.
        let [frame_area, info_area] = Layout::vertical([
                Constraint::Min(4),
                Constraint::Percentage(33),
            ])
            .areas(area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style())
            .title_bottom(ratatui::text::Line::from("─Results"));
        let inner = block.inner(frame_area);
        // Render the box first so the connected divider's `├`/`┤`
        // junctions can overwrite the box's border cells at that row.
        frame.render_widget(block, frame_area);
        let query = ctx.input.value(self.search_buffer);
        let content = crate::ui::render_search_frame_top(
            frame,
            inner,
            &query,
            self.search_input_focused,
            ctx,
        );
        let (list_area, scrollbar_area) = if ctx.config.theme.scrollbar.is_some() {
            crate::ui::scrollbar_strip(content)
        } else {
            (content, Rect::default())
        };
        self.search_results_area = list_area;
        let sources = self.search_sources();
        let dir = &mut self.search_results;
        dir.state
            .set_content_and_viewport_len(dir.items.len(), list_area.height.into());
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            list_area,
            dir.state.offset(),
            dir.items.len(),
            1,
        );
        let items = search_result_items(
            &dir.items,
            &dir.state.marked,
            hover_idx,
            ctx,
            &self.playlist_kinds,
            &sources,
        );
        ratatui::widgets::StatefulWidget::render(
            crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
                .highlight_style(
                    if hover_idx == dir.state.get_selected() || !self.search_input_focused {
                        ctx.config.theme.hovered_item_style
                    } else {
                        ctx.config.theme.current_item_style
                    },
                )
                .style(ctx.config.as_list_name_style()),
            list_area,
            frame.buffer_mut(),
            dir.state.as_render_state_ref(),
        );
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && scrollbar_area.width > 0
        {
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                scrollbar_area,
                dir.state.as_scrollbar_state_ref(),
            );
        }

        // The info box: the selected result's details.
        self.render_search_info(frame, info_area, ctx);
        Ok(())
    }
    /// The info box of the search mode: playlist details for a
    /// playlist-name match, the song preview for a song-inside-playlist
    /// match.
    fn render_search_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let mut items: Vec<ListItem> = Vec::new();
        let key_style = ctx.config.theme.preview_label_style;
        let group = ctx.config.theme.preview_metadata_group_style;
        if let Some(selected) = self.search_results.selected().cloned() {
            match selected {
                DirOrSong::Dir { name, full_path, .. } => {
                    let _matching_path = &full_path;
                    items.push(ListItem::new(Line::styled(" --- [Playlist]", group)));
                    items.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Name", key_style), Span::raw(": "),
                                    Span::raw(name.clone()),
                                ],
                            ),
                        ),
                    );
                    items.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Match", key_style), Span::raw(": "),
                                    Span::styled("playlist name", group),
                                ],
                            ),
                        ),
                    );
                    let count = self
                        .search_playlists
                        .iter()
                        .find(|p| !p.full_path.is_empty() && p.name == name)
                        .or_else(|| {
                            self.search_playlists.iter().find(|p| p.name == name)
                        })
                        .map(|p| p.songs.len())
                        .unwrap_or(0);
                    items.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Tracks", key_style), Span::raw(": "),
                                    Span::raw(count.to_string()),
                                ],
                            ),
                        ),
                    );
                }
                DirOrSong::Song(song) => {
                    for group in song.to_file_preview(ctx) {
                        if let Some(name) = group.name {
                            items.push(
                                ListItem::new(
                                    Line::styled(name, group.header_style.unwrap_or_default()),
                                ),
                            );
                        }
                        items.extend(group.items);
                        items.push(ListItem::new(""));
                    }
                    let source = self.search_playlists.iter().find_map(|p| {
                        p.songs.iter().any(|s| s.file == song.file).then_some(&p.name)
                    });
                    if let Some(playlist) = source {
                        items.push(
                            ListItem::new(Line::styled(" --- [Source]", group)),
                        );
                        items.push(
                            ListItem::new(
                                Line::from(
                                    vec![
                                        Span::styled("In playlist", key_style), Span::raw(": "),
                                        Span::raw(playlist.clone()),
                                    ],
                                ),
                            ),
                        );
                    }
                }
            }
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
    }

    /// Fetch every playlist's songs once per session (stored playlists via
    /// MPD `listplaylistinfo`, library playlist files via the local
    /// parser), then recompute the results.
    fn ensure_search_songs(&mut self, ctx: &Ctx) {
        if self.search_songs_loaded {
            return;
        }
        self.search_songs_loaded = true;
        let library_files: Vec<String> = self
            .stack
            .root()
            .items
            .iter()
            .filter_map(|item| match item {
                DirOrSong::Dir { playlist: true, full_path, .. } if !full_path.is_empty() => {
                    Some(full_path.clone())
                }
                _ => None,
            })
            .collect();
        ctx.query()
            .id(SEARCH_DATA)
            .replace_id(SEARCH_DATA)
            .target(PaneType::Playlists {
                tree: TreeBrowserArgs::default(),
            })
            .query(move |client| {
                let mut plays: Vec<SearchPlaylist> = Vec::new();
                let stored = client.list_playlists()?;
                for pl in stored {
                    if let Ok(songs) = client.list_playlist_info(&pl.name, None) {
                        plays.push(SearchPlaylist {
                            name: pl.name.clone(),
                            full_path: String::new(),
                            songs,
                        });
                    }
                }
                for lib in library_files {
                    let songs = read_library_playlist_songs(&lib);
                    {
                        let stem = std::path::Path::new(&lib)
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| lib.clone());
                        plays.push(SearchPlaylist {
                            name: stem,
                            full_path: lib,
                            songs,
                        });
                    }
                }
                Ok(MpdQueryResult::Any(Box::new(plays)))
            });
    }

    /// Recompute the search results from the query (empty query = empty
    /// results). Playlist-name matches come first, then song matches;
    /// matching is case-insensitive over the name / song tags and path.
    fn search_playlists_local(&mut self, ctx: &Ctx) -> Result<()> {
        let query = ctx.input.value(self.search_buffer).trim().to_lowercase();
        let mut items: Vec<DirOrSong> = Vec::new();
        if !query.is_empty() {
            let mut plays: Vec<SearchPlaylist> = self.search_playlists.clone();
            plays.sort_by(|a, b| a.name.cmp(&b.name));
            for p in &plays {
                if p.name.to_lowercase().contains(&query) {
                    items.push(DirOrSong::Dir {
                        name: p.name.clone(),
                        full_path: p.full_path.clone(),
                        last_modified: chrono::Utc::now(),
                        playlist: true,
                    });
                }
            }
            for p in &plays {
                for song in &p.songs {
                    if song_matches_query(song, &query) {
                        items.push(DirOrSong::Song(song.clone()));
                    }
                }
            }
        }
        self.search_results = Dir::new(items);
        ctx.render()?;
        Ok(())
    }

    /// Song file -> playlist name, for the `in <playlist>` result suffix.
    fn search_sources(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for p in &self.search_playlists {
            for song in &p.songs {
                out.entry(song.file.clone()).or_insert_with(|| p.name.clone());
            }
        }
        out
    }
    /// `d`/`→` (and double-click) on a result: a playlist-name match opens
    /// the playlist (in Library mode), a song plays (replace queue +
    /// autoplay, like the Library pane's `d`).
    fn search_activate(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(selected) = self.search_results.selected().cloned() else {
            return Ok(());
        };
        match selected {
            DirOrSong::Dir { name, full_path, .. } => {
                self.open_search_playlist(&name, &full_path, ctx)
            }
            DirOrSong::Song(song) => {
                let file = song.file.clone();
                ctx.command(move |client| {
                    client.enqueue_multiple(
                        vec![Enqueue::File { path: file }],
                        None,
                        None,
                        true,
                    )?;
                    Ok(())
                });
                Ok(())
            }
        }
    }

    /// Switch to Library mode and open the playlist `name` (its songs
    /// list), after ensuring the root list is loaded.
    fn open_search_playlist(
        &mut self,
        name: &str,
        full_path: &str,
        ctx: &Ctx,
    ) -> Result<()> {
        self.mode = PlaylistsTabMode::Playlists;
        if self.stack.path().is_empty() && self.stack.root().items.is_empty() {
            self.pending_open = Some((name.to_owned(), full_path.to_owned()));
            self.initialized = false;
            self.before_show(ctx)?;
            return Ok(());
        }
        self.select_and_open(name, full_path, ctx)
    }

    /// Select the playlist item at the root and enter it (the shared
    /// songs-pane flow).
    fn select_and_open(
        &mut self,
        name: &str,
        full_path: &str,
        ctx: &Ctx,
    ) -> Result<()> {
        let Some(idx) = self.stack.root().items.iter().position(|item| match item {
            DirOrSong::Dir { playlist: true, name: n, full_path: p, .. } => {
                n == name && p == full_path
            }
            _ => false,
        }) else {
            return Ok(());
        };
        let root = self.stack.root_mut();
        root.select_idx(idx, ctx.config.scrolloff);
        let _ = root;
        self.open_library_playlist = if full_path.is_empty() {
            None
        } else {
            Some(full_path.to_owned())
        };
        self.stack_mut().enter();
        SongListCore::fetch_data_internal(self, ctx)?;
        ctx.render()?;
        Ok(())
    }
    /// Flip Playlists <-> Search (Shift+Tab / toggle click). The search
    /// state (query, results, phase) survives the flip for the session.
    ///
    /// Round 73.2: the flip no longer focuses the query input (the `s` key
    /// does that) — it always RELEASES it instead. Releasing is required,
    /// not just optional: `search_input_focused` drives this pane's
    /// keyboard phase (Up/Down move the results only while the input is
    /// NOT focused), so a flip that left the flag true without insert mode
    /// would freeze the results list.
    fn toggle_mode(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.mode = match self.mode {
            PlaylistsTabMode::Playlists => PlaylistsTabMode::Search,
            PlaylistsTabMode::Search => PlaylistsTabMode::Playlists,
        };
        self.search_results.state.unmark_all();
        self.release_search_input(ctx);
        if self.mode == PlaylistsTabMode::Search {
            self.ensure_search_songs(ctx);
        }
        ctx.render()?;
        Ok(())
    }
    /// Round 73.2 (revision B: `S`): the target of the search key — show the
    /// search page with the query input focused (the caret in the field).
    /// The mode is SET, not toggled, so the key always lands on Search;
    /// nothing is cleared, so the session query and its results are still
    /// there when the page returns.
    fn jump_to_search(&mut self, ctx: &mut Ctx) -> Result<()> {
        let entered = self.mode != PlaylistsTabMode::Search;
        self.mode = PlaylistsTabMode::Search;
        self.focus_search_input(ctx);
        if entered {
            self.ensure_search_songs(ctx);
        }
        ctx.render()?;
        Ok(())
    }
    /// Round 62.1 (S1/S2 parity): leave the search page back to the
    /// Playlists browser — clears the query + results and returns to
    /// keyboard navigation.
    fn exit_search(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.mode = PlaylistsTabMode::Playlists;
        self.release_search_input(ctx);
        self.search_results.state.unmark_all();
        self.search_results.items = Vec::new();
        ctx.input.clear_buffer(self.search_buffer);
        ctx.render()?;
        Ok(())
    }
    /// Focus the search input row: the buffer becomes the active
    /// insert-mode buffer so printable keys type into the query (round
    /// 60b D3 — the buffer was never activated before, so typing reached
    /// no handler and the query stayed empty).
    fn focus_search_input(&mut self, ctx: &Ctx) {
        self.search_input_focused = true;
        self.search_left_presses = 0;
        ctx.input.insert_mode(self.search_buffer);
    }
    /// Leave the input (results / mode toggle / tab switch): drop insert
    /// mode when this pane's buffer is the active one — the input manager
    /// is global, so an unattended insert buffer would swallow keys meant
    /// for other panes, tabs and modals.
    fn release_search_input(&mut self, ctx: &Ctx) {
        self.search_input_focused = false;
        if ctx.input.is_active(self.search_buffer) {
            ctx.input.normal_mode();
        }
    }
    /// Search-mode keys (round 60 B1, interaction parity with the MPD
    /// search): `d`/`→` move from the input into the results, `a`/`←`
    /// return, Enter opens the options menu, Esc clears the marks.
    fn handle_search_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_directories() {
            match action {
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    if self.search_input_focused && !self.search_results.items.is_empty() {
                        self.release_search_input(ctx);
                        ctx.render()?;
                    } else if !self.search_input_focused {
                        self.search_activate(ctx)?;
                    }
                    return Ok(());
                }
                DirectoriesActions::FolderCollapse => {
                    if self.search_input_focused {
                        // Round 62.1 (S2): Left #2 from the search bar
                        // behaves exactly like Esc #2 — leave the search
                        // page back to the Playlists browser.
                        self.exit_search(ctx)?;
                    } else {
                        self.focus_search_input(ctx);
                        ctx.render()?;
                    }
                    return Ok(());
                }
                DirectoriesActions::FolderUp | DirectoriesActions::FolderDown => {
                    if self.search_input_focused {
                        return Ok(());
                    }
                    let dir = if matches!(action, DirectoriesActions::FolderUp) {
                        -1
                    } else {
                        1
                    };
                    return self.search_move(dir, ctx);
                }
            }
        }
        if let Some(action) = event.claim_common() {
            match action {
                CommonAction::Up | CommonAction::Down if !self.search_input_focused => {
                    let dir = if matches!(action, CommonAction::Up) { -1 } else { 1 };
                    return self.search_move(dir, ctx);
                }
                CommonAction::Right if !self.search_results.items.is_empty() => {
                    self.release_search_input(ctx);
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Left if !self.search_input_focused => {
                    self.focus_search_input(ctx);
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Confirm => {
                    return self.search_context_menu(ctx);
                }
                CommonAction::ContextMenu => {
                    return self.search_context_menu(ctx);
                }
                CommonAction::SelectAll => {
                    if !self.search_results.items.is_empty() {
                        self.search_results.state.mark_range(0, self.search_results.items.len() - 1);
                        ctx.render()?;
                    }
                    return Ok(());
                }
                CommonAction::Close => {
                    // Esc deselects the marks first (round 24-27 parity).
                    if !self.search_results.state.marked.is_empty() {
                        self.search_results.state.unmark_all();
                        ctx.render()?;
                        return Ok(());
                    }
                    if self.search_input_focused {
                        // Round 62.1 (S2): Esc #2 from the search bar leaves
                        // the search page back to the Playlists browser and
                        // is consumed so the app-level ShowSettings half
                        // does not fire.
                        self.exit_search(ctx)?;
                    } else {
                        // Round 62.1 (S1): Esc #1 from results goes back to
                        // the search bar — consumed, no Settings.
                        self.focus_search_input(ctx);
                        ctx.render()?;
                    }
                    event.consume();
                    return Ok(());
                }
                _ => event.abandon(),
            }
        }
        self.handle_common_action(event, ctx)?;
        self.handle_global_action(event, ctx)?;
        Ok(())
    }
    /// Move the search-results cursor (search mode).
    fn search_move(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.search_results.items.len();
        if len == 0 {
            return Ok(());
        }
        let state = &mut self.search_results.state;
        if dir < 0 {
            state.prev(ctx.config.scrolloff, false);
        } else {
            state.next(ctx.config.scrolloff, false);
        }
        ctx.render()?;
        Ok(())
    }
    /// The search results' options menu (Enter / right-click): same
    /// actions as the playlists-song menu, scoped to the marked results
    /// (or the highlighted one).
    fn search_context_menu(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(_) = self.search_results.selected() else { return Ok(()) };
        let songs: Vec<Song> = {
            let marked = self.search_results.state.marked.clone();
            let items = &self.search_results.items;
            if marked.is_empty() {
                items
                    .get(self.search_results.state.get_selected().unwrap_or(0))
                    .and_then(|item| match item {
                        DirOrSong::Song(song) => Some(vec![song.clone()]),
                        _ => None,
                    })
                    .unwrap_or_default()
            } else {
                items
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| marked.contains(idx))
                    .filter_map(|(_, item)| match item {
                        DirOrSong::Song(song) => Some(song.clone()),
                        _ => None,
                    })
                    .collect()
            }
        };
        let current_items: Vec<Enqueue> = songs
            .iter()
            .map(|s| Enqueue::File { path: s.file.clone() })
            .collect();
        let list_songs = move |_client: &mut Client<'_>| -> Result<Vec<Song>> {
            Ok(songs.clone())
        };
        let modal = MenuModal::new(ctx)
            .list_section(
                ctx,
                |mut section| {
                    if !current_items.is_empty() {
                        let cloned_items = current_items.clone();
                        section
                            .add_item(
                                "Add to queue",
                                move |ctx| {
                                    ctx.command(move |client| {
                                        client.enqueue_multiple(cloned_items, None, None, false)?;
                                        Ok(())
                                    });
                                    Ok(())
                                },
                            );
                        let cloned_items = current_items.clone();
                        section
                            .add_item(
                                "Replace queue",
                                move |ctx| {
                                    ctx.command(move |client| {
                                        client.enqueue_multiple(cloned_items, None, None, true)?;
                                        Ok(())
                                    });
                                    Ok(())
                                },
                            );
                    }
                    let songs_in_item = list_songs.clone();
                    section
                        .add_item(
                            "Create playlist",
                            move |ctx| {
                                modal!(
                                    ctx, InputModal::new(ctx).title("Create new playlist")
                                    .confirm_label("Save").input_label("Playlist name:")
                                    .on_confirm(move | ctx, value | { let value = value
                                    .to_owned(); let songs_in_item = songs_in_item.clone();
                                    ctx.command(move | client | { let songs = songs_in_item
                                    (client) ?; client.create_playlist(& value, songs
                                    .into_iter().map(| s | s.file).collect(),) ?; Ok(())
                                    }); Ok(()) })
                                );
                                Ok(())
                            },
                        );
                    let songs_in_item = list_songs.clone();
                    section
                        .add_item(
                            "Add to playlist",
                            move |ctx| {
                                let radio_playlist = ctx.config.radio.playlist.clone();
                                let (items, playlists) = ctx
                                    .query_sync(move |client| {
                                        let songs = songs_in_item(client)?;
                                        let playlists = client
                                            .picker_playlists(&radio_playlist)?
                                            .into_iter()
                                            .map(|p| p.name)
                                            .collect_vec();
                                        Ok((songs, playlists))
                                    })?;
                                modal!(
                                    ctx, SelectModal::builder().ctx(ctx).options(playlists)
                                    .confirm_label("Add").title("Select a playlist")
                                    .on_confirm(move | ctx, selected, _idx | { ctx.command(move
                                    | client | { client.add_to_playlist_multiple(& selected,
                                    items.into_iter().map(| s | s.file).collect_vec(),) ?;
                                    Ok(()) }); Ok(()) }).build()
                                );
                                Ok(())
                            },
                        );
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
impl Pane for PlaylistsPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        if area.height < 2 {
            return Ok(());
        }
        let [toggle_area, content] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(area);
        self.render_toggle(frame, toggle_area, ctx);
        match self.mode {
            PlaylistsTabMode::Playlists => self.render_library(frame, content, ctx),
            PlaylistsTabMode::Search => self.render_search(frame, content, ctx),
        }
    }
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        let id = if self.initialized { REINIT } else { INIT };
        let compare = StringCompare::from(ctx.config.browser_song_sort.as_ref());
        let radio_playlist = ctx.config.radio.playlist.clone();
        let show_library_files = ctx.config.ui.library_playlist_files;
        ctx.query()
            .id(id)
            .target(PaneType::Playlists {
                tree: TreeBrowserArgs::default(),
            })
            .replace_id(id)
            .query(move |client| {
                let result = playlist_root_items(
                    client,
                    show_library_files,
                    &radio_playlist,
                    &compare,
                )?;
                Ok(MpdQueryResult::DirOrSong {
                    data: result,
                    path: None,
                })
            });
        self.initialized = true;
        // Round 60b (D3): returning to a tab left in search mode with the
        // input focused re-attaches the search buffer (on_hide released
        // it while the tab was away). Round 73.2: the flag is only set by
        // the `S` jump, so this re-attaches the caret the user asked for
        // and nothing else.
        if self.mode == PlaylistsTabMode::Search && self.search_input_focused {
            ctx.input.insert_mode(self.search_buffer);
        }
        Ok(())
    }
    fn on_hide(&mut self, ctx: &Ctx) -> Result<()> {
        // Round 60b (D3): leaving the tab must drop the search input's
        // insert mode — the input manager is global and an unattended
        // insert buffer would swallow keys meant for the other tab.
        if ctx.input.is_active(self.search_buffer) {
            ctx.input.normal_mode();
        }
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
                let show_library_files = ctx.config.ui.library_playlist_files;
                ctx.query()
                    .id(id)
                    .replace_id(id)
                    .target(PaneType::Playlists {
                        tree: TreeBrowserArgs::default(),
                    })
                    .query(move |client| {
                        let compare = StringCompare::from(sort_opts.as_ref());
                        let result =
                            playlist_root_items(client, show_library_files, &radio_playlist,
                            &compare)?;
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
        if matches!(
            event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick
        ) {
            // The mode toggle row (Playlists | Search).
            for (idx, area) in self.toggle_areas.iter().enumerate() {
                if area.contains(position) {
                    let mode = if idx == 0 {
                        PlaylistsTabMode::Playlists
                    } else {
                        PlaylistsTabMode::Search
                    };
                    if self.mode != mode {
                        self.mode = mode;
                        if mode == PlaylistsTabMode::Search {
                            self.ensure_search_songs(ctx);
                        }
                        ctx.render()?;
                    }
                    return Ok(());
                }
            }
            // Round 60 (B3): the `↰ Back` button.
            if self.back_area.width > 0 && self.back_area.contains(position) {
                return self.back_out(ctx);
            }
        }
        if self.mode == PlaylistsTabMode::Search {
            return self.handle_search_mouse(event, ctx);
        }
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
                        let selected = dir.selected().cloned();
                        if let Some(DirOrSong::Dir { name, full_path, .. }) = selected {
                            let library_path = (!full_path.is_empty()).then_some(full_path);
                            return self.open_playlist_menu(
                                &name,
                                library_path.as_deref(),
                                ctx,
                                Some(position),
                            );
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
                MouseEventKind::LeftClick | MouseEventKind::DoubleClick if event
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) => {
                    let row = usize::from(event.y.saturating_sub(self.songs_area.y));
                    self.select_song_at(
                        row,
                        |dir, idx| {
                            // Ctrl+click toggles the row: mark it if it
                            // was not marked, unmark it if it was.
                            let was_marked = dir.state.marked.contains(&idx);
                            if dir.state.marked.is_empty() {
                                if let Some(sel) = dir.state.get_selected() {
                                    dir.state.mark(sel);
                                }
                            }
                            if was_marked {
                                dir.state.unmark(idx);
                            } else {
                                dir.state.mark(idx);
                            }
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
            ) && (self.info_scrollbar_area.contains(event.into())
                || (matches!(event.kind, MouseEventKind::Drag { .. })
                    && self.info_scrollbar_drag.is_active()))
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
        if self.mode == PlaylistsTabMode::Search {
            // Typing always edits the search query (round 60 B1); each
            // change re-runs the local match against the session snapshot.
            match kind {
                InputResultEvent::Push => {
                    self.search_left_presses = 0;
                    self.search_playlists_local(ctx)?;
                }
                InputResultEvent::Pop => {
                    self.search_left_presses = 0;
                    self.search_playlists_local(ctx)?;
                }
                InputResultEvent::Confirm => {
                    // Enter inside the input row moves the focus to the
                    // results: the shared input manager drops insert mode
                    // right after this handler, so an input that stays
                    // "focused" would render its cursor but never receive
                    // another keystroke (round 60b D3).
                    self.search_left_presses = 0;
                    self.release_search_input(ctx);
                }
                InputResultEvent::Cancel => {
                    // Round 62.1 (S2): Esc inside the search bar is stage
                    // two of the staged exit — leave the search page back
                    // to the Playlists browser (clear query + results).
                    self.exit_search(ctx)?;
                }
                InputResultEvent::AtStart => {
                    // Round 63.1 (2): Left with the cursor already at the
                    // input's start (empty query, or the cursor reached
                    // position 0) is the exit press — the buffer no longer
                    // swallows it as a text-cursor move.
                    self.exit_search(ctx)?;
                }
                InputResultEvent::CursorLeft => {
                    // Round 63.1 (2): the first Left at the bar navigates
                    // the text cursor; the SECOND consecutive Left exits
                    // exactly like Esc #2 (Left #2 parity with MPD).
                    if self.search_left_presses > 0 {
                        self.search_left_presses = 0;
                        self.exit_search(ctx)?;
                    } else {
                        self.search_left_presses = 1;
                    }
                }
                InputResultEvent::NoChange => {
                    self.search_left_presses = 0;
                }
            }
            ctx.render()?;
            return Ok(());
        }
        SongListCore::handle_insert_mode(self, kind, ctx)?;
        Ok(())
    }
    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_global() {
            // Round 60 (B1): Shift+Tab toggles the mode while this tab is
            // focused (the same claim as the MPD tab's Library/Search
            // toggle — panes are only reached from their own tab).
            if matches!(action, GlobalAction::ToggleMpdMode) {
                return self.toggle_mode(ctx);
            }
            // Round 73.2 (revision B: `S`): jump to the search page with the
            // input focused. Claimed here (before the mode dispatch), so it
            // works from both the Playlists browser and the search page.
            if matches!(action, GlobalAction::LibrarySearch) {
                return self.jump_to_search(ctx);
            }
            event.abandon();
        }
        if self.mode == PlaylistsTabMode::Search {
            return self.handle_search_action(event, ctx);
        }
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
                if let Some((name, full_path)) = self.pending_open.take() {
                    self.select_and_open(&name, &full_path, ctx)?;
                    ctx.render()?;
                }
                ctx.render()?;
            }
            (SEARCH_DATA, MpdQueryResult::Any(any)) => {
                if let Ok(plays) = any.downcast::<Vec<SearchPlaylist>>() {
                    self.search_playlists = *plays;
                    self.search_playlists_local(ctx)?;
                }
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
        if self.stack.path().is_empty() {
            // Same bookkeeping as open_selected_playlist, for the mouse /
            // Enter-open paths through the shared BrowserPane.
            self.open_library_playlist =
                self.stack.root().selected().and_then(|item| match item {
                    DirOrSong::Dir { full_path, .. } if !full_path.is_empty() => {
                        Some(full_path.clone())
                    }
                    _ => None,
                });
        }
        BrowserPane::open(self, autoplay, ctx)
    }
    fn leave(&mut self, ctx: &Ctx) -> Result<()> {
        self.open_library_playlist = None;
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
                    DirOrSong::Dir { full_path, .. } if !full_path.is_empty() => {
                        read_library_playlist_songs(&full_path)
                    }
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
                let DirOrSong::Dir { name: playlist, full_path, .. } = selected else {
                    log::error!(
                        selected:? = selected; "Expected playlist to be selected"
                    );
                    return Ok(());
                };
                let path = self.stack.next_path();
                let playlist = playlist.to_owned();
                let full_path = full_path.to_owned();
                let library = !full_path.is_empty();
                ctx.query()
                    .id(FETCH_DATA)
                    .replace_id("playlists_data")
                    .target(PaneType::Playlists {
                        tree: TreeBrowserArgs::default(),
                    })
                    .query(move |client| {
                        let data: Vec<DirOrSong> = if library {
                            // Library playlist file: parse locally (read-only).
                            read_library_playlist_songs(&full_path)
                                .into_iter()
                                .map(DirOrSong::Song)
                                .collect()
                        } else {
                            client
                                .list_playlist_info(&playlist, None)?
                                .into_iter()
                                .map(DirOrSong::Song)
                                .collect_vec()
                        };
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
            DirOrSong::Dir { name, full_path, .. } => {
                let playlist = name.clone();
                let library_path = (!full_path.is_empty()).then(|| full_path.clone());
                ctx.query()
                    .target(PaneType::Playlists {
                        tree: TreeBrowserArgs::default(),
                    })
                    .replace_id(PLAYLIST_INFO)
                    .id(PLAYLIST_INFO)
                    .query(move |client| {
                        let data = if let Some(path) = library_path {
                            read_library_playlist_songs(&path)
                        } else {
                            client.list_playlist_info(&playlist, None)?
                        };
                        Ok(MpdQueryResult::SongsList {
                            data,
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
                // Songs inside a library playlist file cannot be edited:
                // the file is user-owned on disk (read-only) and MPD has
                // no command to edit library playlist files.
                if self.open_library_playlist.is_some() {
                    return Vec::new();
                }
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
                        // Library playlist files are never deletable from
                        // the app (`full_path` non-empty).
                        DirOrSong::Dir { name, full_path, .. } if full_path.is_empty() => {
                            Some(MpdDelete::Playlist {
                                name: name.clone(),
                            })
                        }
                        _ => None,
                    })
                    .collect_vec()
            }
            _ => Vec::new(),
        }
    }
    fn can_rename(&self, item: &DirOrSong) -> bool {
        // Stored playlists only: library playlist files are read-only
        // (renaming would rewrite the user's own file).
        matches!(item, DirOrSong::Dir { full_path, .. } if full_path.is_empty())
    }
    fn rename(item: &DirOrSong, ctx: &Ctx) -> Result<()> {
        match item {
            DirOrSong::Dir { name: d, full_path, .. } if full_path.is_empty() => {
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
            // Library playlist files are read-only: no rename.
            DirOrSong::Dir { .. } => {}
            DirOrSong::Song(_) => {}
        }
        Ok(())
    }
    fn move_selected(&mut self, direction: MoveDirection, ctx: &Ctx) -> Result<()> {
        // Songs inside a library playlist file cannot be reordered (MPD
        // playlistmove only touches stored playlists; the file is
        // read-only anyway).
        if self.open_library_playlist.is_some() {
            return Ok(());
        }
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
