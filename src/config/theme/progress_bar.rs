use anyhow::Result;
use ratatui::style::{Color, Style};
use serde::{Deserialize, Serialize};
use super::{StyleFile, style::ToConfigOr};
#[derive(Debug, Default, Clone)]
pub struct ProgressBarConfig {
    /// Symbols for the rogress bar at the bottom of the screen
    /// First symbol is used for the start boundary of the progress bar
    /// Second symbol is used for the elapsed part of the progress bar
    /// Third symbol is used for the thumb
    /// Fourth symbol is used for the remaining part of the progress bar
    /// Fifth symbol is used for the end boundary of the progress bar
    pub symbols: [String; 5],
    /// Fall sback to blue for foreground and black for background
    pub elapsed_style: Style,
    /// Thumb at the end of the elapsed part of the progress bar
    /// Fall sback to blue for foreground and black for background
    pub thumb_style: Style,
    /// Fall sback to black for foreground and default color for background
    /// For transparent track you should set the track symbol to empty string
    pub track_style: Style,
    /// Whether to use only the track symbol and style when the progress is
    /// empty (0%).
    pub use_track_when_empty: bool,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProgressBarConfigFile {
    pub(super) symbols: Vec<String>,
    pub(super) track_style: Option<StyleFile>,
    pub(super) elapsed_style: Option<StyleFile>,
    pub(super) thumb_style: Option<StyleFile>,
    pub(super) use_track_when_empty: bool,
}
impl Default for ProgressBarConfigFile {
    fn default() -> Self {
        Self {
            symbols: vec![
                "█".to_string(), "█".to_string(), "█".to_string(), " ".to_string(),
                "█".to_string(),
            ],
            elapsed_style: Some(StyleFile {
                fg: Some("blue".to_string()),
                bg: None,
                modifiers: None,
            }),
            thumb_style: Some(StyleFile {
                fg: Some("blue".to_string()),
                bg: None,
                modifiers: None,
            }),
            track_style: None,
            use_track_when_empty: true,
        }
    }
}
impl ProgressBarConfigFile {
    pub(super) fn into_config(mut self) -> Result<ProgressBarConfig> {
        if self.symbols.len() == 3 {
            self.symbols.resize(5, String::default());
            let s0 = self.symbols[0].clone();
            let s1 = self.symbols[1].clone();
            let s2 = self.symbols[2].clone();
            let s3 = s2.clone();
            self.symbols[1] = s0;
            self.symbols[2] = s1;
            self.symbols[3] = s2;
            self.symbols[4] = s3;
        }
        let start = std::mem::take(&mut self.symbols[0]);
        let elapsed = std::mem::take(&mut self.symbols[1]);
        let thumb = std::mem::take(&mut self.symbols[2]);
        let track = std::mem::take(&mut self.symbols[3]);
        let end = std::mem::take(&mut self.symbols[4]);
        Ok(ProgressBarConfig {
            symbols: [start, elapsed, thumb, track, end],
            elapsed_style: self.elapsed_style.to_config_or(Some(Color::Blue), None)?,
            thumb_style: self.thumb_style.to_config_or(Some(Color::Blue), None)?,
            track_style: self.track_style.to_config_or(Some(Color::Black), None)?,
            use_track_when_empty: self.use_track_when_empty,
        })
    }
}
