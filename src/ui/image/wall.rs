//! A wall of terminal-side images for the Jellyfin Home carousel.
//!
//! The album-art and Jellyfin-poster overlays each manage **one** image; the
//! Home page shows a row of cards, so it needs many at once. Kitty's
//! protocol already addresses images by id (the Unicode-placeholder cells
//! colour their image id into the foreground), so a wall is one image id per
//! visible card: every slot transmits its own image and paints its own
//! placeholder grid.
//!
//! Only the kitty backend is supported; on every other image method the wall
//! reports itself disabled and the cards fall back to text placeholders. The
//! rest of the app keeps its single-image overlay paths untouched.
use std::{collections::HashMap, io::Write, sync::Arc};

use anyhow::Result;
use ratatui::{buffer::Buffer, layout::Rect, prelude::IntoCrossterm};

use super::kitty::{self, WallTile};
use crate::{config::album_art::ImageMethod, ctx::Ctx, shared::terminal::TERMINAL};

/// Upper bound on simultaneously displayed carousel images. The wall uses
/// `slot + 2` as the kitty image id, and the placeholder cells encode that id
/// through the 256-color range (`38;5;<id>`), so 253 slots is the protocol
/// limit. 32 was too small once the Home page grew several rows: a 5-row page
/// with 7-8 cards per row still misses the last row's posters (2026-10-28).
pub(crate) const MAX_SLOTS: usize = 250;

/// One visible card's image slot, as computed by the Home renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WallPlacement {
    /// Stable slot index for this frame (0-based).
    pub(crate) slot: u16,
    /// Cache tag of the image bytes this slot shows.
    pub(crate) tag: String,
    /// Cells the image fills (the card's inner, borderless rectangle).
    pub(crate) area: Rect,
}

#[derive(Debug)]
struct Slot {
    tag: String,
    area: Rect,
    /// Kitty image id (`slot + 2`; id 0 is kitty's "no id" and id 1 is the
    /// single-image poster overlay's).
    image_id: u32,
    encoded: Option<WallTile>,
    placed: bool,
    dirty: bool,
    /// An encode that failed for this tag/area: do not retry every frame.
    failed: bool,
    /// The area this tile occupied before it moved. Its placeholder cells stay
    /// in the terminal after the image is freed, so the flush repaints that
    /// rectangle from the frame instead of leaving the cells blank (they may
    /// hold the new layout's text: the rows move when a card's geometry
    /// changes).
    stale_area: Option<Rect>,
}

#[derive(Debug)]
pub(crate) struct PosterWall {
    enabled: bool,
    slots: Vec<Option<Slot>>,
    /// Slots the last frame asked for (used to retire the rest).
    placements: Vec<WallPlacement>,
    /// Set when the terminal was cleared under the placeholders: the next
    /// flush re-paints every grid (encoded data is still valid).
    redraw_all: bool,
}

impl PosterWall {
    pub(crate) fn new(ctx: &Ctx) -> Self {
        Self {
            // Unicode placeholders + per-id transfers are kitty-only here.
            enabled: matches!(ctx.config.album_art.method, ImageMethod::Kitty),
            slots: (0..MAX_SLOTS).map(|_| None).collect(),
            placements: Vec::new(),
            redraw_all: false,
        }
    }
    /// Whether this wall can actually draw images (kitty).
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }
    /// Record the visible cards for this frame. No drawing happens here: the
    /// terminal-side placeholders must be written after the frame's buffer
    /// flush, in [`Self::flush`].
    pub(crate) fn set_layout(&mut self, placements: Vec<WallPlacement>) {
        for placement in &placements {
            let idx = usize::from(placement.slot);
            if idx >= self.slots.len() {
                continue;
            }
            // Ids start at 2: the single-image poster overlay uses id 1, and
            // keeping the wall's ids clear of it means a leftover placement
            // can never steal the poster's image in the terminal.
            let image_id = u32::from(placement.slot) + 2;
            match &mut self.slots[idx] {
                Some(slot) if slot.tag == placement.tag && slot.area == placement.area => {}
                Some(slot) => {
                    // Any change invalidates the encoded tile; `placed` is
                    // kept because the terminal still holds an image under
                    // this id, and the flush must free it before the new
                    // transfer (the kitty protocol mandates it).
                    slot.encoded = None;
                    slot.failed = false;
                    slot.tag = placement.tag.clone();
                    if slot.area != placement.area && slot.placed {
                        slot.stale_area = Some(slot.area);
                    }
                    slot.area = placement.area;
                    slot.image_id = image_id;
                    slot.dirty = true;
                }
                None => {
                    self.slots[idx] = Some(Slot {
                        tag: placement.tag.clone(),
                        area: placement.area,
                        image_id,
                        encoded: None,
                        placed: false,
                        dirty: true,
                        failed: false,
                        stale_area: None,
                    });
                }
            }
        }
        self.placements = placements;
    }
    /// Draw every slot that needs it. Encodes on the calling (UI) thread,
    /// like the single-poster overlay does; only slots whose item or area
    /// changed are encoded, so scrolling re-encodes just the arriving cards.
    ///
    /// `frame` is the buffer the caller just flushed. Cells this overlay
    /// uncovered or freed are repainted from it: the diff that put the frame
    /// on the glass will not write them again, so blanking them (the old
    /// `clear_area`) left a hole in whatever the new layout drew there. A card
    /// row's geometry changes when the Libraries carousel is revealed or
    /// hidden (fewer/more cards per row, taller cards), which moves every
    /// card below it - exactly the case where the retired grid sat on top of
    /// the new layout's label rows.
    pub(crate) fn flush(
        &mut self,
        frame: Option<&Buffer>,
        bytes: &HashMap<String, Arc<Vec<u8>>>,
        ctx: &Ctx,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let writer = TERMINAL.writer();
        let mut guard = writer.lock();
        self.flush_to(guard.by_ref(), frame, bytes, ctx)
    }
    /// [`Self::flush`] against a caller-supplied writer (tests replay the
    /// escape stream instead of drawing on the terminal).
    fn flush_to(
        &mut self,
        w: &mut impl Write,
        frame: Option<&Buffer>,
        bytes: &HashMap<String, Arc<Vec<u8>>>,
        ctx: &Ctx,
    ) -> Result<()> {
        let bg = ctx.config.theme.background_color.map(|c| c.into_crossterm());
        // Retire slots the new layout no longer uses.
        let in_layout: Vec<usize> = self
            .placements
            .iter()
            .map(|p| usize::from(p.slot))
            .collect();
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            if in_layout.contains(&idx) {
                continue;
            }
            if let Some(slot) = slot.take()
                && slot.placed
            {
                let _ = kitty::delete_wall_tile(w, slot.image_id);
                restore(w, frame, slot.area, bg)?;
            }
        }
        // Repaint every rectangle a tile is leaving BEFORE any new grid is
        // written. A placeholder cell that is overwritten breaks its kitty
        // placement, so a repaint that ran after another slot's grid was
        // painted would cut that poster in half (the image is drawn only over
        // the cells whose placeholder survived).
        for placement in &self.placements {
            let idx = usize::from(placement.slot);
            let Some(slot) = self.slots.get_mut(idx).and_then(Option::as_mut) else {
                continue;
            };
            if let Some(stale) = slot.stale_area.take() {
                restore(w, frame, stale, bg)?;
            }
        }
        if self.redraw_all {
            self.redraw_all = false;
            // The terminal was cleared: every grid is gone. The encoded data
            // is still valid, so each slot only needs its image freed and its
            // grid re-painted.
            for slot in self.slots.iter_mut().flatten() {
                slot.dirty = true;
            }
        }
        for placement in &self.placements {
            let idx = usize::from(placement.slot);
            let Some(slot) = self.slots.get_mut(idx).and_then(Option::as_mut) else {
                continue;
            };
            if !slot.dirty && slot.placed {
                continue;
            }
            if !slot.failed
                && slot.encoded.is_none()
                && let Some(data) = bytes.get(&placement.tag)
            {
                match kitty::create_wall_tile(
                    data,
                    placement.area.width,
                    placement.area.height,
                    slot.image_id,
                ) {
                    Ok(tile) => slot.encoded = Some(tile),
                    Err(err) => {
                        log::debug!(
                            tag:? = placement.tag.as_str(), error:? = err;
                            "Could not encode a Home carousel image"
                        );
                        slot.failed = true;
                    }
                }
            }
            let Some(tile) = slot.encoded.as_ref() else {
                // Nothing to draw for this card: the server has no artwork
                // for it (the pane remembers that, and the card is drawn with
                // its name instead), or the encode failed. Drop the grid this
                // slot still shows, or the *previous* card's poster stays on
                // the new card: the Libraries row scrolls one card at a time,
                // so an id's old grid sits exactly on the incoming card.
                if slot.placed {
                    let _ = kitty::delete_wall_tile(w, slot.image_id);
                    restore(w, frame, slot.area, bg)?;
                    slot.placed = false;
                }
                continue;
            };
            // Re-transmitting an id requires the old image to be freed first
            // (the kitty protocol mandates it; the new data is not displayed
            // until a placement is created). Always delete: the terminal may
            // still hold this id from an earlier run that did not clean up.
            let _ = kitty::delete_wall_tile(w, slot.image_id);
            match kitty::display_wall_tile(w, tile, slot.area, bg) {
                Ok(()) => {
                    slot.placed = true;
                    slot.dirty = false;
                }
                Err(err) => {
                    log::debug!(error:? = err; "Could not display a Home carousel image");
                    slot.failed = true;
                }
            }
        }
        Ok(())
    }
    /// The slots the last frame asked for (diagnostics and layout tests).
    #[cfg(test)]
    pub(crate) fn layout(&self) -> &[WallPlacement] {
        &self.placements
    }
    /// Remove every image and blank its cells (tab switch, modal, resize).
    pub(crate) fn hide(&mut self, ctx: &Ctx) {
        if !self.enabled {
            return;
        }
        let writer = TERMINAL.writer();
        let mut guard = writer.lock();
        let w = guard.by_ref();
        let bg = ctx.config.theme.background_color.map(|c| c.into_crossterm());
        for slot in self.slots.iter_mut().flatten() {
            if slot.placed {
                let _ = kitty::delete_wall_tile(w, slot.image_id);
                let _ = super::clear_area(w, bg, slot.area);
                slot.placed = false;
                slot.dirty = true;
            }
        }
        self.placements.clear();
    }
    /// The terminal was cleared: every placeholder grid is gone, but the
    /// image data is still valid, so the next flush only re-paints.
    pub(crate) fn redraw_next_flush(&mut self) {
        self.redraw_all = true;
    }
}

/// Put `area` back on the glass after an image left it.
///
/// With the flushed frame at hand the cells are repainted from their real
/// content; without one (a hide, driven by an event rather than a frame) the
/// area is blanked, which is what the next full page render expects.
fn restore(
    w: &mut impl Write,
    frame: Option<&Buffer>,
    area: Rect,
    bg: Option<crossterm::style::Color>,
) -> Result<()> {
    match frame {
        Some(buffer) => super::paint_area(w, buffer, area),
        None => super::clear_area(w, bg, area),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::album_art::ImageMethod;
    use ratatui::style::Style;

    fn test_ctx() -> Ctx {
        crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }

    fn ctx_with_method(method: ImageMethod) -> Ctx {
        let mut ctx = test_ctx();
        let mut config = ctx.config.as_ref().clone();
        config.album_art.method = method;
        ctx.config = std::sync::Arc::new(config);
        ctx
    }

    /// Only the kitty backend can draw a wall; every other method reports
    /// itself disabled so the cards fall back to text.
    #[test]
    fn only_kitty_enables_the_wall() {
        let ctx = ctx_with_method(ImageMethod::Kitty);
        assert!(PosterWall::new(&ctx).is_enabled());
        for method in [
            ImageMethod::Sixel,
            ImageMethod::Iterm2,
            ImageMethod::Block,
            ImageMethod::None,
            ImageMethod::UeberzugWayland,
        ] {
            let ctx = ctx_with_method(method);
            assert!(!PosterWall::new(&ctx).is_enabled(), "{method:?}");
        }
    }

    /// The layout assigns one image id per slot and re-marks a slot dirty
    /// only when its item or its area changed.
    #[test]
    fn a_slot_re_encodes_only_when_its_item_or_area_changes() {
        let ctx = ctx_with_method(ImageMethod::Kitty);
        let mut wall = PosterWall::new(&ctx);
        let area = Rect::new(0, 0, 10, 8);
        wall.set_layout(vec![WallPlacement {
            slot: 0,
            tag: "a".to_owned(),
            area,
        }]);
        let slot = wall.slots[0].as_ref().unwrap();
        assert!(slot.dirty);
        assert_eq!(slot.image_id, 2);

        // Same tag + area: nothing to do (the flush clears `dirty`, so set it
        // back to simulate a displayed slot).
        wall.slots[0].as_mut().unwrap().dirty = false;
        wall.set_layout(vec![WallPlacement {
            slot: 0,
            tag: "a".to_owned(),
            area,
        }]);
        assert!(!wall.slots[0].as_ref().unwrap().dirty);

        // New item in the slot: dirty and the old encoding is dropped.
        wall.set_layout(vec![WallPlacement {
            slot: 0,
            tag: "b".to_owned(),
            area,
        }]);
        let slot = wall.slots[0].as_ref().unwrap();
        assert!(slot.dirty);
        assert!(slot.encoded.is_none());

        // New area for the same item: re-encode, but the slot is still tied
        // to the same kitty id.
        wall.set_layout(vec![WallPlacement {
            slot: 0,
            tag: "b".to_owned(),
            area: Rect::new(0, 0, 12, 8),
        }]);
        let slot = wall.slots[0].as_ref().unwrap();
        assert!(slot.dirty);
        assert_eq!(slot.image_id, 2);
    }

    /// Every visible slot gets its own image id, clear of the single-poster
    /// overlay's id 1, and the layout records what is on screen (the flush
    /// retires the rest).
    #[test]
    fn layout_slots_get_distinct_ids_clear_of_the_poster() {
        let ctx = ctx_with_method(ImageMethod::Kitty);
        let mut wall = PosterWall::new(&ctx);
        wall.set_layout(vec![
            WallPlacement { slot: 0, tag: "a".to_owned(), area: Rect::new(0, 0, 4, 4) },
            WallPlacement { slot: 1, tag: "b".to_owned(), area: Rect::new(6, 0, 4, 4) },
        ]);
        assert_eq!(wall.slots[0].as_ref().unwrap().image_id, 2);
        assert_eq!(wall.slots[1].as_ref().unwrap().image_id, 3);
        wall.set_layout(vec![WallPlacement {
            slot: 1,
            tag: "b".to_owned(),
            area: Rect::new(6, 0, 4, 4),
        }]);
        assert_eq!(wall.placements.len(), 1, "only one card is on screen now");
        assert_eq!(wall.placements[0].slot, 1);
    }

    /// A tile that leaves the page must put the frame's text back, not blank
    /// its cells: the diff that painted the current frame has already run, so
    /// anything this flush erases stays erased until an unrelated repaint.
    /// The reported case: Up (Libraries carousel appears: fewer, wider cards
    /// per row) then Down again - the retired grids of the 4-row layout land
    /// on the bottom row's label lines, which went blank.
    #[test]
    fn a_retired_tile_repaints_the_frames_text_instead_of_blanking_it() {
        let ctx = ctx_with_method(ImageMethod::Kitty);
        let mut wall = PosterWall::new(&ctx);
        let mut bytes: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
        bytes.insert("tag".to_owned(), Arc::new(png_bytes(64, 96)));

        // Frame 1: a card image sits where the labels are about to move to.
        let area = Rect::new(2, 1, 16, 4);
        wall.set_layout(vec![WallPlacement {
            slot: 0,
            tag: "tag".to_owned(),
            area,
        }]);
        let mut first = Vec::new();
        wall.flush_to(&mut first, None, &bytes, &ctx).expect("first flush");
        assert!(wall.slots[0].as_ref().is_some_and(|slot| slot.placed));

        // Frame 2: the card is gone (a shorter layout) and the label line
        // occupies the cells the image used to cover.
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 8));
        buffer.set_string(2, 1, "Season 29", Style::default());
        buffer.set_string(2, 2, "South America", Style::default());
        wall.set_layout(Vec::new());
        let mut out = Vec::new();
        wall.flush_to(&mut out, Some(&buffer), &bytes, &ctx).expect("retire flush");

        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("Season 29"),
            "the retired grid must repaint the label line: {text:?}"
        );
        assert!(
            text.contains("South America"),
            "and its subline: {text:?}"
        );
        assert!(wall.slots[0].is_none(), "the slot is retired");
    }

    /// The same hole appears when a tile *moves* (the rows shift when the card
    /// geometry changes): its old placeholder grid stays in the terminal, so
    /// the flush has to repaint that rectangle too.
    #[test]
    fn a_moved_tile_repaints_its_old_area() {
        let ctx = ctx_with_method(ImageMethod::Kitty);
        let mut wall = PosterWall::new(&ctx);
        let mut bytes: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
        bytes.insert("tag".to_owned(), Arc::new(png_bytes(64, 96)));
        let first_area = Rect::new(2, 1, 16, 4);
        wall.set_layout(vec![WallPlacement {
            slot: 0,
            tag: "tag".to_owned(),
            area: first_area,
        }]);
        let mut first = Vec::new();
        wall.flush_to(&mut first, None, &bytes, &ctx).expect("first flush");

        let moved = Rect::new(2, 5, 16, 4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 12));
        buffer.set_string(2, 1, "Recently Added", Style::default());
        wall.set_layout(vec![WallPlacement {
            slot: 0,
            tag: "tag".to_owned(),
            area: moved,
        }]);
        let mut out = Vec::new();
        wall.flush_to(&mut out, Some(&buffer), &bytes, &ctx).expect("move flush");

        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("Recently Added"),
            "the old grid's cells are repainted from the frame: {text:?}"
        );
        assert!(
            text.contains("c=16,r=4"),
            "and the new grid is still placed: {text:?}"
        );
    }

    /// A repaint must never land on a grid that was painted moments earlier:
    /// kitty drops the placement for cells whose placeholder is overwritten,
    /// so the poster is drawn only over the surviving cells - the half-drawn
    /// cards the first version of this fix produced. Every repaint therefore
    /// happens before the first new grid.
    #[test]
    fn repaints_run_before_any_new_grid_is_painted() {
        let ctx = ctx_with_method(ImageMethod::Kitty);
        let mut wall = PosterWall::new(&ctx);
        let mut bytes: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
        bytes.insert("a".to_owned(), Arc::new(png_bytes(64, 96)));
        bytes.insert("b".to_owned(), Arc::new(png_bytes(64, 96)));
        // Frame 1: both cards sit on the upper rows.
        wall.set_layout(vec![
            WallPlacement { slot: 0, tag: "a".to_owned(), area: Rect::new(2, 1, 16, 4) },
            WallPlacement { slot: 1, tag: "b".to_owned(), area: Rect::new(20, 1, 16, 4) },
        ]);
        let mut first = Vec::new();
        wall.flush_to(&mut first, None, &bytes, &ctx).expect("first flush");

        // Frame 2: both tiles move down; the frame drew a label in each old
        // rectangle.
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 12));
        buffer.set_string(2, 2, "Recently Added", Style::default());
        buffer.set_string(20, 2, "Overgeared", Style::default());
        wall.set_layout(vec![
            WallPlacement { slot: 0, tag: "a".to_owned(), area: Rect::new(2, 6, 16, 4) },
            WallPlacement { slot: 1, tag: "b".to_owned(), area: Rect::new(20, 6, 16, 4) },
        ]);
        let mut out = Vec::new();
        wall.flush_to(&mut out, Some(&buffer), &bytes, &ctx).expect("move flush");

        let text = String::from_utf8_lossy(&out);
        let grid_at = text
            .find("a=T,U=1")
            .unwrap_or_else(|| panic!("no grid was painted: {text:?}"));
        for label in ["Recently Added", "Overgeared"] {
            let at = text
                .find(label)
                .unwrap_or_else(|| panic!("{label} was not repainted: {text:?}"));
            assert!(
                at < grid_at,
                "the repaint of {label} ran after a grid write: {text:?}"
            );
        }
        assert!(
            text.matches("a=T,U=1").count() >= 2,
            "both moved tiles are re-painted: {text:?}"
        );
    }

    /// A card whose item has no artwork must not keep the poster of the card
    /// that occupied the slot before it. The Libraries row scrolls one card
    /// at a time, so the previous item's grid sits exactly on the incoming
    /// card - reported as "a library without a cover shows another library's
    /// cover".
    #[test]
    fn a_card_without_artwork_drops_the_previous_poster() {
        let ctx = ctx_with_method(ImageMethod::Kitty);
        let mut wall = PosterWall::new(&ctx);
        let mut bytes: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
        bytes.insert("with-art".to_owned(), Arc::new(png_bytes(64, 96)));
        let area = Rect::new(2, 1, 16, 4);
        wall.set_layout(vec![WallPlacement {
            slot: 0,
            tag: "with-art".to_owned(),
            area,
        }]);
        let mut first = Vec::new();
        wall.flush_to(&mut first, None, &bytes, &ctx).expect("first flush");
        assert!(wall.slots[0].as_ref().is_some_and(|slot| slot.placed));

        // The row scrolls: the slot now shows an item without artwork (its
        // tag never enters the byte cache).
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 8));
        buffer.set_string(2, 2, "Live Action (Adult)", Style::default());
        wall.set_layout(vec![WallPlacement {
            slot: 0,
            tag: "no-art".to_owned(),
            area,
        }]);
        let mut out = Vec::new();
        wall.flush_to(&mut out, Some(&buffer), &bytes, &ctx).expect("art-less flush");

        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("a=d,d=I,i=2"),
            "the old poster is freed: {text:?}"
        );
        assert!(
            text.contains("Live Action (Adu"),
            "and the cell is repainted from the frame: {text:?}"
        );
        assert!(
            !text.contains("a=T,U=1"),
            "nothing new is transferred for a card without artwork: {text:?}"
        );
        assert!(
            !wall.slots[0].as_ref().is_some_and(|slot| slot.placed),
            "the slot is no longer holding an image"
        );
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::new_rgba8(width, height);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("encode a test png");
        bytes.into_inner()
    }

    /// A multi-row Home page has more visible cards than the old 32 slots, so
    /// the last row lost its later posters. Every card of a 40-card page must
    /// get its own slot/id in one flush.
    #[test]
    fn a_multi_row_page_gets_a_slot_for_every_visible_card() {
        let ctx = ctx_with_method(ImageMethod::Kitty);
        let mut wall = PosterWall::new(&ctx);
        let mut bytes: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
        bytes.insert("poster".to_owned(), Arc::new(png_bytes(32, 48)));
        let placements: Vec<WallPlacement> = (0..40u16)
            .map(|slot| WallPlacement {
                slot,
                tag: "poster".to_owned(),
                area: Rect::new((slot % 8) * 18, (slot / 8) * 12, 16, 10),
            })
            .collect();
        wall.set_layout(placements);
        let mut out = Vec::new();
        wall.flush_to(&mut out, None, &bytes, &ctx).expect("flush");
        let text = String::from_utf8_lossy(&out);
        // Slot 39 is image id 41 (the wall's ids start at 2).
        assert!(
            wall.slots[39].as_ref().is_some_and(|slot| slot.placed),
            "the 40th visible card holds its poster"
        );
        assert!(
            text.contains("a=T,U=1,i=41"),
            "the last card is transferred under its own id: {}",
            &text[..text.len().min(300)]
        );
        assert!(
            text.contains("38;2;0;0;41"),
            "and its placeholder cells name that id: {}",
            &text[..text.len().min(300)]
        );
    }

    /// A tile is encoded to exactly the requested cell grid, under its own
    /// image id, and the transfer/placeholder/delete escapes name that id
    /// (kitty keys images by it).
    #[test]
    fn a_wall_tile_is_transferred_and_deleted_under_its_id() {
        let bytes = png_bytes(64, 96);
        let tile = kitty::create_wall_tile(&bytes, 10, 6, 7).expect("encode tile");
        assert_eq!(tile.cols, 10);
        assert_eq!(tile.rows, 6);
        assert_eq!(tile.id, 7);
        assert!(tile.width >= 16 && tile.height >= 16);

        let area = Rect::new(3, 4, 10, 6);
        let mut out = Vec::new();
        kitty::display_wall_tile(&mut out, &tile, area, None).expect("display");
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("a=T,U=1,i=7"), "{text}");
        assert!(text.contains("c=10,r=6"), "{text}");
        assert!(
            text.contains("38;2;0;0;7"),
            "placeholder cells carry the id as a 24-bit foreground: {text}"
        );
        assert!(text.contains("\u{10EEEE}"), "placeholder glyphs: {text}");

        let mut deleted = Vec::new();
        kitty::delete_wall_tile(&mut deleted, 7).expect("delete");
        let deleted = String::from_utf8_lossy(&deleted);
        assert!(deleted.contains("a=d,d=I,i=7"), "{deleted}");
    }

    /// The cover crop makes the tile fill its slot (no letterbox): the same
    /// portrait source encoded for a wide slot and for a tall one produces
    /// grids whose aspect follows the slots, not the source.
    #[test]
    fn a_tile_is_cover_cropped_to_the_card_shape() {
        let portrait = png_bytes(60, 90);
        let wide = kitty::create_wall_tile(&portrait, 12, 6, 3).expect("encode");
        let tall = kitty::create_wall_tile(&portrait, 8, 12, 4).expect("encode");
        let wide_ratio = f64::from(wide.width) / f64::from(wide.height);
        let tall_ratio = f64::from(tall.width) / f64::from(tall.height);
        // 12x6 is three times as wide (relative to its height) as 8x12.
        assert!(
            ((wide_ratio / tall_ratio) - 3.0).abs() < 0.05,
            "wide {wide_ratio} vs tall {tall_ratio}"
        );
    }
}
