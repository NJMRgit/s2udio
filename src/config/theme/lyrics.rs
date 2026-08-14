use anyhow::Result;
use ratatui::style::Style;
use serde::{Deserialize, Serialize};

use super::style::{StyleFile, ToConfigOr};

#[derive(Debug, Default, Clone)]
pub struct LyricsConfig {
    pub timestamp: bool,
    pub word_highlight: bool,
    /// Style of the per-word timings shown in lyrics edit mode (round 34).
    /// `None` = the lyrics text style with the DIM modifier.
    pub edit_timing: Option<Style>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LyricsConfigFile {
    #[serde(default)]
    pub(super) timestamp: bool,
    /// Karaoke-style word-by-word highlighting of the current line
    /// (defaults to on).
    #[serde(default = "default_word_highlight")]
    pub(super) word_highlight: bool,
    /// Style of the per-word timings shown in lyrics edit mode (round 34).
    /// `None` = the lyrics text style with the DIM modifier.
    #[serde(default)]
    pub(super) edit_timing: Option<StyleFile>,
}

impl Default for LyricsConfigFile {
    fn default() -> Self {
        Self { timestamp: false, word_highlight: true, edit_timing: None }
    }
}

fn default_word_highlight() -> bool {
    true
}

impl TryFrom<LyricsConfigFile> for LyricsConfig {
    type Error = anyhow::Error;

    fn try_from(value: LyricsConfigFile) -> Result<Self> {
        Ok(LyricsConfig {
            timestamp: value.timestamp,
            word_highlight: value.word_highlight,
            edit_timing: value
                .edit_timing
                .map(|style| style.to_config_or(None, None))
                .transpose()?,
        })
    }
}
