use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    time::Duration,
};

use album_art::AlbumArtPane;
use albums::AlbumsPane;
use anyhow::{Context, Result};
use cava::CavaPane;
use directories::DirectoriesPane;
use either::Either;
use header::HeaderPane;
use jellyfin::JellyfinPane;
use lyrics::LyricsPane;
use playlists::PlaylistsPane;
use progress_bar::ProgressBarPane;
use property::PropertyPane;
use queue::QueuePane;
use radio::RadioPane;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position},
    prelude::Rect,
    style::Color,
    text::{Line, Span},
    widgets::{Block, Borders},
};
use search::SearchPane;
use strum::{Display, IntoDiscriminant};
use tabs::TabsPane;
use tag_browser::TagBrowserPane;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use volume::VolumePane;
use controls::ControlsPane;

#[cfg(debug_assertions)]
use self::{frame_count::FrameCountPane, logs::LogsPane};
use super::{
    UiEvent,
    widgets::{scan_status::ScanStatus, volume::Volume},
};
use crate::{
    MpdQueryResult,
    config::{
        tabs::{Pane as ConfigPane, PaneType, SizedPaneOrSplit, SizedSubPane},
        theme::{
            PercentOrLength,
            SymbolsConfig,
            TagResolutionStrategy,
            properties::{
                Property,
                PropertyKind,
                PropertyKindOrText,
                SongProperty,
                StatusProperty,
                Transform,
                WidgetProperty,
            },
        },
    },
    ctx::Ctx,
    mpd::{
        commands::{Song, State, status::OnOffOneshot, volume::Bound},
        mpd_client::Tag,
    },
    shared::{
        ext::{duration::DurationExt, num::NumExt, span::SpanExt},
        keys::ActionEvent,
        mouse_event::MouseEvent,
    },
    ui::{
        input::InputResultEvent,
        panes::{empty::EmptyPane, queue_header::QueueHeaderPane},
        widgets::header::PropertyTemplates,
    },
};

pub mod album_art;
pub mod albums;
pub mod cava;
pub mod controls;
pub mod directories;
pub mod empty;
#[cfg(debug_assertions)]
pub mod frame_count;
pub mod header;
#[cfg(debug_assertions)]
pub mod logs;
pub mod jellyfin;
pub mod lyrics;
pub mod playlists;
pub mod progress_bar;
pub mod property;
pub mod queue;
pub mod queue_header;
pub mod radio;
pub mod search;
pub mod tabs;
pub mod tag_browser;
pub mod volume;

/// The absolute index of the item under the mouse in a vertical list of
/// uniform `item_height`-row items rendered into `inner`, with the first
/// visible item at scroll `offset`. `None` when the pointer is outside the
/// list or past its last item. Used by the mouseover row highlight (queue,
/// MPD, playlists, radio, jellyfin lists).
pub(crate) fn hovered_item(
    mouse: Option<Position>,
    inner: Rect,
    offset: usize,
    len: usize,
    item_height: u16,
) -> Option<usize> {
    let pos = mouse?;
    if !inner.contains(pos) {
        return None;
    }
    let row = u16::from(pos.y.saturating_sub(inner.y));
    let idx = offset + usize::from(row / item_height.max(1));
    (idx < len).then_some(idx)
}

#[derive(Debug, Display, strum::EnumDiscriminants)]
pub enum Panes<'pane_ref> {
    Queue(&'pane_ref mut QueuePane),
    QueueHeader(&'pane_ref mut QueueHeaderPane),
    #[cfg(debug_assertions)]
    Logs(&'pane_ref mut LogsPane),
    Directories(&'pane_ref mut DirectoriesPane),
    Artists(&'pane_ref mut TagBrowserPane),
    AlbumArtists(&'pane_ref mut TagBrowserPane),
    Albums(&'pane_ref mut AlbumsPane),
    Playlists(&'pane_ref mut PlaylistsPane),
    Search(&'pane_ref mut SearchPane),
    Radio(&'pane_ref mut RadioPane),
    Jellyfin(&'pane_ref mut JellyfinPane),
    AlbumArt(&'pane_ref mut AlbumArtPane),
    Lyrics(&'pane_ref mut LyricsPane),
    ProgressBar(&'pane_ref mut ProgressBarPane),
    Header(&'pane_ref mut HeaderPane),
    Tabs(&'pane_ref mut TabsPane),
    #[cfg(debug_assertions)]
    FrameCount(&'pane_ref mut FrameCountPane),
    TabContent,
    Property(PropertyPane<'pane_ref>),
    Others(&'pane_ref mut Box<dyn BoxedPane>),
    Cava(&'pane_ref mut CavaPane),
    Empty(&'pane_ref mut EmptyPane),
}

pub trait BoxedPane: Pane + std::fmt::Debug {}

impl<P: Pane + std::fmt::Debug> BoxedPane for P {}

/// The minimum content rows a pane's box needs to be useful (the info box
/// renders nothing below this: title + context row + description header +
/// one content row). Responsive sub-panes (`window_sizes`) collapse
/// entirely — box and borders — below this plus their border rows.
pub(crate) const MIN_PANE_CONTENT_HEIGHT: u16 = 4;

#[derive(Debug)]
pub struct PaneContainer {
    pub queue: QueuePane,
    pub queue_header: QueueHeaderPane,
    #[cfg(debug_assertions)]
    pub logs: LogsPane,
    pub directories: DirectoriesPane,
    pub albums: AlbumsPane,
    pub artists: TagBrowserPane,
    pub album_artists: TagBrowserPane,
    pub playlists: PlaylistsPane,
    pub search: SearchPane,
    pub radio: RadioPane,
    pub jellyfin: JellyfinPane,
    pub album_art: AlbumArtPane,
    pub lyrics: LyricsPane,
    pub progress_bar: ProgressBarPane,
    pub header: HeaderPane,
    pub tabs: TabsPane,
    pub cava: CavaPane,
    #[cfg(debug_assertions)]
    pub frame_count: FrameCountPane,
    pub empty: EmptyPane,
    pub others: HashMap<PaneType, Box<dyn BoxedPane>>,
}

impl PaneContainer {
    pub fn new(ctx: &Ctx) -> Result<Self> {
        Ok(Self {
            queue: QueuePane::new(ctx),
            queue_header: QueueHeaderPane::new(ctx),
            #[cfg(debug_assertions)]
            logs: LogsPane::new(),
            directories: DirectoriesPane::new(ctx),
            albums: AlbumsPane::new(ctx),
            artists: TagBrowserPane::new(Tag::Artist, PaneType::Artists, None, ctx),
            album_artists: TagBrowserPane::new(Tag::AlbumArtist, PaneType::AlbumArtists, None, ctx),
            playlists: PlaylistsPane::new(ctx),
            search: SearchPane::new(ctx),
            radio: RadioPane::new(ctx),
            jellyfin: JellyfinPane::new(ctx),
            album_art: AlbumArtPane::new(ctx),
            lyrics: LyricsPane::new(ctx),
            progress_bar: ProgressBarPane::new(),
            header: HeaderPane::new(),
            tabs: TabsPane::new(ctx)?,
            cava: CavaPane::new(ctx),
            #[cfg(debug_assertions)]
            frame_count: FrameCountPane::new(),
            empty: EmptyPane,
            others: Self::init_other_panes(ctx).collect(),
        })
    }

    pub fn init_other_panes(
        ctx: &Ctx,
    ) -> impl Iterator<Item = (PaneType, Box<dyn BoxedPane>)> + use<'_> {
        ctx.config
            .tabs
            .tabs
            .iter()
            .flat_map(|(_name, tab)| tab.panes.panes_iter())
            .chain(ctx.config.theme.layout.panes_iter())
            .filter_map(|pane| match &pane.pane {
                PaneType::Browser { root_tag, separator } => Some((
                    pane.pane.clone(),
                    Box::new(TagBrowserPane::new(
                        Tag::Custom(root_tag.clone()),
                        pane.pane.clone(),
                        separator.clone(),
                        ctx,
                    )) as Box<dyn BoxedPane>,
                )),
                PaneType::Volume { kind } => Some((
                    pane.pane.clone(),
                    Box::new(VolumePane::new(kind.clone())) as Box<dyn BoxedPane>,
                )),
                PaneType::Controls => Some((
                    pane.pane.clone(),
                    Box::new(ControlsPane::new()) as Box<dyn BoxedPane>,
                )),
                _ => None,
            })
    }

    pub fn get_mut<'pane_ref, 'pane_type_ref: 'pane_ref>(
        &'pane_ref mut self,
        pane: &'pane_type_ref PaneType,
        ctx: &Ctx,
    ) -> Result<Panes<'pane_ref>> {
        match pane {
            PaneType::Queue => Ok(Panes::Queue(&mut self.queue)),
            PaneType::QueueHeader() => Ok(Panes::QueueHeader(&mut self.queue_header)),
            #[cfg(debug_assertions)]
            PaneType::Logs => Ok(Panes::Logs(&mut self.logs)),
            PaneType::Directories { .. } => Ok(Panes::Directories(&mut self.directories)),
            PaneType::Artists => Ok(Panes::Artists(&mut self.artists)),
            PaneType::AlbumArtists => Ok(Panes::AlbumArtists(&mut self.album_artists)),
            PaneType::Albums => Ok(Panes::Albums(&mut self.albums)),
            PaneType::Playlists { .. } => Ok(Panes::Playlists(&mut self.playlists)),
            PaneType::Search => Ok(Panes::Search(&mut self.search)),
            PaneType::Radio { .. } => Ok(Panes::Radio(&mut self.radio)),
            PaneType::Jellyfin { .. } => Ok(Panes::Jellyfin(&mut self.jellyfin)),
            PaneType::AlbumArt => Ok(Panes::AlbumArt(&mut self.album_art)),
            PaneType::Lyrics => Ok(Panes::Lyrics(&mut self.lyrics)),
            PaneType::ProgressBar => Ok(Panes::ProgressBar(&mut self.progress_bar)),
            PaneType::Header => Ok(Panes::Header(&mut self.header)),
            PaneType::Tabs => Ok(Panes::Tabs(&mut self.tabs)),
            PaneType::TabContent => Ok(Panes::TabContent),
            #[cfg(debug_assertions)]
            PaneType::FrameCount => Ok(Panes::FrameCount(&mut self.frame_count)),
            PaneType::Property { content, align, scroll_speed } => Ok(Panes::Property(
                PropertyPane::<'pane_type_ref>::new(content, *align, (*scroll_speed).into(), ctx),
            )),
            p @ PaneType::Volume { .. } => Ok(Panes::Others(
                self.others
                    .get_mut(pane)
                    .with_context(|| format!("expected pane to be defined {p:?}"))?,
            )),
            p @ PaneType::Controls => Ok(Panes::Others(
                self.others
                    .get_mut(pane)
                    .with_context(|| format!("expected pane to be defined {p:?}"))?,
            )),
            p @ PaneType::Browser { .. } => Ok(Panes::Others(
                self.others
                    .get_mut(pane)
                    .with_context(|| format!("expected pane to be defined {p:?}"))?,
            )),
            PaneType::Cava => Ok(Panes::Cava(&mut self.cava)),
            PaneType::Empty => Ok(Panes::Empty(&mut self.empty)),
        }
    }
}

macro_rules! pane_call {
    ($screen:ident, $fn:ident($($param:expr),+)) => {
        match &mut $screen {
            Panes::Queue(s) => s.$fn($($param),+),
            Panes::QueueHeader(s) => s.$fn($($param),+),
            #[cfg(debug_assertions)]
            Panes::Logs(s) => s.$fn($($param),+),
            Panes::Directories(s) => s.$fn($($param),+),
            Panes::Artists(s) => s.$fn($($param),+),
            Panes::AlbumArtists(s) => s.$fn($($param),+),
            Panes::Albums(s) => s.$fn($($param),+),
            Panes::Playlists(s) => s.$fn($($param),+),
            Panes::Search(s) => s.$fn($($param),+),
            Panes::Radio(s) => s.$fn($($param),+),
            Panes::Jellyfin(s) => s.$fn($($param),+),
            Panes::AlbumArt(s) => s.$fn($($param),+),
            Panes::Lyrics(s) => s.$fn($($param),+),
            Panes::ProgressBar(s) => s.$fn($($param),+),
            Panes::Header(s) => s.$fn($($param),+),
            Panes::Tabs(s) => s.$fn($($param),+),
            Panes::TabContent => Ok(()),
            #[cfg(debug_assertions)]
            Panes::FrameCount(s) => s.$fn($($param),+),
            Panes::Property(s) => s.$fn($($param),+),
            Panes::Others(s) => s.$fn($($param),+),
            Panes::Cava(s) => s.$fn($($param),+),
            Panes::Empty(s) => s.$fn($($param),+),
        }
    }
}
pub(crate) use pane_call;

#[allow(unused_variables)]
pub(crate) trait Pane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()>;

    /// For any cleanup operations, ran when the screen hides
    fn on_hide(&mut self, ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    /// For work that needs to be done BEFORE the first render
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    /// Used to keep the current state but refresh data
    fn on_event(&mut self, event: &mut UiEvent, is_visible: bool, ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }

    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()>;

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn on_query_finished(
        &mut self,
        id: &'static str,
        data: MpdQueryResult,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        Ok(())
    }

    fn calculate_areas(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn resize(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        Ok(())
    }
}

pub(crate) mod browser {

    use itertools::Itertools;
    use ratatui::{
        style::Style,
        text::{Line, Span},
    };

    use crate::{ctx::Ctx, mpd::commands::Song, shared::mpd_query::PreviewGroup};

    impl Song {
        pub(crate) fn to_preview(
            &self,
            key_style: Style,
            group_style: Style,
            ctx: &Ctx,
        ) -> Vec<PreviewGroup> {
            let separator = Span::from(": ");
            let start_of_line_spacer = Span::from(" ");

            let mut info_group = PreviewGroup::new(Some(" --- [Info]"), Some(group_style));

            let file = Line::from(vec![
                start_of_line_spacer.clone(),
                Span::styled("File", key_style),
                separator.clone(),
                Span::from(self.file.clone()),
            ]);
            info_group.push(file.into());

            if let Some(file_name) = self.file_name() {
                info_group.push(
                    Line::from(vec![
                        start_of_line_spacer.clone(),
                        Span::styled("Filename", key_style),
                        separator.clone(),
                        Span::from(file_name.into_owned()),
                    ])
                    .into(),
                );
            }

            if let Some(title) = self.metadata.get("title") {
                title.for_each(|item| {
                    info_group.push(
                        Line::from(vec![
                            start_of_line_spacer.clone(),
                            Span::styled("Title", key_style),
                            separator.clone(),
                            Span::from(item.to_owned()),
                        ])
                        .into(),
                    );
                });
            }
            if let Some(artist) = self.metadata.get("artist") {
                artist.for_each(|item| {
                    info_group.push(
                        Line::from(vec![
                            start_of_line_spacer.clone(),
                            Span::styled("Artist", key_style),
                            separator.clone(),
                            Span::from(item.to_owned()),
                        ])
                        .into(),
                    );
                });
            }

            if let Some(album) = self.metadata.get("album") {
                album.for_each(|item| {
                    info_group.push(
                        Line::from(vec![
                            start_of_line_spacer.clone(),
                            Span::styled("Album", key_style),
                            separator.clone(),
                            Span::from(item.to_owned()),
                        ])
                        .into(),
                    );
                });
            }

            if let Some(duration) = &self.duration {
                info_group.push(
                    Line::from(vec![
                        start_of_line_spacer.clone(),
                        Span::styled("Duration", key_style),
                        separator.clone(),
                        Span::from(ctx.config.duration_format.format(duration.as_secs())),
                    ])
                    .into(),
                );
            }

            info_group.push(
                Line::from(vec![
                    start_of_line_spacer.clone(),
                    Span::styled("Last Modified", key_style),
                    separator.clone(),
                    Span::from(self.last_modified.to_string()),
                ])
                .into(),
            );

            if let Some(added) = &self.added {
                info_group.push(
                    Line::from(vec![
                        start_of_line_spacer.clone(),
                        Span::styled("Added", key_style),
                        separator.clone(),
                        Span::from(added.to_string()),
                    ])
                    .into(),
                );
            }

            let mut tags_group = PreviewGroup::new(Some(" --- [Tags]"), Some(group_style));
            for (k, v) in self
                .metadata
                .iter()
                .filter(|(key, _)| {
                    !["title", "album", "artist", "duration"].contains(&(*key).as_str())
                })
                .sorted_by_key(|(key, _)| *key)
            {
                v.for_each(|item| {
                    tags_group.push(
                        Line::from(vec![
                            start_of_line_spacer.clone(),
                            Span::styled(k.clone(), key_style),
                            separator.clone(),
                            Span::from(item.to_owned()),
                        ])
                        .into(),
                    );
                });
            }

            // A resolved YouTube-style stream shows its video title and
            // description (the stream itself has no metadata, so the group
            // goes first — the file line is a long stream URL anyway).
            let mut result = Vec::new();
            if let Some(yt) = ctx.yt_info.borrow().get(&self.file) {
                let mut yt_group = PreviewGroup::new(Some(" --- [YouTube]"), Some(group_style));
                if !yt.title.is_empty() {
                    yt_group.push(
                        Line::from(vec![
                            start_of_line_spacer.clone(),
                            Span::styled("Title", key_style),
                            separator.clone(),
                            Span::from(yt.title.clone()),
                        ])
                        .into(),
                    );
                }
                if let Some(description) = &yt.description {
                    for (idx, line) in description.lines().take(15).enumerate() {
                        let label =
                            if idx == 0 { Span::styled("Description", key_style) } else { Span::raw(" ") };
                        // Links inside the description are drawn blue (the
                        // info box's link style), matching the queue box.
                        let mut row_spans = vec![
                            start_of_line_spacer.clone(),
                            label,
                            separator.clone(),
                        ];
                        let (spans, _) = crate::ui::panes::lyrics::linkify_line(
                            line,
                            ratatui::style::Style::default(),
                        );
                        row_spans.extend(spans);
                        yt_group.push(Line::from(row_spans).into());
                    }
                }
                result.push(yt_group);
            }
            result.extend([info_group, tags_group]);

            let stickers = ctx.song_stickers_if_supported(&self.file);
            if let Some(stickers) = stickers
                && !stickers.is_empty()
            {
                let mut stickers_group =
                    PreviewGroup::new(Some(" --- [Stickers]"), Some(group_style));

                for (k, v) in stickers.iter().sorted_by_key(|(key, _)| *key) {
                    stickers_group.push(
                        Line::from(vec![
                            start_of_line_spacer.clone(),
                            Span::styled(k.clone(), key_style),
                            separator.clone(),
                            Span::from(v.to_owned()),
                        ])
                        .into(),
                    );
                }

                result.push(stickers_group);
            }

            result
        }
    }
}

impl Song {
    pub fn title_str(&self, separator: &str) -> Cow<'_, str> {
        self.metadata.get("title").map_or(Cow::Borrowed("Untitled"), |v| v.join(separator))
    }

    pub fn artist_str(&self, separator: &str) -> Cow<'_, str> {
        self.metadata.get("artist").map_or(Cow::Borrowed("Unknown"), |v| v.join(separator))
    }

    pub fn file_name(&self) -> Option<Cow<'_, str>> {
        std::path::Path::new(&self.file).file_stem().map(|file_name| file_name.to_string_lossy())
    }

    pub fn file_ext(&self) -> Option<Cow<'_, str>> {
        std::path::Path::new(&self.file).extension().map(|ext| ext.to_string_lossy())
    }

    pub fn format<'song>(
        &'song self,
        property: &SongProperty,
        tag_separator: &str,
        strategy: TagResolutionStrategy,
    ) -> Option<Cow<'song, str>> {
        match property {
            SongProperty::Filename => self.file_name(),
            SongProperty::FileExtension => self.file_ext(),
            SongProperty::File => Some(Cow::Borrowed(self.file.as_str())),
            SongProperty::Title => {
                self.metadata.get("title").map(|v| strategy.resolve(v, tag_separator))
            }
            SongProperty::Artist => {
                self.metadata.get("artist").map(|v| strategy.resolve(v, tag_separator))
            }
            SongProperty::Album => {
                self.metadata.get("album").map(|v| strategy.resolve(v, tag_separator))
            }
            SongProperty::Duration => self.duration.map(|d| Cow::Owned(d.to_string())),
            SongProperty::Other(name) => {
                self.metadata.get(name).map(|v| strategy.resolve(v, tag_separator))
            }
            SongProperty::Disc => self.metadata.get("disc").map(|v| Cow::Borrowed(v.last())),
            SongProperty::Position => self.metadata.get("pos").map(|v| {
                v.last()
                    .parse::<usize>()
                    .map(|v| Cow::Owned((v + 1).to_string()))
                    .unwrap_or_default()
            }),
            SongProperty::Track => self.metadata.get("track").map(|v| {
                Cow::Owned(
                    v.last()
                        .parse::<u32>()
                        .map_or_else(|_| v.last().to_owned(), |v| format!("{v:0>2}")),
                )
            }),
            SongProperty::SampleRate() => self.samplerate().map(|v| Cow::Owned(v.to_string())),
            SongProperty::Bits() => self.bits().map(|v| Cow::Owned(v.to_string())),
            SongProperty::Channels() => self.channels().map(|v| Cow::Owned(v.to_string())),
            SongProperty::Added() => self.added.map(|d| Cow::Owned(d.to_string())),
            SongProperty::LastModified() => Some(Cow::Owned(self.last_modified.to_string())),
        }
    }

    pub fn matches<'a>(
        &self,
        formats: impl IntoIterator<Item = &'a Property<SongProperty>>,
        filter: &str,
        ctx: &Ctx,
    ) -> bool {
        for format in formats {
            let match_found = match &format.kind {
                PropertyKindOrText::Text(value) => {
                    Some(value.to_lowercase().contains(&filter.to_lowercase()))
                }
                PropertyKindOrText::Sticker(key) => ctx
                    .song_stickers(&self.file)
                    .and_then(|s| s.get(key))
                    .map(|value| value.to_lowercase().contains(&filter.to_lowercase()))
                    .or_else(|| {
                        format
                            .default
                            .as_ref()
                            .map(|f| self.matches(std::iter::once(f.as_ref()), filter, ctx))
                    }),
                PropertyKindOrText::Property(property) => {
                    self.format(property, "", TagResolutionStrategy::All).map_or_else(
                        || {
                            format
                                .default
                                .as_ref()
                                .map(|f| self.matches(std::iter::once(f.as_ref()), filter, ctx))
                        },
                        |p| Some(p.to_lowercase().contains(&filter.to_lowercase())),
                    )
                }
                PropertyKindOrText::Group(_) => format
                    .as_string(Some(self), "", TagResolutionStrategy::All, ctx)
                    .map(|v| v.to_lowercase().contains(&filter.to_lowercase())),
                PropertyKindOrText::Transform(Transform::Truncate { .. }) => format
                    .as_string(Some(self), "", TagResolutionStrategy::All, ctx)
                    .map(|v| v.to_lowercase().contains(&filter.to_lowercase())),
                PropertyKindOrText::Transform(Transform::Replace { .. }) => format
                    .as_string(Some(self), "", TagResolutionStrategy::All, ctx)
                    .map(|v| v.to_lowercase().contains(&filter.to_lowercase())),
            };
            if match_found.is_some_and(|v| v) {
                return true;
            }
        }
        return false;
    }

    fn default_as_line<'song, 'stickers: 'song>(
        &'song self,
        format: &Property<SongProperty>,
        tag_separator: &str,
        strategy: TagResolutionStrategy,
        ctx: &'stickers Ctx,
    ) -> Option<Line<'song>> {
        format.default.as_ref().and_then(|f| self.as_line(f.as_ref(), tag_separator, strategy, ctx))
    }

    pub fn as_line<'song, 'stickers: 'song>(
        &'song self,
        format: &Property<SongProperty>,
        tag_separator: &str,
        strategy: TagResolutionStrategy,
        ctx: &'stickers Ctx,
    ) -> Option<Line<'song>> {
        let style = format.style.unwrap_or_default();
        match &format.kind {
            PropertyKindOrText::Text(value) => Some(Line::styled(value.clone(), style)),
            PropertyKindOrText::Sticker(key) => ctx
                .song_stickers(&self.file)
                .and_then(|s| s.get(key))
                .map(|sticker| Line::styled(sticker, style))
                .or_else(|| {
                    format.default.as_ref().and_then(|format| {
                        self.as_line(format.as_ref(), tag_separator, strategy, ctx)
                    })
                }),
            PropertyKindOrText::Property(property) => {
                self.format(property, tag_separator, strategy).map_or_else(
                    || self.default_as_line(format, tag_separator, strategy, ctx),
                    |v| Some(Line::styled(v, style)),
                )
            }
            PropertyKindOrText::Group(group) => {
                let mut buf = Line::default().style(style);
                for grformat in group {
                    if let Some(res) = self.as_line(grformat, tag_separator, strategy, ctx) {
                        for span in res.spans {
                            let span_style = span.style;
                            buf.push_span(span.style(res.style).patch_style(span_style));
                        }
                    } else {
                        return format
                            .default
                            .as_ref()
                            .and_then(|format| self.as_line(format, tag_separator, strategy, ctx));
                    }
                }
                return Some(buf);
            }
            PropertyKindOrText::Transform(Transform::Replace { content, replacements }) => self
                .as_line(content, tag_separator, strategy, ctx)
                .and_then(|line| {
                    let mut content = String::new();
                    for span in &line.spans {
                        content.push_str(span.content.as_ref());
                    }

                    if let Some(replacement) = replacements.get(&content) {
                        return self.as_line(replacement, tag_separator, strategy, ctx).or_else(
                            || {
                                replacement.default.as_ref().and_then(|format| {
                                    self.as_line(format, tag_separator, strategy, ctx)
                                })
                            },
                        );
                    }

                    Some(line)
                })
                .or_else(|| {
                    format
                        .default
                        .as_ref()
                        .and_then(|format| self.as_line(format, tag_separator, strategy, ctx))
                }),
            PropertyKindOrText::Transform(Transform::Truncate { content, length, from_start }) => {
                self.as_line(content, tag_separator, strategy, ctx)
                    .map(|mut line| {
                        let mut buf = VecDeque::new();
                        let mut remaining_len = *length;
                        let push_fn =
                            if *from_start { VecDeque::push_front } else { VecDeque::push_back };
                        let truncate_fn =
                            if *from_start { Span::truncate_start } else { Span::truncate_end };
                        let spans_len = line.spans.len();

                        for i in 0..spans_len {
                            if remaining_len == 0 {
                                break;
                            }
                            let i = if *from_start { spans_len - 1 - i } else { i };
                            let mut span = std::mem::take(&mut line.spans[i]);

                            let remaining = truncate_fn(&mut span, remaining_len);
                            push_fn(&mut buf, span);
                            remaining_len = remaining_len.saturating_sub(remaining);
                        }

                        line.spans = Vec::from(buf);
                        line
                    })
                    .or_else(|| {
                        format
                            .default
                            .as_ref()
                            .and_then(|format| self.as_line(format, tag_separator, strategy, ctx))
                    })
            }
        }
    }

    pub fn as_line_ellipsized<'song, 'stickers: 'song>(
        &'song self,
        format: &Property<SongProperty>,
        max_len: usize,
        symbols: &SymbolsConfig,
        tag_separator: &str,
        strategy: TagResolutionStrategy,
        ctx: &'stickers Ctx,
    ) -> Option<Line<'song>> {
        let mut line = self.as_line(format, tag_separator, strategy, ctx)?;

        let mut remaining = max_len;
        let mut idx = 0;

        let ellipsis_width = symbols.ellipsis.width();
        while remaining > 0 {
            let Some(span) = line.spans.get_mut(idx) else {
                break;
            };

            let sw = span.width();

            if sw < remaining {
                remaining -= sw;
                idx += 1;
                continue;
            }

            if sw == remaining {
                line.spans.truncate(idx + 1);
                break;
            }

            if remaining < ellipsis_width {
                // No space even for the configured ellipsis, just default the whole line to "…"
                span.content = Cow::Borrowed("…");
                line.spans.truncate(idx + 1);
                break;
            }

            let target = remaining - ellipsis_width;

            let mut owned = std::mem::take(&mut span.content).into_owned();

            let mut acc = 0;
            let mut cut_at_byte = 0;

            for (i, g) in owned.grapheme_indices(true) {
                let gw = g.width();
                if acc + gw > target {
                    cut_at_byte = i;
                    break;
                }
                acc += gw;
                cut_at_byte = i + g.len();
            }

            owned.truncate(cut_at_byte);
            owned.push_str(&symbols.ellipsis);
            span.content = Cow::Owned(owned);
            line.spans.truncate(idx + 1);
            break;
        }

        Some(line)
    }
}

impl Property<SongProperty> {
    fn default(
        &self,
        song: Option<&Song>,
        tag_separator: &str,
        strategy: TagResolutionStrategy,
        ctx: &Ctx,
    ) -> Option<String> {
        self.default.as_ref().and_then(|p| p.as_string(song, tag_separator, strategy, ctx))
    }

    pub fn as_string(
        &self,
        song: Option<&Song>,
        tag_separator: &str,
        strategy: TagResolutionStrategy,
        ctx: &Ctx,
    ) -> Option<String> {
        match &self.kind {
            PropertyKindOrText::Text(value) => Some((*value).clone()),
            PropertyKindOrText::Sticker(key) => song
                .and_then(|s| ctx.song_stickers(&s.file))
                .and_then(|s| s.get(key))
                .cloned()
                .or_else(|| self.default(song, tag_separator, strategy, ctx)),
            PropertyKindOrText::Property(property) => {
                if let Some(song) = song {
                    song.format(property, tag_separator, strategy).map_or_else(
                        || self.default(Some(song), tag_separator, strategy, ctx),
                        |v| Some(v.into_owned()),
                    )
                } else {
                    self.default(song, tag_separator, strategy, ctx)
                }
            }
            PropertyKindOrText::Group(group) => {
                let mut buf = String::new();
                for format in group {
                    if let Some(res) = format.as_string(song, tag_separator, strategy, ctx) {
                        buf.push_str(&res);
                    } else {
                        return self
                            .default
                            .as_ref()
                            .and_then(|d| d.as_string(song, tag_separator, strategy, ctx));
                    }
                }
                return Some(buf);
            }
            PropertyKindOrText::Transform(Transform::Replace { content, replacements }) => content
                .as_string(song, tag_separator, strategy, ctx)
                .and_then(|result| {
                    if let Some(replacement) = replacements.get(&result) {
                        return replacement.as_string(song, tag_separator, strategy, ctx).or_else(
                            || {
                                replacement
                                    .default
                                    .as_ref()
                                    .and_then(|d| d.as_string(song, tag_separator, strategy, ctx))
                            },
                        );
                    }

                    Some(result)
                })
                .or_else(|| {
                    self.default
                        .as_ref()
                        .and_then(|d| d.as_string(song, tag_separator, strategy, ctx))
                }),
            PropertyKindOrText::Transform(Transform::Truncate { content, length, from_start }) => {
                content
                    .as_string(song, tag_separator, strategy, ctx)
                    .map(|mut result| {
                        if *from_start {
                            result.truncate_start(*length);
                        } else {
                            result.truncate_end(*length);
                        }
                        result
                    })
                    .or_else(|| {
                        self.default
                            .as_ref()
                            .and_then(|d| d.as_string(song, tag_separator, strategy, ctx))
                    })
            }
        }
    }
}

impl Property<PropertyKind> {
    fn default_as_span<'song: 's, 's>(
        &'s self,
        song: Option<&'song Song>,
        ctx: &'song Ctx,
        tag_separator: &str,
        strategy: TagResolutionStrategy,
    ) -> Option<Either<Span<'s>, Vec<Span<'s>>>> {
        self.default.as_ref().and_then(|p| p.as_span(song, ctx, tag_separator, strategy))
    }

    pub fn as_span<'song: 's, 's>(
        &'s self,
        song: Option<&'song Song>,
        ctx: &'song Ctx,
        tag_separator: &str,
        strategy: TagResolutionStrategy,
    ) -> Option<Either<Span<'s>, Vec<Span<'s>>>> {
        let style = self.style.unwrap_or_default();
        let status = &ctx.status;
        match &self.kind {
            PropertyKindOrText::Text(value) => Some(Either::Left(Span::styled(value, style))),
            PropertyKindOrText::Sticker(key) => {
                if let Some(sticker) =
                    song.and_then(|s| ctx.song_stickers(&s.file)).and_then(|s| s.get(key))
                {
                    Some(Either::Left(Span::styled(sticker, style)))
                } else {
                    self.default_as_span(song, ctx, tag_separator, strategy)
                }
            }
            PropertyKindOrText::Property(PropertyKind::Song(property)) => {
                if let Some(song) = song {
                    song.format(property, tag_separator, strategy).map_or_else(
                        || self.default_as_span(Some(song), ctx, tag_separator, strategy),
                        |s| Some(Either::Left(Span::styled(s, style))),
                    )
                } else {
                    self.default_as_span(song, ctx, tag_separator, strategy)
                }
            }
            PropertyKindOrText::Property(PropertyKind::Status(s)) => match s {
                StatusProperty::State {
                    playing_label,
                    paused_label,
                    stopped_label,
                    playing_style,
                    paused_style,
                    stopped_style,
                } => Some(Either::Left(Span::styled(
                    match status.state {
                        State::Play => playing_label,
                        State::Stop => stopped_label,
                        State::Pause => paused_label,
                    },
                    match status.state {
                        State::Play => playing_style,
                        State::Stop => stopped_style,
                        State::Pause => paused_style,
                    }
                    .unwrap_or(style),
                ))),
                StatusProperty::Duration => {
                    Some(Either::Left(Span::styled(status.duration.to_string(), style)))
                }
                StatusProperty::Elapsed => {
                    Some(Either::Left(Span::styled(status.elapsed.to_string(), style)))
                }
                StatusProperty::Partition => {
                    Some(Either::Left(Span::styled(&status.partition, style)))
                }
                StatusProperty::Volume => {
                    Some(Either::Left(Span::styled(status.volume.value().to_string(), style)))
                }
                StatusProperty::Repeat { on_label, off_label, on_style, off_style } => {
                    Some(Either::Left(Span::styled(
                        if status.repeat { on_label } else { off_label },
                        if status.repeat { on_style } else { off_style }.unwrap_or(style),
                    )))
                }
                StatusProperty::Random { on_label, off_label, on_style, off_style } => {
                    Some(Either::Left(Span::styled(
                        if status.random { on_label } else { off_label },
                        if status.random { on_style } else { off_style }.unwrap_or(style),
                    )))
                }
                StatusProperty::Consume {
                    on_label,
                    off_label,
                    oneshot_label,
                    on_style,
                    off_style,
                    oneshot_style,
                } => Some(Either::Left(Span::styled(
                    match status.consume {
                        OnOffOneshot::On => on_label,
                        OnOffOneshot::Off => off_label,
                        OnOffOneshot::Oneshot => oneshot_label,
                    },
                    match status.consume {
                        OnOffOneshot::On => on_style,
                        OnOffOneshot::Off => off_style,
                        OnOffOneshot::Oneshot => oneshot_style,
                    }
                    .unwrap_or(style),
                ))),
                StatusProperty::Single {
                    on_label,
                    off_label,
                    oneshot_label,
                    on_style,
                    off_style,
                    oneshot_style,
                } => Some(Either::Left(Span::styled(
                    match status.single {
                        OnOffOneshot::On => on_label,
                        OnOffOneshot::Off => off_label,
                        OnOffOneshot::Oneshot => oneshot_label,
                    },
                    match status.single {
                        OnOffOneshot::On => on_style,
                        OnOffOneshot::Off => off_style,
                        OnOffOneshot::Oneshot => oneshot_style,
                    }
                    .unwrap_or(style),
                ))),
                StatusProperty::Bitrate => status.bitrate.as_ref().map_or_else(
                    || self.default_as_span(song, ctx, tag_separator, strategy),
                    |v| Some(Either::Left(Span::styled(v.to_string(), style))),
                ),
                StatusProperty::Crossfade => status.xfade.as_ref().map_or_else(
                    || self.default_as_span(song, ctx, tag_separator, strategy),
                    |v| Some(Either::Left(Span::styled(v.to_string(), style))),
                ),
                StatusProperty::QueueLength { thousands_separator } => {
                    Some(Either::Left(Span::styled(
                        ctx.queue.len().with_thousands_separator(thousands_separator),
                        style,
                    )))
                }
                StatusProperty::QueueTimeTotal { separator } => {
                    let formatted = match separator {
                        Some(sep) => ctx.cached_queue_time_total.format_to_duration(sep),
                        None => ctx.cached_queue_time_total.to_string(),
                    };
                    Some(Either::Left(Span::styled(formatted, style)))
                }
                StatusProperty::QueueTimeRemaining { separator } => {
                    let remaining_time = ctx.find_current_song_in_queue().map_or(
                        Duration::default(),
                        |(current_song_idx, current_song)| {
                            let total_remaining: Duration = ctx
                                .queue
                                .iter()
                                .skip(current_song_idx)
                                .filter_map(|s| s.duration)
                                .sum();
                            if current_song.duration.is_some() {
                                total_remaining.saturating_sub(ctx.status.elapsed)
                            } else {
                                total_remaining
                            }
                        },
                    );
                    let formatted = match separator {
                        Some(sep) => remaining_time.format_to_duration(sep),
                        None => remaining_time.to_string(),
                    };
                    Some(Either::Left(Span::styled(formatted, style)))
                }
                StatusProperty::QueueBoxTitle() => {
                    // The queue box's bottom title adapts to the current
                    // view: queue mode shows "N songs / total time",
                    // chapters mode "N Chapters / total time" (the chapter
                    // count and the sum of the chapter durations), video
                    // mode "N videos / total time" (the mpv playlist's
                    // known durations).
                    let chapters = ctx.current_playback_chapters();
                    let chapters_on = ctx.queue_tab.get() == crate::ctx::QueueTabMode::Chapters
                        && !chapters.is_empty();
                    let video_on = ctx.queue_tab.get() == crate::ctx::QueueTabMode::Video;
                    let (count, total) = if chapters_on {
                        let secs: f64 = chapters.iter().map(|c| c.duration()).sum();
                        (chapters.len(), Duration::from_secs_f64(secs.max(0.0)))
                    } else if video_on {
                        // The Jellyfin session's playlist while a Jellyfin
                        // item plays, else the persistent video playlist.
                        let jellyfin = crate::core::mpv::session_playlist_shown(ctx);
                        let entries: std::cell::Ref<'_, Vec<crate::core::mpv::MpvPlaylistEntry>> =
                            if jellyfin {
                                ctx.mpv.playlist.borrow()
                            } else {
                                ctx.video_playlist.borrow()
                            };
                        let secs: f64 = entries.iter().filter_map(|e| e.duration).sum();
                        (entries.len(), Duration::from_secs_f64(secs))
                    } else {
                        (ctx.queue.len(), ctx.cached_queue_time_total)
                    };
                    // DurationExt::to_string formats it as "52:26".
                    let total = total.to_string();
                    let title = if chapters_on {
                        format!("{count} Chapters / {total} total time")
                    } else if video_on {
                        format!("{count} videos / {total} total time")
                    } else {
                        format!("{count} songs / {total} total time")
                    };
                    Some(Either::Left(Span::styled(title, style)))
                }
                StatusProperty::ActiveTab => {
                    Some(Either::Left(Span::styled(ctx.active_tab.0.as_ref(), style)))
                }
                StatusProperty::InputBuffer() => {
                    Some(Either::Left(Span::styled(ctx.key_resolver.buffer_to_string(), style)))
                }
                StatusProperty::InputMode() => Some(Either::Left(Span::styled(
                    ctx.input.mode().discriminant().to_string(),
                    style,
                ))),
                StatusProperty::SampleRate() => {
                    status.samplerate().map(|v| Either::Left(Span::styled(v.to_string(), style)))
                }
                StatusProperty::Bits() => {
                    status.bits().map(|v| Either::Left(Span::styled(v.to_string(), style)))
                }
                StatusProperty::Channels() => {
                    status.channels().map(|v| Either::Left(Span::styled(v.to_string(), style)))
                }
            },
            PropertyKindOrText::Property(PropertyKind::Widget(w)) => match w {
                WidgetProperty::Volume => {
                    Some(Either::Left(Span::styled(Volume::get_str(*status.volume.value()), style)))
                }
                WidgetProperty::States { active_style, separator_style } => {
                    let separator = Span::styled(" / ", *separator_style);
                    Some(Either::Right(vec![
                        Span::styled("Repeat", if status.repeat { *active_style } else { style }),
                        separator.clone(),
                        Span::styled("Random", if status.random { *active_style } else { style }),
                        separator.clone(),
                        match status.consume {
                            OnOffOneshot::On => Span::styled("Consume", *active_style),
                            OnOffOneshot::Off => Span::styled("Consume", style),
                            OnOffOneshot::Oneshot => Span::styled("Oneshot(C)", *active_style),
                        },
                        separator,
                        match status.single {
                            OnOffOneshot::On => Span::styled("Single", *active_style),
                            OnOffOneshot::Off => Span::styled("Single", style),
                            OnOffOneshot::Oneshot => Span::styled("Oneshot(S)", *active_style),
                        },
                    ]))
                }
                WidgetProperty::ScanStatus => ctx
                    .db_update_start
                    .map(|update_start| {
                        Either::Left(Span::styled(
                            ScanStatus::new(Some(update_start))
                                .get_str()
                                .unwrap_or_default()
                                .to_string(),
                            style,
                        ))
                    })
                    .or_else(|| self.default_as_span(song, ctx, tag_separator, strategy)),
            },
            PropertyKindOrText::Group(group) => {
                let mut buf = Vec::new();
                for format in group {
                    match format.as_span(song, ctx, tag_separator, strategy) {
                        Some(Either::Left(span)) => buf.push(span),
                        Some(Either::Right(spans)) => buf.extend(spans),
                        None => return self.default_as_span(song, ctx, tag_separator, strategy),
                    }
                }
                return Some(Either::Right(buf));
            }
            PropertyKindOrText::Transform(Transform::Replace { content, replacements }) => {
                match content.as_span(song, ctx, tag_separator, strategy) {
                    Some(Either::Left(span)) => {
                        if let Some(replacement) = replacements.get(span.content.as_ref()) {
                            return replacement
                                .as_span(song, ctx, tag_separator, strategy)
                                .or_else(|| {
                                    replacement.default_as_span(song, ctx, tag_separator, strategy)
                                });
                        }

                        Some(Either::Left(span))
                    }
                    Some(Either::Right(spans)) => {
                        let mut content = String::new();
                        for span in &spans {
                            content.push_str(span.content.as_ref());
                        }

                        if let Some(replacement) = replacements.get(&content) {
                            return replacement
                                .as_span(song, ctx, tag_separator, strategy)
                                .or_else(|| {
                                    replacement.default_as_span(song, ctx, tag_separator, strategy)
                                });
                        }

                        Some(Either::Right(spans))
                    }
                    None => self.default_as_span(song, ctx, tag_separator, strategy),
                }
            }
            PropertyKindOrText::Transform(Transform::Truncate { content, length, from_start }) => {
                let truncate_fn =
                    if *from_start { Span::truncate_start } else { Span::truncate_end };
                match content.as_span(song, ctx, tag_separator, strategy) {
                    Some(Either::Left(mut span)) => {
                        truncate_fn(&mut span, *length);
                        Some(Either::Left(span))
                    }
                    Some(Either::Right(mut spans)) => {
                        let mut buf = VecDeque::new();
                        let mut remaining_len = *length;
                        let push_fn =
                            if *from_start { VecDeque::push_front } else { VecDeque::push_back };
                        let spans_len = spans.len();

                        for i in 0..spans.len() {
                            if remaining_len == 0 {
                                break;
                            }
                            let i = if *from_start { spans_len - 1 - i } else { i };
                            let mut span = std::mem::take(&mut spans[i]);

                            let remaining = truncate_fn(&mut span, remaining_len);
                            push_fn(&mut buf, span);
                            remaining_len = remaining_len.saturating_sub(remaining);
                        }
                        Some(Either::Right(buf.into()))
                    }
                    None => self.default_as_span(song, ctx, tag_separator, strategy),
                }
            }
        }
    }
}

impl SizedPaneOrSplit {
    pub fn for_each_pane(
        &self,
        area: Rect,
        root_height: u16,
        pane_callback: &mut impl FnMut(&ConfigPane, Rect, Block, Rect, Option<Color>) -> Result<()>,
        ctx: &Ctx,
    ) -> Result<()> {
        self.for_each_pane_custom_data(
            area,
            root_height,
            (),
            &mut |pane, pane_area, block, block_area, background_color, ()| {
                pane_callback(pane, pane_area, block, block_area, background_color)?;
                Ok(())
            },
            &mut |_, _, _, ()| Ok(()),
            ctx,
        )
    }

    /// Resolve the size of a pane given the terminal window height, using
    /// (height, size) breakpoints with linear interpolation between them.
    fn window_size_at(root_height: u16, points: &[(u16, PercentOrLength)]) -> PercentOrLength {
        let Some((first_h, first_size)) = points.first().copied() else {
            return PercentOrLength::Length(0);
        };
        if root_height >= first_h {
            return first_size;
        }
        for pair in points.windows(2) {
            let (h1, s1) = (pair[0].0, pair[0].1);
            let (h2, s2) = (pair[1].0, pair[1].1);
            if root_height <= h1 && root_height >= h2 {
                let (PercentOrLength::Length(l1), PercentOrLength::Length(l2)) = (s1, s2) else {
                    return s1;
                };
                let span = f64::from(h1 - h2);
                let frac = if span == 0.0 { 0.0 } else { f64::from(h1 - root_height) / span };
                let len = (f64::from(l1) - (f64::from(l1) - f64::from(l2)) * frac).round();
                return PercentOrLength::Length(len.max(0.0) as u16);
            }
        }
        points.last().map(|p| p.1).unwrap_or(PercentOrLength::Length(0))
    }

    pub fn for_each_pane_custom_data<T>(
        &self,
        area: Rect,
        root_height: u16,
        mut custom_data: T,
        pane_callback: &mut impl FnMut(
            &ConfigPane,
            Rect,
            Block,
            Rect,
            Option<Color>,
            &mut T,
        ) -> Result<()>,
        split_callback: &mut impl FnMut(Block, Rect, Option<Color>, &mut T) -> Result<()>,
        ctx: &Ctx,
    ) -> Result<()> {
        let mut stack = vec![(self, area)];

        let song = ctx.find_current_song_in_queue().map(|(_, song)| song);
        while let Some((configured_panes, area)) = stack.pop() {
            match configured_panes {
                SizedPaneOrSplit::Pane(pane) => {
                    // Panes hidden from the Settings panel are skipped
                    // entirely (their border box is not drawn either).
                    if ctx.is_pane_hidden(&pane.pane) {
                        continue;
                    }
                    let mut block = Block::default()
                        .borders(pane.borders)
                        .border_set((&pane.border_symbols).into());
                    let bg_color = pane.background_color;
                    if pane.border_title.is_empty() {
                        let pane_area = block.inner(area);
                        pane_callback(pane, pane_area, block, area, bg_color, &mut custom_data)?;
                    } else {
                        let templs = PropertyTemplates::new(&pane.border_title);
                        let title = templs.format(song, ctx, &ctx.config);

                        block = block
                            .title(title)
                            .title_position(pane.border_title_position)
                            .title_alignment(pane.border_title_alignment);

                        let pane_area = block.inner(area);
                        pane_callback(pane, pane_area, block, area, bg_color, &mut custom_data)?;
                    }
                }
                SizedPaneOrSplit::Split {
                    direction,
                    panes,
                    background_color,
                    borders,
                    border_style,
                    border_title,
                    border_title_position,
                    border_title_alignment,
                    border_symbols,
                } => {
                    let parent_other_size = match direction {
                        ratatui::layout::Direction::Horizontal => area.height,
                        ratatui::layout::Direction::Vertical => area.width,
                    };
                    let split_size = match direction {
                        ratatui::layout::Direction::Horizontal => area.width,
                        ratatui::layout::Direction::Vertical => area.height,
                    };
                    // Panes hidden from the Settings panel are filtered out of
                    // the split before constraints are solved, so the
                    // remaining panes re-distribute the freed space (e.g.
                    // hiding the album art makes the lyrics box take the full
                    // width).
                    let visible: Vec<&SizedSubPane> = panes
                        .iter()
                        .filter(|sub_pane| !Self::is_sub_pane_hidden(&sub_pane.pane, ctx))
                        .collect();
                    // A split with no visible leaves (e.g. both album art and
                    // lyrics disabled) collapses completely: skip it so the
                    // freed space is redistributed to the remaining panes.
                    if visible.is_empty() {
                        continue;
                    }
                    // When the only pane left in a split is the album art it is
                    // stretched to the full split width so the image renders
                    // centered (its pixel-size ratio would otherwise keep it as
                    // a narrow strip).
                    let removed_any = panes.len() > visible.len();
                    let album_art_full_width = removed_any
                        && visible.len() == 1
                        && matches!(
                            &visible[0].pane,
                            SizedPaneOrSplit::Pane(p) if matches!(p.pane, PaneType::AlbumArt)
                        );
                    let constraints: Vec<Constraint> = if album_art_full_width {
                        vec![Constraint::Percentage(100)]
                    } else {
                        visible.iter().map(|pane| {
                            if let Some(min) = pane.collapse_below
                                && split_size < min
                            {
                                return Constraint::Length(0);
                            }
                            let size = if pane.window_sizes.is_empty() {
                                pane.size
                            } else {
                                Self::window_size_at(root_height, &pane.window_sizes)
                            };
                            match (pane.shrink_below, size) {
                                (Some(min), PercentOrLength::Length(s)) if split_size < min => {
                                    Constraint::Length(s / 2)
                                }
                                _ => {
                                    let constraint = size.into_constraint(parent_other_size);
                                    // Responsive sub-panes (`window_sizes`)
                                    // hide entirely — no box, no borders, the
                                    // space freed — when they can no longer
                                    // fit their content minimum (4 rows plus
                                    // the borders around them). The queue
                                    // tab's art+lyrics split below ~6 rows
                                    // would otherwise render as an empty
                                    // bordered shell (its content already
                                    // hides below its own 4-row minimum).
                                    if !pane.window_sizes.is_empty()
                                        && *direction == ratatui::layout::Direction::Vertical
                                        && let Some(height) =
                                            Self::constraint_height(&constraint, split_size)
                                        && height < Self::content_min_height(&pane.pane)
                                    {
                                        Constraint::Length(0)
                                    } else {
                                        constraint
                                    }
                                }
                            }
                        })
                        .collect()
                    };
                    let border_style = border_style.unwrap_or_else(|| ctx.config.as_border_style());
                    let mut block = Block::default()
                        .borders(*borders)
                        .border_style(border_style)
                        .border_set(border_symbols.into());

                    let templs = PropertyTemplates::new(border_title);
                    let title = if border_title.is_empty() {
                        None
                    } else {
                        Some(templs.format(song, ctx, &ctx.config))
                    };
                    if let Some(title) = title {
                        block = block
                            .title(title)
                            .title_position(*border_title_position)
                            .title_alignment(*border_title_alignment);
                    }

                    let pane_areas = block.inner(area);
                    let split_rect = if stack.is_empty()
                        && matches!(direction, ratatui::layout::Direction::Vertical)
                        && constraints.iter().all(|c| matches!(c, Constraint::Length(_)))
                    {
                        // Root stack is all fixed-height (tabs, tab content and
                        // cava have collapsed away): center the remaining
                        // content vertically in the leftover space.
                        let used: u16 = constraints
                            .iter()
                            .map(|c| match c {
                                Constraint::Length(l) => *l,
                                _ => 0,
                            })
                            .sum();
                        if used > 0 && used < pane_areas.height {
                            Rect {
                                x: pane_areas.x,
                                y: pane_areas.y + (pane_areas.height - used) / 2,
                                width: pane_areas.width,
                                height: used,
                            }
                        } else {
                            pane_areas
                        }
                    } else {
                        pane_areas
                    };
                    let areas = Layout::new(*direction, constraints).split(split_rect);
                    split_callback(block, area, *background_color, &mut custom_data)?;
                    stack.extend(
                        areas.iter().enumerate().map(|(idx, area)| {
                            (&visible[idx].pane, *area)
                        }),
                    );
                }
            }
        }

        Ok(())
    }

    /// True when a sub-pane is hidden via the Settings panel. Leaf panes are
    /// hidden individually; a split counts as hidden when every pane inside
    /// it is hidden too (e.g. album art + lyrics both disabled), so the
    /// whole split collapses and the rest of the layout fills the space.
    fn is_sub_pane_hidden(pane: &SizedPaneOrSplit, ctx: &Ctx) -> bool {
        match pane {
            SizedPaneOrSplit::Pane(p) => ctx.is_pane_hidden(&p.pane),
            SizedPaneOrSplit::Split { panes, .. } => {
                panes.iter().all(|sub| Self::is_sub_pane_hidden(&sub.pane, ctx))
            }
        }
    }

    /// The resolved height of a constraint against the split's total (the
    /// dimension being split). Returns None for constraints the pane sizes
    /// never produce (Min/Max/Fill).
    fn constraint_height(constraint: &Constraint, total: u16) -> Option<u16> {
        match constraint {
            Constraint::Length(l) => Some(*l),
            Constraint::Percentage(p) => Some(
                u16::try_from(u32::from(total) * u32::from(*p) / 100).unwrap_or(u16::MAX),
            ),
            Constraint::Ratio(a, b) => {
                Some(u16::try_from(u32::from(total) * *a / *b).unwrap_or(u16::MAX))
            }
            _ => None,
        }
    }

    /// The minimum total height a sub-pane needs to show its content:
    /// [`MIN_PANE_CONTENT_HEIGHT`] rows plus the rows its borders take from
    /// its top/bottom edges. A horizontal split's leaves span its full
    /// height, so the max over the leaves counts.
    fn content_min_height(pane: &SizedPaneOrSplit) -> u16 {
        fn border_rows(b: Borders) -> u16 {
            u16::from(b.contains(Borders::TOP)) + u16::from(b.contains(Borders::BOTTOM))
        }
        match pane {
            SizedPaneOrSplit::Pane(p) => MIN_PANE_CONTENT_HEIGHT + border_rows(p.borders),
            SizedPaneOrSplit::Split { borders, panes, .. } => {
                border_rows(*borders)
                    + panes
                        .iter()
                        .map(|sub| Self::content_min_height(&sub.pane))
                        .max()
                        .unwrap_or(0)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod format_tests {
    use std::{collections::HashMap, time::Duration};

    use either::Either;
    use ratatui::{style::Style, text::Span};
    use rstest::rstest;

    use crate::{
        config::theme::{
            StyleFile,
            TagResolutionStrategy,
            properties::{
                Property,
                PropertyKind,
                PropertyKindOrText,
                SongProperty,
                StatusProperty,
                StatusPropertyFile,
            },
        },
        ctx::Ctx,
        mpd::commands::{Song, State, Status, Volume, status::OnOffOneshot},
        tests::fixtures::ctx,
    };

    mod replace {
        use super::*;
        use crate::config::theme::{SymbolsConfig, properties::Transform};

        #[rstest]
        // simple 1:1 replace
        #[case(PropertyKindOrText::Text("abcdefgh".into()),
            None,
            "abcdefgh",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "replaced text")]
        // No replace input found
        #[case(PropertyKindOrText::Text("a".into()),
            None,
            "abcdefgh",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "a")]
        // Replace of group
        #[case(PropertyKindOrText::Group(vec![Property { kind: PropertyKindOrText::Text("a".into()), style: None, default: None }, Property { kind: PropertyKindOrText::Text("b".into()), style: None, default: None }]),
            None,
            "ab",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "replaced text")]
        // No replace of input found, fallback to original default
        #[case(PropertyKindOrText::Sticker("does not exist".into()),
            Some(PropertyKindOrText::Text("original default".into())),
            "does not match",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "original default")]
        // Replace found, but resolved to None - use replacement's default
        #[case(PropertyKindOrText::Text("a".into()),
            Some(PropertyKindOrText::Text("original default".into())),
            "a",
            PropertyKindOrText::Sticker("does not exist".into()),
            Some(PropertyKindOrText::Text("replacement default".into())),
            "replacement default")]
        fn as_span(
            #[case] input_props: PropertyKindOrText<PropertyKind>,
            #[case] input_default: Option<PropertyKindOrText<PropertyKind>>,
            #[case] input: String,
            #[case] replace_props: PropertyKindOrText<PropertyKind>,
            #[case] replace_default: Option<PropertyKindOrText<PropertyKind>>,
            #[case] expected: String,
            ctx: Ctx,
        ) {
            let format = Property::<PropertyKind> {
                kind: PropertyKindOrText::Transform(Transform::Replace {
                    content: Box::new(Property { kind: input_props, style: None, default: None }),
                    replacements: [(input, Property {
                        kind: replace_props,
                        style: None,
                        default: replace_default
                            .map(|d| Box::new(Property { kind: d, style: None, default: None })),
                    })]
                    .into_iter()
                    .collect(),
                }),
                style: None,
                default: input_default
                    .map(|d| Box::new(Property { kind: d, style: None, default: None })),
            };

            let result = format.as_span(None, &ctx, "", TagResolutionStrategy::All);

            assert_eq!(
                match result {
                    Some(Either::Left(v)) => Some(v.content.into_owned()),
                    Some(Either::Right(v)) =>
                        Some(v.iter().map(|s| s.content.clone()).collect::<String>()),
                    None => None,
                },
                Some(expected)
            );
        }

        #[rstest]
        // simple 1:1 replace
        #[case(PropertyKindOrText::Text("abcdefgh".into()),
            None,
            "abcdefgh",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "replaced text")]
        // No replace input found
        #[case(PropertyKindOrText::Text("a".into()),
            None,
            "abcdefgh",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "a")]
        // Replace of group
        #[case(PropertyKindOrText::Group(vec![Property { kind: PropertyKindOrText::Text("a".into()), style: None, default: None }, Property { kind: PropertyKindOrText::Text("b".into()), style: None, default: None }]),
            None,
            "ab",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "replaced text")]
        // No replace of input found, fallback to original default
        #[case(PropertyKindOrText::Sticker("does not exist".into()),
            Some(PropertyKindOrText::Text("original default".into())),
            "does not match",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "original default")]
        // Replace found, but resolved to None - use replacement's default
        #[case(PropertyKindOrText::Text("a".into()),
            Some(PropertyKindOrText::Text("original default".into())),
            "a",
            PropertyKindOrText::Sticker("does not exist".into()),
            Some(PropertyKindOrText::Text("replacement default".into())),
            "replacement default")]
        fn as_string(
            #[case] input_props: PropertyKindOrText<SongProperty>,
            #[case] input_default: Option<PropertyKindOrText<SongProperty>>,
            #[case] input: String,
            #[case] replace_props: PropertyKindOrText<SongProperty>,
            #[case] replace_default: Option<PropertyKindOrText<SongProperty>>,
            #[case] expected: &str,
            ctx: Ctx,
        ) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Transform(Transform::Replace {
                    content: Box::new(Property { kind: input_props, style: None, default: None }),
                    replacements: [(input, Property {
                        kind: replace_props,
                        style: None,
                        default: replace_default
                            .map(|d| Box::new(Property { kind: d, style: None, default: None })),
                    })]
                    .into_iter()
                    .collect(),
                }),
                style: None,
                default: input_default
                    .map(|d| Box::new(Property { kind: d, style: None, default: None })),
            };

            let result = format.as_string(None, "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some(expected.to_string()));
        }

        #[rstest]
        #[rstest]
        // simple 1:1 replace
        #[case(PropertyKindOrText::Text("abcdefgh".into()),
            None,
            "abcdefgh",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "replaced text")]
        // No replace input found
        #[case(PropertyKindOrText::Text("a".into()),
            None,
            "abcdefgh",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "a")]
        // Replace of group
        #[case(PropertyKindOrText::Group(vec![Property { kind: PropertyKindOrText::Text("a".into()), style: None, default: None }, Property { kind: PropertyKindOrText::Text("b".into()), style: None, default: None }]),
            None,
            "ab",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "replaced text")]
        // No replace of input found, fallback to original default
        #[case(PropertyKindOrText::Sticker("does not exist".into()),
            Some(PropertyKindOrText::Text("original default".into())),
            "does not match",
            PropertyKindOrText::Text("replaced text".into()),
            None,
            "original default")]
        // Replace found, but resolved to None - use replacement's default
        #[case(PropertyKindOrText::Text("a".into()),
            Some(PropertyKindOrText::Text("original default".into())),
            "a",
            PropertyKindOrText::Sticker("does not exist".into()),
            Some(PropertyKindOrText::Text("replacement default".into())),
            "replacement default")]
        fn as_line_ellipsized(
            #[case] input_props: PropertyKindOrText<SongProperty>,
            #[case] input_default: Option<PropertyKindOrText<SongProperty>>,
            #[case] input: String,
            #[case] replace_props: PropertyKindOrText<SongProperty>,
            #[case] replace_default: Option<PropertyKindOrText<SongProperty>>,
            #[case] expected: String,
            ctx: Ctx,
        ) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Transform(Transform::Replace {
                    content: Box::new(Property { kind: input_props, style: None, default: None }),
                    replacements: [(input, Property {
                        kind: replace_props,
                        style: None,
                        default: replace_default
                            .map(|d| Box::new(Property { kind: d, style: None, default: None })),
                    })]
                    .into_iter()
                    .collect(),
                }),
                style: None,
                default: input_default
                    .map(|d| Box::new(Property { kind: d, style: None, default: None })),
            };

            let song = Song::default();
            let result = song.as_line_ellipsized(
                &format,
                999,
                &SymbolsConfig::default(),
                "",
                TagResolutionStrategy::All,
                &ctx,
            );

            assert_eq!(
                result.map(|line| line.spans.iter().map(|s| s.content.clone()).collect::<String>()),
                Some(expected)
            );
        }
    }

    mod truncate {
        use itertools::Itertools;
        use ratatui::text::Line;

        use super::*;
        use crate::config::theme::{SymbolsConfig, properties::Transform};

        #[rstest]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 0, false, Either::Left(""))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 0, true, Either::Left(""))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 3, false, Either::Left("abc"))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 3, true, Either::Left("fgh"))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 8, false, Either::Left("abcdefgh"))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 8, true, Either::Left("abcdefgh"))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 99, false, Either::Left("abcdefgh"))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 99, true, Either::Left("abcdefgh"))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 3, false, Either::Right(vec!["ab", "c"]))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 3, true, Either::Right(vec!["f", "gh"]))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 8, false, Either::Right(vec!["ab", "cd", "ef", "gh"]))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 8, true, Either::Right(vec!["ab", "cd", "ef", "gh"]))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 99, false, Either::Right(vec!["ab", "cd", "ef", "gh"]))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 99, true, Either::Right(vec!["ab", "cd", "ef", "gh"]))]
        fn as_span(
            ctx: Ctx,
            #[case] props: PropertyKindOrText<PropertyKind>,
            #[case] length: usize,
            #[case] from_start: bool,
            #[case] expected: Either<&str, Vec<&str>>,
        ) {
            let format = Property::<PropertyKind> {
                kind: PropertyKindOrText::Transform(Transform::Truncate {
                    content: Box::new(Property { kind: props, style: None, default: None }),
                    length,
                    from_start,
                }),
                style: None,
                default: None,
            };

            let result = format.as_span(None, &ctx, "", TagResolutionStrategy::All);

            assert_eq!(
                result,
                Some(match expected {
                    Either::Left(value) =>
                        either::Either::<Span<'_>, Vec<Span<'_>>>::Left(Span::raw(value)),
                    Either::Right(values) => either::Either::<Span<'_>, Vec<Span<'_>>>::Right(
                        values.into_iter().map(Span::raw).collect()
                    ),
                })
            );
        }

        #[rstest]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 0, false, "")]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 0, true, "")]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 3, false, "abc")]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 3, true, "fgh")]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 8, false, "abcdefgh")]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 8, true, "abcdefgh")]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 99, false, "abcdefgh")]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 99, true, "abcdefgh")]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 3, false, "abc")]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 3, true, "fgh")]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 8, false, "abcdefgh")]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 8, true, "abcdefgh")]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 99, false, "abcdefgh")]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 99, true, "abcdefgh")]
        fn as_string(
            #[case] props: PropertyKindOrText<SongProperty>,
            #[case] length: usize,
            #[case] from_start: bool,
            #[case] expected: &str,
            ctx: Ctx,
        ) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Transform(Transform::Truncate {
                    content: Box::new(Property { kind: props, style: None, default: None }),
                    length,
                    from_start,
                }),
                style: None,
                default: None,
            };

            let result = format.as_string(None, "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some(expected.to_string()));
        }

        #[rstest]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 0, false, Either::Left(""))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 0, true, Either::Left(""))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 3, false, Either::Left("abc"))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 3, true, Either::Left("fgh"))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 8, false, Either::Left("abcdefgh"))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 8, true, Either::Left("abcdefgh"))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 99, false, Either::Left("abcdefgh"))]
        #[case(PropertyKindOrText::Text("abcdefgh".into()), 99, true, Either::Left("abcdefgh"))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 3, false, Either::Right(vec!["ab", "c"]))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 3, true, Either::Right(vec!["f", "gh"]))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 8, false, Either::Right(vec!["ab", "cd", "ef", "gh"]))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 8, true, Either::Right(vec!["ab", "cd", "ef", "gh"]))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 99, false, Either::Right(vec!["ab", "cd", "ef", "gh"]))]
        #[case(PropertyKindOrText::Group(vec![
                Property::builder().kind(PropertyKindOrText::Text("ab".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("cd".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("ef".into())).build(),
                Property::builder().kind(PropertyKindOrText::Text("gh".into())).build(),
            ]), 99, true, Either::Right(vec!["ab", "cd", "ef", "gh"]))]
        fn as_line_ellipsized(
            #[case] props: PropertyKindOrText<SongProperty>,
            #[case] length: usize,
            #[case] from_start: bool,
            #[case] expected: Either<&str, Vec<&str>>,
            ctx: Ctx,
        ) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Transform(Transform::Truncate {
                    content: Box::new(Property { kind: props, style: None, default: None }),
                    length,
                    from_start,
                }),
                style: None,
                default: None,
            };

            let song = Song::default();
            let result = song.as_line_ellipsized(
                &format,
                999,
                &SymbolsConfig::default(),
                "",
                TagResolutionStrategy::All,
                &ctx,
            );

            assert_eq!(
                result,
                Some(match expected {
                    Either::Left(value) => Line::from(value),
                    Either::Right(values) =>
                        Line::from(values.into_iter().map(Span::raw).collect_vec()),
                })
            );
        }
    }

    mod correct_values {
        use super::*;

        #[rstest]
        #[case(SongProperty::Title, "title")]
        #[case(SongProperty::Artist, "artist")]
        #[case(SongProperty::Album, "album")]
        #[case(SongProperty::Track, "123")]
        #[case(SongProperty::Duration, "2:03")]
        #[case(SongProperty::Other("track".to_string()), "123")]
        fn song_property_resolves_correctly(
            #[case] prop: SongProperty,
            #[case] expected: &str,
            ctx: Ctx,
        ) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Property(prop),
                style: None,
                default: None,
            };

            let song = Song {
                id: 123,
                file: "file".to_owned(),
                duration: Some(Duration::from_secs(123)),
                metadata: HashMap::from([
                    ("title".to_string(), "title".into()),
                    ("album".to_string(), "album".into()),
                    ("track".to_string(), "123".into()),
                    ("artist".to_string(), "artist".into()),
                ]),
                last_modified: chrono::Utc::now(),
                added: None,
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some(expected.to_string()));
        }

        #[rstest]
        #[case(StatusProperty::Volume, "100")]
        #[case(StatusProperty::Elapsed, "2:03")]
        #[case(StatusProperty::Duration, "2:03")]
        #[case(StatusProperty::Crossfade, "3")]
        #[case(StatusProperty::Bitrate, "123")]
        fn status_property_resolves_correctly(
            mut ctx: Ctx,
            #[case] prop: StatusProperty,
            #[case] expected: &str,
        ) {
            let format = Property::<PropertyKind> {
                kind: PropertyKindOrText::Property(PropertyKind::Status(prop)),
                style: None,
                default: None,
            };

            let song = Song {
                id: 123,
                file: "file".to_owned(),
                duration: Some(Duration::from_secs(123)),
                metadata: HashMap::from([
                    ("artist".to_string(), "artist".into()),
                    ("album".to_string(), "album".into()),
                    ("title".to_string(), "title".into()),
                    ("track".to_string(), "123".into()),
                ]),
                last_modified: chrono::Utc::now(),
                added: None,
            };
            ctx.status = Status {
                volume: Volume::new(123),
                repeat: true,
                random: true,
                single: OnOffOneshot::On,
                consume: OnOffOneshot::On,
                bitrate: Some(123),
                elapsed: Duration::from_secs(123),
                duration: Duration::from_secs(123),
                xfade: Some(3),
                state: State::Play,
                ..Default::default()
            };

            let result = format.as_span(Some(&song), &ctx, "", TagResolutionStrategy::All);

            assert_eq!(
                result,
                Some(either::Either::<Span<'_>, Vec<Span<'_>>>::Left(Span::raw(expected)))
            );
        }

        #[rstest]
        // Standard format tests (no separator = MM:SS format)
        #[case(StatusProperty::QueueTimeTotal { separator: None }, "6:09", Duration::from_secs(0))]
        #[case(StatusProperty::QueueTimeTotal { separator: Some(String::new())}, "6m9s", Duration::from_secs(0))]
        #[case(StatusProperty::QueueTimeRemaining { separator: None }, "6:09", Duration::from_secs(0))]
        #[case(StatusProperty::QueueTimeRemaining { separator: Some(String::new()) }, "6m9s", Duration::from_secs(0))]
        // With elapsed time, remaining should subtract elapsed from current song
        #[case(StatusProperty::QueueTimeRemaining { separator: None }, "5:49", Duration::from_secs(20))]
        #[case(StatusProperty::QueueTimeRemaining { separator: None }, "5:09", Duration::from_secs(60))]
        // Verbose format tests (with separator = verbose format)
        #[case(StatusProperty::QueueTimeTotal { separator: Some(",".to_string()) }, "6m,9s", Duration::from_secs(0))]
        #[case(StatusProperty::QueueTimeRemaining { separator: Some(",".to_string()) }, "6m,9s", Duration::from_secs(0))]
        #[case(StatusProperty::QueueTimeRemaining { separator: Some(",".to_string()) }, "5m,49s", Duration::from_secs(20))]
        fn queue_time_property_resolves_correctly(
            mut ctx: Ctx,
            #[case] prop: StatusProperty,
            #[case] expected: &str,
            #[case] elapsed: Duration,
        ) {
            let format = Property::<PropertyKind> {
                kind: PropertyKindOrText::Property(PropertyKind::Status(prop)),
                style: None,
                default: None,
            };

            // Test with a fake current song
            let current_song = Song {
                id: 0,
                file: "current.mp3".to_owned(),
                duration: Some(Duration::from_secs(123)),
                metadata: HashMap::from([
                    ("title".to_string(), "Current Song".into()),
                    ("artist".to_string(), "Artist".into()),
                ]),
                last_modified: chrono::Utc::now(),
                added: None,
            };

            // Set up the app context with a fake queue and status
            let mut queue = vec![current_song.clone()];
            queue.push(Song {
                id: 1,
                file: "song1.mp3".to_owned(),
                duration: Some(Duration::from_secs(123)),
                metadata: HashMap::from([("title".to_string(), "Song 1".into())]),
                last_modified: chrono::Utc::now(),
                added: None,
            });
            queue.push(Song {
                id: 2,
                file: "song2.mp3".to_owned(),
                duration: Some(Duration::from_secs(123)),
                metadata: HashMap::from([("title".to_string(), "Song 2".into())]),
                last_modified: chrono::Utc::now(),
                added: None,
            });

            ctx.queue = queue;
            ctx.status = Status {
                elapsed,
                duration: Duration::from_secs(123),
                state: State::Play,
                song: Some(0),
                songid: Some(0),
                ..Default::default()
            };
            ctx.cached_queue_time_total =
                ctx.queue.iter().map(|s| s.duration.unwrap_or_default()).sum();

            let result = format.as_span(Some(&current_song), &ctx, "", TagResolutionStrategy::All);

            assert_eq!(
                result,
                Some(either::Either::<Span<'_>, Vec<Span<'_>>>::Left(Span::raw(expected)))
            );
        }

        #[test]
        fn queue_box_title_switches_between_queue_and_chapters() {
            let (app_tx, _app_rx) = crossbeam::channel::unbounded();
            let mut ctx = crate::tests::fixtures::ctx(
                (app_tx, _app_rx),
                (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
                (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            );
            let current_song = Song {
                id: 0,
                file: "current.mp3".to_owned(),
                duration: Some(Duration::from_secs(123)),
                ..Default::default()
            };
            ctx.queue = vec![
                current_song.clone(),
                Song { id: 1, file: "song1.mp3".to_owned(), duration: Some(Duration::from_secs(123)), ..Default::default() },
            ];
            ctx.status = Status {
                state: State::Play,
                song: Some(0),
                songid: Some(0),
                ..Default::default()
            };
            ctx.cached_queue_time_total = Duration::from_secs(246);
            ctx.chapters.borrow_mut().insert(
                "current.mp3".to_owned(),
                vec![
                    crate::shared::chapters::Chapter { title: "One".into(), start_secs: 0.0, end_secs: 60.0 },
                    crate::shared::chapters::Chapter { title: "Two".into(), start_secs: 60.0, end_secs: 120.0 },
                ],
            );
            let format = Property::<PropertyKind> {
                kind: PropertyKindOrText::Property(PropertyKind::Status(StatusProperty::QueueBoxTitle())),
                style: None,
                default: None,
            };
            let render = |ctx: &Ctx| {
                format
                    .as_span(Some(&current_song), ctx, "", TagResolutionStrategy::All)
                    .map(|s| match s {
                        either::Either::Left(span) => span.content.to_string(),
                        either::Either::Right(_) => String::new(),
                    })
                    .unwrap_or_default()
            };
            // Queue mode: count + total from the queue.
            ctx.queue_tab.set(crate::ctx::QueueTabMode::Audio);
            assert_eq!(render(&ctx), "2 songs / 4:06 total time");
            // Chapters mode: chapter count + sum of chapter durations.
            ctx.queue_tab.set(crate::ctx::QueueTabMode::Chapters);
            assert_eq!(render(&ctx), "2 Chapters / 2:00 total time");
            // Video mode: persistent playlist count + sum of the known
            // durations (survives mpv closing and audio playback).
            ctx.video_playlist = std::cell::RefCell::new(vec![
                crate::core::mpv::MpvPlaylistEntry::new("One", "http://x/1", Some(120.0)),
                crate::core::mpv::MpvPlaylistEntry::new("Two", "http://x/2", Some(180.0)),
            ]);
            ctx.queue_tab.set(crate::ctx::QueueTabMode::Video);
            assert_eq!(render(&ctx), "2 videos / 5:00 total time");
        }

        #[rstest]
        // no current song or if the queue is empty, the queue time should be 0:00
        #[case(StatusProperty::QueueTimeTotal { separator: None }, "0:00")]
        #[case(StatusProperty::QueueTimeRemaining { separator: None }, "0:00")]
        #[case(StatusProperty::QueueTimeTotal { separator: Some(",".to_string()) }, "0s")]
        #[case(StatusProperty::QueueTimeRemaining { separator: Some(",".to_string()) }, "0s")]
        fn queue_time_property_no_current_song(
            mut ctx: Ctx,
            #[case] prop: StatusProperty,
            #[case] expected: &str,
        ) {
            let format = Property::<PropertyKind> {
                kind: PropertyKindOrText::Property(PropertyKind::Status(prop)),
                style: None,
                default: None,
            };

            ctx.queue = vec![];
            ctx.status = Status { state: State::Stop, ..Default::default() };

            let result = format.as_span(None, &ctx, "", TagResolutionStrategy::All);

            assert_eq!(
                result,
                Some(either::Either::<Span<'_>, Vec<Span<'_>>>::Left(Span::raw(expected)))
            );
        }

        #[rstest]
        // Test edge case: songs without duration
        // if somehow the queue contains songs without duration, the queue time should still be 0:00
        #[case(StatusProperty::QueueTimeTotal { separator: None }, "0:00")]
        #[case(StatusProperty::QueueTimeRemaining { separator: None }, "0:00")]
        fn queue_time_property_no_duration(
            mut ctx: Ctx,
            #[case] prop: StatusProperty,
            #[case] expected: &str,
        ) {
            let format = Property::<PropertyKind> {
                kind: PropertyKindOrText::Property(PropertyKind::Status(prop)),
                style: None,
                default: None,
            };

            let song_no_duration = Song {
                id: 0,
                file: "no_duration.mp3".to_owned(),
                duration: None,
                metadata: HashMap::from([("title".to_string(), "No Duration".into())]),
                last_modified: chrono::Utc::now(),
                added: None,
            };

            ctx.queue = vec![song_no_duration.clone()];
            ctx.status = Status { state: State::Play, song: Some(0), ..Default::default() };

            let result =
                format.as_span(Some(&song_no_duration), &ctx, "", TagResolutionStrategy::All);

            assert_eq!(
                result,
                Some(either::Either::<Span<'_>, Vec<Span<'_>>>::Left(Span::raw(expected)))
            );
        }

        #[rstest]
        #[case("otherplay", "otherstopped", "otherpaused", State::Play, "otherplay")]
        #[case("otherplay", "otherstopped", "otherpaused", State::Pause, "otherpaused")]
        #[case("otherplay", "otherstopped", "otherpaused", State::Stop, "otherstopped")]
        fn playback_state_label_is_correct(
            mut ctx: Ctx,
            #[case] playing_label: &'static str,
            #[case] stopped_label: &'static str,
            #[case] paused_label: &'static str,
            #[case] state: State,
            #[case] expected_label: &str,
        ) {
            let format = Property::<PropertyKind> {
                kind: PropertyKindOrText::Property(PropertyKind::Status(StatusProperty::State {
                    playing_label: playing_label.to_string(),
                    paused_label: paused_label.to_string(),
                    stopped_label: stopped_label.to_string(),
                    playing_style: None,
                    paused_style: None,
                    stopped_style: None,
                })),
                style: None,
                default: None,
            };

            let song = Song { id: 1, file: "file".to_owned(), ..Default::default() };
            ctx.status = Status { state, ..Default::default() };

            let result = format.as_span(Some(&song), &ctx, "", TagResolutionStrategy::All);

            assert_eq!(
                result,
                Some(either::Either::<Span<'_>, Vec<Span<'_>>>::Left(Span::raw(expected_label)))
            );
        }

        #[rstest]
        #[case(StatusPropertyFile::ConsumeV2 { on_label: "ye".to_string(), off_label: "naw".to_string(), oneshot_label: "1111".to_string(), on_style: None, off_style: None, oneshot_style: None }, Status { consume: OnOffOneshot::On, ..Default::default() }, "ye")]
        #[case(StatusPropertyFile::ConsumeV2 { on_label: "ye".to_string(), off_label: "naw".to_string(), oneshot_label: "1111".to_string(), on_style: None, off_style: None, oneshot_style: None }, Status { consume: OnOffOneshot::Off, ..Default::default() }, "naw")]
        #[case(StatusPropertyFile::ConsumeV2 { on_label: "ye".to_string(), off_label: "naw".to_string(), oneshot_label: "1111".to_string(), on_style: None, off_style: None, oneshot_style: None }, Status { consume: OnOffOneshot::Oneshot, ..Default::default() }, "1111")]
        #[case(StatusPropertyFile::SingleV2 { on_label: "ye".to_string(), off_label: "naw".to_string(), oneshot_label: "1111".to_string(), on_style: None, off_style: None, oneshot_style: None }, Status { single: OnOffOneshot::On, ..Default::default() }, "ye")]
        #[case(StatusPropertyFile::SingleV2 { on_label: "ye".to_string(), off_label: "naw".to_string(), oneshot_label: "1111".to_string(), on_style: None, off_style: None, oneshot_style: None }, Status { single: OnOffOneshot::Off, ..Default::default() }, "naw")]
        #[case(StatusPropertyFile::SingleV2 { on_label: "ye".to_string(), off_label: "naw".to_string(), oneshot_label: "1111".to_string(), on_style: None, off_style: None, oneshot_style: None }, Status { single: OnOffOneshot::Oneshot, ..Default::default() }, "1111")]
        #[case(StatusPropertyFile::RandomV2 { on_label: "ye".to_string(), off_label: "naw".to_string(), on_style: None, off_style: None }, Status { random: true, ..Default::default() }, "ye")]
        #[case(StatusPropertyFile::RandomV2 { on_label: "ye".to_string(), off_label: "naw".to_string(), on_style: None, off_style: None }, Status { random: false, ..Default::default() }, "naw")]
        #[case(StatusPropertyFile::RepeatV2 { on_label: "ye".to_string(), off_label: "naw".to_string(), on_style: None, off_style: None }, Status { repeat: true, ..Default::default() }, "ye")]
        #[case(StatusPropertyFile::RepeatV2 { on_label: "ye".to_string(), off_label: "naw".to_string(), on_style: None, off_style: None }, Status { repeat: false, ..Default::default() }, "naw")]
        #[case(StatusPropertyFile::Consume, Status { consume: OnOffOneshot::On, ..Default::default() }, "On")]
        #[case(StatusPropertyFile::Consume, Status { consume: OnOffOneshot::Off, ..Default::default() }, "Off")]
        #[case(StatusPropertyFile::Consume, Status { consume: OnOffOneshot::Oneshot, ..Default::default() }, "OS")]
        #[case(StatusPropertyFile::Repeat, Status { repeat: true, ..Default::default() }, "On")]
        #[case(StatusPropertyFile::Repeat, Status { repeat: false, ..Default::default() }, "Off")]
        #[case(StatusPropertyFile::Random, Status { random: true, ..Default::default() }, "On")]
        #[case(StatusPropertyFile::Random, Status { random: false, ..Default::default() }, "Off")]
        #[case(StatusPropertyFile::Single, Status { single: OnOffOneshot::On, ..Default::default() }, "On")]
        #[case(StatusPropertyFile::Single, Status { single: OnOffOneshot::Off, ..Default::default() }, "Off")]
        #[case(StatusPropertyFile::Single, Status { single: OnOffOneshot::Oneshot, ..Default::default() }, "OS")]
        fn on_off_states_label_is_correct(
            mut ctx: Ctx,
            #[case] prop: StatusPropertyFile,
            #[case] status: Status,
            #[case] expected_label: &str,
        ) {
            let format = Property::<PropertyKind> {
                kind: PropertyKindOrText::Property(PropertyKind::Status(prop.try_into().unwrap())),
                style: None,
                default: None,
            };

            let song = Song { id: 1, file: "file".to_owned(), ..Default::default() };

            ctx.status = status;

            let result = format.as_span(Some(&song), &ctx, "", TagResolutionStrategy::All);

            assert_eq!(result, Some(Either::Left(Span::raw(expected_label))));
        }

        #[rstest]
        #[case(StatusPropertyFile::ConsumeV2 { on_style: Some(StyleFile::builder().fg("red".to_string()).build()), off_style: Some(StyleFile::builder().fg("green".to_string()).build()), oneshot_style: Some(StyleFile::builder().fg("blue".to_string()).build()), on_label: String::new(), off_label: String::new(), oneshot_label: String::new() }, Status { consume: OnOffOneshot::On, ..Default::default() }, Some(Style::default().red()))]
        #[case(StatusPropertyFile::SingleV2  { on_style: Some(StyleFile::builder().fg("red".to_string()).build()), off_style: Some(StyleFile::builder().fg("green".to_string()).build()), oneshot_style: Some(StyleFile::builder().fg("blue".to_string()).build()),  on_label: String::new(), off_label: String::new(), oneshot_label: String::new() }, Status { single: OnOffOneshot::On, ..Default::default() }, Some(Style::default().red()))]
        #[case(StatusPropertyFile::RandomV2  { on_style: Some(StyleFile::builder().fg("red".to_string()).build()), off_style: Some(StyleFile::builder().fg("green".to_string()).build()), on_label: String::new(), off_label: String::new() }, Status { random: true, ..Default::default() }, Some(Style::default().red()))]
        #[case(StatusPropertyFile::RepeatV2  { on_style: Some(StyleFile::builder().fg("red".to_string()).build()), off_style: Some(StyleFile::builder().fg("green".to_string()).build()), on_label: String::new(), off_label: String::new() }, Status { repeat: true, ..Default::default() }, Some(Style::default().red()))]
        #[case(StatusPropertyFile::ConsumeV2 { on_style: None, off_style: None, oneshot_style: None, on_label: String::new(), off_label: String::new(), oneshot_label: String::new() }, Status { consume: OnOffOneshot::On, ..Default::default() }, None)]
        #[case(StatusPropertyFile::SingleV2  { on_style: None, off_style: None, oneshot_style: None, on_label: String::new(), off_label: String::new(), oneshot_label: String::new() }, Status { single: OnOffOneshot::On, ..Default::default() }, None)]
        #[case(StatusPropertyFile::RandomV2  { on_style: None, off_style: None, on_label: String::new(), off_label: String::new() }, Status { random: true, ..Default::default() }, None)]
        #[case(StatusPropertyFile::RepeatV2  { on_style: None, off_style: None, on_label: String::new(), off_label: String::new() }, Status { repeat: true, ..Default::default() }, None)]
        fn on_off_oneshot_styles_are_correct(
            mut ctx: Ctx,
            #[case] prop: StatusPropertyFile,
            #[case] status: Status,
            #[case] expected_style: Option<Style>,
        ) {
            let format = Property::<PropertyKind> {
                kind: PropertyKindOrText::Property(PropertyKind::Status(prop.try_into().unwrap())),
                style: None,
                default: None,
            };

            let song = Song { id: 1, file: "file".to_owned(), ..Default::default() };

            ctx.status = status;

            let result = format.as_span(Some(&song), &ctx, "", TagResolutionStrategy::All);

            dbg!(&result);
            assert_eq!(
                result,
                Some(Either::Left(Span::styled(String::new(), expected_style.unwrap_or_default())))
            );
        }
    }

    mod property {
        use super::*;

        #[rstest]
        fn works(ctx: Ctx) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Property(SongProperty::Title),
                style: None,
                default: None,
            };

            let song = Song {
                metadata: HashMap::from([
                    ("artist".to_string(), "artist".into()),
                    ("title".to_string(), "title".into()),
                ]),
                ..Default::default()
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some("title".to_owned()));
        }

        #[rstest]
        fn falls_back(ctx: Ctx) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Property(SongProperty::Track),
                style: None,
                default: Some(
                    Property {
                        kind: PropertyKindOrText::Text("fallback".into()),
                        style: None,
                        default: None,
                    }
                    .into(),
                ),
            };

            let song = Song {
                metadata: HashMap::from([
                    ("artist".to_string(), "artist".into()),
                    ("title".to_string(), "title".into()),
                ]),
                ..Default::default()
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some("fallback".to_owned()));
        }

        #[rstest]
        fn falls_back_to_none(ctx: Ctx) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Property(SongProperty::Track),
                style: None,
                default: None,
            };

            let song = Song {
                metadata: HashMap::from([
                    ("artist".to_string(), "artist".into()),
                    ("title".to_string(), "title".into()),
                ]),
                ..Default::default()
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, None);
        }
    }

    mod text {
        use super::*;

        #[rstest]
        fn works(ctx: Ctx) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Text("test".into()),
                style: None,
                default: None,
            };

            let song = Song {
                metadata: HashMap::from([
                    ("artist".to_string(), "artist".into()),
                    ("title".to_string(), "title".into()),
                ]),
                ..Default::default()
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some("test".to_owned()));
        }

        #[rstest]
        fn fallback_is_ignored(ctx: Ctx) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Text("test".into()),
                style: None,
                default: Some(
                    Property {
                        kind: PropertyKindOrText::Text("fallback".into()),
                        style: None,
                        default: None,
                    }
                    .into(),
                ),
            };

            let song = Song {
                metadata: HashMap::from([
                    ("artist".to_string(), "artist".into()),
                    ("title".to_string(), "title".into()),
                ]),
                ..Default::default()
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some("test".to_owned()));
        }
    }

    mod group {
        use super::*;

        #[rstest]
        fn group_no_fallback(ctx: Ctx) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Group(vec![
                    Property {
                        kind: PropertyKindOrText::Property(SongProperty::Track),
                        style: None,
                        default: None,
                    },
                    Property {
                        kind: PropertyKindOrText::Text(" ".into()),
                        style: None,
                        default: None,
                    },
                ]),
                style: None,
                default: None,
            };

            let song = Song {
                metadata: HashMap::from([
                    ("artist".to_string(), "artist".into()),
                    ("title".to_string(), "title".into()),
                ]),
                ..Default::default()
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, None);
        }

        #[rstest]
        fn group_fallback(ctx: Ctx) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Group(vec![
                    Property {
                        kind: PropertyKindOrText::Property(SongProperty::Track),
                        style: None,
                        default: None,
                    },
                    Property {
                        kind: PropertyKindOrText::Text(" ".into()),
                        style: None,
                        default: None,
                    },
                ]),
                style: None,
                default: Some(
                    Property {
                        kind: PropertyKindOrText::Text("fallback".into()),
                        style: None,
                        default: None,
                    }
                    .into(),
                ),
            };

            let song = Song {
                metadata: HashMap::from([
                    ("artist".to_string(), "artist".into()),
                    ("title".to_string(), "title".into()),
                ]),
                ..Default::default()
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some("fallback".to_owned()));
        }

        #[rstest]
        fn group_resolved(ctx: Ctx) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Group(vec![
                    Property {
                        kind: PropertyKindOrText::Property(SongProperty::Title),
                        style: None,
                        default: None,
                    },
                    Property {
                        kind: PropertyKindOrText::Text("text".into()),
                        style: None,
                        default: None,
                    },
                ]),
                style: None,
                default: Some(
                    Property {
                        kind: PropertyKindOrText::Text("fallback".into()),
                        style: None,
                        default: None,
                    }
                    .into(),
                ),
            };

            let song = Song {
                metadata: HashMap::from([
                    ("artist".to_string(), "artist".into()),
                    ("title".to_string(), "title".into()),
                ]),
                ..Default::default()
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some("titletext".to_owned()));
        }

        #[rstest]
        fn group_fallback_in_group(ctx: Ctx) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Group(vec![
                    Property {
                        kind: PropertyKindOrText::Property(SongProperty::Track),
                        style: None,
                        default: Some(
                            Property {
                                kind: PropertyKindOrText::Text("fallback".into()),
                                style: None,
                                default: None,
                            }
                            .into(),
                        ),
                    },
                    Property {
                        kind: PropertyKindOrText::Text("text".into()),
                        style: None,
                        default: None,
                    },
                ]),
                style: None,
                default: None,
            };

            let song = Song {
                metadata: HashMap::from([
                    ("artist".to_string(), "artist".into()),
                    ("title".to_string(), "title".into()),
                ]),
                ..Default::default()
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some("fallbacktext".to_owned()));
        }

        #[rstest]
        fn group_nesting(ctx: Ctx) {
            let format = Property::<SongProperty> {
                kind: PropertyKindOrText::Group(vec![
                    Property {
                        kind: PropertyKindOrText::Group(vec![
                            Property {
                                kind: PropertyKindOrText::Property(SongProperty::Track),
                                style: None,
                                default: None,
                            },
                            Property {
                                kind: PropertyKindOrText::Text("inner".into()),
                                style: None,
                                default: None,
                            },
                        ]),
                        style: None,
                        default: Some(
                            Property {
                                kind: PropertyKindOrText::Text("innerfallback".into()),
                                style: None,
                                default: None,
                            }
                            .into(),
                        ),
                    },
                    Property {
                        kind: PropertyKindOrText::Text("outer".into()),
                        style: None,
                        default: None,
                    },
                ]),
                style: None,
                default: None,
            };

            let song = Song {
                metadata: HashMap::from([("title".to_string(), "title".into())]),
                ..Default::default()
            };

            let result = format.as_string(Some(&song), "", TagResolutionStrategy::All, &ctx);

            assert_eq!(result, Some("innerfallbackouter".to_owned()));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod layout_visibility_tests {
    use std::sync::Arc;

    use ratatui::{
        layout::{Alignment, Direction, Rect},
        widgets::{Borders, TitlePosition},
    };

    use super::*;
    use crate::{
        config::tabs::{Pane, SizedSubPane},
        config::theme::{PercentOrLength, borders::BorderSymbols},
        shared::id,
        tests::fixtures::ctx,
    };

    fn pane(pane_type: PaneType) -> SizedPaneOrSplit {
        SizedPaneOrSplit::Pane(Pane {
            pane: pane_type,
            background_color: None,
            borders: Borders::NONE,
            border_style: None,
            border_active_style: None,
            border_title: Vec::new(),
            border_title_position: TitlePosition::Top,
            border_title_alignment: Alignment::Left,
            border_symbols: BorderSymbols::Rounded,
            id: id::new(),
        })
    }

    fn sub(size: PercentOrLength, pane: SizedPaneOrSplit) -> SizedSubPane {
        SizedSubPane {
            size,
            collapse_below: None,
            shrink_below: None,
            window_sizes: Vec::new(),
            pane,
        }
    }

    /// The user's Local-tab top split: album art sized by pixel aspect ratio
    /// next to the lyrics box.
    fn art_lyrics_split() -> SizedPaneOrSplit {
        SizedPaneOrSplit::Split {
            background_color: None,
            borders: Borders::NONE,
            border_style: None,
            border_title: Vec::new(),
            border_title_position: TitlePosition::Top,
            border_title_alignment: Alignment::Left,
            border_symbols: BorderSymbols::Rounded,
            direction: Direction::Horizontal,
            panes: vec![
                sub(PercentOrLength::Ratio(2.14), pane(PaneType::AlbumArt)),
                sub(PercentOrLength::Percent(100), pane(PaneType::Lyrics)),
            ],
        }
    }

    fn collect_panes(layout: &SizedPaneOrSplit, ctx: &Ctx) -> Vec<(PaneType, Rect)> {
        collect_panes_at(layout, 10, ctx)
    }

    fn collect_panes_at(
        layout: &SizedPaneOrSplit,
        height: u16,
        ctx: &Ctx,
    ) -> Vec<(PaneType, Rect)> {
        let mut result = Vec::new();
        let area = Rect::new(0, 0, 70, height);
        layout
            .for_each_pane_custom_data(
                area,
                height,
                (),
                &mut |pane, pane_area, _, _, _, ()| {
                    result.push((pane.pane.clone(), pane_area));
                    Ok(())
                },
                &mut |_, _, _, ()| Ok(()),
                ctx,
            )
            .unwrap();
        result
    }

    fn with_ui<F: FnOnce(&mut crate::config::UiSettings)>(ctx: &mut Ctx, f: F) {
        let mut config = ctx.config.as_ref().clone();
        f(&mut config.ui);
        ctx.config = Arc::new(config);
    }

    #[rstest::rstest]
    fn both_visible_share_the_split(ctx: Ctx) {
        let panes = collect_panes(&art_lyrics_split(), &ctx);
        let art = panes.iter().find(|(t, _)| matches!(t, PaneType::AlbumArt)).unwrap();
        let lyrics = panes.iter().find(|(t, _)| matches!(t, PaneType::Lyrics)).unwrap();
        // Art is sized by the pixel aspect ratio against the split height;
        // lyrics take the rest.
        assert_eq!(art.1.width, 21);
        assert!(lyrics.1.width > 0);
        assert_eq!(art.1.width + lyrics.1.width, 70);
    }

    #[rstest::rstest]
    fn hidden_album_art_lets_lyrics_take_the_full_width(mut ctx: Ctx) {
        with_ui(&mut ctx, |ui| ui.show_album_art = false);
        let panes = collect_panes(&art_lyrics_split(), &ctx);
        assert!(!panes.iter().any(|(t, _)| matches!(t, PaneType::AlbumArt)));
        let lyrics = panes.iter().find(|(t, _)| matches!(t, PaneType::Lyrics)).unwrap();
        assert_eq!(lyrics.1.width, 70);
    }

    #[rstest::rstest]
    fn hidden_lyrics_centers_album_art_full_width(mut ctx: Ctx) {
        with_ui(&mut ctx, |ui| ui.show_lyrics = false);
        let panes = collect_panes(&art_lyrics_split(), &ctx);
        assert!(!panes.iter().any(|(t, _)| matches!(t, PaneType::Lyrics)));
        let art = panes.iter().find(|(t, _)| matches!(t, PaneType::AlbumArt)).unwrap();
        assert_eq!(art.1.width, 70);
    }

    #[rstest::rstest]
    fn hidden_art_and_lyrics_collapse_the_split(mut ctx: Ctx) {
        with_ui(&mut ctx, |ui| {
            ui.show_album_art = false;
            ui.show_lyrics = false;
        });
        let panes = collect_panes(&art_lyrics_split(), &ctx);
        assert!(panes.is_empty(), "a split with no visible leaves collapses entirely");
    }

    #[rstest::rstest]
    fn hidden_art_and_lyrics_let_the_queue_fill_the_tab(mut ctx: Ctx) {
        use crate::config::theme::PercentOrLength;
        // The Queue tab's top split (art | lyrics) sits above the queue
        // list; when both are hidden the split must collapse so the queue
        // takes the whole tab height.
        let outer = SizedPaneOrSplit::Split {
            background_color: None,
            borders: Borders::NONE,
            border_style: None,
            border_title: Vec::new(),
            border_title_position: TitlePosition::Top,
            border_title_alignment: Alignment::Left,
            border_symbols: BorderSymbols::Rounded,
            direction: Direction::Vertical,
            panes: vec![
                sub(PercentOrLength::Length(20), art_lyrics_split()),
                sub(PercentOrLength::Percent(100), pane(PaneType::Queue)),
            ],
        };
        with_ui(&mut ctx, |ui| {
            ui.show_album_art = false;
            ui.show_lyrics = false;
        });
        let panes = collect_panes(&outer, &ctx);
        let queue = panes.iter().find(|(t, _)| matches!(t, PaneType::Queue)).unwrap();
        assert_eq!(queue.1.height, 10, "the queue list fills the freed space");
    }

    #[rstest::rstest]
    fn hidden_cava_is_skipped_from_the_theme_layout(mut ctx: Ctx) {
        // Cava lives in the theme layout; hiding it must skip the pane.
        let layout = ctx.config.theme.layout.clone();
        with_ui(&mut ctx, |ui| ui.show_cava = false);
        let panes = collect_panes(&layout, &ctx);
        assert!(!panes.iter().any(|(t, _)| matches!(t, PaneType::Cava)));
    }

    /// The queue tab's art+lyrics split (the only `window_sizes` user)
    /// collapses entirely — box, borders and all — when it can no longer
    /// fit its content minimum (4 rows + the leaves' border rows), so the
    /// queue takes the freed space instead of an empty bordered shell.
    #[rstest::rstest]
    fn art_lyrics_split_collapses_below_its_content_minimum(ctx: Ctx) {
        let layout = ctx
            .config
            .tabs
            .tabs
            .iter()
            .find(|(name, _)| name.as_str() == "Queue")
            .unwrap()
            .1
            .panes
            .clone();
        // 44 rows: the split resolves to 10 rows (8 content) — shown.
        let panes = collect_panes_at(&layout, 44, &ctx);
        let lyrics = panes.iter().find(|(t, _)| matches!(t, PaneType::Lyrics)).unwrap();
        assert_eq!(lyrics.1.height, 8);
        // 39 rows: exactly the minimum (4 content + 2 border rows) — shown.
        let panes = collect_panes_at(&layout, 39, &ctx);
        let lyrics = panes.iter().find(|(t, _)| matches!(t, PaneType::Lyrics)).unwrap();
        assert_eq!(lyrics.1.height, 4);
        // 34-38 rows: the split would be 1-5 rows — the whole box collapses
        // and the queue list sits right at the top (the 3-row header split,
        // nothing above it).
        for height in [38u16, 36, 34] {
            let panes = collect_panes_at(&layout, height, &ctx);
            assert!(
                panes.iter().all(
                    |(t, area)| !matches!(t, PaneType::Lyrics | PaneType::AlbumArt) || area.height == 0
                ),
                "art/lyrics must collapse at {height} rows"
            );
            let queue = panes.iter().find(|(t, _)| matches!(t, PaneType::Queue)).unwrap();
            assert_eq!(queue.1.y, 3, "no art/lyrics rows above the queue at {height}");
        }
    }

    /// The content-minimum collapse is scoped to responsive (`window_sizes`)
    /// sub-panes: a fixed small pane (e.g. the 3-row progress-bar row) keeps
    /// its exact size.
    #[rstest::rstest]
    fn fixed_size_panes_keep_their_height_below_the_minimum(ctx: Ctx) {
        let outer = SizedPaneOrSplit::Split {
            background_color: None,
            borders: Borders::NONE,
            border_style: None,
            border_title: Vec::new(),
            border_title_position: TitlePosition::Top,
            border_title_alignment: Alignment::Left,
            border_symbols: BorderSymbols::Rounded,
            direction: Direction::Vertical,
            panes: vec![sub(PercentOrLength::Length(3), pane(PaneType::Queue))],
        };
        let panes = collect_panes_at(&outer, 10, &ctx);
        assert_eq!(panes[0].1.height, 3, "fixed-size panes keep their height");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod hovered_item_tests {
    use super::hovered_item;
    use ratatui::layout::{Position, Rect};

    #[test]
    fn maps_mouse_row_to_item_with_offset() {
        let inner = Rect::new(10, 5, 20, 10);
        let mouse = Some(Position { x: 12, y: 7 });
        // offset 3, rows are 1 tall: y 7 -> row 2 -> item 3 + 2 = 5.
        assert_eq!(hovered_item(mouse, inner, 3, 20, 1), Some(5));
    }

    #[test]
    fn two_line_items_map_both_rows() {
        let inner = Rect::new(0, 0, 40, 20);
        let mouse = Some(Position { x: 5, y: 3 });
        // item rows are 2 tall: y 3 -> item 1 (rows 2-3).
        assert_eq!(hovered_item(mouse, inner, 0, 20, 2), Some(1));
        let mouse = Some(Position { x: 5, y: 2 });
        assert_eq!(hovered_item(mouse, inner, 0, 20, 2), Some(1));
    }

    #[test]
    fn no_hover_outside_list_or_past_end() {
        let inner = Rect::new(10, 5, 20, 10);
        // Left of the list.
        assert_eq!(hovered_item(Some(Position { x: 5, y: 6 }), inner, 0, 20, 1), None);
        // Below the list.
        assert_eq!(hovered_item(Some(Position { x: 15, y: 16 }), inner, 0, 20, 1), None);
        // Past the last item.
        assert_eq!(hovered_item(Some(Position { x: 15, y: 14 }), inner, 0, 3, 1), None);
        // No mouse at all.
        assert_eq!(hovered_item(None, inner, 0, 20, 1), None);
    }
}
