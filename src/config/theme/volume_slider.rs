use anyhow::Result;
use ratatui::style::{Color, Style};
use serde::{Deserialize, Serialize};
use super::{StyleFile, style::ToConfigOr};
#[derive(Debug, Default, Clone, Hash, Eq, PartialEq)]
pub struct VolumeSliderConfig {
    /// Symbols for the volume slider
    /// First symbol is used for the start boundary of the volume slider
    /// Second symbol is used for the filled part of the volume slider
    /// Third symbol is used for the thumb
    /// Fourth symbol is used for the empty part of the volume slider
    /// Fifth symbol is used for the end boundary of the volume slider
    pub symbols: Symbols,
    /// Style for the filled part of the volume slider
    /// Falls back to blue for foreground and default color for background
    pub filled_style: Style,
    /// Thumb at the end of the filled part of the volume slider
    /// Falls back to blue for foreground and default color for background
    pub thumb_style: Style,
    /// Style for the empty part of the volume slider
    /// Falls back to gray for foreground and default color for background
    pub track_style: Style,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeSliderConfigFile {
    #[serde(default)]
    pub symbols: Symbols,
    pub track_style: Option<StyleFile>,
    pub filled_style: Option<StyleFile>,
    pub thumb_style: Option<StyleFile>,
}
#[derive(Debug, Clone, Hash, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbols {
    pub start: Option<String>,
    pub filled: String,
    pub thumb: String,
    pub track: String,
    pub end: Option<String>,
}
impl Default for Symbols {
    fn default() -> Symbols {
        Symbols {
            start: Some("♪".to_owned()),
            filled: "─".to_owned(),
            thumb: "●".to_owned(),
            track: "─".to_owned(),
            end: Some("♫".to_owned()),
        }
    }
}
impl Default for VolumeSliderConfigFile {
    fn default() -> Self {
        Self {
            symbols: Symbols::default(),
            filled_style: Some(StyleFile {
                fg: Some("blue".to_string()),
                bg: None,
                modifiers: None,
            }),
            thumb_style: Some(StyleFile {
                fg: Some("blue".to_string()),
                bg: None,
                modifiers: None,
            }),
            track_style: Some(StyleFile {
                fg: Some("dark_gray".to_string()),
                bg: None,
                modifiers: None,
            }),
        }
    }
}
impl VolumeSliderConfigFile {
    pub fn into_config(self) -> Result<VolumeSliderConfig> {
        Ok(VolumeSliderConfig {
            symbols: self.symbols,
            filled_style: self.filled_style.to_config_or(Some(Color::Blue), None)?,
            thumb_style: self.thumb_style.to_config_or(Some(Color::Blue), None)?,
            track_style: self.track_style.to_config_or(Some(Color::DarkGray), None)?,
        })
    }
}
