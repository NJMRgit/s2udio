use anyhow::Result;
use ratatui::style::{Color, Style};
use serde::{Deserialize, Serialize};
use super::{StyleFile, style::ToConfigOr};
#[derive(Debug, Default, Clone)]
pub struct ScrollbarConfig {
    /// Symbols used for the scrollbar
    /// First symbol is used for the scrollbar track
    /// Second symbol is used for the scrollbar thumb
    /// Third symbol is used for the scrollbar up button
    /// Fourth symbol is used for the scrollbar down button
    pub symbols: [String; 4],
    /// Fall sback to border color for foreground and default color for
    /// background
    pub track_style: Style,
    /// Fall sback to border color for foreground and default color for
    /// background
    pub ends_style: Style,
    pub thumb_style: Style,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScrollbarConfigFile {
    pub(super) symbols: Vec<String>,
    pub(super) track_style: Option<StyleFile>,
    pub(super) ends_style: Option<StyleFile>,
    pub(super) thumb_style: Option<StyleFile>,
}
impl Default for ScrollbarConfigFile {
    fn default() -> Self {
        Self {
            symbols: vec![
                "│".to_owned(), "█".to_owned(), "▲".to_owned(), "▼".to_owned()
            ],
            track_style: Some(StyleFile {
                fg: None,
                bg: None,
                modifiers: None,
            }),
            ends_style: Some(StyleFile {
                fg: None,
                bg: None,
                modifiers: None,
            }),
            thumb_style: Some(StyleFile {
                fg: Some("blue".to_string()),
                bg: None,
                modifiers: None,
            }),
        }
    }
}
impl ScrollbarConfigFile {
    pub(super) fn into_config(
        mut self,
        fallback_color: Color,
    ) -> Result<ScrollbarConfig> {
        let sb_track = std::mem::take(&mut self.symbols[0]);
        let sb_thumb = std::mem::take(&mut self.symbols[1]);
        let sb_up = std::mem::take(&mut self.symbols[2]);
        let sb_down = std::mem::take(&mut self.symbols[3]);
        Ok(ScrollbarConfig {
            symbols: [sb_track, sb_thumb, sb_up, sb_down],
            ends_style: self.ends_style.to_config_or(Some(fallback_color), None)?,
            thumb_style: self.thumb_style.to_config_or(Some(Color::Blue), None)?,
            track_style: self.track_style.to_config_or(Some(fallback_color), None)?,
        })
    }
}
