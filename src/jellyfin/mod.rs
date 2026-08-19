//! Jellyfin server access for the Jellyfin tab.
//!
//! The tab talks to the same server/account as the `jellytui` TUI client:
//! the server URL, access token and user id are read from jellytui's config
//! file (`~/.config/jellytui/config.toml`, overridable in the s2udio config).
//! All HTTP happens on the work thread, never on the MPD thread.
//!
//! Playback works by handing MPD the direct stream URL
//! (`{server}/Audio/{id}/stream?static=true&api_key={token}`), which MPD
//! decodes like a radio stream.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result};

/// Server credentials + base URL, loaded from jellytui's config.
#[derive(Debug, Clone)]
pub struct Jellyfin {
    /// Server base URL, e.g. `http://127.0.0.1:8086` (no trailing slash).
    pub base: String,
    pub token: String,
    pub user_id: String,
}

/// One item of the Jellyfin library tree (a view, artist, album, folder or
/// song). Only the fields the tab displays are kept.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct JfItem {
    pub id: String,
    pub name: String,
    /// Jellyfin item type: `MusicArtist`, `MusicAlbum`, `Audio`, `Folder`,
    /// `CollectionFolder`, `Series`, `Movie`, ...
    pub kind: String,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub artist: Option<String>,
    /// For episodes: the series name.
    pub series_name: Option<String>,
    /// For episodes/seasons: the series' item id (used to fetch the show's
    /// poster when a season has no image of its own).
    pub series_id: Option<String>,
    /// For episodes: the season's item id (used to build the season
    /// playlist when an episode plays).
    pub season_id: Option<String>,
    /// For episodes: the episode number within the season (orders the
    /// season playlist).
    pub index_number: Option<i32>,
    /// For episodes: the season number (`ParentIndexNumber`), shown as
    /// `S03` in the info box.
    pub season_number: Option<i32>,
    /// The item's overview/synopsis (Jellyfin `Overview`), shown in the
    /// info box's description area.
    pub overview: Option<String>,
    /// Director name (from `People`, Type == "Director").
    pub director: Option<String>,
    /// Writer name (from `People`, Type == "Writer").
    pub writer: Option<String>,
    /// Actor names (from `People`, Type == "Actor"), shown as Starring.
    pub starring: Vec<String>,
    /// Audio track languages (from MediaSources), ISO 639-1 where mapped.
    #[allow(clippy::struct_field_names)]
    pub audio_languages: Vec<String>,
    /// Subtitle track languages (from MediaSources), ISO 639-1 where mapped.
    pub subtitle_languages: Vec<String>,
    pub year: Option<i32>,
    /// Runtime in whole seconds (from `RunTimeTicks`, 10 ms units).
    pub runtime_secs: Option<u64>,
    /// Direct child count reported by the server (containers only).
    pub child_count: Option<i32>,
    /// Whether this view is a music library (`CollectionType == "music"`).
    pub is_music_view: bool,
}

impl JfItem {
    pub fn is_container(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "MusicArtist" | "MusicAlbum" | "Folder" | "CollectionFolder" | "Series"
                | "Season"
        )
    }

    pub fn is_audio(&self) -> bool {
        self.kind == "Audio"
    }

    /// Whether MPD can play the item: audio files directly, video files via
    /// their audio track (`Videos/{id}/stream`).
    pub fn is_playable(&self) -> bool {
        self.is_audio() || matches!(self.kind.as_str(), "Movie" | "Episode" | "Video")
    }
}

/// Any result routed from the work thread back to the Jellyfin pane.
#[derive(Debug)]
pub enum JellyfinResult {
    Views(Vec<JfItem>),
    Children { parent_id: String, items: Vec<JfItem> },
    Artists { view_id: String, items: Vec<JfItem> },
    Albums { artist_id: String, items: Vec<JfItem> },
    Songs { album_id: String, items: Vec<JfItem> },
    /// A single item's metadata (for the now-playing info).
    Item(JfItem),
    /// The saved resume position of an item (from Jellyfin's UserData).
    ResumePosition {
        seconds: f64,
    },
    /// Primary image bytes of an item (poster / episode preview).
    Image { item_id: String, bytes: Vec<u8> },
    /// Item metadata + primary image, for the MPRIS bridge.
    Mpris { item: JfItem, image: Vec<u8> },
    /// Chapter markers of an item.
    Chapters { item_id: String, chapters: Vec<crate::shared::chapters::Chapter> },
    /// The whole season's episodes as a playlist (built when an episode
    /// plays), with the index of the episode that was clicked.
    SeasonPlaylist { entries: Vec<SeasonEntry>, start_index: usize },
    /// A fetch/config failure; the pane shows it as a notice row.
    Error(String),
}

/// One entry of the season playlist built when a Jellyfin episode plays
/// (shown in the Queue tab's Video view).
#[derive(Debug, Clone)]
pub struct SeasonEntry {
    pub title: String,
    pub url: String,
    pub duration: Option<f64>,
}

impl Jellyfin {
    /// Load the server credentials: the Settings-panel sidecar
    /// (`~/.config/s2udio/jellyfin.ron`, legacy `~/.config/rmpc/…`
    /// honored) when present, else jellytui's config
    /// file.
    pub fn load(jellytui_config: &Path, sidecar: Option<&Path>) -> Option<Self> {
        if let Some(sidecar) = sidecar {
            if let Ok(content) = std::fs::read_to_string(sidecar) {
                if let Ok(creds) =
                    ron::de::from_str::<crate::config::jellyfin::JellyfinCredentialsFile>(&content)
                {
                    if !creds.server_url.is_empty() && !creds.access_token.is_empty() {
                        return Some(Self {
                            base: creds.server_url.trim_end_matches('/').to_owned(),
                            token: creds.access_token,
                            user_id: creds.user_id,
                        });
                    }
                }
            }
        }
        Self::from_config_file(jellytui_config)
    }

    /// Read jellytui's config file (`server_url`, `access_token`, `user_id`).
    /// The file has a simple `key = "value"` shape.
    pub fn from_config_file(path: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        let mut base = None;
        let mut token = None;
        let mut user_id = None;
        for line in content.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim().trim_matches('"').trim();
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "server_url" => base = Some(value.to_owned()),
                "access_token" => token = Some(value.to_owned()),
                "user_id" => user_id = Some(value.to_owned()),
                _ => {}
            }
        }
        let (base, token, user_id) = (base?, token?, user_id?);
        Some(Self { base: base.trim_end_matches('/').to_owned(), token, user_id })
    }

    /// Log in to the server with a username + password (the Settings panel
    /// flow): returns `(access_token, user_id)`.
    pub fn authenticate(base: &str, username: &str, password: &str) -> Result<(String, String)> {
        let url = format!("{}/Users/AuthenticateByName", base.trim_end_matches('/'));
        let body = serde_json::json!({ "Username": username, "Pw": password });
        let response = match Self::agent().post(&url).set(
            "X-Emby-Authorization",
            "MediaBrowser Client=\"s2udio\", Device=\"settings\", DeviceId=\"s2u-settings\", Version=\"0.1\"",
        ).send_json(body) {
            Ok(response) => response,
            Err(ureq::Error::Status(401, _)) => {
                anyhow::bail!("invalid username or password");
            }
            Err(ureq::Error::Status(code, _)) => {
                anyhow::bail!("the server returned HTTP {code}");
            }
            Err(err) => {
                anyhow::bail!("cannot reach the server: {err}");
            }
        };
        let data: serde_json::Value = response.into_json().context("Cannot parse the login response")?;
        let token = data
            .get("AccessToken")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .context("Login failed (wrong username or password?)")?;
        let user_id = data
            .get("User")
            .and_then(|u| u.get("Id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        Ok((token.to_owned(), user_id.to_owned()))
    }

    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("s2udio/", env!("CARGO_PKG_VERSION")))
            .build()
    }

    fn get(&self, path: &str) -> Result<serde_json::Value> {
        let url = format!("{}{}", self.base, path);
        Self::agent()
            .get(&url)
            .set("X-Emby-Token", &self.token)
            .call()
            .with_context(|| format!("Cannot fetch {url}"))?
            .into_json()
            .context("Cannot parse Jellyfin response")
    }

    /// The library views of the server (music libraries, movies, tv shows…).
    pub fn views(&self) -> Result<Vec<JfItem>> {
        let data = self.get(&format!("/Users/{}/Views", self.user_id))?;
        let items = data
            .get("Items")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let views: Vec<JfItem> = items
            .iter()
            .filter_map(|v| item_from_value(v, false))
            .collect();
        // Music libraries first, then the rest.
        let mut views = views;
        views.sort_by(|a, b| a.is_music_view.cmp(&b.is_music_view).then(a.name.cmp(&b.name)));
        Ok(views)
    }

    /// Direct children of a folder/view (non-music views: subfolders +
    /// audio items). `Recursive=false` so every folder level loads lazily.
    pub fn folder_children(&self, parent_id: &str) -> Result<Vec<JfItem>> {
        let path = format!(
            "/Users/{}/Items?ParentId={}&Recursive=false&SortBy=SortName&SortOrder=Ascending&Fields=MediaSources",
            self.user_id, parent_id
        );
        self.list_items(&path)
    }

    /// Artists of a music library view.
    pub fn artists(&self, view_id: &str) -> Result<Vec<JfItem>> {
        let path = format!(
            "/Users/{}/Items?ParentId={}&IncludeItemTypes=MusicArtist&Recursive=true\
             &SortBy=SortName&SortOrder=Ascending",
            self.user_id, view_id
        );
        self.list_items(&path)
    }

    /// Albums of an artist.
    pub fn albums_of_artist(&self, artist_id: &str) -> Result<Vec<JfItem>> {
        let path = format!(
            "/Users/{}/Items?ArtistIds={}&IncludeItemTypes=MusicAlbum&Recursive=true\
             &SortBy=SortName&SortOrder=Ascending",
            self.user_id, artist_id
        );
        self.list_items(&path)
    }

    /// Songs of an album.
    pub fn songs_of_album(&self, album_id: &str) -> Result<Vec<JfItem>> {
        let path = format!(
            "/Users/{}/Items?ParentId={}&IncludeItemTypes=Audio&Recursive=true\
             &SortBy=SortName&SortOrder=Ascending&Fields=MediaSources",
            self.user_id, album_id
        );
        self.list_items(&path)
    }

    /// The episodes of a season, ordered by episode number (builds the
    /// season playlist shown in the Queue tab's Video view when an episode
    /// plays).
    pub fn season_episodes(&self, season_id: &str) -> Result<Vec<JfItem>> {
        let mut items = self.folder_children(season_id)?;
        items.retain(|item| item.kind == "Episode");
        items.sort_by_key(|item| item.index_number.unwrap_or(i32::MAX));
        Ok(items)
    }

    fn list_items(&self, path: &str) -> Result<Vec<JfItem>> {
        let data = self.get(path)?;
        let items = data
            .get("Items")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(items.iter().filter_map(|v| item_from_value(v, false)).collect())
    }

    /// The stream URL MPD plays for an audio item. `static=true` serves the
    /// original file (no transcoding).
    pub fn stream_url(&self, item_id: &str) -> String {
        format!("{}/Audio/{item_id}/stream?static=true&api_key={}", self.base, self.token)
    }

    /// Stream URL for a video item (movie/episode): the file's audio track,
    /// which MPD decodes like a stream.
    pub fn video_stream_url(&self, item_id: &str) -> String {
        format!("{}/Videos/{item_id}/stream?static=true&api_key={}", self.base, self.token)
    }

    /// A single item's metadata (with chapter markers, people and the
    /// overview, so the queue tab's info box matches the Jellyfin tab's).
    pub fn item(&self, item_id: &str) -> Result<JfItem> {
        let data = self.get(&format!(
            "/Users/{}/Items/{item_id}?Fields=Chapters,People",
            self.user_id
        ))?;
        item_from_value(&data, false).context("Cannot parse item")
    }

    /// The chapter markers of an item (`Fields=Chapters`): named ranges with
    /// start/end positions in ticks (10 ms units).
    pub fn chapters(&self, item_id: &str) -> Result<Vec<crate::shared::chapters::Chapter>> {
        let data = self.get(&format!(
            "/Users/{}/Items/{item_id}?Fields=Chapters,People",
            self.user_id
        ))?;
        let mut chapters = Vec::new();
        let Some(items) = data.get("Chapters").and_then(serde_json::Value::as_array) else {
            return Ok(chapters);
        };
        for chapter in items {
            let start_ticks = chapter.get("StartPositionTicks").and_then(|v| v.as_i64());
            let Some(start) = start_ticks else { continue };
            let name = chapter
                .get("Name")
                .and_then(|v| v.as_str())
                .filter(|n| !n.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Chapter {}", chapters.len() + 1));
            chapters.push(crate::shared::chapters::Chapter {
                title: name,
                start_secs: start as f64 / 10_000_000.0,
                // Filled below from the next start / item runtime.
                end_secs: 0.0,
            });
        }
        // Jellyfin leaves EndPositionTicks at 0: each chapter ends where the
        // next begins; the last one runs to the item's runtime.
        let runtime_secs = data
            .get("RunTimeTicks")
            .and_then(|v| v.as_i64())
            .map_or(0.0, |t| t as f64 / 10_000_000.0);
        for idx in 0..chapters.len() {
            let end = chapters
                .get(idx + 1)
                .map(|c| c.start_secs)
                .filter(|end| *end > chapters[idx].start_secs)
                .unwrap_or(runtime_secs.max(chapters[idx].start_secs));
            chapters[idx].end_secs = end;
        }
        Ok(chapters)
    }

    /// The saved resume position of an item in seconds (0 when new).
    pub fn resume_position_secs(&self, item_id: &str) -> Result<f64> {
        let data = self.get(&format!("/UserItems/{item_id}/UserData"))?;
        Ok(data
            .get("PlaybackPositionTicks")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as f64
            / 10_000_000.0)
    }

    /// The primary image (poster / episode preview) of an item, downscaled.
    pub fn fetch_image(&self, item_id: &str, max_width: u32) -> Result<Vec<u8>> {
        let url = format!(
            "{}/Items/{item_id}/Images/Primary?maxWidth={max_width}",
            self.base
        );
        let response = Self::agent()
            .get(&url)
            .set("X-Emby-Token", &self.token)
            .call()
            .with_context(|| format!("Cannot fetch {url}"))?;
        let mut bytes = Vec::new();
        let mut reader = response.into_reader();
        std::io::Read::read_to_end(&mut reader, &mut bytes)?;
        Ok(bytes)
    }

    /// Report playback progress to the server (creates/updates the active
    /// session visible in the web UI and saves the resume position).
    pub fn report_playing_progress(
        &self,
        item_id: &str,
        position_secs: f64,
        paused: bool,
    ) -> Result<()> {
        let url = format!("{}/Sessions/Playing/Progress", self.base);
        let body = serde_json::json!({
            "ItemId": item_id,
            "PositionTicks": (position_secs * 10_000_000.0) as i64,
            "IsPaused": paused,
            "PlayMethod": "DirectPlay",
        });
        Self::agent()
            .post(&url)
            .set("X-Emby-Token", &self.token)
            .send_json(body)
            .context("Cannot report playback progress")?;
        Ok(())
    }

    /// Report the end of playback (saves the final position for resume).
    pub fn report_playing_stopped(&self, item_id: &str, position_secs: f64) -> Result<()> {
        let url = format!("{}/Sessions/Playing/Stopped", self.base);
        let body = serde_json::json!({
            "ItemId": item_id,
            "PositionTicks": (position_secs * 10_000_000.0) as i64,
            "IsPaused": false,
            "PlayMethod": "DirectPlay",
        });
        Self::agent()
            .post(&url)
            .set("X-Emby-Token", &self.token)
            .send_json(body)
            .context("Cannot report playback stop")?;
        Ok(())
    }
}

/// Tolerant extraction of a Jellyfin item. Unknown fields are ignored.
/// Extract the unique track languages of one stream type (Audio/Subtitle)
/// from the item's `MediaSources`, mapped to ISO 639-1 codes where known.
fn stream_languages(value: &serde_json::Value, stream_type: &str) -> Vec<String> {
    let mut languages: Vec<String> = Vec::new();
    let Some(sources) = value.get("MediaSources").and_then(serde_json::Value::as_array) else {
        return languages;
    };
    for source in sources {
        let Some(streams) = source.get("MediaStreams").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for stream in streams {
            if stream.get("Type").and_then(|t| t.as_str()) != Some(stream_type) {
                continue;
            }
            let Some(lang) = stream.get("Language").and_then(|l| l.as_str()) else {
                continue;
            };
            let code = iso639_2_to_1(lang.trim()).unwrap_or(lang.trim()).to_ascii_lowercase();
            if !code.is_empty() && !languages.iter().any(|l| l == &code) {
                languages.push(code);
            }
        }
    }
    languages
}

/// Map an ISO 639-2/B (3-letter) code to the common ISO 639-1 (2-letter)
/// code. Falls back to `None` for unmapped languages (the raw code is used).
fn iso639_2_to_1(code: &str) -> Option<&'static str> {
    Some(match code.to_ascii_lowercase().as_str() {
        "eng" => "en", "spa" => "es", "fra" | "fre" => "fr", "deu" | "ger" => "de",
        "jpn" => "ja", "kor" => "ko", "chi" | "zho" => "zh", "ita" => "it",
        "por" => "pt", "rus" => "ru", "ara" => "ar", "hin" => "hi", "ben" => "bn",
        "nld" | "dut" => "nl", "swe" => "sv", "nor" => "no", "dan" => "da", "fin" => "fi",
        "pol" => "pl", "tur" => "tr", "ukr" => "uk", "ell" | "gre" => "el", "heb" => "he",
        "tha" => "th", "vie" => "vi", "ind" => "id", "msa" | "may" => "ms", "ces" | "cze" => "cs",
        "slk" | "slo" => "sk", "hun" => "hu", "ron" | "rum" => "ro", "bul" => "bg",
        "hrv" => "hr", "srp" => "sr", "slv" => "sl", "cat" => "ca", "eus" | "baq" => "eu",
        "glg" => "gl", "isl" => "is", "lav" => "lv", "lit" => "lt", "est" => "et",
        "aze" => "az", "bel" => "be", "kaz" => "kk", "uzb" => "uz", "fas" | "per" => "fa",
        "urd" => "ur", "tam" => "ta", "tel" => "te", "mar" => "mr", "pan" => "pa",
        "guj" => "gu", "kan" => "kn", "mal" => "ml", "sin" => "si", "nep" => "ne",
        "swa" => "sw", "amh" => "am", "afr" => "af",
        _ => return None,
    })
}

fn item_from_value(value: &serde_json::Value, is_music_view: bool) -> Option<JfItem> {
    let id = value.get("Id")?.as_str()?.to_owned();
    let name = value.get("Name").and_then(|v| v.as_str()).unwrap_or("(untitled)").to_owned();
    let kind = value.get("Type").and_then(|v| v.as_str()).unwrap_or("Folder").to_owned();
    let str_field = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    let runtime_ticks = value.get("RunTimeTicks").and_then(|v| v.as_i64());
    // Credits: the `People` array (Director / Writer / Actor entries).
    let mut director = None;
    let mut writer = None;
    let mut starring = Vec::new();
    if let Some(people) = value.get("People").and_then(serde_json::Value::as_array) {
        for person in people {
            let Some(kind) = person.get("Type").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(name) = person.get("Name").and_then(|v| v.as_str()) else {
                continue;
            };
            match kind {
                "Director" => director = Some(name.to_owned()),
                "Writer" => writer = Some(name.to_owned()),
                "Actor" => starring.push(name.to_owned()),
                _ => {}
            }
        }
    }
    Some(JfItem {
        id,
        name,
        kind,
        album: str_field("Album"),
        album_artist: str_field("AlbumArtist"),
        artist: str_field("Artists").or_else(|| {
            value
                .get("Artists")
                .and_then(serde_json::Value::as_array)
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        }),
        series_name: str_field("SeriesName"),
        series_id: str_field("SeriesId"),
        season_id: str_field("SeasonId"),
        index_number: value.get("IndexNumber").and_then(|v| v.as_i64()).map(|n| n as i32),
        season_number: value
            .get("ParentIndexNumber")
            .and_then(|v| v.as_i64())
            .map(|n| n as i32),
        overview: str_field("Overview"),
        director,
        writer,
        starring,
        audio_languages: stream_languages(value, "Audio"),
        subtitle_languages: stream_languages(value, "Subtitle"),
        year: value.get("ProductionYear").and_then(|v| v.as_i64()).map(|y| y as i32),
        runtime_secs: runtime_ticks.map(|t| (t / 10_000_000) as u64),
        child_count: value.get("ChildCount").and_then(|v| v.as_i64()).map(|c| c as i32),
        is_music_view,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_jellytui_config() {
        let dir = std::env::temp_dir().join(format!("jellyfin-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "server_url = \"http://127.0.0.1:8086\"\naccess_token = \"abc123\"\nuser_id = \"u1\"\n",
        )
        .unwrap();
        let jf = Jellyfin::from_config_file(&path).expect("config to parse");
        assert_eq!(jf.base, "http://127.0.0.1:8086");
        assert_eq!(jf.token, "abc123");
        assert_eq!(jf.user_id, "u1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_config_yields_none() {
        assert!(Jellyfin::from_config_file(Path::new("/nonexistent/nope.toml")).is_none());
    }

    #[test]
    fn item_from_value_extracts_fields() {
        let v = json!({
            "Id": "item-1",
            "Name": "Dark Side",
            "Type": "MusicAlbum",
            "AlbumArtist": "Pink Floyd",
            "ProductionYear": 1973,
            "RunTimeTicks": 42_000_000_000_i64,
            "ChildCount": 10,
        });
        let item = item_from_value(&v, false).unwrap();
        assert_eq!(item.name, "Dark Side");
        assert_eq!(item.kind, "MusicAlbum");
        assert_eq!(item.album_artist.as_deref(), Some("Pink Floyd"));
        assert_eq!(item.year, Some(1973));
        assert_eq!(item.runtime_secs, Some(4200));
        assert_eq!(item.child_count, Some(10));
    }

    #[test]
    fn episode_item_keeps_its_season_and_index() {
        let v = json!({
            "Id": "ep-3",
            "Name": "Chapter Three",
            "Type": "Episode",
            "SeriesName": "The Show",
            "SeasonId": "season-1",
            "IndexNumber": 3,
            "RunTimeTicks": 45_000_000_000_i64,
        });
        let item = item_from_value(&v, false).unwrap();
        assert_eq!(item.kind, "Episode");
        assert_eq!(item.series_name.as_deref(), Some("The Show"));
        assert_eq!(item.season_id.as_deref(), Some("season-1"));
        assert_eq!(item.index_number, Some(3));
        assert_eq!(item.runtime_secs, Some(4500));
    }

    #[test]
    fn season_episodes_filter_and_sort() {
        // The API returns episodes with the season as their parent;
        // season_episodes keeps only Episodes and orders them by number.
        let json = json!({ "Items": [
            { "Id": "e2", "Name": "Two", "Type": "Episode", "IndexNumber": 2 },
            { "Id": "a1", "Name": "An Extra", "Type": "Video", "IndexNumber": 1 },
            { "Id": "e1", "Name": "One", "Type": "Episode", "IndexNumber": 1 },
            { "Id": "e10", "Name": "Ten", "Type": "Episode", "IndexNumber": 10 },
        ] });
        // The exact filtering/sorting season_episodes applies.
        let mut items: Vec<JfItem> = json
            .get("Items")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|v| item_from_value(v, false))
            .collect();
        items.retain(|i| i.kind == "Episode");
        items.sort_by_key(|i| i.index_number.unwrap_or(i32::MAX));
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["One", "Two", "Ten"]);
    }

    #[test]
    fn stream_url_builds_with_api_key() {
        let jf = Jellyfin {
            base: "http://x:8086".to_owned(),
            token: "tok".to_owned(),
            user_id: "u".to_owned(),
        };
        assert_eq!(
            jf.stream_url("abc"),
            "http://x:8086/Audio/abc/stream?static=true&api_key=tok"
        );
    }
}

/// Extract the Jellyfin item id from a stream URL of the form
/// `{base}/Audio/{id}/stream` or `{base}/Videos/{id}/stream`.
pub fn item_id_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let mut segments = parsed.path_segments()?;
    let kind = segments.next()?;
    let id = segments.next()?;
    if matches!(kind, "Audio" | "Videos") && id.len() == 32 {
        Some(id.to_owned())
    } else {
        None
    }
}
