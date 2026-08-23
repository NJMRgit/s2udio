use std::{
    borrow::Cow, collections::{HashMap, VecDeque},
    time::Duration,
};
use album_art::AlbumArtPane;
use albums::AlbumsPane;
use anyhow::{Context, Result};
use cava::CavaPane;
use controls::ControlsPane;
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
    Frame, layout::{Constraint, Layout, Position},
    prelude::Rect, style::Color, text::{Line, Span},
    widgets::{Block, Borders},
};
use search::SearchPane;
use strum::{Display, IntoDiscriminant};
use tabs::TabsPane;
use tag_browser::TagBrowserPane;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use volume::VolumePane;
#[cfg(debug_assertions)]
use self::{frame_count::FrameCountPane, logs::LogsPane};
use super::{UiEvent, widgets::{scan_status::ScanStatus, volume::Volume}};
use crate::{
    MpdQueryResult,
    config::{
        tabs::{Pane as ConfigPane, PaneType, SizedPaneOrSplit, SizedSubPane},
        theme::{
            PercentOrLength, SymbolsConfig, TagResolutionStrategy,
            properties::{
                Property, PropertyKind, PropertyKindOrText, SongProperty, StatusProperty,
                Transform, WidgetProperty,
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
        keys::ActionEvent, mouse_event::MouseEvent,
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
pub mod jellyfin;
#[cfg(debug_assertions)]
pub mod logs;
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
            album_artists: TagBrowserPane::new(
                Tag::AlbumArtist,
                PaneType::AlbumArtists,
                None,
                ctx,
            ),
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
                PaneType::Browser { root_tag, separator } => {
                    Some((
                        pane.pane.clone(),
                        Box::new(
                            TagBrowserPane::new(
                                Tag::Custom(root_tag.clone()),
                                pane.pane.clone(),
                                separator.clone(),
                                ctx,
                            ),
                        ) as Box<dyn BoxedPane>,
                    ))
                }
                PaneType::Volume { kind } => {
                    Some((
                        pane.pane.clone(),
                        Box::new(VolumePane::new(kind.clone())) as Box<dyn BoxedPane>,
                    ))
                }
                PaneType::Controls => {
                    Some((
                        pane.pane.clone(),
                        Box::new(ControlsPane::new()) as Box<dyn BoxedPane>,
                    ))
                }
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
            PaneType::Property { content, align, scroll_speed } => {
                Ok(
                    Panes::Property(
                        PropertyPane::<
                            'pane_type_ref,
                        >::new(content, *align, (*scroll_speed).into(), ctx),
                    ),
                )
            }
            p @ PaneType::Volume { .. } => {
                Ok(
                    Panes::Others(
                        self
                            .others
                            .get_mut(pane)
                            .with_context(|| {
                                format!("expected pane to be defined {p:?}")
                            })?,
                    ),
                )
            }
            p @ PaneType::Controls => {
                Ok(
                    Panes::Others(
                        self
                            .others
                            .get_mut(pane)
                            .with_context(|| {
                                format!("expected pane to be defined {p:?}")
                            })?,
                    ),
                )
            }
            p @ PaneType::Browser { .. } => {
                Ok(
                    Panes::Others(
                        self
                            .others
                            .get_mut(pane)
                            .with_context(|| {
                                format!("expected pane to be defined {p:?}")
                            })?,
                    ),
                )
            }
            PaneType::Cava => Ok(Panes::Cava(&mut self.cava)),
            PaneType::Empty => Ok(Panes::Empty(&mut self.empty)),
        }
    }
}
macro_rules! pane_call {
    ($screen:ident, $fn:ident ($($param:expr),+)) => {
        match & mut $screen { Panes::Queue(s) => s.$fn ($($param),+),
        Panes::QueueHeader(s) => s.$fn ($($param),+), #[cfg(debug_assertions)]
        Panes::Logs(s) => s.$fn ($($param),+), Panes::Directories(s) => s.$fn
        ($($param),+), Panes::Artists(s) => s.$fn ($($param),+), Panes::AlbumArtists(s)
        => s.$fn ($($param),+), Panes::Albums(s) => s.$fn ($($param),+),
        Panes::Playlists(s) => s.$fn ($($param),+), Panes::Search(s) => s.$fn
        ($($param),+), Panes::Radio(s) => s.$fn ($($param),+), Panes::Jellyfin(s) => s
        .$fn ($($param),+), Panes::AlbumArt(s) => s.$fn ($($param),+), Panes::Lyrics(s)
        => s.$fn ($($param),+), Panes::ProgressBar(s) => s.$fn ($($param),+),
        Panes::Header(s) => s.$fn ($($param),+), Panes::Tabs(s) => s.$fn ($($param),+),
        Panes::TabContent => Ok(()), #[cfg(debug_assertions)] Panes::FrameCount(s) => s
        .$fn ($($param),+), Panes::Property(s) => s.$fn ($($param),+), Panes::Others(s)
        => s.$fn ($($param),+), Panes::Cava(s) => s.$fn ($($param),+), Panes::Empty(s) =>
        s.$fn ($($param),+), }
    };
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
    fn on_event(
        &mut self,
        event: &mut UiEvent,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        Ok(())
    }
    fn handle_insert_mode(
        &mut self,
        kind: InputResultEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
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
    use ratatui::{style::Style, text::{Line, Span}};
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
            let mut info_group = PreviewGroup::new(
                Some(" --- [Info]"),
                Some(group_style),
            );
            let file = Line::from(
                vec![
                    start_of_line_spacer.clone(), Span::styled("File", key_style),
                    separator.clone(), Span::from(self.file.clone()),
                ],
            );
            info_group.push(file.into());
            if let Some(file_name) = self.file_name() {
                info_group
                    .push(
                        Line::from(
                                vec![
                                    start_of_line_spacer.clone(), Span::styled("Filename",
                                    key_style), separator.clone(), Span::from(file_name
                                    .into_owned()),
                                ],
                            )
                            .into(),
                    );
            }
            if let Some(title) = self.metadata.get("title") {
                title
                    .for_each(|item| {
                        info_group
                            .push(
                                Line::from(
                                        vec![
                                            start_of_line_spacer.clone(), Span::styled("Title",
                                            key_style), separator.clone(), Span::from(item.to_owned()),
                                        ],
                                    )
                                    .into(),
                            );
                    });
            }
            if let Some(artist) = self.metadata.get("artist") {
                artist
                    .for_each(|item| {
                        info_group
                            .push(
                                Line::from(
                                        vec![
                                            start_of_line_spacer.clone(), Span::styled("Artist",
                                            key_style), separator.clone(), Span::from(item.to_owned()),
                                        ],
                                    )
                                    .into(),
                            );
                    });
            }
            if let Some(album) = self.metadata.get("album") {
                album
                    .for_each(|item| {
                        info_group
                            .push(
                                Line::from(
                                        vec![
                                            start_of_line_spacer.clone(), Span::styled("Album",
                                            key_style), separator.clone(), Span::from(item.to_owned()),
                                        ],
                                    )
                                    .into(),
                            );
                    });
            }
            if let Some(duration) = &self.duration {
                info_group
                    .push(
                        Line::from(
                                vec![
                                    start_of_line_spacer.clone(), Span::styled("Duration",
                                    key_style), separator.clone(), Span::from(ctx.config
                                    .duration_format.format(duration.as_secs())),
                                ],
                            )
                            .into(),
                    );
            }
            info_group
                .push(
                    Line::from(
                            vec![
                                start_of_line_spacer.clone(), Span::styled("Last Modified",
                                key_style), separator.clone(), Span::from(self.last_modified
                                .to_string()),
                            ],
                        )
                        .into(),
                );
            if let Some(added) = &self.added {
                info_group
                    .push(
                        Line::from(
                                vec![
                                    start_of_line_spacer.clone(), Span::styled("Added",
                                    key_style), separator.clone(), Span::from(added
                                    .to_string()),
                                ],
                            )
                            .into(),
                    );
            }
            let mut tags_group = PreviewGroup::new(
                Some(" --- [Tags]"),
                Some(group_style),
            );
            for (k, v) in self
                .metadata
                .iter()
                .filter(|(key, _)| {
                    !["title", "album", "artist", "duration"].contains(&(*key).as_str())
                })
                .sorted_by_key(|(key, _)| *key)
            {
                v.for_each(|item| {
                    tags_group
                        .push(
                            Line::from(
                                    vec![
                                        start_of_line_spacer.clone(), Span::styled(k.clone(),
                                        key_style), separator.clone(), Span::from(item.to_owned()),
                                    ],
                                )
                                .into(),
                        );
                });
            }
            let mut result = Vec::new();
            if let Some(yt) = ctx.yt_info.borrow().get(&self.file) {
                let mut yt_group = PreviewGroup::new(
                    Some(" --- [YouTube]"),
                    Some(group_style),
                );
                if !yt.title.is_empty() {
                    yt_group
                        .push(
                            Line::from(
                                    vec![
                                        start_of_line_spacer.clone(), Span::styled("Title",
                                        key_style), separator.clone(), Span::from(yt.title.clone()),
                                    ],
                                )
                                .into(),
                        );
                }
                if let Some(description) = &yt.description {
                    for (idx, line) in description.lines().take(15).enumerate() {
                        let label = if idx == 0 {
                            Span::styled("Description", key_style)
                        } else {
                            Span::raw(" ")
                        };
                        let mut row_spans = vec![
                            start_of_line_spacer.clone(), label, separator.clone()
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
            if let Some(stickers) = stickers && !stickers.is_empty() {
                let mut stickers_group = PreviewGroup::new(
                    Some(" --- [Stickers]"),
                    Some(group_style),
                );
                for (k, v) in stickers.iter().sorted_by_key(|(key, _)| *key) {
                    stickers_group
                        .push(
                            Line::from(
                                    vec![
                                        start_of_line_spacer.clone(), Span::styled(k.clone(),
                                        key_style), separator.clone(), Span::from(v.to_owned()),
                                    ],
                                )
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
    pub fn file_name(&self) -> Option<Cow<'_, str>> {
        std::path::Path::new(&self.file)
            .file_stem()
            .map(|file_name| file_name.to_string_lossy())
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
            SongProperty::Disc => {
                self.metadata.get("disc").map(|v| Cow::Borrowed(v.last()))
            }
            SongProperty::Position => {
                self.metadata
                    .get("pos")
                    .map(|v| {
                        v.last()
                            .parse::<usize>()
                            .map(|v| Cow::Owned((v + 1).to_string()))
                            .unwrap_or_default()
                    })
            }
            SongProperty::Track => {
                self.metadata
                    .get("track")
                    .map(|v| {
                        Cow::Owned(
                            v
                                .last()
                                .parse::<u32>()
                                .map_or_else(
                                    |_| v.last().to_owned(),
                                    |v| format!("{v:0>2}"),
                                ),
                        )
                    })
            }
            SongProperty::SampleRate() => {
                self.samplerate().map(|v| Cow::Owned(v.to_string()))
            }
            SongProperty::Bits() => self.bits().map(|v| Cow::Owned(v.to_string())),
            SongProperty::Channels() => {
                self.channels().map(|v| Cow::Owned(v.to_string()))
            }
            SongProperty::Added() => self.added.map(|d| Cow::Owned(d.to_string())),
            SongProperty::LastModified() => {
                Some(Cow::Owned(self.last_modified.to_string()))
            }
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
                PropertyKindOrText::Sticker(key) => {
                    ctx.song_stickers(&self.file)
                        .and_then(|s| s.get(key))
                        .map(|value| {
                            value.to_lowercase().contains(&filter.to_lowercase())
                        })
                        .or_else(|| {
                            format
                                .default
                                .as_ref()
                                .map(|f| {
                                    self.matches(std::iter::once(f.as_ref()), filter, ctx)
                                })
                        })
                }
                PropertyKindOrText::Property(property) => {
                    self.format(property, "", TagResolutionStrategy::All)
                        .map_or_else(
                            || {
                                format
                                    .default
                                    .as_ref()
                                    .map(|f| {
                                        self.matches(std::iter::once(f.as_ref()), filter, ctx)
                                    })
                            },
                            |p| Some(p.to_lowercase().contains(&filter.to_lowercase())),
                        )
                }
                PropertyKindOrText::Group(_) => {
                    format
                        .as_string(Some(self), "", TagResolutionStrategy::All, ctx)
                        .map(|v| v.to_lowercase().contains(&filter.to_lowercase()))
                }
                PropertyKindOrText::Transform(Transform::Truncate { .. }) => {
                    format
                        .as_string(Some(self), "", TagResolutionStrategy::All, ctx)
                        .map(|v| v.to_lowercase().contains(&filter.to_lowercase()))
                }
                PropertyKindOrText::Transform(Transform::Replace { .. }) => {
                    format
                        .as_string(Some(self), "", TagResolutionStrategy::All, ctx)
                        .map(|v| v.to_lowercase().contains(&filter.to_lowercase()))
                }
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
        format
            .default
            .as_ref()
            .and_then(|f| self.as_line(f.as_ref(), tag_separator, strategy, ctx))
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
            PropertyKindOrText::Sticker(key) => {
                ctx.song_stickers(&self.file)
                    .and_then(|s| s.get(key))
                    .map(|sticker| Line::styled(sticker, style))
                    .or_else(|| {
                        format
                            .default
                            .as_ref()
                            .and_then(|format| {
                                self.as_line(format.as_ref(), tag_separator, strategy, ctx)
                            })
                    })
            }
            PropertyKindOrText::Property(property) => {
                self.format(property, tag_separator, strategy)
                    .map_or_else(
                        || self.default_as_line(format, tag_separator, strategy, ctx),
                        |v| Some(Line::styled(v, style)),
                    )
            }
            PropertyKindOrText::Group(group) => {
                let mut buf = Line::default().style(style);
                for grformat in group {
                    if let Some(res) = self
                        .as_line(grformat, tag_separator, strategy, ctx)
                    {
                        for span in res.spans {
                            let span_style = span.style;
                            buf.push_span(span.style(res.style).patch_style(span_style));
                        }
                    } else {
                        return format
                            .default
                            .as_ref()
                            .and_then(|format| {
                                self.as_line(format, tag_separator, strategy, ctx)
                            });
                    }
                }
                return Some(buf);
            }
            PropertyKindOrText::Transform(
                Transform::Replace { content, replacements },
            ) => {
                self.as_line(content, tag_separator, strategy, ctx)
                    .and_then(|line| {
                        let mut content = String::new();
                        for span in &line.spans {
                            content.push_str(span.content.as_ref());
                        }
                        if let Some(replacement) = replacements.get(&content) {
                            return self
                                .as_line(replacement, tag_separator, strategy, ctx)
                                .or_else(|| {
                                    replacement
                                        .default
                                        .as_ref()
                                        .and_then(|format| {
                                            self.as_line(format, tag_separator, strategy, ctx)
                                        })
                                });
                        }
                        Some(line)
                    })
                    .or_else(|| {
                        format
                            .default
                            .as_ref()
                            .and_then(|format| {
                                self.as_line(format, tag_separator, strategy, ctx)
                            })
                    })
            }
            PropertyKindOrText::Transform(
                Transform::Truncate { content, length, from_start },
            ) => {
                self.as_line(content, tag_separator, strategy, ctx)
                    .map(|mut line| {
                        let mut buf = VecDeque::new();
                        let mut remaining_len = *length;
                        let push_fn = if *from_start {
                            VecDeque::push_front
                        } else {
                            VecDeque::push_back
                        };
                        let truncate_fn = if *from_start {
                            Span::truncate_start
                        } else {
                            Span::truncate_end
                        };
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
                            .and_then(|format| {
                                self.as_line(format, tag_separator, strategy, ctx)
                            })
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
        self.default
            .as_ref()
            .and_then(|p| p.as_string(song, tag_separator, strategy, ctx))
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
            PropertyKindOrText::Sticker(key) => {
                song.and_then(|s| ctx.song_stickers(&s.file))
                    .and_then(|s| s.get(key))
                    .cloned()
                    .or_else(|| self.default(song, tag_separator, strategy, ctx))
            }
            PropertyKindOrText::Property(property) => {
                if let Some(song) = song {
                    song.format(property, tag_separator, strategy)
                        .map_or_else(
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
                    if let Some(res) = format
                        .as_string(song, tag_separator, strategy, ctx)
                    {
                        buf.push_str(&res);
                    } else {
                        return self
                            .default
                            .as_ref()
                            .and_then(|d| {
                                d.as_string(song, tag_separator, strategy, ctx)
                            });
                    }
                }
                return Some(buf);
            }
            PropertyKindOrText::Transform(
                Transform::Replace { content, replacements },
            ) => {
                content
                    .as_string(song, tag_separator, strategy, ctx)
                    .and_then(|result| {
                        if let Some(replacement) = replacements.get(&result) {
                            return replacement
                                .as_string(song, tag_separator, strategy, ctx)
                                .or_else(|| {
                                    replacement
                                        .default
                                        .as_ref()
                                        .and_then(|d| {
                                            d.as_string(song, tag_separator, strategy, ctx)
                                        })
                                });
                        }
                        Some(result)
                    })
                    .or_else(|| {
                        self.default
                            .as_ref()
                            .and_then(|d| {
                                d.as_string(song, tag_separator, strategy, ctx)
                            })
                    })
            }
            PropertyKindOrText::Transform(
                Transform::Truncate { content, length, from_start },
            ) => {
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
                            .and_then(|d| {
                                d.as_string(song, tag_separator, strategy, ctx)
                            })
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
            PropertyKindOrText::Text(value) => {
                Some(Either::Left(Span::styled(value, style)))
            }
            PropertyKindOrText::Sticker(key) => {
                if let Some(sticker) = song
                    .and_then(|s| ctx.song_stickers(&s.file))
                    .and_then(|s| s.get(key))
                {
                    Some(Either::Left(Span::styled(sticker, style)))
                } else {
                    self.default_as_span(song, ctx, tag_separator, strategy)
                }
            }
            PropertyKindOrText::Property(PropertyKind::Song(property)) => {
                if let Some(song) = song {
                    song.format(property, tag_separator, strategy)
                        .map_or_else(
                            || {
                                self
                                    .default_as_span(Some(song), ctx, tag_separator, strategy)
                            },
                            |s| Some(Either::Left(Span::styled(s, style))),
                        )
                } else {
                    self.default_as_span(song, ctx, tag_separator, strategy)
                }
            }
            PropertyKindOrText::Property(PropertyKind::Status(s)) => {
                match s {
                    StatusProperty::State {
                        playing_label,
                        paused_label,
                        stopped_label,
                        playing_style,
                        paused_style,
                        stopped_style,
                    } => {
                        Some(
                            Either::Left(
                                Span::styled(
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
                                ),
                            ),
                        )
                    }
                    StatusProperty::Duration => {
                        Some(
                            Either::Left(
                                Span::styled(status.duration.to_string(), style),
                            ),
                        )
                    }
                    StatusProperty::Elapsed => {
                        Some(
                            Either::Left(Span::styled(status.elapsed.to_string(), style)),
                        )
                    }
                    StatusProperty::Partition => {
                        Some(Either::Left(Span::styled(&status.partition, style)))
                    }
                    StatusProperty::Volume => {
                        Some(
                            Either::Left(
                                Span::styled(status.volume.value().to_string(), style),
                            ),
                        )
                    }
                    StatusProperty::Repeat {
                        on_label,
                        off_label,
                        on_style,
                        off_style,
                    } => {
                        Some(
                            Either::Left(
                                Span::styled(
                                    if status.repeat { on_label } else { off_label },
                                    if status.repeat { on_style } else { off_style }
                                        .unwrap_or(style),
                                ),
                            ),
                        )
                    }
                    StatusProperty::Random {
                        on_label,
                        off_label,
                        on_style,
                        off_style,
                    } => {
                        Some(
                            Either::Left(
                                Span::styled(
                                    if status.random { on_label } else { off_label },
                                    if status.random { on_style } else { off_style }
                                        .unwrap_or(style),
                                ),
                            ),
                        )
                    }
                    StatusProperty::Consume {
                        on_label,
                        off_label,
                        oneshot_label,
                        on_style,
                        off_style,
                        oneshot_style,
                    } => {
                        Some(
                            Either::Left(
                                Span::styled(
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
                                ),
                            ),
                        )
                    }
                    StatusProperty::Single {
                        on_label,
                        off_label,
                        oneshot_label,
                        on_style,
                        off_style,
                        oneshot_style,
                    } => {
                        Some(
                            Either::Left(
                                Span::styled(
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
                                ),
                            ),
                        )
                    }
                    StatusProperty::Bitrate => {
                        status
                            .bitrate
                            .as_ref()
                            .map_or_else(
                                || self.default_as_span(song, ctx, tag_separator, strategy),
                                |v| Some(Either::Left(Span::styled(v.to_string(), style))),
                            )
                    }
                    StatusProperty::Crossfade => {
                        status
                            .xfade
                            .as_ref()
                            .map_or_else(
                                || self.default_as_span(song, ctx, tag_separator, strategy),
                                |v| Some(Either::Left(Span::styled(v.to_string(), style))),
                            )
                    }
                    StatusProperty::QueueLength { thousands_separator } => {
                        Some(
                            Either::Left(
                                Span::styled(
                                    ctx
                                        .queue
                                        .len()
                                        .with_thousands_separator(thousands_separator),
                                    style,
                                ),
                            ),
                        )
                    }
                    StatusProperty::QueueTimeTotal { separator } => {
                        let formatted = match separator {
                            Some(sep) => {
                                ctx.cached_queue_time_total.format_to_duration(sep)
                            }
                            None => ctx.cached_queue_time_total.to_string(),
                        };
                        Some(Either::Left(Span::styled(formatted, style)))
                    }
                    StatusProperty::QueueTimeRemaining { separator } => {
                        let remaining_time = ctx
                            .find_current_song_in_queue()
                            .map_or(
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
                        let chapters = ctx.current_playback_chapters();
                        let chapters_on = ctx.queue_tab.get()
                            == crate::ctx::QueueTabMode::Chapters
                            && !chapters.is_empty();
                        let video_on = ctx.queue_tab.get()
                            == crate::ctx::QueueTabMode::Video;
                        let (count, total) = if chapters_on {
                            let secs: f64 = chapters.iter().map(|c| c.duration()).sum();
                            (chapters.len(), Duration::from_secs_f64(secs.max(0.0)))
                        } else if video_on {
                            let jellyfin = crate::core::mpv::session_playlist_shown(ctx);
                            let entries: std::cell::Ref<
                                '_,
                                Vec<crate::core::mpv::MpvPlaylistEntry>,
                            > = if jellyfin {
                                ctx.mpv.playlist.borrow()
                            } else {
                                ctx.video_playlist.borrow()
                            };
                            let secs: f64 = entries
                                .iter()
                                .filter_map(|e| e.duration)
                                .sum();
                            (entries.len(), Duration::from_secs_f64(secs))
                        } else {
                            (ctx.queue.len(), ctx.cached_queue_time_total)
                        };
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
                        Some(
                            Either::Left(Span::styled(ctx.active_tab.0.as_ref(), style)),
                        )
                    }
                    StatusProperty::InputBuffer() => {
                        Some(
                            Either::Left(
                                Span::styled(ctx.key_resolver.buffer_to_string(), style),
                            ),
                        )
                    }
                    StatusProperty::InputMode() => {
                        Some(
                            Either::Left(
                                Span::styled(
                                    ctx.input.mode().discriminant().to_string(),
                                    style,
                                ),
                            ),
                        )
                    }
                    StatusProperty::SampleRate() => {
                        status
                            .samplerate()
                            .map(|v| Either::Left(Span::styled(v.to_string(), style)))
                    }
                    StatusProperty::Bits() => {
                        status
                            .bits()
                            .map(|v| Either::Left(Span::styled(v.to_string(), style)))
                    }
                    StatusProperty::Channels() => {
                        status
                            .channels()
                            .map(|v| Either::Left(Span::styled(v.to_string(), style)))
                    }
                }
            }
            PropertyKindOrText::Property(PropertyKind::Widget(w)) => {
                match w {
                    WidgetProperty::Volume => {
                        Some(
                            Either::Left(
                                Span::styled(Volume::get_str(*status.volume.value()), style),
                            ),
                        )
                    }
                    WidgetProperty::States { active_style, separator_style } => {
                        let separator = Span::styled(" / ", *separator_style);
                        Some(
                            Either::Right(
                                vec![
                                    Span::styled("Repeat", if status.repeat { * active_style }
                                    else { style }), separator.clone(), Span::styled("Random",
                                    if status.random { * active_style } else { style }),
                                    separator.clone(), match status.consume { OnOffOneshot::On
                                    => Span::styled("Consume", * active_style),
                                    OnOffOneshot::Off => Span::styled("Consume", style),
                                    OnOffOneshot::Oneshot => Span::styled("Oneshot(C)", *
                                    active_style), }, separator, match status.single {
                                    OnOffOneshot::On => Span::styled("Single", * active_style),
                                    OnOffOneshot::Off => Span::styled("Single", style),
                                    OnOffOneshot::Oneshot => Span::styled("Oneshot(S)", *
                                    active_style), },
                                ],
                            ),
                        )
                    }
                    WidgetProperty::ScanStatus => {
                        ctx.db_update_start
                            .map(|update_start| {
                                Either::Left(
                                    Span::styled(
                                        ScanStatus::new(Some(update_start))
                                            .get_str()
                                            .unwrap_or_default()
                                            .to_string(),
                                        style,
                                    ),
                                )
                            })
                            .or_else(|| {
                                self.default_as_span(song, ctx, tag_separator, strategy)
                            })
                    }
                }
            }
            PropertyKindOrText::Group(group) => {
                let mut buf = Vec::new();
                for format in group {
                    match format.as_span(song, ctx, tag_separator, strategy) {
                        Some(Either::Left(span)) => buf.push(span),
                        Some(Either::Right(spans)) => buf.extend(spans),
                        None => {
                            return self
                                .default_as_span(song, ctx, tag_separator, strategy);
                        }
                    }
                }
                return Some(Either::Right(buf));
            }
            PropertyKindOrText::Transform(
                Transform::Replace { content, replacements },
            ) => {
                match content.as_span(song, ctx, tag_separator, strategy) {
                    Some(Either::Left(span)) => {
                        if let Some(replacement) = replacements
                            .get(span.content.as_ref())
                        {
                            return replacement
                                .as_span(song, ctx, tag_separator, strategy)
                                .or_else(|| {
                                    replacement
                                        .default_as_span(song, ctx, tag_separator, strategy)
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
                                    replacement
                                        .default_as_span(song, ctx, tag_separator, strategy)
                                });
                        }
                        Some(Either::Right(spans))
                    }
                    None => self.default_as_span(song, ctx, tag_separator, strategy),
                }
            }
            PropertyKindOrText::Transform(
                Transform::Truncate { content, length, from_start },
            ) => {
                let truncate_fn = if *from_start {
                    Span::truncate_start
                } else {
                    Span::truncate_end
                };
                match content.as_span(song, ctx, tag_separator, strategy) {
                    Some(Either::Left(mut span)) => {
                        truncate_fn(&mut span, *length);
                        Some(Either::Left(span))
                    }
                    Some(Either::Right(mut spans)) => {
                        let mut buf = VecDeque::new();
                        let mut remaining_len = *length;
                        let push_fn = if *from_start {
                            VecDeque::push_front
                        } else {
                            VecDeque::push_back
                        };
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
        pane_callback: &mut impl FnMut(
            &ConfigPane,
            Rect,
            Block,
            Rect,
            Option<Color>,
        ) -> Result<()>,
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
    fn window_size_at(
        root_height: u16,
        points: &[(u16, PercentOrLength)],
    ) -> PercentOrLength {
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
                let (PercentOrLength::Length(l1), PercentOrLength::Length(l2)) = (s1, s2)
                else {
                    return s1;
                };
                let span = f64::from(h1 - h2);
                let frac = if span == 0.0 {
                    0.0
                } else {
                    f64::from(h1 - root_height) / span
                };
                let len = (f64::from(l1) - (f64::from(l1) - f64::from(l2)) * frac)
                    .round();
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
        split_callback: &mut impl FnMut(
            Block,
            Rect,
            Option<Color>,
            &mut T,
        ) -> Result<()>,
        ctx: &Ctx,
    ) -> Result<()> {
        let mut stack = vec![(self, area)];
        let song = ctx.find_current_song_in_queue().map(|(_, song)| song);
        while let Some((configured_panes, area)) = stack.pop() {
            match configured_panes {
                SizedPaneOrSplit::Pane(pane) => {
                    if ctx.is_pane_hidden(&pane.pane) {
                        continue;
                    }
                    let mut block = Block::default()
                        .borders(pane.borders)
                        .border_set((&pane.border_symbols).into());
                    let bg_color = pane.background_color;
                    if pane.border_title.is_empty() {
                        let pane_area = block.inner(area);
                        pane_callback(
                            pane,
                            pane_area,
                            block,
                            area,
                            bg_color,
                            &mut custom_data,
                        )?;
                    } else {
                        let templs = PropertyTemplates::new(&pane.border_title);
                        let title = templs.format(song, ctx, &ctx.config);
                        block = block
                            .title(title)
                            .title_position(pane.border_title_position)
                            .title_alignment(pane.border_title_alignment);
                        let pane_area = block.inner(area);
                        pane_callback(
                            pane,
                            pane_area,
                            block,
                            area,
                            bg_color,
                            &mut custom_data,
                        )?;
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
                    let visible: Vec<&SizedSubPane> = panes
                        .iter()
                        .filter(|sub_pane| {
                            !Self::is_sub_pane_hidden(&sub_pane.pane, ctx)
                        })
                        .collect();
                    if visible.is_empty() {
                        continue;
                    }
                    let removed_any = panes.len() > visible.len();
                    let album_art_full_width = removed_any && visible.len() == 1
                        && matches!(
                            & visible[0].pane, SizedPaneOrSplit::Pane(p) if matches!(p
                            .pane, PaneType::AlbumArt)
                        );
                    let constraints: Vec<Constraint> = if album_art_full_width {
                        vec![Constraint::Percentage(100)]
                    } else {
                        visible
                            .iter()
                            .map(|pane| {
                                if let Some(min) = pane.collapse_below && split_size < min {
                                    return Constraint::Length(0);
                                }
                                let size = if pane.window_sizes.is_empty() {
                                    pane.size
                                } else {
                                    Self::window_size_at(root_height, &pane.window_sizes)
                                };
                                match (pane.shrink_below, size) {
                                    (
                                        Some(min),
                                        PercentOrLength::Length(s),
                                    ) if split_size < min => Constraint::Length(s / 2),
                                    _ => {
                                        let constraint = size.into_constraint(parent_other_size);
                                        if !pane.window_sizes.is_empty()
                                            && *direction == ratatui::layout::Direction::Vertical
                                            && let Some(height) = Self::constraint_height(
                                                &constraint,
                                                split_size,
                                            ) && height < Self::content_min_height(&pane.pane)
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
                    let border_style = border_style
                        .unwrap_or_else(|| ctx.config.as_border_style());
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
                    stack
                        .extend(
                            areas
                                .iter()
                                .enumerate()
                                .map(|(idx, area)| (&visible[idx].pane, *area)),
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
            Constraint::Percentage(p) => {
                Some(
                    u16::try_from(u32::from(total) * u32::from(*p) / 100)
                        .unwrap_or(u16::MAX),
                )
            }
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
