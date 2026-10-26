use super::*;

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
    pub(crate) fn flush_pending_poster(&mut self, ctx: &Ctx) {
        self.poster.flush_pending(ctx);
        // The Home carousel's images are terminal-side overlays as well: they
        // are placed after the frame's buffer flush so the flush cannot
        // overwrite their placeholder cells.
        if self.mode == JellyfinTabMode::Home
            && let Err(err) = self.wall.flush(&self.home_images, ctx)
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
            crate::config::album_art::ImageMethod::Kitty => PosterBackend::Kitty(Kitty),
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
    }
    fn hide_at(&mut self, area: Rect, ctx: &Ctx) {
        let bg = ctx.config.theme.background_color.map(|c| c.into_crossterm());
        let writer = crate::shared::terminal::TERMINAL.writer();
        let mut w = writer.lock();
        let w = w.by_ref();
        match &mut self.backend {
            PosterBackend::Kitty(b) => {
                let _ = b.hide(w, area, bg);
            }
            PosterBackend::Ueberzug(b) => {
                let _ = b.hide(w, area, bg);
            }
            PosterBackend::Iterm2(b) => {
                let _ = b.hide(w, area, bg);
            }
            PosterBackend::Sixel(b) => {
                let _ = b.hide(w, area, bg);
            }
            PosterBackend::Block(b) => {
                let _ = b.hide(w, area, bg);
            }
            PosterBackend::None => {}
        }
    }
    pub(super) fn set_bytes(&mut self, item_id: String, bytes: Vec<u8>) {
        self.item_id = Some(item_id);
        self.img_px = Self::image_dimensions(&bytes);
        self.bytes = Some(std::sync::Arc::new(bytes));
        self.drawn_area = None;
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
    fn display(&mut self, data: EncodeData, ctx: &Ctx) -> bool {
        let writer = crate::shared::terminal::TERMINAL.writer();
        let mut w = writer.lock();
        let w = w.by_ref();
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
    /// Display the queued overlay (called after the frame flush). No-op
    /// when nothing is queued.
    pub(super) fn flush_pending(&mut self, ctx: &Ctx) {
        let Some(data) = self.pending.take() else { return };
        let area = self.pending_area.take().unwrap_or_default();
        if let Some(old) = self.drawn_area.take() && old != area {
            self.hide_at(old, ctx);
        }
        if self.display(data, ctx) {
            self.drawn_area = Some(area);
        }
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
