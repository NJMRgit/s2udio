use serde::{Deserialize, Serialize};

use super::defaults;

/// Internet radio configuration.
///
/// The Radio tab shows three kinds of stations:
/// - **Favourites**: the user's curated stations, stored in a local m3u
///   under the s2udio config dir (`~/.config/s2udio/radio/<playlist>.m3u`,
///   default `radio`). The pane reads and rewrites the file directly in
///   EXTINF format so every station keeps its name. Deliberately NOT an MPD
///   stored playlist — rmpc's playlist UI would otherwise show a playlist it
///   doesn't understand (setup.sh migrates an existing MPD-side file).
/// - **Local**: the closest stations to the user's location, fetched from the
///   radio-browser.info directory. The location comes from the config or is
///   geo-IP detected.
/// - **Geographic**: top stations of the directory grouped by country.
#[derive(Debug, Clone, PartialEq)]
pub struct Radio {
    /// Name of the MPD stored playlist that holds the favourite stations.
    pub playlist: String,
    /// Where the "Local" section is centered. `None` = auto-detect via geo-IP.
    pub location: Option<GeoLocation>,
    /// Upper bound of how many favourites are shown at the top of the list.
    pub max_favourites: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeoLocation {
    pub lat: f64,
    pub lon: f64,
    /// Human-readable label, e.g. "Berlin, DE". Shown in the Local header.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct RadioFile {
    pub playlist: Option<String>,
    pub location: Option<GeoLocation>,
    pub max_favourites: Option<usize>,
}

impl From<RadioFile> for Radio {
    fn from(value: RadioFile) -> Self {
        Self {
            playlist: value.playlist.unwrap_or_else(defaults::default_radio_playlist),
            location: value.location,
            max_favourites: value.max_favourites.unwrap_or_else(defaults::default_max_favourites),
        }
    }
}
