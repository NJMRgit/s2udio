use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{List, ListItem, ListState, StatefulWidget},
};

/// A ratatui `List` that renders from its state's offset, showing only
/// the visible slice. The selection is shifted by the offset so the
/// highlight only appears when the selected row is inside the window —
/// the selection can leave the visible area (round-32 wheel behavior:
/// the wheel scrolls the viewport, not the selection).
#[derive(Debug)]
pub struct VirtualizedList<'a> {
    items: Vec<ListItem<'a>>,
    highlight_style: Style,
    style: Style,
    /// Rows per item (1 for single-line lists, 2 for radio stations).
    row_height: u16,
}

impl<'a> VirtualizedList<'a> {
    pub fn new(items: Vec<ListItem<'a>>) -> Self {
        Self { items, highlight_style: Style::default(), style: Style::default(), row_height: 1 }
    }

    pub fn highlight_style(mut self, style: Style) -> Self {
        self.highlight_style = style;
        self
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    pub fn row_height(mut self, height: u16) -> Self {
        self.row_height = height.max(1);
        self
    }
}

impl<'a> StatefulWidget for VirtualizedList<'a> {
    type State = ListState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        // Save the real state and restore it afterwards: the caller's
        // offset is the authoritative viewport start and the selection is
        // the absolute row index, independent of what is on screen.
        let viewport = (area.height as usize) / (self.row_height as usize);
        // A stale offset (the list shrank since the last scroll) must not
        // leave the viewport past the end of the content.
        let original_offset = state.offset().min(self.items.len().saturating_sub(viewport));
        let original_selected = state.selected();
        let visible_selected = original_selected
            .and_then(|s| s.checked_sub(original_offset))
            .filter(|s| *s < viewport);
        let visible: Vec<ListItem> =
            self.items.into_iter().skip(original_offset).take(viewport).collect();

        let mut render_state = ListState::default();
        render_state.select(visible_selected);
        StatefulWidget::render(
            List::new(visible).highlight_style(self.highlight_style).style(self.style),
            area,
            buf,
            &mut render_state,
        );

        *state.offset_mut() = original_offset;
        state.select(original_selected);
    }
}

/// Scroll a `ListState`'s viewport by `dir * amount` rows without moving
/// the selection (the selection may leave the visible area). The offset
/// clamps at the list's ends.
pub fn scroll_viewport(
    state: &mut ListState,
    dir: i64,
    amount: usize,
    content_len: usize,
    viewport_len: usize,
) {
    let max_offset = content_len.saturating_sub(viewport_len);
    let new_offset = if dir < 0 {
        state.offset().saturating_sub(amount)
    } else {
        state.offset().saturating_add(amount).min(max_offset)
    };
    if new_offset != state.offset() {
        *state.offset_mut() = new_offset;
    }
}

/// Scroll a `ListState` so its selection is visible again after a
/// viewport-only wheel scroll (keyboard moves scroll the selection back
/// into view with the standard scrolloff behavior).
pub fn scroll_selection_into_view(
    state: &mut ListState,
    content_len: usize,
    viewport_len: usize,
    scrolloff: usize,
) {
    let Some(selected) = state.selected() else { return };
    if viewport_len == 0 {
        return;
    }
    let max_offset = content_len.saturating_sub(viewport_len);
    let offset = state.offset();

    // Always place the cursor in the middle of the screen when scrolloff
    // is too big.
    if scrolloff.saturating_mul(2) >= viewport_len {
        *state.offset_mut() = selected.saturating_sub(viewport_len / 2).min(max_offset);
        return;
    }

    let scrolloff_start_down =
        (offset.saturating_add(viewport_len)).saturating_sub(scrolloff.saturating_add(1));
    if selected > scrolloff_start_down {
        *state.offset_mut() =
            (offset.saturating_add(selected.saturating_sub(scrolloff_start_down))).min(max_offset);
        return;
    }

    if selected < offset.saturating_add(scrolloff) {
        *state.offset_mut() =
            offset.saturating_sub((offset.saturating_add(scrolloff)).saturating_sub(selected));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, layout::Rect, text::Line, widgets::StatefulWidget as _};

    fn items(n: usize) -> Vec<ListItem<'static>> {
        (0..n).map(|i| ListItem::new(Line::from(format!("row {i}")))).collect()
    }

    #[test]
    fn renders_the_visible_slice_from_the_offset() {
        let mut state = ListState::default();
        *state.offset_mut() = 5;
        state.select(Some(5));

        let backend = TestBackend::new(30, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                StatefulWidget::render(
                    VirtualizedList::new(items(20)).highlight_style(Style::default()),
                    Rect::new(0, 0, 30, 3),
                    frame.buffer_mut(),
                    &mut state,
                )
            })
            .unwrap();

        // The real state is untouched (offset 5, selection 5).
        assert_eq!(state.offset(), 5);
        assert_eq!(state.selected(), Some(5));
        // The buffer shows rows 5..8.
        let content = terminal.backend().buffer().content();
        let row_text = |row: usize| -> String {
            content[row * 30..row * 30 + 30].iter().map(|cell| cell.symbol()).collect::<String>()
        };
        assert!(row_text(0).contains("row 5"), "first line shows row 5: {:?}", row_text(0));
        assert!(row_text(2).contains("row 7"), "third line shows row 7: {:?}", row_text(2));
    }

    #[test]
    fn off_screen_selection_renders_no_highlight() {
        let mut state = ListState::default();
        *state.offset_mut() = 10;
        state.select(Some(0)); // selection far above the window

        let backend = TestBackend::new(30, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                StatefulWidget::render(
                    VirtualizedList::new(items(20)).highlight_style(Style::default()),
                    Rect::new(0, 0, 30, 3),
                    frame.buffer_mut(),
                    &mut state,
                )
            })
            .unwrap();

        assert_eq!(state.offset(), 10, "the viewport offset is preserved");
        assert_eq!(state.selected(), Some(0), "the selection is preserved");
    }

    #[test]
    fn scroll_viewport_clamps_at_the_ends() {
        let mut state = ListState::default();
        scroll_viewport(&mut state, 1, 3, 20, 5);
        assert_eq!(state.offset(), 3);
        scroll_viewport(&mut state, 1, 100, 20, 5);
        assert_eq!(state.offset(), 15, "clamps at content_len - viewport");
        scroll_viewport(&mut state, -1, 100, 20, 5);
        assert_eq!(state.offset(), 0);
    }

    #[test]
    fn scroll_selection_into_view_brings_an_off_screen_selection_back() {
        let mut state = ListState::default();
        *state.offset_mut() = 10;
        state.select(Some(12));
        scroll_selection_into_view(&mut state, 20, 5, 0);
        assert_eq!(state.offset(), 10, "already visible, no move");

        state.select(Some(18));
        scroll_selection_into_view(&mut state, 20, 5, 0);
        assert_eq!(state.offset(), 14, "scrolled down so row 18 is the last visible row");

        state.select(Some(0));
        scroll_selection_into_view(&mut state, 20, 5, 0);
        assert_eq!(state.offset(), 0, "scrolled up so row 0 is visible");
    }
}
