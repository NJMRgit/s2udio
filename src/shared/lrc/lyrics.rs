use std::{io::BufRead, str::FromStr, time::Duration};
use anyhow::Result;
use serde::Serialize;
use unicode_width::UnicodeWidthStr;
use super::{LrcOffset, parse_length};
/// Result of parsing a single tag from an LRC line.
#[derive(Debug, Clone)]
enum TagParseResult {
    /// timestamp tag (e.g.: [00:12.34]) with the timestamp content
    Timestamp(String),
    /// metadata tag (e.g.: [ti:Song Title]) with key and value
    Metadata(String, String),
    /// invalid or unrecognized tag
    Invalid,
}
/// Parse a single tag from a line starting with '['.
/// Returns the tag content and the number of characters consumed.
fn parse_next_tag(line: &str) -> Option<(TagParseResult, usize)> {
    if !line.starts_with('[') {
        return None;
    }
    let mut bracket_count = 0;
    let mut close_pos = None;
    for (i, c) in line[1..].char_indices() {
        match c {
            '[' => bracket_count += 1,
            ']' => {
                if bracket_count == 0 {
                    close_pos = Some(i);
                    break;
                }
                bracket_count -= 1;
            }
            _ => {}
        }
    }
    let close_pos = close_pos?;
    let tag_content = &line[1..=close_pos];
    let chars_consumed = close_pos + 2;
    let tag_result = if is_timestamp_tag(tag_content) {
        TagParseResult::Timestamp(tag_content.to_string())
    } else if let Some((key, value)) = tag_content.split_once(':') {
        TagParseResult::Metadata(key.trim().to_string(), value.trim().to_string())
    } else {
        TagParseResult::Invalid
    };
    Some((tag_result, chars_consumed))
}
/// Checks if a tag content represents a timestamp (starts with digit and
/// contains ':').
fn is_timestamp_tag(tag_content: &str) -> bool {
    tag_content.chars().next().is_some_and(|c| c.is_numeric())
        && tag_content.contains(':')
}
/// A single word of a lyrics line together with the time at which it should
/// be considered sung (karaoke-style word highlighting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedWord {
    pub text: String,
    pub time: Duration,
}
impl Lrc {
    /// Word timing for the line at `line_idx`, karaoke-style.
    ///
    /// Returns `Some` only when the line carries inline `<mm:ss.xx>` word
    /// timestamps (enhanced LRC, extracted at parse time). Lines without
    /// such markers return `None` so the pane can fall back to whole-line
    /// highlighting.
    pub fn timed_words(
        &self,
        line_idx: usize,
        offset: LrcOffset,
    ) -> Option<Vec<TimedWord>> {
        let line = self.lines.get(line_idx)?;
        let word_times = line.word_times.as_ref()?;
        Some(
            word_times
                .iter()
                .map(|(t, text)| TimedWord {
                    text: text.clone(),
                    time: if offset.negative {
                        *t + offset.value
                    } else {
                        t.saturating_sub(offset.value)
                    },
                })
                .collect(),
        )
    }
}
fn words_of(content: &str) -> Vec<String> {
    content.split_whitespace().map(str::to_owned).collect()
}
/// Append `words` to `out`, each getting the time at which it starts being
/// sung, distributed proportionally to display width between `from` and `to`.
fn push_interpolated(
    out: &mut Vec<(Duration, String)>,
    words: &[String],
    from: Duration,
    to: Duration,
) {
    if words.is_empty() {
        return;
    }
    let span = to.saturating_sub(from);
    let total: usize = words.iter().map(|w| w.width()).sum::<usize>().max(1);
    let mut acc = 0usize;
    for w in words {
        out.push((from + span.mul_f64(acc as f64 / total as f64), w.clone()));
        acc += w.width();
    }
}
/// Extract inline `<mm:ss.xx>` word markers (enhanced LRC) from a line's
/// content, removing them from the displayed text. The first word of each
/// `<time>text` segment starts at that time; any remaining words of the
/// segment interpolate up to the next segment's time (or take the segment
/// time for the last segment). Returns `(cleaned_text, word_times)` where
/// `word_times` is `None` when the line carries no markers at all.
fn extract_word_times(content: &str) -> (String, Option<Vec<(Duration, String)>>) {
    match parse_word_segments(content) {
        Some((cleaned, words)) => (cleaned, Some(words)),
        None => (content.to_owned(), None),
    }
}
/// Split enhanced-LRC content on inline `<time>` markers.
/// Returns `(cleaned_text, word_times)` or `None` if there are no valid
/// markers (in which case the content is left untouched).
fn parse_word_segments(content: &str) -> Option<(String, Vec<(Duration, String)>)> {
    let mut segments: Vec<(Duration, String)> = Vec::new();
    let mut prev_time = Duration::ZERO;
    let mut rest = content;
    let mut found = false;
    while let Some(lt) = rest.find('<') {
        let (text, after) = rest.split_at(lt);
        if !text.trim().is_empty() {
            segments.push((prev_time, text.to_owned()));
        }
        let after = &after[1..];
        let gt = after.find('>')?;
        let time = parse_timestamp(&after[..gt], None)?;
        found = true;
        prev_time = time;
        rest = &after[gt + 1..];
    }
    if !rest.trim().is_empty() {
        segments.push((prev_time, rest.to_owned()));
    }
    if !found {
        return None;
    }
    let cleaned = segments.iter().map(|(_, text)| text.as_str()).collect::<String>();
    let mut out = Vec::new();
    for (i, (seg_time, text)) in segments.iter().enumerate() {
        let end = segments.get(i + 1).map_or(*seg_time, |(t, _)| *t);
        let words = words_of(text);
        if words.is_empty() {
            continue;
        }
        out.push((*seg_time, words[0].clone()));
        push_interpolated(&mut out, &words[1..], *seg_time, end);
    }
    Some((cleaned, out))
}
/// Parse a timestamp string into a Duration.
fn parse_timestamp(timestamp: &str, offset: Option<i64>) -> Option<Duration> {
    let (minutes, time_rest) = timestamp.split_once(':')?;
    let (seconds, fractions_of_second) = time_rest
        .split_once('.')
        .or_else(|| time_rest.split_once(':'))?;
    let fractions_of_second = &fractions_of_second[..3.min(fractions_of_second.len())];
    let (minutes, seconds, fractions) = (
        minutes.parse::<u64>().ok()?,
        seconds.parse::<u64>().ok()?,
        fractions_of_second.parse::<u64>().ok()?,
    );
    let mut millis = 0;
    millis += minutes * 60 * 1000;
    millis += seconds * 1000;
    millis
        += fractions
            * (10u64.pow(3 - u32::try_from(fractions_of_second.len()).unwrap_or(0)));
    millis = match offset {
        Some(offset) if offset > 0 => millis.saturating_sub(offset.unsigned_abs()),
        Some(offset) if offset < 0 => millis.saturating_add(offset.unsigned_abs()),
        _ => millis,
    };
    Some(Duration::from_millis(millis))
}
/// A single line of LRC lyrics with its timestamp.
#[derive(Debug, Eq, PartialEq)]
pub struct LrcLine {
    /// The timestamp when this line should be displayed
    time: Duration,
    /// The lyrics content for this line (inline word markers removed)
    pub content: String,
    /// Inline `<mm:ss.xx>` word timestamps (enhanced LRC), raw (pre-offset).
    /// Each entry is the time at which the word starts being sung.
    word_times: Option<Vec<(Duration, String)>>,
}
impl LrcLine {
    pub fn time(&self, offset: LrcOffset) -> Duration {
        if offset.negative {
            self.time.saturating_add(offset.value)
        } else {
            self.time.saturating_sub(offset.value)
        }
    }
}
/// Parsed LRC file containing metadata and timed lyrics lines.
#[derive(Debug, Eq, PartialEq)]
pub struct Lrc {
    /// The timed lyrics lines, sorted by timestamp
    pub lines: Vec<LrcLine>,
    /// Song title (from [ti:] tag)
    pub title: Option<String>,
    /// Artist name (from [ar:] tag)
    pub artist: Option<String>,
    /// Album name (from [al:] tag)
    pub album: Option<String>,
    /// Author/lyricist name (from [au:] tag)
    pub author: Option<String>,
    /// Song length (from [length:] tag)
    pub length: Option<Duration>,
}
/// Efficiently parse only metadata from LRC content, stopping at the first
/// timestamp. and returning the line index where lyrics start.
pub fn parse_metadata_only(content: &str) -> (LrcMetadata, usize) {
    let mut metadata = LrcMetadata::default();
    for (line_idx, line) in content.lines().enumerate() {
        let line_content = line.trim();
        if line_content.is_empty() || line_content.starts_with('#') {
            continue;
        }
        if !line_content.starts_with('[') {
            continue;
        }
        let mut remaining = line_content;
        let mut found_timestamp = false;
        while let Some((tag_result, chars_consumed)) = parse_next_tag(remaining) {
            match tag_result {
                TagParseResult::Timestamp(_) => {
                    found_timestamp = true;
                    break;
                }
                TagParseResult::Metadata(key, value) => {
                    match key.as_str() {
                        "ti" => metadata.title = Some(value),
                        "ar" => metadata.artist = Some(value),
                        "al" => metadata.album = Some(value),
                        "au" => metadata.author = Some(value),
                        "length" => {
                            if let Ok(parsed_length) = parse_length(&value) {
                                metadata.length = Some(parsed_length);
                            }
                        }
                        "offset" => {
                            if let Ok(parsed_offset) = value.parse::<i64>() {
                                metadata.offset = Some(parsed_offset);
                            }
                        }
                        _ => {}
                    }
                }
                TagParseResult::Invalid => {}
            }
            remaining = &remaining[chars_consumed..];
            if !remaining.starts_with('[') {
                break;
            }
        }
        if found_timestamp {
            return (metadata, line_idx);
        }
    }
    (metadata, content.lines().count())
}
/// Metadata extracted from LRC file header tags.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct LrcMetadata {
    /// Song title (from [ti:] tag)
    pub title: Option<String>,
    /// Artist name (from [ar:] tag)
    pub artist: Option<String>,
    /// Album name (from [al:] tag)
    pub album: Option<String>,
    /// Author/lyricist name (from [au:] tag)
    pub author: Option<String>,
    /// Song length (from [length:] tag)
    pub length: Option<Duration>,
    /// Timing offset in milliseconds (from [offset:] tag)
    pub offset: Option<i64>,
}
impl LrcMetadata {
    pub(super) fn read(mut read: impl BufRead) -> Result<Option<Self>> {
        let mut content = String::new();
        let mut line = String::new();
        loop {
            if read.read_line(&mut line)? == 0 {
                break;
            }
            let trimmed = line.trim();
            if !trimmed.is_empty() && trimmed.starts_with('[')
                && let Some(bracket_end) = trimmed.find(']')
            {
                let tag_content = &trimmed[1..bracket_end];
                if tag_content.chars().next().is_some_and(|c| c.is_numeric())
                    && tag_content.contains(':')
                {
                    content.push_str(&line);
                    break;
                }
            }
            content.push_str(&line);
            line.clear();
        }
        let (metadata, _) = parse_metadata_only(&content);
        Ok(Some(metadata))
    }
}
impl FromStr for Lrc {
    type Err = anyhow::Error;
    /// Parse a complete LRC file from string content.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (metadata, lyrics_start_line) = parse_metadata_only(s);
        let offset = metadata.offset;
        let remaining_lines = s.lines().count().saturating_sub(lyrics_start_line);
        let estimated_capacity = remaining_lines * 2;
        let mut result = Self {
            lines: Vec::with_capacity(estimated_capacity),
            title: metadata.title,
            artist: metadata.artist,
            album: metadata.album,
            author: metadata.author,
            length: metadata.length,
        };
        for line in s.lines().skip(lyrics_start_line) {
            let line_content = line.trim();
            if line_content.is_empty() || line_content.starts_with('#') {
                continue;
            }
            if !line_content.starts_with('[') {
                continue;
            }
            let mut timestamps = Vec::new();
            let mut remaining = line_content;
            let mut lyrics_start_pos = 0;
            while let Some((tag_result, chars_consumed)) = parse_next_tag(remaining) {
                match tag_result {
                    TagParseResult::Timestamp(timestamp) => {
                        timestamps.push(timestamp);
                        lyrics_start_pos += chars_consumed;
                        remaining = &remaining[chars_consumed..];
                        if !remaining.starts_with('[') {
                            break;
                        }
                    }
                    TagParseResult::Metadata(_, _) | TagParseResult::Invalid => {
                        break;
                    }
                }
            }
            let lyrics_text = if lyrics_start_pos < line_content.len() {
                &line_content[lyrics_start_pos..]
            } else {
                remaining
            }
                .trim();
            if timestamps.is_empty() {
                continue;
            }
            let (clean_text, word_times) = extract_word_times(lyrics_text);
            for timestamp_content in timestamps {
                if let Some(time) = parse_timestamp(&timestamp_content, offset) {
                    result
                        .lines
                        .push(LrcLine {
                            time,
                            content: clean_text.clone(),
                            word_times: word_times.clone(),
                        });
                }
            }
        }
        Ok(result)
    }
}
