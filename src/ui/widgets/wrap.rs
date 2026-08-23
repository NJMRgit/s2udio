//! Text wrapping helpers shared by the lyrics pane and the jellyfin info
//! box: display-width-aware paragraph wrapping (`wrap_to_width`) and
//! span-aware row wrapping (`wrap_spans`).
use ratatui::{layout::Alignment, style::Style, text::{Line, Span}};
/// Wrap a paragraph into lines of at most `width` cells, breaking on
/// whitespace (display-width aware, so CJK text doesn't overflow).
pub(crate) fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        let mut current_w = 0usize;
        for word in paragraph.split_whitespace() {
            let word_w = word.width();
            let sep = usize::from(!current.is_empty());
            if current_w + sep + word_w > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_w = 0;
            }
            if !current.is_empty() {
                current.push(' ');
                current_w += 1;
            }
            current.push_str(word);
            current_w += word_w;
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}
/// Wrap a sequence of spans into rows of at most `width` columns, joining
/// words with single spaces and centering each row.
pub(crate) fn wrap_spans(spans: &[Span], width: u16) -> Vec<Line<'static>> {
    if spans.is_empty() {
        return Vec::new();
    }
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut row_w: usize = 0;
    for span in spans {
        let w = span.width();
        let space_w = usize::from(!row.is_empty());
        if !row.is_empty() && row_w + space_w + w > usize::from(width) {
            rows.push(Line::from(std::mem::take(&mut row)).alignment(Alignment::Center));
            row_w = 0;
        }
        if !row.is_empty() {
            row.push(Span::styled(" ", Style::default()));
            row_w += 1;
        }
        row.push(Span::styled(span.content.clone().into_owned(), span.style));
        row_w += w;
    }
    if !row.is_empty() {
        rows.push(Line::from(row).alignment(Alignment::Center));
    }
    rows
}
