use std::ops::Range;
use ratatui::{
    style::{Style, Stylize},
    text::Span,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use crate::ui::input::{InputEvent, InputResultEvent};
#[derive(Debug, Default, Clone)]
pub(super) struct InputBuffer {
    value: String,
    cursor: usize,
    visible_slice: Range<usize>,
    available_columns: usize,
}
#[derive(Default)]
pub(super) struct Grapheme {
    offset: usize,
    len: usize,
}
impl InputBuffer {
    pub(super) fn new(initial_value: Option<&str>) -> Self {
        Self {
            value: initial_value.unwrap_or_default().to_owned(),
            cursor: initial_value.map_or(0, |s| s.len()),
            visible_slice: 0..initial_value.map_or(0, |s| s.len()),
            available_columns: 0,
        }
    }
    pub(super) fn value(&self) -> &str {
        &self.value
    }
    pub(super) fn set_value(&mut self, new_value: String) {
        self.cursor = new_value.len();
        self.value = new_value;
        self.visible_slice = 0..0;
    }
    pub(super) fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.visible_slice = 0..0;
    }
    pub fn as_spans(
        &mut self,
        prefix: Option<&'static str>,
        available_width: impl Into<usize>,
        style: Style,
        is_active: bool,
    ) -> Vec<Span<'static>> {
        let value = &self.value;
        let value_len = value.len();
        let cursor = self.cursor;
        self.available_columns = available_width.into();
        let reserved_for_prefix = prefix.map_or(0, |p| p.width() + 1);
        let reserved_for_caret = (is_active && self.cursor == value_len) as usize;
        let cols = self
            .available_columns
            .saturating_sub(reserved_for_prefix)
            .saturating_sub(reserved_for_caret);
        if cols == 0 {
            self.visible_slice = 0..0;
            return Vec::new();
        }
        let mut start = snap_to_grapheme_start(
            value,
            self.visible_slice.start.min(value_len),
        );
        let mut end = fill_to_end(value, start, cols);
        if cursor < start {
            start = snap_to_grapheme_start(value, cursor);
            end = fill_to_end(value, start, cols);
        } else if cursor >= end {
            let target_end = next_grapheme_end(value, cursor);
            let mut remaining = cols;
            let mut new_start = target_end;
            for (i, g) in value
                .grapheme_indices(true)
                .rev()
                .skip_while(|(i, _)| *i >= target_end)
            {
                let w = g.width();
                if w > remaining {
                    break;
                }
                remaining = remaining.saturating_sub(w);
                new_start = i;
                if remaining == 0 {
                    break;
                }
            }
            start = new_start;
            end = target_end;
        }
        let mut result = Vec::new();
        if let Some(p) = prefix {
            result.push(Span::styled(p, style));
            result.push(Span::styled(" ", style));
        }
        let mut buf = String::new();
        for (idx, g) in value
            .grapheme_indices(true)
            .skip_while(|(i, _)| *i < start)
            .take_while(|(i, _)| *i < end)
        {
            if idx == cursor {
                if !buf.is_empty() {
                    result.push(Span::styled(std::mem::take(&mut buf), style));
                }
                result.push(Span::styled(g.to_owned(), style).reversed());
            } else {
                buf.push_str(g);
            }
        }
        if !buf.is_empty() {
            result.push(Span::styled(buf, style));
        }
        if is_active && cursor == value_len {
            result.push(Span::styled("█", style));
        }
        self.visible_slice = start..end;
        result
    }
    pub fn handle_input(&mut self, ev: Option<InputEvent>) -> InputResultEvent {
        let old_cursor = self.cursor;
        let result = match ev {
            Some(InputEvent::Push(c)) => {
                let g = self.current_grapheme();
                if g.len > 0 && g.offset < self.cursor && self.cursor < g.offset + g.len
                {
                    self.cursor = g.offset;
                }
                self.value.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                InputResultEvent::Push
            }
            Some(InputEvent::PopLeft) => {
                if self.cursor == 0 {
                    return InputResultEvent::NoChange;
                }
                let grapheme = self.current_grapheme();
                self.value.drain(grapheme.offset..grapheme.offset + grapheme.len);
                self.cursor = grapheme.offset;
                InputResultEvent::Pop
            }
            Some(InputEvent::PopRight) => {
                if self.cursor == self.value.len() {
                    return InputResultEvent::NoChange;
                }
                let grapheme = self.next_grapheme();
                self.value.drain(grapheme.offset..grapheme.offset + grapheme.len);
                InputResultEvent::Pop
            }
            Some(InputEvent::PopWordLeft) => {
                if self.cursor == 0 {
                    return InputResultEvent::NoChange;
                }
                let deletion_start = self
                    .value
                    .unicode_word_indices()
                    .find(|(idx, w)| {
                        (*idx..*idx + w.len()).contains(&self.cursor.saturating_sub(1))
                    })
                    .map(|(idx, _)| idx)
                    .or_else(|| {
                        self.value
                            .unicode_word_indices()
                            .take_while(|(idx, _)| *idx < self.cursor)
                            .last()
                            .map(|(idx, _)| idx)
                    })
                    .unwrap_or(0);
                if deletion_start >= self.cursor {
                    return InputResultEvent::NoChange;
                }
                self.value.drain(deletion_start..self.cursor);
                self.cursor = deletion_start;
                InputResultEvent::Pop
            }
            Some(InputEvent::PopWordRight) => {
                if self.cursor >= self.value.len() {
                    return InputResultEvent::NoChange;
                }
                let bytes_to_drain = self
                    .value
                    .unicode_word_indices()
                    .find(|(idx, w)| (*idx..*idx + w.len()).contains(&self.cursor))
                    .map(|(idx, w)| {
                        w.len().saturating_sub(self.cursor.saturating_sub(idx))
                    })
                    .or_else(|| {
                        self.value
                            .unicode_word_indices()
                            .find(|(idx, _)| idx > &self.cursor)
                            .map(|(idx, w)| idx.saturating_sub(self.cursor) + w.len())
                    })
                    .unwrap_or_else(|| self.value.len().saturating_sub(self.cursor));
                self.value.drain(self.cursor..self.cursor + bytes_to_drain);
                InputResultEvent::Pop
            }
            Some(InputEvent::DeleteToStart) => {
                if self.cursor == 0 {
                    return InputResultEvent::NoChange;
                }
                self.value.drain(0..self.cursor);
                self.cursor = 0;
                InputResultEvent::Pop
            }
            Some(InputEvent::DeleteToEnd) => {
                if self.cursor == self.value.len() {
                    return InputResultEvent::NoChange;
                }
                let grapheme = self.next_grapheme();
                self.value.drain(grapheme.offset..);
                self.cursor = self.value.len();
                InputResultEvent::Pop
            }
            Some(InputEvent::Back) => {
                self.cursor = self.current_grapheme().offset;
                InputResultEvent::NoChange
            }
            Some(InputEvent::Forward) => {
                let g = self.next_grapheme();
                self.cursor = (g.offset + g.len).min(self.value.len());
                InputResultEvent::NoChange
            }
            Some(InputEvent::Start) => {
                self.cursor = 0;
                InputResultEvent::NoChange
            }
            Some(InputEvent::End) => {
                self.cursor = self.value.len();
                InputResultEvent::NoChange
            }
            Some(InputEvent::BackWord) => {
                let prev = self.prev_word_boundary();
                self.cursor = prev.max(0);
                InputResultEvent::NoChange
            }
            Some(InputEvent::ForwardWord) => {
                let next = self.next_word_boundary();
                self.cursor = next.min(self.value.len());
                InputResultEvent::NoChange
            }
            None => InputResultEvent::NoChange,
        };
        if !self.visible_slice.contains(&self.cursor) {
            if old_cursor > self.cursor {
                let start = self.cursor;
                let mut end = start;
                let mut remaining = self.available_columns;
                for (i, g) in self
                    .value
                    .grapheme_indices(true)
                    .skip_while(|(i, _)| *i < start)
                {
                    let w = g.width();
                    if w > remaining {
                        break;
                    }
                    remaining -= w;
                    end = i + g.len();
                }
                self.visible_slice = start..end.min(self.value.len());
            } else {
                let mut end = self.cursor;
                if let Some((i, g)) = self
                    .value
                    .grapheme_indices(true)
                    .take_while(|(i, _)| *i <= self.cursor)
                    .last()
                {
                    end = i + g.len();
                }
                let mut start = end;
                let mut remaining = self.available_columns;
                for (i, g) in self
                    .value
                    .grapheme_indices(true)
                    .rev()
                    .skip_while(|(i, _)| *i >= end)
                {
                    let w = g.width();
                    if w > remaining {
                        break;
                    }
                    remaining -= w;
                    start = i;
                }
                self.visible_slice = start
                    .min(self.value.len())..end.min(self.value.len());
            }
        }
        result
    }
    #[inline]
    pub fn next_word_boundary(&self) -> usize {
        self.value
            .unicode_word_indices()
            .find(|(idx, _)| idx > &self.cursor)
            .map_or(self.value.len(), |(idx, _)| idx)
    }
    #[inline]
    pub fn prev_word_boundary(&self) -> usize {
        self.value
            .unicode_word_indices()
            .take_while(|(idx, _)| idx < &self.cursor)
            .last()
            .map_or(0, |(idx, _)| idx)
    }
    #[inline]
    pub fn current_grapheme(&self) -> Grapheme {
        self.value
            .grapheme_indices(true)
            .take_while(|(idx, _)| idx < &self.cursor)
            .last()
            .map_or(
                Grapheme::default(),
                |(idx, g)| Grapheme {
                    offset: idx,
                    len: g.len(),
                },
            )
    }
    #[inline]
    pub fn next_grapheme(&self) -> Grapheme {
        self.value
            .grapheme_indices(true)
            .take_while(|(idx, _)| idx <= &self.cursor)
            .last()
            .map_or(
                Grapheme::default(),
                |(idx, g)| Grapheme {
                    offset: idx,
                    len: g.len(),
                },
            )
    }
}
#[inline]
fn snap_to_grapheme_start(value: &str, pos: usize) -> usize {
    value
        .grapheme_indices(true)
        .take_while(|(i, _)| *i <= pos)
        .last()
        .map_or(0, |(i, _)| i)
}
#[inline]
fn next_grapheme_end(value: &str, pos: usize) -> usize {
    let value_len = value.len();
    if pos >= value_len {
        return value_len;
    }
    value
        .grapheme_indices(true)
        .find(|(i, _)| *i > pos)
        .map_or(value_len, |(i, g)| i + g.len())
}
fn fill_to_end(value: &str, start: usize, available_cols: usize) -> usize {
    let mut used = 0usize;
    let mut end = start;
    for (i, g) in value.grapheme_indices(true).skip_while(|(i, _)| *i < start) {
        let w = g.width();
        if w > available_cols.saturating_sub(used) {
            break;
        }
        used += w;
        end = i + g.len();
        if used >= available_cols {
            break;
        }
    }
    end
}
