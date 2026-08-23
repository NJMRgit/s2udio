use bon::Builder;
use ratatui::{
    prelude::{Buffer, Rect},
    style::{Color, Style},
    widgets::Widget,
};
#[derive(Clone, Builder)]
pub struct ProgressBar<'a> {
    value: f32,
    start_char: &'a str,
    elapsed_char: &'a str,
    thumb_char: &'a str,
    track_char: &'a str,
    end_char: &'a str,
    elapsed_style: Style,
    thumb_style: Style,
    track_style: Style,
    use_track_when_empty: bool,
    /// Column (relative to the bar's left edge) that a pointer hovers;
    /// every column strictly left of it gets the hovered styles while the
    /// rest keeps the normal styles (the "played-portion highlight").
    /// `None` keeps the pre-hover rendering.
    hover_col: Option<u16>,
    /// Keyboard-seek cursor column: the thumb renders at this column and
    /// columns left of it get the hovered styles (used while the seekbar
    /// owns keyboard control). Overrides `hover_col` when set.
    cursor_col: Option<u16>,
}
impl Widget for ProgressBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < 1 || area.width < 1 {
            return;
        }
        let left = area.left();
        let top = area.top();
        let len = area.width;
        buf.set_string(
            left,
            top,
            self.track_char.repeat(len as usize),
            self.track_style,
        );
        let filled_cols = ((len as f32 * self.value).round() as u16).min(len);
        for i in 0..len {
            let x = left + i;
            let last_idx = len.saturating_sub(1);
            let (char, style) = if i == 0 && self.use_track_when_empty
                && filled_cols == 0
            {
                (self.track_char, self.track_style)
            } else if i == last_idx && self.use_track_when_empty
                && filled_cols < last_idx
            {
                (self.track_char, self.track_style)
            } else if i == 0 {
                let style = if filled_cols == 0 {
                    self.track_style
                } else {
                    self.elapsed_style
                };
                (self.start_char, style)
            } else if i == last_idx {
                let style = if filled_cols < last_idx {
                    self.track_style
                } else {
                    self.elapsed_style
                };
                (self.end_char, style)
            } else if i == filled_cols {
                (self.thumb_char, self.thumb_style)
            } else if i < filled_cols {
                (self.elapsed_char, self.elapsed_style)
            } else {
                (self.track_char, self.track_style)
            };
            let (char, style) = if let Some(cursor) = self.cursor_col {
                if i == cursor
                    && !(i == 0 && self.use_track_when_empty && filled_cols == 0)
                {
                    (self.thumb_char, crate::config::hover_style(self.thumb_style))
                } else if i < cursor {
                    let hovered = crate::config::hover_style(style);
                    if i == 0 && self.use_track_when_empty && filled_cols == 0 {
                        (char, hovered)
                    } else if i == 0 {
                        (self.start_char, hovered)
                    } else if i < filled_cols {
                        (self.elapsed_char, hovered)
                    } else {
                        (self.track_char, hovered)
                    }
                } else {
                    (char, style)
                }
            } else if let Some(hover) = self.hover_col && i < hover {
                let hovered = crate::config::hover_style(style);
                if i == 0 {
                    (self.start_char, hovered)
                } else if i < filled_cols {
                    (self.elapsed_char, hovered)
                } else if i == filled_cols && i != last_idx {
                    (self.thumb_char, crate::config::hover_style(self.thumb_style))
                } else {
                    (self.track_char, hovered)
                }
            } else {
                (char, style)
            };
            buf.set_string(x, top, char, style);
        }
    }
}
impl Default for ProgressBar<'_> {
    fn default() -> Self {
        Self {
            value: 0.0,
            start_char: "-",
            elapsed_char: "█",
            thumb_char: "",
            track_char: " ",
            end_char: "═",
            elapsed_style: Style::default().fg(Color::Blue),
            thumb_style: Style::default().bg(Color::Black).fg(Color::Blue),
            track_style: Style::default().bg(Color::Black),
            use_track_when_empty: false,
            hover_col: None,
            cursor_col: None,
        }
    }
}
