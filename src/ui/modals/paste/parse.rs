use crate::{
    config::utils::tilde_expand, shared::ytdlp::YtDlpContent,
};

use super::PastedItem;

/// File extensions considered audio (lowercase, no dot).
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3",
    "flac",
    "ogg",
    "opus",
    "oga",
    "m4a",
    "m4b",
    "aac",
    "wav",
    "wma",
    "ape",
    "alac",
    "aiff",
    "aif",
    "wv",
    "mka",
    "spx",
    "ac3",
];
/// File extensions considered video (lowercase, no dot).
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4",
    "m4v",
    "mkv",
    "webm",
    "mov",
    "avi",
    "mpg",
    "mpeg",
    "ts",
    "m2ts",
    "mts",
    "flv",
    "wmv",
    "vob",
    "ogv",
    "3gp",
    "divx",
];
/// Whether a path/URL carries an audio extension (query strings and
/// fragments stripped first, so `…/song.mp3?x=1` matches). Used by the
/// paste classification and the torrent file picker.
pub(crate) fn is_audio_extension(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}
/// Whether a path/URL carries a video extension (query strings and
/// fragments stripped first, so `…/movie.mp4?x=1` matches). Used by the
/// paste popup's classification and the playlist audio/video detection.
pub(crate) fn is_video_extension(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}
/// Whether a path/URL carries a torrent metainfo extension (`.torrent`,
/// query strings and fragments stripped first).
pub(crate) fn is_torrent_extension(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
}
/// A local path is a real `.torrent` file (~ expanded).
fn is_local_torrent(path: &str) -> bool {
    if !is_torrent_extension(path) {
        return false;
    }
    let expanded = tilde_expand(path);
    std::path::Path::new(expanded.as_ref()).is_file()
}
/// The full infohash of a magnet URI (lowercased): from
/// `xt=urn:btih:<hash>` or a bare `btih=<hash>` query parameter. `None`
/// when the magnet carries no recognizable infohash.
///
/// Round 20: this is the magnet's **canonical scan key** — the same
/// torrent pasted twice (even via a different magnet URI with extra
/// trackers) must hit the same `Ctx.torrent_scans` slot so the second
/// paste reuses the first engine instead of spawning a second rqbit
/// against the same cache dir.
pub(crate) fn magnet_infohash_full(magnet: &str) -> Option<String> {
    let query = magnet.split_once('?').map_or(magnet, |(_, q)| q);
    query
        .split('&')
        .find_map(|param| {
            param
                .strip_prefix("xt=urn:btih:")
                .or_else(|| param.strip_prefix("btih="))
                .map(|hash| hash.to_lowercase())
        })
}
/// The infohash prefix of a magnet URI (the first 8 characters, used for
/// labels): from `xt=urn:btih:<hash>` or a bare `btih=<hash>` query
/// parameter. `None` when the magnet carries no recognizable infohash.
pub(crate) fn magnet_infohash(magnet: &str) -> Option<String> {
    magnet_infohash_full(magnet).map(|hash| hash.chars().take(8).collect())
}
/// Unescape kitty's drag&drop escaping (`\ ` for spaces, `\\` for a
/// backslash) in a local path.
fn unescape_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(' ') => out.push(' '),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}
/// Decode `%XX` escapes (UTF-8 aware). Sequences that are malformed — a
/// lone `%`, a non-hex digit, or bytes that do not form valid UTF-8 — are
/// left exactly as they arrived, so a path that legitimately contains a
/// percent sign is never mangled. Round 90.
fn unescape_percent(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
            if let (Some(high), Some(low)) = (hex(bytes[idx + 1]), hex(bytes[idx + 2])) {
                out.push(high * 16 + low);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    match String::from_utf8(out) {
        Ok(decoded) => decoded,
        // Not valid UTF-8 once decoded (a truncated multi-byte escape):
        // keep the text as it was pasted.
        Err(_) => s.to_owned(),
    }
}
/// Split on whitespace while keeping backslash-escaped characters (kitty's
/// drag&drop escapes spaces as `\ `) and single- or double-quoted groups
/// (round 90: `"a path with spaces.flac"` is one token) inside one token.
fn split_unescaped(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                current.push(c);
                current.push(next);
                chars.next();
            } else {
                current.push(c);
            }
        } else if let Some(open) = quote {
            // Inside a quoted group quoting is literal (`'it''s'`); only
            // the matching quote closes it.
            current.push(c);
            if c == open {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            current.push(c);
        } else if c.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}
/// Split the pasted text into recognized audio items. Anything unrecognized
/// is silently ignored; the paste is dropped entirely when nothing matches.
///
/// Round 90: the text is processed one pasted LINE at a time, so an
/// unquoted path that contains spaces can still be recognized as a whole.
pub fn parse_paste(input: &str) -> Vec<PastedItem> {
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut seen_magnets = std::collections::HashSet::new();
    for line in input.split('\n') {
        for item in parse_paste_line(line) {
            let is_new = match &item {
                PastedItem::Magnet(magnet) => {
                    let key = magnet_infohash_full(magnet)
                        .unwrap_or_else(|| magnet.clone());
                    seen_magnets.insert(key)
                }
                _ => seen.insert(item.clone()),
            };
            if is_new {
                items.push(item);
            }
        }
    }
    items
}
/// The recognized items of a single pasted line: the whitespace/quoted
/// token split first, and — only when that found nothing at all — the
/// whole line as one token (the unquoted spaced path case, round 90).
fn parse_paste_line(line: &str) -> Vec<PastedItem> {
    let tokens: Vec<String> = split_unescaped(line)
        .iter()
        .filter_map(|raw| {
            let token = strip_quotes(raw).trim();
            (!token.is_empty()).then(|| token.to_owned())
        })
        .collect();
    let mut items = Vec::new();
    for token in &tokens {
        if let Some(item) = classify(token) {
            items.push(item);
        }
    }
    if items.is_empty() {
        return whole_line_item(line).into_iter().collect();
    }
    items
}
/// The whole line as ONE candidate (an unquoted path with spaces): trimmed,
/// quotes stripped, then backslash- and percent-unescaped. Round 90.
fn whole_line_item(line: &str) -> Option<PastedItem> {
    let candidate = strip_quotes(line.trim());
    let candidate = candidate.trim();
    ((candidate.contains(' ') || candidate.contains('\t')) && looks_like_path(candidate))
        .then(|| classify(&unescape_path(&unescape_percent(candidate))))
        .flatten()
}
/// Strip one layer of surrounding quotes (`"…"` or `'…'`).
fn strip_quotes(token: &str) -> &str {
    let trimmed = token.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(trimmed)
}
/// Whether a whole line carries an audio/video/torrent extension (query
/// strings/fragments and a `file://` prefix stripped): the guard that stops
/// an arbitrary prose line (a chat message) from being read as one path.
fn looks_like_path(candidate: &str) -> bool {
    let candidate = candidate.strip_prefix("file://").unwrap_or(candidate);
    let candidate = candidate.split(['?', '#']).next().unwrap_or(candidate);
    is_audio_extension(candidate)
        || is_video_extension(candidate)
        || is_torrent_extension(candidate)
}
/// Classify a single whitespace-separated token.
fn classify(token: &str) -> Option<PastedItem> {
    if token.starts_with("magnet:") {
        return Some(PastedItem::Magnet(token.to_owned()));
    }
    if let Some(rest) = token.strip_prefix("file://") {
        // Round 90: `file://` URIs arrive percent-encoded (spaces as
        // `%20`) — decode before the on-disk checks.
        let path = unescape_path(&unescape_percent(rest));
        if is_local_torrent(&path) {
            return Some(PastedItem::Torrent(path));
        }
        if is_local_audio(&path) {
            return Some(PastedItem::File(path));
        }
        if is_local_video(&path) {
            return Some(PastedItem::VideoFile(path));
        }
        return None;
    }
    // The extractors are known-host URLs, so accept them with or without a
    // scheme: links copied out of a chat line, a status bar or a plain-text
    // list often arrive as `youtu.be/ID` / `youtube.com/watch?v=ID`
    // (2026-09-10 — those pasted as nothing before). A local path never
    // parses as one of these hosts, so this cannot swallow file pastes.
    let candidate = if token.contains("://") {
        token.to_owned()
    } else {
        format!("https://{token}")
    };
    if candidate.parse::<YtDlpContent>().is_ok() {
        return Some(PastedItem::Yt(candidate));
    }
    if token.starts_with("http://") || token.starts_with("https://") {
        if is_torrent_extension(token) {
            return Some(PastedItem::Torrent(token.to_owned()));
        }
        if is_audio_extension(token) {
            return Some(PastedItem::Url(token.to_owned()));
        }
        if is_video_extension(token) {
            return Some(PastedItem::VideoUrl(token.to_owned()));
        }
        return None;
    }
    // Round 90: a plain path can carry `%XX` escapes too (a path dropped
    // or pasted from a URL-aware source).
    let path = unescape_path(&unescape_percent(token));
    if is_local_torrent(&path) {
        return Some(PastedItem::Torrent(path));
    }
    if is_local_audio(&path) {
        return Some(PastedItem::File(path));
    }
    if is_local_video(&path) {
        return Some(PastedItem::VideoFile(path));
    }
    None
}
/// A local path is a real file with an audio extension (~ expanded).
fn is_local_audio(path: &str) -> bool {
    if !is_audio_extension(path) {
        return false;
    }
    let expanded = tilde_expand(path);
    std::path::Path::new(expanded.as_ref()).is_file()
}
/// A local path is a real file with a video extension (~ expanded).
fn is_local_video(path: &str) -> bool {
    if !is_video_extension(path) {
        return false;
    }
    let expanded = tilde_expand(path);
    std::path::Path::new(expanded.as_ref()).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp dir with files to paste; `None` when it cannot be created.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "s2udio-paste-{name}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        /// Create an empty file and return its absolute path.
        fn file(&self, name: &str) -> String {
            let path = self.0.join(name);
            std::fs::write(&path, b"").expect("temp file");
            path.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn spaced_path_is_one_token() {
        // Round 90: dragging (or pasting) an unquoted path with spaces did
        // nothing because the tokenizer split it. The whole line is now
        // tried as one path when the split found nothing.
        let dir = TempDir::new("spaced");
        let path = dir.file("my song.flac");
        assert_eq!(parse_paste(&path), vec![PastedItem::File(path.clone())]);
        // Leading directories contain spaces too.
        assert_eq!(
            parse_paste(&format!("  {path}  ")),
            vec![PastedItem::File(path)]
        );
    }

    #[test]
    fn quoted_spaced_path_is_one_token() {
        let dir = TempDir::new("quoted");
        let path = dir.file("a song with spaces.flac");
        assert_eq!(
            parse_paste(&format!("\"{path}\"")),
            vec![PastedItem::File(path.clone())]
        );
        assert_eq!(
            parse_paste(&format!("'{path}'")),
            vec![PastedItem::File(path.clone())]
        );
        // Two quoted paths on one line stay two items.
        let other = dir.file("second song.mp3");
        assert_eq!(
            parse_paste(&format!("\"{path}\" \"{other}\"")),
            vec![PastedItem::File(path), PastedItem::File(other)]
        );
    }

    #[test]
    fn file_uri_decodes_percent_escapes() {
        let dir = TempDir::new("uri");
        let path = dir.file("uri song.flac");
        let encoded = path.replace(' ', "%20");
        assert_eq!(
            parse_paste(&format!("file://{encoded}")),
            vec![PastedItem::File(path)]
        );
        // UTF-8 escapes decode to the original characters.
        let utf8 = dir.file("M\u{e4}rchen.flac");
        let encoded = utf8.replace('\u{e4}', "%C3%A4");
        assert_eq!(
            parse_paste(&format!("file://{encoded}")),
            vec![PastedItem::File(utf8)]
        );
    }

    #[test]
    fn plain_path_decodes_percent_escapes() {
        let dir = TempDir::new("plain");
        let path = dir.file("plain song.flac");
        assert_eq!(
            parse_paste(&path.replace(' ', "%20")),
            vec![PastedItem::File(path.clone())]
        );
        // A malformed escape is left alone instead of being mangled.
        let weird = dir.file("100% song.flac");
        assert_eq!(
            parse_paste(&weird.replace(' ', "%20")),
            vec![PastedItem::File(weird)]
        );
    }

    #[test]
    fn backslash_escaped_path_still_works() {
        // Kitty's legacy drag&drop escaping (`\ ` for a space).
        let dir = TempDir::new("backslash");
        let path = dir.file("escaped song.flac");
        let escaped = path.replace(' ', "\\ ");
        assert_eq!(parse_paste(&escaped), vec![PastedItem::File(path)]);
    }

    #[test]
    fn multi_item_paste_keeps_every_item() {
        // One URL plus one spaced path: the whole-line fallback must not
        // swallow the URL's own line.
        let dir = TempDir::new("multi");
        let path = dir.file("another song.flac");
        let text = format!("https://example.com/song.mp3\n{path}");
        assert_eq!(
            parse_paste(&text),
            vec![
                PastedItem::Url("https://example.com/song.mp3".to_owned()),
                PastedItem::File(path),
            ]
        );
    }

    #[test]
    fn prose_with_spaces_stays_unrecognized() {
        // Guard: a chat line must never be read as one long path.
        assert!(parse_paste("hello there, how are you today?").is_empty());
        assert!(parse_paste("no such audio file here.flac").is_empty());
    }
}
