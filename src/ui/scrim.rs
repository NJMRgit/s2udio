//! Backdrop dimming behind modal dialogs.
//!
//! `[theme] modal_backdrop` used to paint one flat `Color::DarkGray` over every
//! foreground in the buffer. That threw away the theme: syntax colours, the
//! accent, the cava gradient — everything read as grey, and the background was
//! untouched so the dialog did not stand out from it.
//!
//! The backdrop now *blends* each cell toward the theme background, the
//! foreground harder than the background, by an animated factor
//! (`Ctx::advance_modal_fade`). The dialog keeps its own colours because modals
//! render **after** this pass, and the panes behind it keep their shape because
//! their backgrounds are dimmed less than their text.
//!
//! Two limits are deliberate:
//!
//! * only truecolor cells are blended — an indexed or named colour has no
//!   meaningful midpoint, so those cells instead get the terminal's own
//!   `DIM` attribute (binary, applied past the halfway point of the fade);
//! * the pass is O(cells) over the whole screen, which is a few thousand
//!   `Cell`s — nothing next to the render itself, and only while a modal is
//!   open.

use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};

/// How far a foreground is pulled toward the backdrop at factor 1.
const FG_DIM: f32 = 0.65;
/// How far a background is pulled toward the backdrop (less: the panes must
/// keep their shape, and a fully flattened background hides the dividers).
const BG_DIM: f32 = 0.45;

/// Blend `from` toward `to` by `t` (clamped to `0.0..=1.0`).
///
/// `None` when either colour is not truecolor: blending an indexed colour by
/// arithmetic would invent a palette index that means something else.
pub(crate) fn blend(from: Color, to: Color, t: f32) -> Option<Color> {
    let (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) = (from, to) else {
        return None;
    };
    let t = f64::from(if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) });
    Some(Color::Rgb(mix(fr, tr, t), mix(fg, tg, t), mix(fb, tb, t)))
}

fn mix(from: u8, to: u8, t: f64) -> u8 {
    let value = f64::from(from) + (f64::from(to) - f64::from(from)) * t;
    value.round().clamp(0.0, 255.0) as u8
}

/// The colour cells fade toward when the theme does not name one.
pub(crate) fn backdrop(theme_bg: Option<Color>) -> Color {
    theme_bg.unwrap_or(Color::Rgb(0, 0, 0))
}

/// Dim every cell of `buffer` by `factor` (`0.0` does nothing, `1.0` is the
/// full backdrop).
///
/// Called after the background panes render and before the modals do, so the
/// dialog is painted on top of the dimmed screen at full brightness.
pub(crate) fn dim_backdrop(buffer: &mut Buffer, factor: f32, backdrop: Color) {
    let factor = if factor.is_nan() { 0.0 } else { factor.clamp(0.0, 1.0) };
    if factor <= f32::EPSILON {
        return;
    }
    let area = *buffer.area();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let Some(cell) = buffer.cell_mut((x, y)) else {
                continue;
            };
            if let Some(fg) = blend(cell.fg, backdrop, FG_DIM * factor) {
                cell.fg = fg;
            } else if factor >= 0.5 {
                cell.modifier.insert(Modifier::DIM);
            }
            if let Some(bg) = blend(cell.bg, backdrop, BG_DIM * factor) {
                cell.bg = bg;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect, style::Style};

    const BG: Color = Color::Rgb(0, 0, 0);

    #[test]
    fn blending_pins_both_ends() {
        let from = Color::Rgb(200, 100, 50);
        assert_eq!(blend(from, BG, 0.0), Some(from));
        assert_eq!(blend(from, BG, 1.0), Some(BG));
    }

    #[test]
    fn blending_moves_toward_the_backdrop() {
        let from = Color::Rgb(200, 100, 50);
        assert_eq!(blend(from, BG, 0.5), Some(Color::Rgb(100, 50, 25)));
    }

    #[test]
    fn blending_clamps_and_survives_nan() {
        let from = Color::Rgb(10, 20, 30);
        assert_eq!(blend(from, BG, -1.0), Some(from));
        assert_eq!(blend(from, BG, 2.0), Some(BG));
        assert_eq!(blend(from, BG, f32::NAN), Some(from));
    }

    #[test]
    fn indexed_and_named_colours_cannot_be_blended() {
        // A palette index is not a point in RGB space: arithmetic on it would
        // invent a different colour.
        assert_eq!(blend(Color::Indexed(4), BG, 0.5), None);
        assert_eq!(blend(Color::Red, BG, 0.5), None);
        assert_eq!(blend(Color::Rgb(1, 2, 3), Color::Red, 0.5), None);
    }

    #[test]
    fn a_zero_factor_leaves_the_buffer_alone() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer.set_style(Rect::new(0, 0, 2, 1), Style::default().fg(Color::Rgb(9, 9, 9)));
        let before = buffer.cell((0, 0)).cloned();
        dim_backdrop(&mut buffer, 0.0, BG);
        assert_eq!(buffer.cell((0, 0)).cloned(), before);
    }

    #[test]
    fn a_full_factor_dims_foreground_more_than_background() {
        let area = Rect::new(0, 0, 2, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_style(
            area,
            Style::default().fg(Color::Rgb(200, 200, 200)).bg(Color::Rgb(100, 100, 100)),
        );

        dim_backdrop(&mut buffer, 1.0, BG);

        let cell = buffer.cell((0, 0)).cloned().expect("cell");
        // fg 200 -> 200 * (1 - 0.65) = 70; bg 100 -> 100 * (1 - 0.45) = 55.
        // The background keeps more of itself than the text does, which is what
        // keeps the panes readable as shapes behind the dialog.
        assert_eq!(cell.fg, Color::Rgb(70, 70, 70));
        assert_eq!(cell.bg, Color::Rgb(55, 55, 55));
    }

    #[test]
    fn an_indexed_foreground_gets_the_dim_attribute_instead() {
        let area = Rect::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_style(area, Style::default().fg(Color::Indexed(3)));

        // Below halfway the fade leaves the colour alone and adds nothing.
        dim_backdrop(&mut buffer, 0.25, BG);
        let cell = buffer.cell((0, 0)).cloned().expect("cell");
        assert_eq!(cell.fg, Color::Indexed(3));
        assert!(!cell.modifier.contains(Modifier::DIM));

        dim_backdrop(&mut buffer, 0.75, BG);
        let cell = buffer.cell((0, 0)).cloned().expect("cell");
        assert!(cell.modifier.contains(Modifier::DIM), "dim kicks in past halfway");
    }
}
