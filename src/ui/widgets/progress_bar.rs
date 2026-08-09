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

        buf.set_string(left, top, self.track_char.repeat(len as usize), self.track_style);

        let filled_cols = ((len as f32 * self.value).round() as u16).min(len);

        for i in 0..len {
            let x = left + i;
            let last_idx = len.saturating_sub(1);

            let (char, style) = if i == 0 && self.use_track_when_empty && filled_cols == 0 {
                // start char
                (self.track_char, self.track_style)
            } else if i == last_idx && self.use_track_when_empty && filled_cols < last_idx {
                // end char
                (self.track_char, self.track_style)
            } else if i == 0 {
                // start char
                let style = if filled_cols == 0 { self.track_style } else { self.elapsed_style };
                (self.start_char, style)
            } else if i == last_idx {
                // end char
                let style =
                    if filled_cols < last_idx { self.track_style } else { self.elapsed_style };
                (self.end_char, style)
            } else if i == filled_cols {
                // thumb
                (self.thumb_char, self.thumb_style)
            } else if i < filled_cols {
                // elapsed
                (self.elapsed_char, self.elapsed_style)
            } else {
                // track
                (self.track_char, self.track_style)
            };

            // The cursor column renders the thumb (whatever the play
            // fraction says); columns left of the cursor light up. This is
            // the "played-portion" highlight the pointer sees and the
            // keyboard cursor drives while the seekbar is focused.
            let (char, style) = if let Some(cursor) = self.cursor_col {
                if i == cursor && !(i == 0 && self.use_track_when_empty && filled_cols == 0) {
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
            } else if let Some(hover) = self.hover_col
                && i < hover
            {
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

#[cfg(test)]
mod tests {
    use ratatui::{
        buffer::Cell,
        prelude::{Buffer, Rect},
        style::{Color, Style},
        widgets::Widget,
    };

    use super::ProgressBar;

    #[test]
    fn lower_bound_is_correct() {
        let wg = ProgressBar {
            start_char: "S",
            elapsed_char: "E",
            thumb_char: "T",
            track_char: "B",
            end_char: "E",
            value: 0.0,
            ..Default::default()
        };
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer { area, content: vec![Cell::default(); 5] };

        wg.render(area, &mut buf);

        assert_eq!(buf[(0, 0)].symbol(), "S");
        assert_eq!(buf[(1, 0)].symbol(), "B");
        assert_eq!(buf[(2, 0)].symbol(), "B");
        assert_eq!(buf[(3, 0)].symbol(), "B");
        assert_eq!(buf[(4, 0)].symbol(), "E");
    }

    #[test]
    fn upper_bound_is_correct() {
        let wg = ProgressBar {
            start_char: "S",
            elapsed_char: "E",
            thumb_char: "T",
            track_char: "B",
            end_char: "E",
            value: 1.0,
            ..Default::default()
        };
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer { area, content: vec![Cell::default(); 5] };

        wg.render(area, &mut buf);

        assert_eq!(buf[(0, 0)].symbol(), "S");
        assert_eq!(buf[(1, 0)].symbol(), "E");
        assert_eq!(buf[(2, 0)].symbol(), "E");
        assert_eq!(buf[(3, 0)].symbol(), "E");
        assert_eq!(buf[(4, 0)].symbol(), "E");
    }

    #[test]
    fn middle_is_correct() {
        let wg = ProgressBar {
            start_char: "S",
            elapsed_char: "E",
            thumb_char: "T",
            track_char: "B",
            end_char: "X",
            value: 0.49,
            ..Default::default()
        };
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer { area, content: vec![Cell::default(); 5] };

        wg.render(area, &mut buf);

        assert_eq!(buf[(0, 0)].symbol(), "S");
        assert_eq!(buf[(1, 0)].symbol(), "E");
        assert_eq!(buf[(2, 0)].symbol(), "T");
        assert_eq!(buf[(3, 0)].symbol(), "B");
        assert_eq!(buf[(4, 0)].symbol(), "X");
    }

    #[test]
    fn only_track_when_empty() {
        let wg = ProgressBar {
            start_char: "S",
            elapsed_char: "E",
            thumb_char: "T",
            track_char: "B",
            end_char: "E",
            value: 0.0,
            use_track_when_empty: true,
            ..Default::default()
        };
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer { area, content: vec![Cell::default(); 5] };

        wg.render(area, &mut buf);

        assert_eq!(buf[(0, 0)].symbol(), "B");
        assert_eq!(buf[(1, 0)].symbol(), "B");
        assert_eq!(buf[(2, 0)].symbol(), "B");
        assert_eq!(buf[(3, 0)].symbol(), "B");
        assert_eq!(buf[(4, 0)].symbol(), "B");
    }

    #[test]
    fn hover_col_highlights_only_left_of_pointer() {
        // value 0.8 -> filled_cols 4, thumb at 4.
        let wg = ProgressBar {
            start_char: "S",
            elapsed_char: "E",
            thumb_char: "T",
            track_char: "B",
            end_char: "X",
            value: 0.8,
            hover_col: Some(2),
            elapsed_style: Style::default().fg(Color::Rgb(0x40, 0x40, 0x40)),
            thumb_style: Style::default().bg(Color::Black).fg(Color::White),
            track_style: Style::default().fg(Color::Rgb(0x40, 0x40, 0x40)).bg(Color::Black),
            ..Default::default()
        };
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer { area, content: vec![Cell::default(); 5] };

        wg.render(area, &mut buf);

        // Columns strictly left of the hover col keep their symbols but
        // take the hovered (lightened) style; columns at/right of it keep
        // the plain styles. The thumb stays at the played fraction (4).
        let hovered_fg =
            crate::config::hover_style(Style::default().fg(Color::Rgb(0x40, 0x40, 0x40))).fg;
        assert_eq!(buf[(0, 0)].symbol(), "S");
        assert_eq!(buf[(0, 0)].style().fg, hovered_fg, "start col 0 is left of hover 2");
        assert_eq!(buf[(1, 0)].symbol(), "E");
        assert_eq!(buf[(1, 0)].style().fg, hovered_fg, "col 1 left of hover 2 lights up");
        assert_eq!(buf[(2, 0)].symbol(), "E");
        assert_ne!(buf[(2, 0)].style().fg, hovered_fg, "at/right of hover keeps plain styles");
        assert_eq!(buf[(3, 0)].symbol(), "E");
        assert_ne!(buf[(3, 0)].style().fg, hovered_fg, "right of hover stays plain");
        assert_eq!(buf[(4, 0)].symbol(), "X");
    }

    #[test]
    fn cursor_col_renders_thumb_at_cursor_and_lights_left() {
        // Playback value 0.2 (thumb at 1), keyboard cursor at col 3: the
        // cursor wins — thumb renders at 3, columns 0..3 light up (their
        // symbols keep the true played fraction: S then track), the track
        // right of it stays plain.
        let wg = ProgressBar {
            start_char: "S",
            elapsed_char: "E",
            thumb_char: "T",
            track_char: "B",
            end_char: "X",
            value: 0.2,
            cursor_col: Some(3),
            elapsed_style: Style::default().fg(Color::Rgb(0x40, 0x40, 0x40)),
            thumb_style: Style::default().bg(Color::Black).fg(Color::White),
            track_style: Style::default().fg(Color::Rgb(0x40, 0x40, 0x40)).bg(Color::Black),
            ..Default::default()
        };
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer { area, content: vec![Cell::default(); 5] };

        wg.render(area, &mut buf);

        let hovered_fg =
            crate::config::hover_style(Style::default().fg(Color::Rgb(0x40, 0x40, 0x40))).fg;
        assert_eq!(buf[(0, 0)].symbol(), "S");
        assert_eq!(buf[(0, 0)].style().fg, hovered_fg, "start left of cursor lights up");
        assert_eq!(buf[(1, 0)].symbol(), "B");
        // The vacated thumb slot (played fraction position) lights up with
        // the hovered thumb style.
        assert_eq!(buf[(1, 0)].style().fg, Some(Color::White));
        assert_eq!(buf[(2, 0)].symbol(), "B");
        assert_eq!(buf[(2, 0)].style().fg, hovered_fg);
        assert_eq!(buf[(3, 0)].symbol(), "T", "cursor col renders the thumb");
        assert_ne!(buf[(3, 0)].style().fg, hovered_fg, "thumb uses its own hovered style");
        assert_eq!(buf[(4, 0)].symbol(), "X");
        assert_ne!(buf[(4, 0)].style().fg, hovered_fg, "right of cursor stays plain");
    }
}
