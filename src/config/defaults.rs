#![allow(clippy::unnecessary_wraps)]

use std::collections::HashMap;

use super::theme::{Modifiers, StyleFile, properties::SongPropertyFile};
use crate::config::{
    tabs::{
        BorderTitlePosition,
        BordersFile,
        DirectionFile,
        PaneOrSplitFile,
        PaneTypeFile,
        SubPaneFile,
        VolumeTypeFile,
    },
    theme::{
        borders::BorderSymbolsFile,
        properties::{
            Alignment,
            PropertyFile,
            PropertyKindFile,
            PropertyKindFileOrText,
            ReplacementFile,
            StatusPropertyFile,
            TransformFile,
        },
        volume_slider::VolumeSliderConfigFile,
    },
};

pub fn bool<const V: bool>() -> bool {
    V
}

pub fn u8<const V: u8>() -> u8 {
    V
}

pub fn i32<const V: i32>() -> i32 {
    V
}

pub fn default_radio_playlist() -> String {
    "radio".to_owned()
}

pub fn default_max_favourites() -> usize {
    10
}

pub fn default_torrent_port() -> u16 {
    3030
}

/// KB/s threshold for the torrent bandwidth gate (default 500 KB/s —
/// comfortably above typical 720p–1080p encodings).
pub fn default_torrent_min_speed_kbps() -> u32 {
    500
}

/// Minimum sampling window of the bandwidth gate before it can pass.
pub fn default_torrent_warmup_secs() -> u64 {
    5
}

/// Hard deadline of the bandwidth gate before the Retry / Play anyway /
/// Cancel dialog shows.
pub fn default_torrent_max_wait_secs() -> u64 {
    15
}

/// How long to wait for any peer before aborting a dead magnet.
pub fn default_torrent_no_peers_timeout_secs() -> u64 {
    30
}

/// Where rqbit stores the streaming/partial files. Must exist before the
/// engine starts (rqbit requires the download folder to exist). Round 23:
/// the s2udio cache dir, not the legacy rmpc one.
pub fn default_torrent_cache_dir() -> std::path::PathBuf {
    "~/.cache/s2udio/torrents".into()
}

/// jellytui's config file, which holds the Jellyfin server URL, access
/// token and user id the Jellyfin tab reuses.
pub fn default_jellyfin_config_file() -> String {
    "~/.config/jellytui/config.toml".to_owned()
}

pub fn default_playing_label() -> String {
    "Playing".to_string()
}

pub fn default_paused_label() -> String {
    "Paused".to_string()
}

pub fn default_stopped_label() -> String {
    "Stopped".to_string()
}

pub fn default_on_label() -> String {
    "On".to_string()
}

pub fn default_off_label() -> String {
    "Off".to_string()
}

pub fn default_oneshot_label() -> String {
    "OS".to_string()
}

pub fn default_song_sort() -> Vec<SongPropertyFile> {
    vec![
        SongPropertyFile::Disc,
        SongPropertyFile::Track,
        SongPropertyFile::Artist,
        SongPropertyFile::Title,
    ]
}

pub fn playlist_symbol() -> String {
    "P".to_owned()
}

pub fn default_thousands_separator() -> String {
    ",".to_string()
}

pub fn rating_options() -> Vec<i32> {
    vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
}

pub fn components() -> HashMap<String, PaneOrSplitFile> {
    HashMap::from([
        (
            "state".to_string(),
            PaneOrSplitFile::Pane(PaneTypeFile::Property {
                align: Alignment::Left,
                scroll_speed: 0,
                content: vec![
                    PropertyFile {
                        kind: PropertyKindFileOrText::Text("[".to_string()),
                        style: Some(StyleFile {
                            fg: Some("yellow".to_string()),
                            bg: None,
                            modifiers: Some(Modifiers::Bold),
                        }),
                        default: None,
                    },
                    PropertyFile {
                        kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                            StatusPropertyFile::StateV2 {
                                playing_label: default_playing_label(),
                                paused_label: default_paused_label(),
                                stopped_label: default_stopped_label(),
                                playing_style: None,
                                paused_style: None,
                                stopped_style: None,
                            },
                        )),
                        style: Some(StyleFile {
                            fg: Some("yellow".to_string()),
                            bg: None,
                            modifiers: Some(Modifiers::Bold),
                        }),
                        default: None,
                    },
                    PropertyFile {
                        kind: PropertyKindFileOrText::Text("]".to_string()),
                        style: Some(StyleFile {
                            fg: Some("yellow".to_string()),
                            bg: None,
                            modifiers: Some(Modifiers::Bold),
                        }),
                        default: None,
                    },
                ],
            }),
        ),
        (
            "elapsed_and_bitrate".to_string(),
            PaneOrSplitFile::Pane(PaneTypeFile::Property {
                align: Alignment::Left,
                scroll_speed: 0,
                content: vec![
                    PropertyFile {
                        kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                            StatusPropertyFile::Elapsed,
                        )),
                        style: None,
                        default: None,
                    },
                    PropertyFile {
                        kind: PropertyKindFileOrText::Text(" / ".to_string()),
                        style: None,
                        default: None,
                    },
                    PropertyFile {
                        kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                            StatusPropertyFile::Duration,
                        )),
                        style: None,
                        default: None,
                    },
                    PropertyFile {
                        kind: PropertyKindFileOrText::Group(vec![
                            PropertyFile {
                                kind: PropertyKindFileOrText::Text(" (".to_string()),
                                style: None,
                                default: None,
                            },
                            PropertyFile {
                                kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                                    StatusPropertyFile::Bitrate,
                                )),
                                style: None,
                                default: None,
                            },
                            PropertyFile {
                                kind: PropertyKindFileOrText::Text(" kbps)".to_string()),
                                style: None,
                                default: None,
                            },
                        ]),
                        style: None,
                        default: None,
                    },
                ],
            }),
        ),
        (
            "title".to_string(),
            PaneOrSplitFile::Pane(PaneTypeFile::Property {
                align: Alignment::Center,
                scroll_speed: 1,
                content: vec![PropertyFile {
                    kind: PropertyKindFileOrText::Property(PropertyKindFile::Song(
                        SongPropertyFile::Title,
                    )),
                    style: Some(StyleFile { fg: None, bg: None, modifiers: Some(Modifiers::Bold) }),
                    default: Some(Box::new(PropertyFile {
                        kind: PropertyKindFileOrText::Text("No Playback".to_string()),
                        style: Some(StyleFile {
                            fg: None,
                            bg: None,
                            modifiers: Some(Modifiers::Bold),
                        }),
                        default: None,
                    })),
                }],
            }),
        ),
        (
            "artist_and_album".to_string(),
            PaneOrSplitFile::Pane(PaneTypeFile::Property {
                align: Alignment::Center,
                scroll_speed: 1,
                content: vec![
                    PropertyFile {
                        kind: PropertyKindFileOrText::Property(PropertyKindFile::Song(
                            SongPropertyFile::Artist,
                        )),
                        style: Some(StyleFile {
                            fg: Some("yellow".to_string()),
                            bg: None,
                            modifiers: Some(Modifiers::Bold),
                        }),
                        default: Some(Box::new(PropertyFile {
                            kind: PropertyKindFileOrText::Text("Unknown".to_string()),
                            style: Some(StyleFile {
                                fg: Some("yellow".to_string()),
                                bg: None,
                                modifiers: Some(Modifiers::Bold),
                            }),
                            default: None,
                        })),
                    },
                    PropertyFile {
                        kind: PropertyKindFileOrText::Text(" - ".to_string()),
                        style: None,
                        default: None,
                    },
                    PropertyFile {
                        kind: PropertyKindFileOrText::Property(PropertyKindFile::Song(
                            SongPropertyFile::Album,
                        )),
                        style: None,
                        default: Some(Box::new(PropertyFile {
                            kind: PropertyKindFileOrText::Text("Unknown Album".to_string()),
                            style: None,
                            default: None,
                        })),
                    },
                ],
            }),
        ),
        ("volume".to_string(), PaneOrSplitFile::Split {
            direction: DirectionFile::Horizontal,
            borders: BordersFile::NONE,
            panes: vec![
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "1".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::Property {
                        align: Alignment::Left,
                        scroll_speed: 0,
                        content: vec![PropertyFile {
                            kind: PropertyKindFileOrText::Text(String::new()),
                            style: None,
                            default: None,
                        }],
                    }),
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
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::Volume {
                        kind: VolumeTypeFile::Slider(VolumeSliderConfigFile {
                            symbols: crate::config::theme::volume_slider::Symbols {
                                start: None,
                                filled: "─".to_string(),
                                thumb: "●".to_string(),
                                track: "─".to_string(),
                                end: None,
                            },
                            track_style: None,
                            filled_style: None,
                            thumb_style: None,
                        }),
                    }),
                },
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "3".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::Property {
                        align: Alignment::Right,
                        scroll_speed: 0,
                        content: vec![PropertyFile {
                            kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                                StatusPropertyFile::Volume,
                            )),
                            style: Some(StyleFile {
                                fg: Some("blue".to_string()),
                                bg: None,
                                modifiers: None,
                            }),
                            default: None,
                        }],
                    }),
                },
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "2".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::Property {
                        align: Alignment::Left,
                        scroll_speed: 0,
                        content: vec![PropertyFile {
                            kind: PropertyKindFileOrText::Text("%".to_string()),
                            style: Some(StyleFile {
                                fg: Some("blue".to_string()),
                                bg: None,
                                modifiers: None,
                            }),
                            default: None,
                        }],
                    }),
                },
            ],
        }),
        (
            "input_mode".to_string(),
            PaneOrSplitFile::Pane(PaneTypeFile::Property {
                align: Alignment::Center,
                scroll_speed: 0,
                content: vec![PropertyFile {
                    kind: PropertyKindFileOrText::Transform(TransformFile::Replace {
                        content: Box::new(PropertyFile {
                            kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                                StatusPropertyFile::InputMode(),
                            )),
                            style: None,
                            default: None,
                        }),
                        replacements: vec![
                            ReplacementFile {
                                r#match: "Normal".to_string(),
                                replace: PropertyFile {
                                    kind: PropertyKindFileOrText::Text(" NORMAL ".to_string()),
                                    style: Some(StyleFile {
                                        fg: Some("black".to_string()),
                                        bg: Some("blue".to_string()),
                                        modifiers: None,
                                    }),
                                    default: None,
                                },
                            },
                            ReplacementFile {
                                r#match: "Insert".to_string(),
                                replace: PropertyFile {
                                    kind: PropertyKindFileOrText::Text(" INSERT ".to_string()),
                                    style: Some(StyleFile {
                                        fg: Some("black".to_string()),
                                        bg: Some("green".to_string()),
                                        modifiers: None,
                                    }),
                                    default: None,
                                },
                            },
                        ],
                    }),
                    style: None,
                    default: None,
                }],
            }),
        ),
        ("states".to_string(), PaneOrSplitFile::Split {
            direction: DirectionFile::Horizontal,
            borders: BordersFile::NONE,
            panes: vec![
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "1".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::Empty()),
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
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::Property {
                        align: Alignment::Left,
                        scroll_speed: 0,
                        content: vec![PropertyFile {
                            kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                                StatusPropertyFile::InputBuffer(),
                            )),
                            style: Some(StyleFile {
                                fg: Some("blue".to_string()),
                                bg: None,
                                modifiers: None,
                            }),
                            default: None,
                        }],
                    }),
                },
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "6".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::Property {
                        align: Alignment::Right,
                        scroll_speed: 0,
                        content: vec![
                            PropertyFile {
                                kind: PropertyKindFileOrText::Text("[".to_string()),
                                style: Some(StyleFile {
                                    fg: Some("blue".to_string()),
                                    bg: None,
                                    modifiers: Some(Modifiers::Bold),
                                }),
                                default: None,
                            },
                            PropertyFile {
                                kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                                    StatusPropertyFile::RepeatV2 {
                                        on_label: "z".to_string(),
                                        off_label: "z".to_string(),
                                        on_style: Some(StyleFile {
                                            fg: Some("yellow".to_string()),
                                            bg: None,
                                            modifiers: Some(Modifiers::Bold),
                                        }),
                                        off_style: Some(StyleFile {
                                            fg: Some("blue".to_string()),
                                            bg: None,
                                            modifiers: Some(Modifiers::Dim),
                                        }),
                                    },
                                )),
                                style: None,
                                default: None,
                            },
                            PropertyFile {
                                kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                                    StatusPropertyFile::RandomV2 {
                                        on_label: "x".to_string(),
                                        off_label: "x".to_string(),
                                        on_style: Some(StyleFile {
                                            fg: Some("yellow".to_string()),
                                            bg: None,
                                            modifiers: Some(Modifiers::Bold),
                                        }),
                                        off_style: Some(StyleFile {
                                            fg: Some("blue".to_string()),
                                            bg: None,
                                            modifiers: Some(Modifiers::Dim),
                                        }),
                                    },
                                )),
                                style: None,
                                default: None,
                            },
                            PropertyFile {
                                kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                                    StatusPropertyFile::ConsumeV2 {
                                        on_label: "c".to_string(),
                                        off_label: "c".to_string(),
                                        oneshot_label: "c".to_string(),
                                        on_style: Some(StyleFile {
                                            fg: Some("yellow".to_string()),
                                            bg: None,
                                            modifiers: Some(Modifiers::Bold),
                                        }),
                                        off_style: Some(StyleFile {
                                            fg: Some("blue".to_string()),
                                            bg: None,
                                            modifiers: Some(Modifiers::Dim),
                                        }),
                                        oneshot_style: Some(StyleFile {
                                            fg: Some("red".to_string()),
                                            bg: None,
                                            modifiers: Some(Modifiers::Dim),
                                        }),
                                    },
                                )),
                                style: None,
                                default: None,
                            },
                            PropertyFile {
                                kind: PropertyKindFileOrText::Property(PropertyKindFile::Status(
                                    StatusPropertyFile::SingleV2 {
                                        on_label: "v".to_string(),
                                        off_label: "v".to_string(),
                                        oneshot_label: "v".to_string(),
                                        on_style: Some(StyleFile {
                                            fg: Some("yellow".to_string()),
                                            bg: None,
                                            modifiers: Some(Modifiers::Bold),
                                        }),
                                        off_style: Some(StyleFile {
                                            fg: Some("blue".to_string()),
                                            bg: None,
                                            modifiers: Some(Modifiers::Dim),
                                        }),
                                        oneshot_style: Some(StyleFile {
                                            fg: Some("red".to_string()),
                                            bg: None,
                                            modifiers: Some(Modifiers::Bold),
                                        }),
                                    },
                                )),
                                style: None,
                                default: None,
                            },
                            PropertyFile {
                                kind: PropertyKindFileOrText::Text("]".to_string()),
                                style: Some(StyleFile {
                                    fg: Some("blue".to_string()),
                                    bg: None,
                                    modifiers: Some(Modifiers::Bold),
                                }),
                                default: None,
                            },
                        ],
                    }),
                },
            ],
        }),
        ("header_left".to_string(), PaneOrSplitFile::Split {
            direction: DirectionFile::Vertical,
            borders: BordersFile::NONE,
            panes: vec![
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "1".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Component("state".to_string()),
                },
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "1".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Component("elapsed_and_bitrate".to_string()),
                },
            ],
        }),
        ("header_center".to_string(), PaneOrSplitFile::Split {
            direction: DirectionFile::Vertical,
            borders: BordersFile::NONE,
            panes: vec![
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "1".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Component("title".to_string()),
                },
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "1".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Component("artist_and_album".to_string()),
                },
            ],
        }),
        ("header_right".to_string(), PaneOrSplitFile::Split {
            direction: DirectionFile::Vertical,
            borders: BordersFile::NONE,
            panes: vec![
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "1".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Component("volume".to_string()),
                },
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "1".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Component("states".to_string()),
                },
            ],
        }),
        ("progress_bar".to_string(), PaneOrSplitFile::Split {
            direction: DirectionFile::Horizontal,
            borders: BordersFile::NONE,
            panes: vec![
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "1".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::Empty()),
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
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::ProgressBar),
                },
                SubPaneFile {

                        collapse_below: None, shrink_below: None, window_sizes: Vec::new(),size: "1".to_string(),
                    background_color: None,
                    borders: BordersFile::NONE,
                    border_style: None,
                    border_active_style: None,
                    border_title: Vec::new(),
                    border_title_position: BorderTitlePosition::Top,
                    border_title_alignment: Alignment::Left,
                    border_symbols: BorderSymbolsFile::default(),
                    pane: PaneOrSplitFile::Pane(PaneTypeFile::Empty()),
                },
            ],
        }),
    ])
}
