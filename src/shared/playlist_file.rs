//! Playlist-file parsing for the library playlist files (`.m3u` / `.pls`
//! / `.xspf`) listed in the Playlists tab (Settings > MPD "show library
//! playlist files").
//!
//! Why the app parses the files itself instead of asking MPD: MPD only
//! expands a library playlist file located at the music directory root
//! (and a stored playlist shadows a same-named library file); nested
//! files and `add <path>` do nothing. The library files are read-only:
//! the app only lists / opens / enqueues them, never edits or deletes.
//!
//! The radio favourites parser (`ui/panes/radio.rs::parse_m3u`) delegates
//! to [`parse_m3u`]; the radio pane keeps its EXTINF write path.

use std::{collections::HashSet, path::Path};

use anyhow::{Context, Result, bail};

/// One ordered entry of a parsed playlist file: an optional title (and
/// `.xspf` creator / duration metadata) plus the URI to enqueue
/// (library-relative path, absolute path, or URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEntry {
    /// Display title from the playlist metadata (`#EXTINF` title, `.pls`
    /// `Title<n>`, `.xspf` `<title>`).
    pub title: Option<String>,
    /// `.xspf` `<creator>` (the only format carrying an artist).
    pub artist: Option<String>,
    /// The raw entry URI as written in the file (see [`mpd_uri`]).
    pub uri: String,
    /// `.xspf` `<duration>` in milliseconds (the only format carrying a
    /// length); `None` for the other formats / streams.
    pub duration_ms: Option<u64>,
}

/// The recognized library playlist-file extensions (lowercase, with dot).
pub const PLAYLIST_EXTS: [&str; 3] = [".m3u", ".pls", ".xspf"];

/// Whether a library-relative path is a library playlist file: the name
/// ends in `.m3u` / `.pls` / `.xspf` (case-insensitive), `.m3u8` / HLS
/// deliberately excluded.
pub fn is_library_playlist_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    !lower.ends_with(".m3u8") && PLAYLIST_EXTS.iter().any(|ext| lower.ends_with(ext))
}

/// The lowercase extension (with dot) of a library playlist path, or
/// `None` when it is not a recognized playlist file.
pub fn playlist_extension(path: &str) -> Option<&'static str> {
    if !is_library_playlist_path(path) {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    PLAYLIST_EXTS.iter().copied().find(|ext| lower.ends_with(ext))
}

/// The display name of a library playlist file: its file name without the
/// extension (a stored playlist of the same name is listed side by side,
/// marked differently). `B/Sub/x.m3u` -> `x`.
pub fn playlist_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    if let Some((stem, ext)) = name.rsplit_once('.') {
        let ext = ext.to_ascii_lowercase();
        if ["m3u", "pls", "xspf"].contains(&ext.as_str()) {
            return stem.to_owned();
        }
    }
    name.to_owned()
}

/// Resolve a raw playlist entry into the MPD URI to enqueue.
///
/// - URLs (`scheme://`) and absolute paths pass through unchanged;
/// - relative paths resolve against the directory the playlist file
///   itself lives in (`file_dir`, e.g. `Album/Sub`; the empty string
///   for root-level files). Nested album `.m3u` files write
///   file-relative entries (`01 - Track.flac` next to the file), so a
///   bare name inside `Album/Sub/x.m3u` becomes
///   `Album/Sub/01 - Track.flac`; root-level files keep the identity
///   mapping (file-dir == library root). `.` and `..` segments are
///   normalized, clamped at the library root.
pub fn mpd_uri(uri: &str, file_dir: &str) -> String {
    let uri = uri.trim();
    if uri.contains("://") || uri.starts_with('/') {
        return uri.to_owned();
    }
    let mut parts: Vec<&str> = if file_dir.is_empty() {
        Vec::new()
    } else {
        file_dir.split('/').collect()
    };
    for segment in uri.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            segment => parts.push(segment),
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("/")
    }
}

/// Parse playlist content by its extension (`.m3u` / `.pls` / `.xspf`);
/// unknown extensions yield an empty list.
pub fn parse_playlist_file(content: &str, ext: &str) -> Vec<PlaylistEntry> {
    match ext {
        ".m3u" => parse_m3u(content),
        ".pls" => parse_pls(content),
        ".xspf" => parse_xspf(content),
        _ => Vec::new(),
    }
}

/// Parse an `.m3u` file: plain text, one entry per line, optional
/// `#EXTM3U` header and `#EXTINF:<length>,<title>` per entry (length may
/// be `-1` for streams). Blank lines and comments are skipped; duplicate
/// URIs keep their first occurrence.
pub fn parse_m3u(content: &str) -> Vec<PlaylistEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut pending_title: Option<String> = None;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending_title = Some(
                rest.splitn(2, ',').nth(1).unwrap_or_default().trim().to_string(),
            );
            continue;
        }
        if line.starts_with('#') {
            continue; // #EXTM3U header and any other comment
        }
        let title = pending_title.take().filter(|t| !t.is_empty());
        if seen.insert(line.to_owned()) {
            entries.push(PlaylistEntry {
                title,
                artist: None,
                uri: line.to_owned(),
                duration_ms: None,
            });
        }
    }
    entries
}

/// Parse a `.pls` file: an INI-style `[playlist]` section with numbered
/// `File<n>` / `Title<n>` keys (`Length<n>` is ignored) — entries may be
/// local paths or stream URLs; streams are common. Ordered by the entry
/// number; duplicates keep the first occurrence.
pub fn parse_pls(content: &str) -> Vec<PlaylistEntry> {
    use std::collections::BTreeMap;
    let mut files: BTreeMap<u32, String> = BTreeMap::new();
    let mut titles: BTreeMap<u32, String> = BTreeMap::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(['[', ';', '#']) {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        for prefix in ["file", "title"] {
            if let Some(num) = key.strip_prefix(prefix)
                && let Ok(num) = num.parse::<u32>()
            {
                match prefix {
                    "file" => files.insert(num, value),
                    _ => titles.insert(num, value),
                };
                break;
            }
        }
    }
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for (num, uri) in files {
        if !seen.insert(uri.clone()) {
            continue;
        }
        let title = titles.remove(&num).filter(|t| !t.is_empty());
        entries.push(PlaylistEntry {
            title,
            artist: None,
            uri,
            duration_ms: None,
        });
    }
    entries
}

/// Parse an `.xspf` file (XML): a `<playlist><trackList>` of `<track>`
/// elements with `<location>` (required) plus optional `<title>` /
/// `<creator>` / `<duration>` (milliseconds) metadata. Entries may be
/// local paths or URLs. The format is simple enough that a light
/// tag-scanner suffices (no full XML parser dependency); standard XML
/// entities are decoded.
pub fn parse_xspf(content: &str) -> Vec<PlaylistEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut rest = content;
    while let Some(start) = rest.find("<track") {
        // `<trackList` also matches (it starts with `<track`); the segment
        // then runs to the first `</track>`, which still yields the same
        // first-track content — safe for this flat format.
        let Some(close_rel) = rest[start..].find("</track>") else {
            break;
        };
        let end = start + close_rel + "</track>".len();
        let segment = &rest[start..end];
        if let Some(uri) = xml_text(segment, "location") {
            if !uri.is_empty() && seen.insert(uri.clone()) {
                entries.push(PlaylistEntry {
                    title: xml_text(segment, "title"),
                    artist: xml_text(segment, "creator"),
                    uri,
                    duration_ms: xml_text(segment, "duration").and_then(|d| d.parse().ok()),
                });
            }
        }
        rest = &rest[end..];
    }
    entries
}

/// The text of the first `<tag>...</tag>` element in `segment` (the open
/// tag may carry attributes; the content is entity-decoded). `None` when
/// the tag is absent or empty.
fn xml_text(segment: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = segment.find(&open)?;
    let open_end = segment[start..].find('>')? + start;
    let after = &segment[open_end + 1..];
    let end = after.find(&close)?;
    let text = after[..end].trim();
    if text.is_empty() { None } else { Some(decode_xml_entities(text)) }
}

/// Decode the standard XML entities (`&amp;` `&lt;` `&gt;` `&quot;`
/// `&apos;` plus numeric `&#NN;` / `&#xHH;`); unknown entities are kept
/// verbatim.
fn decode_xml_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp + 1..];
        let Some(semi_rel) = tail.find(';') else {
            out.push_str(&rest[amp..]);
            return out;
        };
        let entity = &rest[amp..amp + 1 + semi_rel + 1];
        let decoded = match entity {
            "&amp;" => Some("&".to_owned()),
            "&lt;" => Some("<".to_owned()),
            "&gt;" => Some(">".to_owned()),
            "&quot;" => Some("\"".to_owned()),
            "&apos;" => Some("'".to_owned()),
            _ => {
                // "&#65;" -> "65", "&#x42;" -> "x42" (0x prefix kept).
                let digits = &entity[2..entity.len() - 1];
                let (base, num) = if let Some(hex) =
                    digits.strip_prefix('x').or_else(|| digits.strip_prefix('X'))
                {
                    (16, hex)
                } else if digits.chars().all(|c| c.is_ascii_digit()) {
                    (10, digits)
                } else {
                    (0, "")
                };
                if base != 0 && !num.is_empty() && num.chars().all(|c| c.is_ascii_hexdigit())
                {
                    u32::from_str_radix(num, base)
                        .ok()
                        .and_then(char::from_u32)
                        .map(|c| c.to_string())
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(d) => {
                out.push_str(&d);
                rest = &rest[amp + 1 + semi_rel + 1..];
            }
            None => {
                out.push_str(entity);
                rest = &rest[amp + 1 + semi_rel + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Read + parse a library playlist file at the library-relative `path`
/// under `library_root`, with every entry resolved to an MPD URI.
///
/// Relative entries resolve against the directory the file itself lives
/// in (nested album `.m3u` files write file-relative paths); root-level
/// files use the empty directory, i.e. the identity mapping — one rule
/// covers both.
pub fn read_library_playlist(library_root: &str, path: &str) -> Result<Vec<PlaylistEntry>> {
    let Some(ext) = playlist_extension(path) else {
        bail!("not a recognized playlist file: {path}");
    };
    let abs = Path::new(library_root).join(path);
    let content = std::fs::read_to_string(&abs)
        .with_context(|| format!("Cannot read library playlist {path}"))?;
    let file_dir = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or_default();
    Ok(
        parse_playlist_file(&content, ext)
            .into_iter()
            .map(|mut entry| {
                entry.uri = mpd_uri(&entry.uri, file_dir);
                entry
            })
            .collect(),
    )
}

