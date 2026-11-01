use super::*;
use ratatui::buffer::{Buffer, Cell};

impl JellyfinPane {
    /// Round 64: whether the selected item gets the small poster shelf.
    /// Every kind sync_poster fetches an image for (playables + the
    /// season/folder containers) is shelf-worthy; the shelf replaces both
    /// the old full-box art (`image_only`) and the 40%-column split.
    pub(super) fn poster_relevant(&self) -> bool {
        // Home has no info box and no poster shelf: its cards draw a wall of
        // images instead.
        if self.mode == JellyfinTabMode::Home {
            return false;
        }
        self.selected_item()
            .is_some_and(|i| i.kind_matches_poster())
            || (self.mode == JellyfinTabMode::Libraries
                && self
                    .selected
                    .as_ref()
                    .is_some_and(|k| k.item().kind_matches_poster()))
    }
    /// Fetch the primary image of the item selected in the right pane (or of
    /// the opened node when nothing is selected) and display it in the info
    /// box.
    pub(super) fn sync_poster(&mut self, ctx: &Ctx) {
        let target = self
            .selected_item()
            .filter(|item| item.is_playable() || item.is_container())
            .or_else(|| {
                (self.mode == JellyfinTabMode::Libraries).then(|| {
                    self.selected.as_ref().map(|k| k.item().clone())
                }).flatten()
            });
        let Some(target) = target else {
            if self.poster.item_id.is_some() {
                self.poster.clear(ctx);
            }
            return;
        };
        if self.poster.item_id.as_deref() != Some(target.id.as_str()) {
            self.poster.clear(ctx);
            self.poster.item_id = Some(target.id.clone());
            self.poster.fallback_id = if target.kind == "Season" {
                target.series_id.clone()
            } else {
                None
            };
            let _ = ctx
                .work_sender
                .send(WorkRequest::FetchJellyfinImage {
                    item_id: target.id.clone(),
                    fallback_item_id: self.poster.fallback_id.clone(),
                })
                .map_err(|err| {
                    log::error!(error:? = err; "Failed to request jellyfin poster")
                });
        }
    }
    /// Display the poster queued by the last render. Called by the event
    /// loop after the frame's buffer flush, so the flush cannot overwrite
    /// the overlay's placeholder cells.
    pub(crate) fn flush_pending_poster(
        &mut self,
        frame: &ratatui::buffer::Buffer,
        ctx: &Ctx,
    ) {
        self.poster.flush_pending(Some(frame), ctx);
        // The Home carousel's images are terminal-side overlays as well: they
        // are placed after the frame's buffer flush so the flush cannot
        // overwrite their placeholder cells. The frame goes along so the cells
        // a retiring grid uncovers are repainted instead of blanked.
        if self.mode == JellyfinTabMode::Home
            && let Err(err) = self.wall.flush(Some(frame), &self.home_images, ctx)
        {
            log::debug!(error:? = err; "Failed to draw the Home carousel images");
        }
    }

    /// A full terminal clear deleted every kitty overlay (the cava-row
    /// drop repaint). The poster facade only re-draws when its area
    /// changes, so without this the info-box image would stay blank until
    /// the next selection/tab redraw. Force the next render to re-place
    /// it (identical recovery to the tab re-entry path).
    pub(crate) fn redraw_poster_after_clear(&mut self, ctx: &Ctx) {
        self.poster.redraw_next_render(ctx);
        self.wall.redraw_next_flush();
    }
    /// Hide the poster overlay (window resizing / transient state).
    pub(crate) fn hide_pending_poster(&mut self, ctx: &Ctx) {
        self.poster.hide(ctx);
        self.wall.hide(ctx);
    }
}

/// Poster / episode preview of the selected item, drawn as a terminal-side
/// overlay using the same resolved image backend as the album art (kitty /
/// sixel / iterm2 / ueberzug / block), so the terminal's best image
/// protocol is used. The overlay persists between frames (ratatui diffs
/// don't overwrite it); it is cleared on selection change and re-drawn when
/// the new image arrives or the area changes.
#[derive(Debug)]
pub(super) struct JfPoster {
    backend: PosterBackend,
    /// Primary-image bytes of the selected item.
    bytes: Option<std::sync::Arc<Vec<u8>>>,
    /// Item whose poster is loaded/requested (for stale-result checks).
    pub(super) item_id: Option<String>,
    /// When the item has no own image, the id whose image is used instead
    /// (e.g. the series poster for a season without art); re-used by a
    /// failed-bytes redraw.
    fallback_id: Option<String>,
    /// Pixel dimensions of the loaded artwork (header-only decode), used to
    /// size the shelf to the image's aspect (round 66).
    img_px: Option<(u32, u32)>,
    /// `album_art.max_size_px` captured at construction, reused to compute
    /// the artwork's display width at the shelf's fixed height.
    max_size_px: crate::config::Size,
    /// Area where the overlay was last drawn (None = nothing drawn).
    pub(super) drawn_area: Option<Rect>,
    /// Encoded image queued during render but not yet displayed: the
    /// overlay must go up *after* the frame's buffer flush, which would
    /// otherwise overwrite the kitty placeholder cells (they changed while
    /// other tabs were shown).
    pending: Option<EncodeData>,
    pending_area: Option<Rect>,
    /// The overlay last put on the glass, with the box it filled. A frame that
    /// rewrites that box's cells deletes the placement (ratatui wrote them to
    /// the terminal), and the flush re-places *this* encode instead of
    /// re-encoding the artwork.
    last_drawn: Option<(EncodeData, Rect)>,
    /// The frame cells the overlay covered at the previous flush, for the
    /// [`crate::ui::image::area_rewritten`] check.
    prev_cells: Vec<Cell>,
}
#[derive(Debug)]
enum PosterBackend {
    Kitty(Kitty),
    Ueberzug(Ueberzug),
    Iterm2(Iterm2),
    Sixel(Sixel),
    Block(ImageBlock),
    None,
}
impl JfPoster {
    pub(super) fn new(ctx: &Ctx) -> Self {
        let backend = match ctx.config.album_art.method {
            crate::config::album_art::ImageMethod::Kitty => PosterBackend::Kitty(Kitty::new()),
            crate::config::album_art::ImageMethod::UeberzugWayland => {
                PosterBackend::Ueberzug(Ueberzug::new(Layer::Wayland))
            }
            crate::config::album_art::ImageMethod::UeberzugX11 => {
                PosterBackend::Ueberzug(Ueberzug::new(Layer::X11))
            }
            crate::config::album_art::ImageMethod::Iterm2 => {
                PosterBackend::Iterm2(Iterm2)
            }
            crate::config::album_art::ImageMethod::Sixel => PosterBackend::Sixel(Sixel),
            crate::config::album_art::ImageMethod::Block => {
                PosterBackend::Block(ImageBlock)
            }
            crate::config::album_art::ImageMethod::None => PosterBackend::None,
        };
        Self {
            backend,
            bytes: None,
            item_id: None,
            fallback_id: None,
            img_px: None,
            max_size_px: ctx.config.album_art.max_size_px,
            drawn_area: None,
            pending: None,
            pending_area: None,
            last_drawn: None,
            prev_cells: Vec::new(),
        }
    }
    /// Clear the current poster: hide the overlay and drop the bytes (used
    /// when the selection changes; the new item's image is drawn on
    /// arrival).
    pub(super) fn clear(&mut self, ctx: &Ctx) {
        if let Some(area) = self.drawn_area.take() {
            self.hide_at(area, ctx);
        }
        self.pending = None;
        self.pending_area = None;
        self.bytes = None;
        self.item_id = None;
        self.img_px = None;
        self.last_drawn = None;
        self.prev_cells.clear();
    }
    fn hide_at(&mut self, area: Rect, ctx: &Ctx) {
        let writer = crate::shared::terminal::TERMINAL.writer();
        let mut w = writer.lock();
        let _ = self.hide_at_to(w.by_ref(), area, ctx);
    }
    /// [`Self::hide_at`] against a caller-supplied writer (tests replay the
    /// escape stream instead of drawing on the terminal).
    fn hide_at_to(&mut self, w: &mut impl Write, area: Rect, ctx: &Ctx) -> Result<()> {
        ctx.placements
            .borrow_mut()
            .release(crate::ui::image::placements::Surface::Poster);
        let bg = ctx.config.theme.background_color.map(|c| c.into_crossterm());
        match &mut self.backend {
            PosterBackend::Kitty(b) => b.hide(w, area, bg),
            PosterBackend::Ueberzug(b) => b.hide(w, area, bg),
            PosterBackend::Iterm2(b) => b.hide(w, area, bg),
            PosterBackend::Sixel(b) => b.hide(w, area, bg),
            PosterBackend::Block(b) => b.hide(w, area, bg),
            PosterBackend::None => Ok(()),
        }
    }
    pub(super) fn set_bytes(&mut self, item_id: String, bytes: Vec<u8>) {
        self.item_id = Some(item_id);
        self.img_px = Self::image_dimensions(&bytes);
        self.bytes = Some(std::sync::Arc::new(bytes));
        self.drawn_area = None;
        self.last_drawn = None;
        self.prev_cells.clear();
    }
    /// Header-only decode of the artwork's pixel size (no full decode —
    /// cheap enough to run when the bytes arrive). `None` on any failure.
    fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
        use std::io::Cursor;
        image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()
    }
    /// The artwork's width in terminal cells when displayed at `inner_h`
    /// rows tall (the shelf's fixed inner height) — the shelf adds 2 border
    /// columns plus a 1-char margin on each side (round 66). Mirrors
    /// `create_aligned_area`'s height-limited fit for an unbounded-width
    /// area, so the encoder fills the resulting inner area exactly. Falls
    /// back to `POSTER_SHELF_W` when the image isn't loaded yet or the
    /// terminal pixel metrics are unavailable.
    pub(super) fn natural_width_cells(&self, inner_h: u16) -> u16 {
        let Some((iw, ih)) = self.img_px else { return POSTER_SHELF_W };
        if iw == 0 || ih == 0 {
            return POSTER_SHELF_W;
        }
        let Ok(ws) = crossterm::terminal::window_size() else {
            return POSTER_SHELF_W;
        };
        if ws.width == 0 || ws.height == 0 || ws.rows == 0 || ws.columns == 0 {
            return POSTER_SHELF_W;
        }
        let cell_w = ws.width as f64 / ws.columns as f64;
        let cell_h = ws.height as f64 / ws.rows as f64;
        let bounds_w = self.max_size_px.width as f64;
        let bounds_h = ((inner_h as f64) * cell_h).min(self.max_size_px.height as f64);
        let scale = (bounds_w / iw as f64).min(bounds_h / ih as f64);
        let used_w_px = iw as f64 * scale;
        ((used_w_px / cell_w).ceil() as u16).max(1)
    }
    /// Hide the overlay (tab switch, modal opened).
    pub(super) fn hide(&mut self, ctx: &Ctx) {
        if let Some(area) = self.drawn_area.take() {
            self.hide_at(area, ctx);
        }
        self.pending = None;
        self.pending_area = None;
        self.last_drawn = None;
        self.prev_cells.clear();
    }
    fn encode(&mut self, area: Rect, ctx: &Ctx) -> Option<EncodeData> {
        let bytes = self.bytes.as_ref()?;
        let max_size = ctx.config.album_art.max_size_px;
        let halign = ctx.config.album_art.horizontal_align;
        let valign = ctx.config.album_art.vertical_align;
        let result = match &self.backend {
            PosterBackend::Kitty(_) => {
                Kitty::create_data(bytes, area, max_size, halign, valign)
                    .map(EncodeData::Kitty)
            }
            PosterBackend::Ueberzug(_) => {
                Ueberzug::create_data(bytes, area, max_size, halign, valign)
                    .map(EncodeData::Ueberzug)
            }
            PosterBackend::Iterm2(_) => {
                Iterm2::create_data(bytes, area, max_size, halign, valign)
                    .map(EncodeData::Iterm2)
            }
            PosterBackend::Sixel(_) => {
                Sixel::create_data(bytes, area, max_size, halign, valign)
                    .map(EncodeData::Sixel)
            }
            PosterBackend::Block(_) => {
                ImageBlock::create_data(bytes, area, max_size, halign, valign)
                    .map(EncodeData::Block)
            }
            PosterBackend::None => return None,
        };
        match result {
            Ok(data) => Some(data),
            Err(err) => {
                log::debug!(error:? = err; "Failed to encode jellyfin poster");
                None
            }
        }
    }
    /// Display `data` on a caller-supplied writer (the tests replay the escape
    /// stream instead of drawing on the terminal).
    fn display_to(&mut self, w: &mut impl Write, data: EncodeData, ctx: &Ctx) -> bool {
        match (&mut self.backend, data) {
            (PosterBackend::Kitty(b), EncodeData::Kitty(d)) => {
                b.display(w, d, ctx).is_ok()
            }
            (PosterBackend::Ueberzug(b), EncodeData::Ueberzug(d)) => {
                b.display(w, d, ctx).is_ok()
            }
            (PosterBackend::Iterm2(b), EncodeData::Iterm2(d)) => {
                b.display(w, d, ctx).is_ok()
            }
            (PosterBackend::Sixel(b), EncodeData::Sixel(d)) => {
                b.display(w, d, ctx).is_ok()
            }
            (PosterBackend::Block(b), EncodeData::Block(d)) => {
                b.display(w, d, ctx).is_ok()
            }
            _ => false,
        }
    }
    /// Re-place the overlay without re-encoding it: a backend that already
    /// holds this payload (kitty, by image id) sends the placement and the
    /// grid only, which is what makes the heal below affordable.
    fn replace_to(
        &mut self,
        w: &mut impl Write,
        area: Rect,
        data: EncodeData,
        ctx: &Ctx,
    ) -> Result<()> {
        let bg = ctx.config.theme.background_color.map(|c| c.into_crossterm());
        match (&mut self.backend, data) {
            (PosterBackend::Kitty(b), EncodeData::Kitty(d)) => {
                b.replace(w, area, bg, d, ctx, None)
            }
            (PosterBackend::Ueberzug(b), EncodeData::Ueberzug(d)) => {
                b.replace(w, area, bg, d, ctx, None)
            }
            (PosterBackend::Iterm2(b), EncodeData::Iterm2(d)) => {
                b.replace(w, area, bg, d, ctx, None)
            }
            (PosterBackend::Sixel(b), EncodeData::Sixel(d)) => {
                b.replace(w, area, bg, d, ctx, None)
            }
            (PosterBackend::Block(b), EncodeData::Block(d)) => {
                b.replace(w, area, bg, d, ctx, None)
            }
            _ => Ok(()),
        }
    }
    /// Draw the poster into `area`, re-encoding only when the area changed
    /// since the last draw. The display itself is deferred to
    /// `flush_pending`, which the event loop calls *after* the frame's
    /// buffer flush — displaying during the render would let that flush
    /// overwrite the kitty placeholder cells with the newly drawn tab
    /// content (the cells changed while other tabs were shown).
    pub(super) fn draw(&mut self, area: Rect, ctx: &Ctx) {
        let show = self.bytes.is_some() && area.height >= 3 && area.width >= 3;
        if !show {
            self.pending = None;
            self.pending_area = None;
            if let Some(old) = self.drawn_area.take() {
                self.hide_at(old, ctx);
            }
            return;
        }
        if self.drawn_area == Some(area) {
            return;
        }
        if self.pending.is_some() && self.pending_area == Some(area) {
            return;
        }
        let Some(data) = self.encode(area, ctx) else {
            self.drawn_area = Some(area);
            return;
        };
        self.pending = Some(data);
        self.pending_area = Some(area);
    }
    /// Display the queued overlay (called after the frame flush), then heal a
    /// placement the frame's diff deleted. No-op when nothing is queued and
    /// nothing was rewritten.
    pub(super) fn flush_pending(&mut self, frame: Option<&Buffer>, ctx: &Ctx) {
        let writer = crate::shared::terminal::TERMINAL.writer();
        let mut w = writer.lock();
        if let Err(err) = self.flush_pending_to(w.by_ref(), frame, ctx) {
            log::debug!(error:? = err; "Failed to display the Jellyfin poster");
        }
    }
    /// [`Self::flush_pending`] against a caller-supplied writer (tests replay
    /// the escape stream instead of drawing on the terminal).
    fn flush_pending_to(
        &mut self,
        w: &mut impl Write,
        frame: Option<&Buffer>,
        ctx: &Ctx,
    ) -> Result<()> {
        let mut just_drawn = false;
        if let Some(data) = self.pending.take() {
            let area = self.pending_area.take().unwrap_or_default();
            if let Some(old) = self.drawn_area.take() && old != area {
                let _ = self.hide_at_to(w, old, ctx);
            }
            if self.display_to(w, data.clone(), ctx) {
                self.drawn_area = Some(area);
                self.last_drawn = Some((data, area));
                just_drawn = true;
                // The poster is a terminal-side overlay: record its rect so a
                // clear (or anything asking what is under a point) finds it.
                ctx.placements
                    .borrow_mut()
                    .declare(crate::ui::image::placements::Surface::Poster, area);
            }
        }
        let Some(frame) = frame else { return Ok(()) };
        let Some(area) = self.drawn_area else {
            self.prev_cells.clear();
            return Ok(());
        };
        // A frame that rewrote the cells under the overlay deleted its
        // placement — the modal backdrop's fade recolours every cell, so the
        // detail page's poster vanished the moment a menu closed and the
        // scrim lifted (2026-11-01, same hole as the Home carousel). Re-place
        // the encode we already have; the terminal still stores the payload.
        let rewritten = !just_drawn
            && crate::ui::image::area_rewritten(frame, area, &self.prev_cells);
        if rewritten
            && let Some((data, drawn)) = self.last_drawn.clone()
            && drawn == area
        {
            self.replace_to(w, area, data, ctx)?;
        }
        self.prev_cells = crate::ui::image::snapshot_area(frame, area);
        Ok(())
    }
    /// Force the next render to redraw the overlay (the tab was reopened
    /// and the overlay was hidden on the way out). Re-requests the image
    /// when the bytes are missing (an earlier fetch failed).
    pub(super) fn redraw_next_render(&mut self, ctx: &Ctx) {
        self.drawn_area = None;
        if self.bytes.is_none() && let Some(item_id) = self.item_id.clone() {
            let _ = ctx
                .work_sender
                .send(WorkRequest::FetchJellyfinImage {
                    item_id,
                    fallback_item_id: self.fallback_id.clone(),
                })
                .map_err(|err| {
                    log::error!(error:? = err; "Failed to request jellyfin poster")
                });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::album_art::ImageMethod;
    use ratatui::{buffer::Buffer, style::Style};

    fn test_ctx() -> Ctx {
        crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }

    fn ctx_with_kitty() -> Ctx {
        let mut ctx = test_ctx();
        let mut config = ctx.config.as_ref().clone();
        config.album_art.method = ImageMethod::Kitty;
        ctx.config = std::sync::Arc::new(config);
        ctx
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::new_rgba8(width, height);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("encode a test png");
        bytes.into_inner()
    }

    /// A frame whose diff rewrote the cells under the poster deleted its
    /// kitty placement. The modal backdrop's fade recolours every cell, so
    /// the detail page's poster vanished the moment a menu closed and the
    /// scrim lifted — the same hole the Home carousel had (2026-11-01). The
    /// flush re-places the encode it already has: a placement command, not a
    /// second transfer.
    #[test]
    fn a_frame_that_rewrites_the_poster_cells_re_places_it() {
        let ctx = ctx_with_kitty();
        let mut poster = JfPoster::new(&ctx);
        poster.set_bytes("item".to_owned(), png_bytes(64, 96));

        let area = Rect::new(0, 0, 20, 10);
        poster.draw(area, &ctx);
        assert!(poster.pending.is_some(), "the draw queues an encode");

        let mut frame = Buffer::empty(Rect::new(0, 0, 40, 20));
        frame.set_string(0, 0, "info", Style::default());
        let mut first = Vec::new();
        poster
            .flush_pending_to(&mut first, Some(&frame), &ctx)
            .expect("first flush");
        let text = String::from_utf8_lossy(&first);
        assert!(text.contains("a=t"), "the first flush transmits the payload: {text:?}");
        assert!(text.contains("a=p"), "and places it: {text:?}");
        assert_eq!(poster.drawn_area, Some(area));

        // The same frame again: nothing was rewritten, so nothing is
        // re-placed (an unconditional re-place strobes the poster).
        let mut steady = Vec::new();
        poster
            .flush_pending_to(&mut steady, Some(&frame), &ctx)
            .expect("steady flush");
        assert!(steady.is_empty(), "an unchanged frame must not re-place the poster");

        // The backdrop's fade recoloured every cell under the overlay.
        let mut dimmed = frame.clone();
        dimmed.set_style(area, Style::default().bg(ratatui::style::Color::Rgb(9, 9, 9)));
        let mut healed = Vec::new();
        poster
            .flush_pending_to(&mut healed, Some(&dimmed), &ctx)
            .expect("heal flush");
        let text = String::from_utf8_lossy(&healed);
        assert!(
            text.contains("a=p"),
            "the poster must be re-placed from the terminal's copy: {text:?}"
        );
        assert!(
            !text.contains("a=t"),
            "a re-place sends the placement and the grid, not the payload: {text:?}"
        );
    }

    /// Hiding the overlay drops the encode the heal would re-place: after a
    /// `ModalOpened` there is nothing on the glass to put back.
    #[test]
    fn a_hidden_poster_does_not_heal() {
        let ctx = ctx_with_kitty();
        let mut poster = JfPoster::new(&ctx);
        poster.set_bytes("item".to_owned(), png_bytes(64, 96));
        let area = Rect::new(0, 0, 20, 10);
        poster.draw(area, &ctx);
        let frame = Buffer::empty(Rect::new(0, 0, 40, 20));
        let mut first = Vec::new();
        poster
            .flush_pending_to(&mut first, Some(&frame), &ctx)
            .expect("first flush");
        // Drop the drawn area first so `hide` has nothing to erase on the
        // glass (a test must not write to the terminal); the bookkeeping it
        // clears is what this checks.
        poster.drawn_area = None;
        poster.hide(&ctx);
        assert!(poster.last_drawn.is_none());

        let mut dimmed = frame.clone();
        dimmed.set_style(area, Style::default().bg(ratatui::style::Color::Rgb(9, 9, 9)));
        let mut out = Vec::new();
        poster
            .flush_pending_to(&mut out, Some(&dimmed), &ctx)
            .expect("flush after hide");
        assert!(out.is_empty(), "a hidden overlay has nothing to re-place: {out:?}");
    }
}
