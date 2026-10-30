use std::{io::Write, sync::Arc, time::Instant};

use crate::ui::anim::{Anim, Ease};
use anyhow::Result;
use crossbeam::channel::Sender;
use ratatui::{buffer::Buffer, layout::Rect, prelude::IntoCrossterm};
use super::{
    Backend, block::Block, iterm2::Iterm2, kitty::Kitty, sixel::Sixel,
    ueberzug::{Layer, Ueberzug},
};
use crate::{
    config::album_art::ImageMethod, ctx::Ctx,
    shared::{events::WorkRequest, macros::status_error, terminal::TERMINAL},
};
/// How long a freshly-shown picture takes to wipe in.
///
/// Linear on purpose: a wipe reads as a constant reveal, where an eased curve
/// looks like the picture hesitates. Not configurable yet — 180 ms is short
/// enough that it never delays the art, long enough to read as motion.
const ART_REVEAL: std::time::Duration = std::time::Duration::from_millis(180);

/// One encoded album-art frame plus the pane rect and encode epoch it was
/// made for. The epoch lets the facade drop a result that finished after the
/// pane moved (a resize while the encode was in flight): drawing it would
/// place the old-size image over the freshly resized UI.
#[derive(Debug, Default, Clone)]
pub struct EncodedImage {
    pub data: EncodeData,
    pub size: Rect,
    pub generation: u64,
}

#[derive(Debug)]
pub struct AlbumArtFacade {
    image_backend: ImageBackend,
    current_album_art: Option<Arc<Vec<u8>>>,
    last_size: Rect,
    work_tx: Sender<WorkRequest>,
    is_showing: bool,
    request_queue: Vec<Arc<Vec<u8>>>,
    /// Bumped whenever the pane geometry changes or the overlay is hidden.
    /// An encode sent before the bump belongs to the old geometry and must
    /// not be drawn, even if it finishes after the new frame is on screen.
    encode_generation: u64,
    /// Encoded image waiting to be drawn after the current frame's buffer
    /// flush (see [`Self::flush_display`]), with the size it was encoded for.
    pending_display: Option<EncodeData>,
    pending_size: Option<Rect>,
    /// The last successfully drawn encode: re-placed when a frame's diff
    /// rewrote its (transient) placeholder cells so the wipe heals within
    /// one frame (see [`Self::frame_rendered`]).
    last_drawn: Option<EncodeData>,
    /// The pane size [`Self::last_drawn`] was encoded for. A re-place at a
    /// different size would paint the old image over the new layout.
    last_drawn_size: Rect,
    /// Snapshot of the album-art pane area cells of the previous frame. A
    /// frame whose buffer differs here rewrote those cells, which deletes a
    /// transient kitty placement sitting on them.
    prev_art_region: Vec<ratatui::buffer::Cell>,
    /// The pane area [`Self::prev_art_region`] was snapshotted at; a change
    /// (resize / layout change) forces a heal instead of comparing across
    /// mismatched areas.
    prev_art_region_area: Rect,
    /// The last frame's diff rewrote cells inside the art pane area (the
    /// image may have been wiped and needs a re-place).
    region_dirty: bool,
    /// The reveal in flight for a freshly-shown picture (a wipe, one draw per
    /// frame). `None` once it finished or when the art just appeared.
    reveal: Option<Anim>,
    /// The next encode is a **new** picture (not the same art re-encoded for a
    /// new size), so it should wipe in.
    reveal_pending: bool,
}
#[derive(Debug, Default)]
enum ImageBackend {
    Kitty(Kitty),
    Ueberzug(Ueberzug),
    Iterm2(Iterm2),
    Sixel(Sixel),
    Block(Block),
    #[default]
    None,
}
#[derive(Debug, Default, Clone)]
pub enum EncodeData {
    Kitty(<Kitty as Backend>::EncodedData),
    Ueberzug(<Ueberzug as Backend>::EncodedData),
    Iterm2(<Iterm2 as Backend>::EncodedData),
    Sixel(<Sixel as Backend>::EncodedData),
    Block(<Block as Backend>::EncodedData),
    #[default]
    Empty,
}
/// What [`AlbumArtFacade::flush_display`] should do with the current state.
#[derive(Debug)]
enum FlushAction {
    Nothing,
    /// The data matches the current pane geometry: draw it.
    Draw(EncodeData),
    /// A pending encode was made for an older geometry.
    Reencode,
}
impl AlbumArtFacade {
    pub fn new(ctx: &Ctx) -> Self {
        let config = ctx.config.as_ref();
        let image_backend = match config.album_art.method {
            ImageMethod::Kitty => ImageBackend::Kitty(Kitty::new()),
            ImageMethod::UeberzugWayland => {
                ImageBackend::Ueberzug(Ueberzug::new(Layer::Wayland))
            }
            ImageMethod::UeberzugX11 => ImageBackend::Ueberzug(Ueberzug::new(Layer::X11)),
            ImageMethod::Iterm2 => ImageBackend::Iterm2(Iterm2),
            ImageMethod::Sixel => ImageBackend::Sixel(Sixel),
            ImageMethod::Block => ImageBackend::Block(Block),
            ImageMethod::None => ImageBackend::None,
        };
        Self {
            image_backend,
            current_album_art: None,
            last_size: Rect::default(),
            work_tx: ctx.work_sender.clone(),
            is_showing: false,
            request_queue: Vec::new(),
            encode_generation: 0,
            pending_display: None,
            pending_size: None,
            last_drawn: None,
            last_drawn_size: Rect::default(),
            prev_art_region: Vec::new(),
            prev_art_region_area: Rect::default(),
            region_dirty: false,
            reveal: None,
            reveal_pending: false,
        }
    }
    /// Whether real art was last shown. The album art pane uses it to
    /// decide whether the pane collapses (Round 48: no art -> no box).
    pub fn has_current(&self) -> bool {
        self.current_album_art.is_some()
    }
    /// Whether an image is currently active on screen (drives the
    /// off-tab hide: only an erase that really writes cells needs a
    /// full-refresh repair afterwards).
    pub(crate) fn is_showing(&self) -> bool {
        self.is_showing
    }
    pub fn show_current(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(current_album_art) = self.current_album_art.as_ref().map(Arc::clone)
        else {
            log::warn!("Tried to display current album art but none was present");
            return Ok(());
        };
        self.show(current_album_art, ctx)?;
        return Ok(());
    }
    /// Show a **YouTube stream thumbnail**, trimming its letterbox bars first.
    ///
    /// `hqdefault.jpg` is 480x360 — a 16:9 picture with 43-45 px of solid bar
    /// top and bottom — so in a 16:9 art box it renders with visible bars.
    /// The crop happens here, on the **bytes**, before they become
    /// `current_album_art`: a later resize re-place then re-encodes the
    /// already-cropped image instead of cropping a cropped image.
    ///
    /// Only this path crops. Embedded covers, the cover search, the Jellyfin
    /// poster and radio icons keep the plain [`Self::show`], because they are
    /// not letterboxed stream thumbnails.
    pub fn show_stream_thumbnail(
        &mut self,
        data: impl Into<Arc<Vec<u8>>>,
        ctx: &Ctx,
    ) -> Result<()> {
        let data = data.into();
        let cropped =
            crate::shared::image::crop_letterbox_bytes(&data, ctx.config.album_art.crop_letterbox);
        if cropped == *data {
            return self.show(data, ctx);
        }
        self.show(Arc::new(cropped), ctx)
    }
    pub fn show(&mut self, data: impl Into<Arc<Vec<u8>>>, ctx: &Ctx) -> Result<()> {
        self.is_showing = true;
        let max_size = ctx.config.album_art.max_size_px;
        let halign = ctx.config.album_art.horizontal_align;
        let valign = ctx.config.album_art.vertical_align;
        let size = self.last_size;
        let generation = self.encode_generation;
        let data = data.into();
        // Same `Arc` = the same picture re-shown (a resize re-encode, a pane
        // toggled back on): it must not wipe again. A new `Arc` is new art.
        self.reveal_pending = self
            .current_album_art
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, &data));
        self.current_album_art = Some(Arc::clone(&data));
        self.request_queue.push(Arc::clone(&data));
        if self.request_queue.len() > 1 {
            log::debug!("Image encode request already in flight, queueing the new one.");
            return Ok(());
        }
        match &mut self.image_backend {
            ImageBackend::Kitty(_kitty) => {
                self.work_tx
                    .send(
                        WorkRequest::ResizeImage(
                            Box::new(move || {
                                Ok(EncodedImage {
                                    data: EncodeData::Kitty(
                                        Kitty::create_data(&data, size, max_size, halign, valign)?,
                                    ),
                                    size,
                                    generation,
                                })
                            }),
                        ),
                    )?;
            }
            ImageBackend::Iterm2(_iterm2) => {
                self.work_tx
                    .send(
                        WorkRequest::ResizeImage(
                            Box::new(move || {
                                Ok(EncodedImage {
                                    data: EncodeData::Iterm2(
                                        Iterm2::create_data(&data, size, max_size, halign, valign)?,
                                    ),
                                    size,
                                    generation,
                                })
                            }),
                        ),
                    )?;
            }
            ImageBackend::Sixel(_sixel) => {
                log::debug!("Sending sixel image encode request");
                self.work_tx
                    .send(
                        WorkRequest::ResizeImage(
                            Box::new(move || {
                                Ok(EncodedImage {
                                    data: EncodeData::Sixel(
                                        Sixel::create_data(&data, size, max_size, halign, valign)?,
                                    ),
                                    size,
                                    generation,
                                })
                            }),
                        ),
                    )?;
            }
            ImageBackend::Block(_block) => {
                self.work_tx
                    .send(
                        WorkRequest::ResizeImage(
                            Box::new(move || {
                                Ok(EncodedImage {
                                    data: EncodeData::Block(
                                        Block::create_data(&data, size, max_size, halign, valign)?,
                                    ),
                                    size,
                                    generation,
                                })
                            }),
                        ),
                    )?;
            }
            ImageBackend::Ueberzug(_ueberzug) => {
                self.work_tx
                    .send(
                        WorkRequest::ResizeImage(
                            Box::new(move || {
                                Ok(EncodedImage {
                                    data: EncodeData::Ueberzug(
                                        Ueberzug::create_data(
                                            &data,
                                            size,
                                            max_size,
                                            halign,
                                            valign,
                                        )?,
                                    ),
                                    size,
                                    generation,
                                })
                            }),
                        ),
                    )?;
            }
            ImageBackend::None => {}
        }
        Ok(())
    }
    pub fn image_processing_failed(
        &mut self,
        err: &anyhow::Error,
        ctx: &Ctx,
    ) -> Result<()> {
        status_error!("Failed to process album art image: {err:?}");
        if let Some(req_data) = self.request_queue.pop()
            && !self.request_queue.is_empty()
        {
            log::debug!("More image requests in queue, encoding the latest one instead");
            self.request_queue.clear();
            self.show(req_data, ctx)?;
        }
        Ok(())
    }
    /// Accept an encoded frame. A frame from an older generation (the pane
    /// moved while it was being encoded) or made for a different rect is
    /// dropped, never drawn: on a resize the old-size image must not land on
    /// the new UI. The encode coalescing queue then asks for the current
    /// geometry if nothing is in flight for it.
    pub fn display(&mut self, image: EncodedImage, ctx: &Ctx) -> Result<()> {
        if !self.is_showing {
            log::trace!(
                "Not showing image because its not supposed to be displayed anymore"
            );
            self.request_queue.clear();
            return Ok(());
        }
        if image.generation != self.encode_generation {
            log::debug!(
                encoded_for:? = image.size,
                current:? = self.last_size,
                encoded_generation = image.generation,
                current_generation = self.encode_generation;
                "Dropping album-art encode from before a resize/hide"
            );
            // The stale request occupied the work thread, so a newer request
            // may never have been sent. Send one for the current geometry.
            if self.request_queue.is_empty()
                && let Some(art) = self.current_album_art.clone()
            {
                self.show(art, ctx)?;
            }
            return Ok(());
        }
        if let Some(req_data) = self.request_queue.pop()
            && !self.request_queue.is_empty()
        {
            log::debug!("More image requests in queue, encoding the latest one instead");
            self.request_queue.clear();
            self.show(req_data, ctx)?;
            return Ok(());
        }
        if image.size != self.last_size {
            log::debug!(
                encoded_for:? = image.size, current:? = self.last_size;
                "Dropping stale album-art encode, re-encoding at the current size"
            );
            if let Some(art) = self.current_album_art.clone() {
                self.show(art, ctx)?;
            }
            return Ok(());
        }
        log::debug!(data:? = image.data, area:? = image.size; "Received encoded data",);
        self.pending_display = Some(image.data);
        self.pending_size = Some(image.size);
        Ok(())
    }
    /// Record the frame that was just flushed so the next
    /// [`Self::flush_display`] re-places the image only when the frame's
    /// diff actually rewrote cells in the art pane area (a transient kitty
    /// placement is deleted when any cell it occupies is rewritten). Called
    /// after every `terminal.draw`, before [`Self::flush_display`].
    pub fn frame_rendered(&mut self, buffer: &Buffer) {
        let area = self.last_size;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let snapshot = |buffer: &Buffer, area: Rect| {
            let mut region = Vec::with_capacity(
                usize::from(area.width) * usize::from(area.height),
            );
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    region.push(buffer.cell((x, y)).cloned().unwrap_or_default());
                }
            }
            region
        };
        if self.prev_art_region_area != area {
            self.region_dirty = true;
            self.prev_art_region = snapshot(buffer, area);
            self.prev_art_region_area = area;
            return;
        }
        let region = snapshot(buffer, area);
        if region != self.prev_art_region {
            self.region_dirty = true;
        }
        self.prev_art_region = region;
    }
    /// What a display flush should do with the current state.
    fn flush_action(&mut self) -> FlushAction {
        if let Some(data) = self.pending_display.take() {
            let size = self.pending_size.take();
            if !self.is_showing {
                return FlushAction::Nothing;
            }
            if size != Some(self.last_size) {
                log::debug!(
                    encoded_for:? = size, current:? = self.last_size;
                    "Dropping stale album-art encode, re-encoding at the current size"
                );
                return FlushAction::Reencode;
            }
            return FlushAction::Draw(data);
        }
        // A reveal re-draws every frame whether or not the frame's diff
        // touched the art cells: the wipe IS the drawing.
        if self.reveal.is_some()
            && self.is_showing
            && self.last_drawn_size == self.last_size
            && let Some(data) = &self.last_drawn
        {
            return FlushAction::Draw(data.clone());
        }
        match &self.last_drawn {
            Some(data) if self.is_showing && self.region_dirty
                && self.last_drawn_size == self.last_size => FlushAction::Draw(data.clone()),
            _ => FlushAction::Nothing,
        }
    }
    /// Draw the encoded image queued by [`Self::display`], or re-place the
    /// last drawn one. Called by the event loop after every frame's buffer
    /// flush. A pending encode made for another pane size is dropped and
    /// re-encoded at the current size, and a drawn image is only re-placed
    /// when it was drawn at the current size, so a resize can never put the
    /// old image over the new UI. The last drawn image is re-placed only when
    /// [`Self::frame_rendered`] saw the frame rewrite the art pane area
    /// (re-placing after *every* frame would strobe the art while playing,
    /// since status updates / mpv polls render many frames a second).
    pub fn flush_display(&mut self, ctx: &Ctx) -> Result<()> {
        let data = match self.flush_action() {
            FlushAction::Nothing => return Ok(()),
            FlushAction::Reencode => {
                if let Some(art) = self.current_album_art.clone() {
                    self.show(art, ctx)?;
                }
                return Ok(());
            }
            FlushAction::Draw(data) => data,
        };
        let now = Instant::now();
        // A brand-new picture wipes in; a re-place of the same payload (the
        // frame repainted the art cells, the pane moved) just redraws.
        if self.reveal_pending {
            self.reveal_pending = false;
            self.reveal = Some(Anim::new(now, ART_REVEAL, Ease::Linear));
        }
        let reveal = self.reveal.map(|anim| anim.sample(now));
        let w = TERMINAL.writer();
        let mut w = w.lock();
        let w = w.by_ref();
        let c = ctx.config.theme.background_color.map(|c| c.into_crossterm());
        let drawn = data.clone();
        // `replace` retires the previous drawing and draws this one. The kitty
        // backend recognizes "same payload, same box" and skips the transfer,
        // which is what makes a per-frame reveal affordable; every other
        // backend falls back to hide + display (and ignores the fraction).
        let result = match (&mut self.image_backend, data) {
            (ImageBackend::Kitty(kitty), EncodeData::Kitty(data)) => {
                kitty.replace(w, self.last_size, c, data, ctx, reveal)
            }
            (ImageBackend::Ueberzug(ueberzug), EncodeData::Ueberzug(data)) => {
                ueberzug.replace(w, self.last_size, c, data, ctx, reveal)
            }
            (ImageBackend::Iterm2(iterm2), EncodeData::Iterm2(data)) => {
                iterm2.replace(w, self.last_size, c, data, ctx, reveal)
            }
            (ImageBackend::Sixel(sixel), EncodeData::Sixel(data)) => {
                sixel.replace(w, self.last_size, c, data, ctx, reveal)
            }
            (ImageBackend::Block(block), EncodeData::Block(data)) => {
                block.replace(w, self.last_size, c, data, ctx, reveal)
            }
            (ImageBackend::None, EncodeData::Empty) => {
                log::warn!("Tried to display image but no backend is selected");
                Ok(())
            }
            _ => {
                status_error!(
                    "Received encoded data for a different backend than the one in use. Please report this."
                );
                Ok(())
            }
        };
        if let Err(err) = result {
            status_error!("Failed to display image {err:#}");
            self.reveal = None;
        } else {
            self.last_drawn = Some(drawn);
            self.last_drawn_size = self.last_size;
            self.region_dirty = false;
            // The art is a terminal-side overlay: declare the rect so a clear
            // and any "what is under this point" question can find it.
            ctx.placements
                .borrow_mut()
                .declare(crate::ui::image::placements::Surface::AlbumArt, self.last_size);
            match self.reveal {
                Some(anim) if anim.is_done(now) => self.reveal = None,
                // Still wiping: the next frame is what advances it.
                Some(_) => ctx.animator.request_frame(),
                None => {}
            }
        }
        Ok(())
    }
    /// Drop the held image data without touching the terminal: the source
    /// resolved to "nothing to show", so a later re-show must not re-place
    /// a stale image. Callers erase separately through [`Self::hide`].
    pub fn forget(&mut self) {
        self.reveal = None;
        self.reveal_pending = false;
        self.current_album_art = None;
        self.request_queue.clear();
        self.encode_generation = self.encode_generation.wrapping_add(1);
        self.pending_display = None;
        self.pending_size = None;
        self.last_drawn = None;
        self.region_dirty = false;
    }
    pub fn hide(&mut self, ctx: &Ctx) -> Result<()> {
        ctx.placements
            .borrow_mut()
            .release(crate::ui::image::placements::Surface::AlbumArt);
        // An encode that is still in flight was made for a geometry that is
        // no longer on screen: its result must not be drawn when it lands.
        self.encode_generation = self.encode_generation.wrapping_add(1);
        self.pending_display = None;
        self.pending_size = None;
        self.last_drawn = None;
        self.region_dirty = false;
        self.reveal = None;
        self.reveal_pending = false;
        self.request_queue.clear();
        if !self.is_showing {
            // Nothing is on screen: skip the backend erase. The erase
            // (clear_area) blanks the stale art region with cells the
            // frame diff will not repaint, so repeated no-art collapses
            // left a permanent blank box where the art used to be. Host
            // fix 2026-08-27.
            return Ok(());
        }
        self.is_showing = false;
        let w = TERMINAL.writer();
        let mut w = w.lock();
        let w = w.by_ref();
        let c = ctx.config.theme.background_color.map(|c| c.into_crossterm());
        match &mut self.image_backend {
            ImageBackend::Kitty(s) => s.hide(w, self.last_size, c)?,
            ImageBackend::Ueberzug(s) => s.hide(w, self.last_size, c)?,
            ImageBackend::Iterm2(s) => s.hide(w, self.last_size, c)?,
            ImageBackend::Sixel(s) => s.hide(w, self.last_size, c)?,
            ImageBackend::Block(s) => s.hide(w, self.last_size, c)?,
            ImageBackend::None => {}
        }
        Ok(())
    }
    pub fn cleanup(&mut self) -> Result<()> {
        let state = std::mem::take(&mut self.image_backend);
        self.is_showing = false;
        match state {
            ImageBackend::Kitty(kitty) => Box::new(kitty).cleanup(self.last_size),
            ImageBackend::Ueberzug(ueberzug) => {
                Box::new(ueberzug).cleanup(self.last_size)
            }
            ImageBackend::Iterm2(iterm2) => Box::new(iterm2).cleanup(self.last_size),
            ImageBackend::Sixel(s) => Box::new(s).cleanup(self.last_size),
            ImageBackend::Block(s) => Box::new(s).cleanup(self.last_size),
            ImageBackend::None => Ok(()),
        }
    }
    pub fn set_size(&mut self, area: Rect) {
        if self.last_size != area {
            // The pane geometry changed (resize / layout change). Anything
            // encoded for the old rect is now stale: never re-place it at
            // the new one. A fresh encode at the new size is the only image
            // that may be drawn (2026-10-28 resize regression).
            self.encode_generation = self.encode_generation.wrapping_add(1);
            self.pending_display = None;
            self.pending_size = None;
            self.last_drawn = None;
            self.last_drawn_size = Rect::default();
            self.request_queue.clear();
            // The frame at the new rect must heal after the old placement
            // was deleted, so force the next snapshot to be dirty.
            self.region_dirty = true;
            self.prev_art_region_area = Rect::default();
            self.prev_art_region.clear();
        }
        self.last_size = area;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crossbeam::channel::{Receiver, Sender};
    use rstest::rstest;

    use super::{AlbumArtFacade, EncodeData, EncodedImage, FlushAction};
    use crate::{
        shared::events::{AppEvent, ClientRequest, WorkRequest},
        tests::fixtures::{app_event_channel, client_request_channel, ctx, work_request_channel},
    };

    /// The self-heal must only trigger when a frame's diff actually rewrote
    /// the art pane area: re-placing after every frame made the art strobe
    /// while playing (status updates render many frames a second).
    #[rstest]
    fn re_place_skipped_while_the_art_area_is_unchanged(
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        let mut facade = AlbumArtFacade::new(&ctx(
            app_event_channel,
            work_request_channel,
            client_request_channel,
        ));
        facade.set_size(ratatui::layout::Rect::new(0, 0, 10, 5));
        let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 5));

        // First frame at a fresh area is treated as dirty (nothing drawn yet);
        // the flush that follows a change draws and clears the latch.
        facade.frame_rendered(&buffer);
        assert!(facade.region_dirty);
        facade.region_dirty = false;

        // Identical frames must not re-dirty: this is the steady-state
        // playback case that used to re-place the image every frame.
        facade.frame_rendered(&buffer);
        assert!(!facade.region_dirty);

        // A frame that rewrote a cell inside the art pane area (modal
        // closed, resize, layout change) re-dirties so the image heals.
        buffer.get_mut(3, 2).set_symbol("x");
        facade.frame_rendered(&buffer);
        assert!(facade.region_dirty);

        // The latch stays set until a draw clears it, so a sequence of
        // changing frames keeps the heal armed.
        facade.frame_rendered(&buffer);
        assert!(facade.region_dirty);
    }

    /// A pane-area change (resize / layout change) forces a heal instead of
    /// comparing across mismatched areas.
    #[rstest]
    fn pane_area_change_marks_dirty(
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        let mut facade = AlbumArtFacade::new(&ctx(
            app_event_channel,
            work_request_channel,
            client_request_channel,
        ));
        facade.set_size(ratatui::layout::Rect::new(0, 0, 10, 5));
        let buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 5));
        facade.frame_rendered(&buffer);
        facade.region_dirty = false;

        facade.set_size(ratatui::layout::Rect::new(0, 0, 12, 6));
        facade.frame_rendered(&buffer);
        assert!(facade.region_dirty);
    }

    /// Cells outside the art pane area must not dirty the region.
    #[rstest]
    fn cells_outside_the_art_area_are_ignored(
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        let mut facade = AlbumArtFacade::new(&ctx(
            app_event_channel,
            work_request_channel,
            client_request_channel,
        ));
        facade.set_size(ratatui::layout::Rect::new(0, 0, 4, 4));
        let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 10));
        facade.frame_rendered(&buffer);
        facade.region_dirty = false;

        // A change at (9, 9) is outside the 4x4 art pane area: no heal.
        buffer.get_mut(9, 9).set_symbol("x");
        facade.frame_rendered(&buffer);
        assert!(!facade.region_dirty);
    }

    /// A resize must never replay an image encoded for the old rect. The
    /// pending/drawn image is dropped when the pane area changes, and a late
    /// result from the pre-resize generation is refused even if it carries
    /// the new size (2026-10-28: the old-size image was drawn after the new
    /// frame, over the resized UI).
    #[rstest]
    fn a_resize_drops_images_encoded_for_the_old_rect(
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        // Keep a receiver alive: `show` sends an encode request, and the
        // fixture's ctx only keeps the sender.
        let _work_rx = work_request_channel.1.clone();
        let ctx = ctx(app_event_channel, work_request_channel, client_request_channel);
        let mut facade = AlbumArtFacade::new(&ctx);
        let old = ratatui::layout::Rect::new(0, 0, 10, 5);
        let new = ratatui::layout::Rect::new(0, 0, 20, 6);

        facade.set_size(old);
        facade.show(vec![1, 2, 3], &ctx).unwrap();
        let old_generation = facade.encode_generation;
        facade
            .display(
                EncodedImage { data: EncodeData::Empty, size: old, generation: old_generation },
                &ctx,
            )
            .unwrap();
        assert!(facade.pending_display.is_some(), "the old encode is pending");

        // The pane resizes before the next flush.
        facade.set_size(new);
        assert!(facade.pending_display.is_none(), "the old pending image is dropped");
        assert!(facade.pending_size.is_none());
        assert!(facade.last_drawn.is_none());
        assert!(facade.encode_generation > old_generation);
        assert!(matches!(facade.flush_action(), FlushAction::Nothing));

        // A result for the old generation arriving after the resize is
        // refused even though it names the new rect.
        facade.show(vec![4, 5, 6], &ctx).unwrap();
        facade
            .display(
                EncodedImage { data: EncodeData::Empty, size: new, generation: old_generation },
                &ctx,
            )
            .unwrap();
        assert!(facade.pending_display.is_none(), "a pre-resize encode is refused");
    }

    /// A result for the current geometry is still accepted and drawn.
    #[rstest]
    fn a_current_encode_is_drawn(
        app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
        work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
        client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
    ) {
        // Keep a receiver alive: `show` sends an encode request, and the
        // fixture's ctx only keeps the sender.
        let _work_rx = work_request_channel.1.clone();
        let ctx = ctx(app_event_channel, work_request_channel, client_request_channel);
        let mut facade = AlbumArtFacade::new(&ctx);
        let area = ratatui::layout::Rect::new(0, 0, 10, 5);
        facade.set_size(area);
        facade.show(vec![1, 2, 3], &ctx).unwrap();
        facade
            .display(
                EncodedImage {
                    data: EncodeData::Empty,
                    size: area,
                    generation: facade.encode_generation,
                },
                &ctx,
            )
            .unwrap();
        assert!(facade.pending_display.is_some());
        assert!(matches!(facade.flush_action(), FlushAction::Draw(_)));
    }
}
