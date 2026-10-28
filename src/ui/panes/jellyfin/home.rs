use std::{collections::HashSet, sync::Arc};

use super::*;
use crate::jellyfin::JfImageType;

/// How long a fetched Home page is considered fresh. A tab return inside
/// this window reuses the shelves (no request storm while moving between
/// tabs); a later return refreshes, so an item watched in the meantime
/// drops out of Continue Watching.
const HOME_REFRESH_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
/// Vertical space a card costs besides its image: 1 header row, 2 border
/// rows, 2 label rows and 1 blank row between sections.
const CARD_CHROME_ROWS: u16 = 6;
/// The narrowest a poster card may get before the terminal is considered too
/// narrow for images (the card then still shows its labels).
const MIN_IMAGE_W: u16 = 3;
/// The preferred tallest image of a Home card. A taller page does not grow
/// the posters: the spare rows go to the Libraries carousel (when all the
/// rows fit at this height) and then stay blank. 10 rows is what the three
/// carousels get on a normal-height terminal, so the cards keep the size
/// users already know when the terminal grows (2026-10-28: the posters used
/// to scale with the page until far too large).
pub(super) const MAX_IMAGE_H: u16 = 10;
/// A poster is ~2:3, a Jellyfin episode/backdrop preview ~16:9.
const POSTER_ASPECT: f64 = 2.0 / 3.0;
const PREVIEW_ASPECT: f64 = 16.0 / 9.0;

/// `12:34` for a duration in seconds (used by the progress subline).
pub(super) fn clock(secs: u64) -> String {
    crate::ui::panes::lyrics::format_clock(secs)
}

/// Which Home shelf a section is (drives the card shape and the image the
/// server is asked for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HomeShelf {
    /// The library views (the row above the shelves, revealed with Up).
    Libraries,
    ContinueWatching,
    NextUp,
    RecentlyAdded,
}
impl HomeShelf {
    fn title(self) -> &'static str {
        match self {
            Self::Libraries => "Libraries",
            Self::ContinueWatching => "Continue Watching",
            Self::NextUp => "Next Up",
            Self::RecentlyAdded => "Recently Added",
        }
    }
    /// Wide 16:9 episode/movie previews for what is in progress, 2:3 posters
    /// for the rest (the Jellyfin web UI's card shapes).
    fn aspect(self) -> f64 {
        match self {
            // Library tiles are wide browse cards, like the web UI's
            // "My Media" row.
            Self::Libraries | Self::ContinueWatching => PREVIEW_ASPECT,
            Self::NextUp | Self::RecentlyAdded => POSTER_ASPECT,
        }
    }
    /// The image to fetch for `item`: `(target id, kind, fallback id)`.
    fn image_target(self, item: &JfItem) -> (String, JfImageType, Option<String>) {
        match self {
            // A library view's own image (libraries often have none; the
            // card then shows its name).
            Self::Libraries => (item.id.clone(), JfImageType::Primary, None),
            // Resume cards show the episode still / movie backdrop; the
            // series poster is the last resort.
            Self::ContinueWatching => {
                let kind = if item.kind == "Episode" {
                    JfImageType::Thumb
                } else {
                    JfImageType::Backdrop
                };
                (item.id.clone(), kind, item.series_id.clone())
            }
            // Next Up shows the *series* poster, like the web UI.
            Self::NextUp => (
                item.series_id.clone().unwrap_or_else(|| item.id.clone()),
                JfImageType::Primary,
                None,
            ),
            // A recent *episode* is shown as its series' poster (the web UI
            // does the same); seasons and shows use their own artwork.
            Self::RecentlyAdded => {
                if item.kind == "Episode" {
                    (
                        item.series_id.clone().unwrap_or_else(|| item.id.clone()),
                        JfImageType::Primary,
                        None,
                    )
                } else {
                    (item.id.clone(), JfImageType::Primary, item.series_id.clone())
                }
            }
        }
    }
}

/// One shelf of the Home page: its items and its own cursor/scroll.
#[derive(Debug, Clone)]
pub(super) struct HomeSection {
    pub(super) shelf: HomeShelf,
    /// The row's header text: the shelf title, or `Recently Added in …` for
    /// one library's strip.
    pub(super) title: String,
    /// One library's own Recently Added strip rather than one of the three
    /// server shelves. Per-library strips are extra rows: the card size is
    /// derived from the base shelves, so they scroll instead of shrinking
    /// every poster.
    pub(super) library_strip: bool,
    pub(super) items: Vec<JfItem>,
    pub(super) cursor: usize,
    /// Index of the first card shown (the row scrolls horizontally).
    pub(super) scroll: u16,
}
impl HomeSection {
    fn cursor(&self) -> usize {
        self.cursor.min(self.items.len().saturating_sub(1))
    }
    fn selected(&self) -> Option<&JfItem> {
        self.items.get(self.cursor())
    }
}

/// The cursor a rebuilt row inherits from the previous sections, keyed by
/// its title (every Home row has a unique one), clamped to the new length.
fn restored_cursor(previous: &[(String, usize)], title: &str, len: usize) -> usize {
    previous
        .iter()
        .find(|(t, _)| t == title)
        .map_or(0, |(_, cursor)| *cursor)
        .min(len.saturating_sub(1))
}

/// Card geometry for one section at the current terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CardMetrics {
    pub(super) img_w: u16,
    pub(super) img_h: u16,
    pub(super) card_w: u16,
    pub(super) card_h: u16,
    pub(super) gap: u16,
    pub(super) visible: u16,
}

/// Identifies a rendered Home row: the Libraries carousel, or a shelf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HomeRowId {
    Libraries,
    Section(usize),
}

/// Which row the Home cursor is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HomeFocus {
    Libraries,
    Section(usize),
}

/// One card as painted: its rect (for clicks) and its image area (for the
/// poster wall).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HomeCard {
    pub(super) row: HomeRowId,
    pub(super) index: usize,
    pub(super) rect: Rect,
    pub(super) image: Rect,
}

/// What one rendered row shows (owned, so a row can be painted while the
/// pane's caches are borrowed).
#[derive(Debug, Clone)]
struct HomeRowView {
    shelf: HomeShelf,
    title: String,
    items: Vec<JfItem>,
    cursor: usize,
    scroll: u16,
}

/// The card grid painted by the last Home render.
#[derive(Debug, Default, Clone)]
pub(super) struct HomeLayout {
    pub(super) cards: Vec<HomeCard>,
    /// `(header rect, card rows rect)` per visible section.
    pub(super) sections: Vec<(Rect, Rect)>,
    /// The carousel arrows drawn on the header row: `(rect, row, dir)`.
    pub(super) arrows: Vec<(Rect, HomeRowId, i64)>,
}

/// Keep scroll such that the cursor stays visible, moving as little as
/// possible (the carousel scrolls one card at a time at the edges).
pub(super) fn visible_offset(scroll: u16, cursor: usize, len: usize, visible: u16) -> u16 {
    let visible = visible.max(1) as usize;
    if len <= visible {
        return 0;
    }
    let max = (len - visible) as u16;
    let scroll = scroll.min(max);
    let cursor = cursor.min(len.saturating_sub(1));
    if cursor < usize::from(scroll) {
        cursor as u16
    } else if cursor >= usize::from(scroll) + visible {
        (cursor + 1 - visible) as u16
    } else {
        scroll
    }
}

/// Card width for a row, from the terminal width, the shared image height
/// and the terminal's cell aspect. Pure so the layout can be tested without a
/// terminal.
pub(super) fn card_metrics(
    shelf: HomeShelf,
    area_width: u16,
    img_h: u16,
    cell_ratio: f64,
) -> CardMetrics {
    let mut img_w = ((shelf.aspect() * f64::from(img_h) * cell_ratio).round() as u16)
        .max(MIN_IMAGE_W);
    // Never let one card exceed the pane (leave room for the border).
    let max_w = area_width.saturating_sub(4).max(MIN_IMAGE_W + 2);
    if img_w + 2 > max_w {
        img_w = max_w.saturating_sub(2).max(2);
    }
    let card_w = img_w + 2;
    let gap = 1;
    let visible = ((area_width + gap) / (card_w + gap)).max(1);
    CardMetrics {
        img_w,
        img_h,
        card_w,
        card_h: img_h + 4,
        gap,
        visible,
    }
}

/// How wide the `‹`/`›` gutters are: the glyph plus one blank column, so an
/// arrow never sits on a card's border.
const ARROW_GUTTER: u16 = 2;
/// Below this width a row shows no arrows at all (the cards need the room).
const MIN_ARROW_ROW_W: u16 = 20;

/// Where one row's cards sit and whether its arrows are shown. The arrows
/// live in the row's own margins, vertically centred on the cards: `‹` in a
/// left gutter that appears once the row has scrolled back, `›` in the spare
/// columns after the last card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RowGeometry {
    pub(super) metrics: CardMetrics,
    /// Columns reserved on the left/right of the cards for the arrows.
    pub(super) left: u16,
    pub(super) right: u16,
    /// The first card drawn (recomputed for the reserved gutters).
    pub(super) scroll: u16,
    pub(super) can_left: bool,
    pub(super) can_right: bool,
}

/// The card geometry, the reserved arrow gutters and the scroll offset of one
/// row. Pure so the layout can be tested without a terminal: a gutter shrinks
/// the grid, which can change how many cards fit, so the offset is recomputed
/// after each reservation (reserving space never *lowers* it).
#[allow(clippy::too_many_arguments)]
pub(super) fn row_geometry(
    shelf: HomeShelf,
    area_width: u16,
    img_h: u16,
    cell_ratio: f64,
    len: usize,
    cursor: usize,
    scroll: u16,
) -> RowGeometry {
    let arrows = area_width > MIN_ARROW_ROW_W;
    let mut left = 0;
    let mut right = 0;
    let mut metrics = card_metrics(shelf, area_width, img_h, cell_ratio);
    let mut offset = visible_offset(scroll, cursor, len, metrics.visible);
    // Cards beyond the right edge: keep the spare columns for the `›`.
    if arrows && len > usize::from(metrics.visible) {
        right = ARROW_GUTTER;
        metrics = card_metrics(shelf, area_width - right, img_h, cell_ratio);
        offset = visible_offset(scroll, cursor, len, metrics.visible);
    }
    // Scrolled back: reserve the left margin for the `‹`.
    if arrows && offset > 0 {
        left = ARROW_GUTTER;
        metrics = card_metrics(shelf, area_width - right - left, img_h, cell_ratio);
        offset = visible_offset(scroll, cursor, len, metrics.visible);
    }
    RowGeometry {
        metrics,
        left,
        right,
        scroll: offset,
        // No gutter, no arrow: the keyboard still scrolls the row.
        can_left: left > 0 && offset > 0,
        can_right: right > 0 && usize::from(offset) + usize::from(metrics.visible) < len,
    }
}

/// The vertical split of the Home page: the image height every section
/// shares, and the per-section slot height (header + card + gap). The image
/// height stops at [`MAX_IMAGE_H`], so a tall page uses its spare rows for
/// another carousel instead of inflating the posters. When the sections do
/// not all fit, the caller scrolls them vertically.
pub(super) fn section_geometry(area_height: u16, sections: usize) -> (u16, u16) {
    let sections = sections.max(1) as u16;
    let slot_h = (area_height / sections).max(8);
    let img_h = slot_h.saturating_sub(CARD_CHROME_ROWS).clamp(2, MAX_IMAGE_H);
    (img_h, img_h + CARD_CHROME_ROWS)
}

/// Whether the Libraries row can stay on the page **without scrolling or
/// shrinking the cards**: every row (the base shelves, one strip per
/// library, and the Libraries carousel itself) must fit at the preferred
/// image height. With more rows the carousel goes back to being the
/// `↑ Libraries` hint, revealed with Up; a small server whose whole page
/// fits keeps it visible.
pub(super) fn libraries_row_fits(
    area_height: u16,
    base_shelves: usize,
    total_rows: usize,
) -> bool {
    if base_shelves == 0 || total_rows == 0 {
        return false;
    }
    let (img_h, section_h) = section_geometry(area_height, base_shelves);
    img_h >= MAX_IMAGE_H
        && (total_rows + 1).saturating_mul(usize::from(section_h))
            <= usize::from(area_height)
}

/// The terminal's cell height/width ratio, for sizing poster cards so their
/// pixel aspect matches the artwork. Falls back to a typical 2.25 (8x18 px
/// cells) when the terminal does not report pixel sizes.
pub(super) fn detect_cell_ratio() -> f64 {
    let Ok(ws) = crossterm::terminal::window_size() else {
        return 2.25;
    };
    if ws.columns == 0 || ws.rows == 0 || ws.width == 0 || ws.height == 0 {
        return 2.25;
    }
    let cell_w = f64::from(ws.width) / f64::from(ws.columns);
    let cell_h = f64::from(ws.height) / f64::from(ws.rows);
    (cell_h / cell_w).clamp(1.2, 3.0)
}

impl JellyfinPane {
    /// Fetch the three Home shelves (the loading indicator on the toggle row
    /// tracks this fetch like the library ones).
    pub(super) fn fetch_home(&mut self, ctx: &Ctx) {
        if self.load_server(ctx).is_none() {
            return;
        }
        self.home_fetched_at = Some(std::time::Instant::now());
        self.request_fetch(ctx, WorkRequest::FetchJellyfinHome);
    }
    /// Fetch the shelves when they are missing or stale.
    pub(super) fn refresh_home_if_stale(&mut self, ctx: &Ctx) {
        let stale = self
            .home_fetched_at
            .is_none_or(|at| at.elapsed() >= HOME_REFRESH_AFTER);
        if stale {
            self.fetch_home(ctx);
        }
    }
    /// Rebuild the sections from the fetched shelves, keeping each row's
    /// cursor when it still exists. The three server shelves come first,
    /// then one "Recently Added in <library>" strip per library with recent
    /// items.
    pub(super) fn rebuild_home_sections(&mut self) {
        let previous: Vec<(String, usize)> = self
            .home_sections
            .iter()
            .map(|section| (section.title.clone(), section.cursor()))
            .collect();
        let shelves: [(HomeShelf, &Vec<JfItem>); 3] = [
            (HomeShelf::ContinueWatching, &self.home.continue_watching),
            (HomeShelf::NextUp, &self.home.next_up),
            (HomeShelf::RecentlyAdded, &self.home.recently_added),
        ];
        let mut sections = Vec::new();
        for (shelf, items) in shelves {
            if items.is_empty() {
                continue;
            }
            let title = shelf.title().to_owned();
            sections.push(HomeSection {
                shelf,
                cursor: restored_cursor(&previous, &title, items.len()),
                title,
                library_strip: false,
                items: items.clone(),
                scroll: 0,
            });
        }
        for latest in &self.home.library_latest {
            if latest.items.is_empty() {
                continue;
            }
            let title = format!("Recently Added in {}", latest.library.name);
            sections.push(HomeSection {
                // A library strip is the same kind of row as the server's
                // Recently Added shelf: 2:3 posters of the newest items.
                shelf: HomeShelf::RecentlyAdded,
                cursor: restored_cursor(&previous, &title, latest.items.len()),
                title,
                library_strip: true,
                items: latest.items.clone(),
                scroll: 0,
            });
        }
        self.home_sections = sections;
        if self.home_sections.is_empty() {
            self.home_focus = HomeFocus::Section(0);
        } else if let HomeFocus::Section(idx) = self.home_focus {
            self.home_focus = HomeFocus::Section(idx.min(self.home_sections.len() - 1));
        }
        if self.views.is_empty() {
            self.home_libraries_revealed = false;
            self.home_libraries_fits = false;
        }
        self.home_libraries_cursor = self
            .home_libraries_cursor
            .min(self.views.len().saturating_sub(1));
        // A refresh can change what is on screen: drop the failed-image
        // memory so retried items are fetched again.
        self.home_image_failed.clear();
    }
    /// How many rows belong to the three server shelves (not one library's
    /// strip). The card size and the Libraries-row fit are derived from
    /// these: extra per-library rows scroll instead of shrinking the cards.
    fn home_base_shelf_count(&self) -> usize {
        self.home_sections
            .iter()
            .filter(|section| !section.library_strip)
            .count()
    }
    /// Whether the Libraries carousel is part of the page right now: once
    /// the user pressed Up from the first shelf, or already when the page is
    /// tall enough to paint it at the cards' preferred size (it is otherwise
    /// just a hint line).
    fn libraries_row_shown(&self) -> bool {
        (self.home_libraries_revealed || self.home_libraries_fits)
            && !self.views.is_empty()
            && !self.home_sections.is_empty()
    }
    /// The rows in paint order: Libraries (when revealed) then the shelves.
    fn home_row_ids(&self) -> Vec<HomeRowId> {
        let mut rows = Vec::with_capacity(self.home_sections.len() + 1);
        if self.libraries_row_shown() {
            rows.push(HomeRowId::Libraries);
        }
        rows.extend((0..self.home_sections.len()).map(HomeRowId::Section));
        rows
    }
    fn home_focus_clamped(&self) -> HomeFocus {
        match self.home_focus {
            HomeFocus::Libraries if self.libraries_row_shown() => HomeFocus::Libraries,
            HomeFocus::Libraries => HomeFocus::Section(0),
            HomeFocus::Section(idx) => {
                HomeFocus::Section(idx.min(self.home_sections.len().saturating_sub(1)))
            }
        }
    }
    /// The focus as an index into [`Self::home_row_ids`].
    fn home_focus_row(&self) -> usize {
        match self.home_focus_clamped() {
            HomeFocus::Libraries => 0,
            HomeFocus::Section(idx) => idx + usize::from(self.libraries_row_shown()),
        }
    }
    fn home_set_focus_row(&mut self, row: usize) {
        if self.home_sections.is_empty() {
            return;
        }
        if self.libraries_row_shown() {
            self.home_focus = if row == 0 {
                HomeFocus::Libraries
            } else {
                HomeFocus::Section((row - 1).min(self.home_sections.len() - 1))
            };
        } else {
            self.home_focus = HomeFocus::Section(row.min(self.home_sections.len() - 1));
        }
    }
    /// Focus one row directly (a click on its header or an arrow). Moving
    /// from the Libraries row down to a shelf hides the Libraries row again,
    /// like the keyboard's Down.
    fn home_set_focus_id(&mut self, id: HomeRowId) {
        match id {
            HomeRowId::Libraries => {
                if self.libraries_row_shown() {
                    self.home_focus = HomeFocus::Libraries;
                }
            }
            HomeRowId::Section(idx) => {
                if idx < self.home_sections.len() {
                    if matches!(self.home_focus_clamped(), HomeFocus::Libraries) {
                        self.home_libraries_revealed = false;
                    }
                    self.home_focus = HomeFocus::Section(idx);
                }
            }
        }
    }
    /// The data of one row (owned; the cursor/scroll are clamped to what
    /// exists).
    fn home_row_view(&self, id: HomeRowId) -> Option<HomeRowView> {
        match id {
            HomeRowId::Libraries => {
                if !self.libraries_row_shown() {
                    return None;
                }
                Some(HomeRowView {
                    shelf: HomeShelf::Libraries,
                    title: HomeShelf::Libraries.title().to_owned(),
                    items: self.views.clone(),
                    cursor: self
                        .home_libraries_cursor
                        .min(self.views.len().saturating_sub(1)),
                    scroll: self.home_libraries_scroll,
                })
            }
            HomeRowId::Section(idx) => self.home_sections.get(idx).map(|section| {
                HomeRowView {
                    shelf: section.shelf,
                    title: section.title.clone(),
                    items: section.items.clone(),
                    cursor: section.cursor(),
                    scroll: section.scroll,
                }
            }),
        }
    }
    fn home_row_item(&self, id: HomeRowId, index: usize) -> Option<&JfItem> {
        match id {
            HomeRowId::Libraries => self.views.get(index),
            HomeRowId::Section(idx) => self.home_sections.get(idx)?.items.get(index),
        }
    }
    fn home_row_cursor(&self, id: HomeRowId) -> usize {
        match id {
            HomeRowId::Libraries => self
                .home_libraries_cursor
                .min(self.views.len().saturating_sub(1)),
            HomeRowId::Section(idx) => self
                .home_sections
                .get(idx)
                .map_or(0, HomeSection::cursor),
        }
    }
    fn home_row_len(&self, id: HomeRowId) -> usize {
        match id {
            HomeRowId::Libraries => self.views.len(),
            HomeRowId::Section(idx) => {
                self.home_sections.get(idx).map_or(0, |section| section.items.len())
            }
        }
    }
    fn set_home_row_cursor(&mut self, id: HomeRowId, cursor: usize) {
        match id {
            HomeRowId::Libraries => {
                self.home_libraries_cursor = cursor.min(self.views.len().saturating_sub(1));
            }
            HomeRowId::Section(idx) => {
                if let Some(section) = self.home_sections.get_mut(idx) {
                    section.cursor = cursor.min(section.items.len().saturating_sub(1));
                }
            }
        }
    }
    fn set_home_row_scroll(&mut self, id: HomeRowId, scroll: u16) {
        match id {
            HomeRowId::Libraries => self.home_libraries_scroll = scroll,
            HomeRowId::Section(idx) => {
                if let Some(section) = self.home_sections.get_mut(idx) {
                    section.scroll = scroll;
                }
            }
        }
    }
    fn home_row_scroll(&self, id: HomeRowId) -> u16 {
        match id {
            HomeRowId::Libraries => self.home_libraries_scroll,
            HomeRowId::Section(idx) => {
                self.home_sections.get(idx).map_or(0, |section| section.scroll)
            }
        }
    }
    /// How many of a row's cards the last frame drew (the carousel's window).
    fn home_row_visible(&self, id: HomeRowId) -> usize {
        self.home_layout
            .cards
            .iter()
            .filter(|card| card.row == id)
            .count()
    }
    /// The item under the Home cursor (a library tile or a shelf card).
    pub(super) fn home_selected_item(&self) -> Option<JfItem> {
        let focus = self.home_focus_clamped();
        let id = match focus {
            HomeFocus::Libraries => HomeRowId::Libraries,
            HomeFocus::Section(idx) => HomeRowId::Section(idx),
        };
        self.home_row_item(id, self.home_row_cursor(id)).cloned()
    }
    /// Move the focused row's card cursor by `dir` (horizontal carousel
    /// movement).
    pub(super) fn home_move_card(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let focus = self.home_focus_clamped();
        let id = match focus {
            HomeFocus::Libraries => HomeRowId::Libraries,
            HomeFocus::Section(idx) => HomeRowId::Section(idx),
        };
        let len = self.home_row_len(id);
        if len == 0 {
            return Ok(());
        }
        let current = self.home_row_cursor(id) as i64;
        let next = (current + dir).clamp(0, len.saturating_sub(1) as i64) as usize;
        if next == current as usize {
            return Ok(());
        }
        self.set_home_row_cursor(id, next);
        ctx.render()?;
        Ok(())
    }
    /// Step a carousel by one window, for the on-screen `‹`/`›` buttons: the
    /// row scrolls one card and the selection jumps to the card that just
    /// came into view at that edge. A click therefore *reveals the next
    /// hidden item* instead of walking the selection through the cards that
    /// are already on screen.
    pub(super) fn home_page_row(&mut self, id: HomeRowId, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.home_row_len(id);
        let visible = self.home_row_visible(id).max(1);
        if len <= visible {
            return Ok(());
        }
        let max = len - visible;
        let scroll = usize::from(self.home_row_scroll(id)).min(max);
        let next_scroll = if dir < 0 {
            scroll.saturating_sub(1)
        } else {
            (scroll + 1).min(max)
        };
        if next_scroll == scroll {
            return Ok(());
        }
        let next_cursor = if dir < 0 {
            next_scroll
        } else {
            (next_scroll + visible - 1).min(len - 1)
        };
        self.set_home_row_scroll(id, u16::try_from(next_scroll).unwrap_or(u16::MAX));
        self.set_home_row_cursor(id, next_cursor);
        ctx.render()?;
        Ok(())
    }
    /// Move the focus between rows (vertical movement). Up from the first
    /// shelf reveals the Libraries carousel.
    pub(super) fn home_move_section(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let rows = self.home_row_ids();
        if rows.is_empty() {
            return Ok(());
        }
        let current = self.home_focus_row();
        // Up from the first shelf, with the Libraries row still hidden:
        // reveal it and land on it.
        if dir < 0
            && !self.libraries_row_shown()
            && matches!(self.home_focus_clamped(), HomeFocus::Section(0))
            && !self.views.is_empty()
        {
            self.home_libraries_revealed = true;
            self.home_focus = HomeFocus::Libraries;
            ctx.render()?;
            return Ok(());
        }
        // Down from the Libraries row hides it again: it collapses back to
        // the `↑ Libraries` hint and the first shelf takes the focus.
        if dir > 0 && matches!(self.home_focus_clamped(), HomeFocus::Libraries) {
            self.home_libraries_revealed = false;
            self.home_focus = HomeFocus::Section(0);
            ctx.render()?;
            return Ok(());
        }
        let target = (current as i64 + dir).clamp(0, rows.len() as i64 - 1) as usize;
        if target == current {
            return Ok(());
        }
        self.home_set_focus_row(target);
        ctx.render()?;
        Ok(())
    }
    /// Select one card (mouse click) and focus its row.
    pub(super) fn home_select_card(
        &mut self,
        row: HomeRowId,
        index: usize,
        ctx: &Ctx,
    ) -> Result<()> {
        if index >= self.home_row_len(row) {
            return Ok(());
        }
        self.set_home_row_cursor(row, index);
        self.home_set_focus_id(row);
        ctx.render()?;
        Ok(())
    }
    /// The card under a terminal position (from the last render).
    fn home_card_at(&self, position: ratatui::layout::Position) -> Option<HomeCard> {
        self.home_layout
            .cards
            .iter()
            .find(|card| card.rect.contains(position))
            .cloned()
    }
    /// Activate the focused card: a movie/episode plays, a song plays through
    /// MPD, a series/season opens in the tree, a **library** opens the tree on
    /// that library.
    pub(super) fn home_activate(&mut self, ctx: &Ctx) -> Result<()> {
        match self.home_focus_clamped() {
            HomeFocus::Libraries => {
                let cursor = self.home_row_cursor(HomeRowId::Libraries);
                let Some(view) = self.views.get(cursor).cloned() else {
                    return Ok(());
                };
                self.open_library(view, ctx)
            }
            HomeFocus::Section(idx) => {
                let Some(item) = self
                    .home_sections
                    .get(idx)
                    .and_then(HomeSection::selected)
                    .cloned()
                else {
                    return Ok(());
                };
                match item.kind.as_str() {
                    "Audio" => {
                        if let Some(url) = self.stream_url(&item) {
                            self.play_temp_url(
                                ctx,
                                JF_PLAY,
                                PaneType::Jellyfin {
                                    tree: TreeBrowserArgs::default(),
                                },
                                item.name.clone(),
                                url,
                            );
                            status_info!("Playing {}", item.name);
                        }
                        Ok(())
                    }
                    "Movie" | "Episode" | "Video" => {
                        let Some(url) = self.stream_url(&item) else { return Ok(()) };
                        Self::play_video(
                            ctx,
                            url,
                            item.name.clone(),
                            item.id.clone(),
                            item.season_id.clone(),
                        )
                    }
                    _ => self.start_jump(item, false, None, None, ctx),
                }
            }
        }
    }
    /// Open the library tree on one library view (a Libraries card).
    fn open_library(&mut self, view: JfItem, ctx: &Ctx) -> Result<()> {
        if self.mode == JellyfinTabMode::Home {
            self.wall.hide(ctx);
        }
        self.mode = JellyfinTabMode::Libraries;
        self.release_search_input(ctx);
        let kind = self.node_kind_for(&view);
        self.expanded.insert(kind.key());
        self.selected = Some(kind.clone());
        self.ensure_loaded(&kind, ctx);
        self.rebuild_tree();
        self.populate_items();
        if let Some(idx) = self
            .tree
            .iter()
            .position(|node| node.kind.key() == kind.key())
        {
            self.tree_list.select(Some(idx));
        }
        self.scroll_tree_selection_into_view(ctx);
        self.poster.hide(ctx);
        self.sync_poster(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Home-mode keys. `w`/`s` (and Up/Down) move between rows; `a`/`d`
    /// (and Left/Right) move the carousel; Enter activates the focused card.
    pub(super) fn handle_home_action(
        &mut self,
        event: &mut ActionEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        if let Some(action) = event.claim_directories() {
            match action {
                DirectoriesActions::FolderUp => {
                    self.home_move_section(-1, ctx)?;
                    return Ok(());
                }
                DirectoriesActions::FolderDown => {
                    self.home_move_section(1, ctx)?;
                    return Ok(());
                }
                DirectoriesActions::FolderCollapse => {
                    self.home_move_card(-1, ctx)?;
                    return Ok(());
                }
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    self.home_move_card(1, ctx)?;
                    return Ok(());
                }
            }
        }
        if let Some(action) = event.claim_common() {
            match action {
                CommonAction::Up | CommonAction::Down => {
                    let dir = if matches!(action, CommonAction::Up) { -1 } else { 1 };
                    self.home_move_section(dir, ctx)?;
                    return Ok(());
                }
                CommonAction::Left => {
                    self.home_move_card(-1, ctx)?;
                    return Ok(());
                }
                CommonAction::Right => {
                    self.home_move_card(1, ctx)?;
                    return Ok(());
                }
                CommonAction::Top => {
                    self.home_libraries_revealed |= !self.views.is_empty();
                    if let Some(first) = self.home_row_ids().first().copied() {
                        self.home_set_focus_id(first);
                        self.set_home_row_cursor(first, 0);
                    }
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Bottom => {
                    if let Some(last) = self.home_row_ids().last().copied() {
                        self.home_set_focus_id(last);
                        let len = self.home_row_len(last);
                        self.set_home_row_cursor(last, len.saturating_sub(1));
                    }
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Confirm => {
                    self.home_activate(ctx)?;
                    return Ok(());
                }
                CommonAction::ContextMenu => {
                    self.open_context_menu(ctx, None)?;
                    return Ok(());
                }
                CommonAction::ShowInfo => {
                    if !self.show_info(ctx)? {
                        event.abandon();
                    }
                    return Ok(());
                }
                _ => event.abandon(),
            }
        }
        Ok(())
    }
    /// Home-mode mouse: click selects a card (and focuses its row), the
    /// margin arrows page a carousel one window, double-click activates, the
    /// wheel moves between rows like Up/Down, right-click opens the item
    /// menu.
    pub(super) fn handle_home_mouse(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<bool> {
        let position: ratatui::layout::Position = event.into();
        // The carousel arrows win over a card hit (they sit on the header
        // row, above the cards).
        if matches!(
            event.kind,
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
        ) {
            let arrow = self
                .home_layout
                .arrows
                .iter()
                .find(|(rect, _, _)| rect.contains(position))
                .copied();
            if let Some((_, row, dir)) = arrow {
                self.home_set_focus_id(row);
                self.home_page_row(row, dir, ctx)?;
                return Ok(true);
            }
        }
        let card = self.home_card_at(position);
        let row_at = self
            .home_layout
            .sections
            .iter()
            .position(|(header, cards)| cards.contains(position) || header.contains(position))
            .and_then(|idx| self.home_row_ids().get(idx).copied());
        match event.kind {
            MouseEventKind::LeftClick => {
                if let Some(card) = card {
                    self.home_select_card(card.row, card.index, ctx)?;
                } else if let Some(row) = row_at {
                    self.home_set_focus_id(row);
                    ctx.render()?;
                }
            }
            MouseEventKind::DoubleClick => {
                if let Some(card) = card {
                    self.home_select_card(card.row, card.index, ctx)?;
                    self.home_activate(ctx)?;
                }
            }
            MouseEventKind::RightClick => {
                if let Some(card) = card {
                    self.home_select_card(card.row, card.index, ctx)?;
                    self.open_context_menu(ctx, Some(position))?;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // The wheel moves between rows exactly like Up/Down; the
                // horizontal movement belongs to the row arrows and `a`/`d`.
                let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                    -1
                } else {
                    1
                };
                self.home_move_section(dir, ctx)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    /// The Home page: the Libraries carousel (revealed with Up) over the
    /// shelf carousels, no info box.
    pub(super) fn render_home(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &Ctx,
    ) -> Result<()> {
        let mut layout = HomeLayout::default();
        if self.home_sections.is_empty() {
            let text = if self.error.is_some() {
                " Could not load the server's home page."
            } else {
                " Loading your library…"
            };
            frame.render_widget(
                Paragraph::new(text).style(ctx.config.as_list_text_style()),
                Rect { y: area.y, height: 1, ..area },
            );
            self.home_layout = layout;
            self.wall.set_layout(Vec::new());
            return Ok(());
        }
        let cell_ratio = self.home_cell_ratio.unwrap_or_else(detect_cell_ratio);
        let base = ctx.config.as_list_text_style();
        let dim = base.add_modifier(Modifier::DIM);
        // The card size comes from the base shelves (Continue Watching, Next
        // Up, Recently Added), not from the per-library strips: more rows
        // scroll instead of shrinking every poster. The Libraries row stays
        // on the page only when every row still fits at that preferred size
        // (a small server); with more rows it is the `↑ Libraries` hint
        // again, so the visible page is all content. The fit is measured on
        // the full content area because a fitting row needs no hint line
        // (and the hint would be dropped anyway).
        let base_shelves = self.home_base_shelf_count();
        self.home_libraries_fits = !self.views.is_empty()
            && libraries_row_fits(area.height, base_shelves, self.home_sections.len());
        let rows = self.home_row_ids();
        let mut usable = area;
        // The Libraries row is hidden on a short page until Up: a one-line
        // hint says so.
        if !self.libraries_row_shown() && !self.views.is_empty() && area.height > 2 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("   ↑ {}", HomeShelf::Libraries.title()),
                    dim,
                ))),
                Rect { x: area.x, y: area.y, width: area.width, height: 1 },
            );
            usable = Rect {
                y: area.y + 1,
                height: area.height - 1,
                ..area
            };
        }
        let (img_h, section_h) = section_geometry(usable.height, base_shelves);
        let visible_rows = (usable.height / section_h).max(1) as usize;
        let focused_row = self.home_focus_row().min(rows.len().saturating_sub(1));
        let section_scroll = visible_offset(
            u16::try_from(self.home_section_scroll).unwrap_or(u16::MAX),
            focused_row,
            rows.len(),
            u16::try_from(visible_rows).unwrap_or(u16::MAX),
        ) as usize;
        self.home_section_scroll = section_scroll;
        for line in 0..visible_rows {
            let row_idx = section_scroll + line;
            let Some(row) = rows.get(row_idx).copied() else {
                break;
            };
            let y = usable.y + line as u16 * section_h;
            if y >= usable.bottom() {
                break;
            }
            self.render_home_row(
                frame,
                ctx,
                row,
                y,
                usable,
                img_h,
                cell_ratio,
                row_idx == focused_row,
                &mut layout,
            );
        }
        // The wall's slots follow the cards' render order and are numbered
        // across all rows: the same slot keeps the same kitty image id while
        // a row scrolls. Requests go focused-row first.
        let mut placements: Vec<crate::ui::image::wall::WallPlacement> = Vec::new();
        // The cards' images are terminal-side overlays placed *after* the
        // frame's flush, so they would be painted on top of a modal (the
        // right-click menu, info, confirm…) drawn over them in the buffer.
        // While a modal is up the wall stays empty; `ModalClosed` re-paints.
        if self.wall.is_enabled() && !self.is_modal_open {
            let mut tiles: Vec<(String, String, JfImageType, Option<String>, Rect)> =
                Vec::with_capacity(layout.cards.len());
            for card in &layout.cards {
                let Some(item) = self.home_row_item(card.row, card.index) else {
                    continue;
                };
                let shelf = match card.row {
                    HomeRowId::Libraries => HomeShelf::Libraries,
                    HomeRowId::Section(idx) => self
                        .home_sections
                        .get(idx)
                        .map_or(HomeShelf::RecentlyAdded, |section| section.shelf),
                };
                let (target, kind, fallback) = shelf.image_target(item);
                tiles.push((
                    format!("{kind:?}:{target}"),
                    target,
                    kind,
                    fallback,
                    card.image,
                ));
            }
            let focused_id = match self.home_focus_clamped() {
                HomeFocus::Libraries => HomeRowId::Libraries,
                HomeFocus::Section(idx) => HomeRowId::Section(idx),
            };
            let mut request_order: Vec<usize> = (0..tiles.len())
                .filter(|idx| {
                    layout
                        .cards
                        .get(*idx)
                        .is_some_and(|card| card.row == focused_id)
                })
                .collect();
            request_order.extend(
                (0..tiles.len()).filter(|idx| {
                    !layout
                        .cards
                        .get(*idx)
                        .is_some_and(|card| card.row == focused_id)
                }),
            );
            for i in request_order {
                let (tag, target, kind, fallback, _) = &tiles[i];
                if !self.home_images.contains_key(tag)
                    && !self.home_image_failed.contains(tag)
                    && self.home_image_requests.insert(tag.clone())
                {
                    self.request_fetch(
                        ctx,
                        WorkRequest::FetchJellyfinImageTagged {
                            tag: tag.clone(),
                            item_id: target.clone(),
                            fallback_item_id: fallback.clone(),
                            image_type: *kind,
                        },
                    );
                }
            }
            for (slot, (tag, _, _, _, image_area)) in tiles.into_iter().enumerate() {
                placements.push(crate::ui::image::wall::WallPlacement {
                    slot: u16::try_from(slot).unwrap_or(u16::MAX),
                    tag,
                    area: image_area,
                });
            }
        }
        self.wall.set_layout(placements);
        self.home_layout = layout;
        Ok(())
    }
    /// Paint one row: its header (title, range, scroll arrows) and its cards.
    #[allow(clippy::too_many_arguments)]
    fn render_home_row(
        &mut self,
        frame: &mut Frame,
        ctx: &Ctx,
        row: HomeRowId,
        y: u16,
        area: Rect,
        img_h: u16,
        cell_ratio: f64,
        focused: bool,
        layout: &mut HomeLayout,
    ) {
        let Some(view) = self.home_row_view(row) else {
            return;
        };
        let items_len = view.items.len();
        let geometry = row_geometry(
            view.shelf,
            area.width,
            img_h,
            cell_ratio,
            items_len,
            view.cursor,
            view.scroll,
        );
        let metrics = geometry.metrics;
        let visible = metrics.visible;
        let scroll = geometry.scroll;
        self.set_home_row_scroll(row, scroll);
        let name_style = ctx.config.as_list_name_style();
        let base = ctx.config.as_list_text_style();
        let dim = base.add_modifier(Modifier::DIM);
        let header_rect = Rect { x: area.x, y, width: area.width, height: 1 };
        let header_style = if focused {
            name_style.add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        let range = if items_len > usize::from(visible) {
            format!(
                "  ⟨{}-{} of {}⟩",
                scroll + 1,
                (scroll + visible).min(items_len as u16),
                items_len
            )
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" ── {} ({})", view.title, items_len),
                    header_style,
                ),
                Span::styled(range, dim),
            ])),
            header_rect,
        );
        let cards_rect = Rect {
            x: area.x,
            y: y + 1,
            width: area.width,
            height: metrics.card_h.min(area.bottom().saturating_sub(y + 1)),
        };
        layout.sections.push((header_rect, cards_rect));
        if cards_rect.height < 3 {
            return;
        }
        let border_style = ctx.config.as_border_style();
        let border_set = ctx.config.as_border_set();
        let selected_style = ctx.config.theme.hovered_item_style;
        // The cards start after the left arrow's gutter, and the blank space
        // after the last one belongs to the `›`.
        let content_x = area.x + geometry.left;
        let drawn = usize::from(visible).min(items_len.saturating_sub(usize::from(scroll)));
        let last_right = content_x.saturating_add(
            (u16::try_from(drawn).unwrap_or(u16::MAX))
                .saturating_sub(1)
                .saturating_mul(metrics.card_w + metrics.gap)
                .saturating_add(if drawn == 0 { 0 } else { metrics.card_w }),
        );
        let buffer = frame.buffer_mut();
        for index
            in usize::from(scroll)..items_len.min(usize::from(scroll) + usize::from(visible))
        {
            let column = (index - usize::from(scroll)) as u16;
            let x = content_x + column.saturating_mul(metrics.card_w + metrics.gap);
            if x + metrics.card_w > area.right() {
                break;
            }
            let box_rect = Rect {
                x,
                y: cards_rect.y,
                width: metrics.card_w,
                height: (metrics.img_h + 2)
                    .min(cards_rect.height)
                    .min(area.bottom().saturating_sub(cards_rect.y)),
            };
            if box_rect.height < 3 {
                continue;
            }
            let image_rect = Rect {
                x: box_rect.x + 1,
                y: box_rect.y + 1,
                width: metrics.img_w.min(box_rect.width.saturating_sub(2)),
                height: box_rect.height.saturating_sub(2),
            };
            let selected = focused && index == view.cursor;
            let style = if selected {
                selected_style
            } else if focused {
                border_style
            } else {
                border_style.add_modifier(Modifier::DIM)
            };
            ratatui::widgets::Widget::render(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(border_set)
                    .border_style(style),
                box_rect,
                buffer,
            );
            for cy in image_rect.top()..image_rect.bottom() {
                for cx in image_rect.left()..image_rect.right() {
                    if let Some(cell) = buffer.cell_mut((cx, cy)) {
                        cell.set_symbol(" ").set_style(base);
                    }
                }
            }
            let item = &view.items[index];
            let image_pending = self.wall.is_enabled() && {
                let (target, kind, _) = view.shelf.image_target(item);
                !self.home_images.contains_key(&format!("{kind:?}:{target}"))
            };
            if (!self.wall.is_enabled() || image_pending)
                && image_rect.height >= 1
                && image_rect.width >= 4
            {
                let label = crate::shared::string_util::truncate_chars(
                    &home_card_title(item),
                    usize::from(image_rect.width.saturating_sub(2)),
                );
                let mid = image_rect.y + image_rect.height / 2;
                buffer.set_string(image_rect.x + 1, mid, &label, dim);
            }
            layout.cards.push(HomeCard {
                row,
                index,
                rect: Rect {
                    height: box_rect.height + 2,
                    ..box_rect
                },
                image: image_rect,
            });
            let label_y = box_rect.bottom();
            let label_style = if selected {
                selected_style.add_modifier(Modifier::BOLD)
            } else if focused {
                name_style
            } else {
                dim
            };
            if label_y >= area.bottom() {
                continue;
            }
            let marker = if selected { "▸ " } else { "  " };
            let name = crate::shared::string_util::truncate_chars(
                &home_card_title(item),
                usize::from(box_rect.width.saturating_sub(4)),
            );
            buffer.set_string(
                box_rect.x + 1,
                label_y,
                &format!("{marker}{name}"),
                label_style,
            );
            if label_y + 1 < area.bottom() {
                let subline = crate::shared::string_util::truncate_chars(
                    &home_subline(item),
                    usize::from(box_rect.width.saturating_sub(2)),
                );
                buffer.set_string(
                    box_rect.x + 1,
                    label_y + 1,
                    &format!("  {subline}"),
                    dim,
                );
            }
        }
        // The arrows sit in the row's margins, on the cards' middle row: the
        // `‹` in the left gutter, the `›` in the blank space after the last card.
        let arrow_y = cards_rect.y + (metrics.img_h + 2).min(cards_rect.height) / 2;
        let arrow_base = if focused {
            name_style.add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        let mouse = ctx.mouse_pos();
        let mut arrow =
            |layout: &mut HomeLayout, strip: Rect, at: u16, glyph: &str, dir: i64| {
                let style = if mouse.is_some_and(|pos| strip.contains(pos)) {
                    crate::config::hover_style(arrow_base)
                } else {
                    arrow_base
                };
                buffer.set_string(at, arrow_y, glyph, style);
                layout.arrows.push((strip, row, dir));
            };
        if geometry.can_left {
            let strip = Rect {
                x: area.x,
                y: cards_rect.y,
                width: geometry.left,
                height: cards_rect.height,
            };
            arrow(layout, strip, strip.x, "‹", -1);
        }
        if geometry.can_right && last_right < area.right() {
            let strip = Rect {
                x: last_right,
                y: cards_rect.y,
                width: area.right().saturating_sub(last_right),
                height: cards_rect.height,
            };
            arrow(layout, strip, strip.right().saturating_sub(1), "›", 1);
        }
    }
}

/// The big label of a Home card. An episode is shown under its **series**
/// name (the web UI's cards do the same): the series poster/backdrop is what
/// the card shows, and the episode itself is named in the subline.
pub(super) fn home_card_title(item: &JfItem) -> String {
    if item.kind == "Episode" {
        item.series_name.clone().unwrap_or_else(|| item.name.clone())
    } else {
        item.name.clone()
    }
}

/// The second line of a Home card: where the item sits in the library, plus
/// how much of it has been watched (the information the web UI's cards
/// show).
pub(super) fn home_subline(item: &JfItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    match item.kind.as_str() {
        "Episode" => {
            if let Some(code) = item.episode_code() {
                parts.push(code);
            }
            // The series is the card's title; the subline names the episode.
            if !item.name.is_empty() {
                parts.push(item.name.clone());
            }
        }
        "Series" => {
            if let Some(count) = plural(item.child_count, "season", "seasons") {
                parts.push(count);
            }
            if let Some(year) = item.year {
                parts.push(year.to_string());
            }
        }
        "Season" => {
            if let Some(series) = &item.series_name {
                parts.push(series.clone());
            }
            if let Some(count) = plural(item.child_count, "episode", "episodes") {
                parts.push(count);
            }
        }
        _ => {
            if let Some(year) = item.year {
                parts.push(year.to_string());
            }
            if let Some(secs) = item.runtime_secs {
                parts.push(clock(secs));
            }
        }
    }
    if let Some(watched) = item.progress_fraction() {
        parts.push(format!("{}%", (watched * 100.0).round() as u32));
    }
    if let Some(resume) = item.resume_secs() {
        let left = item
            .runtime_secs
            .map_or(0, |runtime| runtime.saturating_sub(resume));
        if left > 0 {
            parts.push(format!("{} left", clock(left)));
        } else {
            parts.push(format!("{} in", clock(resume)));
        }
    } else if item.played {
        parts.push("watched".to_owned());
    }
    parts.join(" · ")
}

/// `"3 seasons"` / `"1 season"` for a child count.
fn plural(count: Option<i32>, one: &str, many: &str) -> Option<String> {
    count.map(|n| format!("{n} {}", if n == 1 { one } else { many }))
}

/// The per-item image bytes the wall draws (tag → bytes).
pub(super) type HomeImageCache = std::collections::HashMap<String, Arc<Vec<u8>>>;
/// Image tags with a fetch in flight.
pub(super) type HomeImageRequests = HashSet<String>;
