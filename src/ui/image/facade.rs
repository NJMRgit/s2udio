use std::{io::Write, sync::Arc};
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
#[derive(Debug)]
pub struct AlbumArtFacade {
    image_backend: ImageBackend,
    current_album_art: Option<Arc<Vec<u8>>>,
    default_album_art: Arc<Vec<u8>>,
    last_size: Rect,
    /// The pane size the currently-queued encode was made for (a flush at a
    /// different size is stale).
    last_show_size: Rect,
    work_tx: Sender<WorkRequest>,
    is_showing: bool,
    request_queue: Vec<Arc<Vec<u8>>>,
    /// Encoded image waiting to be drawn after the current frame's buffer
    /// flush (see [`Self::flush_display`]).
    pending_display: Option<EncodeData>,
    /// The last successfully drawn encode: re-placed when a frame's diff
    /// rewrote its (transient) placeholder cells so the wipe heals within
    /// one frame (see [`Self::frame_rendered`]).
    last_drawn: Option<EncodeData>,
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
impl AlbumArtFacade {
    pub fn new(ctx: &Ctx) -> Self {
        let config = ctx.config.as_ref();
        let image_backend = match config.album_art.method {
            ImageMethod::Kitty => ImageBackend::Kitty(Kitty),
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
            last_show_size: Rect::default(),
            default_album_art: Arc::new(config.theme.default_album_art.to_vec()),
            work_tx: ctx.work_sender.clone(),
            is_showing: false,
            request_queue: Vec::new(),
            pending_display: None,
            last_drawn: None,
            prev_art_region: Vec::new(),
            prev_art_region_area: Rect::default(),
            region_dirty: false,
        }
    }
    pub fn show_default(&mut self, ctx: &Ctx) -> Result<()> {
        self.current_album_art = Some(Arc::clone(&self.default_album_art));
        self.show_current(ctx)
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
    pub fn show(&mut self, data: impl Into<Arc<Vec<u8>>>, ctx: &Ctx) -> Result<()> {
        self.is_showing = true;
        let max_size = ctx.config.album_art.max_size_px;
        let halign = ctx.config.album_art.horizontal_align;
        let valign = ctx.config.album_art.vertical_align;
        let size = self.last_size;
        self.last_show_size = size;
        let data = data.into();
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
                                Ok(
                                    EncodeData::Kitty(
                                        Kitty::create_data(&data, size, max_size, halign, valign)?,
                                    ),
                                )
                            }),
                        ),
                    )?;
            }
            ImageBackend::Iterm2(_iterm2) => {
                self.work_tx
                    .send(
                        WorkRequest::ResizeImage(
                            Box::new(move || {
                                Ok(
                                    EncodeData::Iterm2(
                                        Iterm2::create_data(&data, size, max_size, halign, valign)?,
                                    ),
                                )
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
                                Ok(
                                    EncodeData::Sixel(
                                        Sixel::create_data(&data, size, max_size, halign, valign)?,
                                    ),
                                )
                            }),
                        ),
                    )?;
            }
            ImageBackend::Block(_block) => {
                self.work_tx
                    .send(
                        WorkRequest::ResizeImage(
                            Box::new(move || {
                                Ok(
                                    EncodeData::Block(
                                        Block::create_data(&data, size, max_size, halign, valign)?,
                                    ),
                                )
                            }),
                        ),
                    )?;
            }
            ImageBackend::Ueberzug(_ueberzug) => {
                self.work_tx
                    .send(
                        WorkRequest::ResizeImage(
                            Box::new(move || {
                                Ok(
                                    EncodeData::Ueberzug(
                                        Ueberzug::create_data(
                                            &data,
                                            size,
                                            max_size,
                                            halign,
                                            valign,
                                        )?,
                                    ),
                                )
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
    pub fn display(&mut self, data: EncodeData, ctx: &Ctx) -> Result<()> {
        if !self.is_showing {
            log::trace!(
                "Not showing image because its not supposed to be displayed anymore"
            );
            self.request_queue.clear();
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
        log::debug!(data:?, area:? = self.last_size; "Received encoded data",);
        self.pending_display = Some(data);
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
    /// Draw the encoded image queued by [`Self::display`], or re-place the
    /// last drawn one. Called by the event loop after every frame's buffer
    /// flush. A stale encode (the pane changed size since it was queued) is
    /// re-encoded at the current size instead of drawn, so the image can
    /// never cover the UI. The last drawn image is re-placed only when
    /// [`Self::frame_rendered`] saw the frame rewrite the art pane area
    /// (re-placing after *every* frame would strobe the art while playing,
    /// since status updates / mpv polls render many frames a second).
    pub fn flush_display(&mut self, ctx: &Ctx) -> Result<()> {
        let data = match self.pending_display.take() {
            Some(data) => {
                if !self.is_showing {
                    return Ok(());
                }
                if self.last_show_size != self.last_size {
                    log::debug!(
                        last_show:? = self.last_show_size, current:? = self.last_size;
                        "Dropping stale album-art encode, re-encoding at the current size"
                    );
                    if let Some(art) = self.current_album_art.clone() {
                        self.show(art, ctx)?;
                    }
                    return Ok(());
                }
                data
            }
            None => {
                match &self.last_drawn {
                    Some(
                        data,
                    ) if self.is_showing && self.region_dirty
                        && self.last_show_size == self.last_size => data.clone(),
                    _ => return Ok(()),
                }
            }
        };
        let w = TERMINAL.writer();
        let mut w = w.lock();
        let w = w.by_ref();
        let c = ctx.config.theme.background_color.map(|c| c.into_crossterm());
        let drawn = data.clone();
        let result = match (&mut self.image_backend, data) {
            (ImageBackend::Kitty(kitty), EncodeData::Kitty(data)) => {
                kitty
                    .hide(w, self.last_size, c)
                    .and_then(|()| kitty.display(w, data, ctx))
            }
            (ImageBackend::Ueberzug(ueberzug), EncodeData::Ueberzug(data)) => {
                ueberzug
                    .hide(w, self.last_size, c)
                    .and_then(|()| ueberzug.display(w, data, ctx))
            }
            (ImageBackend::Iterm2(iterm2), EncodeData::Iterm2(data)) => {
                iterm2
                    .hide(w, self.last_size, c)
                    .and_then(|()| iterm2.display(w, data, ctx))
            }
            (ImageBackend::Sixel(sixel), EncodeData::Sixel(data)) => {
                sixel
                    .hide(w, self.last_size, c)
                    .and_then(|()| sixel.display(w, data, ctx))
            }
            (ImageBackend::Block(block), EncodeData::Block(data)) => {
                block
                    .hide(w, self.last_size, c)
                    .and_then(|()| block.display(w, data, ctx))
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
        } else {
            self.last_drawn = Some(drawn);
            self.region_dirty = false;
        }
        Ok(())
    }
    pub fn hide(&mut self, ctx: &Ctx) -> Result<()> {
        self.is_showing = false;
        self.last_drawn = None;
        self.region_dirty = false;
        let w = TERMINAL.writer();
        let mut w = w.lock();
        let w = w.by_ref();
        let c = ctx.config.theme.background_color.map(|c| c.into_crossterm());
        self.request_queue.clear();
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
        self.last_size = area;
    }
}
