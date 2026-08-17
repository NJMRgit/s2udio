//! Internet radio directory fetching (radio-browser.info).
//!
//! The Radio tab pulls station lists from the community radio-browser.info
//! database: the closest stations to the user's location plus the most-voted
//! stations of the whole directory grouped by country. All HTTP work happens
//! on the work thread, never on the MPD thread. The last successful result is
//! cached on disk so the tab renders instantly on the next start.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A station from the radio-browser.info directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectoryStation {
    pub name: String,
    /// Stream URL ready to be handed to MPD (`url_resolved` when present).
    pub url: String,
    pub country: String,
    pub country_code: String,
    pub state: Option<String>,
    pub city: Option<String>,
    pub language: Vec<String>,
    pub tags: Vec<String>,
    pub codec: Option<String>,
    pub bitrate: Option<u32>,
    pub votes: u32,
    pub distance_km: Option<f64>,
    pub geo_lat: Option<f64>,
    pub geo_long: Option<f64>,
    pub favicon: Option<String>,
    pub homepage: Option<String>,
}

/// Everything the Radio tab needs from the directory, in one shot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RadioDirectory {
    pub location: Option<crate::config::radio::GeoLocation>,
    /// ISO country code of the user's detected country; its group is shown
    /// complete (all stations), all other countries only their top 15.
    #[serde(default)]
    pub country_code: Option<String>,
    /// Closest stations, sorted by distance (up to [`LOCAL_LIMIT`]).
    pub local: Vec<DirectoryStation>,
    /// Countries, biggest first. The user's country comes first and carries
    /// its full station list; the rest only their top 15 (states and state
    /// stations are fetched lazily on expand).
    pub countries: Vec<CountryGroup>,
    /// Set when the directory could not be reached; the tab shows it as a
    /// notice row instead of failing silently.
    pub error: Option<String>,
}

/// One country group in the directory: its top [`COUNTRY_LIMIT`] stations
/// (most-voted first). `states` is filled lazily when the group is expanded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CountryGroup {
    pub name: String,
    /// ISO country code (for the state-list fetch).
    #[serde(default)]
    pub code: Option<String>,
    pub top: Vec<DirectoryStation>,
    #[serde(default)]
    pub states: Option<Vec<StateGroup>>,
}

/// A state/province inside a country. `stations` is filled lazily when the
/// state is expanded (fetched from the API).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateGroup {
    pub name: String,
    /// Number of stations in this state (from the states endpoint).
    pub count: usize,
    #[serde(default)]
    pub stations: Option<Vec<DirectoryStation>>,
}

/// Cache file location: `<cache_dir>/radio-directory.json`, defaulting to
/// `~/.cache/s2udio` (round 23; the legacy `~/.cache/rmpc` file is
/// returned when it still exists — migration read).
pub fn radio_cache_path(cache_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = cache_dir {
        return dir.join("radio-directory.json");
    }
    let new = crate::shared::paths::s2udio_cache_dir()
        .unwrap_or_else(|| {
            crate::config::utils::tilde_expand("~/.cache/s2udio").into_owned().into()
        })
        .join("radio-directory.json");
    let legacy: PathBuf = crate::config::utils::tilde_expand("~/.cache/rmpc").into_owned().into();
    if new.exists() {
        new
    } else if legacy.join("radio-directory.json").exists() {
        legacy.join("radio-directory.json")
    } else {
        new
    }
}

pub fn load_radio_cache(path: &Path) -> Option<RadioDirectory> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn save_radio_cache(path: &Path, directory: &RadioDirectory) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_vec(directory) {
        let _ = std::fs::write(path, data);
    }
}

const DIRECTORY_BASE: &str = "https://all.api.radio-browser.info/json";
const LOCAL_LIMIT: usize = 100;
const DIRECTORY_LIMIT: usize = 1_000;
/// Per-country cap: every region caches its top 100 most-voted stations
/// (loaded from the global list at directory time, completed / reloaded on
/// demand from the per-country endpoint).
const COUNTRY_LIMIT: usize = 100;
/// Radius (metres) around the user for the Local section.
const LOCAL_RADIUS_M: u32 = 300_000;
/// Candidate pool for the Local section (bigger than the limit so the closest
/// stations win even when the radius is dense).
const LOCAL_CANDIDATES: usize = 500;

/// Tolerant extraction of a station entry. The directory's JSON is messy:
/// fields come and go and switch types (e.g. `tags` is an array on some
/// entries and a comma separated string on others), so every field is read
/// from a generic value.
fn station_from_value(value: &serde_json::Value) -> Option<DirectoryStation> {
    fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
        value.get(key)?.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
    }
    fn list_field(value: &serde_json::Value, key: &str) -> Vec<String> {
        match value.get(key) {
            Some(serde_json::Value::Array(items)) => {
                items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect()
            }
            Some(serde_json::Value::String(s)) => {
                s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
            }
            _ => Vec::new(),
        }
    }
    fn u32_field(value: &serde_json::Value, key: &str) -> Option<u32> {
        match value.get(key) {
            Some(serde_json::Value::Number(n)) => n.as_u64().map(|n| n as u32),
            _ => None,
        }
    }
    fn f64_field(value: &serde_json::Value, key: &str) -> Option<f64> {
        match value.get(key) {
            Some(serde_json::Value::Number(n)) => n.as_f64(),
            // geo_lat / geo_long come back as strings ("43.64" or "").
            Some(serde_json::Value::String(s)) => s.trim().parse().ok().filter(|v: &f64| *v != 0.0),
            _ => None,
        }
    }

    let name = str_field(value, "name")?;
    let url = str_field(value, "url_resolved")
        .or_else(|| str_field(value, "url"))
        .filter(|url| url.starts_with("http://") || url.starts_with("https://"))?;
    Some(DirectoryStation {
        name,
        url,
        country: str_field(value, "country").unwrap_or_else(|| "Unknown".to_owned()),
        country_code: str_field(value, "countrycode").unwrap_or_else(|| "??".to_owned()),
        state: str_field(value, "state"),
        city: str_field(value, "city"),
        language: list_field(value, "language"),
        tags: list_field(value, "tags"),
        codec: str_field(value, "codec"),
        bitrate: u32_field(value, "bitrate"),
        votes: u32_field(value, "votes").unwrap_or(0),
        distance_km: f64_field(value, "distance"),
        geo_lat: f64_field(value, "geo_lat"),
        geo_long: f64_field(value, "geo_long"),
        favicon: str_field(value, "favicon"),
        homepage: str_field(value, "homepage"),
    })
}

/// Great-circle distance in kilometres between two coordinates.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0;
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    R * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

/// Guess the user's location and country code from their IP. No key, plain
/// HTTPS; if it fails the Local section and the "your country" group are
/// simply omitted.
pub fn fetch_geo_location() -> Result<(crate::config::radio::GeoLocation, Option<String>)> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("rmpc/", env!("CARGO_PKG_VERSION")))
        .build();
    #[derive(Deserialize)]
    struct GeoResponse {
        #[serde(default)]
        latitude: Option<f64>,
        #[serde(default)]
        longitude: Option<f64>,
        #[serde(default)]
        city: Option<String>,
        #[serde(default)]
        country: Option<String>,
        #[serde(default)]
        country_code: Option<String>,
    }
    let resp: GeoResponse = agent
        .get("https://ipwho.is/")
        .call()
        .context("Cannot reach ipwho.is")?
        .into_json()
        .context("Cannot parse ipwho.is response")?;
    let (Some(lat), Some(lon)) = (resp.latitude, resp.longitude) else {
        anyhow::bail!("ipwho.is did not return coordinates");
    };
    let name = match (resp.city, resp.country) {
        (Some(city), Some(country)) => Some(format!("{city}, {country}")),
        (Some(city), None) => Some(city),
        (None, Some(country)) => Some(country),
        (None, None) => None,
    };
    let country_code = resp
        .country_code
        .map(|code| code.trim().to_uppercase())
        .filter(|code| code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()));
    Ok((crate::config::radio::GeoLocation { lat, lon, name }, country_code))
}

/// Group key for stations: country code when valid, else the country name.
/// Short display label for long official country names. The API's real
/// name is kept for queries and lookups (`CountryGroup.name`); only the
/// tree label is shortened.
pub fn short_country_name(name: &str) -> String {
    match name.trim().to_lowercase().as_str() {
        "the united states of america" => "United States".to_owned(),
        "islamic republic of iran" => "Iran".to_owned(),
        "the russian federation" => "Russia".to_owned(),
        "the united kingdom of great britain and northern ireland" => "United Kingdom".to_owned(),
        "syrian arab republic" => "Syria".to_owned(),
        "the czech republic" => "Czechia".to_owned(),
        "the republic of korea" | "republic of korea" => "South Korea".to_owned(),
        _ => name.to_owned(),
    }
}

fn group_key(station: &DirectoryStation) -> String {
    let code = station.country_code.trim().to_uppercase();
    if code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        format!("code:{code}")
    } else {
        format!("name:{}", station.country.to_lowercase())
    }
}

/// Fetch the closest stations, the user's whole country and the top stations
/// of the directory. Never panics: failures are reported through
/// `RadioDirectory::error`. The requests run concurrently so one slow
/// endpoint does not delay the others.
pub fn fetch_radio_directory(
    configured_location: Option<crate::config::radio::GeoLocation>,
) -> RadioDirectory {
    // Resolve the location once; the country code (from geo-IP) decides which
    // country group is shown complete.
    let (location, country_code) = match configured_location {
        Some(loc) => {
            let code = fetch_geo_location().ok().and_then(|(_, code)| code);
            (Some(loc), code)
        }
        None => match fetch_geo_location() {
            Ok((loc, code)) => (Some(loc), code),
            Err(_) => (None, None),
        },
    };

    let (local_result, directory_result, country_result) = std::thread::scope(|scope| {
        let location = location.clone();
        let local = scope.spawn(move || fetch_local(location));
        let directory = scope.spawn(fetch_directory);
        let country_code = country_code.clone();
        let country = scope.spawn(move || {
            country_code.as_deref().map(fetch_country_top).unwrap_or_else(|| Ok(Vec::new()))
        });
        (
            local.join().expect("local fetch thread panicked"),
            directory.join().expect("directory fetch thread panicked"),
            country.join().expect("country fetch thread panicked"),
        )
    });

    let mut errors: Vec<String> = Vec::new();
    let mut local: Vec<DirectoryStation> = Vec::new();
    match local_result {
        Ok((_, stations)) => {
            local = stations;
            local.sort_by(|a, b| {
                a.distance_km.partial_cmp(&b.distance_km).unwrap_or(std::cmp::Ordering::Equal)
            });
            local.truncate(LOCAL_LIMIT);
        }
        Err(err) => errors.push(format!("local stations: {err}")),
    }

    let mut countries: Vec<CountryGroup> = match directory_result {
        Ok(stations) => stations,
        Err(err) => {
            errors.push(format!("directory: {err}"));
            Vec::new()
        }
    };

    // The user's own country goes first with its own top 15 (fetched above —
    // it rarely appears in the global top-1000). Every other country keeps
    // its top 15 from the global list; states and state stations load lazily.
    match country_result {
        Ok(user_stations) if !user_stations.is_empty() => {
            let country_name = user_stations[0].country.clone();
            if let Some(code) = &country_code {
                let user_key = format!("code:{code}");
                countries
                    .retain(|group| group.top.first().is_none_or(|s| group_key(s) != user_key));
            }
            countries.insert(
                0,
                CountryGroup {
                    name: country_name,
                    code: country_code.clone(),
                    top: user_stations,
                    states: None,
                },
            );
        }
        Ok(_) => {}
        Err(err) => errors.push(format!("your country: {err}")),
    }
    countries.sort_by(|a, b| b.top.len().cmp(&a.top.len()).then(a.name.cmp(&b.name)));
    // Keep the user's own country pinned at the top of the tree.
    if let Some(code) = &country_code {
        let user_key = format!("code:{code}");
        if let Some(idx) = countries
            .iter()
            .position(|group| group.top.first().is_some_and(|s| group_key(s) == user_key))
            && idx != 0
        {
            let group = countries.remove(idx);
            countries.insert(0, group);
        }
    }

    RadioDirectory {
        location,
        country_code,
        local,
        countries,
        error: if errors.is_empty() { None } else { Some(errors.join("; ")) },
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("rmpc/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Resolve the location and fetch the closest stations. The Local section is
/// skipped when no coordinates can be found.
fn fetch_local(
    location: Option<crate::config::radio::GeoLocation>,
) -> Result<(Option<crate::config::radio::GeoLocation>, Vec<DirectoryStation>)> {
    let Some(loc) = &location else {
        return Ok((None, Vec::new()));
    };

    // `order=distance` does not exist; the API instead filters with
    // `geo_distance` (metres) when `has_geo_info` and lat/lon are given. The
    // distance itself is computed client-side from the coordinates.
    let url = format!(
        "{DIRECTORY_BASE}/stations/search?has_geo_info=true&geo_lat={}&geo_long={}\
         &geo_distance={LOCAL_RADIUS_M}&order=votes&reverse=true&limit={LOCAL_CANDIDATES}&hidebroken=true",
        loc.lat, loc.lon
    );
    let stations = fetch_stations(&agent(), &url)?;
    let local = stations
        .into_iter()
        .filter_map(|mut station| {
            let (Some(lat), Some(lon)) = (station.geo_lat, station.geo_long) else {
                return None;
            };
            station.distance_km = Some(haversine_km(loc.lat, loc.lon, lat, lon));
            Some(station)
        })
        .collect();
    Ok((Some(loc.clone()), local))
}

/// Top stations of the whole directory: every country gets its top
/// [`COUNTRY_LIMIT`] by votes, biggest countries first.
fn fetch_directory() -> Result<Vec<CountryGroup>> {
    let url = format!(
        "{DIRECTORY_BASE}/stations/search?order=votes&reverse=true&hidebroken=true&limit={DIRECTORY_LIMIT}"
    );
    let stations = fetch_stations(&agent(), &url)?;

    // The list comes in vote order, so the first COUNTRY_LIMIT per group are
    // the country's most-voted stations. Group by country code to survive
    // name-case inconsistencies.
    let mut groups: std::collections::BTreeMap<String, (String, Vec<DirectoryStation>)> =
        std::collections::BTreeMap::new();
    for station in stations {
        let key = group_key(&station);
        let entry = groups.entry(key).or_insert_with(|| (station.country.clone(), Vec::new()));
        if entry.1.len() < COUNTRY_LIMIT {
            entry.1.push(station);
        }
    }
    let mut countries: Vec<CountryGroup> = groups
        .into_values()
        .map(|(name, top)| {
            let code = top.first().and_then(|s| {
                let c = s.country_code.trim().to_uppercase();
                (c.len() == 2 && c.chars().all(|ch| ch.is_ascii_alphabetic())).then_some(c)
            });
            CountryGroup { name, code, top, states: None }
        })
        .collect();
    countries.sort_by(|a, b| b.top.len().cmp(&a.top.len()).then(a.name.cmp(&b.name)));
    Ok(countries)
}

/// The top [`COUNTRY_LIMIT`] most-voted stations of one country, used to
/// complete / refresh a region's cached station list.
pub fn fetch_country_top(country_code: &str) -> Result<Vec<DirectoryStation>> {
    let url = format!(
        "{DIRECTORY_BASE}/stations/bycountrycodeexact/{country_code}?order=votes&reverse=true&hidebroken=true&limit={COUNTRY_LIMIT}"
    );
    fetch_stations(&agent(), &url)
}

/// The states/provinces of a country, derived from a bounded sample of its
/// stations (the dedicated states endpoint ignores the country filter for
/// some countries and returns junk names). One request per country expand,
/// ~300 stations — not a full-country load.
pub fn fetch_country_states(country_code: &str, country_name: &str) -> Result<Vec<StateGroup>> {
    let url = format!(
        "{DIRECTORY_BASE}/stations/bycountrycodeexact/{country_code}?order=votes&reverse=true&hidebroken=true&limit=300"
    );
    let stations = fetch_stations(&agent(), &url)?;

    // Group by normalized province name, folding case. Messy directory
    // values like "Ottawa, ON" or "Sao Paulo (Brazil)" are folded into the
    // right province (Canada) or dropped, so the tree only shows real
    // provinces.
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut spellings: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, usize>,
    > = std::collections::BTreeMap::new();
    for station in stations {
        let state = station.state.as_deref().unwrap_or("");
        // The country name stored as a state is junk, not a province.
        if state.trim().eq_ignore_ascii_case(country_name.trim()) {
            continue;
        }
        if let Some(normalized) = normalize_state(country_code, state) {
            // Stations without a state have no province to group under; the
            // "Unknown" bucket would be a dead category (nothing to query).
            if normalized.eq_ignore_ascii_case("unknown") {
                continue;
            }
            let key = normalized.to_lowercase();
            *counts.entry(key.clone()).or_insert(0) += 1;
            *spellings.entry(key).or_default().entry(normalized).or_insert(0) += 1;
        }
    }
    let mut states: Vec<StateGroup> = counts
        .into_iter()
        .map(|(key, count)| {
            let display = spellings
                .get(&key)
                .and_then(|variants| variants.iter().max_by_key(|(_, n)| **n))
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| key.clone());
            StateGroup { name: display, count, stations: None }
        })
        .collect();
    states.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
    Ok(states)
}

/// Normalize a station's `state` value into a real province name, or `None`
/// for junk we do not want in the tree. The directory stores cities and
/// "City, PROV" strings in the state field; Canada's are mapped to their
/// province, other countries' comma/paren forms are dropped.
fn normalize_state(country_code: &str, state: &str) -> Option<String> {
    let trimmed = state.trim();
    if trimmed.is_empty() {
        return Some("Unknown".to_owned());
    }
    if trimmed.contains('(') || trimmed.contains(')') {
        return None; // legacy junk like "Sao Paulo (Brazil)"
    }
    let lower = trimmed.to_lowercase();
    if country_code.eq_ignore_ascii_case("ca") {
        // "Ottawa, ON" -> province code -> name.
        if let Some((_, code)) = trimmed.rsplit_once(',') {
            let code = code.trim().to_uppercase();
            return ca_province_code(&code).map(str::to_owned);
        }
        if let Some(province) = ca_city_province(&lower) {
            return Some(province.to_owned());
        }
        if lower == "newfoundland" {
            return Some("Newfoundland and Labrador".to_owned());
        }
    } else if trimmed.contains(',') {
        return None;
    }
    Some(trimmed.to_owned())
}

fn ca_province_code(code: &str) -> Option<&'static str> {
    Some(match code {
        "ON" => "Ontario",
        "QC" => "Quebec",
        "NS" => "Nova Scotia",
        "NB" => "New Brunswick",
        "MB" => "Manitoba",
        "BC" => "British Columbia",
        "PE" => "Prince Edward Island",
        "SK" => "Saskatchewan",
        "AB" => "Alberta",
        "NL" => "Newfoundland and Labrador",
        "YT" => "Yukon",
        "NT" => "Northwest Territories",
        "NU" => "Nunavut",
        _ => return None,
    })
}

fn ca_city_province(city: &str) -> Option<&'static str> {
    Some(match city {
        "toronto" | "ottawa" | "hamilton" | "london" | "mississauga" | "brampton" | "markham"
        | "vaughan" | "kitchener" | "windsor" | "kingston" | "guelph" | "waterloo" | "oakville"
        | "burlington" | "barrie" | "oshawa" | "st. catharines" => "Ontario",
        "montreal" | "quebec city" | "sherbrooke" | "trois-rivieres" | "trois rivières"
        | "laval" | "gatineau" => "Quebec",
        "vancouver" | "victoria" | "kelowna" | "surrey" | "burnaby" | "richmond" | "nanaimo"
        | "kamloops" | "prince george" => "British Columbia",
        "calgary" | "edmonton" | "red deer" | "lethbridge" => "Alberta",
        "winnipeg" | "brandon" => "Manitoba",
        "halifax" | "sydney" | "truro" => "Nova Scotia",
        "saskatoon" | "regina" | "prince albert" => "Saskatchewan",
        "st. john's" | "st john's" | "saint john" | "fredericton" | "moncton" => "New Brunswick",
        "charlottetown" => "Prince Edward Island",
        "whitehorse" => "Yukon",
        "yellowknife" => "Northwest Territories",
        "iqaluit" => "Nunavut",
        _ => return None,
    })
}

/// Every station of one state/province (most-voted first), used when a state
/// group is expanded. The advanced search filters by exact country + state.
pub fn fetch_state_stations(country: &str, state: &str) -> Result<Vec<DirectoryStation>> {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("country", country)
        .append_pair("countryExact", "true")
        .append_pair("state", state)
        .append_pair("stateExact", "true")
        .append_pair("order", "votes")
        .append_pair("reverse", "true")
        .append_pair("hidebroken", "true")
        .append_pair("limit", "2000")
        .finish();
    let url = format!("{DIRECTORY_BASE}/stations/search?{query}");
    fetch_stations(&agent(), &url)
}

fn fetch_stations(agent: &ureq::Agent, url: &str) -> Result<Vec<DirectoryStation>> {
    let raw: Vec<serde_json::Value> = agent
        .get(url)
        .call()
        .with_context(|| format!("Cannot fetch {url}"))?
        .into_json()
        .context("Cannot parse station directory")?;
    Ok(raw.iter().filter_map(station_from_value).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn station_from_value_uses_url_resolved() {
        let value = json!({
            "name": "My Radio",
            "url": "http://original.example/stream",
            "url_resolved": "http://resolved.example/stream",
            "country": "Germany",
            "countrycode": "DE",
            "tags": "rock,  indie",
            "bitrate": 128,
            "votes": 42,
            "distance": 12.5,
        });
        let station = station_from_value(&value).unwrap();
        assert_eq!(station.name, "My Radio");
        assert_eq!(station.url, "http://resolved.example/stream");
        assert_eq!(station.country, "Germany");
        assert_eq!(station.tags, vec!["rock", "indie"]);
        assert_eq!(station.bitrate, Some(128));
        assert_eq!(station.votes, 42);
        assert_eq!(station.distance_km, Some(12.5));
    }

    #[test]
    fn station_from_value_handles_array_tags() {
        let value = json!({
            "name": "X",
            "url": "http://x.example",
            "tags": ["a", "b"],
            "language": "english",
        });
        let station = station_from_value(&value).unwrap();
        assert_eq!(station.tags, vec!["a", "b"]);
        assert_eq!(station.language, vec!["english"]);
    }

    #[test]
    fn station_from_value_rejects_non_http() {
        let value = json!({"name": "X", "url": "ftp://nope"});
        assert!(station_from_value(&value).is_none());
    }

    #[test]
    fn station_from_value_defaults_country() {
        let value = json!({"name": "X", "url": "http://x.example", "countrycode": "DE"});
        let station = station_from_value(&value).unwrap();
        assert_eq!(station.country, "Unknown");
        assert_eq!(station.country_code, "DE");
    }
}
