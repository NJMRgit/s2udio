use serde::{Deserialize, Serialize};
use strum::Display;

use super::Size;
use crate::{mpd::mpd_client::AlbumArtOrder, shared::terminal::ImageBackend};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
#[serde(default)]
pub struct AlbumArtConfigFile {
    pub method: ImageMethodFile,
    pub order: AlbumArtOrderFile,
    pub max_size_px: Size,
    pub disabled_protocols: Vec<String>,
    pub vertical_align: VerticalAlignFile,
    pub horizontal_align: HorizontalAlignFile,
    /// Trim the letterbox bars off YouTube stream thumbnails. Defaults **on**:
    /// `hqdefault.jpg` is a 4:3 letterbox of a 16:9 picture, so without the
    /// crop the cover sits in a 16:9 box with black bars top and bottom.
    /// Library artwork is never cropped — only the stream-thumbnail path.
    pub crop_letterbox: bool,
}

impl Default for AlbumArtConfigFile {
    fn default() -> Self {
        Self {
            method: ImageMethodFile::default(),
            order: AlbumArtOrderFile::default(),
            max_size_px: Size::default(),
            disabled_protocols: vec!["http://".to_string(), "https://".to_string()],
            vertical_align: VerticalAlignFile::default(),
            horizontal_align: HorizontalAlignFile::default(),
            crop_letterbox: true,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct AlbumArtConfig {
    pub method: ImageMethod,
    pub order: AlbumArtOrder,
    pub max_size_px: Size,
    pub disabled_protocols: Vec<String>,
    pub vertical_align: VerticalAlign,
    pub horizontal_align: HorizontalAlign,
    pub crop_letterbox: bool,
}

#[derive(Default, Display, Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum HorizontalAlignFile {
    Left,
    #[default]
    Center,
    Right,
}
#[derive(Default, Display, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizontalAlign {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Default, Display, Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum AlbumArtOrderFile {
    #[default]
    EmbeddedFirst,
    FileFirst,
    EmbeddedOnly,
    FileOnly,
}

#[derive(Default, Display, Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum VerticalAlignFile {
    Top,
    #[default]
    Center,
    Bottom,
}
#[derive(Default, Display, Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalAlign {
    Top,
    #[default]
    Center,
    Bottom,
}

#[derive(Default, Display, Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum ImageMethodFile {
    Kitty,
    UeberzugWayland,
    UeberzugX11,
    Iterm2,
    Sixel,
    Block,
    None,
    #[default]
    Auto,
}

#[derive(Default, Display, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageMethod {
    Kitty,
    UeberzugWayland,
    UeberzugX11,
    Iterm2,
    Sixel,
    None,
    #[default]
    Block,
}

impl From<AlbumArtConfigFile> for AlbumArtConfig {
    fn from(value: AlbumArtConfigFile) -> Self {
        let size = value.max_size_px;
        AlbumArtConfig {
            method: ImageMethod::default(),
            order: match value.order {
                AlbumArtOrderFile::EmbeddedFirst => AlbumArtOrder::EmbeddedFirst,
                AlbumArtOrderFile::FileFirst => AlbumArtOrder::FileFirst,
                AlbumArtOrderFile::EmbeddedOnly => AlbumArtOrder::EmbeddedOnly,
                AlbumArtOrderFile::FileOnly => AlbumArtOrder::FileOnly,
            },
            max_size_px: Size {
                width: if size.width == 0 { u16::MAX } else { size.width },
                height: if size.height == 0 { u16::MAX } else { size.height },
            },
            disabled_protocols: value.disabled_protocols,
            vertical_align: value.vertical_align.into(),
            horizontal_align: value.horizontal_align.into(),
            crop_letterbox: value.crop_letterbox,
        }
    }
}

impl From<VerticalAlignFile> for VerticalAlign {
    fn from(value: VerticalAlignFile) -> Self {
        match value {
            VerticalAlignFile::Top => VerticalAlign::Top,
            VerticalAlignFile::Center => VerticalAlign::Center,
            VerticalAlignFile::Bottom => VerticalAlign::Bottom,
        }
    }
}

impl From<HorizontalAlignFile> for HorizontalAlign {
    fn from(value: HorizontalAlignFile) -> Self {
        match value {
            HorizontalAlignFile::Left => HorizontalAlign::Left,
            HorizontalAlignFile::Center => HorizontalAlign::Center,
            HorizontalAlignFile::Right => HorizontalAlign::Right,
        }
    }
}

impl From<ImageBackend> for ImageMethod {
    fn from(value: ImageBackend) -> Self {
        match value {
            ImageBackend::Kitty => ImageMethod::Kitty,
            ImageBackend::Iterm2 => ImageMethod::Iterm2,
            ImageBackend::Sixel => ImageMethod::Sixel,
            ImageBackend::UeberzugWayland => ImageMethod::UeberzugWayland,
            ImageBackend::UeberzugX11 => ImageMethod::UeberzugX11,
            ImageBackend::Block => ImageMethod::Block,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AlbumArtConfig, AlbumArtConfigFile};

    /// The crop defaults **on**: every existing config keeps the new
    /// behaviour without an edit, which is what the user asked for.
    #[test]
    fn crop_letterbox_defaults_on() {
        assert!(AlbumArtConfigFile::default().crop_letterbox);
        let config: AlbumArtConfig = AlbumArtConfigFile::default().into();
        assert!(config.crop_letterbox);
    }

    /// An older `config.ron` with no `crop_letterbox` key still parses and
    /// picks up the on default (`#[serde(default)]` on the struct).
    #[test]
    fn an_absent_field_in_an_older_config_defaults_on() {
        let parsed: AlbumArtConfigFile =
            ron::from_str("(order: EmbeddedFirst, max_size_px: (width: 0, height: 0))")
                .expect("an older config parses");
        assert!(parsed.crop_letterbox, "a missing key means on");
    }

    /// The option can be switched off, and it survives the conversion to the
    /// runtime config the panes read.
    #[test]
    fn crop_letterbox_can_be_switched_off() {
        let parsed: AlbumArtConfigFile =
            ron::from_str("(crop_letterbox: false)").expect("parses");
        assert!(!parsed.crop_letterbox);
        let config: AlbumArtConfig = parsed.into();
        assert!(!config.crop_letterbox, "off reaches the runtime config");
    }
}
