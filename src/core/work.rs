use std::{path::PathBuf, sync::Arc};
use anyhow::Result;
use crossbeam::channel::{Receiver, Sender};
/// The art source (thumbnail URL / item id) the currently playing stream
/// expects in `<cache_dir>/mpris-art`. Set by `ensure_mpris_metadata` on
/// every song change; the work thread only writes art for the *current*
/// source, so a slow download for a previous stream can never overwrite the
/// current thumbnail (which showed a stale image in the media controls).
static EXPECTED_MPRIS_ART: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(
    None,
);
/// Record the art source the current stream expects (None = no stream).
pub(crate) fn set_expected_mpris_art(source: Option<String>) {
    *EXPECTED_MPRIS_ART.lock().unwrap_or_else(|p| p.into_inner()) = source;
}
/// Whether `source` is still the art the current stream expects.
pub(crate) fn is_expected_mpris_art(source: &str) -> bool {
    EXPECTED_MPRIS_ART.lock().unwrap_or_else(|p| p.into_inner()).as_deref()
        == Some(source)
}
use crate::{
    config::{Config, cli_config::CliConfig},
    jellyfin::{Jellyfin, JellyfinResult},
    radio,
    shared::{
        events::{AppEvent, ClientRequest, WorkDone, WorkRequest},
        lrc::LrcIndex, macros::try_skip, mpd_query::MpdCommand as QueryCmd,
        ytdlp::{YtDlp, YtDlpDownloadError},
    },
};
pub fn init(
    work_rx: Receiver<WorkRequest>,
    client_tx: Sender<ClientRequest>,
    event_tx: Sender<AppEvent>,
    config: Arc<Config>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("work".to_owned())
        .spawn(move || {
            let ytdlp = config.cache_dir.as_ref().map(|dir| YtDlp::new(dir.clone()));
            let cli_config = config.as_ref().into();
            let torrent_config = config.torrent.clone();
            let jellyfin_config_file = config.jellyfin.config_file.clone();
            while let Ok(req) = work_rx.recv() {
                let result = handle_work_request(
                    req,
                    &client_tx,
                    &event_tx,
                    &cli_config,
                    &torrent_config,
                    ytdlp.as_ref(),
                    &jellyfin_config_file,
                );
                try_skip!(
                    event_tx.send(AppEvent::WorkDone(result)),
                    "Failed to send work done notification"
                );
            }
        })
}
fn handle_work_request(
    request: WorkRequest,
    client_tx: &Sender<ClientRequest>,
    event_tx: &Sender<AppEvent>,
    config: &CliConfig,
    torrent_config: &crate::config::torrent::Torrent,
    ytdlp: Option<&YtDlp>,
    jellyfin_config_file: &PathBuf,
) -> Result<WorkDone> {
    match request {
        WorkRequest::FetchRadioDirectory { location, cache_dir } => {
            let directory = radio::fetch_radio_directory(location);
            if directory.error.is_none() {
                radio::save_radio_cache(
                    &radio::radio_cache_path(cache_dir.as_deref()),
                    &directory,
                );
            }
            Ok(WorkDone::MpdCommandFinished {
                id: crate::ui::panes::radio::RADIO_DIRECTORY,
                target: Some(crate::config::tabs::PaneType::Radio {
                    tree: crate::config::tabs::TreeBrowserArgs::default(),
                }),
                data: crate::shared::mpd_query::MpdQueryResult::Any(Box::new(directory)),
            })
        }
        WorkRequest::FetchRadioStates { country, country_code } => {
            let states = match country_code.as_deref() {
                Some(code) => radio::fetch_country_states(code, &country),
                None => Ok(Vec::new()),
            };
            Ok(WorkDone::MpdCommandFinished {
                id: crate::ui::panes::radio::RADIO_STATES,
                target: Some(crate::config::tabs::PaneType::Radio {
                    tree: crate::config::tabs::TreeBrowserArgs::default(),
                }),
                data: crate::shared::mpd_query::MpdQueryResult::Any(
                    Box::new((country, states)),
                ),
            })
        }
        WorkRequest::FetchRadioCountryStations { country, country_code } => {
            let stations = radio::fetch_country_top(&country_code);
            Ok(WorkDone::MpdCommandFinished {
                id: crate::ui::panes::radio::RADIO_COUNTRY_STATIONS,
                target: Some(crate::config::tabs::PaneType::Radio {
                    tree: crate::config::tabs::TreeBrowserArgs::default(),
                }),
                data: crate::shared::mpd_query::MpdQueryResult::Any(
                    Box::new((country, stations)),
                ),
            })
        }
        WorkRequest::FetchRadioStateStations { country, state } => {
            let stations = radio::fetch_state_stations(&country, &state);
            Ok(WorkDone::MpdCommandFinished {
                id: crate::ui::panes::radio::RADIO_STATE_STATIONS,
                target: Some(crate::config::tabs::PaneType::Radio {
                    tree: crate::config::tabs::TreeBrowserArgs::default(),
                }),
                data: crate::shared::mpd_query::MpdQueryResult::Any(
                    Box::new((country, state, stations)),
                ),
            })
        }
        WorkRequest::ResolveYtStreams { urls, action } => {
            let (info, failures) = crate::shared::ytdlp::resolve_audio_urls(&urls);
            Ok(WorkDone::YtStreamsResolved {
                info,
                action,
                failures,
            })
        }
        WorkRequest::PlayTorrent { item, download } => {
            let key = item.source_key();
            let event_tx = event_tx.clone();
            let torrent_config = torrent_config.clone();
            std::thread::Builder::new()
                .name(
                    format!(
                        "torrent-play-{}", key.chars().take(24).collect::< String > ()
                    ),
                )
                .spawn(move || {
                    let result: Result<WorkDone> = (|| {
                        let engine = crate::core::torrent::start_engine(&torrent_config)
                            .map_err(|err| anyhow::anyhow!("{err}"))?;
                        let id = crate::core::torrent::add_torrent(
                                &engine,
                                item.source(),
                            )
                            .map_err(|err| anyhow::anyhow!("{err}"))?;
                        let details = crate::core::torrent::wait_for_files(
                                &engine,
                                &id,
                                None,
                            )
                            .map_err(|err| anyhow::anyhow!("{err}"))?;
                        let torrent_name = details
                            .name
                            .clone()
                            .unwrap_or_else(|| item.label());
                        let (file_idx, file) = crate::core::torrent::pick_playable_file(
                                &details.files,
                            )
                            .ok_or_else(|| {
                                anyhow::anyhow!("No playable media in this torrent")
                            })?;
                        let file_length = file.length;
                        let stream_url = engine.stream_url(&id, file_idx as u64);
                        Ok(WorkDone::TorrentStreamPrepared {
                            key,
                            engine,
                            stream_url,
                            torrent_name,
                            file_name: file.name.clone(),
                            torrent_id: id,
                            file_idx,
                            file_length,
                            download,
                        })
                    })();
                    try_skip!(
                        event_tx.send(AppEvent::WorkDone(result)),
                        "Failed to send work done notification"
                    );
                })
                .map_err(|err| {
                    anyhow::anyhow!("Failed to spawn torrent play thread: {err}")
                })?;
            Ok(WorkDone::None)
        }
        WorkRequest::DownloadTorrent { item, indices } => {
            let key = item.source_key();
            let event_tx = event_tx.clone();
            let torrent_config = torrent_config.clone();
            std::thread::Builder::new()
                .name(
                    format!("torrent-dl-{}", key.chars().take(24).collect::< String > ()),
                )
                .spawn(move || {
                    let result: Result<WorkDone> = (|| {
                        let engine = crate::core::torrent::start_engine(&torrent_config)
                            .map_err(|err| anyhow::anyhow!("{err}"))?;
                        let id = crate::core::torrent::add_torrent(
                                &engine,
                                item.source(),
                            )
                            .map_err(|err| anyhow::anyhow!("{err}"))?;
                        let details = crate::core::torrent::wait_for_files(
                                &engine,
                                &id,
                                None,
                            )
                            .map_err(|err| anyhow::anyhow!("{err}"))?;
                        let torrent_name = details
                            .name
                            .clone()
                            .unwrap_or_else(|| item.label());
                        let files: Vec<crate::core::torrent::ScannedFile> = if indices
                            .is_empty()
                        {
                            crate::core::torrent::pick_playable_file(&details.files)
                                .into_iter()
                                .map(|(idx, file)| crate::core::torrent::ScannedFile {
                                    index: idx,
                                    name: file.name.clone(),
                                    length: file.length,
                                })
                                .collect()
                        } else {
                            indices
                                .iter()
                                .filter_map(|i| {
                                    details.files.get(*i).map(|file| (*i, file))
                                })
                                .map(|(idx, file)| crate::core::torrent::ScannedFile {
                                    index: idx,
                                    name: file.name.clone(),
                                    length: file.length,
                                })
                                .collect()
                        };
                        if files.is_empty() {
                            return Err(
                                anyhow::anyhow!("No playable media in this torrent"),
                            );
                        }
                        Ok(WorkDone::TorrentDownloadPrepared {
                            key,
                            engine,
                            torrent_id: id,
                            torrent_name,
                            files,
                        })
                    })();
                    try_skip!(
                        event_tx.send(AppEvent::WorkDone(result)),
                        "Failed to send work done notification"
                    );
                })
                .map_err(|err| {
                    anyhow::anyhow!("Failed to spawn torrent download thread: {err}")
                })?;
            Ok(WorkDone::None)
        }
        WorkRequest::ScanTorrent { item, cancel } => {
            let key = item.source_key();
            let key_in_thread = key.clone();
            let event_tx = event_tx.clone();
            let torrent_config = torrent_config.clone();
            let spawn = std::thread::Builder::new()
                .name(
                    format!(
                        "torrent-scan-{}", key_in_thread.chars().take(24).collect::<
                        String > ()
                    ),
                )
                .spawn(move || {
                    let result = crate::core::torrent::scan_torrent(
                            &item,
                            &torrent_config,
                            &cancel,
                            &event_tx,
                        )
                        .map_err(|err| err.to_string());
                    try_skip!(
                        event_tx.send(AppEvent::WorkDone(Ok(WorkDone::TorrentScanned {
                        key : key_in_thread, result, }))),
                        "Failed to send work done notification"
                    );
                });
            match spawn {
                Ok(_) => Ok(WorkDone::None),
                Err(err) => {
                    Ok(WorkDone::TorrentScanned {
                        key,
                        result: Err(format!("Failed to start torrent scan: {err}")),
                    })
                }
            }
        }
        WorkRequest::FetchJellyfinViews => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| jf.views().map(JellyfinResult::Views),
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_VIEWS,
                data,
            })
        }
        WorkRequest::FetchJellyfinFolder { parent_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        jf.folder_children(&parent_id)
                            .map(|items| JellyfinResult::Children {
                                parent_id,
                                items,
                            })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_FOLDER,
                data,
            })
        }
        WorkRequest::FetchJellyfinArtists { view_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        jf.artists(&view_id)
                            .map(|items| JellyfinResult::Artists {
                                view_id,
                                items,
                            })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_ARTISTS,
                data,
            })
        }
        WorkRequest::FetchJellyfinAlbums { artist_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        jf.albums_of_artist(&artist_id)
                            .map(|items| JellyfinResult::Albums {
                                artist_id,
                                items,
                            })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_ALBUMS,
                data,
            })
        }
        WorkRequest::FetchJellyfinChapters { item_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        jf.chapters(&item_id)
                            .map(|chapters| JellyfinResult::Chapters {
                                item_id,
                                chapters,
                            })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_CHAPTERS,
                data,
            })
        }
        WorkRequest::FetchJellyfinSeason { season_id, episode_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        let episodes = jf.season_episodes(&season_id)?;
                        let entries: Vec<crate::jellyfin::SeasonEntry> = episodes
                            .iter()
                            .map(|ep| crate::jellyfin::SeasonEntry {
                                title: ep.name.clone(),
                                url: jf.video_stream_url(&ep.id),
                                duration: ep.runtime_secs.map(|s| s as f64),
                            })
                            .collect();
                        let start_index = episodes
                            .iter()
                            .position(|ep| ep.id == episode_id)
                            .unwrap_or(0);
                        Ok(JellyfinResult::SeasonPlaylist {
                            entries,
                            start_index,
                        })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_SEASON_PLAY,
                data,
            })
        }
        WorkRequest::FetchFileChapters { file } => {
            let chapters = fetch_file_chapters(&file);
            Ok(WorkDone::MpdCommandFinished {
                id: crate::ui::panes::queue::FILE_CHAPTERS,
                target: Some(crate::config::tabs::PaneType::Queue),
                data: crate::shared::mpd_query::MpdQueryResult::Any(
                    Box::new((file, chapters)),
                ),
            })
        }
        WorkRequest::FetchYtThumbnail { url } => {
            let result = fetch_yt_thumbnail(&url);
            Ok(WorkDone::MpdCommandFinished {
                id: crate::ui::panes::album_art::YT_THUMBNAIL,
                target: Some(crate::config::tabs::PaneType::AlbumArt),
                data: crate::shared::mpd_query::MpdQueryResult::Any(Box::new(result)),
            })
        }
        WorkRequest::SaveMprisArt { url } => {
            match fetch_yt_thumbnail(&url) {
                Ok(bytes) if is_expected_mpris_art(&url) => save_mpris_art(None, &bytes),
                Ok(_) => log::debug!("Skipping stale MPRIS art (stream changed)"),
                Err(err) => {
                    log::warn!(error:? = err; "Failed to fetch MPRIS art");
                    if is_expected_mpris_art(&url) {
                        let _ = std::fs::remove_file(
                            crate::ui::modals::paste::mpris_art_path(None),
                        );
                    }
                }
            }
            Ok(WorkDone::None)
        }
        WorkRequest::SaveMpvMprisArt { url, cache_dir } => {
            match fetch_yt_thumbnail(&url) {
                Ok(bytes) => {
                    let path = crate::ui::modals::paste::mpv_mpris_art_path(
                        cache_dir.as_deref(),
                    );
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(err) = std::fs::write(&path, bytes) {
                        log::warn!(error:? = err; "Failed to write mpv MPRIS art");
                    }
                }
                Err(err) => log::warn!(error:? = err; "Failed to fetch mpv MPRIS art"),
            }
            Ok(WorkDone::None)
        }
        WorkRequest::FetchJellyfinMpris { item_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        let item = jf.item(&item_id)?;
                        let image = jf.fetch_image(&item_id, 600).unwrap_or_default();
                        Ok(JellyfinResult::Mpris {
                            item,
                            image,
                        })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_MPRIS,
                data,
            })
        }
        WorkRequest::FetchJellyfinSongs { album_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        jf.songs_of_album(&album_id)
                            .map(|items| JellyfinResult::Songs {
                                album_id,
                                items,
                            })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_SONGS,
                data,
            })
        }
        WorkRequest::FetchJellyfinItem { item_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| { jf.item(&item_id).map(JellyfinResult::Item) },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_ITEM,
                data,
            })
        }
        WorkRequest::FetchJellyfinResume { item_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        jf.resume_position_secs(&item_id)
                            .map(|seconds| JellyfinResult::ResumePosition {
                                seconds,
                            })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_RESUME,
                data,
            })
        }
        WorkRequest::FetchJellyfinImage { item_id, fallback_item_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        match jf.fetch_image(&item_id, 600) {
                            Ok(bytes) => {
                                Ok(JellyfinResult::Image {
                                    item_id,
                                    bytes,
                                })
                            }
                            Err(primary_err) => {
                                if let Some(fallback) = fallback_item_id {
                                    jf.fetch_image(&fallback, 600)
                                        .map(|bytes| JellyfinResult::Image {
                                            item_id,
                                            bytes,
                                        })
                                        .map_err(|_| primary_err)
                                } else {
                                    Err(primary_err)
                                }
                            }
                        }
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_IMAGE,
                data,
            })
        }
        WorkRequest::FetchJellyfinVideoArt { item_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        jf.fetch_image(&item_id, 600)
                            .map(|bytes| JellyfinResult::Image {
                                item_id,
                                bytes,
                            })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::album_art::JF_VIDEO_ART,
                data,
            })
        }
        WorkRequest::Command(command) => {
            let callback = command.execute(config)?;
            try_skip!(
                client_tx.send(ClientRequest::Command(QueryCmd { callback })),
                "Failed to send client request to complete command"
            );
            Ok(WorkDone::None)
        }
        WorkRequest::IndexLyrics { lyrics_dir } => {
            let index = LrcIndex::index(&PathBuf::from(lyrics_dir));
            Ok(WorkDone::LyricsIndexed { index })
        }
        WorkRequest::IndexSingleLrc { path } => {
            let metadata = LrcIndex::index_single(&path)?;
            Ok(WorkDone::SingleLrcIndexed {
                path,
                metadata,
            })
        }
        WorkRequest::ResizeImage(fn_once) => {
            Ok(WorkDone::ImageResized {
                data: fn_once(),
            })
        }
        WorkRequest::SearchYt { query, kind, limit, interactive, position } => {
            if ytdlp.is_none() {
                anyhow::bail!("Youtube support requires 'cache_dir' to be configured")
            }
            let limit = if interactive { limit } else { 1 };
            let items = YtDlp::search(kind, &query, limit)?;
            Ok(WorkDone::SearchYtResults {
                items,
                position,
                interactive,
            })
        }
        WorkRequest::YtDlpDownload { id, url, spec } => {
            let result = if let Some(spec) = spec.as_ref() {
                match ytdlp {
                    Some(ytdlp) => ytdlp.download_stream(&url, spec),
                    None => YtDlp::new(PathBuf::new()).download_stream(&url, spec),
                }
            } else {
                let Some(ytdlp) = ytdlp else {
                    return Ok(WorkDone::YtDlpDownloaded {
                        id,
                        result: Err(
                            YtDlpDownloadError::InvalidConfig(
                                "Youtube support requires 'cache_dir' to be configured",
                            ),
                        ),
                        spec: None,
                    });
                };
                ytdlp.download_single(&url)
            };
            Ok(WorkDone::YtDlpDownloaded {
                id,
                result,
                spec,
            })
        }
        WorkRequest::YtDlpResolvePlaylist { playlist } => {
            let Some(ytdlp) = ytdlp else {
                anyhow::bail!("Youtube support requires 'cache_dir' to be configured")
            };
            let result = ytdlp.resolve_playlist_urls(&playlist)?;
            Ok(WorkDone::YtDlpPlaylistResolved {
                urls: result,
            })
        }
    }
}
/// Chapter markers of a local file via ffprobe. The MPD song file is a
/// relative path under the music directory; the absolute path is resolved
/// from mpd.conf (the `config` command is TCP-restricted).
fn fetch_file_chapters(
    file: &str,
) -> Result<Vec<crate::shared::chapters::Chapter>, String> {
    let music_dir = crate::ui::modals::paste::music_directory()
        .ok_or_else(|| "cannot determine MPD music directory".to_owned())?;
    let path = std::path::Path::new(&music_dir).join(file);
    if !path.is_file() {
        return Err(format!("file not found: {}", path.display()));
    }
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-print_format", "json", "-show_chapters"])
        .arg(&path)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    #[derive(serde::Deserialize)]
    struct FfprobeChapter {
        #[serde(default)]
        start_time: Option<String>,
        #[serde(default)]
        end_time: Option<String>,
        #[serde(default)]
        tags: Option<serde_json::Value>,
    }
    #[derive(serde::Deserialize)]
    struct FfprobeChapters {
        #[serde(default)]
        chapters: Vec<FfprobeChapter>,
    }
    let parsed: FfprobeChapters = serde_json::from_slice(&output.stdout)
        .map_err(|e| e.to_string())?;
    let mut chapters = Vec::new();
    for (idx, chapter) in parsed.chapters.iter().enumerate() {
        let start = chapter
            .start_time
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let end = chapter
            .end_time
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(start);
        let title = chapter
            .tags
            .as_ref()
            .and_then(|t| t.get("title"))
            .and_then(|t| t.as_str())
            .filter(|t| !t.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Chapter {}", idx + 1));
        chapters
            .push(crate::shared::chapters::Chapter {
                title,
                start_secs: start,
                end_secs: end,
            });
    }
    Ok(chapters)
}
/// Write the MPRIS album-art file (`<cache_dir>/mpris-art`); written for
/// the media controls (no patched mpDris2 serves it anymore).
pub(crate) fn save_mpris_art(cache_dir: Option<&std::path::Path>, bytes: &[u8]) {
    let path = crate::ui::modals::paste::mpris_art_path(cache_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::write(&path, bytes) {
        log::warn!(error:? = err; "Failed to write MPRIS art");
    }
}
/// Download a YouTube thumbnail (used as album art for yt audio streams).
fn fetch_yt_thumbnail(url: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("s2udio/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = agent.get(url).call().map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    response.into_reader().read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}
/// Run a Jellyfin API call with the configured server credentials. A missing
/// or unreadable jellytui config is reported through the result (the pane
/// shows a notice instead of crashing).
fn jellyfin_handle(
    config_file: &PathBuf,
    f: impl FnOnce(&Jellyfin) -> Result<JellyfinResult>,
) -> Result<JellyfinResult> {
    let sidecar = crate::config::jellyfin::jellyfin_sidecar_path();
    let jf = Jellyfin::load(config_file, Some(&sidecar))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Jellyfin is not configured: cannot read {}", config_file.display()
            )
        })?;
    f(&jf)
}
