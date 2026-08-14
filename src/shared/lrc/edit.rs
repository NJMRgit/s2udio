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
}

impl LrcEditSession {
    /// Open an editing session over `raw` (the file at `path`). Never
    /// fails: any text parses (lines without a valid structure simply
    /// produce no editable lines).
    pub fn open(path: PathBuf, raw: String) -> Self {
        let lines = parse_lines(&raw);
        Self { path, raw, lines, pending: Vec::new() }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn is_dirty(&self) -> bool {
        !self.pending.is_empty()
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

    /// Write every pending edit back to the file (atomic replace), then
    /// rebuild the session from the saved text. No-op when nothing is
    /// pending.
    pub fn save(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let new_raw = self.render_pending()?;
        Self::write_atomic(&self.path, &new_raw)?;
        self.raw = new_raw;
        self.lines = parse_lines(&self.raw);
        self.pending.clear();
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

    /// Apply all pending edits to the current raw text (descending byte
    /// order keeps earlier offsets valid).
    fn render_pending(&self) -> Result<String> {
        let mut edits = Vec::new();
        for &(line, word) in &self.pending {
            let l = self.lines.get(line).context("line out of range")?;
            let w = l.words.get(word).context("word out of range")?;
            let (start, end) = self.span_of(line, word)?;
            edits.push((start, end, format!("<{}>", Self::format_time(w.time))));
        }
        edits.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
        let mut new_raw = self.raw.clone();
        for (start, end, marker) in edits {
            new_raw.replace_range(start..end, &marker);
        }
        Ok(new_raw)
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
        if let Some(parsed) = parse_line(line, start) {
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
    Some(EditableLine { time: line_time, content, words })
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
}
