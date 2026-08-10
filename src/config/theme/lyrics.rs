use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone)]
pub struct LyricsConfig {
    pub timestamp: bool,
    pub word_highlight: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LyricsConfigFile {
    #[serde(default)]
    pub(super) timestamp: bool,
    /// Karaoke-style word-by-word highlighting of the current line
    /// (defaults to on).
    #[serde(default = "default_word_highlight")]
    pub(super) word_highlight: bool,
}

impl Default for LyricsConfigFile {
    fn default() -> Self {
        Self { timestamp: false, word_highlight: true }
    }
}

fn default_word_highlight() -> bool {
    true
}

impl From<LyricsConfigFile> for LyricsConfig {
    fn from(value: LyricsConfigFile) -> Self {
        LyricsConfig { timestamp: value.timestamp, word_highlight: value.word_highlight }
    }
}
