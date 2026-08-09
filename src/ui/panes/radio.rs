use std::{collections::HashSet, path::PathBuf};

use anyhow::Result;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{CommonAction, DirectoriesActions},
        tabs::PaneType,
        utils::tilde_expand,
    },
    ctx::Ctx,
    mpd::{
        client::Client,
        commands::{Song, State},
        errors::{ErrorCode, MpdError},
        mpd_client::MpdClient,
    },
    radio::{CountryGroup, DirectoryStation, RadioDirectory},
    shared::{
        events::WorkRequest,
        keys::ActionEvent,
        macros::{modal, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{
        UiEvent,
        input::InputResultEvent,
        modals::{
            confirm_modal::{Action, ConfirmModal},
            input_modal::InputModal,
            menu::modal::MenuModal,
        },
    },
};

/// Result ids of the work-thread radio fetches.
pub const RADIO_DIRECTORY: &str = "radio_directory";
pub const RADIO_STATES: &str = "radio_states";
pub const RADIO_STATE_STATIONS: &str = "radio_state_stations";
pub const RADIO_COUNTRY_STATIONS: &str = "radio_country_stations";
const INIT: &str = "radio_init";
const REINIT: &str = "radio_reinit";
const PLAY: &str = "radio_play";

/// One entry of the favourite stations list (an MPD stored playlist whose
/// entries are stream URLs). Names come from the `#EXTINF` lines of the
/// underlying `.m3u` file, which MPD itself never rewrites without dropping
/// them, so all list mutations go through direct file writes (MPD hot-reloads
/// the file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioStation {
    pub name: String,
    pub url: String,
}

impl RadioStation {
    pub(crate) fn from_song(song: &Song) -> Self {
        let name = song
            .metadata
            .get("name")
            .map(|v| v.last().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| song.file.clone());
        Self { name, url: song.file.clone() }
    }
}

/// Identifies a region in the left tree: Local, a country, a state or a city.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RegionKind {
    Favourites,
    Local,
    Country(String),
    /// A state/province — the deepest category: selecting it filters the
    /// station list to all stations of that province.
    State { country: String, state: String },
}

impl RegionKind {
    fn key(&self) -> String {
        match self {
            Self::Favourites => "fav".to_owned(),
            Self::Local => "local".to_owned(),
            Self::Country(c) => format!("country:{c}"),
            Self::State { country, state } => format!("state:{country}/{state}"),
        }
    }
}

/// One row of the region tree (left pane). All rows are a single terminal
/// line; depth indents nested regions.
#[derive(Debug, Clone)]
pub struct RegionRow {
    pub kind: RegionKind,
    pub label: String,
    pub depth: u8,
    pub expandable: bool,
    pub expanded: bool,
}

/// One row of the station list (right pane): a favourite or a directory
/// station, each rendered as two lines.
#[derive(Debug, Clone)]
pub enum StationRow {
    Favourite(RadioStation),
    Directory(DirectoryStation),
}

impl StationRow {
    fn url(&self) -> &str {
        match self {
            Self::Favourite(station) => &station.url,
            Self::Directory(station) => &station.url,
        }
    }
}

/// Recognized stream URL schemes; anything else in the radio playlist is
/// skipped.
pub(crate) fn is_stream_url(url: &str) -> bool {
    ["http://", "https://", "rtmp://", "rtmps://", "mms://", "icecast://"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
}

/// Path of the playlist file MPD stores the favourites in. `config` is only
/// available over a unix socket, so fall back to the standard locations.
fn playlist_file_path(client: &mut Client<'_>, playlist: &str) -> Result<PathBuf> {
    let file_name = format!("{playlist}.m3u");
    if let Some(cfg) = client.config()
        && !cfg.playlist_directory.is_empty()
    {
        let dir = tilde_expand(&cfg.playlist_directory);
        return Ok(PathBuf::from(dir.into_owned()).join(file_name));
    }
    for dir in ["~/.config/mpd/playlists", "~/.mpd/playlists", "/var/lib/mpd/playlists"] {
        let dir = PathBuf::from(tilde_expand(dir).into_owned());
        if dir.exists() {
            return Ok(dir.join(file_name));
        }
    }
    // Last resort: create the default location.
    Ok(PathBuf::from(tilde_expand("~/.config/mpd/playlists").into_owned()).join(file_name))
}

/// Fetch the favourites list from MPD. A missing playlist is not an error.
fn fetch_stations(client: &mut Client<'_>, playlist: &str) -> Result<Vec<RadioStation>> {
    let songs = match client.list_playlist_info(playlist, None) {
        Ok(songs) => songs,
        Err(MpdError::Mpd(failure)) if failure.code == ErrorCode::NoExist => Vec::new(),
        Err(err) => return Err(err.into()),
    };
    Ok(songs
        .iter()
        .filter(|song| is_stream_url(&song.file))
        .map(RadioStation::from_song)
        .collect())
}

/// EXTINF m3u serialization: every station keeps its name, which MPD's own
/// playlist commands would drop on rewrite.
fn m3u_content(stations: &[RadioStation]) -> String {
    let mut content = String::from("#EXTM3U\n");
    for station in stations {
        // EXTINF is one line: strip anything that would break the format.
        let name = station.name.replace(['\n', '\r'], " ");
        let url = station.url.replace(['\n', '\r'], " ");
        content.push_str(&format!("#EXTINF:-1,{name}\n{url}\n"));
    }
    content
}

/// Rewrite the whole `.m3u` in EXTINF format so every station keeps its name
/// (MPD's own playlist commands drop `#EXTINF` lines on rewrite). MPD notices
/// the file change and reloads the playlist.
fn write_stations_file(
    client: &mut Client<'_>,
    playlist: &str,
    stations: &[RadioStation],
) -> Result<()> {
    let path = playlist_file_path(client, playlist)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, m3u_content(stations))?;
    Ok(())
}

/// "12 km", "3.4 km" etc.
fn format_distance(km: Option<f64>) -> Option<String> {
    let km = km?;
    if km < 10.0 {
        Some(format!("{km:.1} km"))
    } else {
        Some(format!("{km:.0} km"))
    }
}

/// Second (dim) line of a directory/local station row.
fn station_subline(station: &DirectoryStation) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(dist) = format_distance(station.distance_km) {
        parts.push(dist);
    }
    if let Some(city) = station.city.as_deref() {
        parts.push(city.to_owned());
    } else if let Some(state) = station.state.as_deref() {
        parts.push(state.to_owned());
    }
    parts.push(station.country.clone());
    if station.votes > 0 {
        parts.push(format!("{} votes", station.votes));
    }
    parts.join(" · ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The list the single keyboard cursor is on: the region tree, or the
/// station list once a region is entered (`d`/`→`/Enter enter a region,
/// `a`/`←` back out).
enum PaneFocus {
    Regions,
    Stations,
}

#[derive(Debug)]
pub struct RadioPane {
    /// Favourites from MPD (uncapped; display caps at max_favourites).
    favourites: Vec<RadioStation>,
    /// Directory data (local + countries) from the work thread.
    directory: Option<RadioDirectory>,
    /// True while the first directory fetch is still in flight.
    directory_loading: bool,
    /// Visible region-tree rows (left pane).
    regions: Vec<RegionRow>,
    region_list: ListState,
    /// Stations of the selected region (right pane).
    stations: Vec<StationRow>,
    station_list: ListState,
    /// The region whose stations are shown on the right.
    selected: Option<RegionKind>,
    /// Regions expanded in the tree (states/cities revealed).
    expanded: HashSet<String>,
    /// The list the single keyboard cursor is on (regions or stations);
    /// `d`/`→`/Enter enter a region and move it to the stations, `a`/`←`
    /// back out.
    focus: PaneFocus,
    /// Queue id of a station played via PlayFile; removed from the queue when
    /// the song changes or playback stops so it doesn't linger.
    temp_play_id: Option<u32>,
    initialized: bool,
    regions_area: Rect,
    stations_area: Rect,
    info_area: Rect,
}

impl RadioPane {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            favourites: Vec::new(),
            directory: None,
            directory_loading: false,
            regions: Vec::new(),
            region_list: ListState::default(),
            stations: Vec::new(),
            station_list: ListState::default(),
            selected: None,
            expanded: HashSet::new(),
            focus: PaneFocus::Regions,
            temp_play_id: None,
            initialized: false,
            regions_area: Rect::default(),
            stations_area: Rect::default(),
            info_area: Rect::default(),
        }
    }

    fn max_favourites(&self, ctx: &Ctx) -> usize {
        ctx.config.radio.max_favourites.max(1)
    }

    fn query_favourites(&self, ctx: &Ctx, id: &'static str) {
        let playlist = ctx.config.radio.playlist.clone();
        ctx.query().id(id).replace_id(id).target(PaneType::Radio).query(move |client| {
            Ok(MpdQueryResult::Any(Box::new(fetch_stations(client, &playlist)?)))
        });
    }

    /// Ask the work thread for the station directory (local + countries).
    fn query_directory(&mut self, ctx: &Ctx) {
        self.directory_loading = true;
        let location = ctx.config.radio.location.clone();
        let cache_dir = ctx.config.cache_dir.clone();
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchRadioDirectory { location, cache_dir })
            .map_err(|err| log::error!(error:? = err; "Failed to request radio directory"));
    }

    /// Last cached directory (renders instantly until the background fetch
    /// lands). Cheap file read, done once per session.
    fn load_cached_directory(ctx: &Ctx) -> Option<RadioDirectory> {
        let path = crate::radio::radio_cache_path(ctx.config.cache_dir.as_deref());
        crate::radio::load_radio_cache(&path)
    }

    /// Find a country group by name.
    fn find_country(&self, name: &str) -> Option<&CountryGroup> {
        self.directory.as_ref()?.countries.iter().find(|group| group.name == name)
    }

    fn find_country_mut(&mut self, name: &str) -> Option<&mut CountryGroup> {
        self.directory.as_mut()?.countries.iter_mut().find(|group| group.name == name)
    }

    /// Rebuild the visible region tree from the current data + expansion
    /// state, keeping the selection.
    fn rebuild_regions(&mut self) {
        let prev_key = self.selected.as_ref().map(RegionKind::key);
        let mut regions: Vec<RegionRow> = Vec::new();

        if !self.favourites.is_empty() {
            regions.push(RegionRow {
                kind: RegionKind::Favourites,
                label: "★ Favourites".to_owned(),
                depth: 0,
                expandable: false,
                expanded: false,
            });
        }
        if let Some(directory) = &self.directory {
            if !directory.local.is_empty() {
                regions.push(RegionRow {
                    kind: RegionKind::Local,
                    label: "◎ Local".to_owned(),
                    depth: 0,
                    expandable: false,
                    expanded: false,
                });
            }
            for country in &directory.countries {
                let expanded = self.expanded.contains(&RegionKind::Country(country.name.clone()).key());
                regions.push(RegionRow {
                    kind: RegionKind::Country(country.name.clone()),
                    label: crate::radio::short_country_name(&country.name),
                    depth: 0,
                    expandable: true,
                    expanded,
                });
                if !expanded {
                    continue;
                }
                let Some(states) = country.states.as_ref() else { continue };
                for state in states {
                    // Provinces are the deepest category — no arrow, no
                    // children; selecting one filters the station list.
                    regions.push(RegionRow {
                        kind: RegionKind::State {
                            country: country.name.clone(),
                            state: state.name.clone(),
                        },
                        label: state.name.clone(),
                        depth: 1,
                        expandable: false,
                        expanded: false,
                    });
                }
            }
        }

        self.regions = regions;
        // Keep the tree selection on the previously selected region.
        if let Some(key) = prev_key {
            if let Some(idx) = self.regions.iter().position(|row| row.kind.key() == key) {
                self.region_list.select(Some(idx));
                return;
            }
        }
        self.region_list.select(if self.regions.is_empty() { None } else { Some(0) });
    }

    /// Populate the right station list for a region, loading data lazily.
    fn select_region(&mut self, kind: &RegionKind, ctx: &Ctx) -> Result<()> {
        self.selected = Some(kind.clone());
        self.ensure_region_data(kind, ctx);
        self.populate_stations(kind);
        ctx.render()?;
        Ok(())
    }

    /// Persist the current directory (with any lazily completed / refreshed
    /// region station caches) to disk, so the next start renders instantly.
    fn save_directory_cache(&self, ctx: &Ctx) {
        if let Some(directory) = &self.directory {
            let path = crate::radio::radio_cache_path(ctx.config.cache_dir.as_deref());
            crate::radio::save_radio_cache(&path, directory);
        }
    }

    /// Trigger lazy loads (states of a country, stations of a state). The
    /// top-100 station cache of a region is only filled here — never
    /// reloaded (reloads happen when looking at sub-regions, see
    /// `set_region_expanded`).
    fn ensure_region_data(&mut self, kind: &RegionKind, ctx: &Ctx) {
        let Some(directory) = self.directory.as_mut() else { return };
        match kind {
            RegionKind::Country(country) => {
                let Some(group) = directory.countries.iter_mut().find(|g| &g.name == country) else {
                    return;
                };
                let country = country.clone();
                let country_code = group.code.clone();
                // First look at a region: complete its cached station list
                // to the top 100 (the directory fetch may only have carried
                // a handful).
                if group.top.len() < 100
                    && let Some(code) = country_code.clone()
                {
                    let _ = ctx
                        .work_sender
                        .send(WorkRequest::FetchRadioCountryStations {
                            country: country.clone(),
                            country_code: code,
                        })
                        .map_err(|err| {
                            log::error!(error:? = err; "Failed to request radio country stations")
                        });
                }
                if group.states.is_none() {
                    let _ = ctx
                        .work_sender
                        .send(WorkRequest::FetchRadioStates { country, country_code })
                        .map_err(|err| {
                            log::error!(error:? = err; "Failed to request radio states")
                        });
                }
            }
            RegionKind::State { country, state } => {
                let Some(group) = directory.countries.iter_mut().find(|g| &g.name == country) else {
                    return;
                };
                // The state must exist in the tree (its stations load into
                // the group); the actual fetch happens below.
                let state_exists = group.states.as_ref().is_some_and(|states| {
                    states.iter().any(|s| &s.name == state)
                });
                if !state_exists {
                    return;
                }
                // Looking at a sub-region refreshes **its** cache: the
                // highlighted state's stations are re-fetched every time it
                // is selected (the parent region's list is never touched).
                let country = country.clone();
                let state = state.clone();
                let _ = ctx
                    .work_sender
                    .send(WorkRequest::FetchRadioStateStations { country, state })
                    .map_err(|err| {
                        log::error!(error:? = err; "Failed to request radio state stations")
                    });
            }
            _ => {}
        }
    }

    /// Build the right-pane station list for a region from whatever data is
    /// loaded. Unloaded data shows a placeholder row. Repopulations keep the
    /// selected station highlighted: a background data arrival for the
    /// region already on screen (directory refresh, state/station cache
    /// loads, top-100 fills) must not yank the cursor back to the top.
    fn populate_stations(&mut self, kind: &RegionKind) {
        // Remember the selection before rebuilding the list: restore the
        // same station when it survives, otherwise keep the same position
        // (clamped) instead of jumping to row 0.
        let previous_url = self
            .station_list
            .selected()
            .and_then(|idx| self.stations.get(idx))
            .map(|row| row.url().to_owned());
        let previous_idx = self.station_list.selected().unwrap_or(0);
        self.station_list.select(None);
        match kind {
            RegionKind::Favourites => {
                self.stations =
                    self.favourites.iter().cloned().map(StationRow::Favourite).collect();
            }
            RegionKind::Local => {
                self.stations = self
                    .directory
                    .as_ref()
                    .map(|d| {
                        d.local.iter().cloned().map(StationRow::Directory).collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
            }
            RegionKind::Country(country) => {
                self.stations = self
                    .find_country(country)
                    .map(|group| {
                        group.top.iter().cloned().map(StationRow::Directory).collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
            }
            RegionKind::State { country, state } => {
                let Some(group) = self.find_country(country) else {
                    self.stations = Vec::new();
                    return;
                };
                let Some(state_group) =
                    group.states.as_ref().and_then(|s| s.iter().find(|s| &s.name == state))
                else {
                    self.stations = Vec::new();
                    return;
                };
                self.stations = state_group
                    .stations
                    .as_ref()
                    .map(|stations| {
                        stations.iter().cloned().map(StationRow::Directory).collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
            }
        }
        if !self.stations.is_empty() {
            let idx = previous_url
                .as_deref()
                .and_then(|url| self.stations.iter().position(|row| row.url() == url))
                .unwrap_or_else(|| previous_idx.min(self.stations.len().saturating_sub(1)));
            self.station_list.select(Some(idx));
        }
    }

    /// Expand (or collapse) a tree region. Expanding a country fetches its
    /// states (once) — it does **not** reload the country's station list:
    /// caches only refresh for the specific sub-region being highlighted
    /// (a state selection re-fetches that state's stations).
    fn set_region_expanded(&mut self, kind: &RegionKind, expanded: bool, ctx: &Ctx) -> Result<()> {
        let key = kind.key();
        if expanded {
            self.expanded.insert(key);
            self.ensure_region_data(kind, ctx);
        } else {
            self.expanded.remove(&key);
        }
        self.rebuild_regions();
        ctx.render()?;
        Ok(())
    }

    /// Toggle the selected tree row's expansion.
    /// Move the tree selection; keeps the right pane in sync.
    fn move_region(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        if self.regions.is_empty() {
            return Ok(());
        }
        let current = self.region_list.selected().unwrap_or(0) as i64;
        let new_idx = (current + dir).clamp(0, self.regions.len() as i64 - 1) as usize;
        if new_idx != current as usize {
            self.region_list.select(Some(new_idx));
            let kind = self.regions[new_idx].kind.clone();
            self.select_region(&kind, ctx)?;
        }
        Ok(())
    }

    /// Move the station selection (right pane).
    fn move_station(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        if self.stations.is_empty() {
            return Ok(());
        }
        let current = self.station_list.selected().unwrap_or(0) as i64;
        let new_idx = (current + dir).clamp(0, self.stations.len() as i64 - 1) as usize;
        if new_idx != current as usize {
            self.station_list.select(Some(new_idx));
            ctx.render()?;
        }
        Ok(())
    }

    fn selected_station(&self) -> Option<RadioStation> {
        let idx = self.station_list.selected()?;
        match self.stations.get(idx) {
            Some(StationRow::Favourite(station)) => {
                Some(RadioStation { name: station.name.clone(), url: station.url.clone() })
            }
            Some(StationRow::Directory(station)) => {
                Some(RadioStation { name: station.name.clone(), url: station.url.clone() })
            }
            None => None,
        }
    }

    /// URL of the currently playing stream, if any.
    fn playing_url(ctx: &Ctx) -> Option<String> {
        ctx.find_current_song_in_queue()
            .map(|(_, song)| song.file.clone())
            .filter(|file| is_stream_url(file))
    }

    /// Play the highlighted station without adding it to the queue (it is
    /// removed again once the song changes or playback stops).
    fn play_selected(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(station) = self.selected_station() else { return Ok(()) };
        let url = station.url;
        ctx.query().id(PLAY).replace_id(PLAY).target(PaneType::Radio).query(move |client| {
            let id = client.add_id(&url, None)?;
            client.play_id(id)?;
            Ok(MpdQueryResult::Any(Box::new(id)))
        });
        Ok(())
    }

    /// `d`/`→`/Enter on the region tree: expand a country, or enter a
    /// leaf region — the cursor moves to its station list.
    fn open_region(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(idx) = self.region_list.selected() else { return Ok(()) };
        let Some(row) = self.regions.get(idx).cloned() else { return Ok(()) };
        if row.expandable && !row.expanded {
            return self.set_region_expanded(&row.kind, true, ctx);
        }
        // A leaf region (or one already expanded) is selected; entering
        // moves the cursor to its station list.
        self.focus = PaneFocus::Stations;
        self.select_region(&row.kind, ctx)
    }

    /// `a`/`←`: back out one level. On the station list the cursor
    /// returns to the region tree; on the tree an expanded branch
    /// collapses in place, otherwise the cursor moves up to the parent
    /// region, collapsing the branch left (the MPD back-out behavior).
    fn back_out(&mut self, ctx: &Ctx) -> Result<()> {
        self.focus = PaneFocus::Regions;
        let Some(idx) = self.region_list.selected() else { return Ok(()) };
        let Some(row) = self.regions.get(idx).cloned() else { return Ok(()) };
        if row.expandable && row.expanded {
            return self.set_region_expanded(&row.kind, false, ctx);
        }
        // Move to the parent region (nearest shallower row), collapsing
        // the branch we leave. The parent is selected first so the
        // rebuild (after the collapse) keeps the cursor on it.
        if let Some(parent) = self.regions[..idx].iter().rposition(|r| r.depth < row.depth) {
            let parent_row = self.regions[parent].clone();
            self.select_region(&parent_row.kind, ctx)?;
            self.region_list.select(Some(parent));
            if parent_row.expandable && parent_row.expanded {
                self.set_region_expanded(&parent_row.kind, false, ctx)?;
            }
            return Ok(());
        }
        ctx.render()?;
        Ok(())
    }

    /// Drop the temporary play song once playback has moved on.
    fn cleanup_temp_play(&mut self, ctx: &Ctx) {
        if let Some(temp) = self.temp_play_id
            && ctx.status.songid != Some(temp)
        {
            self.temp_play_id = None;
            ctx.temp_play_id.set(None);
            ctx.command(move |client| {
                client.delete_id(temp)?;
                Ok(())
            });
        }
    }

    /// Read the current favourites, apply `mutate`, write the file back and
    /// refresh the list.
    pub(crate) fn mutate_stations(
        ctx: &Ctx,
        playlist: String,
        mutate: impl FnOnce(&mut Vec<RadioStation>) + Send + 'static,
    ) -> Result<()> {
        let playlist_name = playlist.clone();
        let result = ctx.query_sync(move |client| {
            let mut stations = fetch_stations(client, &playlist_name)?;
            mutate(&mut stations);
            write_stations_file(client, &playlist_name, &stations)?;
            Ok(stations)
        })?;
        status_info!("Favourites updated ({} stations)", result.len());
        // Re-read so the list matches the file (MPD also picks it up via
        // inotify, but be explicit).
        ctx.query()
            .id(REINIT)
            .replace_id(REINIT)
            .target(PaneType::Radio)
            .query(move |client| {
                Ok(MpdQueryResult::Any(Box::new(fetch_stations(client, &playlist)?)))
            });
        ctx.render()?;
        Ok(())
    }

    fn is_favourite(&self, url: &str) -> bool {
        self.favourites.iter().any(|station| station.url == url)
    }

    fn open_menu(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(station) = self.selected_station() else { return Ok(()) };
        let name = station.name.clone();
        let url = station.url.clone();
        let playlist = ctx.config.radio.playlist.clone();
        let is_favourite = self.is_favourite(&url);
        let favourites = self.favourites.clone();
        let max = self.max_favourites(ctx);

        let menu = MenuModal::new(ctx)
            .list_section(ctx, |mut section| {
                section.add_item(
                    "Play now",
                    {
                        let url = url.clone();
                        move |ctx| {
                            ctx.command(move |client| {
                                let id = client.add_id(&url, None)?;
                                client.play_id(id)?;
                                Ok(())
                            });
                            Ok(())
                        }
                    },
                );
                if is_favourite {
                    section.add_item(
                        "Remove from favourites",
                        {
                            let playlist = playlist.clone();
                            let url = url.clone();
                            move |ctx| {
                                Self::mutate_stations(ctx, playlist, move |stations| {
                                    stations.retain(|station| station.url != url);
                                })?;
                                Ok(())
                            }
                        },
                    );
                    section.add_item(
                        "Rename…",
                        {
                            let playlist = playlist.clone();
                            let name = name.clone();
                            let url = url.clone();
                            move |ctx| {
                                Self::prompt_rename(ctx, playlist, name, url);
                                Ok(())
                            }
                        },
                    );
                } else {
                    section.add_item(
                        "★ Add to favourites",
                        Self::menu_add_favourite(playlist.clone(), station.clone(), favourites, max),
                    );
                }
                section.add_item(
                    "Add to queue",
                    {
                        let url = url.clone();
                        move |ctx| {
                            ctx.command(move |client| {
                                client.add(&url, None)?;
                                Ok(())
                            });
                            Ok(())
                        }
                    },
                );
                section.add_item(
                    "Replace queue with station",
                    {
                        let url = url.clone();
                        move |ctx| {
                            ctx.command(move |client| {
                                client.clear()?;
                                client.add(&url, None)?;
                                client.play()?;
                                Ok(())
                            });
                            Ok(())
                        }
                    },
                );
                Some(section)
            })
            .list_section(ctx, |mut section| {
                section.add_item(
                    "Add station by URL…",
                    Self::menu_add_station(playlist.clone()),
                );
                section.add_item(
                    "Refresh stations from directory",
                    move |ctx| {
                        let location = ctx.config.radio.location.clone();
                        let cache_dir = ctx.config.cache_dir.clone();
                        let _ = ctx
                            .work_sender
                            .send(WorkRequest::FetchRadioDirectory { location, cache_dir })
                            .map_err(|err| {
                                log::error!(error:? = err; "Failed to request radio directory")
                            });
                        status_info!("Refreshing station directory…");
                        Ok(())
                    },
                );
                Some(section)
            });

        modal!(ctx, menu);
        Ok(())
    }

    fn menu_add_favourite(
        playlist: String,
        station: RadioStation,
        favourites: Vec<RadioStation>,
        max: usize,
    ) -> impl FnOnce(&Ctx) -> Result<()> {
        move |ctx| {
            if favourites.iter().any(|s| s.url == station.url) {
                status_warn!("Already in favourites");
                return Ok(());
            }
            if favourites.len() >= max {
                status_warn!(
                    "Favourites full ({max} max) — remove one first (station menu → Remove from favourites)"
                );
                return Ok(());
            }
            Self::mutate_stations(ctx, playlist, move |stations| stations.push(station))?;
            Ok(())
        }
    }

    /// Two chained input modals: name first, then URL.
    fn menu_add_station(playlist: String) -> impl FnOnce(&Ctx) -> Result<()> {
        move |ctx| {
            let playlist = playlist.clone();
            modal!(
                ctx,
                InputModal::new(ctx)
                    .title("Add radio station")
                    .confirm_label("Next")
                    .input_label("Station name:")
                    .on_confirm(move |ctx, name| {
                        let name = name.to_owned();
                        let playlist = playlist.clone();
                        modal!(
                            ctx,
                            InputModal::new(ctx)
                                .title("Add radio station")
                                .confirm_label("Add")
                                .input_label("Stream URL:")
                                .on_confirm(move |ctx, url| {
                                    let url = url.trim().to_owned();
                                    if !is_stream_url(&url) {
                                        status_warn!(
                                            "Not a stream URL (expected http(s)://...): {url}"
                                        );
                                        return Ok(());
                                    }
                                    Self::mutate_stations(ctx, playlist, move |stations| {
                                        stations.push(RadioStation {
                                            name: name.clone(),
                                            url: url.clone(),
                                        });
                                    })?;
                                    Ok(())
                                })
                        );
                        Ok(())
                    })
            );
            Ok(())
        }
    }

    fn prompt_rename(ctx: &Ctx, playlist: String, name: String, url: String) {
        modal!(
            ctx,
            InputModal::new(ctx)
                .title("Rename radio station")
                .confirm_label("Rename")
                .input_label("New name:")
                .initial_value(name)
                .on_confirm(move |ctx, new_name| {
                    let new_name = new_name.to_owned();
                    Self::mutate_stations(ctx, playlist, move |stations| {
                        if let Some(station) = stations.iter_mut().find(|s| s.url == url) {
                            station.name = new_name;
                        }
                    })?;
                    Ok(())
                })
        );
    }

    fn confirm_delete(ctx: &Ctx, playlist: String, name: String, url: String) {
        modal!(
            ctx,
            ConfirmModal::builder()
                .ctx(ctx)
                .message(vec![
                    format!("Remove station '{name}'?"),
                    "It will be deleted from the radio playlist.".to_owned(),
                ])
                .action(Action::Single {
                    confirm_label: Some("Delete"),
                    cancel_label: None,
                    on_confirm: Box::new(move |ctx| {
                        Self::mutate_stations(ctx, playlist, move |stations| {
                            stations.retain(|s| s.url != url);
                        })?;
                        Ok(())
                    }),
                })
                .size((45, 6))
                .build()
        );
    }

    fn render_regions(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let base = ctx.config.as_list_name_style();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(" Regions ");
        let inner = block.inner(area);
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            inner,
            self.region_list.offset(),
            self.regions.len(),
            1,
        );
        let hovered = ctx.config.theme.hovered_item_style;
        let items: Vec<ListItem> = self
            .regions
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let indent = "  ".repeat(usize::from(row.depth));
                let arrow = if row.expandable {
                    if row.expanded { "▼ " } else { "▶ " }
                } else {
                    ""
                };
                let line = format!("{indent}{arrow}{}", row.label);
                let mut item = ListItem::new(Line::from(line));
                if hover_idx == Some(idx) {
                    item = item.style(hovered);
                }
                item
            })
            .collect();

        ratatui::widgets::StatefulWidget::render(
            List::new(items)
                .highlight_style(if hover_idx == self.region_list.selected()
                    || self.focus == PaneFocus::Regions
                {
                    // The row under the mouse, or the keyboard cursor when
                    // this pane is the one being navigated.
                    ctx.config.theme.hovered_item_style
                } else {
                    ctx.config.theme.current_item_style
                })
                .style(base),
            inner,
            frame.buffer_mut(),
            &mut self.region_list,
        );
        ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
        self.regions_area = inner;
    }

    fn render_stations(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let playing_url = Self::playing_url(ctx);
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        let title = match &self.selected {
            Some(RegionKind::Favourites) => " Favourites ".to_owned(),
            Some(RegionKind::Local) => " Local — closest ".to_owned(),
            Some(RegionKind::Country(name)) => format!(" {name} "),
            Some(RegionKind::State { state, .. }) => format!(" {state} "),
            None => " Stations ".to_owned(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(format!("{title}({}) ", self.stations.len()));
        let inner = block.inner(area);
        // Station rows are two lines tall (name + subline): the hover row
        // maps both lines to the same item.
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            inner,
            self.station_list.offset(),
            self.stations.len(),
            2,
        );
        let hovered = ctx.config.theme.hovered_item_style;

        let items: Vec<ListItem> = self
            .stations
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let style = if hover_idx == Some(idx) { hovered } else { Style::new() };
                match row {
                    StationRow::Favourite(station) => {
                        let is_playing = playing_url.as_deref() == Some(station.url.as_str());
                        let prefix = if is_playing { "▶ " } else { "  " };
                        let name_style = if is_playing { base.add_modifier(Modifier::BOLD) } else { base };
                        ListItem::new(vec![
                            Line::from(Span::styled(format!("{prefix}{}", station.name), name_style)),
                            Line::from(Span::styled(format!("  {}", station.url), dim)),
                        ])
                        .style(style)
                    }
                    StationRow::Directory(station) => {
                        let is_playing = playing_url.as_deref() == Some(station.url.as_str());
                        let prefix = if is_playing { "▶ " } else { "  " };
                        let name_style = if is_playing { base.add_modifier(Modifier::BOLD) } else { base };
                        ListItem::new(vec![
                            Line::from(Span::styled(format!("{prefix}{}", station.name), name_style)),
                            Line::from(Span::styled(format!("  {}", station_subline(station)), dim)),
                        ])
                        .style(style)
                    }
                }
            })
            .collect();

        ratatui::widgets::StatefulWidget::render(
            List::new(items)
                .highlight_style(if hover_idx == self.station_list.selected()
                    || self.focus == PaneFocus::Stations
                {
                    // The row under the mouse, or the keyboard cursor when
                    // this pane is the one being navigated.
                    ctx.config.theme.hovered_item_style
                } else {
                    ctx.config.theme.current_item_style
                })
                .style(base),
            inner,
            frame.buffer_mut(),
            &mut self.station_list,
        );
        ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
        self.stations_area = inner;
    }

    /// Keybinding hints, one line each, in the strip between the station list
    /// and the info panel (same spot as the Directories/Playlists tabs).
    fn render_tips(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        let tips = vec![
            Line::from(vec![
                Span::styled("w/s · ↑/↓", base),
                Span::styled("  move list", dim),
            ]),
            Line::from(vec![
                Span::styled("d / →", base),
                Span::styled("  open region · play station", dim),
            ]),
            Line::from(vec![
                Span::styled("a / ←", base),
                Span::styled("  back out", dim),
                Span::styled("Enter", base),
                Span::styled("  context menu", dim),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(tips).style(dim),
            area.inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 }),
        );
    }

    fn render_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let key = ctx.config.theme.preview_label_style;
        let group = ctx.config.theme.preview_metadata_group_style;
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();

        let mut items: Vec<ListItem> = Vec::new();
        match self.selected_station() {
            Some(RadioStation { name, url }) if self.favourites.iter().any(|f| f.url == url) => {
                items.push(ListItem::new(Line::styled(" --- [Favourite]", group)));
                items.push(ListItem::new(Line::from(vec![
                    Span::styled("Name", key),
                    Span::raw(": "),
                    Span::raw(name),
                ])));
                items.push(ListItem::new(Line::from(vec![
                    Span::styled("URL", key),
                    Span::raw(": "),
                    Span::styled(url, dim),
                ])));
            }
            Some(RadioStation { name, url }) => {
                let station = self
                    .stations
                    .iter()
                    .find_map(|row| match row {
                        StationRow::Directory(s) if s.url == url => Some(s.clone()),
                        _ => None,
                    });
                match station {
                    Some(station) => {
                        items.push(ListItem::new(Line::styled(" --- [Station]", group)));
                        items.push(ListItem::new(Line::from(vec![
                            Span::styled("Name", key),
                            Span::raw(": "),
                            Span::raw(station.name.clone()),
                        ])));
                        if let Some(dist) = format_distance(station.distance_km) {
                            items.push(ListItem::new(Line::from(vec![
                                Span::styled("Distance", key),
                                Span::raw(": "),
                                Span::raw(dist),
                            ])));
                        }
                        items.push(ListItem::new(Line::from(vec![
                            Span::styled("Country", key),
                            Span::raw(format!(
                                ": {}{}",
                                station.country,
                                station
                                    .country_code
                                    .chars()
                                    .all(|c| c.is_ascii_uppercase())
                                    .then(|| format!(" ({})", station.country_code))
                                    .unwrap_or_default()
                            )),
                        ])));
                        if let Some(state) = &station.state {
                            items.push(ListItem::new(Line::from(vec![
                                Span::styled("State", key),
                                Span::raw(": "),
                                Span::raw(state.clone()),
                            ])));
                        }
                        if let Some(city) = &station.city {
                            items.push(ListItem::new(Line::from(vec![
                                Span::styled("City", key),
                                Span::raw(": "),
                                Span::raw(city.clone()),
                            ])));
                        }
                        if !station.language.is_empty() {
                            items.push(ListItem::new(Line::from(vec![
                                Span::styled("Language", key),
                                Span::raw(": "),
                                Span::raw(station.language.join(", ")),
                            ])));
                        }
                        if !station.tags.is_empty() {
                            items.push(ListItem::new(Line::from(vec![
                                Span::styled("Tags", key),
                                Span::raw(": "),
                                Span::raw(station.tags.join(", ")),
                            ])));
                        }
                        if let (Some(codec), Some(bitrate)) = (&station.codec, station.bitrate) {
                            items.push(ListItem::new(Line::from(vec![
                                Span::styled("Stream", key),
                                Span::raw(format!(": {codec} {bitrate} kbps")),
                            ])));
                        } else if let Some(bitrate) = station.bitrate {
                            items.push(ListItem::new(Line::from(vec![
                                Span::styled("Stream", key),
                                Span::raw(format!(": {bitrate} kbps")),
                            ])));
                        }
                        if station.votes > 0 {
                            items.push(ListItem::new(Line::from(vec![
                                Span::styled("Votes", key),
                                Span::raw(format!(": {}", station.votes)),
                            ])));
                        }
                        items.push(ListItem::new(Line::from(vec![
                            Span::styled("URL", key),
                            Span::raw(": "),
                            Span::styled(station.url.clone(), dim),
                        ])));
                    }
                    None => {
                        items.push(ListItem::new(Line::styled(" --- [Station]", group)));
                        items.push(ListItem::new(Line::from(vec![
                            Span::styled("Name", key),
                            Span::raw(": "),
                            Span::raw(name),
                        ])));
                        items.push(ListItem::new(Line::from(vec![
                            Span::styled("URL", key),
                            Span::raw(": "),
                            Span::styled(url, dim),
                        ])));
                    }
                }
            }
            None => {
                items.push(ListItem::new(Line::styled(" --- [Region]", group)));
                let label = match &self.selected {
                    Some(RegionKind::Favourites) => "Favourites".to_owned(),
                    Some(RegionKind::Local) => "Local — closest stations".to_owned(),
                    Some(RegionKind::Country(name)) => name.clone(),
                    Some(RegionKind::State { state, .. }) => state.clone(),
                    None => "No region selected".to_owned(),
                };
                items.push(ListItem::new(Line::from(vec![
                    Span::styled("Region", key),
                    Span::raw(": "),
                    Span::raw(label),
                ])));
                if self.stations.is_empty() {
                    items.push(ListItem::new(Line::from(vec![
                        Span::styled("Hint", key),
                        Span::raw(": "),
                        Span::styled("Pick a region on the left.", dim),
                    ])));
                }
            }
        }

        // Live stream info when a station is playing.
        if let Some((_, song)) = ctx.find_current_song_in_queue()
            && is_stream_url(&song.file)
            && ctx.status.state == State::Play
        {
            items.push(ListItem::new(""));
            items.push(ListItem::new(Line::styled(" --- [Now playing]", group)));
            if let Some(title) = song.metadata.get("title") {
                items.push(ListItem::new(Line::from(vec![
                    Span::styled("Title", key),
                    Span::raw(": "),
                    Span::raw(title.join(", ").into_owned()),
                ])));
            }
            if let Some(artist) = song.metadata.get("artist") {
                items.push(ListItem::new(Line::from(vec![
                    Span::styled("Artist", key),
                    Span::raw(": "),
                    Span::raw(artist.join(", ").into_owned()),
                ])));
            }
            if let Some(name) = song.metadata.get("name") {
                items.push(ListItem::new(Line::from(vec![
                    Span::styled("Stream", key),
                    Span::raw(": "),
                    Span::raw(name.join(", ").into_owned()),
                ])));
            }
            if let Some(bitrate) = ctx.status.bitrate {
                items.push(ListItem::new(Line::from(vec![
                    Span::styled("Bitrate", key),
                    Span::raw(format!(": {bitrate} kbps")),
                ])));
            }
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(area);
        ratatui::widgets::Widget::render(
            List::new(items).block(block).style(base),
            area,
            frame.buffer_mut(),
        );
        self.info_area = inner;
    }
}

impl Pane for RadioPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        // Directories-style: region tree left, station list + info right.
        let [regions_area, right] = Layout::horizontal([
            Constraint::Percentage(30),
            Constraint::Percentage(70),
        ])
        .areas(area);
        let [stations_area, tips_area, info_area] = Layout::vertical([
            Constraint::Percentage(60),
            Constraint::Length(3),
            Constraint::Percentage(33),
        ])
        .areas(right);

        self.render_regions(frame, regions_area, ctx);
        self.render_stations(frame, stations_area, ctx);
        self.render_tips(frame, tips_area, ctx);
        self.render_info(frame, info_area, ctx);
        Ok(())
    }

    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        let id = if self.initialized { REINIT } else { INIT };
        self.query_favourites(ctx, id);
        if self.directory.is_none() {
            // Show the cached directory immediately, then refresh in the
            // background (the work request always runs).
            if let Some(cached) = Self::load_cached_directory(ctx) {
                self.directory = Some(cached);
                self.rebuild_regions();
            }
            self.query_directory(ctx);
        }
        self.initialized = true;
        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::StoredPlaylist => self.query_favourites(ctx, REINIT),
            UiEvent::SongChanged => self.cleanup_temp_play(ctx),
            // Fired after ctx.status is refreshed (unlike Player, which
            // arrives while the status is still stale), so the Stop
            // transition is reliably visible here.
            UiEvent::PlaybackStateChanged => {
                if ctx.status.state == State::Stop
                    && let Some(temp) = self.temp_play_id
                {
                    self.temp_play_id = None;
                    ctx.temp_play_id.set(None);
                    ctx.command(move |client| {
                        client.delete_id(temp)?;
                        Ok(())
                    });
                }
            }
            UiEvent::Player => {
                // Streams keep the same queue entry while playing, so
                // SongChanged never fires. After stop, MPD still reports the
                // last songid, so drop the temp entry on the state transition
                // itself instead of waiting for a song change.
                if ctx.status.state == State::Stop
                    && let Some(temp) = self.temp_play_id
                {
                    self.temp_play_id = None;
                    ctx.temp_play_id.set(None);
                    ctx.command(move |client| {
                        client.delete_id(temp)?;
                        Ok(())
                    });
                }
            }
            UiEvent::Reconnected => {
                self.initialized = false;
                self.temp_play_id = None;
                self.before_show(ctx)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        // Left pane: the region tree.
        if self.regions_area.contains(event.into()) {
            self.focus = PaneFocus::Regions;
            match event.kind {
                MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                    let row = usize::from(event.y.saturating_sub(self.regions_area.y));
                    let idx = self.region_list.offset() + row;
                    if let Some(region) = self.regions.get(idx).cloned() {
                        self.region_list.select(Some(idx));
                        let is_double = matches!(event.kind, MouseEventKind::DoubleClick);
                        if is_double && region.expandable {
                            self.set_region_expanded(&region.kind, !region.expanded, ctx)?;
                        } else {
                            self.select_region(&region.kind, ctx)?;
                        }
                    }
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                    self.move_region(dir, ctx)?;
                }
                _ => {}
            }
            return Ok(());
        }

        // Right pane: the station list.
        if self.stations_area.contains(event.into()) {
            self.focus = PaneFocus::Stations;
            match event.kind {
                MouseEventKind::RightClick => {
                    let row = usize::from(event.y.saturating_sub(self.stations_area.y));
                    // Stations are two lines each.
                    let idx = self.station_list.offset() + row / 2;
                    if idx < self.stations.len() {
                        self.station_list.select(Some(idx));
                        ctx.render()?;
                        return self.open_menu(ctx);
                    }
                }
                MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                    let row = usize::from(event.y.saturating_sub(self.stations_area.y));
                    let idx = self.station_list.offset() + row / 2;
                    if idx < self.stations.len() {
                        self.station_list.select(Some(idx));
                        if matches!(event.kind, MouseEventKind::DoubleClick) {
                            self.play_selected(ctx)?;
                        }
                        ctx.render()?;
                    }
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                    self.move_station(dir, ctx)?;
                }
                _ => {}
            }
            return Ok(());
        }

        Ok(())
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &mut Ctx) -> Result<()> {
        // Input modals manage their own buffers; nothing to do here.
        let _ = (kind, ctx);
        Ok(())
    }

    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        // The MPD / Jellyfin / Playlists scheme: one cursor on the list
        // in focus — the region tree, or the station list once a region
        // is entered. w/s/↑/↓ all move that same cursor, `d`/`→`/Enter
        // open a region or play a station, `a`/`←` back out.
        if let Some(action) = event.claim_directories() {
            return match action {
                DirectoriesActions::FolderUp | DirectoriesActions::FolderDown => {
                    let dir =
                        if matches!(action, DirectoriesActions::FolderUp) { -1 } else { 1 };
                    if self.focus == PaneFocus::Stations {
                        self.move_station(dir, ctx)
                    } else {
                        self.move_region(dir, ctx)
                    }
                }
                // `d` mirrors `→`: on the region tree it opens the
                // highlighted region (expanding a country, entering a
                // leaf), on the station list it plays the station.
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    if self.focus == PaneFocus::Stations {
                        self.play_selected(ctx)
                    } else {
                        self.open_region(ctx)
                    }
                }
                // `a` backs out one level (collapse / move up the tree).
                DirectoriesActions::FolderCollapse => self.back_out(ctx),
            };
        }
        if let Some(action) = event.claim_common() {
            match action {
                // ↑/↓ move the same list w/s move (the focused one).
                CommonAction::Up | CommonAction::Down => {
                    let dir = if matches!(action, CommonAction::Up) { -1 } else { 1 };
                    return if self.focus == PaneFocus::Stations {
                        self.move_station(dir, ctx)
                    } else {
                        self.move_region(dir, ctx)
                    };
                }
                CommonAction::Left => return self.back_out(ctx),
                CommonAction::Top => {
                    if self.focus == PaneFocus::Stations {
                        if !self.stations.is_empty() {
                            self.station_list.select(Some(0));
                            ctx.render()?;
                        }
                    } else if let Some(first) = self.regions.first() {
                        let kind = first.kind.clone();
                        self.region_list.select(Some(0));
                        self.select_region(&kind, ctx)?;
                    }
                    return Ok(());
                }
                CommonAction::Bottom => {
                    if self.focus == PaneFocus::Stations {
                        if !self.stations.is_empty() {
                            self.station_list.select(Some(self.stations.len() - 1));
                            ctx.render()?;
                        }
                    } else if let Some(last) = self.regions.last() {
                        let kind = last.kind.clone();
                        self.region_list.select(Some(self.regions.len() - 1));
                        self.select_region(&kind, ctx)?;
                    }
                    return Ok(());
                }
                CommonAction::Confirm => {
                    // Enter: on the region tree it opens the highlighted
                    // region (like `d`), on the station list it opens the
                    // station's context menu (like right-click).
                    return if self.focus == PaneFocus::Stations {
                        self.open_menu(ctx)
                    } else {
                        self.open_region(ctx)
                    };
                }
                CommonAction::ContextMenu => return self.open_menu(ctx),
                CommonAction::Delete => {
                    if let Some(idx) = self.station_list.selected()
                        && let Some(StationRow::Favourite(station)) = self.stations.get(idx)
                    {
                        Self::confirm_delete(
                            ctx,
                            ctx.config.radio.playlist.clone(),
                            station.name.clone(),
                            station.url.clone(),
                        );
                    }
                    return Ok(());
                }
                _ => event.abandon(),
            }
        }
        Ok(())
    }

    fn on_query_finished(
        &mut self,
        id: &'static str,
        mpd_command: MpdQueryResult,
        _is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match (id, mpd_command) {
            (INIT | REINIT, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<Vec<RadioStation>>() {
                    self.favourites = *boxed;
                    self.rebuild_regions();
                    // Keep the right pane in sync if Favourites is open.
                    if matches!(self.selected, Some(RegionKind::Favourites)) {
                        self.populate_stations(&RegionKind::Favourites);
                    }
                    ctx.render()?;
                }
            }
            (RADIO_DIRECTORY, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<RadioDirectory>() {
                    let fresh = *boxed;
                    self.directory_loading = false;
                    // A failed background refresh must never clobber the
                    // working directory (loaded from the disk cache): keep
                    // the cached regions and surface the error as a notice.
                    // Only adopt the failed result when nothing is loaded at
                    // all (no cache, first run).
                    if let Some(err) = &fresh.error
                        && self.directory.is_some()
                    {
                        status_warn!("Radio directory refresh failed: {err}");
                    } else {
                        self.directory = Some(fresh);
                    }
                    self.rebuild_regions();
                    // Auto-select the first region so the right pane is not
                    // empty on the first open.
                    if self.selected.is_none()
                        && let Some(first) = self.regions.first().map(|r| r.kind.clone())
                    {
                        self.select_region(&first, ctx)?;
                    } else if let Some(kind) = self.selected.clone() {
                        // Re-populate the right pane for the currently
                        // selected region now that the data is fresh.
                        self.populate_stations(&kind);
                    }
                    ctx.render()?;
                }
            }
            (RADIO_STATES, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<(
                    String,
                    Result<Vec<crate::radio::StateGroup>, anyhow::Error>,
                )>() {
                    let (country, result) = *boxed;
                    match result {
                        Ok(states) => {
                            if let Some(group) = self.find_country_mut(&country) {
                                group.states = Some(states);
                            }
                            self.rebuild_regions();
                            if let Some(kind) = self.selected.clone() {
                                self.populate_stations(&kind);
                            }
                            self.save_directory_cache(ctx);
                            ctx.render()?;
                        }
                        Err(err) => status_warn!("Cannot load states: {err}"),
                    }
                }
            }
            (RADIO_COUNTRY_STATIONS, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<(
                    String,
                    Result<Vec<DirectoryStation>, anyhow::Error>,
                )>() {
                    let (country, result) = *boxed;
                    match result {
                        Ok(stations) => {
                            if let Some(group) = self.find_country_mut(&country) {
                                group.top = stations;
                            }
                            if let Some(kind) = self.selected.clone() {
                                self.populate_stations(&kind);
                            }
                            self.save_directory_cache(ctx);
                            ctx.render()?;
                        }
                        Err(err) => status_warn!("Cannot load {country} stations: {err}"),
                    }
                }
            }
            (RADIO_STATE_STATIONS, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<(
                    String,
                    String,
                    Result<Vec<DirectoryStation>, anyhow::Error>,
                )>() {
                    let (country, state, result) = *boxed;
                    match result {
                        Ok(stations) => {
                            if let Some(directory) = self.directory.as_mut()
                                && let Some(group) =
                                    directory.countries.iter_mut().find(|g| g.name == country)
                                && let Some(state_group) = group.states.as_mut().and_then(
                                    |states| states.iter_mut().find(|s| s.name == state),
                                )
                            {
                                state_group.stations = Some(stations);
                            }
                            self.rebuild_regions();
                            if let Some(kind) = self.selected.clone() {
                                self.populate_stations(&kind);
                            }
                            self.save_directory_cache(ctx);
                            ctx.render()?;
                        }
                        Err(err) => status_warn!("Cannot load state stations: {err}"),
                    }
                }
            }
            (PLAY, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<u32>() {
                    self.temp_play_id = Some(*boxed);
                }
            }
            // Pasted/dropped files played via the paste popup: the Radio pane
            // owns the temporary entry lifecycle (hidden from the queue,
            // removed on song change / stop), so its result lands here.
            (crate::ui::modals::paste::PASTE_PLAY, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<u32>() {
                    self.temp_play_id = Some(*boxed);
                    ctx.temp_play_id.set(Some(*boxed));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rstest::rstest;

    use super::{RadioPane, RadioStation, RegionKind, PaneFocus, is_stream_url, m3u_content};
    use crate::{
        MpdQueryResult,
        ctx::Ctx,
        mpd::commands::Song,
        radio::{CountryGroup, DirectoryStation, RadioDirectory, StateGroup},
        shared::events::WorkRequest,
        tests::fixtures::ctx,
        ui::panes::Pane,
    };

    fn song(url: &str, name: Option<&str>) -> Song {
        let mut metadata = HashMap::new();
        if let Some(name) = name {
            metadata.insert("name".to_owned(), name.into());
        }
        Song {
            id: 0,
            file: url.to_owned(),
            duration: None,
            metadata,
            last_modified: chrono::Utc::now(),
            added: None,
        }
    }

    fn station(name: &str, url: &str) -> RadioStation {
        RadioStation { name: name.to_owned(), url: url.to_owned() }
    }

    fn dir_station(name: &str, country: &str) -> DirectoryStation {
        DirectoryStation {
            name: name.to_owned(),
            url: format!("http://{}.example/stream", name.to_lowercase()),
            country: country.to_owned(),
            country_code: "XX".to_owned(),
            state: None,
            city: None,
            language: Vec::new(),
            tags: Vec::new(),
            codec: None,
            bitrate: None,
            votes: 1,
            distance_km: None,
            geo_lat: None,
            geo_long: None,
            favicon: None,
            homepage: None,
        }
    }

    fn load_favourites(pane: &mut RadioPane, ctx: &Ctx, stations: Vec<RadioStation>) {
        pane.on_query_finished(
            super::INIT,
            MpdQueryResult::Any(Box::new(stations)),
            true,
            ctx,
        )
        .unwrap();
    }

    /// A directory with one country ("Germany", top 2 + full list) and a
    /// Local section of 2 stations.
    fn load_directory(pane: &mut RadioPane, ctx: &Ctx) {
        let full = vec![dir_station("G1", "Germany"), dir_station("G2", "Germany")];
        let directory = RadioDirectory {
            location: None,
            country_code: None,
            local: vec![dir_station("L1", "Canada"), dir_station("L2", "Canada")],
            countries: vec![CountryGroup {
                name: "Germany".to_owned(),
                code: Some("DE".to_owned()),
                top: full,
                states: None,
            }],
            error: None,
        };
        pane.on_query_finished(
            super::RADIO_DIRECTORY,
            MpdQueryResult::Any(Box::new(directory)),
            true,
            ctx,
        )
        .unwrap();
    }

    #[rstest]
    fn from_song_uses_name_metadata() {
        let s = RadioStation::from_song(&song("http://x.example/stream", Some("My Station")));
        assert_eq!(s, station("My Station", "http://x.example/stream"));
    }

    #[rstest]
    fn from_song_falls_back_to_url() {
        let s = RadioStation::from_song(&song("http://x.example/stream", None));
        assert_eq!(s, station("http://x.example/stream", "http://x.example/stream"));
    }

    #[rstest]
    fn m3u_content_writes_extinf_names() {
        let content = m3u_content(&[
            station("SomaFM Groove Salad", "http://ice1.somafm.com/groovesalad-128-mp3"),
            station("Radio Paradise", "https://stream.radioparadise.com/mp3-128"),
        ]);
        assert_eq!(
            content,
            "#EXTM3U\n\
             #EXTINF:-1,SomaFM Groove Salad\n\
             http://ice1.somafm.com/groovesalad-128-mp3\n\
             #EXTINF:-1,Radio Paradise\n\
             https://stream.radioparadise.com/mp3-128\n"
        );
    }

    #[rstest]
    fn m3u_content_strips_newlines() {
        let content = m3u_content(&[station("Line\nBreak", "http://x/stream\nbogus")]);
        assert_eq!(content, "#EXTM3U\n#EXTINF:-1,Line Break\nhttp://x/stream bogus\n");
    }

    #[rstest]
    fn is_stream_url_accepts_http() {
        assert!(is_stream_url("http://ice1.somafm.com/x"));
        assert!(is_stream_url("https://stream.example/x"));
        assert!(!is_stream_url("/mnt/music/song.flac"));
        assert!(!is_stream_url("relative/path.mp3"));
    }

    #[rstest]
    fn regions_list_local_and_countries(mut ctx: Ctx) {
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        pane.rebuild_regions();
        // [◎ Local (2), ▶ Germany (2)] — countries collapsed.
        assert_eq!(pane.regions.len(), 2);
        assert!(matches!(pane.regions[0].kind, RegionKind::Local));
        assert!(matches!(pane.regions[1].kind, RegionKind::Country(ref c) if c == "Germany"));
        assert!(pane.regions[1].expandable);
        assert!(!pane.regions[1].expanded);
    }

    #[rstest]
    fn selecting_local_shows_its_stations(mut ctx: Ctx) {
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        pane.select_region(&RegionKind::Local, &ctx).unwrap();
        assert_eq!(pane.stations.len(), 2);
    }

    #[rstest]
    fn selecting_country_shows_top_stations(mut ctx: Ctx) {
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        pane.select_region(&RegionKind::Country("Germany".to_owned()), &ctx).unwrap();
        assert_eq!(pane.stations.len(), 2);
    }

    /// A background data arrival for the region on screen (top-100 fill,
    /// state load, directory refresh) must not move the station selection.
    #[rstest]
    fn repopulating_preserves_the_station_selection(mut ctx: Ctx) {
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        pane.select_region(&RegionKind::Country("Germany".to_owned()), &ctx).unwrap();
        pane.move_station(1, &ctx).unwrap();
        assert_eq!(pane.station_list.selected(), Some(1));

        // The top-100 fill arrives and replaces the short list: the cursor
        // stays on the same station.
        let full = vec![
            dir_station("G1", "Germany"),
            dir_station("G2", "Germany"),
            dir_station("G3", "Germany"),
        ];
        pane.on_query_finished(
            super::RADIO_COUNTRY_STATIONS,
            MpdQueryResult::Any(Box::new((
                "Germany".to_owned(),
                Ok::<Vec<DirectoryStation>, anyhow::Error>(full),
            ))),
            true,
            &ctx,
        )
        .unwrap();
        assert_eq!(
            pane.station_list.selected(),
            Some(1),
            "selection preserved by station across repopulation"
        );
    }

    /// A region's top-100 station cache is filled on the first look,
    /// never re-requested on plain views, and reloaded only when the region
    /// is expanded (looking at sub-regions).
    #[rstest]
    fn country_station_cache_fills_once_and_reloads_on_expand() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx); // Germany's cached top has 2 stations

        // First look at the region: the top-100 cache fill is requested.
        pane.select_region(&RegionKind::Country("Germany".to_owned()), &ctx).unwrap();
        let requests: Vec<_> = work_rx.try_iter().collect();
        assert!(
            requests.iter().any(|r| matches!(
                r,
                WorkRequest::FetchRadioCountryStations { country, .. } if country == "Germany"
            )),
            "top-100 fill requested on first look"
        );

        // The fill arrives and replaces the short cached list.
        let full: Vec<DirectoryStation> =
            (0..100).map(|i| dir_station(&format!("G{i}"), "Germany")).collect();
        pane.on_query_finished(
            super::RADIO_COUNTRY_STATIONS,
            MpdQueryResult::Any(Box::new((
                "Germany".to_owned(),
                Ok::<Vec<DirectoryStation>, anyhow::Error>(full.clone()),
            ))),
            true,
            &ctx,
        )
        .unwrap();
        assert_eq!(pane.stations.len(), 100, "the top-100 replaced the short list");

        // Looking at the region again uses the cache — no reload request.
        pane.select_region(&RegionKind::Country("Germany".to_owned()), &ctx).unwrap();
        let requests: Vec<_> = work_rx.try_iter().collect();
        assert!(
            !requests.iter().any(|r| matches!(r, WorkRequest::FetchRadioCountryStations { .. })),
            "no reload when just viewing the region"
        );

        // Expanding the region only fetches the states — the country's
        // station list is NOT reloaded (caches refresh only for the
        // specific sub-region being highlighted).
        pane.set_region_expanded(&RegionKind::Country("Germany".to_owned()), true, &ctx)
            .unwrap();
        let requests: Vec<_> = work_rx.try_iter().collect();
        assert!(
            !requests.iter().any(|r| matches!(r, WorkRequest::FetchRadioCountryStations { .. })),
            "expanding does not reload the whole region"
        );
        assert!(
            requests.iter().any(|r| matches!(r, WorkRequest::FetchRadioStates { .. })),
            "expanding fetches the states"
        );

        // Looking at a sub-region (a state) refreshes THAT state's
        // stations every time it is selected.
        let states = vec![StateGroup { name: "Berlin".to_owned(), count: 2, stations: None }];
        pane.on_query_finished(
            super::RADIO_STATES,
            MpdQueryResult::Any(Box::new((
                "Germany".to_owned(),
                Ok::<Vec<StateGroup>, anyhow::Error>(states.clone()),
            ))),
            true,
            &ctx,
        )
        .unwrap();
        pane.select_region(
            &RegionKind::State { country: "Germany".to_owned(), state: "Berlin".to_owned() },
            &ctx,
        )
        .unwrap();
        let requests: Vec<_> = work_rx.try_iter().collect();
        assert!(
            requests.iter().any(|r| matches!(
                r,
                WorkRequest::FetchRadioStateStations { state, .. } if state == "Berlin"
            )),
            "the highlighted sub-region's cache is refreshed"
        );
        // Selecting it again refreshes it again.
        pane.select_region(
            &RegionKind::State { country: "Germany".to_owned(), state: "Berlin".to_owned() },
            &ctx,
        )
        .unwrap();
        let requests: Vec<_> = work_rx.try_iter().collect();
        assert!(
            requests.iter().any(|r| matches!(r, WorkRequest::FetchRadioStateStations { .. })),
            "each sub-region selection refreshes its own cache"
        );
    }

    #[rstest]
    fn expanding_country_loads_states_lazily(mut ctx: Ctx) {
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        pane.set_region_expanded(&RegionKind::Country("Germany".to_owned()), true, &ctx)
            .unwrap();
        // States are not loaded yet (the work request is in flight).
        assert_eq!(pane.regions.len(), 2); // [◎ Local, ▼ Germany]
        // The states result arrives from the work thread.
        pane.on_query_finished(
            super::RADIO_STATES,
            MpdQueryResult::Any(Box::new((
                "Germany".to_owned(),
                Ok::<Vec<StateGroup>, anyhow::Error>(vec![StateGroup {
                    name: "Berlin".to_owned(),
                    count: 2,
                    stations: None,
                }]),
            ))),
            true,
            &ctx,
        )
        .unwrap();
        // [◎ Local, ▼ Germany,   ▶ Berlin (collapsed)]
        assert_eq!(pane.regions.len(), 3);
        assert!(matches!(pane.regions[2].kind, RegionKind::State { ref country, .. } if country == "Germany"));
        assert_eq!(pane.regions[2].depth, 1);
    }

    fn selecting_state_loads_its_stations_lazily(mut ctx: Ctx) {
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        pane.set_region_expanded(&RegionKind::Country("Germany".to_owned()), true, &ctx)
            .unwrap();
        // Simulate the states result, then select the state (stations load).
        pane.on_query_finished(
            super::RADIO_STATES,
            MpdQueryResult::Any(Box::new((
                "Germany".to_owned(),
                Ok::<Vec<StateGroup>, anyhow::Error>(vec![StateGroup {
                    name: "Berlin".to_owned(),
                    count: 2,
                    stations: None,
                }]),
            ))),
            true,
            &ctx,
        )
        .unwrap();
        // Provinces are the deepest category: no arrow, no children.
        let state_row = pane.regions.iter().find(|r| {
            matches!(&r.kind, RegionKind::State { state, .. } if state == "Berlin")
        });
        assert!(state_row.is_some_and(|r| !r.expandable));
        let state_kind = RegionKind::State {
            country: "Germany".to_owned(),
            state: "Berlin".to_owned(),
        };
        pane.select_region(&state_kind, &ctx).unwrap();
        // Stations not loaded yet -> empty right pane (fetch in flight).
        assert!(pane.stations.is_empty());
        // The state stations result arrives.
        pane.on_query_finished(
            super::RADIO_STATE_STATIONS,
            MpdQueryResult::Any(Box::new((
                "Germany".to_owned(),
                "Berlin".to_owned(),
                Ok::<Vec<DirectoryStation>, anyhow::Error>(vec![
                    dir_station("S1", "Germany"),
                    dir_station("S2", "Germany"),
                ]),
            ))),
            true,
            &ctx,
        )
        .unwrap();
        assert_eq!(pane.stations.len(), 2);
    }

    #[rstest]
    fn favourites_region_appears_when_non_empty(mut ctx: Ctx) {
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, vec![station("A", "http://a")]);
        load_directory(&mut pane, &ctx);
        pane.rebuild_regions();
        assert!(matches!(pane.regions[0].kind, RegionKind::Favourites));
        pane.select_region(&RegionKind::Favourites, &ctx).unwrap();
        assert_eq!(pane.stations.len(), 1);
    }

    #[rstest]
    fn favourites_region_hidden_when_empty(mut ctx: Ctx) {
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        pane.rebuild_regions();
        assert!(pane.regions.iter().all(|r| !matches!(r.kind, RegionKind::Favourites)));
    }

    #[rstest]
    fn moving_regions_selects_and_populates(mut ctx: Ctx) {
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        pane.move_region(1, &ctx).unwrap(); // Local -> Germany
        assert!(matches!(pane.selected, Some(RegionKind::Country(ref c)) if c == "Germany"));
        assert_eq!(pane.stations.len(), 2);
        pane.move_region(-1, &ctx).unwrap(); // back to Local
        assert!(matches!(pane.selected, Some(RegionKind::Local)));
    }

    fn act(pane: &mut RadioPane, ctx: &mut Ctx, actions: Vec<crate::shared::keys::Actions>) {
        use std::sync::Arc;
        use crate::shared::keys::ActionEvent;
        let mut event = ActionEvent::from(Arc::new(actions));
        pane.handle_action(&mut event, ctx).unwrap();
    }

    #[rstest]
    fn wasd_and_arrows_share_the_single_cursor(mut ctx: Ctx) {
        use crate::config::keys::{CommonAction, DirectoriesActions};
        use crate::shared::keys::Actions;

        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        // w/s and ↑/↓ move the same region cursor.
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderDown)]);
        assert_eq!(pane.region_list.selected(), Some(1), "s moves to Germany");
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Down)]);
        assert_eq!(pane.region_list.selected(), Some(1), "↓ moves regions too");
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Up)]);
        assert_eq!(pane.region_list.selected(), Some(0), "↑ moves back to Local");
        // Enter on the leaf region enters it: the cursor moves to stations.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Confirm)]);
        assert_eq!(pane.focus, PaneFocus::Stations);
        assert_eq!(pane.stations.len(), 2, "Local shows its stations");
        // w/s and ↑/↓ now move the station list.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Down)]);
        assert_eq!(pane.station_list.selected(), Some(1));
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderUp)]);
        assert_eq!(pane.station_list.selected(), Some(0));
        // `a`/`←` back out to the region tree (Local has no parent).
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderCollapse)]);
        assert_eq!(pane.focus, PaneFocus::Regions);
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderDown)]);
        assert_eq!(pane.region_list.selected(), Some(1), "back on the region cursor");
    }

    #[rstest]
    fn d_expands_a_country_and_a_collapses_it(mut ctx: Ctx) {
        use crate::config::keys::DirectoriesActions;
        use crate::shared::keys::Actions;

        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        // Cursor on Germany (index 1).
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderDown)]);
        assert_eq!(pane.region_list.selected(), Some(1));
        // `d` expands the country; the cursor stays on the tree.
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderExpand)]);
        assert_eq!(pane.focus, PaneFocus::Regions);
        assert!(pane.expanded.contains(&RegionKind::Country("Germany".to_owned()).key()));
        // `a` collapses it again.
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderCollapse)]);
        assert_eq!(pane.focus, PaneFocus::Regions);
        assert!(!pane.expanded.contains(&RegionKind::Country("Germany".to_owned()).key()));
    }

    #[rstest]
    fn back_out_from_a_state_moves_up_and_collapses_the_branch(mut ctx: Ctx) {
        use crate::config::keys::{CommonAction, DirectoriesActions};
        use crate::shared::keys::Actions;

        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        load_directory(&mut pane, &ctx);
        // Expand Germany and load its state (Berlin) like the work thread.
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderDown)]);
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderExpand)]);
        pane.on_query_finished(
            super::RADIO_STATES,
            MpdQueryResult::Any(Box::new((
                "Germany".to_owned(),
                Ok::<Vec<StateGroup>, anyhow::Error>(vec![StateGroup {
                    name: "Berlin".to_owned(),
                    count: 2,
                    stations: None,
                }]),
            ))),
            true,
            &ctx,
        )
        .unwrap();
        // Enter the Berlin leaf: the cursor moves to its (not yet loaded)
        // station list.
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderDown)]);
        assert!(matches!(pane.regions[2].kind, RegionKind::State { ref state, .. } if state == "Berlin"));
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Confirm)]);
        assert_eq!(pane.focus, PaneFocus::Stations);
        // `←` backs out: cursor to the parent country, branch collapsed.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Left)]);
        assert_eq!(pane.focus, PaneFocus::Regions);
        assert_eq!(pane.region_list.selected(), Some(1), "cursor on Germany");
        assert!(
            !pane.expanded.contains(&RegionKind::Country("Germany".to_owned()).key()),
            "backing out collapses the branch left"
        );
        assert!(
            matches!(pane.selected, Some(RegionKind::Country(ref c)) if c == "Germany"),
            "the stations pane follows the parent"
        );
    }

    #[test]
    fn enter_on_a_station_opens_the_context_menu() {
        use std::time::Duration;

        use crate::config::keys::CommonAction;
        use crate::shared::events::AppEvent;
        use crate::shared::keys::Actions;
        use crate::ui::UiAppEvent;

        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let app_rx = app_rx.clone();
        let mut pane_ctx = ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = RadioPane::new(&pane_ctx);
        load_favourites(&mut pane, &pane_ctx, vec![station("A", "http://a")]);
        load_directory(&mut pane, &pane_ctx);
        // The Favourites region is row 0 (leaf): Enter enters it, then
        // Enter on the highlighted station pushes the context-menu modal.
        act(&mut pane, &mut pane_ctx, vec![Actions::Common(CommonAction::Confirm)]);
        assert_eq!(pane.focus, PaneFocus::Stations);
        act(&mut pane, &mut pane_ctx, vec![Actions::Common(CommonAction::Confirm)]);
        // Render/status events may precede the modal push; drain until the
        // context-menu modal arrives.
        loop {
            match app_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(AppEvent::UiEvent(UiAppEvent::Modal(_))) => break,
                Ok(_) => continue,
                Err(err) => panic!("expected a context-menu modal, got recv error: {err}"),
            }
        }
    }

    #[rstest]
    fn focused_pane_selection_uses_the_hover_highlight(mut ctx: Ctx) {
        use ratatui::backend::TestBackend;
        use ratatui::prelude::Rect;

        use crate::config::keys::CommonAction;
        use crate::shared::keys::Actions;

        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, vec![station("A", "http://a")]);
        load_directory(&mut pane, &ctx);
        // Enter the Favourites region: the keyboard cursor moves to the
        // stations list and the right pane fills with the favourite.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Confirm)]);
        assert_eq!(pane.focus, PaneFocus::Stations);
        assert_eq!(pane.stations.len(), 1);
        pane.region_list.select(Some(0));
        pane.station_list.select(Some(0));

        let render_bg = |pane: &mut RadioPane,
                         ctx: &Ctx,
                         left: Rect,
                         right: Rect|
         -> (Option<ratatui::style::Color>, Option<ratatui::style::Color>) {
            let backend = TestBackend::new(80, 40);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 40), ctx).unwrap())
                .unwrap();
            let buf = terminal.backend().buffer();
            (buf[(left.x + 1, left.y)].style().bg, buf[(right.x + 1, right.y)].style().bg)
        };

        let hovered = ctx.config.theme.hovered_item_style.bg;
        let current = ctx.config.theme.current_item_style.bg;

        // Render once so the pane areas are known, then snapshot them (the
        // render borrows the pane, the areas must be copied out first).
        let _ = render_bg(&mut pane, &ctx, Rect::new(0, 0, 80, 40), Rect::new(0, 0, 80, 40));
        let (regions_area, stations_area) = (pane.regions_area, pane.stations_area);

        // Keyboard cursor on the stations: the stations row uses the hover
        // highlight, the regions row keeps the plain selection.
        let (regions, stations) = render_bg(&mut pane, &ctx, regions_area, stations_area);
        assert_eq!(stations, hovered, "the focused pane's cursor uses the hover highlight");
        assert_eq!(regions, current, "the other pane keeps the plain selection");

        // Cursor back on the regions: the roles swap.
        pane.focus = PaneFocus::Regions;
        let (regions, stations) = render_bg(&mut pane, &ctx, regions_area, stations_area);
        assert_eq!(regions, hovered, "the focused pane's cursor uses the hover highlight");
        assert_eq!(stations, current, "the other pane keeps the plain selection");
    }

    #[rstest]
    fn stale_states_reference_does_not_panic(mut ctx: Ctx) {
        // The states result must not crash when the country is gone.
        let mut pane = RadioPane::new(&ctx);
        load_favourites(&mut pane, &ctx, Vec::new());
        pane.on_query_finished(
            super::RADIO_STATES,
            MpdQueryResult::Any(Box::new((
                "Nope".to_owned(),
                Ok::<Vec<StateGroup>, anyhow::Error>(vec![StateGroup {
                    name: "X".to_owned(),
                    count: 1,
                    stations: None,
                }]),
            ))),
            true,
            &ctx,
        )
        .unwrap();
        assert!(pane.regions.is_empty());
    }
}

#[cfg(test)]
mod paste_play_tests {
    use super::RadioPane;
    use crate::{
        MpdQueryResult,
        tests::fixtures::ctx,
        ui::panes::Pane,
    };

    /// "Play (don't add to queue)" routes its `addid` result to the Radio
    /// pane, which records the id so the Queue pane hides the entry (and
    /// the pane drops it again on song change / stop).
    #[test]
    fn paste_play_result_registers_the_hidden_entry() {
        let mut pane_ctx = ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = RadioPane::new(&pane_ctx);
        pane.on_query_finished(
            crate::ui::modals::paste::PASTE_PLAY,
            MpdQueryResult::Any(Box::new(7u32)),
            true,
            &pane_ctx,
        )
        .unwrap();
        assert_eq!(pane.temp_play_id, Some(7));
        assert_eq!(pane_ctx.temp_play_id.get(), Some(7), "the queue pane reads it via Ctx");

        // A song change (or stop) drops the temporary entry.
        pane_ctx.status.songid = Some(8);
        pane_ctx.status.state = crate::mpd::commands::State::Play;
        let mut event = crate::ui::UiEvent::SongChanged;
        pane.on_event(&mut event, true, &pane_ctx).unwrap();
        assert_eq!(pane.temp_play_id, None);
        assert_eq!(pane_ctx.temp_play_id.get(), None);
    }
}
