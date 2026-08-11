//! Shared marquee / carousel drawing: the controls-bar song-info carousel
//! and the info-box title marquee use the same cycle — hold at the start,
//! scroll left to the tail, hold at the end, wrap 3× faster with a
//! continuous news-ticker gap. `ScrollingLine` (`scrolling_line.rs`) stays
//! a separate widget: its cycle is a continuous `|`-separated repeat that
//! never holds, a genuinely different shape (see the Phase-5 close-out).

use ratatui::{
    buffer::Buffer,
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// Song-info carousel (used when the full group doesn't fit): each panel
/// (Artist | Album, then Title) holds for CAROUSEL_PAUSE_MS, then scrolls
/// left at 7.5 columns/second (1.5x the original 5 col/sec marquee) until it
/// has fully exited before the next panel takes over.
pub(crate) const CAROUSEL_PAUSE_MS: u64 = 2000; // hold each panel in place
pub(crate) const CAROUSEL_SPEED_X10: u64 = 75; // columns per 10 seconds (7.5 cols/sec)
/// Columns of slack between the tail and the re-entering head when the
/// title wraps around: the panel repeats after this gap, so the wrap is a
/// continuous news-ticker (tail … 5 cols … head) instead of a long blank
/// exit followed by a fresh entry.
pub(crate) const CAROUSEL_WRAP_GAP: u16 = 5;

/// Continuous marquee for a line wider than its window: holds briefly at
/// the left edge so the truncated start can be read, then scrolls left
/// and re-enters from the right (news-ticker style) with no blank gap.
pub(crate) fn draw_marquee(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    line: &Line,
    style: Style,
    progress_ms: u64,
) {
    let p = line.width() as u16;
    let w = width;
    if p <= w {
        draw_line(buf, x, y, width, line, style);
        return;
    }
    let o = marquee_offset(progress_ms, p, w);
    // The head copy first (its gap-fill blanks the window ahead of the
    // re-entering head); the tail is drawn last so it stays on top
    // while both are visible — the wrap reads tail … gap … head.
    draw_panel_at(
        buf,
        x,
        y,
        width,
        line,
        o - i64::from(p + CAROUSEL_WRAP_GAP),
        style,
    );
    draw_panel_at(buf, x, y, width, line, o, style);
}

/// The marquee window offset for a title of `panel_len` columns shown
/// in a `window_len` window at `progress_ms` into the cycle: hold 2s at
/// the start, scroll left until the tail is visible, hold 2s at the
/// end, then keep scrolling left through the wrap (the head copy,
/// drawn by the caller at `o - (panel_len + CAROUSEL_WRAP_GAP)`, follows
/// the tail with a 5-column slack) and repeat. Shared by the
/// controls-bar carousel and the info-box title.
pub(crate) fn marquee_offset(progress_ms: u64, panel_len: u16, window_len: u16) -> i64 {
    let p = i64::from(panel_len);
    let w = i64::from(window_len);
    if p <= w {
        return 0;
    }
    let gap = i64::from(CAROUSEL_WRAP_GAP);
    let ms_per_col = 10_000 / CAROUSEL_SPEED_X10;
    let scroll_to_tail_ms = ((p - w) * ms_per_col as i64) as u64;
    // The wrap (the tail exiting + the head re-entering) runs 3x faster
    // than the reveal. The cycle runs p + gap columns: the tail exits
    // over `w` columns and the head copy arrives over the remaining
    // `gap`.
    let wrap_ms_per_col = (ms_per_col / 3).max(1);
    let wrap_ms = ((w + gap) * wrap_ms_per_col as i64) as u64;
    let cycle_ms = 2 * CAROUSEL_PAUSE_MS + scroll_to_tail_ms + wrap_ms;
    let t = progress_ms % cycle_ms;
    if t < CAROUSEL_PAUSE_MS {
        0 // hold at the start
    } else if t < CAROUSEL_PAUSE_MS + scroll_to_tail_ms {
        ((t - CAROUSEL_PAUSE_MS) / ms_per_col) as i64 // 0 .. p-w (tail visible)
    } else if t < 2 * CAROUSEL_PAUSE_MS + scroll_to_tail_ms {
        p - w // hold at the end
    } else {
        // Continue past the tail to p + gap: the head copy at
        // o - (p + gap) lands back at offset 0 there, closing the
        // ticker loop.
        (p - w)
            + ((t - 2 * CAROUSEL_PAUSE_MS - scroll_to_tail_ms) / wrap_ms_per_col) as i64
    }
}

/// Draw a panel (text at strip positions [0, P)) with the window showing
/// strip columns [o, o + width): a negative `o` centers the panel during
/// the hold, and once `o >= P` the panel has fully exited.
pub(crate) fn draw_panel_at(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    panel: &Line,
    o: i64,
    style: Style,
) {
    let p = panel.width() as u16;
    if o < 0 {
        let gap = ((-o) as u16).min(width);
        if gap > 0 {
            buf.set_string(x, y, " ".repeat(usize::from(gap)), style);
        }
        let rest = width - gap;
        if rest > 0 {
            draw_spans(buf, x + gap, y, &panel.spans, 0, rest, style);
        }
    } else if (o as u16) < p {
        draw_spans(buf, x, y, &panel.spans, o as usize, width, style);
    }
    // o >= p: fully exited; leave the window blank until the next panel
}

/// Draw a styled line left-aligned into `width` columns at (x, y). `style`
/// is the base style, patched by each span's own style. (The marquee's fit
/// case never centers; the panes center by choosing `x` themselves.)
fn draw_line(buf: &mut Buffer, x: u16, y: u16, width: u16, line: &Line, style: Style) {
    if width == 0 || line.width() == 0 {
        return;
    }
    if line.width() as u16 <= width {
        let mut cx = x;
        for span in &line.spans {
            buf.set_string(cx, y, span.content.as_ref(), style.patch(span.style));
            cx += span.width() as u16;
        }
    } else {
        draw_spans(buf, x, y, &line.spans, 0, width, style);
    }
}

/// Draw `spans` starting `skip` columns in, for up to `max` columns,
/// patching each span's style over `style`. Returns the columns drawn.
fn draw_spans(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    spans: &[Span],
    skip: usize,
    max: u16,
    style: Style,
) -> u16 {
    let mut drawn = 0u16;
    let mut skip = skip;
    for span in spans {
        if drawn >= max {
            break;
        }
        let span_w = span.width();
        if skip >= span_w {
            skip -= span_w;
            continue;
        }
        let span_style = style.patch(span.style);
        let mut taken = String::new();
        let mut w = 0u16;
        for ch in span.content.chars() {
            let cw = ch.to_string().width();
            if skip > 0 {
                if skip >= cw {
                    skip -= cw;
                    continue;
                }
                skip = 0;
            }
            if w + cw as u16 > max - drawn {
                break;
            }
            taken.push(ch);
            w += cw as u16;
        }
        if !taken.is_empty() {
            buf.set_string(x + drawn, y, taken, span_style);
            drawn += w;
        }
    }
    drawn
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use ratatui::{layout::Rect, style::Style, text::Line};

    use super::*;

    /// During the wrap the tail is still visible while the head copy
    /// enters at the right edge, with the wrap gap between them — a
    /// continuous news-ticker, never a blank exit.
    #[test]
    fn marquee_wrap_shows_tail_gap_and_head_together() {
        let p = 20u16;
        let w = 10u16;
        let ms_per_col = 10_000 / CAROUSEL_SPEED_X10;
        let wrap_ms_per_col = (ms_per_col / 3).max(1);
        let scroll_ms = ((p - w) as u64) * ms_per_col;
        let t0 = 2 * CAROUSEL_PAUSE_MS + scroll_ms;
        // offset o = p - 4: the tail's last 4 columns are visible and the
        // head is entering at the right edge, 5 columns after the tail.
        let o = p as i64 - 4;
        let t = t0 + (o - (p - w) as i64) as u64 * wrap_ms_per_col;

        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, w, 1));
        let line = Line::from("ABCDEFGHIJKLMNOPQRST"); // 20 chars
        super::draw_marquee(&mut buf, 0, 0, w, &line, Style::new(), t);

        let row: String =
            (0..w).map(|x| buf[(x, 0)].symbol().chars().next().unwrap_or(' ')).collect();
        assert_eq!(
            row,
            "QRST     A",
            "tail (QRST), a {}-column gap, then the head (A)",
            CAROUSEL_WRAP_GAP
        );
    }

    #[test]
    fn marquee_offset_holds_at_both_ends_and_wraps_forward() {
        let p = 40u16; // panel (title) width
        let w = 20u16; // window width
        let ms_per_col = 10_000 / CAROUSEL_SPEED_X10;
        let scroll_ms = ((p - w) as u64) * ms_per_col; // scroll to the tail
        // The wrap runs 3x faster than the reveal; the cycle covers
        // w + CAROUSEL_WRAP_GAP columns (tail exit + head-copy arrival).
        let wrap_ms_per_col = (ms_per_col / 3).max(1);
        let wrap_ms = ((w + CAROUSEL_WRAP_GAP) as u64) * wrap_ms_per_col;
        let cycle_ms = 2 * CAROUSEL_PAUSE_MS + scroll_ms + wrap_ms;

        // Hold at the start.
        assert_eq!(super::marquee_offset(0, p, w), 0);
        assert_eq!(super::marquee_offset(CAROUSEL_PAUSE_MS - 1, p, w), 0);
        // Scroll left to the tail (window at the last columns of the title).
        assert_eq!(
            super::marquee_offset(CAROUSEL_PAUSE_MS + scroll_ms, p, w),
            (p - w) as i64
        );
        // Hold at the end.
        assert_eq!(
            super::marquee_offset(CAROUSEL_PAUSE_MS + scroll_ms + 1, p, w),
            (p - w) as i64
        );
        assert_eq!(
            super::marquee_offset(2 * CAROUSEL_PAUSE_MS + scroll_ms - 1, p, w),
            (p - w) as i64
        );
        // The wrap keeps scrolling left (3x faster than the reveal) and
        // stays non-negative: the offset grows past the tail so the head
        // copy (drawn by the caller at o - (p + CAROUSEL_WRAP_GAP)) follows
        // with a 5-column slack and lands back at 0.
        let t0 = 2 * CAROUSEL_PAUSE_MS + scroll_ms;
        assert_eq!(
            super::marquee_offset(t0 + wrap_ms_per_col, p, w),
            (p - w + 1) as i64
        );
        assert!(
            super::marquee_offset(t0 + w as u64 * wrap_ms_per_col, p, w) >= p as i64,
            "the wrap continues past the tail (no negative offsets)"
        );
        assert_eq!(
            super::marquee_offset(t0 + wrap_ms - 1, p, w),
            i64::from(p) + i64::from(CAROUSEL_WRAP_GAP) - 1,
            "the wrap runs up to p + gap (the head copy lands back at 0)"
        );
        // The cycle repeats from the start hold.
        assert_eq!(super::marquee_offset(cycle_ms, p, w), 0);
        assert_eq!(super::marquee_offset(cycle_ms + CAROUSEL_PAUSE_MS / 2, p, w), 0);
    }
}
