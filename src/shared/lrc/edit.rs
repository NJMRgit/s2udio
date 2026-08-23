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
        Self {
            path,
            raw,
            lines,
            pending: Vec::new(),
            structural: false,
            deleted_spans: Vec::new(),
        }
    }
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
    pub fn is_dirty(&self) -> bool {
        self.structural || !self.pending.is_empty() || self.lines.iter().any(|l| l.dirty)
    }
    /// Change one word's time in the session (written back on `save`).
    pub fn set_word_time(
        &mut self,
        line: usize,
        word: usize,
        time: Duration,
    ) -> Result<()> {
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
            anyhow::bail!(
                "insert position out of range: {position} > {}", self.lines.len()
            );
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
                (word + 1, prev + l.words[word + 1].time.saturating_sub(prev) / 2, false)
            } else {
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
            let prev = l.words[word - 1].time;
            let next = l.words[word].time;
            (word, prev + next.saturating_sub(prev) / 2, false)
        } else {
            let first = l.words[0].time;
            let time = first.saturating_sub(step);
            let extend = time < l.time;
            (word, time, extend)
        };
        let l = self.lines.get_mut(line).context("line out of range")?;
        if extend_tag {
            l.time = time;
        }
        l.words
            .insert(
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
        self.pending = std::mem::take(&mut self.pending)
            .into_iter()
            .map(|(l, w)| if l == line && w >= insert_at { (l, w + 1) } else { (l, w) })
            .collect();
        Ok(insert_at)
    }
    /// Round 41: delete a WORD from a line (pending until `save`, like
    /// the word nudges). The line re-renders with the remaining words'
    /// explicit markers; when the deleted word was the line's ONLY word
    /// the whole line is removed (blank lyric rows are never left
    /// behind). Returns `true` when the line itself was removed.
    pub fn delete_word_at(&mut self, line: usize, word: usize) -> Result<bool> {
        let l = self.lines.get_mut(line).context("line out of range")?;
        if word >= l.words.len() {
            anyhow::bail!("word out of range");
        }
        l.words.remove(word);
        l.content = l.words.iter().map(|w| w.text.clone()).collect::<Vec<_>>().join(" ");
        l.dirty = true;
        self.pending.retain(|&(l, w)| !(l == line && w == word));
        for (l, w) in &mut self.pending {
            if *l == line && *w > word {
                *w -= 1;
            }
        }
        if l.words.is_empty() {
            self.delete_line(line)?;
            return Ok(true);
        }
        Ok(false)
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
    pub fn apply_to_raw(
        raw: &str,
        line: usize,
        word: usize,
        time: Duration,
    ) -> Result<String> {
        let session = Self::open(PathBuf::new(), raw.to_owned());
        session.apply_word_time(line, word, time)
    }
    /// The new raw text with one word's marker replaced (or inserted for
    /// an interpolated word).
    fn apply_word_time(
        &self,
        line: usize,
        word: usize,
        time: Duration,
    ) -> Result<String> {
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
    fn apply_word_pending(
        &self,
        line: usize,
        start: usize,
        end: usize,
    ) -> Result<String> {
        let mut edits = Vec::new();
        for &(l, w) in &self.pending {
            if l != line {
                continue;
            }
            let lr = self.lines.get(l).context("line out of range")?;
            let wr = lr.words.get(w).context("word out of range")?;
            let (ws, we) = self.span_of(l, w)?;
            if ws >= start && we <= end {
                edits
                    .push((
                        ws - start,
                        we - start,
                        format!("<{}>", Self::format_time(wr.time)),
                    ));
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
        let consumed = gt + 1;
        let is_timestamp = tag.chars().next().is_some_and(|c| c.is_ascii_digit())
            && tag.contains(':');
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
    let tail = &trimmed[pos..];
    let tail_lead = tail.len() - tail.trim_start().len();
    let content_raw = tail.trim();
    let content_base = line_start + lead + pos + tail_lead;
    let (content, words) = parse_words(content_raw, content_base);
    Some(EditableLine {
        time: line_time,
        content,
        words,
        raw_span: None,
        dirty: false,
    })
}
/// Split a line's content on inline `<time>` markers, producing the
/// cleaned content and the editable words (mirroring the LRC parser's
/// segment/interpolation algorithm so the edit view matches the karaoke
/// view word-for-word).
fn parse_words(content_raw: &str, base: usize) -> (String, Vec<EditableWord>) {
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
            segments
                .push((
                    prev_time,
                    text.to_owned(),
                    last_marker,
                    (base + pos, base + pos + lt),
                ));
        }
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else {
            valid = false;
            break;
        };
        let Some(time) = parse_timestamp_raw(&after[..gt]) else {
            valid = false;
            break;
        };
        found = true;
        last_marker = Some((base + pos + lt, base + pos + lt + 1 + gt + 1));
        prev_time = time;
        pos += lt + 1 + gt + 1;
        rest = &after[gt + 1..];
    }
    if !valid || !found {
        return (content_raw.to_owned(), Vec::new());
    }
    if !rest.trim().is_empty() {
        segments
            .push((
                prev_time,
                rest.to_owned(),
                last_marker,
                (base + pos, base + pos + rest.len()),
            ));
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
        words
            .push(EditableWord {
                text: ws[0].0.clone(),
                time: *seg_time,
                marker: *marker,
                text_span: Some(ws[0].1),
            });
        let span = end.saturating_sub(*seg_time);
        let total: usize = ws[1..].iter().map(|(w, _)| w.width()).sum::<usize>().max(1);
        let mut acc = 0usize;
        for (w, span_abs) in &ws[1..] {
            words
                .push(EditableWord {
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
fn split_words_with_spans(
    text: &str,
    base: (usize, usize),
) -> Vec<(String, (usize, usize))> {
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
        let time = if i == 0 {
            start
        } else {
            start + span.mul_f64(acc as f64 / total as f64)
        };
        acc += w.width();
        out.push(EditableWord {
            text: w.to_owned(),
            time,
            marker: None,
            text_span: None,
        });
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
