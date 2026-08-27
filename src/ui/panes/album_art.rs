use anyhow::Result;
use ratatui::{Frame, layout::Rect};
use super::Pane;
use crate::{
    MpdQueryResult, config::{album_art::ImageMethod, tabs::PaneType},
    ctx::Ctx, mpd::commands::{Song, State}, mpd::mpd_client::MpdClient,
    shared::{events::WorkRequest, keys::ActionEvent},
    ui::{UiEvent, image::facade::AlbumArtFacade},
};
#[derive(Debug)]
pub struct AlbumArtPane {
    album_art: AlbumArtFacade,
    is_modal_open: bool,
    fetch_needed: bool,
    /// The file the paused/stopped art box fetched last (selection-driven;
    /// avoids refetching every frame).
    paused_art_file: Option<String>,
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
        Self {
            album_art: AlbumArtFacade::new(ctx),
            is_modal_open: false,
            fetch_needed: false,
            paused_art_file: None,
        }
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
            .send(WorkRequest::FetchYtThumbnail {
                url: thumbnail,
            })
            .map_err(|err| {
                log::error!(error:? = err; "Failed to request youtube thumbnail")
            });
    }
    /// Collapse the pane entirely: there is no art to display, so the box
    /// hides (Round 48 — replaces the old default-placeholder fallbacks).
    /// The pane returns when art becomes available again (a fetch completes
    /// with data, the next song, resume).
    fn collapse(&mut self, ctx: &Ctx) -> Result<()> {
        ctx.album_art_collapsed.set(true);
        self.album_art.hide(ctx)
    }
    /// Show the already-known current art, or try to fetch it; collapse
    /// only when there is genuinely nothing to display or fetch. Host fix
    /// 2026-08-27: the round-48 version bailed out while collapsed, so the
    /// pane could never re-arm itself (see on_query_finished / on_event).
    fn show_current_or_collapse(&mut self, ctx: &Ctx) -> Result<()> {
        if ctx.status.state != State::Play {
            return self.check_selected_art(ctx);
        }
        if self.album_art.has_current() {
            ctx.album_art_collapsed.set(false);
            self.album_art.show_current(ctx)
        } else if AlbumArtPane::fetch_album_art(ctx).is_none() {
            self.collapse(ctx)
        } else {
            Ok(())
        }
    }
    /// returns none if album art is supposed to be hidden
    fn fetch_album_art(ctx: &Ctx) -> Option<()> {
        if matches!(ctx.config.album_art.method, ImageMethod::None) {
            return None;
        }
        let (_, current_song) = ctx.find_current_song_in_queue()?;
        Self::fetch_art_query(ctx, current_song.file.clone())
    }
    /// Dispatch an album-art query for `song_uri` (replace_id dedupes
    /// overlapping fetches). Returns None when art is disabled for the
    /// method/protocol — the caller collapses the box in that case.
    fn fetch_art_query(ctx: &Ctx, song_uri: String) -> Option<()> {
        if matches!(ctx.config.album_art.method, ImageMethod::None) {
            return None;
        }
        if ctx
            .config
            .album_art
            .disabled_protocols
            .iter()
            .any(|proto| song_uri.starts_with(proto))
        {
            log::debug!(
                uri = song_uri.as_str();
                "Not downloading album art because the protocol is disabled"
            );
            return None;
        }
        let order = ctx.config.album_art.order;
        ctx.query()
            .id(ALBUM_ART)
            .replace_id(ALBUM_ART)
            .target(PaneType::AlbumArt)
            .query(move |client| {
                let start = std::time::Instant::now();
                log::debug!(file = song_uri.as_str(); "Searching for album art");
                let result = client.find_album_art(&song_uri, order)?;
                log::debug!(
                    elapsed:? = start.elapsed(), size = result.as_ref().map(| v | v
                    .len()); "Found album art"
                );
                Ok(MpdQueryResult::AlbumArt(result))
            });
        Some(())
    }
    /// The song currently selected in the queue list. The queue pane syncs
    /// its id into `ctx.queue_selected_id` on every render (same channel
    /// the lyrics pane uses).
    fn selected_queue_song(ctx: &Ctx) -> Option<&Song> {
        ctx.queue_selected_id
            .get()
            .and_then(|id| ctx.queue.iter().find(|song| song.id == id))
    }
    /// Paused/stopped: the box follows the queue selection. Refetches only
    /// when the selection's file changes; a cleared selection collapses.
    fn check_selected_art(&mut self, ctx: &Ctx) -> Result<()> {
        let file = Self::selected_queue_song(ctx).map(|song| song.file.clone());
        if file == self.paused_art_file {
            return Ok(());
        }
        self.paused_art_file = file.clone();
        match file {
            Some(uri) => {
                if Self::fetch_art_query(ctx, uri).is_none() {
                    self.collapse(ctx)?;
                }
                Ok(())
            }
            None => self.collapse(ctx),
        }
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
        // Round 48 add-on: while paused/stopped the box follows the queue
        // selection (refetch only when the selection's file changed).
        if ctx.status.state != State::Play {
            self.check_selected_art(ctx)?;
        }
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
        if self.is_modal_open || ctx.album_art_collapsed.get() {
            return Ok(());
        }
        self.album_art.set_size(area);
        self.album_art.show_current(ctx)
    }
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        if self.is_modal_open {
            return Ok(());
        }
        // Paused/stopped: the art box follows the queue selection (checked
        // every frame in flush_pending_display); the playing-song / video
        // paths below apply only while actually playing.
        if ctx.status.state != State::Play {
            return self.check_selected_art(ctx);
        }
        if crate::core::mpv::mpv_is_ui_source(ctx) {
            if let Some(item_id) = ctx.mpv.item_id.as_deref() {
                let _ = ctx
                    .work_sender
                    .send(WorkRequest::FetchJellyfinVideoArt {
                        item_id: item_id.to_owned(),
                    })
                    .map_err(|err| {
                        log::error!(error:? = err; "Failed to request video art")
                    });
                return Ok(());
            }
            if let Some(yt) = crate::ui::modals::paste::mpv_yt_info(ctx) {
                match yt.thumbnail {
                    Some(thumbnail) => Self::fetch_yt_thumbnail(ctx, thumbnail),
                    None => self.collapse(ctx)?,
                }
                return Ok(());
            }
            self.collapse(ctx)?;
            return Ok(());
        }
        if let Some(yt) = Self::current_yt_info(ctx) {
            match yt.thumbnail {
                Some(thumbnail) => Self::fetch_yt_thumbnail(ctx, thumbnail),
                None => self.collapse(ctx)?,
            }
            return Ok(());
        }
        if AlbumArtPane::fetch_album_art(ctx).is_none() {
            self.collapse(ctx)?;
        }
        Ok(())
    }
    fn on_query_finished(
        &mut self,
        id: &'static str,
        data: MpdQueryResult,
        _is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        // Host fix 2026-08-27: a collapsed pane is "hidden" (is_pane_hidden
        // consults album_art_collapsed), which made every arriving art
        // result drop out here and the collapse become permanent. Results
        // must be processed regardless of visibility (the modal is the only
        // reason to defer).
        if self.is_modal_open {
            return Ok(());
        }
        match (id, data) {
            (ALBUM_ART, MpdQueryResult::AlbumArt(Some(data))) => {
                ctx.album_art_collapsed.set(false);
                self.album_art.show(data, ctx)?;
            }
            (ALBUM_ART, MpdQueryResult::AlbumArt(None)) => {
                self.collapse(ctx)?;
            }
            (YT_THUMBNAIL, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<Result<Vec<u8>, String>>() {
                    match boxed.as_ref() {
                        Ok(bytes) if !bytes.is_empty() => {
                            ctx.album_art_collapsed.set(false);
                            self.album_art.show(bytes.clone(), ctx)?;
                        }
                        Ok(_) => self.collapse(ctx)?,
                        Err(err) => {
                            log::debug!(
                                error:? = err; "Failed to fetch youtube thumbnail"
                            );
                            self.collapse(ctx)?;
                        }
                    }
                }
            }
            (JF_VIDEO_ART, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<crate::jellyfin::JellyfinResult>()
                    && let crate::jellyfin::JellyfinResult::Image { item_id, bytes } = boxed
                        .as_ref() && !bytes.is_empty()
                    && crate::core::mpv::mpv_is_ui_source(ctx)
                    && ctx.mpv.item_id.as_deref() == Some(item_id.as_str())
                {
                    ctx.album_art_collapsed.set(false);
                    self.album_art.show(bytes.clone(), ctx)?;
                } else {
                    self.collapse(ctx)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn on_event(
        &mut self,
        event: &mut UiEvent,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match event {
            UiEvent::SongChanged | UiEvent::Reconnected => {
                if self.is_modal_open {
                    self.fetch_needed = true;
                    return Ok(());
                }
                self.before_show(ctx)?;
            }
            // Round 48: a pause/resume transition re-asserts the current
            // track's art (SongChanged only fires when the song id
            // changes). Runs before_show again; a paused box that has no
            // art collapses instead of showing stale art.
            UiEvent::PlaybackStateChanged => {
                if self.is_modal_open {
                    self.fetch_needed = true;
                    return Ok(());
                }
                self.before_show(ctx)?;
            }
            UiEvent::Displayed => {
                if !self.is_modal_open {
                    self.show_current_or_collapse(ctx)?;
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
                self.show_current_or_collapse(ctx)?;
            }
            UiEvent::ConfigChanged => {
                if is_visible && !self.is_modal_open {
                    self.show_current_or_collapse(ctx)?;
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
