use std::{collections::HashSet, path::PathBuf};
use anyhow::Result;
use ratatui::{
    Frame, layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};
use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{CommonAction, DirectoriesActions},
        tabs::{PaneType, PaneTypeDiscriminants, TreeBrowserArgs},
        utils::tilde_expand,
    },
    ctx::Ctx, mpd::{commands::State, mpd_client::MpdClient},
    radio::{CountryGroup, DirectoryStation, RadioDirectory},
    shared::{
        events::WorkRequest, keys::ActionEvent,
        macros::{modal, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{
        UiEvent,
        modals::{
            confirm_modal::{Action, ConfirmModal},
            input_modal::InputModal, menu::modal::MenuModal,
        },
        tree_browser::{TreeBrowserCore, TreeRowView},
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
/// One entry of the favourite stations list: a local m3u under the s2udio
/// config dir (`~/.config/s2udio/radio/<playlist>.m3u`) whose entries are
/// stream URLs. Names come from the `#EXTINF` lines of the file. The
/// favourites are deliberately NOT an MPD stored playlist — keeping the
/// file in the MPD playlist dir made rmpc list a playlist it doesn't
/// understand (setup.sh migrates an existing file during install/update).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioStation {
    pub name: String,
    pub url: String,
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
/// Path of the radio favourites file: `~/.config/s2udio/radio/<playlist>.m3u`.
/// The favourites are NOT an MPD stored playlist anymore — keeping the file
/// in the MPD playlist dir made rmpc list a playlist it doesn't understand.
/// setup.sh migrates an existing MPD-side file during install/update.
fn playlist_file_path(playlist: &str) -> PathBuf {
    let dir = crate::shared::paths::config_dir()
        .unwrap_or_else(|| PathBuf::from(tilde_expand("~/.config/s2udio").into_owned()));
    dir.join("radio").join(format!("{playlist}.m3u"))
}
/// Fetch the favourites by reading the m3u under the s2udio config dir. A
/// missing file is not an error (no favourites yet).
fn fetch_stations(playlist: &str) -> Result<Vec<RadioStation>> {
    let content = match std::fs::read_to_string(playlist_file_path(playlist)) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    Ok(parse_m3u(&content))
}
/// Parse an EXTINF m3u (the format the app writes; MPD's own stored
/// playlists are plain URL-per-line and parse the same). `#EXTINF:-1,name`
/// lines name the following URL; bare URLs keep the URL as their name.
fn parse_m3u(content: &str) -> Vec<RadioStation> {
    let mut stations = Vec::new();
    let mut pending_name: Option<String> = None;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("#EXTM3U") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending_name = Some(
                rest.splitn(2, ',').nth(1).unwrap_or_default().trim().to_string(),
            );
        } else if is_stream_url(line) {
            let name = pending_name
                .take()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| line.to_string());
            stations
                .push(RadioStation {
                    name,
                    url: line.to_string(),
                });
        } else {
            pending_name = None;
        }
    }
    stations
}
/// EXTINF m3u serialization: every station keeps its name, which MPD's own
/// playlist commands would drop on rewrite.
fn m3u_content(stations: &[RadioStation]) -> String {
    let mut content = String::from("#EXTM3U\n");
    for station in stations {
        let name = station.name.replace(['\n', '\r'], " ");
        let url = station.url.replace(['\n', '\r'], " ");
        content.push_str(&format!("#EXTINF:-1,{name}\n{url}\n"));
    }
    content
}
/// Rewrite the whole `.m3u` in EXTINF format so every station keeps its name.
fn write_stations_file(playlist: &str, stations: &[RadioStation]) -> Result<()> {
    let path = playlist_file_path(playlist);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, m3u_content(stations))?;
    Ok(())
}
/// "12 km", "3.4 km" etc.
fn format_distance(km: Option<f64>) -> Option<String> {
    let km = km?;
    if km < 10.0 { Some(format!("{km:.1} km")) } else { Some(format!("{km:.0} km")) }
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
    /// Tree-browser layout args from the config. The regions tree keeps
    /// its always-visible 30% share regardless (radio's shape), so the
    /// args are plumbing for uniformity; the width/hide args do not apply.
    tree_args: TreeBrowserArgs,
    initialized: bool,
    regions_area: Rect,
    stations_area: Rect,
    info_area: Rect,
}
impl RadioPane {
    pub fn new(ctx: &Ctx) -> Self {
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
            tree_args: ctx.config.tree_browser_args(PaneTypeDiscriminants::Radio),
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
        ctx.query()
            .id(id)
            .replace_id(id)
            .target(PaneType::Radio {
                tree: TreeBrowserArgs::default(),
            })
            .query(move |_client| Ok(
                MpdQueryResult::Any(Box::new(fetch_stations(&playlist)?)),
            ));
    }
    /// Ask the work thread for the station directory (local + countries).
    fn query_directory(&mut self, ctx: &Ctx) {
        self.directory_loading = true;
        let location = ctx.config.radio.location.clone();
        let cache_dir = ctx.config.cache_dir.clone();
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchRadioDirectory {
                location,
                cache_dir,
            })
            .map_err(|err| {
                log::error!(error:? = err; "Failed to request radio directory")
            });
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
            regions
                .push(RegionRow {
                    kind: RegionKind::Favourites,
                    label: "★ Favourites".to_owned(),
                    depth: 0,
                    expandable: false,
                    expanded: false,
                });
        }
        if let Some(directory) = &self.directory {
            if !directory.local.is_empty() {
                regions
                    .push(RegionRow {
                        kind: RegionKind::Local,
                        label: "◎ Local".to_owned(),
                        depth: 0,
                        expandable: false,
                        expanded: false,
                    });
            }
            for country in &directory.countries {
                let expanded = self
                    .expanded
                    .contains(&RegionKind::Country(country.name.clone()).key());
                regions
                    .push(RegionRow {
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
                    regions
                        .push(RegionRow {
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
        if let Some(key) = prev_key {
            if let Some(idx) = self.regions.iter().position(|row| row.kind.key() == key)
            {
                self.region_list.select(Some(idx));
                return;
            }
        }
        self.region_list.select(if self.regions.is_empty() { None } else { Some(0) });
        *self.region_list.offset_mut() = 0;
    }
    /// Populate the right station list for a region, loading data lazily.
    fn select_region(&mut self, kind: &RegionKind, ctx: &Ctx) -> Result<()> {
        self.selected = Some(kind.clone());
        self.ensure_region_data(kind, ctx);
        self.populate_stations(kind);
        crate::ui::widgets::virtualized_list::scroll_selection_into_view(
            &mut self.station_list,
            self.stations.len(),
            (self.stations_area.height as usize) / 2,
            ctx.config.scrolloff,
        );
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
                let Some(group) = directory
                    .countries
                    .iter_mut()
                    .find(|g| &g.name == country) else {
                    return;
                };
                let country = country.clone();
                let country_code = group.code.clone();
                if group.top.len() < 100 && let Some(code) = country_code.clone() {
                    let _ = ctx
                        .work_sender
                        .send(WorkRequest::FetchRadioCountryStations {
                            country: country.clone(),
                            country_code: code,
                        })
                        .map_err(|err| {
                            log::error!(
                                error:? = err; "Failed to request radio country stations"
                            )
                        });
                }
                if group.states.is_none() {
                    let _ = ctx
                        .work_sender
                        .send(WorkRequest::FetchRadioStates {
                            country,
                            country_code,
                        })
                        .map_err(|err| {
                            log::error!(error:? = err; "Failed to request radio states")
                        });
                }
            }
            RegionKind::State { country, state } => {
                let Some(group) = directory
                    .countries
                    .iter_mut()
                    .find(|g| &g.name == country) else {
                    return;
                };
                let state_exists = group
                    .states
                    .as_ref()
                    .is_some_and(|states| states.iter().any(|s| &s.name == state));
                if !state_exists {
                    return;
                }
                let country = country.clone();
                let state = state.clone();
                let _ = ctx
                    .work_sender
                    .send(WorkRequest::FetchRadioStateStations {
                        country,
                        state,
                    })
                    .map_err(|err| {
                        log::error!(
                            error:? = err; "Failed to request radio state stations"
                        )
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
        let previous_url = self
            .station_list
            .selected()
            .and_then(|idx| self.stations.get(idx))
            .map(|row| row.url().to_owned());
        let previous_idx = self.station_list.selected().unwrap_or(0);
        self.station_list.select(None);
        match kind {
            RegionKind::Favourites => {
                self.stations = self
                    .favourites
                    .iter()
                    .cloned()
                    .map(StationRow::Favourite)
                    .collect();
            }
            RegionKind::Local => {
                self.stations = self
                    .directory
                    .as_ref()
                    .map(|d| {
                        d
                            .local
                            .iter()
                            .cloned()
                            .map(StationRow::Directory)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
            }
            RegionKind::Country(country) => {
                self.stations = self
                    .find_country(country)
                    .map(|group| {
                        group
                            .top
                            .iter()
                            .cloned()
                            .map(StationRow::Directory)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
            }
            RegionKind::State { country, state } => {
                let Some(group) = self.find_country(country) else {
                    self.stations = Vec::new();
                    return;
                };
                let Some(state_group) = group
                    .states
                    .as_ref()
                    .and_then(|s| s.iter().find(|s| &s.name == state)) else {
                    self.stations = Vec::new();
                    return;
                };
                self.stations = state_group
                    .stations
                    .as_ref()
                    .map(|stations| {
                        stations
                            .iter()
                            .cloned()
                            .map(StationRow::Directory)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
            }
        }
        if !self.stations.is_empty() {
            let idx = previous_url
                .as_deref()
                .and_then(|url| self.stations.iter().position(|row| row.url() == url))
                .unwrap_or_else(|| {
                    previous_idx.min(self.stations.len().saturating_sub(1))
                });
            self.station_list.select(Some(idx));
        }
        *self.station_list.offset_mut() = 0;
    }
    /// Expand (or collapse) a tree region. Expanding a country fetches its
    /// states (once) — it does **not** reload the country's station list:
    /// caches only refresh for the specific sub-region being highlighted
    /// (a state selection re-fetches that state's stations).
    fn set_region_expanded(
        &mut self,
        kind: &RegionKind,
        expanded: bool,
        ctx: &Ctx,
    ) -> Result<()> {
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
    /// Move the station selection (right pane).
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
        ctx.query()
            .id(PLAY)
            .replace_id(PLAY)
            .target(PaneType::Radio {
                tree: TreeBrowserArgs::default(),
            })
            .query(move |client| {
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
        self.focus = PaneFocus::Stations;
        self.select_region(&row.kind, ctx)
    }
    /// Move the tree selection (the shared core); keeps the right pane in
    /// sync (each highlighted region shows its stations).
    fn move_region(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        self.move_tree(dir, ctx)
    }
    /// Move the station selection (the shared core).
    fn move_station(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        self.move_items(dir, ctx)
    }
    /// The highlighted station (a favourite or a directory station).
    fn selected_station(&self) -> Option<RadioStation> {
        self.selected_item()
            .map(|row| match row {
                StationRow::Favourite(station) => {
                    RadioStation {
                        name: station.name.clone(),
                        url: station.url.clone(),
                    }
                }
                StationRow::Directory(station) => {
                    RadioStation {
                        name: station.name.clone(),
                        url: station.url.clone(),
                    }
                }
            })
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
        if let Some(parent) = self
            .regions[..idx]
            .iter()
            .rposition(|r| r.depth < row.depth)
        {
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
    /// Read the current favourites, apply `mutate`, write the file back and
    /// refresh the list.
    pub(crate) fn mutate_stations(
        ctx: &Ctx,
        playlist: String,
        mutate: impl FnOnce(&mut Vec<RadioStation>) + Send + 'static,
    ) -> Result<()> {
        let playlist_name = playlist.clone();
        let result = ctx
            .query_sync(move |_client| {
                let mut stations = fetch_stations(&playlist_name)?;
                mutate(&mut stations);
                write_stations_file(&playlist_name, &stations)?;
                Ok(stations)
            })?;
        status_info!("Favourites updated ({} stations)", result.len());
        ctx.query()
            .id(REINIT)
            .replace_id(REINIT)
            .target(PaneType::Radio {
                tree: TreeBrowserArgs::default(),
            })
            .query(move |_client| Ok(
                MpdQueryResult::Any(Box::new(fetch_stations(&playlist)?)),
            ));
        ctx.render()?;
        Ok(())
    }
    fn is_favourite(&self, url: &str) -> bool {
        self.favourites.iter().any(|station| station.url == url)
    }
    fn open_menu(
        &mut self,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        let Some(station) = self.selected_station() else { return Ok(()) };
        let name = station.name.clone();
        let url = station.url.clone();
        let playlist = ctx.config.radio.playlist.clone();
        let is_favourite = self.is_favourite(&url);
        let favourites = self.favourites.clone();
        let max = self.max_favourites(ctx);
        let menu = MenuModal::new(ctx)
            .anchor(anchor)
            .list_section(
                ctx,
                |mut section| {
                    section
                        .add_item(
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
                        section
                            .add_item(
                                "Remove from favourites",
                                {
                                    let playlist = playlist.clone();
                                    let url = url.clone();
                                    move |ctx| {
                                        Self::mutate_stations(
                                            ctx,
                                            playlist,
                                            move |stations| {
                                                stations.retain(|station| station.url != url);
                                            },
                                        )?;
                                        Ok(())
                                    }
                                },
                            );
                        section
                            .add_item(
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
                        section
                            .add_item(
                                "★ Add to favourites",
                                Self::menu_add_favourite(
                                    playlist.clone(),
                                    station.clone(),
                                    favourites,
                                    max,
                                ),
                            );
                    }
                    section
                        .add_item(
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
                    section
                        .add_item(
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
                },
            )
            .list_section(
                ctx,
                |mut section| {
                    section
                        .add_item(
                            "Add station by URL…",
                            Self::menu_add_station(playlist.clone()),
                        );
                    section
                        .add_item(
                            "Refresh stations from directory",
                            move |ctx| {
                                let location = ctx.config.radio.location.clone();
                                let cache_dir = ctx.config.cache_dir.clone();
                                let _ = ctx
                                    .work_sender
                                    .send(WorkRequest::FetchRadioDirectory {
                                        location,
                                        cache_dir,
                                    })
                                    .map_err(|err| {
                                        log::error!(
                                            error:? = err; "Failed to request radio directory"
                                        )
                                    });
                                status_info!("Refreshing station directory…");
                                Ok(())
                            },
                        );
                    Some(section)
                },
            );
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
            Self::mutate_stations(
                ctx,
                playlist,
                move |stations| stations.push(station),
            )?;
            Ok(())
        }
    }
    /// Two chained input modals: name first, then URL.
    fn menu_add_station(playlist: String) -> impl FnOnce(&Ctx) -> Result<()> {
        move |ctx| {
            let playlist = playlist.clone();
            modal!(
                ctx, InputModal::new(ctx).title("Add radio station")
                .confirm_label("Next").input_label("Station name:").on_confirm(move |
                ctx, name | { let name = name.to_owned(); let playlist = playlist
                .clone(); modal!(ctx, InputModal::new(ctx).title("Add radio station")
                .confirm_label("Add").input_label("Stream URL:").on_confirm(move | ctx,
                url | { let url = url.trim().to_owned(); if ! is_stream_url(& url) {
                status_warn!("Not a stream URL (expected http(s)://...): {url}"); return
                Ok(()); } Self::mutate_stations(ctx, playlist, move | stations | {
                stations.push(RadioStation { name : name.clone(), url : url.clone(), });
                }) ?; Ok(()) })); Ok(()) })
            );
            Ok(())
        }
    }
    fn prompt_rename(ctx: &Ctx, playlist: String, name: String, url: String) {
        modal!(
            ctx, InputModal::new(ctx).title("Rename radio station")
            .confirm_label("Rename").input_label("New name:").initial_value(name)
            .on_confirm(move | ctx, new_name | { let new_name = new_name.to_owned();
            Self::mutate_stations(ctx, playlist, move | stations | { if let Some(station)
            = stations.iter_mut().find(| s | s.url == url) { station.name = new_name; }
            }) ?; Ok(()) })
        );
    }
    fn confirm_delete(ctx: &Ctx, playlist: String, name: String, url: String) {
        modal!(
            ctx, ConfirmModal::builder().ctx(ctx)
            .message(vec![format!("Remove station '{name}'?"),
            "It will be deleted from the radio playlist.".to_owned(),])
            .action(Action::Single { confirm_label : Some("Delete"), cancel_label : None,
            on_confirm : Box::new(move | ctx | { Self::mutate_stations(ctx, playlist,
            move | stations | { stations.retain(| s | s.url != url); }) ?; Ok(()) }), })
            .size((45, 6)).build()
        );
    }
}
impl TreeBrowserCore for RadioPane {
    type Item = StationRow;
    fn tree_rows(&self) -> Vec<TreeRowView> {
        self.regions
            .iter()
            .map(|row| TreeRowView {
                label: row.label.clone(),
                depth: row.depth,
                expandable: row.expandable,
                expanded: row.expanded,
                root: false,
            })
            .collect()
    }
    fn tree_selected(&self) -> usize {
        self.region_list.selected().unwrap_or(0)
    }
    fn tree_list(&self) -> &ListState {
        &self.region_list
    }
    fn tree_list_mut(&mut self) -> &mut ListState {
        &mut self.region_list
    }
    fn tree_area(&self) -> Rect {
        self.regions_area
    }
    fn set_tree_area(&mut self, area: Rect) {
        self.regions_area = area;
    }
    fn set_expanded_idx(&mut self, idx: usize, expanded: bool, ctx: &Ctx) -> Result<()> {
        let Some(row) = self.regions.get(idx).cloned() else { return Ok(()) };
        self.set_region_expanded(&row.kind, expanded, ctx)
    }
    fn items_len(&self) -> usize {
        self.stations.len()
    }
    fn items_list(&self) -> &ListState {
        &self.station_list
    }
    fn items_list_mut(&mut self) -> &mut ListState {
        &mut self.station_list
    }
    fn items_area(&self) -> Rect {
        self.stations_area
    }
    fn set_items_area(&mut self, area: Rect) {
        self.stations_area = area;
    }
    fn item_at(&self, idx: usize) -> Option<Self::Item> {
        self.stations.get(idx).cloned()
    }
    fn item_row_height(&self) -> u16 {
        2
    }
    fn item_row(&self, idx: usize, hovered: bool, ctx: &Ctx) -> ListItem<'static> {
        let playing_url = Self::playing_url(ctx);
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        let style = if hovered {
            ctx.config.theme.hovered_item_style
        } else {
            Style::new()
        };
        match &self.stations[idx] {
            StationRow::Favourite(station) => {
                let is_playing = playing_url.as_deref() == Some(station.url.as_str());
                let prefix = if is_playing { "▶ " } else { "  " };
                let name_style = if is_playing {
                    base.add_modifier(Modifier::BOLD)
                } else {
                    base
                };
                ListItem::new(
                        vec![
                            Line::from(Span::styled(format!("{prefix}{}", station.name),
                            name_style)), Line::from(Span::styled(format!("  {}", station
                            .url), dim)),
                        ],
                    )
                    .style(style)
            }
            StationRow::Directory(station) => {
                let is_playing = playing_url.as_deref() == Some(station.url.as_str());
                let prefix = if is_playing { "▶ " } else { "  " };
                let name_style = if is_playing {
                    base.add_modifier(Modifier::BOLD)
                } else {
                    base
                };
                ListItem::new(
                        vec![
                            Line::from(Span::styled(format!("{prefix}{}", station.name),
                            name_style)), Line::from(Span::styled(format!("  {}",
                            station_subline(station)), dim)),
                        ],
                    )
                    .style(style)
            }
        }
    }
    /// Highlight a tree row and show its region's stations (the region
    /// tree and the right pane share the selection).
    fn highlight_tree_node(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        let Some(row) = self.regions.get(idx).cloned() else { return Ok(()) };
        self.region_list.select(Some(idx));
        self.select_region(&row.kind, ctx)
    }
    fn select_parent(&mut self, ctx: &Ctx) -> Result<()> {
        self.back_out(ctx)
    }
    fn activate_selected(&mut self, ctx: &Ctx) -> Result<()> {
        if self.focus == PaneFocus::Stations {
            self.play_selected(ctx)
        } else {
            self.open_region(ctx)
        }
    }
    fn open_context_menu(
        &mut self,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        self.open_menu(ctx, anchor)
    }
    /// Keybinding hints, one line each, in the strip between the station list
    /// and the info panel (same spot as the Directories/Playlists tabs).
    fn render_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let key = ctx.config.theme.preview_label_style;
        let group = ctx.config.theme.preview_metadata_group_style;
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        let mut items: Vec<ListItem> = Vec::new();
        match self.selected_station() {
            Some(
                RadioStation { name, url },
            ) if self.favourites.iter().any(|f| f.url == url) => {
                items.push(ListItem::new(Line::styled(" --- [Favourite]", group)));
                items
                    .push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Name", key), Span::raw(": "), Span::raw(name),
                                ],
                            ),
                        ),
                    );
                items
                    .push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("URL", key), Span::raw(": "), Span::styled(url,
                                    dim),
                                ],
                            ),
                        ),
                    );
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
                        items
                            .push(
                                ListItem::new(
                                    Line::from(
                                        vec![
                                            Span::styled("Name", key), Span::raw(": "),
                                            Span::raw(station.name.clone()),
                                        ],
                                    ),
                                ),
                            );
                        if let Some(dist) = format_distance(station.distance_km) {
                            items
                                .push(
                                    ListItem::new(
                                        Line::from(
                                            vec![
                                                Span::styled("Distance", key), Span::raw(": "),
                                                Span::raw(dist),
                                            ],
                                        ),
                                    ),
                                );
                        }
                        items
                            .push(
                                ListItem::new(
                                    Line::from(
                                        vec![
                                            Span::styled("Country", key), Span::raw(format!(": {}{}",
                                            station.country, station.country_code.chars().all(| c | c
                                            .is_ascii_uppercase()).then(|| format!(" ({})", station
                                            .country_code)).unwrap_or_default())),
                                        ],
                                    ),
                                ),
                            );
                        if let Some(state) = &station.state {
                            items
                                .push(
                                    ListItem::new(
                                        Line::from(
                                            vec![
                                                Span::styled("State", key), Span::raw(": "), Span::raw(state
                                                .clone()),
                                            ],
                                        ),
                                    ),
                                );
                        }
                        if let Some(city) = &station.city {
                            items
                                .push(
                                    ListItem::new(
                                        Line::from(
                                            vec![
                                                Span::styled("City", key), Span::raw(": "), Span::raw(city
                                                .clone()),
                                            ],
                                        ),
                                    ),
                                );
                        }
                        if !station.language.is_empty() {
                            items
                                .push(
                                    ListItem::new(
                                        Line::from(
                                            vec![
                                                Span::styled("Language", key), Span::raw(": "),
                                                Span::raw(station.language.join(", ")),
                                            ],
                                        ),
                                    ),
                                );
                        }
                        if !station.tags.is_empty() {
                            items
                                .push(
                                    ListItem::new(
                                        Line::from(
                                            vec![
                                                Span::styled("Tags", key), Span::raw(": "),
                                                Span::raw(station.tags.join(", ")),
                                            ],
                                        ),
                                    ),
                                );
                        }
                        if let (Some(codec), Some(bitrate)) = (
                            &station.codec,
                            station.bitrate,
                        ) {
                            items
                                .push(
                                    ListItem::new(
                                        Line::from(
                                            vec![
                                                Span::styled("Stream", key),
                                                Span::raw(format!(": {codec} {bitrate} kbps")),
                                            ],
                                        ),
                                    ),
                                );
                        } else if let Some(bitrate) = station.bitrate {
                            items
                                .push(
                                    ListItem::new(
                                        Line::from(
                                            vec![
                                                Span::styled("Stream", key),
                                                Span::raw(format!(": {bitrate} kbps")),
                                            ],
                                        ),
                                    ),
                                );
                        }
                        if station.votes > 0 {
                            items
                                .push(
                                    ListItem::new(
                                        Line::from(
                                            vec![
                                                Span::styled("Votes", key), Span::raw(format!(": {}",
                                                station.votes)),
                                            ],
                                        ),
                                    ),
                                );
                        }
                        items
                            .push(
                                ListItem::new(
                                    Line::from(
                                        vec![
                                            Span::styled("URL", key), Span::raw(": "),
                                            Span::styled(station.url.clone(), dim),
                                        ],
                                    ),
                                ),
                            );
                    }
                    None => {
                        items.push(ListItem::new(Line::styled(" --- [Station]", group)));
                        items
                            .push(
                                ListItem::new(
                                    Line::from(
                                        vec![
                                            Span::styled("Name", key), Span::raw(": "), Span::raw(name),
                                        ],
                                    ),
                                ),
                            );
                        items
                            .push(
                                ListItem::new(
                                    Line::from(
                                        vec![
                                            Span::styled("URL", key), Span::raw(": "), Span::styled(url,
                                            dim),
                                        ],
                                    ),
                                ),
                            );
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
                items
                    .push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Region", key), Span::raw(": "),
                                    Span::raw(label),
                                ],
                            ),
                        ),
                    );
                if self.stations.is_empty() {
                    items
                        .push(
                            ListItem::new(
                                Line::from(
                                    vec![
                                        Span::styled("Hint", key), Span::raw(": "),
                                        Span::styled("Pick a region on the left.", dim),
                                    ],
                                ),
                            ),
                        );
                }
            }
        }
        if let Some((_, song)) = ctx.find_current_song_in_queue()
            && is_stream_url(&song.file) && ctx.status.state == State::Play
        {
            items.push(ListItem::new(""));
            items.push(ListItem::new(Line::styled(" --- [Now playing]", group)));
            if let Some(title) = song.metadata.get("title") {
                items
                    .push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Title", key), Span::raw(": "), Span::raw(title
                                    .join(", ").into_owned()),
                                ],
                            ),
                        ),
                    );
            }
            if let Some(artist) = song.metadata.get("artist") {
                items
                    .push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Artist", key), Span::raw(": "),
                                    Span::raw(artist.join(", ").into_owned()),
                                ],
                            ),
                        ),
                    );
            }
            if let Some(name) = song.metadata.get("name") {
                items
                    .push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Stream", key), Span::raw(": "), Span::raw(name
                                    .join(", ").into_owned()),
                                ],
                            ),
                        ),
                    );
            }
            if let Some(bitrate) = ctx.status.bitrate {
                items
                    .push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Bitrate", key),
                                    Span::raw(format!(": {bitrate} kbps")),
                                ],
                            ),
                        ),
                    );
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
    fn temp_play_id(&self) -> Option<u32> {
        self.temp_play_id
    }
    fn set_temp_play_id(&mut self, id: Option<u32>) {
        self.temp_play_id = id;
    }
    fn tree_title(&self) -> &'static str {
        " Regions "
    }
    fn items_title(&self) -> String {
        match &self.selected {
            Some(RegionKind::Favourites) => " Favourites ".to_owned(),
            Some(RegionKind::Local) => " Local — closest ".to_owned(),
            Some(RegionKind::Country(name)) => format!(" {name} "),
            Some(RegionKind::State { state, .. }) => format!(" {state} "),
            None => " Stations ".to_owned(),
        }
    }
    fn tips_lines(&self, ctx: &Ctx) -> Vec<Line<'static>> {
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        vec![
            Line::from(vec![Span::styled("w/s · ↑/↓", base),
            Span::styled("  move list", dim)]), Line::from(vec![Span::styled("d / →",
            base), Span::styled("  open region · play station", dim),]),
            Line::from(vec![Span::styled("a / ←", base), Span::styled("  back out",
            dim), Span::styled("Enter", base), Span::styled("  context menu", dim),]),
        ]
    }
    /// The radio tips strip is inset by one column.
    fn tips_area(&self, area: Rect) -> Rect {
        area.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 0,
        })
    }
    /// The configured tree-browser args (plumbing for uniformity; the
    /// regions tree keeps its 30% share below).
    fn tree_args(&self) -> TreeBrowserArgs {
        self.tree_args.clone()
    }
    /// The radio region tree always takes its 30% share (no narrow-TUI
    /// collapse).
    fn split_tree(&self, area: Rect) -> (Rect, Rect) {
        let [regions_area, right] = Layout::horizontal([
                Constraint::Percentage(30),
                Constraint::Percentage(70),
            ])
            .areas(area);
        (regions_area, right)
    }
    /// The station cursor never moves the region-tree highlight.
    fn on_items_cursor_moved(&mut self, ctx: &Ctx) -> Result<()> {
        ctx.render()?;
        Ok(())
    }
    fn on_tree_focus(&mut self) {
        self.focus = PaneFocus::Regions;
    }
    fn on_items_focus(&mut self) {
        self.focus = PaneFocus::Stations;
    }
    /// The row under the mouse, or the keyboard cursor when this pane is
    /// the one being navigated.
    fn tree_highlight(&self, hover_idx: Option<usize>, ctx: &Ctx) -> Style {
        if hover_idx == self.tree_list().selected() || self.focus == PaneFocus::Regions {
            ctx.config.theme.hovered_item_style
        } else {
            ctx.config.theme.current_item_style
        }
    }
    fn items_highlight(&self, hover_idx: Option<usize>, ctx: &Ctx) -> Style {
        if hover_idx == self.items_list().selected() || self.focus == PaneFocus::Stations
        {
            ctx.config.theme.hovered_item_style
        } else {
            ctx.config.theme.current_item_style
        }
    }
    fn on_reconnected(&mut self, ctx: &Ctx) -> Result<()> {
        self.initialized = false;
        self.temp_play_id = None;
        self.before_show(ctx)?;
        Ok(())
    }
}
impl Pane for RadioPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        self.render_tree_browser(frame, area, ctx)
    }
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        let id = if self.initialized { REINIT } else { INIT };
        self.query_favourites(ctx, id);
        if self.directory.is_none() {
            if let Some(cached) = Self::load_cached_directory(ctx) {
                self.directory = Some(cached);
                self.rebuild_regions();
            }
            self.query_directory(ctx);
        }
        self.initialized = true;
        Ok(())
    }
    fn on_event(
        &mut self,
        event: &mut UiEvent,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        if self.handle_tree_events(event, is_visible, ctx)? {
            return Ok(());
        }
        match event {
            UiEvent::StoredPlaylist => self.query_favourites(ctx, REINIT),
            _ => {}
        }
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if self.regions_area.contains(event.into()) {
            self.focus = PaneFocus::Regions;
            match event.kind {
                MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                    let row = usize::from(event.y.saturating_sub(self.regions_area.y));
                    let idx = self.region_list.offset() + row;
                    if let Some(region) = self.regions.get(idx).cloned() {
                        self.region_list.select(Some(idx));
                        let is_double = matches!(
                            event.kind, MouseEventKind::DoubleClick
                        );
                        if is_double && region.expandable {
                            self.set_region_expanded(
                                &region.kind,
                                !region.expanded,
                                ctx,
                            )?;
                        } else {
                            self.select_region(&region.kind, ctx)?;
                        }
                    }
                }
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                        -1
                    } else {
                        1
                    };
                    crate::ui::widgets::virtualized_list::scroll_viewport(
                        &mut self.region_list,
                        dir,
                        ctx.config.scroll_amount.max(1),
                        self.regions.len(),
                        self.regions_area.height as usize,
                    );
                    ctx.render()?;
                }
                _ => {}
            }
            return Ok(());
        }
        if self.stations_area.contains(event.into()) {
            self.focus = PaneFocus::Stations;
            match event.kind {
                MouseEventKind::RightClick => {
                    let row = usize::from(event.y.saturating_sub(self.stations_area.y));
                    let idx = self.station_list.offset() + row / 2;
                    if idx < self.stations.len() {
                        self.station_list.select(Some(idx));
                        ctx.render()?;
                        return self.open_menu(ctx, Some(event.into()));
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
                    let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                        -1
                    } else {
                        1
                    };
                    crate::ui::widgets::virtualized_list::scroll_viewport(
                        &mut self.station_list,
                        dir,
                        ctx.config.scroll_amount.max(1),
                        self.stations.len(),
                        (self.stations_area.height as usize) / 2,
                    );
                    ctx.render()?;
                }
                _ => {}
            }
            return Ok(());
        }
        Ok(())
    }
    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_directories() {
            return match action {
                DirectoriesActions::FolderUp | DirectoriesActions::FolderDown => {
                    let dir = if matches!(action, DirectoriesActions::FolderUp) {
                        -1
                    } else {
                        1
                    };
                    if self.focus == PaneFocus::Stations {
                        self.move_station(dir, ctx)
                    } else {
                        self.move_region(dir, ctx)
                    }
                }
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    if self.focus == PaneFocus::Stations {
                        self.play_selected(ctx)
                    } else {
                        self.open_region(ctx)
                    }
                }
                DirectoriesActions::FolderCollapse => self.back_out(ctx),
            };
        }
        if let Some(action) = event.claim_common() {
            match action {
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
                    return if self.focus == PaneFocus::Stations {
                        self.open_menu(ctx, None)
                    } else {
                        self.open_region(ctx)
                    };
                }
                CommonAction::ContextMenu => return self.open_menu(ctx, None),
                CommonAction::Delete => {
                    if let Some(idx) = self.station_list.selected()
                        && let Some(StationRow::Favourite(station)) = self
                            .stations
                            .get(idx)
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
                    if let Some(err) = &fresh.error && self.directory.is_some() {
                        status_warn!("Radio directory refresh failed: {err}");
                    } else {
                        self.directory = Some(fresh);
                    }
                    self.rebuild_regions();
                    if self.selected.is_none()
                        && let Some(first) = self.regions.first().map(|r| r.kind.clone())
                    {
                        self.select_region(&first, ctx)?;
                    } else if let Some(kind) = self.selected.clone() {
                        self.populate_stations(&kind);
                    }
                    ctx.render()?;
                }
            }
            (RADIO_STATES, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any
                    .downcast::<
                        (String, Result<Vec<crate::radio::StateGroup>, anyhow::Error>),
                    >()
                {
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
                if let Ok(boxed) = any
                    .downcast::<(String, Result<Vec<DirectoryStation>, anyhow::Error>)>()
                {
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
                if let Ok(boxed) = any
                    .downcast::<
                        (String, String, Result<Vec<DirectoryStation>, anyhow::Error>),
                    >()
                {
                    let (country, state, result) = *boxed;
                    match result {
                        Ok(stations) => {
                            if let Some(directory) = self.directory.as_mut()
                                && let Some(group) = directory
                                    .countries
                                    .iter_mut()
                                    .find(|g| g.name == country)
                                && let Some(state_group) = group
                                    .states
                                    .as_mut()
                                    .and_then(|states| {
                                        states.iter_mut().find(|s| s.name == state)
                                    })
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
            (PLAY, MpdQueryResult::Any(any)) => self.handle_play_result(any, ctx)?,
            (crate::ui::modals::paste::PASTE_PLAY, MpdQueryResult::Any(any)) => {
                self.handle_play_result(any, ctx)?;
            }
            _ => {}
        }
        Ok(())
    }
}
