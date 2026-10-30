//! One list of every surface that paints **outside** the ratatui buffer.
//!
//! Three kinds of thing write to the terminal behind ratatui's back: the album
//! art (kitty/iTerm2/sixel/ueberzug placements, or a block-glyph fallback), the
//! Jellyfin poster (plus the Home carousel's wall of tiles), and the cava bars
//! (a dedicated thread writing its own band). Each one owns cells the frame
//! buffer does not, which has three consequences the app has to act on:
//!
//! * a `terminal.clear()` wipes the cells and the image disappears, so every
//!   declared surface has to place again ([`crate::ui::Ui::refresh_overlays_after_clear`]);
//! * a frame diff that rewrites those cells deletes the placement, so the owner
//!   must re-place (each surface does that on its own terms — see
//!   `backend/image-overlays`);
//! This module is the truth for the first: it deliberately does not draw or
//! re-place anything, it is the registry the surfaces declare into and the
//! clear repair reads. Hit-testing ("who owns this cell?") belongs here too
//! when something needs it; today nothing does, so it is not implemented.

use ratatui::layout::Rect;

/// A surface that paints outside the frame buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Surface {
    /// The album-art pane's image (any backend).
    AlbumArt,
    /// The Jellyfin tab's poster overlay.
    Poster,
    /// The cava visualizer's band, drawn by its own thread.
    Cava,
}

/// The declared surfaces, in declaration order (the most recent last, which is
/// also the paint order: a later surface is on top).
#[derive(Debug, Default)]
pub(crate) struct Placements {
    entries: Vec<(Surface, Rect)>,
}

impl Placements {
    /// Declare where `surface` is painting. A second declaration of the same
    /// surface replaces the first — an overlay moves, it does not stack.
    pub(crate) fn declare(&mut self, surface: Surface, area: Rect) {
        if area.width == 0 || area.height == 0 {
            self.release(surface);
            return;
        }
        match self.entries.iter_mut().find(|(known, _)| *known == surface) {
            Some(entry) => entry.1 = area,
            None => self.entries.push((surface, area)),
        }
    }

    /// Forget `surface`: it is no longer on screen.
    pub(crate) fn release(&mut self, surface: Surface) {
        self.entries.retain(|(known, _)| *known != surface);
    }

    /// Where `surface` is painting, if it is.
    pub(crate) fn area(&self, surface: Surface) -> Option<Rect> {
        self.entries
            .iter()
            .find(|(known, _)| *known == surface)
            .map(|&(_, area)| area)
    }

    /// The declared surfaces, bottom to top.
    pub(crate) fn surfaces(&self) -> impl Iterator<Item = Surface> + '_ {
        self.entries.iter().map(|&(surface, _)| surface)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn nothing_is_declared_to_begin_with() {
        let placements = Placements::default();
        assert_eq!(placements.area(Surface::AlbumArt), None);
        assert_eq!(placements.surfaces().count(), 0);
    }

    #[test]
    fn a_declaration_is_recorded_and_can_be_found() {
        let mut placements = Placements::default();
        placements.declare(Surface::AlbumArt, area(10, 2, 20, 10));
        assert_eq!(placements.area(Surface::AlbumArt), Some(area(10, 2, 20, 10)));
        assert_eq!(placements.surfaces().count(), 1);
    }

    #[test]
    fn a_second_declaration_replaces_the_first() {
        // An overlay moves; it does not stack up one entry per frame.
        let mut placements = Placements::default();
        placements.declare(Surface::AlbumArt, area(10, 2, 20, 10));
        placements.declare(Surface::AlbumArt, area(12, 4, 18, 8));
        assert_eq!(placements.area(Surface::AlbumArt), Some(area(12, 4, 18, 8)));
        assert_eq!(placements.surfaces().count(), 1, "still one surface");
    }

    #[test]
    fn declaring_an_empty_rect_removes_the_surface() {
        let mut placements = Placements::default();
        placements.declare(Surface::Cava, area(0, 30, 80, 5));
        placements.declare(Surface::Cava, area(0, 30, 80, 0));
        assert_eq!(placements.area(Surface::Cava), None);
    }

    #[test]
    fn releasing_a_surface_forgets_it_and_leaves_the_others() {
        let mut placements = Placements::default();
        placements.declare(Surface::AlbumArt, area(0, 0, 10, 10));
        placements.declare(Surface::Cava, area(0, 30, 80, 5));
        placements.release(Surface::AlbumArt);
        assert_eq!(placements.area(Surface::AlbumArt), None);
        assert_eq!(placements.area(Surface::Cava), Some(area(0, 30, 80, 5)));
        placements.release(Surface::AlbumArt);
        assert_eq!(placements.surfaces().count(), 1, "releasing twice is a no-op");
    }
}
