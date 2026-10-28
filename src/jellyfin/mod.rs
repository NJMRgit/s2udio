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
/// Client identification sent to the Emby/Jellyfin API.
///
/// Jellyfin 12 (2026-09) no longer parses the legacy `X-Emby-*` headers, so
/// every request carries the standard `Authorization: MediaBrowser Client=…,
/// Device=…, DeviceId=…, Version=…` header. The legacy `X-Emby-Token` /
/// `X-Emby-Authorization` headers are sent alongside it, because **older
/// servers (Jellyfin 10.x/11.x, Emby) only look at those** — one binary
/// therefore works against both. Jellyfin 12 ignores the extra legacy header.
const CLIENT_NAME: &str = "s2udio";
const CLIENT_VERSION: &str = "0.1";
/// Stable device identity for the token-bearing API calls, so the server's
/// device list does not grow a new entry per session.
const DEVICE_NAME: &str = "desktop";
const DEVICE_ID: &str = "s2u-desktop";
/// Login identity — unchanged from what the Settings panel has always sent.
const LOGIN_DEVICE_NAME: &str = "settings";
const LOGIN_DEVICE_ID: &str = "s2u-settings";

/// The `MediaBrowser …` authorization value (with a token when one is known).
fn media_browser_auth(device: &str, device_id: &str, token: Option<&str>) -> String {
    let mut value = format!(
        "MediaBrowser Client=\"{CLIENT_NAME}\", Device=\"{device}\", DeviceId=\"{device_id}\", Version=\"{CLIENT_VERSION}\""
    );
    if let Some(token) = token {
        value.push_str(", Token=\"");
        value.push_str(token);
        value.push('"');
    }
    value
}
/// Server credentials + base URL, loaded from jellytui's config.
#[derive(Debug, Clone)]
pub struct Jellyfin {
    /// Server base URL, e.g. `http://127.0.0.1:8086` (no trailing slash).
    pub base: String,
    pub token: String,
    pub user_id: String,
}
/// One item of the Jellyfin library tree (a view, artist, album, folder or
/// song). Only the fields the tab displays are kept. Serializable so the
/// round-92 views cache can store them verbatim.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
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
    /// The item's parent (the folder/series/season it sits in), from
    /// `ParentId`. The search jump uses it to place an item in the tree.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// `UserData.Played`: the server has this item marked watched.
    #[serde(default)]
    pub played: bool,
    /// `UserData.PlayedPercentage` (0–100) as the server reports it.
    #[serde(default)]
    pub played_percentage: Option<f64>,
    /// `UserData.PlaybackPositionTicks` in seconds: where playback stopped
    /// last time. This is Jellyfin's own resume position — the value the
    /// web UI's Continue Watching list is built from.
    #[serde(default)]
    pub playback_position_secs: f64,
}
impl JfItem {
    pub fn is_container(&self) -> bool {
        matches!(
            self.kind.as_str(), "MusicArtist" | "MusicAlbum" | "Folder" |
            "CollectionFolder" | "Series" | "Season"
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
    /// Round 64: kinds that get the small poster shelf beside the list
    /// (everything sync_poster fetches an image for — playables plus the
    /// season/folder containers). The shelf replaces the old full-box art
    /// and the 40%-column poster split.
    pub fn kind_matches_poster(&self) -> bool {
        self.is_playable() || self.is_container()
    }
    /// The season number of a season item (Jellyfin reports a `Season`'s
    /// number as `IndexNumber`, not `ParentIndexNumber`), or of an
    /// episode's parent season.
    pub fn season_index(&self) -> Option<i32> {
        if self.kind == "Season" {
            self.index_number.or(self.season_number)
        } else {
            self.season_number
        }
    }
    /// `S03E03` for an episode whose numbers are known.
    pub fn episode_code(&self) -> Option<String> {
        let season = self.season_number?;
        let episode = self.index_number?;
        Some(format!("S{season:02}E{episode:02}"))
    }
    /// How much of the item has been watched (0.0–1.0), from the server's
    /// `PlayedPercentage` or the saved position over the runtime.
    pub fn progress_fraction(&self) -> Option<f64> {
        if let Some(pct) = self.played_percentage {
            return Some((pct / 100.0).clamp(0.0, 1.0));
        }
        let runtime = self.runtime_secs.filter(|secs| *secs > 0)?;
        if self.playback_position_secs <= 0.0 {
            return None;
        }
        Some((self.playback_position_secs / runtime as f64).clamp(0.0, 1.0))
    }
    /// The resume position in whole seconds, when the server has one.
    pub fn resume_secs(&self) -> Option<u64> {
        let secs = self.playback_position_secs;
        (secs >= 1.0).then(|| secs as u64)
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
    ResumePosition { seconds: f64 },
    /// Primary image bytes of an item (poster / episode preview).
    Image { item_id: String, bytes: Vec<u8> },
    /// Item metadata + primary image, for the MPRIS bridge.
    Mpris { item: JfItem, image: Vec<u8> },
    /// One Home-wall image, tagged with the carousel slot it belongs to.
    TaggedImage { tag: String, bytes: Vec<u8> },
    /// The Home page's three shelves (Continue Watching, Next Up, Recently
    /// Added), fetched as one work-thread round trip.
    Home(Box<JfHome>),
    /// The ancestor chain of an item (library → series → season → …), used
    /// to open the library tree on a search result.
    Ancestors { item_id: String, items: Vec<JfItem> },
    /// Chapter markers of an item.
    Chapters { item_id: String, chapters: Vec<crate::shared::chapters::Chapter> },
    /// The whole season's episodes as a playlist (built when an episode
    /// plays), with the index of the episode that was clicked.
    SeasonPlaylist { entries: Vec<SeasonEntry>, start_index: usize },
    /// Server-side search results (round 60 B2, `SearchHints`).
    SearchHints { items: Vec<JfItem> },
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
/// One library's "Recently Added" strip: the Home page's per-library rows
/// (`Users/{uid}/Items/Latest?ParentId=…`).
#[derive(Debug, Clone, PartialEq)]
pub struct JfLibraryLatest {
    /// The library view the strip belongs to (its name titles the row).
    pub library: JfItem,
    /// That library's newest items, newest first.
    pub items: Vec<JfItem>,
}
/// The Home page's shelves, in the order the page paints them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JfHome {
    /// `Users/{uid}/Items/Resume` filtered to video: started, not finished.
    pub continue_watching: Vec<JfItem>,
    /// `Shows/NextUp`: the next unwatched episode of every started series.
    pub next_up: Vec<JfItem>,
    /// `Users/{uid}/Items/Latest`: the newest items added to the libraries.
    pub recently_added: Vec<JfItem>,
    /// One strip per library with recent additions (titled "Recently Added
    /// in <library>"), after the server-wide `recently_added` shelf.
    pub library_latest: Vec<JfLibraryLatest>,
}
impl JfHome {
    pub fn is_empty(&self) -> bool {
        self.continue_watching.is_empty()
            && self.next_up.is_empty()
            && self.recently_added.is_empty()
            && self.library_latest.is_empty()
    }
}
/// How many rows each Home shelf asks the server for.
const HOME_SHELF_LIMIT: u32 = 12;

/// The `Items/Latest` request path, optionally scoped to one library (the
/// per-library Home strips). The endpoint answers a bare JSON array.
fn latest_media_path(user_id: &str, parent_id: Option<&str>) -> String {
    let parent = parent_id.map_or_else(String::new, |id| format!("&ParentId={id}"));
    format!(
        "/Users/{user_id}/Items/Latest?Limit={HOME_SHELF_LIMIT}{parent}\
         &Fields=Overview,MediaSources&ImageTypeLimit=1"
    )
}
/// Which Jellyfin image an item is asked for (the Home carousel shows wide
/// episode previews for Continue Watching and posters everywhere else).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum JfImageType {
    /// The item's own artwork: poster for movies/series/seasons, a still for
    /// episodes when the server has one.
    #[default]
    Primary,
    /// A 16:9 still (episode screenshots; the server also exposes them for
    /// movies when present).
    Thumb,
    /// A 16:9 backdrop image (movies and series).
    Backdrop,
}
impl JfImageType {
    fn api_name(self) -> &'static str {
        match self {
            Self::Primary => "Primary",
            Self::Thumb => "Thumb",
            Self::Backdrop => "Backdrop",
        }
    }
}
impl Jellyfin {
    /// Load the server credentials: the Settings-panel sidecar
    /// (`~/.config/s2udio/jellyfin.ron`, legacy `~/.config/rmpc/…`
    /// honored) when present, else jellytui's config
    /// file.
    pub fn load(jellytui_config: &Path, sidecar: Option<&Path>) -> Option<Self> {
        if let Some(sidecar) = sidecar {
            if let Ok(content) = std::fs::read_to_string(sidecar) {
                if let Ok(creds) = ron::de::from_str::<
                    crate::config::jellyfin::JellyfinCredentialsFile,
                >(&content) {
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
        Some(Self {
            base: base.trim_end_matches('/').to_owned(),
            token,
            user_id,
        })
    }
    /// Log in to the server with a username + password (the Settings panel
    /// flow): returns `(access_token, user_id)`.
    pub fn authenticate(
        base: &str,
        username: &str,
        password: &str,
    ) -> Result<(String, String)> {
        let url = format!("{}/Users/AuthenticateByName", base.trim_end_matches('/'));
        let body = serde_json::json!({
            "Username" : username,
            "Pw" : password,
            // Jellyfin 12 requires the client's app name in the login body
            // (a missing `App` is a 500 ArgumentNullException there); older
            // servers simply ignore the extra field.
            "App" : CLIENT_NAME,
        });
        let auth = media_browser_auth(LOGIN_DEVICE_NAME, LOGIN_DEVICE_ID, None);
        let response = match Self::agent()
            .post(&url)
            .set("Authorization", &auth)
            // legacy header for servers that predate the standard one
            .set("X-Emby-Authorization", &auth)
            .send_json(body)
        {
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
        let data: serde_json::Value = response
            .into_json()
            .context("Cannot parse the login response")?;
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
            .set(
                "Authorization",
                &media_browser_auth(DEVICE_NAME, DEVICE_ID, Some(&self.token)),
            )
            // legacy header for servers that predate the standard one
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
        let mut views = views;
        views
            .sort_by(|a, b| {
                a.is_music_view.cmp(&b.is_music_view).then(a.name.cmp(&b.name))
            });
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
    /// Server-side search across the user's libraries (round 60 B2): the
    /// Jellyfin `SearchHints` endpoint returns matching items of every
    /// kind (movies, episodes, music albums/artists/songs, folders…)
    /// with the primary-image + overview fields the pane displays.
    pub fn search_hints(&self, query: &str) -> Result<Vec<JfItem>> {
        let encoded: String =
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        let path = format!(
            "/Search/Hints?SearchTerm={encoded}&UserId={}&Limit=60",
            self.user_id
        );
        let data = self.get(&path)?;
        let hints = data
            .get("SearchHints")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(
            hints
                .iter()
                .filter_map(search_hint_item)
                .collect(),
        )
    }
    /// The Home page's three shelves in one work-thread round trip (three
    /// GETs: Continue Watching, Next Up, Recently Added). Each shelf is
    /// independent: one failing endpoint leaves that shelf empty instead of
    /// blanking the whole page.
    pub fn home(&self) -> Result<JfHome> {
        let mut home = JfHome::default();
        let mut first_error: Option<anyhow::Error> = None;
        let mut try_shelf = |result: Result<Vec<JfItem>>| match result {
            Ok(items) => items,
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
                Vec::new()
            }
        };
        home.continue_watching = try_shelf(self.resume_items());
        home.next_up = try_shelf(self.next_up());
        home.recently_added = try_shelf(self.recently_added());
        // A "Recently Added in <library>" strip per library, after the
        // server-wide shelf. The views call is one more GET; a failing
        // library strip is skipped instead of blanking the page.
        let views = try_shelf(self.views());
        for view in views {
            let items = try_shelf(self.library_latest(&view.id));
            if !items.is_empty() {
                home.library_latest.push(JfLibraryLatest { library: view, items });
            }
        }
        // A page with nothing at all is a failure worth reporting (the
        // pane then shows the notice row); a partially filled page is not.
        if home.is_empty() {
            if let Some(err) = first_error {
                return Err(err);
            }
        }
        Ok(home)
    }
    /// Continue Watching: the items the user started and has not finished
    /// (Jellyfin's own resume list, newest first). Video only — the tab's
    /// Home page is a video page like the web UI's.
    pub fn resume_items(&self) -> Result<Vec<JfItem>> {
        let path = format!(
            "/Users/{}/Items/Resume?MediaTypes=Video&Limit={HOME_SHELF_LIMIT}\
             &Fields=Overview,MediaSources&ImageTypeLimit=1",
            self.user_id
        );
        self.list_items(&path)
    }
    /// Next Up: the next unwatched episode of every series the user has
    /// started (Jellyfin's `Shows/NextUp`).
    pub fn next_up(&self) -> Result<Vec<JfItem>> {
        let path = format!(
            "/Shows/NextUp?UserId={}&Limit={HOME_SHELF_LIMIT}\
             &Fields=Overview,MediaSources&ImageTypeLimit=1",
            self.user_id
        );
        self.list_items(&path)
    }
    /// Recently Added: the newest items added to the libraries. The
    /// endpoint answers a bare JSON array (not an `Items` object).
    pub fn recently_added(&self) -> Result<Vec<JfItem>> {
        self.latest_media(None)
    }
    /// Recently Added in one library (the Home page's per-library strip).
    pub fn library_latest(&self, library_id: &str) -> Result<Vec<JfItem>> {
        self.latest_media(Some(library_id))
    }
    fn latest_media(&self, parent_id: Option<&str>) -> Result<Vec<JfItem>> {
        let data = self.get(&latest_media_path(&self.user_id, parent_id))?;
        Ok(list_from_value(&data))
    }
    /// A tagged Home-wall image (see `FetchJellyfinImageTagged`): the tag
    /// echoes back with the bytes so the pane can match a slot.
    pub fn tagged_image(
        &self,
        item_id: &str,
        fallback_item_id: Option<&str>,
        image_type: JfImageType,
        max_width: u32,
    ) -> Result<Vec<u8>> {
        let mut attempts: Vec<(&str, JfImageType)> =
            vec![(item_id, image_type), (item_id, JfImageType::Primary)];
        if let Some(fallback) = fallback_item_id {
            attempts.push((fallback, JfImageType::Primary));
        }
        let mut last_err = None;
        for (target, kind) in attempts {
            match self.fetch_image_of(target, kind, max_width) {
                Ok(bytes) => return Ok(bytes),
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no image for {item_id}")))
    }
    /// The ancestor chain of an item, nearest folder first
    /// (`Items/{id}/Ancestors`): for an episode that is
    /// `[Season, Series, Library]`, for a movie `[Library]`.
    pub fn ancestors(&self, item_id: &str) -> Result<Vec<JfItem>> {
        let data = self.get(
            &format!("/Items/{item_id}/Ancestors?UserId={}", self.user_id),
        )?;
        Ok(list_from_value(&data))
    }
    /// The stream URL MPD plays for an audio item. `static=true` serves the
    /// original file (no transcoding).
    pub fn stream_url(&self, item_id: &str) -> String {
        format!(
            "{}/Audio/{item_id}/stream?static=true&api_key={}", self.base, self.token
        )
    }
    /// Stream URL for a video item (movie/episode): the file's audio track,
    /// which MPD decodes like a stream.
    pub fn video_stream_url(&self, item_id: &str) -> String {
        format!(
            "{}/Videos/{item_id}/stream?static=true&api_key={}", self.base, self.token
        )
    }
    /// A single item's metadata (with chapter markers, people and the
    /// overview, so the queue tab's info box matches the Jellyfin tab's).
    pub fn item(&self, item_id: &str) -> Result<JfItem> {
        let data = self
            .get(
                &format!(
                    "/Users/{}/Items/{item_id}?Fields=Chapters,People", self.user_id
                ),
            )?;
        item_from_value(&data, false).context("Cannot parse item")
    }
    /// The chapter markers of an item (`Fields=Chapters`): named ranges with
    /// start/end positions in ticks (10 ms units).
    pub fn chapters(
        &self,
        item_id: &str,
    ) -> Result<Vec<crate::shared::chapters::Chapter>> {
        let data = self
            .get(
                &format!(
                    "/Users/{}/Items/{item_id}?Fields=Chapters,People", self.user_id
                ),
            )?;
        let mut chapters = Vec::new();
        let Some(items) = data.get("Chapters").and_then(serde_json::Value::as_array)
        else {
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
            chapters
                .push(crate::shared::chapters::Chapter {
                    title: name,
                    start_secs: start as f64 / 10_000_000.0,
                    end_secs: 0.0,
                });
        }
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
    ///
    /// Jellyfin has no `UserItems/{id}/UserData` route (that request 404s) —
    /// the position lives in the item DTO's `UserData`, which
    /// `Users/{uid}/Items/{id}` returns by default (`DtoOptions` enables
    /// user data). Reading it from the item keeps one request.
    pub fn resume_position_secs(&self, item_id: &str) -> Result<f64> {
        let data = self.get(&format!("/Users/{}/Items/{item_id}", self.user_id))?;
        Ok(
            data.get("UserData")
                .and_then(|ud| ud.get("PlaybackPositionTicks"))
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
                as f64
                / 10_000_000.0,
        )
    }
    /// The primary image (poster / episode preview) of an item, downscaled.
    pub fn fetch_image(&self, item_id: &str, max_width: u32) -> Result<Vec<u8>> {
        self.fetch_image_of(item_id, JfImageType::Primary, max_width)
    }
    /// One specific image type of an item (`Primary`, `Thumb`, `Backdrop`),
    /// downscaled to `max_width` pixels.
    pub fn fetch_image_of(
        &self,
        item_id: &str,
        image_type: JfImageType,
        max_width: u32,
    ) -> Result<Vec<u8>> {
        let kind = image_type.api_name();
        let url = format!(
            "{}/Items/{item_id}/Images/{kind}?maxWidth={max_width}", self.base
        );
        let response = Self::agent()
            .get(&url)
            .set(
                "Authorization",
                &media_browser_auth(DEVICE_NAME, DEVICE_ID, Some(&self.token)),
            )
            // legacy header for servers that predate the standard one
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
    ///
    /// The body carries the full identifying set a real client sends
    /// (`MediaSourceId` + `PlaySessionId`), so the server can attribute the
    /// update to one playback of one version — an `ItemId` alone relied on
    /// the server's fallback and lost the report when a session was
    /// re-created.
    pub fn report_playing_progress(
        &self,
        item_id: &str,
        position_secs: f64,
        paused: bool,
        play_session_id: &str,
    ) -> Result<()> {
        self.post_playback_report(
            "/Sessions/Playing/Progress",
            playback_progress_body(item_id, position_secs, paused, play_session_id),
        )
        .context("Cannot report playback progress")
    }
    /// Report the start of playback (creates the session the progress and
    /// stop reports belong to; without it the server has no playback
    /// session and stores no resume position).
    pub fn report_playing_start(
        &self,
        item_id: &str,
        position_secs: f64,
        play_session_id: &str,
    ) -> Result<()> {
        self.post_playback_report(
            "/Sessions/Playing",
            playback_progress_body(item_id, position_secs, false, play_session_id),
        )
        .context("Cannot report playback start")
    }
    /// Report the end of playback (saves the final position for resume, and
    /// marks the item watched once the server sees it was near the end).
    ///
    /// A caller that knows nothing better than position 0 must not call
    /// this: a 0-tick stop is below `MinResumePct`, so the server stores 0
    /// and **wipes** the resume position the progress reports had saved.
    /// `position_secs <= 0.0` is therefore dropped here as a no-op.
    pub fn report_playing_stopped(
        &self,
        item_id: &str,
        position_secs: f64,
        play_session_id: &str,
    ) -> Result<()> {
        if position_secs <= 0.0 {
            log::debug!(
                item_id:? = item_id; "Skipping a 0-position Jellyfin stop report (it would wipe the saved resume position)"
            );
            return Ok(());
        }
        self.post_playback_report(
            "/Sessions/Playing/Stopped",
            playback_stop_body(item_id, position_secs, play_session_id),
        )
        .context("Cannot report playback stop")
    }
    fn post_playback_report(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<()> {
        let url = format!("{}{path}", self.base);
        Self::agent()
            .post(&url)
            .set(
                "Authorization",
                &media_browser_auth(DEVICE_NAME, DEVICE_ID, Some(&self.token)),
            )
            // legacy header for servers that predate the standard one
            .set("X-Emby-Token", &self.token)
            .send_json(body)
            .with_context(|| format!("Cannot POST {url}"))?;
        Ok(())
    }
}

/// A stable-per-playback session id for the Jellyfin playback reports.
///
/// `PlaySessionId` identifies one playback of one item to the server;
/// process id + a counter is unique per session without pulling in a uuid
/// dependency, and it is deliberately not persisted (a restart is a new
/// playback).
pub fn new_play_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("s2u-{}-{seq}", std::process::id())
}

/// Whether a playback position is worth reporting to Jellyfin at all.
///
/// The server's `UpdatePlayState` treats a report below `MinResumePct`
/// (5 %, the default) as position 0 **and saves it**, which erases the
/// resume position the item already had. A client that reports every 10 s
/// from the very start therefore wipes the position it is supposed to
/// track. Only report once playback is meaningfully in (or has reached the
/// end): the end-of-item case always reports so a finished episode is
/// marked watched.
pub fn reportable_position(position_secs: f64, duration_secs: f64) -> bool {
    if position_secs <= 0.0 {
        return false;
    }
    let near_end = duration_secs > 0.0 && position_secs >= duration_secs - 15.0;
    near_end
        || position_secs >= 60.0
        || (duration_secs > 0.0 && position_secs / duration_secs >= 0.05)
}

/// Ticks (Jellyfin's 10 ms unit) for a position in seconds, never negative.
fn position_ticks(position_secs: f64) -> i64 {
    (position_secs.max(0.0) * 10_000_000.0) as i64
}

/// The shared start/progress report body. Only fields the server's
/// `PlaybackProgressInfo` model actually has are sent (unknown members
/// would be rejected by a strict binder).
fn playback_progress_body(
    item_id: &str,
    position_secs: f64,
    paused: bool,
    play_session_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "ItemId": item_id,
        "MediaSourceId": item_id,
        "PlaySessionId": play_session_id,
        "PositionTicks": position_ticks(position_secs),
        "CanSeek": true,
        "IsPaused": paused,
        "IsMuted": false,
        "PlayMethod": "DirectPlay",
    })
}

/// The stop report body (`PlaybackStopInfo`).
fn playback_stop_body(
    item_id: &str,
    position_secs: f64,
    play_session_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "ItemId": item_id,
        "MediaSourceId": item_id,
        "PlaySessionId": play_session_id,
        "PositionTicks": position_ticks(position_secs),
        "Failed": false,
    })
}
/// Tolerant extraction of a Jellyfin item. Unknown fields are ignored.
/// Extract the unique track languages of one stream type (Audio/Subtitle)
/// from the item's `MediaSources`, mapped to ISO 639-1 codes where known.
fn stream_languages(value: &serde_json::Value, stream_type: &str) -> Vec<String> {
    let mut languages: Vec<String> = Vec::new();
    let Some(sources) = value.get("MediaSources").and_then(serde_json::Value::as_array)
    else {
        return languages;
    };
    for source in sources {
        let Some(streams) = source
            .get("MediaStreams")
            .and_then(serde_json::Value::as_array) else {
            continue;
        };
        for stream in streams {
            if stream.get("Type").and_then(|t| t.as_str()) != Some(stream_type) {
                continue;
            }
            let Some(lang) = stream.get("Language").and_then(|l| l.as_str()) else {
                continue;
            };
            let code = iso639_2_to_1(lang.trim())
                .unwrap_or(lang.trim())
                .to_ascii_lowercase();
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
    Some(
        match code.to_ascii_lowercase().as_str() {
            "eng" => "en",
            "spa" => "es",
            "fra" | "fre" => "fr",
            "deu" | "ger" => "de",
            "jpn" => "ja",
            "kor" => "ko",
            "chi" | "zho" => "zh",
            "ita" => "it",
            "por" => "pt",
            "rus" => "ru",
            "ara" => "ar",
            "hin" => "hi",
            "ben" => "bn",
            "nld" | "dut" => "nl",
            "swe" => "sv",
            "nor" => "no",
            "dan" => "da",
            "fin" => "fi",
            "pol" => "pl",
            "tur" => "tr",
            "ukr" => "uk",
            "ell" | "gre" => "el",
            "heb" => "he",
            "tha" => "th",
            "vie" => "vi",
            "ind" => "id",
            "msa" | "may" => "ms",
            "ces" | "cze" => "cs",
            "slk" | "slo" => "sk",
            "hun" => "hu",
            "ron" | "rum" => "ro",
            "bul" => "bg",
            "hrv" => "hr",
            "srp" => "sr",
            "slv" => "sl",
            "cat" => "ca",
            "eus" | "baq" => "eu",
            "glg" => "gl",
            "isl" => "is",
            "lav" => "lv",
            "lit" => "lt",
            "est" => "et",
            "aze" => "az",
            "bel" => "be",
            "kaz" => "kk",
            "uzb" => "uz",
            "fas" | "per" => "fa",
            "urd" => "ur",
            "tam" => "ta",
            "tel" => "te",
            "mar" => "mr",
            "pan" => "pa",
            "guj" => "gu",
            "kan" => "kn",
            "mal" => "ml",
            "sin" => "si",
            "nep" => "ne",
            "swa" => "sw",
            "amh" => "am",
            "afr" => "af",
            _ => return None,
        },
    )
}
/// Convert one `SearchHints` entry (JSON shape: `ItemId` instead of
/// `Id`, plus `StartDate`, `PrimaryImageTag`, …) into a [`JfItem`] by
/// normalizing its id, then reusing the shared item parser.
fn search_hint_item(value: &serde_json::Value) -> Option<JfItem> {
    let mut normalized = value.clone();
    if let Some(id) = value.get("ItemId").and_then(serde_json::Value::as_str) {
        normalized.as_object_mut()?.insert("Id".to_owned(), serde_json::Value::String(id.to_owned()));
    }
    item_from_value(&normalized, false)
}

fn item_from_value(value: &serde_json::Value, is_music_view: bool) -> Option<JfItem> {
    let id = value.get("Id")?.as_str()?.to_owned();
    let name = value
        .get("Name")
        .and_then(|v| v.as_str())
        .unwrap_or("(untitled)")
        .to_owned();
    let kind = value.get("Type").and_then(|v| v.as_str()).unwrap_or("Folder").to_owned();
    let str_field = |key: &str| {
        value.get(key).and_then(|v| v.as_str()).map(str::to_owned)
    };
    let runtime_ticks = value.get("RunTimeTicks").and_then(|v| v.as_i64());
    let user_data = value.get("UserData");
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
        artist: str_field("Artists")
            .or_else(|| {
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
        index_number: value
            .get("IndexNumber")
            .and_then(|v| v.as_i64())
            .map(|n| n as i32),
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
        parent_id: str_field("ParentId"),
        played: user_data
            .and_then(|ud| ud.get("Played"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        played_percentage: user_data
            .and_then(|ud| ud.get("PlayedPercentage"))
            .and_then(serde_json::Value::as_f64),
        playback_position_secs: user_data
            .and_then(|ud| ud.get("PlaybackPositionTicks"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0) as f64
            / 10_000_000.0,
    })
}

/// Normalize the two list shapes the Jellyfin API answers with: the
/// `{ "Items": [...] }` envelope most endpoints use, and the bare array
/// `Items/Latest` (and `Ancestors`) return.
fn list_from_value(value: &serde_json::Value) -> Vec<JfItem> {
    let items = match value {
        serde_json::Value::Array(items) => items.as_slice(),
        other => other
            .get("Items")
            .and_then(serde_json::Value::as_array)
            .map_or(&[][..], Vec::as_slice),
    };
    items.iter().filter_map(|v| item_from_value(v, false)).collect()
}
/// Round 92: on-disk cache of the server's top-level library views
/// (`<cache_dir>/jellyfin-views.json`).
///
/// The Libraries tab used to render empty until the server answered the
/// first views fetch — on a fresh boot the tab stayed blank for a while
/// with no sign that anything was loading. The pane now paints the cached
/// list on the first frame and refreshes it in the background
/// (stale-while-revalidate); this file is what makes that first frame
/// possible.
///
/// The cache is keyed to the account it was fetched from (the server URL
/// is recorded too, but only as context: one server can be reached through
/// several addresses, e.g. a LAN IP and a localhost proxy).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JfViewsCache {
    /// Server base URL the views came from.
    pub server: String,
    /// Account the views belong to.
    pub user_id: String,
    /// When the cache was written (epoch seconds; diagnostics only — the
    /// pane always refreshes, so there is no TTL).
    #[serde(default)]
    pub saved_at: u64,
    /// The top-level views (libraries/categories) as the server returned
    /// them.
    pub views: Vec<JfItem>,
}
/// File name of the views cache inside the cache dir.
const VIEWS_CACHE_FILE: &str = "jellyfin-views.json";
/// Path of the views cache: `<cache_dir>/jellyfin-views.json` (the
/// configured cache dir, else s2udio's default `~/.cache/s2udio`).
pub fn views_cache_path(cache_dir: Option<&Path>) -> std::path::PathBuf {
    if let Some(dir) = cache_dir {
        return dir.join(VIEWS_CACHE_FILE);
    }
    crate::shared::paths::s2udio_cache_dir()
        .unwrap_or_else(|| {
            crate::config::utils::tilde_expand("~/.cache/s2udio")
                .into_owned()
                .into()
        })
        .join(VIEWS_CACHE_FILE)
}
/// The cached views, or None when there is no readable cache.
pub fn load_views_cache(cache_dir: Option<&Path>) -> Option<JfViewsCache> {
    let data = std::fs::read(views_cache_path(cache_dir)).ok()?;
    serde_json::from_slice(&data).ok()
}
/// Write the views cache. Atomic (temp file + rename), so a crash mid-write
/// cannot leave a half-written cache behind.
pub fn save_views_cache(
    cache_dir: Option<&Path>,
    server: &str,
    user_id: &str,
    views: &[JfItem],
) {
    let cache = JfViewsCache {
        server: server.to_owned(),
        user_id: user_id.to_owned(),
        saved_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        views: views.to_vec(),
    };
    let Ok(data) = serde_json::to_vec(&cache) else {
        log::warn!("Failed to serialize the Jellyfin views cache");
        return;
    };
    let path = views_cache_path(cache_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, data)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .is_err()
    {
        log::warn!(path:?; "Failed to write the Jellyfin views cache");
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
    fn parses_user_data_from_the_item_dto() {
        // The resume position and watched flag live in the item's
        // `UserData` (there is no `UserItems/{id}/UserData` route).
        let v = json!({
            "Id": "ep-1",
            "Name": "Chapter One",
            "Type": "Episode",
            "ParentId": "season-1",
            "RunTimeTicks": 1_350_000_000_i64,
            "UserData": {
                "Played": true,
                "PlayedPercentage": 42.5,
                "PlaybackPositionTicks": 573_750_000_i64,
                "PlayCount": 2,
            },
        });
        let item = item_from_value(&v, false).unwrap();
        assert!(item.played);
        assert_eq!(item.played_percentage, Some(42.5));
        assert!((item.playback_position_secs - 57.375).abs() < f64::EPSILON);
        assert_eq!(item.parent_id.as_deref(), Some("season-1"));
        assert_eq!(item.resume_secs(), Some(57));
        assert!(item.progress_fraction().is_some());
    }

    #[test]
    fn missing_user_data_reads_as_new() {
        let v = json!({ "Id": "e", "Name": "N", "Type": "Episode" });
        let item = item_from_value(&v, false).unwrap();
        assert!(!item.played);
        assert_eq!(item.played_percentage, None);
        assert_eq!(item.playback_position_secs, 0.0);
        assert_eq!(item.resume_secs(), None);
        assert_eq!(item.progress_fraction(), None);
    }

    #[test]
    fn both_list_shapes_parse() {
        // Most endpoints answer `{ "Items": [...] }`; `Items/Latest` and
        // `Items/{id}/Ancestors` answer a bare array (measured against the
        // live server).
        let envelope = json!({ "Items": [
            { "Id": "a", "Name": "A", "Type": "Movie" },
            { "Id": "b", "Name": "B", "Type": "Episode" },
        ] });
        let bare = json!([
            { "Id": "c", "Name": "C", "Type": "Season" },
        ]);
        assert_eq!(
            list_from_value(&envelope).iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            list_from_value(&bare).iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["c"]
        );
        assert!(list_from_value(&json!({})).is_empty());
    }

    #[test]
    fn a_season_takes_its_number_from_the_index() {
        // Jellyfin reports a Season's number as `IndexNumber` (the
        // `ParentIndexNumber` is the series' root), while an episode's
        // season number is `ParentIndexNumber`.
        let season = item_from_value(
            &json!({ "Id": "s2", "Name": "Season 2", "Type": "Season",
                     "IndexNumber": 2 }),
            false,
        )
        .unwrap();
        assert_eq!(season.season_index(), Some(2));
        let episode = item_from_value(
            &json!({ "Id": "e3", "Name": "Three", "Type": "Episode",
                     "IndexNumber": 3, "ParentIndexNumber": 2 }),
            false,
        )
        .unwrap();
        assert_eq!(episode.season_index(), Some(2));
        assert_eq!(episode.episode_code().as_deref(), Some("S02E03"));
    }

    #[test]
    fn positions_below_the_resume_threshold_are_not_reported() {
        // A report below MinResumePct is stored as 0 and WIPES the saved
        // resume position, so early ticks must stay silent.
        assert!(!reportable_position(0.0, 1200.0));
        assert!(!reportable_position(5.0, 1200.0));
        assert!(!reportable_position(30.0, 1200.0), "2.5% of a 20 min episode");
        assert!(reportable_position(90.0, 1200.0), "7.5% is past the threshold");
        assert!(reportable_position(60.0, 3600.0), "one minute floor");
        // A short clip that reached its end still reports.
        assert!(reportable_position(28.0, 30.0));
        // Below 60 s with an unknown duration: nothing to report.
        assert!(!reportable_position(30.0, 0.0));
    }

    #[test]
    fn playback_report_bodies_carry_the_full_client_identity() {
        let body = playback_progress_body("abc", 12.5, true, "sess-1");
        assert_eq!(body["ItemId"], json!("abc"));
        assert_eq!(body["MediaSourceId"], json!("abc"));
        assert_eq!(body["PlaySessionId"], json!("sess-1"));
        assert_eq!(body["PositionTicks"], json!(125_000_000_i64));
        assert_eq!(body["IsPaused"], json!(true));
        assert_eq!(body["PlayMethod"], json!("DirectPlay"));
        assert_eq!(body["CanSeek"], json!(true));

        let stop = playback_stop_body("abc", 99.0, "sess-2");
        assert_eq!(stop["Failed"], json!(false));
        assert_eq!(stop["PositionTicks"], json!(990_000_000_i64));
        assert_eq!(stop["PlaySessionId"], json!("sess-2"));
        // A negative position (a seek report gone wrong) clamps to 0
        // instead of sending a tick count the server rejects.
        assert_eq!(position_ticks(-3.0), 0);
    }

    #[test]
    fn latest_media_scopes_to_the_library() {
        let all = latest_media_path("u", None);
        assert!(all.starts_with("/Users/u/Items/Latest?Limit=12"), "{all}");
        assert!(!all.contains("ParentId"), "the server-wide shelf is unscoped: {all}");
        let anime = latest_media_path("u", Some("lib1"));
        assert!(anime.contains("ParentId=lib1"), "a strip is scoped to its library: {anime}");
        assert!(anime.contains("Fields=Overview,MediaSources"), "{anime}");
    }

    #[test]
    fn play_session_ids_are_unique_per_playback() {
        let a = new_play_session_id();
        let b = new_play_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("s2u-"), "{a}");
    }

    #[test]
    fn a_zero_position_stop_does_not_reach_the_server() {
        // The early return must happen before any HTTP: no server is
        // reachable here and the call still succeeds.
        let jf = Jellyfin {
            base: "http://127.0.0.1:1".to_owned(),
            token: "tok".to_owned(),
            user_id: "u".to_owned(),
        };
        assert!(jf.report_playing_stopped("abc", 0.0, "sess").is_ok());
    }

    #[test]
    fn stream_url_builds_with_api_key() {
        let jf = Jellyfin {
            base: "http://x:8086".to_owned(),
            token: "tok".to_owned(),
            user_id: "u".to_owned(),
        };
        assert_eq!(jf.stream_url("abc"), "http://x:8086/Audio/abc/stream?static=true&api_key=tok");
    }
}
