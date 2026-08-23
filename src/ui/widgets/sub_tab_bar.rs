use ratatui::{
    Frame, layout::Rect, style::{Modifier, Style},
    text::Line,
};
use unicode_width::UnicodeWidthStr;
use crate::{config::hover_style, ctx::Ctx};
/// One labeled segment of a `SubTabBar` (e.g. `Audio`); `active` controls
/// the ●/⭘ marker and the bold/dim styling.
#[derive(Debug, Clone, Copy)]
pub struct Segment<'a> {
    pub label: &'a str,
    pub active: bool,
}
/// A row of labeled mode segments — the queue's `● Audio ○ Video ○
/// Chapters` toggle row (Phase 4, §4.4 of the rewrite outline). Draws
/// each segment on one row starting at `(x, y)` and returns its click
/// area; the active segment gets the filled dot (●) and bold text, the
/// inactive ones the hollow dot (⭘) and dim text, and the segment under
/// the pointer lightens like other clickable text. The ●/⭘ glyphs are
/// both single-width so the row never shifts between modes.
///
/// Callers keep whatever layout context they need around the bar: the
/// queue pane locates the row above its box border and maps the returned
/// areas back to its modes.
#[derive(Debug, Clone, Copy)]
pub struct SubTabBar<'a> {
    segments: &'a [Segment<'a>],
    x: u16,
    y: u16,
    right: u16,
}
impl<'a> SubTabBar<'a> {
    pub fn new(segments: &'a [Segment<'a>], x: u16, y: u16, right: u16) -> Self {
        Self { segments, x, y, right }
    }
    /// Render the segments and return their click areas (one per segment,
    /// in order).
    pub fn render(&self, frame: &mut Frame, ctx: &Ctx) -> Vec<Rect> {
        let base = ctx
            .config
            .theme
            .text_color
            .map_or_else(Style::default, |c| Style::default().fg(c));
        let dim = base.add_modifier(Modifier::DIM);
        let mouse = ctx.mouse_pos();
        let mut x = self.x;
        let mut areas = Vec::with_capacity(self.segments.len());
        for segment in self.segments {
            let label = format!(
                " {} {} ", if segment.active { "●" } else { "⭘" }, segment.label
            );
            let width = (label.width() as u16).min(self.right.saturating_sub(x));
            let area = Rect {
                x,
                y: self.y,
                width,
                height: 1,
            };
            let base_style = if segment.active {
                base.add_modifier(Modifier::BOLD)
            } else {
                dim
            };
            let style = if mouse.is_some_and(|p| area.contains(p)) {
                hover_style(base_style)
            } else {
                base_style
            };
            frame.render_widget(Line::styled(label, style), area);
            areas.push(area);
            x += width;
        }
        areas
    }
}
