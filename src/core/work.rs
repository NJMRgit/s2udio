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
            // Round 111: yt-dlp resolves run on their own small pool instead
            // of inline on this single thread. Each resolve is a `yt-dlp`
            // subprocess costing seconds, so running them here made every
            // request queue behind them - measured live, a merely-selected
            // row's metadata warm waited 22.6 s behind unrelated resolves.
            let resolves = ResolvePool::new(&event_tx);
            while let Ok(req) = work_rx.recv() {
                // The resolve requests are handed to the pool and answer with
                // their own `WorkDone`, so this thread stays free for
                // everything else.
                if let WorkRequest::ResolveYtStreams {
                    urls,
                    action,
                    meta_only,
                    play_stamp,
                } = req
                {
                    resolves.submit(urls, action, meta_only, play_stamp);
                    continue;
                }
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
/// Round 111: how many `yt-dlp` resolves may run at once. Each is a network
/// call costing seconds, and they are independent of one another, so a couple
/// of workers remove the head-of-line blocking without stampeding YouTube (or
/// the user's connection) - a queue of playlist rows still resolves in order,
/// just not strictly one-at-a-time behind a selection warm.
const RESOLVE_WORKERS: usize = 2;

/// A bounded pool for `yt-dlp` resolves.
///
/// The `work` thread submits a job and immediately returns to its own queue;
/// the pool's workers answer with `AppEvent::WorkDone(YtStreamsResolved)`, the
/// same message a synchronous resolve produced, so the event loop and every
/// downstream action are unchanged.
struct ResolvePool {
    tx: crossbeam::channel::Sender<ResolveJob>,
}

/// One queued resolve.
struct ResolveJob {
    urls: Vec<String>,
    action: crate::ui::modals::paste::YtAction,
    meta_only: bool,
    /// The play-request generation (`shared::ytdlp::play_request`), `None`
    /// for a resolve that is not the user's current play intent.
    play_stamp: Option<u64>,
}

impl ResolvePool {
    fn new(event_tx: &Sender<AppEvent>) -> Self {
        let (tx, rx) = crossbeam::channel::unbounded::<ResolveJob>();
        for idx in 0..RESOLVE_WORKERS {
            let rx = rx.clone();
            let event_tx = event_tx.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("yt-resolve-{idx}"))
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        // 2026-10-25: a play request superseded while this job
                        // waited is not run at all — the user moved on, and
                        // its result would be dropped anyway.
                        if let Some(stamp) = job.play_stamp
                            && !crate::shared::ytdlp::play_request::is_current(stamp)
                        {
                            log::debug!(
                                generation = stamp;
                                "Skipping a superseded play resolve"
                            );
                            continue;
                        }
                        let (info, failures) = if job.meta_only {
                            crate::shared::ytdlp::resolve_meta_urls(&job.urls)
                        } else {
                            crate::shared::ytdlp::resolve_audio_urls(&job.urls)
                        };
                        let result = Ok(WorkDone::YtStreamsResolved {
                            info,
                            action: job.action,
                            failures,
                            play_stamp: job.play_stamp,
                        });
                        try_skip!(
                            event_tx.send(AppEvent::WorkDone(result)),
                            "Failed to send a resolved stream"
                        );
                    }
                });
            if let Err(err) = spawned {
                log::error!(error:? = err; "Failed to start a yt-dlp resolve worker");
            }
        }
        Self { tx }
    }
    /// Queue a resolve. A closed pool (the app is shutting down) simply drops
    /// the job: the row keeps its tagged link and resolves again when played.
    fn submit(
        &self,
        urls: Vec<String>,
        action: crate::ui::modals::paste::YtAction,
        meta_only: bool,
        play_stamp: Option<u64>,
    ) {
        try_skip!(
            self.tx.send(ResolveJob { urls, action, meta_only, play_stamp }),
            "Failed to queue a stream resolve"
        );
    }
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
        // Round 111: reached only if a caller bypasses the pool (the `work`
        // thread intercepts these); kept as the full, synchronous resolve.
        WorkRequest::ResolveYtStreams { urls, action, meta_only, play_stamp } => {
            let (info, failures) = if meta_only {
                crate::shared::ytdlp::resolve_meta_urls(&urls)
            } else {
                crate::shared::ytdlp::resolve_audio_urls(&urls)
            };
            Ok(WorkDone::YtStreamsResolved {
                info,
                action,
                failures,
                play_stamp,
            })
        }
        WorkRequest::PlayTorrent { item } => {
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
                            info_hash: details
                                .info_hash
                                .as_deref()
                                .map(|h| h.to_lowercase()),
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
        WorkRequest::StartDlDaemon => {
            let event_tx = event_tx.clone();
            std::thread::Builder::new()
                .name("dl-daemon-start".to_owned())
                .spawn(move || {
                    // Idempotent: a running daemon is reported as ok (it
                    // already owns the spool). The spawn blocks ≤ 15 s on
                    // the daemon's READY line.
                    let result = crate::core::dlctl::start_daemon_for_tui();
                    let _ = event_tx.send(AppEvent::WorkDone(Ok(WorkDone::DlDaemonStarted {
                        result,
                    })));
                })
                .map_err(|err| {
                    anyhow::anyhow!("Failed to spawn the downloader daemon start thread: {err}")
                })?;
            Ok(WorkDone::None)
        }
        WorkRequest::WatchDlResponse { request_id, cancel } => {
            let event_tx = event_tx.clone();
            std::thread::Builder::new()
                .name(format!("dl-wait-{}", request_id.chars().take(24).collect::<String>()))
                .spawn(move || {
                    let result = crate::core::dlctl::wait_for_response(
                        &request_id,
                        &cancel,
                        Some(&event_tx),
                    );
                    let _ = event_tx.send(AppEvent::DlWaitReady {
                        request_id,
                        result,
                    });
                })
                .map_err(|err| {
                    anyhow::anyhow!("Failed to spawn the downloader wait thread: {err}")
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
        WorkRequest::FetchJellyfinHome => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| jf.home().map(|home| JellyfinResult::Home(Box::new(home))),
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_HOME,
                data,
            })
        }
        WorkRequest::FetchJellyfinAncestors { item_id } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        jf.ancestors(&item_id)
                            .map(|items| JellyfinResult::Ancestors {
                                item_id,
                                items,
                            })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_ANCESTORS,
                data,
            })
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
        WorkRequest::FetchJellyfinSearch { query } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        jf.search_hints(&query)
                            .map(|items| JellyfinResult::SearchHints { items })
                    },
                )
                .unwrap_or_else(|err| JellyfinResult::Error(err.to_string()));
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_SEARCH,
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
        WorkRequest::SaveMprisArt { url, crop_letterbox } => {
            match fetch_yt_thumbnail(&url) {
                Ok(bytes) if is_expected_mpris_art(&url) => {
                    let bytes =
                        crate::shared::image::crop_letterbox_bytes(&bytes, crop_letterbox);
                    save_mpris_art(None, &bytes);
                }
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
        WorkRequest::SaveMpvMprisArt { url, cache_dir, crop_letterbox } => {
            match fetch_yt_thumbnail(&url) {
                Ok(bytes) => {
                    let path = crate::ui::modals::paste::mpv_mpris_art_path(
                        cache_dir.as_deref(),
                    );
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    // The media controls read this file, not the pane, so
                    // cropping only the pane left the desktop poster showing
                    // the barred image.
                    let bytes =
                        crate::shared::image::crop_letterbox_bytes(&bytes, crop_letterbox);
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
        WorkRequest::FetchJellyfinImageTagged {
            tag,
            item_id,
            fallback_item_id,
            image_type,
        } => {
            let data = jellyfin_handle(
                    jellyfin_config_file,
                    |jf| {
                        // Try the row's preferred shape, then the item's
                        // own poster/still, then the fallback item (the
                        // series behind a season, the series behind an
                        // episode).
                        let result_tag = tag.clone();
                        jf.tagged_image(
                            &item_id,
                            fallback_item_id.as_deref(),
                            image_type,
                            600,
                        )
                        .map(|bytes| JellyfinResult::TaggedImage {
                            tag: result_tag,
                            bytes,
                        })
                    },
                )
                .unwrap_or_else(|err| {
                    log::debug!(error:? = err; "Home carousel image fetch failed");
                    JellyfinResult::TaggedImage {
                        tag,
                        bytes: Vec::new(),
                    }
                });
            Ok(WorkDone::JellyfinFetched {
                id: crate::ui::panes::jellyfin::JF_WALL_IMAGE,
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
        // Round 96: the Queue tab's search modal. The result is the items
        // themselves (the modal queues or plays one when the user picks it);
        // a failure is carried inside the variant so the modal can show it
        // in place instead of leaving a stuck "Searching…" row.
        WorkRequest::SearchYtModal { request_id, query, kind, limit } => {
            if ytdlp.is_none() {
                return Ok(WorkDone::SearchYtModalResults {
                    request_id,
                    items: Vec::new(),
                    error: Some(
                        "Youtube support requires 'cache_dir' to be configured".to_owned(),
                    ),
                });
            }
            let (items, error) = match YtDlp::search(kind, &query, limit) {
                Ok(items) => (items, None),
                Err(err) => (Vec::new(), Some(err.to_string())),
            };
            Ok(WorkDone::SearchYtModalResults {
                request_id,
                items,
                error,
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
        WorkRequest::YtDlpResolvePlaylist { playlist, action } => {
            let Some(ytdlp) = ytdlp else {
                anyhow::bail!("Youtube support requires 'cache_dir' to be configured")
            };
            // Round 83: listing the playlist is one fast yt-dlp call (it
            // carries each item's title). What happens next is what the
            // action decides: the popup's queue rows enqueue stream entries
            // without downloading anything, the rest hand the item list to
            // the UI.
            let urls = match ytdlp.resolve_playlist_urls(&playlist) {
                Ok(urls) => urls,
                // 2026-10-24: the failure is the request's *result*, not a
                // work error: the import's loading row has to come down and
                // the message has to say the playlist could not be read.
                Err(err) => {
                    return Ok(WorkDone::YtDlpPlaylistFailed {
                        action,
                        error: err.to_string(),
                    });
                }
            };
            // Round 95: the listing carries every item's title, channel and
            // duration, so the queue rows no longer wait for a per-item
            // resolve - `QueueStreams` hands the listed items to the UI,
            // which queues them all at once (the old shape resolved one
            // item at a time and appended each as it landed, so a playlist
            // trickled into the queue over minutes).
            Ok(WorkDone::YtDlpPlaylistResolved { urls, action })
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
                "Jellyfin is not configured — sign in via Esc → Settings → Jellyfin."
            )
        })?;
    f(&jf)
}

#[cfg(test)]
mod mpris_art_guard_tests {
    use super::{is_expected_mpris_art, set_expected_mpris_art};

    #[test]
    fn only_the_expected_source_writes() {
        // No stream: nothing is current.
        set_expected_mpris_art(None);
        assert!(!is_expected_mpris_art("https://i.ytimg.com/vi/a/maxresdefault.jpg"));

        // The current stream's art is expected...
        set_expected_mpris_art(Some("https://i.ytimg.com/vi/b/maxresdefault.jpg".to_owned()));
        assert!(is_expected_mpris_art("https://i.ytimg.com/vi/b/maxresdefault.jpg"));
        // ...a previous stream's art is stale and must be skipped.
        assert!(!is_expected_mpris_art("https://i.ytimg.com/vi/a/maxresdefault.jpg"));

        // Jellyfin streams are keyed by their item id.
        set_expected_mpris_art(Some("jellyfin:abcdef0123456789abcdef0123456789".to_owned()));
        assert!(is_expected_mpris_art("jellyfin:abcdef0123456789abcdef0123456789"));
        assert!(!is_expected_mpris_art("https://i.ytimg.com/vi/b/maxresdefault.jpg"));
    }

    // ── 2026-10-15: the MPRIS poster gets the same letterbox crop ────────

    fn letterboxed_jpeg() -> Vec<u8> {
        use image::{DynamicImage, Rgb, RgbImage};
        let mut img = RgbImage::new(480, 360);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = if y < 45 || y >= 315 {
                Rgb([0, 0, 0])
            } else {
                Rgb([(x % 251) as u8, (y % 241) as u8, 128])
            };
        }
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new(&mut out);
        enc.encode_image(&DynamicImage::ImageRgb8(img)).expect("encode");
        out
    }

    /// The poster bytes the media controls read are the **cropped** thumbnail,
    /// so the desktop art matches the pane.
    #[test]
    fn mpris_poster_bytes_are_the_cropped_thumbnail() {
        let raw = letterboxed_jpeg();
        let out = crate::shared::image::crop_letterbox_bytes(&raw, true);
        let img = image::load_from_memory(&out).expect("decodes");
        let aspect = img.width() as f64 / img.height() as f64;
        assert!(
            aspect > 1.6,
            "the bars are gone, so the poster is ~16:9 (got {aspect:.3})"
        );
    }

    /// The option governs this surface too: off means the bytes are written
    /// through untouched.
    #[test]
    fn mpris_poster_crop_follows_the_option() {
        let raw = letterboxed_jpeg();
        assert_eq!(
            crate::shared::image::crop_letterbox_bytes(&raw, false),
            raw,
            "off leaves the poster bytes alone"
        );
    }

    /// Bar-free art (a real poster, `maxresdefault`) is written through
    /// unchanged: the crop must never damage genuine artwork.
    #[test]
    fn mpris_poster_leaves_bar_free_art_unchanged() {
        use image::{DynamicImage, Rgb, RgbImage};
        let mut img = RgbImage::new(1280, 720);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8]);
        }
        let mut raw = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new(&mut raw);
        enc.encode_image(&DynamicImage::ImageRgb8(img)).expect("encode");
        assert_eq!(crate::shared::image::crop_letterbox_bytes(&raw, true), raw);
    }
}
