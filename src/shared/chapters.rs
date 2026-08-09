//! Chapter markers of the currently playing track (YouTube videos,
//! Jellyfin movies/episodes, audiobooks and other local files with
//! embedded chapter markers). Shown in the Queue tab via the Queue /
//! Chapters toggle.

/// One chapter marker: a titled range within the track.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Chapter {
    pub title: String,
    pub start_secs: f64,
    pub end_secs: f64,
}

impl Chapter {
    pub fn duration(&self) -> f64 {
        (self.end_secs - self.start_secs).max(0.0)
    }
}
