use std::num::{ParseFloatError, ParseIntError};

use anyhow::{Context, Result, bail};
use itertools::Itertools;
use ratatui::layout::Constraint;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use thiserror::Error;

use super::{
    StyleFile,
    properties::{
        Alignment, Property, PropertyFile, PropertyKindFileOrText, PropertyKindOrText,
        SongProperty, SongPropertyFile, Transform, TransformFile,
    },
    style::ToConfigOr,
};

#[derive(Debug, Clone, Copy)]
pub enum PercentOrLength {
    Percent(u16),
    Length(u16),
    Ratio(f64),
}

impl PercentOrLength {
    pub fn into_constraint(self, parent_size: u16) -> Constraint {
        match self {
            PercentOrLength::Percent(val) => Constraint::Percentage(val),
            PercentOrLength::Length(val) => Constraint::Length(val),
            PercentOrLength::Ratio(val) => {
                Constraint::Length((f64::from(parent_size) * val).round() as u16)
            }
        }
    }
}

#[derive(Error, Debug)]
pub enum ParseSizeError {
    #[error("Invalid size format: '{0}'")]
    ParseIntError(#[from] ParseIntError),
    #[error("Invalid size format: '{0}'")]
    ParseFloatError(#[from] ParseFloatError),
}

impl std::str::FromStr for PercentOrLength {
    type Err = ParseSizeError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.ends_with('r') {
            Ok(PercentOrLength::Ratio(s.trim_end_matches('r').parse()?))
        } else if s.ends_with('%') {
            Ok(PercentOrLength::Percent(s.trim_end_matches('%').parse()?))
        } else {
            Ok(PercentOrLength::Length(s.parse()?))
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SongTableColumnFile {
    /// Property to display in the column
    /// Can be one of: `Duration`, `Filename`, `Artist`, `AlbumArtist`, `Title`,
    /// `Album`, `Date`, `Genre` or `Comment`
    pub(super) prop: PropertyFile<SongPropertyFile>,
    /// Label to display in the column header
    /// If not set, the property name will be used
    pub(super) label: Option<String>,
    pub(super) label_prop: Option<PropertyFile<SongPropertyFile>>,
    /// Width of the column in percent
    pub(super) width_percent: Option<u16>,
    pub(super) width: Option<String>,
    /// Text alignment of the text in the column
    pub(super) alignment: Option<Alignment>,
}

#[derive(Debug, Clone)]
pub struct SongTableColumn {
    pub prop: Property<SongProperty>,
    pub label: Property<SongProperty>,
    pub width: PercentOrLength,
    pub alignment: Alignment,
}

#[derive(Debug)]
pub(super) struct QueueTableColumns(pub Vec<SongTableColumn>);

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct QueueTableColumnsFile(pub Vec<SongTableColumnFile>);

impl Default for QueueTableColumnsFile {
    fn default() -> Self {
        QueueTableColumnsFile(vec![
            SongTableColumnFile {
                prop: PropertyFile {
                    kind: PropertyKindFileOrText::Property(SongPropertyFile::Title),
                    default: Some(Box::new(PropertyFile {
                        kind: PropertyKindFileOrText::Text("Unknown".to_string()),
                        style: Some(StyleFile {
                            fg: Some("#d0d0d0".to_string()),
                            bg: None,
                            modifiers: None,
                        }),
                        default: None,
                    })),
                    style: Some(StyleFile {
                        fg: Some("#d0d0d0".to_string()),
                        bg: None,
                        modifiers: None,
                    }),
                },
                label: None,
                label_prop: Some(PropertyFile {
                    kind: PropertyKindFileOrText::Text("Title".to_string()),
                    default: None,
                    style: None,
                }),
                width_percent: None,
                width: Some("50%".to_string()),
                alignment: None,
            },
            // **Artist and Album share one column, `ARTIST - ALBUM`.**
            // Two separate columns spent a third of the row on an Album cell
            // that is empty on exactly the content that needs the room: a web
            // stream has a channel and no album, so its album cell was blank
            // while its title was truncated. The Title leads the row (the
            // column everyone scans for) and takes the freed space.
            //
            // A group's default only applies when one of its **parts** has no
            // value (`Song::as_line` falls back per part), so the defaults sit
            // on the parts — and a part without a style converts to
            // `Style::default()`, so every colour below is set explicitly.
            //
            // **Brightness (2026-10-20, user):** the Title and the Duration are
            // the cells you scan a queue for and carry the bright tone
            // (`#d0d0d0`); the Artist - Album group is deliberately
            // desaturated to the queue's original grey (`#8f8f8f`) so the two
            // read as secondary.
            SongTableColumnFile {
                prop: PropertyFile {
                    kind: PropertyKindFileOrText::Group(vec![
                        PropertyFile {
                            kind: PropertyKindFileOrText::Property(
                                SongPropertyFile::Artist,
                            ),
                            default: Some(Box::new(PropertyFile {
                                kind: PropertyKindFileOrText::Text("Unknown".to_string()),
                                style: Some(StyleFile {
                                    fg: Some("#8f8f8f".to_string()),
                                    bg: None,
                                    modifiers: None,
                                }),
                                default: None,
                            })),
                            style: Some(StyleFile {
                                fg: Some("#8f8f8f".to_string()),
                                bg: None,
                                modifiers: None,
                            }),
                        },
                        PropertyFile {
                            kind: PropertyKindFileOrText::Text(" - ".to_string()),
                            default: None,
                            style: None,
                        },
                        PropertyFile {
                            kind: PropertyKindFileOrText::Property(
                                SongPropertyFile::Album,
                            ),
                            default: Some(Box::new(PropertyFile {
                                kind: PropertyKindFileOrText::Text("Unknown Album".to_string()),
                                style: Some(StyleFile {
                                    fg: Some("#8f8f8f".to_string()),
                                    bg: None,
                                    modifiers: None,
                                }),
                                default: None,
                            })),
                            style: Some(StyleFile {
                                fg: Some("#8f8f8f".to_string()),
                                bg: None,
                                modifiers: None,
                            }),
                        },
                    ]),
                    default: None,
                    style: None,
                },
                label: None,
                label_prop: Some(PropertyFile {
                    kind: PropertyKindFileOrText::Text("Artist - Album".to_string()),
                    default: None,
                    style: None,
                }),
                width_percent: None,
                width: Some("35%".to_string()),
                alignment: None,
            },
            SongTableColumnFile {
                prop: PropertyFile {
                    kind: PropertyKindFileOrText::Property(SongPropertyFile::Duration),
                    default: Some(Box::new(PropertyFile {
                        kind: PropertyKindFileOrText::Text("-".to_string()),
                        style: Some(StyleFile {
                            fg: Some("#d0d0d0".to_string()),
                            bg: None,
                            modifiers: None,
                        }),
                        default: None,
                    })),
                    style: Some(StyleFile {
                        fg: Some("#d0d0d0".to_string()),
                        bg: None,
                        modifiers: None,
                    }),
                },
                label: None,
                label_prop: Some(PropertyFile {
                    kind: PropertyKindFileOrText::Text("Duration".to_string()),
                    default: None,
                    style: None,
                }),
                width_percent: None,
                width: Some("15%".to_string()),
                alignment: Some(Alignment::Right),
            },
        ])
    }
}

impl TryFrom<QueueTableColumnsFile> for QueueTableColumns {
    type Error = anyhow::Error;

    fn try_from(value: QueueTableColumnsFile) -> Result<Self, Self::Error> {
        Ok(QueueTableColumns(
            value
                .0
                .into_iter()
                .map(|v| -> Result<_> {
                    let prop: Property<SongProperty> = v.prop.convert()?;

                    let text = |label: String| Property {
                        kind: PropertyKindOrText::Text(label),
                        default: None,
                        style: None,
                    };

                    let label = match (v.label, v.label_prop) {
                        (Some(_), Some(new)) | (None, Some(new)) => {
                            new.convert_text_group_only()?
                        }
                        (Some(old), None) => text(old.clone()),
                        (None, None) => match &prop.kind {
                            PropertyKindOrText::Text { .. } => text(String::new()),
                            PropertyKindOrText::Sticker { .. } => text(String::new()),
                            PropertyKindOrText::Transform { .. } => text(String::new()),
                            PropertyKindOrText::Property(prop) => text(prop.to_string()),
                            PropertyKindOrText::Group(_) => text(String::new()),
                        },
                    };

                    Ok(SongTableColumn {
                        prop,
                        label,
                        width: v
                            .width
                            .as_ref()
                            .map_or_else(
                                || -> Result<Option<PercentOrLength>> {
                                    Ok(v.width_percent.map(PercentOrLength::Percent))
                                },
                                |width| -> Result<Option<PercentOrLength>> {
                                    match width.parse() {
                                        Ok(PercentOrLength::Ratio(_)) => {
                                            bail!("song_table_format cannot contain ratio widths.")
                                        }
                                        Ok(val) => Ok(Some(val)),
                                        Err(err) => {
                                            bail!("Invalid width format: '{width}'. Error: {err}")
                                        }
                                    }
                                },
                            )
                            .context("Failed to parse width in song table column width")?
                            .context(
                                "Invalid width config. Song table column width must be specified",
                            )?,
                        alignment: v.alignment.unwrap_or(Alignment::Left),
                    })
                })
                .try_collect()?,
        ))
    }
}

impl PropertyFile<SongPropertyFile> {
    pub fn convert(self) -> Result<Property<SongProperty>> {
        Ok(Property {
            kind: match self.kind {
                PropertyKindFileOrText::Text(value) => PropertyKindOrText::Text(value),
                PropertyKindFileOrText::Transform(TransformFile::Truncate {
                    content,
                    length,
                    from_start,
                }) => PropertyKindOrText::Transform(Transform::Truncate {
                    content: Box::new((*content).convert()?),
                    length,
                    from_start,
                }),
                PropertyKindFileOrText::Transform(TransformFile::Replace {
                    content,
                    replacements,
                }) => PropertyKindOrText::Transform(Transform::Replace {
                    content: Box::new((*content).convert()?),
                    replacements: replacements
                        .into_iter()
                        .map(|r| -> Result<_> { Ok((r.r#match, r.replace.convert()?)) })
                        .try_collect()?,
                }),
                PropertyKindFileOrText::Sticker(value) => PropertyKindOrText::Sticker(value),
                PropertyKindFileOrText::Property(prop) => PropertyKindOrText::Property(prop.into()),
                PropertyKindFileOrText::Group(group) => {
                    let res: Vec<_> = group
                        .into_iter()
                        .map(|p| -> Result<Property<SongProperty>> { p.convert() })
                        .try_collect()?;
                    PropertyKindOrText::Group(res)
                }
            },
            style: Some(self.style.to_config_or(None, None)?),
            default: self
                .default
                .map(|v| -> Result<_> { Ok(Box::new((*v).convert()?)) })
                .transpose()?,
        })
    }

    fn convert_text_group_only(self) -> Result<Property<SongProperty>> {
        Ok(Property {
            kind: match self.kind {
                PropertyKindFileOrText::Transform(_) => {
                    bail!("Transforms are not supported in the label")
                }
                PropertyKindFileOrText::Sticker(_) => {
                    bail!("Stickers are not supported in the label")
                }
                PropertyKindFileOrText::Property(_) => {
                    bail!("Properties are not supported in the label")
                }
                PropertyKindFileOrText::Text(value) => PropertyKindOrText::Text(value),
                PropertyKindFileOrText::Group(group) => {
                    let res: Vec<_> = group
                        .into_iter()
                        .map(|p| -> Result<Property<SongProperty>> { p.convert_text_group_only() })
                        .try_collect()?;
                    PropertyKindOrText::Group(res)
                }
            },
            style: Some(self.style.to_config_or(None, None)?),
            default: self
                .default
                .map(|v| -> Result<_> { Ok(Box::new((*v).convert_text_group_only()?)) })
                .transpose()?,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use ratatui::style::Color;

    use super::*;
    use crate::config::theme::properties::PropertyKindOrText;

    /// The default queue layout is Title | Artist - Album | Duration: the
    /// title **leads** the row (2026-10-20, user), and the artist and album
    /// share ONE column (which frees the Album column's third of the row for
    /// the title, and leaves no blank album cell on a web stream).
    #[test]
    fn the_default_layout_combines_artist_and_album() {
        let columns = QueueTableColumns::try_from(QueueTableColumnsFile::default())
            .expect("the defaults must parse");
        let labels: Vec<String> = columns
            .0
            .iter()
            .map(|column| column.label.as_string(None, "", Default::default(), &test_ctx())
                .unwrap_or_default())
            .collect();
        assert_eq!(labels, vec!["Title", "Artist - Album", "Duration"]);

        let widths: Vec<String> =
            columns.0.iter().map(|column| format!("{:?}", column.width)).collect();
        assert_eq!(
            widths,
            vec![
                "Percent(50)".to_owned(),
                "Percent(35)".to_owned(),
                "Percent(15)".to_owned()
            ],
            "the title leads and takes the freed space; the combined column and the \
             title split the Album column's old 30%"
        );

        // The second column is the group: artist, ` - `, album.
        let PropertyKindOrText::Group(parts) = &columns.0[1].prop.kind else {
            panic!("the Artist - Album column must be a group");
        };
        let kinds: Vec<String> = parts
            .iter()
            .map(|part| match &part.kind {
                PropertyKindOrText::Property(SongProperty::Artist) => "Artist".to_owned(),
                PropertyKindOrText::Property(SongProperty::Album) => "Album".to_owned(),
                PropertyKindOrText::Text(text) => format!("Text({text})"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(kinds, vec!["Artist", "Text( - )", "Album"]);

        // Each part carries its own missing-value default, because a group's
        // own default never applies while its parts resolve (`Song::as_line`).
        assert!(
            parts[0].default.as_ref().is_some_and(|d| matches!(
                &d.kind,
                PropertyKindOrText::Text(text) if text == "Unknown"
            )),
            "the artist part needs its own Unknown default"
        );
        assert!(
            parts[2].default.as_ref().is_some_and(|d| matches!(
                &d.kind,
                PropertyKindOrText::Text(text) if text == "Unknown Album"
            )),
            "the album part needs its own Unknown Album default"
        );
        // **Brightness roles (2026-10-20, user).** The title and the duration
        // are the scanned cells and carry one bright tone; the artist and the
        // album are secondary and share one desaturated tone. Pinned here so a
        // later default edit cannot quietly flatten the two levels.
        let fg = |column: &SongTableColumn| {
            column.prop.style.and_then(|style| style.fg)
        };
        let title_fg = fg(&columns.0[0]);
        let duration_fg = fg(&columns.0[2]);
        assert_eq!(title_fg, Some(Color::Rgb(0xd0, 0xd0, 0xd0)), "the title is bright");
        assert_eq!(
            duration_fg, title_fg,
            "the duration is as bright as the title — both are scanned cells"
        );
        let artist_fg = parts[0].style.and_then(|style| style.fg);
        let album_fg = parts[2].style.and_then(|style| style.fg);
        assert_eq!(
            artist_fg,
            Some(Color::Rgb(0x8f, 0x8f, 0x8f)),
            "the artist - album group stays desaturated at the queue's original grey"
        );
        assert_eq!(album_fg, artist_fg, "the album matches the artist, not white");
        assert_ne!(
            artist_fg,
            Some(Color::White),
            "the group is desaturated: nothing in it is ANSI white"
        );
        assert_ne!(
            title_fg, artist_fg,
            "the title and the artist must keep two distinct levels"
        );
        // Each cell's missing-value default is the same tone as its value, so
        // `Unknown` never reads brighter than the text beside it.
        assert_eq!(
            parts[0]
                .default
                .as_ref()
                .and_then(|d| d.style)
                .and_then(|style| style.fg),
            artist_fg
        );
        assert_eq!(
            columns.0[0]
                .prop
                .default
                .as_ref()
                .and_then(|d| d.style)
                .and_then(|style| style.fg),
            title_fg
        );
    }

    /// A context is needed to resolve a label only when it is not plain text;
    /// the labels above are text, but `as_string` takes one regardless.
    fn test_ctx() -> crate::ctx::Ctx {
        crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }
}
