use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
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
        let base =
            ctx.config.theme.text_color.map_or_else(Style::default, |c| Style::default().fg(c));
        let dim = base.add_modifier(Modifier::DIM);
        let mouse = ctx.mouse_pos();

        let mut x = self.x;
        let mut areas = Vec::with_capacity(self.segments.len());
        for segment in self.segments {
            let label = format!(" {} {} ", if segment.active { "●" } else { "⭘" }, segment.label);
            let width = (label.width() as u16).min(self.right.saturating_sub(x));
            let area = Rect { x, y: self.y, width, height: 1 };
            let base_style = if segment.active { base.add_modifier(Modifier::BOLD) } else { dim };
            // Hovering a toggle lightens it (clickable text).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> Ctx {
        crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }

    fn render(bar: &SubTabBar<'_>, ctx: &mut Ctx) -> (String, Vec<Rect>) {
        let backend = ratatui::backend::TestBackend::new(60, 3);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let mut areas = Vec::new();
        terminal.draw(|frame| areas = bar.render(frame, ctx)).expect("draw ok");
        let width = terminal.backend().buffer().area.width as usize;
        let line: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .skip(width * (bar.y as usize))
            .take(width)
            .map(|c| c.symbol())
            .collect();
        (line, areas)
    }

    /// The active segment gets the filled dot and bold text; inactive ones
    /// the hollow dot and dim text; the areas come back one per segment.
    #[test]
    fn renders_segments_with_active_marker_and_click_areas() {
        let mut ctx = test_ctx();
        let segments = [
            Segment { label: "Audio", active: true },
            Segment { label: "Video", active: false },
            Segment { label: "Chapters", active: false },
        ];
        let bar = SubTabBar::new(&segments, 1, 1, 59);
        let (line, areas) = render(&bar, &mut ctx);

        assert!(line.contains(" ● Audio "), "active segment shows ●: {line}");
        assert!(line.contains(" ⭘ Video "), "inactive segment shows ⭘: {line}");
        assert!(line.contains(" ⭘ Chapters "), "inactive segment shows ⭘: {line}");
        assert_eq!(areas.len(), 3, "one click area per segment");
        assert!(areas[0].contains(ratatui::layout::Position::new(2, 1)));
        assert!(areas[1].x >= areas[0].right(), "segments are laid out left to right");
        assert_eq!(areas[2].y, 1, "all segments share the row");
    }

    /// A short right bound clips the last segment instead of overflowing.
    #[test]
    fn clips_at_the_right_bound() {
        let mut ctx = test_ctx();
        let segments =
            [Segment { label: "Audio", active: true }, Segment { label: "Video", active: false }];
        let bar = SubTabBar::new(&segments, 1, 1, 12);
        let (line, areas) = render(&bar, &mut ctx);
        assert!(areas[1].right() <= 12, "no segment past the right bound");
        let _ = line;
    }
}
