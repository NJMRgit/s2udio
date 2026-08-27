//! Rubber-band (drag-rect) selection: the state shared by every list that
//! supports mouse drag-to-select (the `SongListCore` lists — search
//! results, albums, tag browser, the MPD browser's items, the playlists
//! songs pane — plus the queue audio table and the queue video playlist).
//! Selection feedback is the existing marked-row highlight (per user
//! feedback, 2026-08-27: no rounded band rectangle is drawn).
//!
//! Round 46 semantics: a left press inside a list *arms* the band without
//! touching the marks yet (a click must not wipe a multi-selection);
//! the first `Drag` event that leaves the anchor row turns it into a band
//! — plain drag replaces every mark with the anchor→current range, ctrl
//! drag adds/contracts the range keeping the other marks. `LeftRelease`
//! finalizes (the band's marks stay) or applies the deferred plain-click
//! unmark when the press never moved.
//!
//! A drag that leaves the list area keeps updating (capture semantics):
//! [`band_current_row`] clamps the pointer into the visible list.

use ratatui::layout::Rect;

/// How a press that armed a band resolves on release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandEnd {
    /// No band was armed; nothing to do.
    None,
    /// The press never moved: this was a plain click. `clear_marks` says
    /// whether the click landed on a row other than the selection (the
    /// plain-click "clicking a different row drops the multi-selection"
    /// semantics, deferred until the click/drag disambiguation resolves).
    Click { clear_marks: bool },
    /// The press became a drag: the band's marks stay as the finalize
    /// state.
    Drag,
}

/// Rubber-band (drag-rect) selection state of one list.
#[derive(Debug, Default, Clone, Copy)]
pub struct BandState {
    /// A band is armed (left-pressed inside the list) or active (dragging).
    pub active: bool,
    /// A `Drag` event actually moved the pointer off the anchor row
    /// (press ≠ click).
    pub moved: bool,
    /// The list index where the press landed (the fixed band edge).
    pub anchor: Option<usize>,
    /// The list index under the current drag position (the moving band
    /// edge); equals `anchor` until the first drag event.
    pub current: Option<usize>,
    /// The press landed on a row other than the selected row (a click
    /// resolves by clearing the marks, see [`BandEnd::Click`]).
    pub click_on_different_row: bool,
}

impl BandState {
    /// Arm the band from a left press at `idx`, deferring any mark changes
    /// until the press resolves into a click or a drag.
    pub fn arm(&mut self, idx: usize, click_on_different_row: bool) {
        self.active = true;
        self.moved = false;
        self.anchor = Some(idx);
        self.current = Some(idx);
        self.click_on_different_row = click_on_different_row;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Move the band's free edge to `idx` (a drag event). A drag that
    /// stays on the anchor row is treated as jitter and does not flip the
    /// press into a drag (zero-distance drags stay clicks).
    pub fn update(&mut self, idx: usize) {
        if Some(idx) == self.anchor {
            return;
        }
        self.moved = true;
        self.current = Some(idx);
    }

    /// A release resolved the press: deactivate and report what the
    /// caller should do with the marks.
    pub fn release(&mut self) -> BandEnd {
        if !self.active {
            return BandEnd::None;
        }
        self.active = false;
        self.anchor = None;
        self.current = None;
        if self.moved {
            BandEnd::Drag
        } else {
            BandEnd::Click { clear_marks: self.click_on_different_row }
        }
    }

    /// Cancel the band without touching the marks (release missed outside
    /// the terminal, another interaction took over). Returns whether a
    /// band was active.
    pub fn cancel(&mut self) -> bool {
        if self.active {
            self.active = false;
            self.anchor = None;
            self.current = None;
            true
        } else {
            false
        }
    }
}

/// The absolute list index the band's pointer currently covers. The
/// pointer row is clamped into the visible list (a drag outside the list
/// area still resolves to the edge row) and to the item count.
/// `row_height` is the terminal lines per row (1 for every single-line
/// target list).
pub fn band_current_row(
    pointer_y: u16,
    area: Rect,
    offset: usize,
    len: usize,
    row_height: u16,
) -> Option<usize> {
    if len == 0 || area.height == 0 {
        return None;
    }
    let row_h = row_height.max(1);
    let clamped_px = pointer_y.clamp(area.y, area.bottom().saturating_sub(1));
    let within = (clamped_px - area.y) / row_h;
    let visible = (area.height / row_h) as usize;
    let idx = offset + within as usize;
    Some(
        idx.min(
            offset
                .saturating_add(visible)
                .saturating_sub(1),
        )
        .min(len - 1),
    )
}

