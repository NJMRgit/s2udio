use anyhow::Result;
use ratatui::{Frame, layout::Rect};

use super::Pane;
use crate::{
    MpdQueryResult,
    config::{album_art::ImageMethod, tabs::PaneType},
    ctx::Ctx,
    mpd::mpd_client::MpdClient,
    shared::{events::WorkRequest, keys::ActionEvent},
    ui::{UiEvent, image::facade::AlbumArtFacade},
};

#[derive(Debug)]
pub struct AlbumArtPane {
    album_art: AlbumArtFacade,
    is_modal_open: bool,
    fetch_needed: bool,
}

const ALBUM_ART: &str = "album_art";
/// Result id of a YouTube thumbnail download (shown as album art while the
/// video's audio stream plays).
pub const YT_THUMBNAIL: &str = "yt_thumbnail";
/// Result id of the Jellyfin primary image of the video playing in mpv
/// (shown as album art while the video plays).
pub const JF_VIDEO_ART: &str = "jellyfin_video_art";

impl AlbumArtPane {
    pub fn new(ctx: &Ctx) -> Self {
        Self { album_art: AlbumArtFacade::new(ctx), is_modal_open: false, fetch_needed: false }
    }

    /// The YouTube info of the current song when it is a resolved
    /// `YouTube`-style audio stream, matched by its stream URL or the
    /// original link (the shared lookup also handles the mpv video case,
    /// a no-op on the audio branch that calls this).
    fn current_yt_info(ctx: &Ctx) -> Option<crate::shared::ytdlp::YtStreamInfo> {
        crate::ui::modals::paste::current_yt_info(ctx)
    }

    /// Download and show the current YouTube video's thumbnail.
    fn fetch_yt_thumbnail(ctx: &Ctx, thumbnail: String) {
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchYtThumbnail { url: thumbnail })
            .map_err(|err| log::error!(error:? = err; "Failed to request youtube thumbnail"));
    }

    /// returns none if album art is supposed to be hidden
    fn fetch_album_art(ctx: &Ctx) -> Option<()> {
        if matches!(ctx.config.album_art.method, ImageMethod::None) {
            return None;
        }

        let (_, current_song) = ctx.find_current_song_in_queue()?;

        let disabled_protos = &ctx.config.album_art.disabled_protocols;
        let song_uri = current_song.file.as_str();
        if disabled_protos.iter().any(|proto| song_uri.starts_with(proto)) {
            log::debug!(uri = song_uri; "Not downloading album art because the protocol is disabled");
            return None;
        }

        let song_uri = song_uri.to_owned();
        let order = ctx.config.album_art.order;
        ctx.query().id(ALBUM_ART).replace_id(ALBUM_ART).target(PaneType::AlbumArt).query(move |client| {
            let start = std::time::Instant::now();
            log::debug!(file = song_uri.as_str(); "Searching for album art");
            let result = client.find_album_art(&song_uri, order)?;
            log::debug!(elapsed:? = start.elapsed(), size = result.as_ref().map(|v|v.len()); "Found album art");

            Ok(MpdQueryResult::AlbumArt(result))
        });

        Some(())
    }
    /// Draw the encoded image queued by the last `UiEvent::ImageEncoded`
    /// after the frame's buffer flush (the flush would otherwise overwrite
    /// the kitty placeholder cells and delete the transient image — the
    /// broken art left behind after a vertical resize), or heal the last
    /// drawn image when the frame's diff rewrote the art pane area.
    pub(crate) fn flush_pending_display(
        &mut self,
        buffer: &ratatui::buffer::Buffer,
        ctx: &Ctx,
    ) -> Result<()> {
        self.album_art.frame_rendered(buffer);
        self.album_art.flush_display(ctx)
    }
}

impl Pane for AlbumArtPane {
    fn render(&mut self, _frame: &mut Frame, area: Rect, _ctx: &Ctx) -> Result<()> {
        self.album_art.set_size(area);
        Ok(())
    }

    fn calculate_areas(&mut self, area: Rect, _ctx: &Ctx) -> Result<()> {
        self.album_art.set_size(area);
        Ok(())
    }

    fn handle_action(&mut self, _event: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }

    fn on_hide(&mut self, ctx: &Ctx) -> Result<()> {
        self.album_art.hide(ctx)
    }

    fn resize(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        if self.is_modal_open {
            return Ok(());
        }
        self.album_art.set_size(area);
        self.album_art.show_current(ctx)
    }

    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        // Never repaint the art while a modal (Settings, …) is open: the
        // image is a terminal-side overlay and would draw over the modal's
        // full-window view. The ModalClosed event re-shows it.
        if self.is_modal_open {
            return Ok(());
        }
        // A video that is the UI source owns the album art box: its
        // Jellyfin image or resolved YouTube thumbnail, or the generic
        // default when no thumbnail exists — never the audio album art of
        // the paused song.
        if crate::core::mpv::mpv_is_ui_source(ctx) {
            // Jellyfin item: fetch its primary image.
            if let Some(item_id) = ctx.mpv.item_id.as_deref() {
                let _ = ctx
                    .work_sender
                    .send(WorkRequest::FetchJellyfinVideoArt { item_id: item_id.to_owned() })
                    .map_err(|err| log::error!(error:? = err; "Failed to request video art"));
                return Ok(());
            }
            // YouTube-style video: its resolved thumbnail is the album art.
            if let Some(yt) = crate::ui::modals::paste::mpv_yt_info(ctx) {
                match yt.thumbnail {
                    Some(thumbnail) => Self::fetch_yt_thumbnail(ctx, thumbnail),
                    // No thumbnail available: fall back to the generic
                    // image, not the audio album art.
                    None => self.album_art.show_default(ctx)?,
                }
                return Ok(());
            }
            // mpv active but no art source known (yet): the generic image
            // until the info arrives.
            self.album_art.show_default(ctx)?;
            return Ok(());
        }
        // A YouTube-style audio stream has no MPD album art; its video
        // thumbnail takes its place.
        if let Some(yt) = Self::current_yt_info(ctx) {
            match yt.thumbnail {
                Some(thumbnail) => Self::fetch_yt_thumbnail(ctx, thumbnail),
                None => self.album_art.show_default(ctx)?,
            }
            return Ok(());
        }
        if AlbumArtPane::fetch_album_art(ctx).is_none() {
            self.album_art.show_default(ctx)?;
        }
        Ok(())
    }

    fn on_query_finished(
        &mut self,
        id: &'static str,
        data: MpdQueryResult,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        if !is_visible || self.is_modal_open {
            return Ok(());
        }
        match (id, data) {
            (ALBUM_ART, MpdQueryResult::AlbumArt(Some(data))) => {
                self.album_art.show(data, ctx)?;
            }
            (ALBUM_ART, MpdQueryResult::AlbumArt(None)) => {
                self.album_art.show_default(ctx)?;
            }
            (YT_THUMBNAIL, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<Result<Vec<u8>, String>>() {
                    match boxed.as_ref() {
                        Ok(bytes) => self.album_art.show(bytes.clone(), ctx)?,
                        Err(err) => {
                            log::debug!(error:? = err; "Failed to fetch youtube thumbnail");
                            self.album_art.show_default(ctx)?;
                        }
                    }
                }
            }
            (JF_VIDEO_ART, MpdQueryResult::Any(any)) => {
                // The primary image of the video playing in mpv, shown as
                // album art. Stale results (the video advanced / ended) are
                // dropped.
                if let Ok(boxed) = any.downcast::<crate::jellyfin::JellyfinResult>()
                    && let crate::jellyfin::JellyfinResult::Image { item_id, bytes } =
                        boxed.as_ref()
                    && !bytes.is_empty()
                    && crate::core::mpv::mpv_is_ui_source(ctx)
                    && ctx.mpv.item_id.as_deref() == Some(item_id.as_str())
                {
                    self.album_art.show(bytes.clone(), ctx)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::SongChanged | UiEvent::Reconnected if is_visible => {
                if self.is_modal_open {
                    self.fetch_needed = true;
                    return Ok(());
                }
                self.before_show(ctx)?;
            }
            UiEvent::Displayed if is_visible => {
                if is_visible && !self.is_modal_open {
                    self.album_art.show_current(ctx)?;
                }
            }
            UiEvent::ModalOpened if is_visible => {
                if !self.is_modal_open {
                    self.album_art.hide(ctx)?;
                }
                self.is_modal_open = true;
            }
            UiEvent::ModalClosed if is_visible => {
                self.is_modal_open = false;

                if self.fetch_needed {
                    self.fetch_needed = false;
                    self.before_show(ctx)?;
                    return Ok(());
                }
                self.album_art.show_current(ctx)?;
            }
            UiEvent::ConfigChanged => {
                if is_visible && !self.is_modal_open {
                    self.album_art.show_current(ctx)?;
                }
            }
            UiEvent::Exit => {
                self.album_art.cleanup()?;
            }
            UiEvent::ImageEncoded { data } => {
                self.album_art.display(std::mem::take(data), ctx)?;
            }
            UiEvent::ImageEncodeFailed { err } => {
                self.album_art.image_processing_failed(err, ctx)?;
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use crossbeam::channel::{Receiver, RecvTimeoutError, Sender};
    use rstest::rstest;

    use super::AlbumArtPane;
    use crate::{
        MpdQueryResult,
        config::{Config, album_art::ImageMethod, tabs::PaneType},
        mpd::commands::{Song, State},
        shared::{
            events::{AppEvent, ClientRequest, WorkRequest},
            mpd_query::MpdQuery,
        },
        tests::fixtures::{app_event_channel, client_request_channel, ctx, work_request_channel},
        ui::{
            UiEvent,
            panes::{
                Pane,
                album_art::{ALBUM_ART, JF_VIDEO_ART},
            },
        },
    };

    #[rstest]
    #[case(ImageMethod::Kitty, true)]
    #[case(ImageMethod::None, false)]
    fn searches_for_album_art_before_show(
        #[case] method: ImageMethod,
        #[case] should_search: bool,
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        let rx = client_request_channel.1.clone();
        let mut ctx = ctx(app_event_channel, work_request_channel, client_request_channel);
        let selected_song_id = 333;
        let mut config = Config::default();
        config.album_art.method = method;
        ctx.config = std::sync::Arc::new(config);
        ctx.queue.push(Song { id: selected_song_id, ..Default::default() });
        ctx.status.songid = Some(selected_song_id);
        ctx.status.state = State::Play;
        let mut screen = AlbumArtPane::new(&ctx);

        screen.before_show(&ctx).unwrap();

        if should_search {
            assert!(matches!(
                rx.recv_timeout(Duration::from_millis(100)).unwrap(),
                ClientRequest::Query(MpdQuery {
                    id: ALBUM_ART,
                    replace_id: Some(ALBUM_ART),
                    target: Some(PaneType::AlbumArt),
                    ..
                })
            ));
        } else {
            assert!(
                rx.recv_timeout(Duration::from_millis(100))
                    .is_err_and(|err| RecvTimeoutError::Timeout == err)
            );
        }
    }

    #[rstest]
    #[case(ImageMethod::Kitty, true)]
    #[case(ImageMethod::None, false)]
    fn searches_for_album_art_on_event(
        #[case] method: ImageMethod,
        #[case] should_search: bool,
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        let rx = client_request_channel.1.clone();
        let mut ctx = ctx(app_event_channel, work_request_channel, client_request_channel);
        let selected_song_id = 333;
        let mut config = Config::default();
        config.album_art.method = method;
        ctx.config = std::sync::Arc::new(config);
        ctx.queue.push(Song { id: selected_song_id, ..Default::default() });
        ctx.status.songid = Some(selected_song_id);
        ctx.status.state = State::Play;
        let mut screen = AlbumArtPane::new(&ctx);

        screen.on_event(&mut UiEvent::SongChanged, true, &ctx).unwrap();

        if should_search {
            assert!(matches!(
                rx.recv_timeout(Duration::from_millis(100)).unwrap(),
                ClientRequest::Query(MpdQuery {
                    id: ALBUM_ART,
                    replace_id: Some(ALBUM_ART),
                    target: Some(PaneType::AlbumArt),
                    ..
                })
            ));
        } else {
            let result = rx.recv_timeout(Duration::from_millis(100));
            assert!(result.is_err_and(|err| RecvTimeoutError::Timeout == err));
        }
    }

    #[test]
    fn video_playing_requests_the_video_art_as_album_art() {
        use crate::core::mpv::MpvSession;
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let mut ctx = ctx(
            (app_tx, _app_rx),
            (work_tx.clone(), work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.mpv = MpvSession {
            active: true,
            item_id: Some("abcdef0123456789abcdef0123456789".to_owned()),
            ..Default::default()
        };
        let mut screen = AlbumArtPane::new(&ctx);
        screen.before_show(&ctx).unwrap();
        assert!(matches!(
            work_rx.try_recv(),
            Ok(WorkRequest::FetchJellyfinVideoArt { item_id })
                if item_id == "abcdef0123456789abcdef0123456789"
        ));
    }

    #[test]
    fn video_art_result_shows_when_the_item_is_current() {
        use crate::core::mpv::MpvSession;
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let mut ctx = ctx(
            (app_tx, _app_rx),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let item_id = "abcdef0123456789abcdef0123456789";
        ctx.mpv =
            MpvSession { active: true, item_id: Some(item_id.to_owned()), ..Default::default() };
        let mut screen = AlbumArtPane::new(&ctx);
        let bytes = vec![0u8; 64];
        let result = MpdQueryResult::Any(Box::new(crate::jellyfin::JellyfinResult::Image {
            item_id: item_id.to_owned(),
            bytes: bytes.clone(),
        }));
        // The current item's image is accepted: the facade queues an encode.
        screen.on_query_finished(JF_VIDEO_ART, result, true, &ctx).unwrap();
        assert!(matches!(work_rx.try_recv(), Ok(WorkRequest::ResizeImage(_))));
        // A stale item's image is dropped (no encode queued).
        ctx.mpv.item_id = Some("deadbeefdeadbeefdeadbeefdeadbeef".to_owned());
        let stale = MpdQueryResult::Any(Box::new(crate::jellyfin::JellyfinResult::Image {
            item_id: item_id.to_owned(),
            bytes,
        }));
        screen.on_query_finished(JF_VIDEO_ART, stale, true, &ctx).unwrap();
        assert!(matches!(work_rx.try_recv(), Err(crossbeam::channel::TryRecvError::Empty)));
    }
}
