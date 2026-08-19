#![allow(deprecated)] // TODO remove after cleanup
use std::collections::HashMap;

use anyhow::{Result, ensure};
use derive_more::{Deref, Display, Into};
use itertools::Itertools;
use ratatui::{
    layout::Direction,
    style::{Color, Style},
    widgets::{Borders, TitlePosition},
};
use serde::{
    Deserialize, Serialize, de::Error as _,
    // The versioned hidden module serde's own derive uses for untagged
    // deserialization (Content capture); pinned by the lockfile (1.0.228).
    __private228::de::{Content, ContentDeserializer, ContentVisitor},
};
use thiserror::Error;
use unicase::UniCase;

use super::theme::{
    PercentOrLength,
    properties::{Property, PropertyFile, PropertyKind, PropertyKindFile},
    queue_table::ParseSizeError,
    volume_slider::{VolumeSliderConfig, VolumeSliderConfigFile},
};
use crate::{
    config::{
        defaults,
        theme::{
            ConfigColor,
            StyleFile,
            borders::{BorderSetInherited, BorderSetLib, BorderSymbols, BorderSymbolsFile},
            properties::{Alignment, PropertyKindFileOrText, StatusPropertyFile},
            style::ToConfigOr,
        },
    },
    shared::id::{self, Id},
};

#[derive(Debug, Into, Deref, Display)]
pub struct TabName(pub std::sync::Arc<String>);

impl From<String> for TabName {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<&str> for TabName {
    fn from(value: &str) -> Self {
        Self(value.to_owned().into())
    }
}

impl Clone for TabName {
    fn clone(&self) -> Self {
        TabName(std::sync::Arc::clone(&self.0))
    }
}

impl PartialEq for TabName {
    fn eq(&self, other: &Self) -> bool {
        UniCase::new(self.0.as_str()) == UniCase::new(other.0.as_str())
    }
}

impl Eq for TabName {}

impl std::hash::Hash for TabName {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        UniCase::new(self.0.as_str()).hash(state);
    }
}

/// Tree-browser layout args, shared by the four browser-family panes
/// (`Directories` / `Playlists` / `Jellyfin` / `Radio`): the left tree's
/// minimum width and narrow-TUI hide threshold, and the info box height
/// cap. `#[serde(default)]` on the variant fields keeps today's bare
/// `Directories` config syntax parsing unchanged; the defaults reproduce
/// the pre-args constants exactly (`directories::tree_width`: min 50 cols,
/// hidden <= 120; `.min(15)` info cap).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct TreeBrowserArgs {
    /// Minimum tree width in columns (default 50, the round-7/8 behavior).
    pub tree_min_width: u16,
    /// TUI width at/below which the tree is hidden entirely (default 120).
    pub tree_hide_below: u16,
    /// Info box height cap (default Some(15)); `None` = uncapped (the
    /// round-8/9 behavior).
    pub info_box_cap: Option<u16>,
}

impl Default for TreeBrowserArgs {
    fn default() -> Self {
        Self { tree_min_width: 50, tree_hide_below: 120, info_box_cap: Some(15) }
    }
}

impl TreeBrowserArgs {
    /// The left-tree width at `total` columns: hidden entirely at/below
    /// `tree_hide_below`, else 30% of the width with a `tree_min_width`
    /// floor (never the whole area — the right pane keeps >= 1 column).
    /// With the default args this is exactly the round-7/8
    /// `directories::tree_width` behavior.
    pub fn tree_width(&self, total: u16) -> u16 {
        if total <= self.tree_hide_below {
            return 0;
        }
        let by_percent = (u32::from(total) * 30 / 100) as u16;
        by_percent.max(self.tree_min_width).min(total - 1)
    }

    /// The info box height for a raw two-thirds share: capped at
    /// `info_box_cap` when set (`Some(15)` default = round-9 behavior);
    /// `None` keeps the raw share (round-8 behavior).
    pub fn info_box_height(&self, raw: u16) -> u16 {
        raw.min(self.info_box_cap.unwrap_or(raw))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum PaneTypeFile {
    Queue,
    QueueHeader(),
    #[cfg(debug_assertions)]
    Logs,
    Directories {
        #[serde(default)]
        tree: TreeBrowserArgs,
    },
    Artists,
    Albums,
    AlbumArtists,
    Playlists {
        #[serde(default)]
        tree: TreeBrowserArgs,
    },
    Search,
    Radio {
        #[serde(default)]
        tree: TreeBrowserArgs,
    },
    Jellyfin {
        #[serde(default)]
        tree: TreeBrowserArgs,
    },
    AlbumArt,
    Lyrics,
    ProgressBar,
    Volume {
        #[serde(default)]
        kind: VolumeTypeFile,
    },
    Controls,
    Header,
    Tabs,
    TabContent,
    #[cfg(debug_assertions)]
    FrameCount,
    Property {
        content: Vec<PropertyFile<PropertyKindFile>>,
        #[serde(default)]
        align: super::theme::properties::Alignment,
        #[serde(default)]
        scroll_speed: u16,
    },
    Browser {
        root_tag: String,
        separator: Option<String>,
    },
    Cava,
    Empty(),
}

/// Deserialize-only mirror of [`PaneTypeFile`] with the browser-tree args
/// (the explicit `Directories(tree: (...))` form and every parenthesized
/// variant). The manual [`Deserialize`] impl below feeds it from a
/// captured value tree, so the four browser panes ALSO accept today's
/// bare unit syntax (`Directories` without parens -> default args).
#[derive(Debug, Deserialize)]
#[allow(clippy::large_enum_variant)]
enum PaneTypeFileArgs {
    Queue,
    QueueHeader(),
    #[cfg(debug_assertions)]
    Logs,
    Directories {
        #[serde(default)]
        tree: TreeBrowserArgs,
    },
    Artists,
    Albums,
    AlbumArtists,
    Playlists {
        #[serde(default)]
        tree: TreeBrowserArgs,
    },
    Search,
    Radio {
        #[serde(default)]
        tree: TreeBrowserArgs,
    },
    Jellyfin {
        #[serde(default)]
        tree: TreeBrowserArgs,
    },
    AlbumArt,
    Lyrics,
    ProgressBar,
    Volume {
        #[serde(default)]
        kind: VolumeTypeFile,
    },
    Controls,
    Header,
    Tabs,
    TabContent,
    #[cfg(debug_assertions)]
    FrameCount,
    Property {
        content: Vec<PropertyFile<PropertyKindFile>>,
        #[serde(default)]
        align: super::theme::properties::Alignment,
        #[serde(default)]
        scroll_speed: u16,
    },
    Browser {
        root_tag: String,
        separator: Option<String>,
    },
    Cava,
    Empty(),
}

impl From<PaneTypeFileArgs> for PaneTypeFile {
    fn from(value: PaneTypeFileArgs) -> Self {
        match value {
            PaneTypeFileArgs::Queue => PaneTypeFile::Queue,
            PaneTypeFileArgs::QueueHeader() => PaneTypeFile::QueueHeader(),
            #[cfg(debug_assertions)]
            PaneTypeFileArgs::Logs => PaneTypeFile::Logs,
            PaneTypeFileArgs::Directories { tree } => PaneTypeFile::Directories { tree },
            PaneTypeFileArgs::Artists => PaneTypeFile::Artists,
            PaneTypeFileArgs::Albums => PaneTypeFile::Albums,
            PaneTypeFileArgs::AlbumArtists => PaneTypeFile::AlbumArtists,
            PaneTypeFileArgs::Playlists { tree } => PaneTypeFile::Playlists { tree },
            PaneTypeFileArgs::Search => PaneTypeFile::Search,
            PaneTypeFileArgs::Radio { tree } => PaneTypeFile::Radio { tree },
            PaneTypeFileArgs::Jellyfin { tree } => PaneTypeFile::Jellyfin { tree },
            PaneTypeFileArgs::AlbumArt => PaneTypeFile::AlbumArt,
            PaneTypeFileArgs::Lyrics => PaneTypeFile::Lyrics,
            PaneTypeFileArgs::ProgressBar => PaneTypeFile::ProgressBar,
            PaneTypeFileArgs::Volume { kind } => PaneTypeFile::Volume { kind },
            PaneTypeFileArgs::Controls => PaneTypeFile::Controls,
            PaneTypeFileArgs::Header => PaneTypeFile::Header,
            PaneTypeFileArgs::Tabs => PaneTypeFile::Tabs,
            PaneTypeFileArgs::TabContent => PaneTypeFile::TabContent,
            #[cfg(debug_assertions)]
            PaneTypeFileArgs::FrameCount => PaneTypeFile::FrameCount,
            PaneTypeFileArgs::Property { content, align, scroll_speed } => {
                PaneTypeFile::Property { content, align, scroll_speed }
            }
            PaneTypeFileArgs::Browser { root_tag, separator } => {
                PaneTypeFile::Browser { root_tag, separator }
            }
            PaneTypeFileArgs::Cava => PaneTypeFile::Cava,
            PaneTypeFileArgs::Empty() => PaneTypeFile::Empty(),
        }
    }
}

/// Recursively rewrite a captured `{Variant: ()}` (a zero-length tuple
/// variant such as `Empty()` / `InputBuffer()`) into `{Variant: ()}`'s
/// serde replay shape `{Variant: Seq([])}`: RON's value capture encodes
/// every parenthesized variant as a map `{name: payload}`, and serde's
/// tuple-variant replay requires a sequence payload for the empty tuple.
/// Unit variants capture as plain strings and struct/newtype variants as
/// maps with non-unit payloads, so the single-entry-unit case is exactly
/// a zero-length tuple variant.
fn fix_zero_tuple_variants(content: Content<'_>) -> Content<'_> {
    match content {
        Content::Map(mut entries) => {
            if entries.len() == 1 && matches!(entries[0].1, Content::Unit) {
                let (name, _) = entries.pop().expect("len == 1");
                return Content::Map(vec![(name, Content::Seq(Vec::new()))]);
            }
            Content::Map(
                entries
                    .into_iter()
                    .map(|(name, value)| {
                        (fix_zero_tuple_variants(name), fix_zero_tuple_variants(value))
                    })
                    .collect(),
            )
        }
        Content::Seq(values) => {
            Content::Seq(values.into_iter().map(fix_zero_tuple_variants).collect())
        }
        Content::Newtype(value) => Content::Newtype(Box::new(fix_zero_tuple_variants(*value))),
        Content::Some(value) => Content::Some(Box::new(fix_zero_tuple_variants(*value))),
        other => other,
    }
}

impl PaneTypeFile {
    /// The bare unit-form variants (no parentheses) by name; the browser
    /// panes get the default tree args (round-23 syntax).
    fn from_unit_variant(name: &str) -> Result<Self, String> {
        Ok(match name {
            "Queue" => PaneTypeFile::Queue,
            #[cfg(debug_assertions)]
            "Logs" => PaneTypeFile::Logs,
            "Directories" => PaneTypeFile::Directories { tree: TreeBrowserArgs::default() },
            "Artists" => PaneTypeFile::Artists,
            "Albums" => PaneTypeFile::Albums,
            "AlbumArtists" => PaneTypeFile::AlbumArtists,
            "Playlists" => PaneTypeFile::Playlists { tree: TreeBrowserArgs::default() },
            "Search" => PaneTypeFile::Search,
            "Radio" => PaneTypeFile::Radio { tree: TreeBrowserArgs::default() },
            "Jellyfin" => PaneTypeFile::Jellyfin { tree: TreeBrowserArgs::default() },
            "AlbumArt" => PaneTypeFile::AlbumArt,
            "Lyrics" => PaneTypeFile::Lyrics,
            "ProgressBar" => PaneTypeFile::ProgressBar,
            "Controls" => PaneTypeFile::Controls,
            "Header" => PaneTypeFile::Header,
            "Tabs" => PaneTypeFile::Tabs,
            "TabContent" => PaneTypeFile::TabContent,
            #[cfg(debug_assertions)]
            "FrameCount" => PaneTypeFile::FrameCount,
            "Cava" => PaneTypeFile::Cava,
            other => return Err(format!("unknown pane variant `{other}`")),
        })
    }
}

impl<'de> Deserialize<'de> for PaneTypeFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Capture the value with serde's Content: RON feeds the variant
        // names to it (`Directories` -> Str, `Directories(...)` ->
        // Map{name: payload}, `Empty()` -> Map{name: ()}). Bare unit
        // variants are dispatched by name (backward compat); every
        // parenthesized form replays into the derived mirror. This lives
        // in `serde::__private228` (the versioned hidden module serde's
        // own derive uses for untagged deserialization); the repo pins
        // serde 1.0.228, so the suffix is stable for the lockfile.
        let content = deserializer.deserialize_any(ContentVisitor::new())?;
        match content {
            Content::Str(name) => PaneTypeFile::from_unit_variant(name).map_err(D::Error::custom),
            Content::String(name) => {
                PaneTypeFile::from_unit_variant(&name).map_err(D::Error::custom)
            }
            content => PaneTypeFileArgs::deserialize(ContentDeserializer::<serde::de::value::Error>::new(
                fix_zero_tuple_variants(content),
            ))
            .map(Into::into)
            .map_err(D::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, strum::Display, strum::EnumDiscriminants)]
#[strum_discriminants(derive(strum::Display, Hash))]
pub enum PaneType {
    Queue,
    QueueHeader(),
    #[cfg(debug_assertions)]
    Logs,
    Directories {
        tree: TreeBrowserArgs,
    },
    Artists,
    AlbumArtists,
    Albums,
    Playlists {
        tree: TreeBrowserArgs,
    },
    Search,
    Radio {
        tree: TreeBrowserArgs,
    },
    Jellyfin {
        tree: TreeBrowserArgs,
    },
    AlbumArt,
    Lyrics,
    ProgressBar,
    Volume {
        kind: VolumeType,
    },
    Controls,
    Header,
    Tabs,
    TabContent,
    #[cfg(debug_assertions)]
    FrameCount,
    Property {
        content: Vec<Property<PropertyKind>>,
        align: ratatui::layout::Alignment,
        scroll_speed: u16,
    },
    Browser {
        root_tag: String,
        separator: Option<String>,
    },
    Cava,
    Empty,
}

pub const PANES_ALLOWED_IN_BOTH_TAB_AND_LAYOUT: [PaneTypeDiscriminants; 2] =
    [PaneTypeDiscriminants::Property, PaneTypeDiscriminants::Empty];

#[cfg(debug_assertions)]
pub const UNFOSUSABLE_TABS: [PaneTypeDiscriminants; 12] = [
    PaneTypeDiscriminants::AlbumArt,
    PaneTypeDiscriminants::ProgressBar,
    PaneTypeDiscriminants::Volume,
    PaneTypeDiscriminants::Controls,
    PaneTypeDiscriminants::Header,
    PaneTypeDiscriminants::Tabs,
    PaneTypeDiscriminants::TabContent,
    PaneTypeDiscriminants::FrameCount,
    PaneTypeDiscriminants::Property,
    PaneTypeDiscriminants::Cava,
    PaneTypeDiscriminants::QueueHeader,
    PaneTypeDiscriminants::Empty,
];

#[cfg(not(debug_assertions))]
pub const UNFOSUSABLE_TABS: [PaneTypeDiscriminants; 11] = [
    PaneTypeDiscriminants::AlbumArt,
    PaneTypeDiscriminants::ProgressBar,
    PaneTypeDiscriminants::Volume,
    PaneTypeDiscriminants::Controls,
    PaneTypeDiscriminants::Header,
    PaneTypeDiscriminants::Tabs,
    PaneTypeDiscriminants::TabContent,
    PaneTypeDiscriminants::Property,
    PaneTypeDiscriminants::Cava,
    PaneTypeDiscriminants::QueueHeader,
    PaneTypeDiscriminants::Empty,
];

impl Pane {
    pub fn is_focusable(&self) -> bool {
        !UNFOSUSABLE_TABS.contains(&PaneTypeDiscriminants::from(&self.pane))
    }
}

impl TryFrom<PaneTypeFile> for PaneType {
    type Error = anyhow::Error;

    fn try_from(value: PaneTypeFile) -> Result<PaneType, Self::Error> {
        Ok(match value {
            PaneTypeFile::Queue => PaneType::Queue,
            PaneTypeFile::QueueHeader() => PaneType::QueueHeader(),
            #[cfg(debug_assertions)]
            PaneTypeFile::Logs => PaneType::Logs,
            PaneTypeFile::Directories { tree } => PaneType::Directories { tree },
            PaneTypeFile::Artists => PaneType::Artists,
            PaneTypeFile::AlbumArtists => PaneType::AlbumArtists,
            PaneTypeFile::Albums => PaneType::Albums,
            PaneTypeFile::Playlists { tree } => PaneType::Playlists { tree },
            PaneTypeFile::Search => PaneType::Search,
            PaneTypeFile::Radio { tree } => PaneType::Radio { tree },
            PaneTypeFile::Jellyfin { tree } => PaneType::Jellyfin { tree },
            PaneTypeFile::AlbumArt => PaneType::AlbumArt,
            PaneTypeFile::Lyrics => PaneType::Lyrics,
            PaneTypeFile::ProgressBar => PaneType::ProgressBar,
            PaneTypeFile::Volume { kind } => PaneType::Volume {
                kind: match kind {
                    VolumeTypeFile::Slider(cfg) => VolumeType::Slider(cfg.into_config()?),
                },
            },
            PaneTypeFile::Controls => PaneType::Controls,
            PaneTypeFile::Header => PaneType::Header,
            PaneTypeFile::Tabs => PaneType::Tabs,
            PaneTypeFile::TabContent => PaneType::TabContent,
            #[cfg(debug_assertions)]
            PaneTypeFile::FrameCount => PaneType::FrameCount,
            PaneTypeFile::Property { content: properties, align, scroll_speed } => {
                PaneType::Property {
                    content: properties
                        .into_iter()
                        .map(|prop| prop.try_into().expect(""))
                        .collect_vec(),
                    align: align.into(),
                    scroll_speed,
                }
            }
            PaneTypeFile::Browser { root_tag: tag, separator } => {
                PaneType::Browser { root_tag: tag, separator }
            }
            PaneTypeFile::Cava => PaneType::Cava,
            PaneTypeFile::Empty() => PaneType::Empty,
        })
    }
}

impl TabsFile {
    pub fn convert(
        self,
        library: &HashMap<String, SizedPaneOrSplit>,
        border_set_library: &BorderSetLib,
    ) -> Result<Tabs> {
        let (names, tabs): (Vec<_>, HashMap<_, _>) = self
            .0
            .into_iter()
            .map(|tab| -> Result<_> {
                Ok(Tab {
                    name: tab.name.into(),
                    panes: tab.pane.convert(library, border_set_library)?,
                })
            })
            .try_fold((Vec::new(), HashMap::new()), |(mut names, mut tabs), tab| -> Result<_> {
                let tab = tab?;
                names.push(tab.name.clone());
                tabs.insert(tab.name.clone(), tab);
                Ok((names, tabs))
            })?;

        ensure!(!tabs.is_empty(), "At least one tab is required");

        Ok(Tabs { names, tabs })
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BorderTypeFile {
    Full,
    Single,
    #[default]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabsFile(Vec<TabFile>);

#[derive(Debug, Default, Clone)]
pub struct Tabs {
    pub names: Vec<TabName>,
    pub tabs: HashMap<TabName, Tab>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TabFile {
    name: String,
    #[deprecated]
    #[serde(default)]
    border_type: BorderTypeFile,
    pane: PaneOrSplitFile,
}

#[derive(Debug, Clone)]
pub struct Tab {
    pub name: TabName,
    pub panes: SizedPaneOrSplit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DirectionFile {
    Horizontal,
    Vertical,
}

impl From<DirectionFile> for Direction {
    fn from(value: DirectionFile) -> Self {
        match value {
            DirectionFile::Horizontal => Direction::Horizontal,
            DirectionFile::Vertical => Direction::Vertical,
        }
    }
}

impl From<&DirectionFile> for Direction {
    fn from(value: &DirectionFile) -> Self {
        match value {
            DirectionFile::Horizontal => Direction::Horizontal,
            DirectionFile::Vertical => Direction::Vertical,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum PaneOrSplitFile {
    Pane(PaneTypeFile),
    Component(String),
    Split {
        direction: DirectionFile,
        // Maybe these should be deprecated in favor of using the SubPaneFile borders?
        #[serde(default)]
        borders: BordersFile,
        panes: Vec<SubPaneFile>,
    },
}

impl Default for PaneOrSplitFile {
    fn default() -> Self {
        PaneOrSplitFile::Split {
            direction: DirectionFile::Vertical,
            borders: BordersFile::NONE,
            panes: vec![
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "4".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Split {
                        direction: DirectionFile::Horizontal,
                        borders: BordersFile::NONE,
                        panes: vec![
                            SubPaneFile {

                                    collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "35".to_string(),
                                background_color: None,
                                borders: BordersFile::LEFT | BordersFile::TOP | BordersFile::BOTTOM,
                                border_style: None,
                                border_active_style: None,
                                border_title: Vec::new(),
                                border_title_position: BorderTitlePosition::Top,
                                border_title_alignment: Alignment::Left,
                                border_symbols: BorderSymbolsFile::Inherited(BorderSetInherited {
                                    parent: Box::new(BorderSymbolsFile::Rounded),
                                    bottom_left: Some("├".to_string()),
                                    ..Default::default()
                                }),
                                pane: PaneOrSplitFile::Component("header_left".to_string()),
                            },
                            SubPaneFile {

                                    collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                                background_color: None,
                                borders: BordersFile::ALL,
                                border_style: None,
                                border_active_style: None,
                                border_title: Vec::new(),
                                border_title_position: BorderTitlePosition::Top,
                                border_title_alignment: Alignment::Left,
                                border_symbols: BorderSymbolsFile::Inherited(BorderSetInherited {
                                    parent: Box::new(BorderSymbolsFile::Rounded),
                                    top_left: Some("┬".to_string()),
                                    top_right: Some("┬".to_string()),
                                    bottom_left: Some("┴".to_string()),
                                    bottom_right: Some("┴".to_string()),
                                    ..Default::default()
                                }),
                                pane: PaneOrSplitFile::Component("header_center".to_string()),
                            },
                            SubPaneFile {

                                    collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "35".to_string(),
                                background_color: None,
                                borders: BordersFile::RIGHT
                                    | BordersFile::TOP
                                    | BordersFile::BOTTOM,
                                border_style: None,
                                border_active_style: None,
                                border_title: Vec::new(),
                                border_title_position: BorderTitlePosition::Top,
                                border_title_alignment: Alignment::Left,
                                border_symbols: BorderSymbolsFile::Inherited(BorderSetInherited {
                                    parent: Box::new(BorderSymbolsFile::Rounded),
                                    bottom_right: Some("┤".to_string()),
                                    ..Default::default()
                                }),
                                pane: PaneOrSplitFile::Component("header_right".to_string()),
                            },
                        ],
                    },
                },
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "2".to_string(),
                    background_color: None,
                    borders: BordersFile::LEFT | BordersFile::RIGHT | BordersFile::BOTTOM,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::Rounded,
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::Tabs),
                },
                SubPaneFile {

                        collapse_below: Some(15), shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::TabContent),
                },
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "3".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_title: Vec::new(),
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Split {
                        direction: DirectionFile::Horizontal,
                        borders: BordersFile::NONE,
                        panes: vec![
                            SubPaneFile {

                                    collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "12".to_string(),
                                background_color: None,
                                borders: BordersFile::ALL,
                                border_style: None,
                                border_active_style: None,
                                border_title: Vec::new(),
                                border_title_position: BorderTitlePosition::Top,
                                border_title_alignment: Alignment::Left,
                                border_symbols: BorderSymbolsFile::Inherited(BorderSetInherited {
                                    parent: Box::new(BorderSymbolsFile::Rounded),
                                    top_right: Some("┬".to_string()),
                                    bottom_right: Some("┴".to_string()),
                                    ..Default::default()
                                }),
                                pane: PaneOrSplitFile::Component("input_mode".to_string()),
                            },
                            SubPaneFile {

                                    collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                                background_color: None,
                                borders: BordersFile::TOP
                                    | BordersFile::BOTTOM
                                    | BordersFile::RIGHT,
                                border_style: None,
                                border_active_style: None,
                                border_title_alignment: Alignment::Right,
                                border_symbols: BorderSymbolsFile::Rounded,
                                border_title_position: BorderTitlePosition::Top,
                                border_title: vec![
                                    PropertyFile {
                                        kind: PropertyKindFileOrText::Text(" ".to_string()),
                                        style: None,
                                        default: None,
                                    },
                                    PropertyFile {
                                        kind: PropertyKindFileOrText::Property(
                                            PropertyKindFile::Status(
                                                StatusPropertyFile::QueueLength {
                                                    thousands_separator:
                                                        defaults::default_thousands_separator(),
                                                },
                                            ),
                                        ),
                                        style: None,
                                        default: None,
                                    },
                                    PropertyFile {
                                        kind: PropertyKindFileOrText::Text(" songs / ".to_string()),
                                        style: None,
                                        default: None,
                                    },
                                    PropertyFile {
                                        kind: PropertyKindFileOrText::Property(
                                            PropertyKindFile::Status(
                                                StatusPropertyFile::QueueTimeTotal {
                                                    separator: None,
                                                },
                                            ),
                                        ),
                                        style: None,
                                        default: None,
                                    },
                                    PropertyFile {
                                        kind: PropertyKindFileOrText::Text(
                                            " total time ".to_string(),
                                        ),
                                        style: None,
                                        default: None,
                                    },
                                ],
                                pane: PaneOrSplitFile::Component("progress_bar".to_string()),
                            },
                        ],
                    },
                },
            ],
        }
    }
}

use bitflags::bitflags;
bitflags! {
    #[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
    pub struct BordersFile: u8 {
        const NONE   = 0b0000;
        const TOP    = 0b0001;
        const RIGHT  = 0b0010;
        const BOTTOM = 0b0100;
        const LEFT   = 0b1000;
        const ALL = Self::TOP.bits() | Self::RIGHT.bits() | Self::BOTTOM.bits() | Self::LEFT.bits();
    }
}

impl From<BordersFile> for Borders {
    fn from(value: BordersFile) -> Self {
        self::Borders::from_bits_truncate(value.bits())
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BorderTitlePosition {
    #[default]
    Top,
    Bottom,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SubPaneFile {
    pub size: String,
    pub background_color: Option<String>,
    pub borders: BordersFile,
    pub border_style: Option<StyleFile>,
    pub border_active_style: Option<StyleFile>,
    pub border_title: Vec<PropertyFile<PropertyKindFile>>,
    pub border_title_position: BorderTitlePosition,
    pub border_title_alignment: Alignment,
    pub border_symbols: BorderSymbolsFile,
    pub collapse_below: Option<u16>,
    pub shrink_below: Option<u16>,
    /// (window height, size) breakpoints, evaluated against the terminal
    /// window height; sizes are linearly interpolated between breakpoints.
    pub window_sizes: Vec<(u16, String)>,
    pub pane: PaneOrSplitFile,
}

#[derive(Debug, Clone)]
pub struct Pane {
    pub pane: PaneType,
    pub background_color: Option<Color>,
    pub borders: Borders,
    pub border_style: Option<Style>,
    pub border_active_style: Option<Style>,
    pub border_title: Vec<Property<PropertyKind>>,
    pub border_title_position: TitlePosition,
    pub border_title_alignment: ratatui::layout::Alignment,
    pub border_symbols: BorderSymbols,
    pub id: Id,
}

#[derive(Debug, Clone)]
pub enum SizedPaneOrSplit {
    Pane(Pane),
    Split {
        background_color: Option<Color>,
        borders: Borders,
        border_style: Option<Style>,
        border_title: Vec<Property<PropertyKind>>,
        border_title_position: TitlePosition,
        border_title_alignment: ratatui::layout::Alignment,
        border_symbols: BorderSymbols,
        direction: Direction,
        panes: Vec<SizedSubPane>,
    },
}

impl Default for SizedPaneOrSplit {
    fn default() -> Self {
        Self::Split {
            background_color: None,
            direction: Direction::Horizontal,
            panes: Vec::new(),
            borders: Borders::NONE,
            border_style: None,
            border_title: Vec::new(),
            border_title_position: TitlePosition::Top,
            border_title_alignment: ratatui::layout::Alignment::Left,
            border_symbols: BorderSymbols::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SizedSubPane {
    pub size: PercentOrLength,
    pub collapse_below: Option<u16>,
    pub shrink_below: Option<u16>,
    pub window_sizes: Vec<(u16, PercentOrLength)>,
    pub pane: SizedPaneOrSplit,
}

#[derive(Error, Debug)]
pub enum PaneConversionError {
    #[error("Missing component: {0}")]
    MissingComponent(String),
    #[error("Missing border set: {0}")]
    MissingBorderSet(String),
    #[error("Failed to parse pane size: {0}")]
    ParseError(#[from] ParseSizeError),
    #[error("Failed to parse pane: {0}")]
    Generic(#[from] anyhow::Error),
}

impl PaneOrSplitFile {
    #[allow(
        clippy::too_many_arguments,
        reason = "Recursive function, used only here. More trouble than it is worth to refactor at this point"
    )]
    pub fn convert_recursive(
        &self,
        bg_col: Option<Color>,
        b: Borders,
        b_style: Option<Style>,
        b_active_style: Option<Style>,
        b_title: Vec<Property<PropertyKind>>,
        b_pos: TitlePosition,
        b_alignment: ratatui::layout::Alignment,
        b_symbols: BorderSymbols,
        library: &HashMap<String, SizedPaneOrSplit>,
        border_set_library: &BorderSetLib,
    ) -> Result<SizedPaneOrSplit, PaneConversionError> {
        Ok(match self {
            PaneOrSplitFile::Pane(pane_type_file) => SizedPaneOrSplit::Pane(Pane {
                pane: pane_type_file.clone().try_into()?,
                background_color: bg_col,
                borders: b,
                border_style: b_style,
                border_active_style: b_active_style,
                border_title: b_title,
                border_title_position: b_pos,
                border_title_alignment: b_alignment,
                border_symbols: b_symbols,
                id: id::new(),
            }),
            // Components need to get border etc from the usage site and NOT the ones they are given
            // during resolution because they are given default values initially.
            PaneOrSplitFile::Component(name) => match library.get(name) {
                Some(SizedPaneOrSplit::Pane(pane)) => {
                    let mut v = pane.clone();
                    v.background_color = bg_col;
                    v.borders = b;
                    v.border_style = b_style;
                    v.border_active_style = b_active_style;
                    v.border_title.clone_from(&b_title);
                    v.border_symbols = b_symbols;
                    v.border_title_alignment = b_alignment;
                    v.border_title_position = b_pos;
                    SizedPaneOrSplit::Pane(v)
                }
                Some(SizedPaneOrSplit::Split {
                    background_color: _,
                    borders,
                    direction,
                    panes,
                    border_title: _,
                    border_style: _,
                    border_title_position: _,
                    border_title_alignment: _,
                    border_symbols: _,
                }) => SizedPaneOrSplit::Split {
                    background_color: bg_col,
                    borders: *borders | b,
                    border_style: b_style,
                    border_title: b_title,
                    border_title_position: b_pos,
                    border_title_alignment: b_alignment,
                    border_symbols: b_symbols.clone(),
                    direction: *direction,
                    panes: panes.clone(),
                },
                None => return Err(PaneConversionError::MissingComponent(name.clone())),
            },
            PaneOrSplitFile::Split { direction, borders, panes } => SizedPaneOrSplit::Split {
                direction: direction.into(),
                background_color: bg_col,
                borders: Into::<Borders>::into(*borders) | b,
                border_style: b_style,
                border_title: b_title,
                border_title_position: b_pos,
                border_title_alignment: b_alignment,
                border_symbols: b_symbols,
                panes: panes
                    .iter()
                    .map(|sub_pane| -> Result<SizedSubPane, PaneConversionError> {
                        let size: PercentOrLength = sub_pane.size.parse()?;
                        let borders: Borders = sub_pane.borders.into();
                        let b_title = sub_pane
                            .border_title
                            .iter()
                            .cloned()
                            .map(Property::try_from)
                            .try_collect()?;
                        let b_style = sub_pane
                            .border_style
                            .as_ref()
                            .map(|s| s.to_config_or(None, None))
                            .transpose()?;
                        let b_active_style = sub_pane
                            .border_active_style
                            .as_ref()
                            .map(|s| s.to_config_or(None, None))
                            .transpose()?;

                        let background_color = sub_pane
                            .background_color
                            .as_ref()
                            .map(|c| c.as_bytes().try_into())
                            .transpose()?
                            .map(|c: ConfigColor| c.into());
                        let b_pos = match sub_pane.border_title_position {
                            BorderTitlePosition::Top => TitlePosition::Top,
                            BorderTitlePosition::Bottom => TitlePosition::Bottom,
                        };
                        let b_alignment = sub_pane.border_title_alignment.into();
                        let b_symbols =
                            sub_pane.border_symbols.clone().into_symbols(border_set_library)?;
                        let pane = sub_pane.pane.convert_recursive(
                            background_color,
                            borders,
                            b_style,
                            b_active_style,
                            b_title,
                            b_pos,
                            b_alignment,
                            b_symbols,
                            library,
                            border_set_library,
                        )?;

                        let window_sizes = sub_pane
                            .window_sizes
                            .iter()
                            .map(|(height, size)| -> Result<(u16, PercentOrLength), ParseSizeError> {
                                Ok((*height, size.parse()?))
                            })
                            .try_collect()?;

                        Ok(SizedSubPane {
                            size,
                            pane,
                            collapse_below: sub_pane.collapse_below,
                            shrink_below: sub_pane.shrink_below,
                            window_sizes,
                        })
                    })
                    .try_collect()?,
            },
        })
    }

    pub fn convert(
        &self,
        library: &HashMap<String, SizedPaneOrSplit>,
        border_set_library: &BorderSetLib,
    ) -> Result<SizedPaneOrSplit, PaneConversionError> {
        self.convert_recursive(
            None,
            Borders::NONE,
            None,
            None,
            Vec::new(),
            TitlePosition::default(),
            ratatui::layout::Alignment::default(),
            BorderSymbols::default(),
            library,
            border_set_library,
        )
    }
}

pub struct PaneIter<'a> {
    queue: Vec<&'a SizedPaneOrSplit>,
}

impl<'a> Iterator for PaneIter<'a> {
    type Item = &'a Pane;

    fn next(&mut self) -> Option<Self::Item> {
        match self.queue.pop() {
            Some(SizedPaneOrSplit::Pane(pane)) => Some(pane),
            Some(SizedPaneOrSplit::Split { panes: sub_panes, .. }) => {
                self.queue.extend(sub_panes.iter().map(|v| &v.pane));
                self.next()
            }
            None => None,
        }
    }
}

impl SizedPaneOrSplit {
    pub fn panes_iter(&self) -> PaneIter<'_> {
        PaneIter {
            queue: match self {
                p @ SizedPaneOrSplit::Pane { .. } => vec![p],
                SizedPaneOrSplit::Split { panes: sub_panes, .. } => {
                    sub_panes.iter().map(|v| &v.pane).collect()
                }
            },
        }
    }
}

impl Default for TabsFile {
    fn default() -> Self {
        Self(vec![
            TabFile {
                name: "Local".to_string(),
                border_type: BorderTypeFile::None,
                pane: PaneOrSplitFile::Split {
                    direction: DirectionFile::Horizontal,
                    borders: BordersFile::NONE,
                    panes: vec![
                        SubPaneFile {

                                collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "35%".to_string(),
                            background_color: None,
                            borders: BordersFile::NONE,
                            border_style: None,
                            border_active_style: None,
                            border_title: Vec::new(),
                            border_title_position: BorderTitlePosition::Top,
                            border_title_alignment: Alignment::Left,
                            border_symbols: BorderSymbolsFile::default(),
                            pane: PaneOrSplitFile::Split {
                                direction: DirectionFile::Vertical,
                                borders: BordersFile::NONE,
                                panes: vec![
                                    SubPaneFile {

                                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),pane: PaneOrSplitFile::Pane(PaneTypeFile::AlbumArt),
                                        background_color: None,
                                        size: "100%".to_string(),
                                        borders: BordersFile::TOP
                                            | BordersFile::LEFT
                                            | BordersFile::RIGHT,
                                        border_style: None,
                                        border_active_style: None,
                                        border_title_position: BorderTitlePosition::Top,
                                        border_title_alignment: Alignment::Left,
                                        border_symbols: BorderSymbolsFile::Rounded,
                                        border_title: Vec::new(),
                                    },
                                    SubPaneFile {

                                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),pane: PaneOrSplitFile::Pane(PaneTypeFile::Lyrics),
                                        background_color: None,
                                        size: "7".to_string(),
                                        border_title: vec![PropertyFile {
                                            kind: PropertyKindFileOrText::Text(
                                                " Lyrics ".to_string(),
                                            ),
                                            style: None,
                                            default: None,
                                        }],
                                        borders: BordersFile::ALL,
                                        border_style: None,
                                        border_active_style: None,
                                        border_title_position: BorderTitlePosition::Top,
                                        border_title_alignment: Alignment::Right,
                                        border_symbols: BorderSymbolsFile::Inherited(
                                            BorderSetInherited {
                                                parent: Box::new(BorderSymbolsFile::Rounded),
                                                top_left: Some("├".to_string()),
                                                top_right: Some("┤".to_string()),
                                                ..Default::default()
                                            },
                                        ),
                                    },
                                ],
                            },
                        },
                        SubPaneFile {

                                collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "65%".to_string(),
                            background_color: None,
                            borders: BordersFile::NONE,
                            border_style: None,
                            border_active_style: None,
                            border_title: Vec::new(),
                            border_title_position: BorderTitlePosition::Top,
                            border_title_alignment: Alignment::Left,
                            border_symbols: BorderSymbolsFile::default(),
                            pane: PaneOrSplitFile::Split {
                                direction: DirectionFile::Vertical,
                                borders: BordersFile::NONE,
                                panes: vec![
                                    SubPaneFile {

                                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "3".to_string(),
                                        background_color: None,
                                        borders: BordersFile::ALL,
                                        border_style: None,
                                        border_active_style: None,
                                        border_title: Vec::new(),
                                        border_title_position: BorderTitlePosition::Top,
                                        border_title_alignment: Alignment::Left,
                                        border_symbols: BorderSymbolsFile::Inherited(
                                            BorderSetInherited {
                                                parent: Box::new(BorderSymbolsFile::Rounded),
                                                bottom_left: Some("├".to_string()),
                                                bottom_right: Some("┤".to_string()),
                                                ..Default::default()
                                            },
                                        ),
                                        pane: PaneOrSplitFile::Split {
                                            direction: DirectionFile::Horizontal,
                                            borders: BordersFile::NONE,
                                            panes: vec![
                                                SubPaneFile {

                                                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),pane: PaneOrSplitFile::Pane(
                                                        PaneTypeFile::Empty(),
                                                    ),
                                                    background_color: None,
                                                    size: "1".to_string(),
                                                    borders: BordersFile::NONE,
                                                    border_style: None,
                                                    border_active_style: None,
                                                    border_title: Vec::new(),
                                                    border_title_position: BorderTitlePosition::Top,
                                                    border_title_alignment: Alignment::Left,
                                                    border_symbols: BorderSymbolsFile::default(),
                                                },
                                                SubPaneFile {

                                                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                                                    background_color: None,
                                                    borders: BordersFile::NONE,
                                                    border_style: None,
                                                    border_active_style: None,
                                                    border_title: Vec::new(),
                                                    border_title_position: BorderTitlePosition::Top,
                                                    border_title_alignment: Alignment::Left,
                                                    border_symbols: BorderSymbolsFile::default(),
                                                    pane: PaneOrSplitFile::Pane(
                                                        PaneTypeFile::QueueHeader(),
                                                    ),
                                                },
                                            ],
                                        },
                                    },
                                    SubPaneFile {

                                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                                        background_color: None,
                                        borders: BordersFile::LEFT
                                            | BordersFile::RIGHT
                                            | BordersFile::BOTTOM,
                                        border_style: None,
                                        border_active_style: None,
                                        border_title: Vec::new(),
                                        border_title_position: BorderTitlePosition::Top,
                                        border_title_alignment: Alignment::Left,
                                        border_symbols: BorderSymbolsFile::Rounded,
                                        pane: PaneOrSplitFile::Split {
                                            direction: DirectionFile::Horizontal,
                                            borders: BordersFile::NONE,
                                            panes: vec![
                                                SubPaneFile {

                                                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),pane: PaneOrSplitFile::Pane(
                                                        PaneTypeFile::Empty(),
                                                    ),
                                                    background_color: None,
                                                    size: "1".to_string(),
                                                    borders: BordersFile::NONE,
                                                    border_style: None,
                                                    border_active_style: None,
                                                    border_title: Vec::new(),
                                                    border_title_position: BorderTitlePosition::Top,
                                                    border_title_alignment: Alignment::Left,
                                                    border_symbols: BorderSymbolsFile::default(),
                                                },
                                                SubPaneFile {

                                                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                                                    background_color: None,
                                                    borders: BordersFile::NONE,
                                                    border_style: None,
                                                    border_active_style: None,
                                                    border_title: Vec::new(),
                                                    border_title_position: BorderTitlePosition::Top,
                                                    border_title_alignment: Alignment::Left,
                                                    border_symbols: BorderSymbolsFile::default(),
                                                    pane: PaneOrSplitFile::Pane(
                                                        PaneTypeFile::Queue,
                                                    ),
                                                },
                                            ],
                                        },
                                    },
                                ],
                            },
                        },
                    ],
                },
            },
            #[cfg(debug_assertions)]
            #[cfg(not(test))]
            TabFile {
                name: "Radio".to_string(),
                border_type: BorderTypeFile::None,
                pane: PaneOrSplitFile::Split {
                    borders: BordersFile::NONE,
                    direction: DirectionFile::Vertical,
                    panes: vec![SubPaneFile {

                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                        background_color: None,
                        borders: BordersFile::ALL,
                        border_style: None,
                        border_active_style: None,
                        border_title: Vec::new(),
                        border_title_position: BorderTitlePosition::Top,
                        border_title_alignment: Alignment::Left,
                        border_symbols: BorderSymbolsFile::Rounded,
                        pane: PaneOrSplitFile::Pane(PaneTypeFile::Radio { tree: TreeBrowserArgs::default() }),
                    }],
                },
            },
            #[cfg(debug_assertions)]
            #[cfg(not(test))]
            TabFile {
                name: "Jellyfin".to_string(),
                border_type: BorderTypeFile::None,
                pane: PaneOrSplitFile::Split {
                    borders: BordersFile::NONE,
                    direction: DirectionFile::Vertical,
                    panes: vec![SubPaneFile {

                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                        background_color: None,
                        borders: BordersFile::ALL,
                        border_style: None,
                        border_active_style: None,
                        border_title: Vec::new(),
                        border_title_position: BorderTitlePosition::Top,
                        border_title_alignment: Alignment::Left,
                        border_symbols: BorderSymbolsFile::Rounded,
                        pane: PaneOrSplitFile::Pane(PaneTypeFile::Jellyfin { tree: TreeBrowserArgs::default() }),
                    }],
                },
            },
            TabFile {
                name: "MPD".to_string(),
                border_type: BorderTypeFile::None,
                pane: PaneOrSplitFile::Split {
                    borders: BordersFile::NONE,
                    direction: DirectionFile::Vertical,
                    panes: vec![SubPaneFile {

                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                        background_color: None,
                        borders: BordersFile::ALL,
                        border_style: None,
                        border_active_style: None,
                        border_title: Vec::new(),
                        border_title_position: BorderTitlePosition::Top,
                        border_title_alignment: Alignment::Left,
                        border_symbols: BorderSymbolsFile::Rounded,
                        pane: PaneOrSplitFile::Pane(PaneTypeFile::Directories { tree: TreeBrowserArgs::default() }),
                    }],
                },
            },
            TabFile {
                name: "Artists".to_string(),
                border_type: BorderTypeFile::None,
                pane: PaneOrSplitFile::Split {
                    borders: BordersFile::NONE,
                    direction: DirectionFile::Vertical,
                    panes: vec![SubPaneFile {

                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                        background_color: None,
                        borders: BordersFile::ALL,
                        border_style: None,
                        border_active_style: None,
                        border_title: Vec::new(),
                        border_title_position: BorderTitlePosition::Top,
                        border_title_alignment: Alignment::Left,
                        border_symbols: BorderSymbolsFile::Rounded,
                        pane: PaneOrSplitFile::Pane(PaneTypeFile::Artists),
                    }],
                },
            },
            TabFile {
                name: "Album Artists".to_string(),
                border_type: BorderTypeFile::None,
                pane: PaneOrSplitFile::Split {
                    borders: BordersFile::NONE,
                    direction: DirectionFile::Vertical,
                    panes: vec![SubPaneFile {

                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                        background_color: None,
                        borders: BordersFile::ALL,
                        border_style: None,
                        border_active_style: None,
                        border_title: Vec::new(),
                        border_title_position: BorderTitlePosition::Top,
                        border_title_alignment: Alignment::Left,
                        border_symbols: BorderSymbolsFile::Rounded,
                        pane: PaneOrSplitFile::Pane(PaneTypeFile::AlbumArtists),
                    }],
                },
            },
            TabFile {
                name: "Albums".to_string(),
                border_type: BorderTypeFile::None,
                pane: PaneOrSplitFile::Split {
                    borders: BordersFile::NONE,
                    direction: DirectionFile::Vertical,
                    panes: vec![SubPaneFile {

                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                        background_color: None,
                        borders: BordersFile::ALL,
                        border_style: None,
                        border_active_style: None,
                        border_title: Vec::new(),
                        border_title_position: BorderTitlePosition::Top,
                        border_title_alignment: Alignment::Left,
                        border_symbols: BorderSymbolsFile::Rounded,
                        pane: PaneOrSplitFile::Pane(PaneTypeFile::Albums),
                    }],
                },
            },
            TabFile {
                name: "Playlists".to_string(),
                border_type: BorderTypeFile::None,
                pane: PaneOrSplitFile::Split {
                    borders: BordersFile::NONE,
                    direction: DirectionFile::Vertical,
                    panes: vec![SubPaneFile {

                            collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "100%".to_string(),
                        background_color: None,
                        borders: BordersFile::ALL,
                        border_style: None,
                        border_active_style: None,
                        border_title: Vec::new(),
                        border_title_position: BorderTitlePosition::Top,
                        border_title_alignment: Alignment::Left,
                        border_symbols: BorderSymbolsFile::Rounded,
                        pane: PaneOrSplitFile::Pane(PaneTypeFile::Playlists { tree: TreeBrowserArgs::default() }),
                    }],
                },
            },
        ])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum VolumeTypeFile {
    Slider(VolumeSliderConfigFile),
}

impl Default for VolumeTypeFile {
    fn default() -> Self {
        Self::Slider(VolumeSliderConfigFile::default())
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, strum::Display, strum::EnumDiscriminants)]
pub enum VolumeType {
    Slider(VolumeSliderConfig),
}

pub(crate) fn validate_tabs(layout: &SizedPaneOrSplit, tabs: &Tabs) -> Result<()> {
    let layout_panes = layout.panes_iter().collect_vec();
    ensure!(
        !layout_panes.iter().all(|pane| pane.is_focusable()),
        "Only non-focusable panes are supported in the layout. Possible values: {}",
        UNFOSUSABLE_TABS.iter().join(", ")
    );
    ensure!(
        layout_panes.iter().filter(|pane| pane.pane == PaneType::TabContent).count() == 1,
        "Layout must contain exactly one TabContent pane"
    );

    let all_tab_panes = tabs.tabs.values().flat_map(|tab| tab.panes.panes_iter()).collect_vec();
    let panes_in_both_tabs_and_layout = all_tab_panes
        .iter()
        .flat_map(|tab_pane| {
            layout_panes.iter().filter(|layout_pane| layout_pane.pane == tab_pane.pane)
        })
        .filter(|pane| {
            !PANES_ALLOWED_IN_BOTH_TAB_AND_LAYOUT.contains(&PaneTypeDiscriminants::from(&pane.pane))
        })
        .map(|pane| PaneTypeDiscriminants::from(&pane.pane))
        .unique()
        .collect_vec();
    ensure!(
        panes_in_both_tabs_and_layout.is_empty(),
        "Panes cannot be in layout and tabs at the same time. Please remove following tabs from either layout or tabs: {}",
        panes_in_both_tabs_and_layout.iter().join(", ")
    );

    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Today's bare-`Directories` config syntax (a unit variant, no args)
    /// parses with the default tree args — backward compatibility is
    /// load-bearing: the round-23 configs need no edits.
    #[test]
    fn bare_browser_panes_parse_with_default_tree_args() {
        let tabs: TabsFile = ron::from_str(
            r#"#![enable(unwrap_newtypes)]
[
    (
        name: "MPD",
        pane: Pane(Directories),
    ),
    (
        name: "Playlists",
        pane: Pane(Playlists),
    ),
    (
        name: "Jellyfin",
        pane: Pane(Jellyfin),
    ),
    (
        name: "Radio",
        pane: Pane(Radio),
    ),
]
"#,
        )
        .expect("bare browser panes must keep parsing (round-23 syntax)");
        let panes: Vec<PaneTypeFile> = tabs
            .0
            .into_iter()
            .map(|tab| match tab.pane {
                PaneOrSplitFile::Pane(pane) => pane,
                PaneOrSplitFile::Split { .. } | PaneOrSplitFile::Component(_) => {
                    panic!("test config must use bare panes")
                }
            })
            .collect();
        for pane in panes {
            let tree = match pane {
                PaneTypeFile::Directories { tree }
                | PaneTypeFile::Playlists { tree }
                | PaneTypeFile::Jellyfin { tree }
                | PaneTypeFile::Radio { tree } => tree,
                other => panic!("expected a browser pane, got {other:?}"),
            };
            assert_eq!(
                tree,
                TreeBrowserArgs::default(),
                "bare browser panes parse with the default tree args"
            );
        }
    }

    /// A config that sets explicit tree args round-trips through serde:
    /// the args survive parse -> serialize -> parse unchanged.
    #[test]
    fn explicit_tree_args_round_trip() {
        let tabs: TabsFile = ron::from_str(
            r#"#![enable(unwrap_newtypes)]
[
    (
        name: "MPD",
        pane: Pane(Directories(tree: (tree_min_width: 60, tree_hide_below: 100, info_box_cap: None))),
    ),
]
"#,
        )
        .expect("explicit tree args must parse");
        let PaneOrSplitFile::Pane(PaneTypeFile::Directories { tree }) =
            &tabs.0[0].pane
        else {
            panic!("expected a Directories pane");
        };
        assert_eq!(tree.tree_min_width, 60);
        assert_eq!(tree.tree_hide_below, 100);
        assert_eq!(tree.info_box_cap, None);

        let serialized = ron::to_string(&tabs).expect("round-trip serialize");
        let reparsed: TabsFile =
            ron::from_str(&serialized).expect("round-trip reparse");
        assert_eq!(reparsed, tabs, "explicit tree args round-trip unchanged");
    }

    /// The serde defaults reproduce today's constants exactly (the parity
    /// pin for the args side): 50-col minimum, hidden <= 120, info cap 15.
    #[test]
    fn default_args_are_today_s_constants() {
        let args = TreeBrowserArgs::default();
        assert_eq!(args.tree_min_width, 50);
        assert_eq!(args.tree_hide_below, 120);
        assert_eq!(args.info_box_cap, Some(15));
    }

    /// The construction-pattern bridge (see
    /// docs/design/Rewrite/new-browser-tab.md): a config block
    /// (`Directories { tree: ... }`) converts into the pane type, and
    /// `Config::tree_browser_args` hands those args to the singleton
    /// adapter; pane types absent from the config fall back to defaults.
    #[test]
    fn config_tree_browser_args_read_the_first_occurrence_args() {
        let tabs_file = TabsFile(vec![TabFile {
            name: "Local".to_string(),
            border_type: BorderTypeFile::None,
            pane: PaneOrSplitFile::Pane(PaneTypeFile::Directories {
                tree: TreeBrowserArgs {
                    tree_min_width: 60,
                    tree_hide_below: 100,
                    info_box_cap: None,
                },
            }),
        }]);
        let tabs = tabs_file
            .convert(&HashMap::new(), &BorderSetLib::default())
            .expect("a bare Directories tab converts");
        let config = crate::config::Config { tabs, ..crate::config::Config::default() };
        assert_eq!(
            config.tree_browser_args(PaneTypeDiscriminants::Directories),
            TreeBrowserArgs { tree_min_width: 60, tree_hide_below: 100, info_box_cap: None },
            "the config block's tree args drive the adapter"
        );
        assert_eq!(
            config.tree_browser_args(PaneTypeDiscriminants::Radio),
            TreeBrowserArgs::default(),
            "panes absent from the config fall back to the defaults"
        );
    }

    /// Round-34 live regression: the lyrics pane must be keyboard-
    /// focusable in release builds (it was in `UNFOSUSABLE_TABS`, so
    /// clicks on the pencil/words never moved focus and the edit-mode
    /// keyboard — ←/→/w/s/+/-/Enter/<C-s>/Esc — never reached the pane;
    /// the unit tests exercised `handle_action` directly and missed it).
    #[test]
    fn lyrics_pane_is_focusable() {
        let pane_type: PaneType = PaneTypeFile::Lyrics.try_into().expect("Lyrics pane converts");
        assert!(
            !UNFOSUSABLE_TABS.contains(&PaneTypeDiscriminants::from(&pane_type)),
            "Lyrics must not be in the unfocusable list (round-34 edit-mode keyboard)"
        );
    }
}
