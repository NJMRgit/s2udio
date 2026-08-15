use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use unicode_width::UnicodeWidthStr;

/// A single editable word of a timed lyric line, as the lyrics editor sees
/// it: the displayed text, its time in the FILE (raw, pre-offset), and —
/// for words that came from an explicit `<mm:ss.xx>` marker — the byte
/// span of that marker inside the raw file text. Interpolated words (the
/// remaining words of a segment between two markers, timed proportionally
/// to display width) have `marker == None`: editing them inserts a new
/// explicit marker in front of the word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableWord {
    pub text: String,
    pub time: Duration,
    /// Byte span of the `<mm:ss.xx>` marker in the raw file (for words
    /// with explicit markers); `None` for interpolated words.
    pub marker: Option<(usize, usize)>,
    /// Byte span of the word's text in the raw file (marker insertion
    /// point for interpolated words).
    pub text_span: Option<(usize, usize)>,
}

/// One timed line of the raw LRC file as the editor sees it. Lines
/// without any inline `<mm:ss.xx>` word markers keep an empty `words`
/// list: they are displayed plainly and skipped by word navigation.
#[derive(Debug, Clone)]
pub struct EditableLine {
    /// The line's timestamp (first `[mm:ss.xx]` tag), raw.
    pub time: Duration,
    /// The displayed content (inline word markers stripped).
    pub content: String,
    /// The editable words (explicit markers + interpolated), in display
    /// order.
    pub words: Vec<EditableWord>,
    /// Byte span of the full raw line (including its line ending) in the
    /// file text the session was opened on. `None` for lines inserted
    /// during this session (they are re-rendered on save).
    pub raw_span: Option<(usize, usize)>,
    /// The line changed structurally since open (text or line time), so
    /// `save` re-renders it from the model instead of emitting the raw
    /// line verbatim.
    pub dirty: bool,
}

/// An in-memory editing session over one `.lrc` file. The raw file text is
/// kept verbatim: write-back replaces only the changed word markers (or
/// inserts new ones for previously interpolated words), preserving the
/// header, the `# lrcgen-gap-align:v1` stamp line, line structure and the
/// enhanced `<mm:ss.xx>` format.
#[derive(Debug, Clone)]
pub struct LrcEditSession {
    path: PathBuf,
    raw: String,
    /// The editable lines, in raw file order (one per timed raw line).
    pub lines: Vec<EditableLine>,
    /// (line, word) pairs whose time changed since the last save.
    pending: Vec<(usize, usize)>,
    /// A line was removed since the last save (removed lines leave
    /// `lines`, so `is_dirty` cannot see them — this flag remembers).
    structural: bool,
    /// Raw byte spans of the lines deleted since the last save, so
    /// `render` can skip them during the gap-fill (everything between
    /// the surviving lines' chunks passes through verbatim otherwise).
    deleted_spans: Vec<(usize, usize)>,
}

impl LrcEditSession {
    /// Open an editing session over `raw` (the file at `path`). Never
    /// fails: any text parses (lines without a valid structure simply
    /// produce no editable lines).
    pub fn open(path: PathBuf, raw: String) -> Self {
        let lines = parse_lines(&raw);
        Self { path, raw, lines, pending: Vec::new(), structural: false, deleted_spans: Vec::new() }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn is_dirty(&self) -> bool {
        self.structural || !self.pending.is_empty() || self.lines.iter().any(|l| l.dirty)
    }

    /// Change one word's time in the session (written back on `save`).
    pub fn set_word_time(&mut self, line: usize, word: usize, time: Duration) -> Result<()> {
        let l = self.lines.get_mut(line).context("line out of range")?;
        let w = l.words.get_mut(word).context("word out of range")?;
        w.time = time;
        if !self.pending.contains(&(line, word)) {
            self.pending.push((line, word));
        }
        Ok(())
    }

    /// Round-35 line-level edits (all pending until `save`, like the
    /// word nudges): delete a line, replace a line's text, set a line's
    /// timestamp, insert a new line after an existing one.
    pub fn delete_line(&mut self, index: usize) -> Result<()> {
        let line = self.lines.get(index).context("line out of range")?;
        if let Some(span) = line.raw_span {
            self.deleted_spans.push(span);
        }
        self.lines.remove(index);
        self.structural = true;
        self.pending.retain(|&(l, _)| l != index);
        for (l, _) in &mut self.pending {
            if *l > index {
                *l -= 1;
            }
        }
        Ok(())
    }

    /// Replace a line's text (marker-stripped content). Word timings are
    /// kept word-for-word when the word count is unchanged; otherwise the
    /// new words interpolate from the line's timestamp to the next line's
    /// timestamp (explicit markers are written on save).
    pub fn set_line_text(&mut self, index: usize, text: &str) -> Result<()> {
        let content = text.trim().to_owned();
        let (line_time, end) = {
            let line = self.lines.get(index).context("line out of range")?;
            let end = self
                .lines
                .get(index + 1)
                .map(|l| l.time)
                .filter(|t| *t > line.time)
                .unwrap_or(line.time + Duration::from_secs(5));
            (line.time, end)
        };
        let line = self.lines.get_mut(index).context("line out of range")?;
        let old_words = std::mem::take(&mut line.words);
        let new_words: Vec<&str> = content.split_whitespace().collect();
        line.words = if !old_words.is_empty() && old_words.len() == new_words.len() {
            new_words
                .iter()
                .zip(old_words.iter())
                .map(|(w, ow)| EditableWord {
                    text: (*w).to_owned(),
                    time: ow.time,
                    marker: None,
                    text_span: None,
                })
                .collect()
        } else {
            interpolate_words(&content, line_time, end)
        };
        line.content = content;
        line.dirty = true;
        self.pending.retain(|&(l, _)| l != index);
        Ok(())
    }

    /// Set a line's timestamp (the `[mm:ss.xx]` tag). Word markers are
    /// untouched — they stay the karaoke sync points.
    pub fn set_line_time(&mut self, index: usize, time: Duration) -> Result<()> {
        let line = self.lines.get_mut(index).context("line out of range")?;
        line.time = time;
        line.dirty = true;
        Ok(())
    }

    /// Insert a new empty line at `position` (`0..=len`, i.e. after the
    /// line at `position - 1`; pending until `save`); the caller sets its
    /// text via `set_line_text`. Returns the new line's index.
    pub fn insert_line_at(&mut self, position: usize, time: Duration) -> Result<usize> {
        if position > self.lines.len() {
            anyhow::bail!("insert position out of range: {position} > {}", self.lines.len());
        }
        let line = EditableLine {
            time,
            content: String::new(),
            words: Vec::new(),
            raw_span: None,
            dirty: true,
        };
        self.lines.insert(position, line);
        for (l, _) in &mut self.pending {
            if *l >= position {
                *l += 1;
            }
        }
        Ok(position)
    }

    /// Round 40/41: insert a new WORD into a line (the insert is per
    /// word, not per line — everything stays on the same line).
    /// `after == false` inserts before the word at `word`, `after ==
    /// true` after it.
    ///
    /// Timing: mid-line the new word's time interpolates between its
    /// neighbours (the previous word and the next word). At the line's
    /// edges — where a midpoint has nothing to anchor to — the word is
    /// placed 100 ms before the first word / after the last word so it
    /// gets its own karaoke moment instead of sharing (or floating in)
    /// the boundary's timing: before the FIRST word the line's
    /// timestamp is moved earlier to match, so the new word lights on
    /// screen before the original first word; after the LAST word the
    /// word lands 100 ms on (capped by the next line's midpoint when
    /// the next line starts too soon).
    ///
    /// The line is marked dirty so `save` re-renders it with explicit
    /// `<mm:ss.xx>` markers. Returns the new word's index. Errors when
    /// the line has no words to insert into.
    pub fn insert_word_at(
        &mut self,
        line: usize,
        word: usize,
        after: bool,
        text: &str,
    ) -> Result<usize> {
        let step = Duration::from_millis(100);
        let l = self.lines.get(line).context("line out of range")?;
        if l.words.is_empty() {
            anyhow::bail!("line has no words to insert into");
        }
        if word >= l.words.len() {
            anyhow::bail!("word out of range");
        }
        let (insert_at, time, extend_tag) = if after {
            let prev = l.words[word].time;
            if word + 1 < l.words.len() {
                // Mid-line: midpoint between this word and the next.
                (word + 1, prev + l.words[word + 1].time.saturating_sub(prev) / 2, false)
            } else {
                // After the LAST word: its own moment 100 ms on, capped
                // by the midpoint toward the next line when the next
                // line starts too soon.
                let next_line = self
                    .lines
                    .get(line + 1)
                    .map(|l| l.time)
                    .filter(|t| *t > prev);
                let time = match next_line {
                    Some(next) if next.saturating_sub(prev) <= step => {
                        prev + next.saturating_sub(prev) / 2
                    }
                    _ => prev + step,
                };
                (word + 1, time, false)
            }
        } else if word > 0 {
            // Mid-line: midpoint between the previous word and this one.
            let prev = l.words[word - 1].time;
            let next = l.words[word].time;
            (word, prev + next.saturating_sub(prev) / 2, false)
        } else {
            // Before the FIRST word: its own moment 100 ms before the
            // first word (the line tag extends earlier to match when the
            // line currently starts exactly at the first word).
            let first = l.words[0].time;
            let time = first.saturating_sub(step);
            let extend = time < l.time;
            (word, time, extend)
        };
        let l = self.lines.get_mut(line).context("line out of range")?;
        if extend_tag {
            l.time = time;
        }
        l.words.insert(
            insert_at,
            EditableWord {
                text: text.trim().to_owned(),
                time,
                marker: None,
                text_span: None,
            },
        );
        l.content = l.words.iter().map(|w| w.text.clone()).collect::<Vec<_>>().join(" ");
        l.dirty = true;
        // Pending edits on the same line after the insertion shift by one.
        self.pending = std::mem::take(&mut self.pending)
            .into_iter()
            .map(|(l, w)| if l == line && w >= insert_at { (l, w + 1) } else { (l, w) })
            .collect();
        Ok(insert_at)
    }

    /// Write every pending edit back to the file (atomic replace), then
    /// rebuild the session from the saved text. No-op when nothing is
    /// pending.
    pub fn save(&mut self) -> Result<()> {
        if !self.is_dirty() {
            return Ok(());
        }
        let new_raw = self.render_pending()?;
        Self::write_atomic(&self.path, &new_raw)?;
        self.raw = new_raw;
        self.lines = parse_lines(&self.raw);
        self.pending.clear();
        self.structural = false;
        self.deleted_spans.clear();
        Ok(())
    }

    /// Apply a single word-time edit to `raw` and return the new text
    /// (the file is not touched). Used by the exact-value popup, which
    /// writes from the saved file state so it cannot race pending nudges.
    pub fn apply_to_raw(raw: &str, line: usize, word: usize, time: Duration) -> Result<String> {
        let session = Self::open(PathBuf::new(), raw.to_owned());
        session.apply_word_time(line, word, time)
    }

    /// The new raw text with one word's marker replaced (or inserted for
    /// an interpolated word).
    fn apply_word_time(&self, line: usize, word: usize, time: Duration) -> Result<String> {
        let (start, end) = self.span_of(line, word)?;
        let mut new_raw = self.raw.clone();
        new_raw.replace_range(start..end, &format!("<{}>", Self::format_time(time)));
        Ok(new_raw)
    }

    /// The raw byte range to replace for a word: its marker, or the
    /// insertion point just before an interpolated word's text.
    fn span_of(&self, line: usize, word: usize) -> Result<(usize, usize)> {
        let l = self.lines.get(line).context("line out of range")?;
        let w = l.words.get(word).context("word out of range")?;
        if let Some((start, end)) = w.marker {
            return Ok((start, end));
        }
        let (start, _) = w.text_span.context("word has no position in the file")?;
        Ok((start, start))
    }

    /// Apply all pending edits to the current raw text. Lines that
    /// changed structurally are re-rendered from the model; unchanged
    /// lines are emitted verbatim with only their changed word markers
    /// rewritten (byte-descending order keeps earlier offsets valid).
    fn render_pending(&self) -> Result<String> {
        let mut out = String::new();
        let mut cursor = 0usize;
        // Round 39: lines without a raw span (inserted/split this
        // session) are buffered and emitted right after the raw gap that
        // precedes the next surviving raw line — so a line split before
        // the FIRST lyric still lands after the header/metadata, never
        // in front of it.
        let mut pending_lines: Vec<&EditableLine> = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            match line.raw_span {
                Some((start, end)) => {
                    if start > cursor {
                        out.push_str(&self.gap_without_deleted(cursor, start));
                    }
                    for inserted in pending_lines.drain(..) {
                        out.push_str(&Self::render_line(inserted));
                    }
                    if line.dirty {
                        out.push_str(&Self::render_line(line));
                    } else {
                        out.push_str(&self.apply_word_pending(i, start, end)?);
                    }
                    cursor = end;
                }
                None => pending_lines.push(line),
            }
        }
        for inserted in pending_lines.drain(..) {
            out.push_str(&Self::render_line(inserted));
        }
        if cursor < self.raw.len() {
            out.push_str(&self.gap_without_deleted(cursor, self.raw.len()));
        }
        Ok(out)
    }

    /// The raw text in `[from, to)` with the deleted lines' chunks
    /// removed (header, blank and metadata lines pass through verbatim).
    fn gap_without_deleted(&self, from: usize, to: usize) -> String {
        let mut gap = self.raw[from..to].to_owned();
        let mut spans: Vec<&(usize, usize)> = self
            .deleted_spans
            .iter()
            .filter(|(ds, de)| *ds >= from && *de <= to)
            .collect();
        spans.sort_by_key(|(ds, _)| std::cmp::Reverse(*ds));
        for (ds, de) in spans {
            gap.replace_range(ds - from..de - from, "");
        }
        gap
    }

    /// Apply the pending word-time edits belonging to `line` to its
    /// verbatim raw chunk (offsets relative to the chunk).
    fn apply_word_pending(&self, line: usize, start: usize, end: usize) -> Result<String> {
        let mut edits = Vec::new();
        for &(l, w) in &self.pending {
            if l != line {
                continue;
            }
            let lr = self.lines.get(l).context("line out of range")?;
            let wr = lr.words.get(w).context("word out of range")?;
            let (ws, we) = self.span_of(l, w)?;
            if ws >= start && we <= end {
                edits.push((ws - start, we - start, format!("<{}>", Self::format_time(wr.time))));
            }
        }
        edits.sort_by_key(|(s, _, _)| std::cmp::Reverse(*s));
        let mut chunk = self.raw[start..end].to_owned();
        for (s, e, marker) in edits {
            chunk.replace_range(s..e, &marker);
        }
        Ok(chunk)
    }

    /// Render one line from the model: `[mm:ss.xx]` + either the plain
    /// content or every word as an explicit `<mm:ss.xx>` marker.
    fn render_line(line: &EditableLine) -> String {
        let tag = Self::format_time(line.time);
        if line.words.is_empty() {
            format!("[{tag}]{}
", line.content)
        } else {
            let body = line
                .words
                .iter()
                .map(|w| format!("<{}>{}", Self::format_time(w.time), w.text))
                .collect::<Vec<_>>()
                .join(" ");
            format!("[{tag}]{body}
")
        }
    }

    /// Format a duration as the enhanced-LRC word marker time `mm:ss.xx`
    /// (hundredths of a second).
    pub fn format_time(time: Duration) -> String {
        let total_cs = time.as_millis() / 10;
        let cs = total_cs % 100;
        let total_s = total_cs / 100;
        let s = total_s % 60;
        let m = total_s / 60;
        format!("{m:02}:{s:02}.{cs:02}")
    }

    /// Parse a user-typed time: `mm:ss.xx` (also `mm:ss:xx`, `mm:ss`).
    pub fn parse_time(input: &str) -> Option<Duration> {
        parse_timestamp_raw(input.trim())
    }

    /// Write `raw` to `path` atomically (temp file + rename), so a
    /// crash mid-write cannot truncate the lyrics file.
    pub fn write_atomic(path: &Path, raw: &str) -> Result<()> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "lyrics".to_owned());
        let tmp = path.with_file_name(format!(".{name}.s2u-edit.tmp"));
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Parse the raw file text into editable lines (one per timed raw line).
fn parse_lines(raw: &str) -> Vec<EditableLine> {
    let mut lines = Vec::new();
    let mut offset = 0usize;
    for chunk in raw.split_inclusive('\n') {
        let start = offset;
        let end = offset + chunk.len();
        offset = end;
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(mut parsed) = parse_line(line, start) {
            parsed.raw_span = Some((start, end));
            lines.push(parsed);
        }
    }
    lines
}

/// Parse one raw line into an editable line. `None` when the line has no
/// leading timestamp tag (metadata header, comment, blank).
fn parse_line(line: &str, line_start: usize) -> Option<EditableLine> {
    // Mirror the LRC parser's tag scanning: leading `[mm:ss.xx]` tags are
    // the line timestamps; scanning stops at the first non-timestamp tag
    // and the rest of the line becomes content.
    let lead = line.len() - line.trim_start().len();
    let trimmed = &line[lead..];
    let mut pos = 0usize;
    let mut line_time = None;
    loop {
        let rest = &trimmed[pos..];
        if !rest.starts_with('[') {
            break;
        }
        let Some(gt) = rest.find(']') else { break };
        let tag = &rest[1..gt];
        // The tag spans `[` (index 0) through `]` (index `gt`), so the
        // content starts at `gt + 1`.
        let consumed = gt + 1;
        let is_timestamp =
            tag.chars().next().is_some_and(|c| c.is_ascii_digit()) && tag.contains(':');
        if !is_timestamp {
            break;
        }
        let Some(time) = parse_timestamp_raw(tag) else { break };
        if line_time.is_none() {
            line_time = Some(time);
        }
        pos += consumed;
    }
    let line_time = line_time?;

    // The content portion: the tail after the tags, trimmed like the LRC
    // parser trims it (`&tail[..].trim()`), with markers intact.
    let tail = &trimmed[pos..];
    let tail_lead = tail.len() - tail.trim_start().len();
    let content_raw = tail.trim();
    let content_base = line_start + lead + pos + tail_lead;

    let (content, words) = parse_words(content_raw, content_base);
    Some(EditableLine { time: line_time, content, words, raw_span: None, dirty: false })
}

/// Split a line's content on inline `<time>` markers, producing the
/// cleaned content and the editable words (mirroring the LRC parser's
/// segment/interpolation algorithm so the edit view matches the karaoke
/// view word-for-word).
fn parse_words(content_raw: &str, base: usize) -> (String, Vec<EditableWord>) {
    // (time, text, first-word marker span, text span in raw coords)
    let mut segments: Vec<(Duration, String, Option<(usize, usize)>, (usize, usize))> = Vec::new();
    let mut prev_time = Duration::ZERO;
    let mut last_marker: Option<(usize, usize)> = None;
    let mut rest = content_raw;
    let mut pos = 0usize;
    let mut found = false;
    let mut valid = true;
    while let Some(lt) = rest.find('<') {
        let text = &rest[..lt];
        if !text.trim().is_empty() {
            segments.push((prev_time, text.to_owned(), last_marker, (base + pos, base + pos + lt)));
        }
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else {
            // A `<` without a closing `>`: not a marker line at all.
            valid = false;
            break;
        };
        let Some(time) = parse_timestamp_raw(&after[..gt]) else {
            valid = false;
            break;
        };
        found = true;
        // The `<...>` span, brackets included.
        last_marker = Some((base + pos + lt, base + pos + lt + 1 + gt + 1));
        prev_time = time;
        pos += lt + 1 + gt + 1;
        rest = &after[gt + 1..];
    }
    if !valid || !found {
        // No valid word markers: a plain line, content shown verbatim.
        return (content_raw.to_owned(), Vec::new());
    }
    if !rest.trim().is_empty() {
        segments.push((prev_time, rest.to_owned(), last_marker, (base + pos, base + pos + rest.len())));
    }

    let mut content = String::new();
    let mut words = Vec::new();
    for (i, (seg_time, text, marker, text_span)) in segments.iter().enumerate() {
        content.push_str(text);
        let end = segments.get(i + 1).map_or(*seg_time, |s| s.0);
        let ws = split_words_with_spans(text, *text_span);
        if ws.is_empty() {
            continue;
        }
        words.push(EditableWord {
            text: ws[0].0.clone(),
            time: *seg_time,
            marker: *marker,
            text_span: Some(ws[0].1),
        });
        // The remaining words of the segment interpolate proportionally
        // to display width up to the next segment's time (mirroring
        // `push_interpolated` in the LRC parser).
        let span = end.saturating_sub(*seg_time);
        let total: usize = ws[1..].iter().map(|(w, _)| w.width()).sum::<usize>().max(1);
        let mut acc = 0usize;
        for (w, span_abs) in &ws[1..] {
            words.push(EditableWord {
                text: w.clone(),
                time: *seg_time + span.mul_f64(acc as f64 / total as f64),
                marker: None,
                text_span: Some(*span_abs),
            });
            acc += w.width();
        }
    }
    (content, words)
}

/// Split a text segment into words with their absolute byte spans
/// (whitespace-separated, matching `split_whitespace` closely enough for
/// lyric text).
fn split_words_with_spans(text: &str, base: (usize, usize)) -> Vec<(String, (usize, usize))> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if i > start {
                out.push((text[start..i].to_owned(), (base.0 + start, base.0 + i)));
            }
            start = i + c.len_utf8();
        }
    }
    if start < text.len() {
        out.push((text[start..].to_owned(), (base.0 + start, base.0 + text.len())));
    }
    out
}

/// Time the words of a user-typed lyric line: the first word starts at
/// `start`, the rest interpolate proportionally to display width up to
/// `end` (mirroring the parser's segment interpolation). All words come
/// back without markers — the line is re-rendered with explicit markers
/// on save.
fn interpolate_words(text: &str, start: Duration, end: Duration) -> Vec<EditableWord> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    let span = end.saturating_sub(start);
    let total: usize = words.iter().map(|w| w.width()).sum::<usize>().max(1);
    let mut acc = 0usize;
    let mut out = Vec::with_capacity(words.len());
    for (i, w) in words.into_iter().enumerate() {
        let time = if i == 0 { start } else { start + span.mul_f64(acc as f64 / total as f64) };
        acc += w.width();
        out.push(EditableWord { text: w.to_owned(), time, marker: None, text_span: None });
    }
    out
}

/// Parse an LRC timestamp (`mm:ss.xx`, `mm:ss:xx`, `mm:ss`) without any
/// `[offset:]` adjustment — the file's raw time.
fn parse_timestamp_raw(tag: &str) -> Option<Duration> {
    let (minutes, rest) = tag.split_once(':')?;
    let (seconds, frac) = rest
        .split_once('.')
        .or_else(|| rest.split_once(':'))
        .map_or((rest, None), |(a, b)| (a, Some(b)));
    let minutes = minutes.trim().parse::<u64>().ok()?;
    let seconds = seconds.trim().parse::<u64>().ok()?;
    let millis = match frac {
        Some(f) => {
            let f = f.trim();
            if f.is_empty() {
                return None;
            }
            let digits = f.len();
            let value = f.parse::<u64>().ok()?;
            if digits >= 3 {
                value / 10u64.pow(digits as u32 - 3)
            } else {
                value * 10u64.pow(3 - digits as u32)
            }
        }
        None => 0,
    };
    Some(Duration::from_millis(minutes * 60_000 + seconds * 1000 + millis))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[ti:Test]\n[ar:Artist]\n# lrcgen-gap-align:v1\n\n[00:01.00]<00:01.20>hello <00:01.40>world\n[00:02.00]plain line\n[00:03.00]<00:03.10>a <00:03.30>b <00:03.50>c\n";

    #[test]
    fn parses_marker_words_with_spans() {
        let session = LrcEditSession::open(PathBuf::new(), SAMPLE.to_owned());
        assert_eq!(session.lines.len(), 3);
        let l0 = &session.lines[0];
        assert_eq!(l0.content, "hello world");
        assert_eq!(l0.words.len(), 2);
        assert_eq!(l0.words[0].text, "hello");
        assert_eq!(l0.words[0].time, Duration::from_millis(1200));
        let m1 = SAMPLE.find("<00:01.20>").unwrap();
        assert_eq!(l0.words[0].marker, Some((m1, m1 + 10)));
        assert_eq!(l0.words[1].text, "world");
        assert_eq!(l0.words[1].time, Duration::from_millis(1400));
        let m2 = SAMPLE.find("<00:01.40>").unwrap();
        assert_eq!(l0.words[1].marker, Some((m2, m2 + 10)));
        // plain line: no words
        assert!(session.lines[1].words.is_empty());
        assert_eq!(session.lines[1].content, "plain line");
    }

    #[test]
    fn parses_interpolated_words_without_markers() {
        let raw = "[00:01.00]<00:01.20>hello world <00:02.00>next\n";
        let session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        let l0 = &session.lines[0];
        assert_eq!(l0.words.len(), 3);
        assert_eq!(l0.words[0].text, "hello");
        assert_eq!(l0.words[0].time, Duration::from_millis(1200));
        assert!(l0.words[0].marker.is_some());
        assert_eq!(l0.words[1].text, "world");
        assert!(l0.words[1].marker.is_none());
        assert!(l0.words[1].text_span.is_some());
        assert_eq!(l0.words[2].text, "next");
        assert_eq!(l0.words[2].time, Duration::from_secs(2));
    }

    #[test]
    fn save_replaces_only_the_changed_marker() {
        let raw = "[ti:X]\n# lrcgen-gap-align:v1\n[00:01.00]<00:01.20>hello <00:01.40>world\n[00:02.00]plain\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        session.set_word_time(0, 0, Duration::from_millis(1350)).unwrap();
        let new_raw = session.render_pending().unwrap();
        assert_eq!(
            new_raw,
            "[ti:X]\n# lrcgen-gap-align:v1\n[00:01.00]<00:01.35>hello <00:01.40>world\n[00:02.00]plain\n"
        );
        // Only that marker changed: header, stamp, line structure intact.
        assert!(new_raw.starts_with("[ti:X]\n# lrcgen-gap-align:v1\n"));
        assert!(new_raw.contains("<00:01.40>world"));
        assert!(new_raw.contains("[00:02.00]plain"));
    }

    #[test]
    fn save_promotes_an_interpolated_word_to_a_marker() {
        let raw = "[00:01.00]<00:01.20>hello world <00:02.00>next\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        session.set_word_time(0, 1, Duration::from_millis(1500)).unwrap();
        let new_raw = session.render_pending().unwrap();
        assert_eq!(
            new_raw,
            "[00:01.00]<00:01.20>hello <00:01.50>world <00:02.00>next\n"
        );
    }

    #[test]
    fn multiple_nudges_to_one_word_save_the_latest_time() {
        let raw = "[00:01.00]<00:01.20>hello\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        session.set_word_time(0, 0, Duration::from_millis(1210)).unwrap();
        session.set_word_time(0, 0, Duration::from_millis(1230)).unwrap();
        assert_eq!(session.pending.len(), 1, "deduped");
        let new_raw = session.render_pending().unwrap();
        assert_eq!(new_raw, "[00:01.00]<00:01.23>hello\n");
    }

    #[test]
    fn save_writes_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("s2u-lrc-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.lrc");
        let raw = "[ti:X]\n[00:01.00]<00:01.20>hello\n";
        std::fs::write(&path, raw).unwrap();
        let mut session = LrcEditSession::open(path.clone(), raw.to_owned());
        session.set_word_time(0, 0, Duration::from_millis(1990)).unwrap();
        session.save().unwrap();
        assert!(!session.is_dirty());
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "[ti:X]\n[00:01.00]<00:01.99>hello\n");
        // Rebuilt session reflects the saved file.
        assert_eq!(session.lines[0].words[0].time, Duration::from_millis(1990));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_to_raw_edits_one_marker_without_writing() {
        let raw = "[00:01.00]<00:01.20>hello\n";
        let new_raw = LrcEditSession::apply_to_raw(raw, 0, 0, Duration::from_millis(900)).unwrap();
        assert_eq!(new_raw, "[00:01.00]<00:00.90>hello\n");
    }

    #[test]
    fn format_and_parse_times_round_trip() {
        for ms in [0u64, 10, 990, 1000, 61_990, 7_200_000] {
            let d = Duration::from_millis(ms);
            let s = LrcEditSession::format_time(d);
            assert_eq!(LrcEditSession::parse_time(&s), Some(d), "round trip {s}");
        }
        assert_eq!(LrcEditSession::format_time(Duration::from_millis(61_990)), "01:01.99");
        assert_eq!(LrcEditSession::parse_time("1:02.5"), Some(Duration::from_millis(62_500)));
        assert_eq!(LrcEditSession::parse_time("0:00.005"), Some(Duration::from_millis(5)));
        assert_eq!(LrcEditSession::parse_time("2:05"), Some(Duration::from_millis(125_000)));
        assert_eq!(LrcEditSession::parse_time("nope"), None);
    }

    #[test]
    fn lines_without_timestamps_are_skipped() {
        let raw = "[ti:X]\n# stamp\n\n[00:01.00]line\n";
        let session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        assert_eq!(session.lines.len(), 1);
        assert_eq!(session.lines[0].content, "line");
    }

    #[test]
    fn crlf_lines_are_handled() {
        let raw = "[00:01.00]<00:01.20>hello\r\n[00:02.00]plain\r\n";
        let session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        assert_eq!(session.lines.len(), 2);
        let m = "[00:01.00]<00:01.20>hello".find("<00:01.20>").unwrap();
        assert_eq!(session.lines[0].words[0].marker, Some((m, m + 10)));
    }

    #[test]
    fn parsed_lines_record_their_raw_and_tag_spans() {
        let raw = "[ti:X]\n# lrcgen-gap-align:v1\n[00:01.00]<00:01.20>hello\n";
        let session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        assert_eq!(session.lines.len(), 1);
        let l0 = &session.lines[0];
        let raw_line = "[00:01.00]<00:01.20>hello\n";
        let start = raw.find(raw_line).unwrap();
        assert_eq!(l0.raw_span, Some((start, start + raw_line.len())));
        assert!(!l0.dirty);
    }

    #[test]
    fn delete_line_removes_the_line_and_remaps_pending() {
        let raw = "[00:01.00]<00:01.10>a\n[00:02.00]<00:02.10>b\n[00:03.00]<00:03.10>c\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        session.set_word_time(2, 0, Duration::from_millis(3100)).unwrap();
        session.delete_line(0).unwrap();
        assert_eq!(session.lines.len(), 2);
        assert_eq!(session.lines[0].content, "b");
        // The pending edit followed the shifted line.
        assert_eq!(session.pending, vec![(1, 0)]);
        let new_raw = session.render_pending().unwrap();
        assert_eq!(new_raw, "[00:02.00]<00:02.10>b\n[00:03.00]<00:03.10>c\n");
    }

    #[test]
    fn set_line_text_keeps_word_times_when_the_count_matches() {
        let raw = "[00:01.00]<00:01.20>hello <00:01.40>world\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        session.set_line_text(0, "hello there").unwrap();
        assert_eq!(session.lines[0].content, "hello there");
        assert_eq!(session.lines[0].words.len(), 2);
        assert_eq!(session.lines[0].words[0].text, "hello");
        assert_eq!(session.lines[0].words[0].time, Duration::from_millis(1200));
        assert_eq!(session.lines[0].words[1].text, "there");
        assert_eq!(session.lines[0].words[1].time, Duration::from_millis(1400));
        let new_raw = session.render_pending().unwrap();
        assert_eq!(new_raw, "[00:01.00]<00:01.20>hello <00:01.40>there\n");
    }

    #[test]
    fn set_line_text_reinterpolates_when_the_count_changes() {
        let raw = "[00:01.00]<00:01.20>hello\n[00:03.00]next\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        session.set_line_text(0, "a b c d").unwrap();
        let words = &session.lines[0].words;
        assert_eq!(words.len(), 4);
        assert_eq!(words[0].time, Duration::from_millis(1000));
        // The rest interpolate across the 2 s span to the next line.
        assert!(words[1].time > Duration::from_millis(1000));
        assert!(words[3].time < Duration::from_millis(3000));
        let new_raw = session.render_pending().unwrap();
        assert!(new_raw.starts_with("[00:01.00]<00:01.00>a <00:01."), "got {new_raw}");
        assert!(new_raw.ends_with(">d\n[00:03.00]next\n"));
    }

    #[test]
    fn set_line_time_rewrites_the_tag_keeping_the_words() {
        let raw = "[00:01.00]<00:01.20>hello <00:01.40>world\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        session.set_line_time(0, Duration::from_millis(2500)).unwrap();
        let new_raw = session.render_pending().unwrap();
        assert_eq!(new_raw, "[00:02.50]<00:01.20>hello <00:01.40>world\n");
    }

    #[test]
    fn insert_line_adds_a_line_after_the_anchor() {
        let raw = "[00:01.00]<00:01.10>a\n[00:03.00]<00:03.10>b\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        let idx = session.insert_line_at(1, Duration::from_millis(2000)).unwrap();
        assert_eq!(idx, 1);
        session.set_line_text(idx, "middle").unwrap();
        let new_raw = session.render_pending().unwrap();
        assert_eq!(new_raw, "[00:01.00]<00:01.10>a\n[00:02.00]<00:02.00>middle\n[00:03.00]<00:03.10>b\n");
    }

    #[test]
    fn insert_word_before_interpolates_between_the_neighbours() {
        // hello @1.20, world @1.40 — insert "there" before world: it
        // lands between the two, timed at the midpoint (1.30), and the
        // whole line re-renders with explicit markers.
        let raw = "[ti:X]\n# lrcgen-gap-align:v1\n\n[00:01.00]<00:01.20>hello <00:01.40>world\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        let idx = session.insert_word_at(0, 1, false, "there").unwrap();
        assert_eq!(idx, 1);
        assert_eq!(session.lines.len(), 1, "still ONE line — the word joins the same line");
        let words = &session.lines[0].words;
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].text, "hello");
        assert_eq!(words[1].text, "there");
        assert_eq!(words[1].time, Duration::from_millis(1300));
        assert_eq!(words[2].text, "world");
        assert_eq!(words[2].time, Duration::from_millis(1400));
        assert_eq!(session.lines[0].content, "hello there world");
        assert!(session.lines[0].dirty);
        let new_raw = session.render_pending().unwrap();
        assert_eq!(
            new_raw,
            "[ti:X]\n# lrcgen-gap-align:v1\n\n[00:01.00]<00:01.20>hello <00:01.30>there <00:01.40>world\n"
        );
    }

    #[test]
    fn insert_word_after_interpolates_between_the_neighbours() {
        // Insert "there" after hello (word 0): between 1.20 and 1.40.
        let raw = "[00:01.00]<00:01.20>hello <00:01.40>world\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        let idx = session.insert_word_at(0, 0, true, "there").unwrap();
        assert_eq!(idx, 1);
        let words = &session.lines[0].words;
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].text, "hello");
        assert_eq!(words[1].text, "there");
        assert_eq!(words[1].time, Duration::from_millis(1300));
        let new_raw = session.render_pending().unwrap();
        assert_eq!(new_raw, "[00:01.00]<00:01.20>hello <00:01.30>there <00:01.40>world\n");
    }

    #[test]
    fn insert_word_at_the_line_edges_uses_line_or_next_line_times() {
        // Before the FIRST word: interpolate from the line's timestamp.
        let raw = "[00:01.00]<00:01.20>hello\n[00:03.00]<00:03.10>next\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        let idx = session.insert_word_at(0, 0, false, "intro").unwrap();
        assert_eq!(idx, 0);
        assert_eq!(session.lines[0].words[0].text, "intro");
        assert_eq!(session.lines[0].words[0].time, Duration::from_millis(1100));
        // After the LAST word (hello @1.20, now index 1): its own
        // moment 100 ms on (1.30) — not floating at the midpoint of the
        // 1.8 s gap to the next line.
        let idx = session.insert_word_at(0, 1, true, "outro").unwrap();
        assert_eq!(idx, 2);
        assert_eq!(session.lines[0].words[2].time, Duration::from_millis(1300));
        // Last word of the LAST line: 100 ms on as well.
        let mut session = LrcEditSession::open(PathBuf::new(), "[00:01.00]<00:01.20>hello\n".to_owned());
        session.insert_word_at(0, 0, true, "tail").unwrap();
        assert_eq!(session.lines[0].words[1].time, Duration::from_millis(1300));
    }

    #[test]
    fn insert_word_before_the_first_word_gets_its_own_moment() {
        // Line starts exactly at the first word (1.45): the inserted word
        // is placed 100 ms before it AND the line tag extends earlier so
        // the karaoke lights the new word before the original first word.
        let raw = "[00:01.45]<00:01.45>I <00:02.63>still\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        let idx = session.insert_word_at(0, 0, false, "ALPHA").unwrap();
        assert_eq!(idx, 0);
        assert_eq!(session.lines[0].time, Duration::from_millis(1350), "tag extends earlier");
        assert_eq!(session.lines[0].words[0].time, Duration::from_millis(1350));
        assert_eq!(session.lines[0].words[1].time, Duration::from_millis(1450));
        let new_raw = session.render_pending().unwrap();
        assert_eq!(new_raw, "[00:01.35]<00:01.35>ALPHA <00:01.45>I <00:02.63>still\n");
    }

    #[test]
    fn insert_word_requires_a_worded_line_and_valid_word() {
        let raw = "[00:01.00]<00:01.20>hello\n[00:02.00]plain\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        assert!(session.insert_word_at(1, 0, false, "x").is_err(), "wordless line");
        assert!(session.insert_word_at(0, 5, false, "x").is_err(), "word out of range");
        assert_eq!(session.lines.len(), 2, "no partial state on error");
    }

    #[test]
    fn insert_word_remaps_pending_edits_after_the_insertion() {
        let raw = "[00:01.00]<00:01.20>hello <00:01.40>world <00:01.60>again\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        session.set_word_time(0, 2, Duration::from_millis(1700)).unwrap();
        let idx = session.insert_word_at(0, 0, true, "there").unwrap();
        assert_eq!(idx, 1);
        // The pending edit on "again" shifted from word 2 to word 3.
        assert_eq!(session.pending, vec![(0, 3)]);
        let new_raw = session.render_pending().unwrap();
        assert_eq!(
            new_raw,
            "[00:01.00]<00:01.20>hello <00:01.30>there <00:01.40>world <00:01.70>again\n"
        );
    }

    #[test]
    fn save_splices_structural_edits_and_keeps_the_header_and_stamp() {
        let dir = std::env::temp_dir().join(format!("s2u-lrc-edit-r35-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.lrc");
        let raw = "[ti:Test]\n[ar:Artist]\n# lrcgen-gap-align:v1\n\n[00:01.00]<00:01.20>hello <00:01.40>world\n[00:02.00]plain line\n[00:03.00]<00:03.10>a <00:03.30>b <00:03.50>c\n";
        std::fs::write(&path, raw).unwrap();
        let mut session = LrcEditSession::open(path.clone(), raw.to_owned());
        // Structural: delete line 1 (plain), rewrite the last line's
        // text, insert a line after line 0, nudge a word on line 0.
        session.delete_line(1).unwrap();
        session.set_line_text(1, "x y").unwrap();
        let ins = session.insert_line_at(1, Duration::from_millis(1500)).unwrap();
        session.set_line_text(ins, "new line").unwrap();
        session.set_word_time(0, 0, Duration::from_millis(1250)).unwrap();
        assert!(session.is_dirty());
        session.save().unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.starts_with("[ti:Test]\n[ar:Artist]\n# lrcgen-gap-align:v1\n\n"));
        assert_eq!(
            on_disk,
            "[ti:Test]\n[ar:Artist]\n# lrcgen-gap-align:v1\n\n[00:01.00]<00:01.25>hello <00:01.40>world\n[00:01.50]<00:01.50>new <00:02.14>line\n[00:03.00]<00:03.00>x <00:05.50>y\n"
        );
        // The rebuilt session reflects the saved file (header lines are
        // still not editable lines).
        assert_eq!(session.lines.len(), 3);
        assert!(!session.is_dirty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deleting_every_line_keeps_the_header_verbatim() {
        let raw = "[ti:Test]\n# lrcgen-gap-align:v1\n[00:01.00]one\n[00:02.00]two\n";
        let mut session = LrcEditSession::open(PathBuf::new(), raw.to_owned());
        session.delete_line(1).unwrap();
        session.delete_line(0).unwrap();
        let new_raw = session.render_pending().unwrap();
        assert_eq!(new_raw, "[ti:Test]\n# lrcgen-gap-align:v1\n");
    }
}
