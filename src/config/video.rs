use serde::{Deserialize, Serialize};

/// Where video content (Jellyfin movies/episodes, dropped/pasted video
/// files, YouTube links) should play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VideoPlaybackMode {
    /// Ask which player to use every time a video is launched.
    #[default]
    #[serde(rename = "ask")]
    Ask,
    /// Play in an external video player (mpv).
    #[serde(rename = "mpv")]
    Mpv,
    /// Play the audio track through MPD.
    #[serde(rename = "mpd")]
    Mpd,
}

impl VideoPlaybackMode {
    pub const ALL: [Self; 3] = [Self::Ask, Self::Mpv, Self::Mpd];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Mpv => "mpv",
            Self::Mpd => "mpd",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "mpv" => Some(Self::Mpv),
            "mpd" => Some(Self::Mpd),
            _ => None,
        }
    }
}

/// Video playback configuration (how Jellyfin/online video is launched).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Video {
    pub playback: VideoPlaybackMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct VideoFile {
    pub playback: Option<VideoPlaybackMode>,
}

impl From<VideoFile> for Video {
    fn from(value: VideoFile) -> Self {
        Self { playback: value.playback.unwrap_or_default() }
    }
}
