use std::io::Write;

use anyhow::Result;
use crossterm::{
    cursor::{RestorePosition, SavePosition},
    queue,
    style::{
        Attribute, Color as CrosstermColor, Colors, ResetColor, SetAttribute, SetColors,
    },
};
use ratatui::{layout::Rect, prelude::IntoCrossterm, style::Modifier};

use crate::{
    config::{
        Size,
        album_art::{HorizontalAlign, VerticalAlign},
    },
    ctx::Ctx,
    shared::macros::csi_move,
};

pub mod block;
pub mod facade;
pub mod placements;
pub mod iterm2;
pub mod kitty;
pub mod sixel;
pub mod ueberzug;
pub mod wall;

/// How much of a freshly-drawn image to show this frame.
///
/// `None` draws all of it. `Some(fraction)` asks for the first `fraction` of the
/// image's columns — a reveal. Only a backend that can paint a *partial* image
/// honours it (kitty, through its placeholder cells); every other backend draws
/// the whole image, which is the pre-reveal behaviour.
pub type Reveal = Option<f32>;

#[allow(unused)]
pub trait Backend {
    type EncodedData;
    fn hide(
        &mut self,
        w: &mut impl Write,
        size: Rect,
        bg_color: Option<crossterm::style::Color>,
    ) -> Result<()>;
    fn cleanup(self: Box<Self>, rect: Rect) -> Result<()> {
        Ok(())
    }
    fn display(&mut self, w: &mut impl Write, data: Self::EncodedData, ctx: &Ctx) -> Result<()>;
    /// Draw `data`, replacing whatever this backend last drew in `size`.
    ///
    /// The default retires the old drawing and draws the new one. A backend
    /// that can tell "same payload, new place" apart — kitty, by image id —
    /// overrides this so a re-place skips the transfer, which is what makes
    /// the reveal animation (one draw per frame) affordable.
    fn replace(
        &mut self,
        w: &mut impl Write,
        size: Rect,
        bg_color: Option<crossterm::style::Color>,
        data: Self::EncodedData,
        ctx: &Ctx,
        reveal: Reveal,
    ) -> Result<()> {
        // No partial painting here: the image appears whole.
        let _ = reveal;
        self.hide(w, size, bg_color)?;
        self.display(w, data, ctx)
    }
    fn create_data(
        image_data: &[u8],
        area: Rect,
        max_size: Size,
        halign: HorizontalAlign,
        valign: VerticalAlign,
    ) -> Result<Self::EncodedData>;
}

pub fn clear_area(
    w: &mut impl Write,
    bg_color: Option<crossterm::style::Color>,
    area: Rect,
) -> Result<()> {
    let colors = Colors { background: bg_color, foreground: None };
    queue!(w, SetColors(colors))?;
    queue!(w, SavePosition)?;
    let capacity: usize = 2usize * area.width as usize * area.height as usize;
    let mut buf = Vec::with_capacity(capacity);
    for y in area.top()..area.bottom() {
        csi_move!(buf, area.x, y)?;
        for _ in 0..area.width {
            write!(buf, " ")?;
        }
    }

    w.write_all(&buf)?;
    w.flush()?;
    queue!(w, RestorePosition)?;
    queue!(w, ResetColor)?;

    Ok(())
}

/// Write `area` back from the frame the flush belongs to, cell by cell.
///
/// A terminal-side overlay (the Home carousel's wall, the poster) draws
/// *after* ratatui flushed the frame, so a cell it erases or uncovers has no
/// matching write left in the diff: the buffer still describes the cell as
/// painted, while the glass shows whatever the overlay left behind. Blanks are
/// fine (both sides agree), but text is not — it stays missing until something
/// unrelated repaints those cells. `clear_area` therefore can not be used to
/// retire an overlay that sat over text; the cells have to be repainted with
/// the frame's own content instead.
pub fn paint_area(
    w: &mut impl Write,
    buffer: &ratatui::buffer::Buffer,
    area: Rect,
) -> Result<()> {
    let visible = area.intersection(buffer.area);
    if visible.is_empty() {
        return Ok(());
    }
    queue!(w, SavePosition)?;
    let capacity = 8usize * visible.width as usize * visible.height as usize;
    let mut buf: Vec<u8> = Vec::with_capacity(capacity);
    for y in visible.top()..visible.bottom() {
        csi_move!(buf, visible.left(), y)?;
        let mut current: Option<(CrosstermColor, CrosstermColor, Modifier)> = None;
        let mut x = visible.left();
        while x < visible.right() {
            let cell = &buffer[(x, y)];
            let fg = cell.fg.into_crossterm();
            let bg = cell.bg.into_crossterm();
            let style = (fg, bg, cell.modifier);
            if current != Some(style) {
                // `Attribute::Reset` clears every attribute *and* the colours,
                // so the colours are queued again right after it.
                queue!(buf, SetAttribute(Attribute::Reset))?;
                queue!(
                    buf,
                    SetColors(Colors { foreground: Some(fg), background: Some(bg) })
                )?;
                for (modifier, attribute) in MODIFIER_ATTRIBUTES {
                    if cell.modifier.contains(*modifier) {
                        queue!(buf, SetAttribute(*attribute))?;
                    }
                }
                current = Some(style);
            }
            let symbol = cell.symbol();
            write!(buf, "{symbol}")?;
            // A wide symbol owns the following (continuation) cells: skip them
            // so the next write lands on its own column.
            let width = unicode_width::UnicodeWidthStr::width(symbol).max(1);
            x = x.saturating_add(u16::try_from(width).unwrap_or(1));
        }
    }
    w.write_all(&buf)?;
    w.flush()?;
    queue!(w, RestorePosition)?;
    queue!(w, ResetColor)?;
    Ok(())
}

/// The ratatui modifiers a repainted cell can carry, in crossterm terms.
const MODIFIER_ATTRIBUTES: &[(Modifier, Attribute)] = &[
    (Modifier::BOLD, Attribute::Bold),
    (Modifier::DIM, Attribute::Dim),
    (Modifier::ITALIC, Attribute::Italic),
    (Modifier::UNDERLINED, Attribute::Underlined),
    (Modifier::SLOW_BLINK, Attribute::SlowBlink),
    (Modifier::RAPID_BLINK, Attribute::RapidBlink),
    (Modifier::REVERSED, Attribute::Reverse),
    (Modifier::HIDDEN, Attribute::Hidden),
    (Modifier::CROSSED_OUT, Attribute::CrossedOut),
];
