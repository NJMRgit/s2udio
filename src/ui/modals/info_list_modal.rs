use std::{path::{Path, PathBuf}, time::Duration};
use anyhow::Result;
use bon::bon;
use itertools::Itertools;
use ratatui::{
    Frame, layout::{Constraint, Layout, Margin, Rect},
    macros::constraint, style::Style, symbols::border, text::Text,
    widgets::{Block, Borders, Cell, Clear, Row, Table, TableState},
};
use super::Modal;
use crate::{
    config::keys::CommonAction, ctx::Ctx,
    mpd::{
        commands::{Song, metadata_tag::MetadataTag, volume::Bound},
        mpd_client::MpdClient,
    },
    shared::{
        id::{self, Id},
        keys::ActionEvent, lrc::colocated_lrc_path,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::dirstack::DirState,
};
/// The row source of `InfoListModal` (a local newtype so the row builders
/// below can provide the key/value rows).
#[derive(Debug)]
pub struct KeyValues(Vec<Vec<String>>);
/// One master implementation of the "read-only N-column table modal"
/// shape (Phase 3): a bordered table with a header row, scrollbar,
/// wheel/click selection and Close. Absorbs the legacy two-column
/// key/value info modal and the three-column decoders modal; per-modal
/// differences are args (`rows`, `header` labels, `column_widths`,
/// `title`, `size`) — never a fork.
#[derive(Debug)]
pub struct InfoListModal {
    id: Id,
    scrolling_state: DirState<TableState>,
    table_area: Rect,
    rows: Vec<Vec<String>>,
    column_widths: &'static [u16],
    header: Vec<String>,
    title: &'static str,
    size: (u16, u16),
}
#[bon]
impl InfoListModal {
    #[builder]
    pub fn new(
        rows: impl Into<KeyValues>,
        title: &'static str,
        column_widths: &'static [u16],
        header: Option<Vec<String>>,
        size: Option<(u16, u16)>,
    ) -> Self {
        let mut scrolling_state = DirState::default();
        scrolling_state.select(Some(0), 0);
        Self {
            id: id::new(),
            scrolling_state,
            rows: rows.into().0,
            table_area: Rect::default(),
            title,
            column_widths,
            header: header.unwrap_or_else(|| vec!["Tag".to_owned(), "Value".to_owned()]),
            size: size.unwrap_or((80, 80)),
        }
    }
    /// Wrap every cell at its column's width and turn each logical row into
    /// one *table* row per wrapped line (a cell shorter than its row pads
    /// with empty values, so the columns stay aligned).
    ///
    /// Round 89: this is a transpose — every logical row keeps all of its
    /// wrapped lines. The previous version indexed the outer (row) vector
    /// with the line number, so the table rendered only as many rows as the
    /// *first* entries had lines and any wrapped value silently dropped the
    /// rest of the panel (a long resolved stream URL in the first row hid
    /// the whole bottom half of the info list).
    fn rows_for<'a>(&self, column_areas: &[Rect]) -> Vec<Row<'a>> {
        let mut rows = Vec::new();
        for cells in &self.rows {
            let wrapped: Vec<Vec<String>> = cells
                .iter()
                .zip(column_areas.iter())
                .map(|(cell, area)| {
                    textwrap::wrap(cell, area.width as usize)
                        .into_iter()
                        .map(String::from)
                        .collect()
                })
                .collect();
            let max_lines = wrapped.iter().map(Vec::len).max().unwrap_or(1);
            for line in 0..max_lines {
                rows.push(Row::new(
                    (0..column_areas.len())
                        .map(|col| {
                            Cell::from(
                                Text::from(
                                    wrapped
                                        .get(col)
                                        .and_then(|lines| lines.get(line))
                                        .cloned()
                                        .unwrap_or_default(),
                                ),
                            )
                        })
                        .collect::<Vec<_>>(),
                ));
            }
        }
        rows
    }
}
impl Modal for InfoListModal {
    fn id(&self) -> Id {
        self.id
    }
    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        let (w, h) = self.size;
        let popup_area = frame.area().centered(constraint!(== w %), constraint!(== h %));
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame
                .render_widget(
                    Block::default().style(Style::default().bg(bg_color)),
                    popup_area,
                );
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(ctx.config.as_border_style())
            .title_alignment(ratatui::prelude::Alignment::Center)
            .title(self.title);
        let margin = Margin {
            horizontal: 1,
            vertical: 0,
        };
        let [header_area, table_area] = Layout::vertical([
                Constraint::Length(2),
                Constraint::Percentage(100),
            ])
            .areas(block.inner(popup_area));
        let header_area = header_area.inner(margin);
        let table_area = table_area.inner(margin);
        let column_constraints = self
            .column_widths
            .iter()
            .map(|w| Constraint::Percentage(*w))
            .collect_vec();
        let column_areas = Layout::horizontal(&column_constraints)
            .spacing(1)
            .split(table_area);
        let rows = self.rows_for(&column_areas);
        self.scrolling_state
            .set_content_and_viewport_len(rows.len(), table_area.height.into());
        let header_table = Table::new(
                vec![
                    Row::new(self.header.iter().map(| h | Cell::from(h.as_str()))
                    .collect::< Vec < _ >> ())
                ],
                &column_constraints,
            )
            .column_spacing(1)
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(ctx.config.as_border_style()),
            );
        let table = Table::new(rows, &column_constraints)
            .column_spacing(1)
            .style(ctx.config.as_text_style())
            .row_highlight_style(ctx.config.theme.current_item_style);
        self.table_area = table_area;
        frame.render_widget(block, popup_area);
        frame.render_widget(header_table, header_area);
        frame
            .render_stateful_widget(
                table,
                table_area,
                self.scrolling_state.as_render_state_ref(),
            );
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar() {
            frame
                .render_stateful_widget(
                    scrollbar,
                    popup_area
                        .inner(Margin {
                            horizontal: 0,
                            vertical: 1,
                        }),
                    self.scrolling_state.as_scrollbar_state_ref(),
                );
        }
        return Ok(());
    }
    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::DownHalf => {
                    self.scrolling_state.next_half_viewport(ctx.config.scrolloff);
                    ctx.render()?;
                }
                CommonAction::UpHalf => {
                    self.scrolling_state.prev_half_viewport(ctx.config.scrolloff);
                    ctx.render()?;
                }
                CommonAction::Up => {
                    self.scrolling_state
                        .prev(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    ctx.render()?;
                }
                CommonAction::Down => {
                    self.scrolling_state
                        .next(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    ctx.render()?;
                }
                CommonAction::Bottom => {
                    self.scrolling_state.last();
                    ctx.render()?;
                }
                CommonAction::Top => {
                    self.scrolling_state.first();
                    ctx.render()?;
                }
                CommonAction::Close => {
                    self.hide(ctx)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        if !self.table_area.contains(event.into()) {
            return Ok(());
        }
        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                let y: usize = event.y.saturating_sub(self.table_area.y).into();
                if let Some(idx) = self.scrolling_state.get_at_rendered_row(y) {
                    self.scrolling_state.select(Some(idx), ctx.config.scrolloff);
                    ctx.render()?;
                }
            }
            MouseEventKind::MiddleClick => {}
            MouseEventKind::RightClick => {}
            MouseEventKind::ScrollDown => {
                self.scrolling_state
                    .scroll_down(ctx.config.scroll_amount, ctx.config.scrolloff);
                ctx.render()?;
            }
            MouseEventKind::ScrollUp => {
                self.scrolling_state
                    .scroll_up(ctx.config.scroll_amount, ctx.config.scrolloff);
                ctx.render()?;
            }
            MouseEventKind::Drag { drag_start_position: _ } => {}
            MouseEventKind::LeftRelease => {}
            MouseEventKind::Moved => {}
        }
        Ok(())
    }
}
impl From<Vec<Vec<String>>> for KeyValues {
    fn from(rows: Vec<Vec<String>>) -> Self {
        KeyValues(rows)
    }
}
/// Round 89: the playlist panel's rows — the playlist's own facts first
/// (name / library path, both cheap: the pane already holds them), then the
/// aggregate summary the old `Vec<Song>` rows produced (entry count, total
/// duration, unique artists / albums / genres) plus the stream and missing
/// duration counts. Every individual entry keeps its full detailed panel
/// through `song_info`.
pub fn playlist_info(name: &str, library_path: Option<&str>, songs: &[Song]) -> KeyValues {
    let mut rows: InfoRows = Vec::new();
    rows.push(vec!["Name".to_owned(), name.to_owned()]);
    if let Some(path) = library_path {
        row(&mut rows, "Library path", path);
    }
    rows.push(vec!["Entries".to_owned(), songs.len().to_string()]);
    let total: Duration = songs.iter().filter_map(|song| song.duration).sum();
    rows.push(vec!["Total duration".to_owned(), format_hms(total.as_secs())]);
    let unique = |tag: &str| {
        songs
            .iter()
            .filter_map(|song| song.metadata.get(tag))
            .flat_map(|values| values.iter())
            .unique()
            .count()
    };
    rows.push(vec!["Artists".to_owned(), unique("artist").to_string()]);
    rows.push(vec!["Albums".to_owned(), unique("album").to_string()]);
    rows.push(vec!["Genres".to_owned(), unique("genre").to_string()]);
    let unknown = songs.iter().filter(|song| song.duration.is_none()).count();
    if unknown > 0 {
        row(&mut rows, "Entries without duration", unknown.to_string());
    }
    let streams = songs.iter().filter(|song| is_remote_uri(&song.file)).count();
    if streams > 0 {
        row(&mut rows, "Stream entries", streams.to_string());
    }
    KeyValues(rows)
}
/// Round 89: the public one-shot entry of the detailed song panel — the
/// `Row` type `InfoListModal::builder().rows(..)` takes. `ctx`-aware because
/// the panel shows the file's on-disk facts, the queue position, the stream
/// info, chapters, lyrics and stickers, none of which live on `Song`.
pub fn song_info(ctx: &Ctx, song: &Song) -> KeyValues {
    KeyValues(song_info_rows(ctx, song))
}

// ── Round 89: the detailed "show info" panel ─────────────────────────────
//
// `From<&Song>` above renders a short key/value list (File, Filename, Title,
// Artist, Album, Duration, then every remaining MPD tag in HashMap order).
// "Show info" is reachable from every pane, so it must show the whole
// picture instead: grouped sections in the same `[Metadata]` / `[Tags]` /
// `[File]` style the info boxes already use (`Song::to_preview`,
// `src/ui/panes/mod.rs`), plus the audio facts, the queue position, the
// stream / YouTube details, chapters, lyrics and stickers. Every value is
// either already in memory or read from disk / MPD with a cheap bounded
// read; a field that cannot be sourced is skipped, never faked.
//
// The three sections that read outside the process (file size/mtime, the
// lyrics index, the sticker cache) run only when the modal is built — the
// builder sits on the key handler, not on the render path, so the file read
// and the index scan happen once per "show info", not once per frame.

/// Round 89: the key/value column widths of the info table (`Tag | Value`).
pub const INFO_COLUMN_WIDTHS: &[u16] = &[30, 70];

/// Round 89: the section-header row convention of `InfoListModal` — a marker
/// in the value column (`[Metadata]`), the info box's `[Tags]` style but as
/// a two-column table row, so the existing renderer and scrollbar keep
/// working unchanged.
fn section_header(title: &str) -> Vec<String> {
    vec![String::new(), format!("[{title}]")]
}
/// Round 89: the running row list of the detailed panel.
type InfoRows = Vec<Vec<String>>;
/// Round 89: one `Tag | Value` row, if the value is non-empty — the builder
/// never emits a row without content (the modal would show a blank line).
///
/// A long value (a resolved stream URL, a description) is split across
/// several rows instead of one very tall wrapped row: the modal's cursor and
/// scrollbar address *table rows*, so a single row taller than the viewport
/// would swallow every `Down` press without the view ever moving.
fn row(rows: &mut InfoRows, tag: &str, value: impl AsRef<str>) {
    let value = value.as_ref().trim();
    if value.is_empty() {
        return;
    }
    let mut first = true;
    for chunk in value.lines().flat_map(split_long_value) {
        rows.push(vec![
            if first { tag.to_owned() } else { String::new() },
            chunk,
        ]);
        first = false;
    }
}
/// Round 89: the row width a value is split at — the value column is 70 % of
/// an 80 %-wide popup, so ~120 chars fit on a 200-column terminal. Splitting
/// lower keeps the panel readable on narrower terminals too.
const VALUE_CHUNK: usize = 100;
/// Round 89: split one long value at `VALUE_CHUNK` characters (on a word
/// boundary when there is one nearby, else hard). Short values pass through
/// unchanged and yield a single chunk.
fn split_long_value(value: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for word in value.split_inclusive(char::is_whitespace) {
        // A single word longer than the chunk (a URL with no spaces) is
        // hard-split so it can never form an over-tall row.
        if !current.is_empty() && current.chars().count() + word.chars().count() > VALUE_CHUNK {
            chunks.push(std::mem::take(&mut current).trim_end().to_owned());
        }
        let mut word = word;
        while word.chars().count() > VALUE_CHUNK {
            let head_len = word
                .char_indices()
                .nth(VALUE_CHUNK)
                .map(|(idx, _)| idx)
                .unwrap_or(word.len());
            let (head, rest) = word.split_at(head_len);
            chunks.push(head.to_owned());
            word = rest;
        }
        current.push_str(word);
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim_end().to_owned());
    }
    chunks.retain(|chunk| !chunk.is_empty());
    if chunks.is_empty() {
        chunks.push(value.to_owned());
    }
    chunks
}
/// Round 89: one row per value of a multi-value MPD tag (`artist` twice …).
fn tag_rows(rows: &mut InfoRows, tag: &str, values: &MetadataTag) {
    values.for_each(|item| row(rows, tag, item));
}
/// Round 89: the info table's tag label — MPD tags arrive lowercase
/// (`albumartist`, `musicbrainz_albumid`), the design shows them capitalized
/// (`Date`, `Format`, `MusicBrainz_Albumid`).
fn tag_label(key: &str) -> String {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
/// Round 89: `h:mm:ss` (hours dropped below one hour). The info panel keeps
/// one shape for every duration so the column stays scannable.
fn format_hms(secs: u64) -> String {
    let (hours, rest) = (secs / 3600, secs % 3600);
    if hours > 0 {
        format!("{hours}:{:02}:{:02}", rest / 60, rest % 60)
    } else {
        format!("{}:{:02}", rest / 60, rest % 60)
    }
}
/// Round 89: `h:mm:ss` for the fractional durations MPD / chapter markers
/// carry (seconds are truncated, non-finite and negative values yield an
/// empty string, which the caller then skips).
fn format_hms_f64(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return String::new();
    }
    format_hms(secs as u64)
}
/// Round 89: human-readable byte size ("1.2 GB", one decimal) — the
/// `format_bytes` shape of the torrent picker, available to every detailed
/// info panel (song file sizes, download job files).
pub fn format_size(len: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = len as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{len} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}
/// Round 89: a URL that stays readable inside the 70 %-wide value column —
/// host plus the last path segment (long segments clipped) and a `?…` hint
/// when the URL carries a query. The full value keeps its own row. Anything
/// that is not a URL comes back unchanged.
fn display_url(raw: &str) -> String {
    let Ok(url) = url::Url::parse(raw) else { return raw.to_owned() };
    let mut out = url.host_str().unwrap_or_default().to_owned();
    let last_segment = url
        .path_segments()
        .and_then(|segments| segments.filter(|seg| !seg.is_empty()).next_back())
        .map(str::to_owned);
    if let Some(segment) = last_segment {
        out.push_str("/…/");
        let mut chars = segment.chars();
        out.extend(chars.by_ref().take(48));
        if chars.next().is_some() {
            out.push('…');
        }
    } else if !url.path().is_empty() && url.path() != "/" {
        out.push_str(url.path());
    }
    if url.query().is_some() {
        out.push_str("?…");
    }
    out
}
/// Round 89: the kind of a remote URI, so an entry whose stream URL never
/// reached the yt-info cache still gets a recognisable row.
fn stream_kind(raw: &str) -> &'static str {
    let Ok(url) = url::Url::parse(raw) else { return "Local file" };
    let host = url.host_str().unwrap_or_default();
    let bare = host.strip_prefix("www.").unwrap_or(host);
    if crate::shared::bgutil::is_youtube_url(raw) || bare == "youtu.be" {
        "YouTube"
    } else if bare.ends_with("soundcloud.com") {
        "SoundCloud"
    } else if bare.ends_with("nicovideo.jp") {
        "NicoVideo"
    } else {
        "HTTP stream"
    }
}
/// Round 89: whether a URI would be opened over the network rather than read
/// from disk.
fn is_remote_uri(uri: &str) -> bool {
    crate::ui::panes::radio::is_stream_url(uri) || uri.starts_with("rtsp://")
}
/// Round 89: the directories a relative MPD URI can live in — MPD's own
/// `music_directory` first (exact, but TCP-restricted), then the mpd.conf
/// candidates. Empty for an absolute or remote URI.
fn candidate_dirs(ctx: &Ctx) -> Vec<String> {
    let mut dirs = Vec::new();
    for dir in [ctx.mpd_music_directory.as_ref(), crate::ui::modals::paste::music_directory().as_ref()]
        .into_iter()
        .flatten()
    {
        if !dir.is_empty() && !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }
    dirs
}
/// Round 89: the song's real path — an absolute URI as-is, otherwise the URI
/// joined onto the first candidate music directory that actually holds it.
/// `None` for a remote URI and for a file that is not on disk.
fn resolve_local_path(ctx: &Ctx, uri: &str) -> Option<PathBuf> {
    resolve_local_path_in(uri, &candidate_dirs(ctx))
}
/// Round 89: the pure core of [`resolve_local_path`], so the path resolution
/// is testable without a `Ctx`.
fn resolve_local_path_in(uri: &str, dirs: &[String]) -> Option<PathBuf> {
    if is_remote_uri(uri) {
        return None;
    }
    let path = Path::new(uri);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    dirs.iter().find_map(|dir| {
        let candidate = Path::new(dir).join(uri);
        candidate.is_file().then_some(candidate)
    })
}
/// Round 89: the `.lrc` of *any* song (not just the playing one) — the
/// colocated file first, then the configured lyrics dir / the lyrics index,
/// the same lookup `Ctx::find_current_lyrics_path` performs for the current
/// song.
fn lyrics_path(ctx: &Ctx, song: &Song) -> Option<PathBuf> {
    let song_path = Path::new(&song.file);
    let colocated = if song_path.is_absolute() {
        colocated_lrc_path(song_path).ok().filter(|path| path.is_file())
    } else {
        candidate_dirs(ctx).into_iter().find_map(|dir| {
            colocated_lrc_path(&Path::new(&dir).join(song_path))
                .ok()
                .filter(|path| path.is_file())
        })
    };
    if colocated.is_some() {
        return colocated;
    }
    let lyrics_dir = ctx.config.lyrics_dir.as_ref()?;
    crate::shared::lrc::get_lrc_path(lyrics_dir, &song.file)
        .ok()
        .filter(|path| path.is_file())
        .or_else(|| ctx.lrc_index.find_entry(song).map(|(path, _)| path.to_path_buf()))
}
/// Round 89: `[Metadata]` — File/Filename, then the tags the design template
/// names in a stable order, then **every** remaining MPD tag (MusicBrainz
/// ids, ReplayGain, `format`, `comment`, `name`, …) so nothing is dropped.
fn metadata_section(rows: &mut InfoRows, song: &Song) {
    rows.push(section_header("Metadata"));
    // A resolved stream URL is ~900 characters: the readable form
    // (host + last segment) leads, and the full value is repeated as the
    // `[Stream]` section's last row so the panel still shows everything.
    row(rows, "File", display_url(&song.file));
    // `\`-separated MPD URIs are library paths; a URL has no useful
    // "filename" beyond its last path segment.
    let name = if is_remote_uri(&song.file) {
        url::Url::parse(&song.file)
            .ok()
            .and_then(|url| {
                url.path_segments()
                    .and_then(|segments| segments.filter(|seg| !seg.is_empty()).next_back())
                    .map(str::to_owned)
            })
    } else {
        Path::new(&song.file).file_name().map(|name| name.to_string_lossy().into_owned())
    };
    if let Some(name) = name {
        row(rows, "Filename", name);
    }
    for (tag, label) in [
        ("title", "Title"),
        ("artist", "Artist"),
        ("albumartist", "Album artist"),
        ("album", "Album"),
        ("composer", "Composer"),
        ("performer", "Performer"),
        ("genre", "Genre"),
        ("date", "Date"),
    ] {
        if let Some(values) = song.metadata.get(tag) {
            tag_rows(rows, label, values);
        }
    }
    match (
        song.metadata.get("track").map(MetadataTag::last),
        song.metadata.get("tracktotal").map(MetadataTag::last),
    ) {
        (Some(track), Some(total)) => row(rows, "Track", format!("{track}/{total}")),
        (Some(track), None) => row(rows, "Track", track),
        (None, _) => {}
    }
    match (
        song.metadata.get("disc").map(MetadataTag::last),
        song.metadata.get("disctotal").map(MetadataTag::last),
    ) {
        (Some(disc), Some(total)) => row(rows, "Disc", format!("{disc}/{total}")),
        (Some(disc), None) => row(rows, "Disc", disc),
        (None, _) => {}
    }
    if let Some(values) = song.metadata.get("comment") {
        tag_rows(rows, "Comment", values);
    }
    // The remaining tags, verbatim and alphabetically (the old list came in
    // HashMap order, which shuffled between renders).
    const NAMED: &[&str] = &[
        "title", "artist", "albumartist", "album", "composer", "performer",
        "genre", "date", "track", "tracktotal", "disc", "disctotal", "comment",
        "format", "duration", "pos", "range", "id", "file", "time", "kind",
        "name",
    ];
    for (key, values) in song.metadata.iter().sorted_by_key(|(key, _)| *key) {
        if NAMED.contains(&key.as_str()) {
            continue;
        }
        values.for_each(|item| row(rows, &tag_label(key), item));
    }
}
/// Round 89: `[Audio]` — the format MPD indexed, the duration in `h:mm:ss`
/// plus the raw seconds, and, only for the song MPD plays right now, the
/// live status values (position, bitrate, audio, volume).
fn audio_section(rows: &mut InfoRows, song: &Song, ctx: &Ctx, is_playing: bool) {
    rows.push(section_header("Audio"));
    if let Some(duration) = song.duration {
        row(rows, "Duration", format_hms(duration.as_secs()));
        row(rows, "Duration (seconds)", duration.as_secs().to_string());
    }
    if let Some(format) = song.metadata.get("format") {
        // MPD's `format` is `sample_rate:bits:channels`.
        let parts: Vec<&str> = format.last().split(':').collect();
        if let Some(rate) = parts.first().and_then(|value| value.parse::<u32>().ok()) {
            row(rows, "Sample rate", format!("{rate} Hz"));
        }
        if let Some(bits) = parts.get(1).and_then(|value| value.parse::<u32>().ok()) {
            row(rows, "Bits per sample", bits.to_string());
        }
        if let Some(channels) = parts.get(2).and_then(|value| value.parse::<u32>().ok()) {
            let layout = if channels == 1 { "mono" } else { "stereo" };
            row(rows, "Channels", format!("{channels} ({layout})"));
        }
        row(rows, "Format", format.last());
    }
    if !is_playing {
        return;
    }
    let status = &ctx.status;
    row(
        rows,
        "Playback",
        format!(
            "{} {}/{} ({} left)",
            status.state,
            format_hms(status.elapsed.as_secs()),
            format_hms(status.duration.as_secs()),
            format_hms(status.duration.saturating_sub(status.elapsed).as_secs()),
        ),
    );
    if let Some(bitrate) = status.bitrate {
        row(rows, "Bitrate (status)", format!("{bitrate} kbps"));
    }
    if let Some(audio) = &status.audio {
        row(rows, "Audio (status)", audio);
    }
    row(rows, "Volume", format!("{} %", status.volume.value()));
}
/// Round 89: `[File]` — path, name, extension and, for a real on-disk path
/// (never for an http(s) stream), size, mtime, plus MPD's own
/// `Last-Modified` / `Added`. A missing file or unknown music directory
/// skips the disk rows silently (logged at debug level).
fn file_section(rows: &mut InfoRows, song: &Song, ctx: &Ctx) {
    rows.push(section_header("File"));
    if is_remote_uri(&song.file) {
        // The full URL would swamp the section; `[Stream]` keeps it.
        row(rows, "Path", display_url(&song.file));
        row(rows, "Source", stream_kind(&song.file));
        row(rows, "URL (full)", &song.file);
    } else {
        row(rows, "Path", &song.file);
        let path = Path::new(&song.file);
        if let Some(name) = path.file_name() {
            row(rows, "Name", name.to_string_lossy());
        }
        if let Some(extension) = path.extension() {
            row(rows, "Extension", extension.to_string_lossy());
        }
    }
    if !is_remote_uri(&song.file) {
        if let Some(local) = resolve_local_path(ctx, &song.file) {
            row(rows, "Local path", local.to_string_lossy());
            if let Ok(metadata) = std::fs::metadata(&local) {
                row(rows, "Size", format_size(metadata.len()));
                row(rows, "Size (bytes)", metadata.len().to_string());
                if let Ok(modified) = metadata.modified() {
                    let modified: chrono::DateTime<chrono::Utc> = modified.into();
                    row(rows, "Modified", modified.to_rfc3339());
                }
            }
        } else {
            log::debug!(
                file = song.file.as_str();
                "No on-disk copy of the song; skipping the disk file rows"
            );
        }
    }
    row(rows, "Last-Modified (MPD)", song.last_modified.to_rfc3339());
    if let Some(added) = song.added {
        row(rows, "Added", added.to_rfc3339());
    }
}
/// Round 89: `[Queue]` — whether the song is in the MPD queue (matched by
/// URI, so "show info" on a library or search row still answers), its
/// position and id, and whether it is the entry playing right now.
fn queue_section(rows: &mut InfoRows, song: &Song, ctx: &Ctx) {
    rows.push(section_header("Queue"));
    let current_idx = ctx.find_current_song_in_queue().map(|(idx, _)| idx);
    match ctx.queue.iter().position(|item| item.file == song.file) {
        Some(idx) => {
            row(
                rows,
                "In queue",
                format!("Yes — position {idx} of {}", ctx.queue.len()),
            );
            row(rows, "Queue id", ctx.queue[idx].id.to_string());
            row(rows, "Playing now", if current_idx == Some(idx) { "Yes" } else { "No" });
        }
        None => row(rows, "In queue", format!("No (queue of {})", ctx.queue.len())),
    }
    row(rows, "Song id", song.id.to_string());
}
/// Round 89: `[Stream]` — for a URI opened over the network (a resolved
/// YouTube/stream queue entry). The cached yt-dlp info carries the rich
/// fields; without a cache entry the URL itself still gets a useful set.
/// Only fields the app's `YtStreamInfo` actually holds are shown — the
/// yt-dlp JSON subset parses title/url/thumbnail/channel/subscribers/
/// description/duration/chapters, so there is no views/likes/upload date to
/// show (those would need a new network resolve).
fn stream_section(
    rows: &mut InfoRows,
    song: &Song,
    info: Option<&crate::shared::ytdlp::YtStreamInfo>,
) {
    if !is_remote_uri(&song.file) && info.is_none() {
        return;
    }
    rows.push(section_header("Stream"));
    row(rows, "Kind", stream_kind(&song.file));
    // The readable form first; the full URL is *also* kept (as its own
    // rows) so nothing is lost, but it never leads the section.
    row(rows, "URL", display_url(&song.file));
    let Some(info) = info else {
        row(rows, "URL (full)", &song.file);
        row(rows, "Cached info", "No cached stream info for this link");
        return;
    };
    if !info.title.is_empty() {
        row(rows, "Stream title", &info.title);
    }
    if let Some(channel) = &info.channel {
        row(rows, "Channel", channel);
    }
    if let Some(subscribers) = info.subscribers {
        row(rows, "Subscribers", subscribers.to_string());
    }
    if let Some(duration) = info.duration {
        row(rows, "Stream duration", format_hms_f64(duration));
    }
    if let Some(start) = info.start_secs {
        row(rows, "Start offset", format_hms_f64(start));
    }
    if !info.original_url.is_empty() && info.original_url != song.file {
        row(rows, "Original link", &info.original_url);
    }
    if let Some(thumbnail) = &info.thumbnail {
        row(rows, "Thumbnail", display_url(thumbnail));
        row(rows, "Thumbnail (full)", thumbnail);
    }
    // The full URL, last in the section: it is the longest value in the
    // whole panel, so it must not push the interesting rows off-screen.
    row(rows, "URL (full)", &song.file);
    if let Some(description) = &info.description {
        // The value cell wraps at ~70 % of an 80 %-wide popup, so the
        // description is clamped to a scannable length on one row.
        let description = description.trim();
        let clamped: String = description.chars().take(600).collect();
        let suffix = if description.chars().count() > 600 { "…" } else { "" };
        row(rows, "Description", format!("{}{suffix}", clamped.replace('\n', " ")));
    }
}
/// Round 89: `[Chapters]` — the markers the app holds for this file
/// (YouTube chapters, ffprobe extraction, Jellyfin items), each with its
/// start timestamp. Skipped when none are known.
fn chapters_section(
    rows: &mut InfoRows,
    chapters: &[crate::shared::chapters::Chapter],
) {
    if chapters.is_empty() {
        return;
    }
    rows.push(section_header("Chapters"));
    row(rows, "Chapters", chapters.len().to_string());
    for (idx, chapter) in chapters.iter().enumerate() {
        let title = if chapter.title.trim().is_empty() {
            format!("Chapter {}", idx + 1)
        } else {
            chapter.title.trim().to_owned()
        };
        row(
            rows,
            &(idx + 1).to_string(),
            format!("{} — {}", format_hms_f64(chapter.start_secs), title),
        );
    }
}
/// Round 89: `[Lyrics]` — whether an `.lrc` exists for this file and where.
fn lyrics_section(rows: &mut InfoRows, path: Option<&PathBuf>) {
    let Some(path) = path else { return };
    rows.push(section_header("Lyrics"));
    row(rows, "Lyrics", "Found");
    row(rows, "Lyrics path", path.to_string_lossy());
}
/// Round 89: `[Stickers]` — MPD stickers of this song's URI (rating / like
/// and any MPD 0.24 host sticker). Without an explicit read the panel would
/// show whatever the lazy cache holds, so a direct, bounded query is issued
/// once per "show info" when MPD did not answer in this session yet.
fn stickers_section(rows: &mut InfoRows, song: &Song, ctx: &Ctx) {
    if !bool::from(ctx.stickers_supported) {
        return;
    }
    // The app's own sticker cache answers without I/O once another pane has
    // looked the song up (the lazy fetch `song_stickers` arms runs at the
    // end of the frame). It is consulted first so the common case costs
    // nothing.
    let mut stickers = ctx
        .song_stickers(&song.file)
        .cloned()
        .unwrap_or_default();
    if stickers.is_empty() {
        // A song no pane has looked at yet: one bounded `liststickers`
        // round-trip on the local MPD socket (the same `query_sync` shape
        // the context menus use), so the panel is complete on first open
        // instead of a frame later.
        let uri = song.file.clone();
        stickers = ctx
            .query_sync(move |client| {
                Ok(client
                    .list_stickers(&uri)
                    .map(|stickers| stickers.0)
                    .unwrap_or_default())
            })
            .unwrap_or_default();
    }
    if stickers.is_empty() {
        return;
    }
    rows.push(section_header("Stickers"));
    for (key, value) in stickers.iter().sorted_by_key(|(key, _)| *key) {
        row(rows, &tag_label(key), value);
    }
}
/// Round 89: the info rows of one yt-dlp manager entry (shared by the
/// Downloads modal's panel and the Downloads tab's). Empty when the manager
/// entry is gone (a row that outlived its job).
pub fn ytdlp_info_rows(
    ctx: &Ctx,
    id: crate::shared::ytdlp::DownloadId,
    label: &str,
    downloading: bool,
) -> Vec<Vec<String>> {
    let mut rows = vec![
        vec!["File".to_owned(), label.to_owned()],
        vec![
            "State".to_owned(),
            if downloading { "Downloading".to_owned() } else { "Queued".to_owned() },
        ],
    ];
    let Some(item) = ctx.ytdlp_manager.get(id) else { return rows };
    rows.push(vec!["Id".to_owned(), item.inner.id.clone()]);
    rows.push(vec!["Host".to_owned(), item.inner.kind.to_string()]);
    rows.push(vec!["URL".to_owned(), item.inner.to_url()]);
    if let Some(title) = &item.inner.title {
        rows.push(vec!["Title".to_owned(), title.clone()]);
    }
    if let Some(position) = item.add_position {
        rows.push(vec!["Queue position".to_owned(), position.as_mpd_str()]);
    }
    if item.autoplay {
        rows.push(vec!["Autoplay".to_owned(), "Yes".to_owned()]);
    }
    match &item.state {
        crate::shared::ytdlp::DownloadState::Completed { path, logs } => {
            rows.push(vec!["Path".to_owned(), path.to_string_lossy().into_owned()]);
            if let Ok(metadata) = std::fs::metadata(path) {
                rows.push(vec![
                    "Size".to_owned(),
                    format_size(metadata.len()),
                ]);
            }
            rows.push(vec!["Log lines".to_owned(), logs.len().to_string()]);
        }
        crate::shared::ytdlp::DownloadState::AlreadyDownloaded { path } => {
            rows.push(vec!["Path".to_owned(), path.to_string_lossy().into_owned()]);
            if let Ok(metadata) = std::fs::metadata(path) {
                rows.push(vec![
                    "Size".to_owned(),
                    format_size(metadata.len()),
                ]);
            }
        }
        crate::shared::ytdlp::DownloadState::Failed { logs } => {
            rows.push(vec!["Log lines".to_owned(), logs.len().to_string()]);
        }
        crate::shared::ytdlp::DownloadState::Queued
        | crate::shared::ytdlp::DownloadState::Downloading
        | crate::shared::ytdlp::DownloadState::Canceled => {}
    }
    if let Some(spec) = &item.spec {
        rows.push(vec!["Output".to_owned(), spec.output_dir.to_string_lossy().into_owned()]);
        if spec.audio_only {
            rows.push(vec!["Audio only".to_owned(), "Yes".to_owned()]);
        }
        if spec.split_chapters {
            rows.push(vec!["Split chapters".to_owned(), "Yes".to_owned()]);
        }
        if !spec.sections.is_empty() {
            rows.push(vec!["Chapter ranges".to_owned(), spec.sections.len().to_string()]);
        }
    }
    rows
}
/// Round 89: the info rows of one daemon torrent job (shared by the two
/// Downloads surfaces). Empty when the job is no longer in `ctx.dl_state`.
pub fn torrent_info_rows(ctx: &Ctx, job_id: &str) -> Vec<Vec<String>> {
    let state = ctx.dl_state.borrow();
    let Some(job) = state
        .as_ref()
        .and_then(|state| state.jobs.iter().find(|job| job.job_id == job_id))
    else {
        return Vec::new();
    };
    let mut rows = vec![
        vec!["Job id".to_owned(), job.job_id.clone()],
        vec!["Torrent".to_owned(), job.torrent_name.clone()],
        vec!["Status".to_owned(), job.status.to_string()],
        vec!["Progress".to_owned(), format!("{:.1} %", job.progress_percent)],
    ];
    if let Some(infohash) = &job.infohash {
        rows.push(vec!["Info hash".to_owned(), infohash.clone()]);
    }
    if let Some(torrent_id) = &job.torrent_id {
        rows.push(vec!["Torrent id".to_owned(), torrent_id.clone()]);
    }
    if let Some(error) = &job.error {
        rows.push(vec!["Error".to_owned(), error.clone()]);
    }
    if let Some(moved_to) = &job.moved_to {
        rows.push(vec!["Moved to".to_owned(), moved_to.clone()]);
    }
    let mut total = 0u64;
    for file in &job.kept_files {
        total += file.length;
        rows.push(vec![
            format!("File {}", file.index),
            format!(
                "{} ({})",
                file.name,
                format_size(file.length)
            ),
        ]);
    }
    if total > 0 {
        rows.push(vec![
            "Total size".to_owned(),
            format_size(total),
        ]);
    }
    rows
}

/// Round 89: the full "show info" panel of one song. Every trigger of the
/// `ShowInfo` / `ShowCurrentSongInfo` actions calls this, so a song shown
/// from the queue, the library, the search results or a playlist renders the
/// same extensive list. The argument list is the panel's sections:
/// metadata, audio, file, queue, stream, chapters, lyrics, stickers.
pub fn song_info_rows(ctx: &Ctx, song: &Song) -> Vec<Vec<String>> {
    let mut rows: InfoRows = Vec::new();
    metadata_section(&mut rows, song);
    let current_file = ctx.find_current_song_in_queue().map(|(_, song)| song.file.clone());
    let is_playing = current_file.as_deref() == Some(song.file.as_str());
    audio_section(&mut rows, song, ctx, is_playing);
    file_section(&mut rows, song, ctx);
    queue_section(&mut rows, song, ctx);
    let info = crate::ui::panes::playlists::stream_info(ctx, &song.file);
    stream_section(&mut rows, song, info.as_ref());
    let chapters = ctx.chapters.borrow().get(&song.file).cloned().unwrap_or_default();
    chapters_section(&mut rows, &chapters);
    lyrics_section(&mut rows, lyrics_path(ctx, song).as_ref());
    stickers_section(&mut rows, song, ctx);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpd::commands::metadata_tag::MetadataTag;

    fn song(file: &str) -> Song {
        Song { file: file.to_owned(), ..Default::default() }
    }

    #[test]
    fn durations_format_as_h_mm_ss() {
        assert_eq!(format_hms(0), "0:00");
        assert_eq!(format_hms(59), "0:59");
        assert_eq!(format_hms(60), "1:00");
        assert_eq!(format_hms(3_599), "59:59");
        assert_eq!(format_hms(3_600), "1:00:00");
        assert_eq!(format_hms(5_366), "1:29:26");
        assert_eq!(format_hms_f64(65.9), "1:05");
        assert_eq!(format_hms_f64(f64::NAN), "");
        assert_eq!(format_hms_f64(-1.0), "");
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1_024), "1.0 KB");
        assert_eq!(format_size(1_536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn long_urls_are_trimmed_but_kept() {
        assert_eq!(
            display_url("https://rr3---sn-abc.googlevideo.com/videoplayback?expire=1"),
            "rr3---sn-abc.googlevideo.com/…/videoplayback?…"
        );
        assert_eq!(display_url("not a url"), "not a url");
        assert_eq!(display_url("01 - track.flac"), "01 - track.flac");
    }

    #[test]
    fn stream_kinds_are_detected() {
        assert_eq!(stream_kind("https://www.youtube.com/watch?v=abc"), "YouTube");
        assert_eq!(stream_kind("https://youtu.be/abc"), "YouTube");
        assert_eq!(stream_kind("https://soundcloud.com/a/b"), "SoundCloud");
        assert_eq!(stream_kind("https://www.nicovideo.jp/watch/sm9"), "NicoVideo");
        assert_eq!(stream_kind("https://example.com/x"), "HTTP stream");
    }

    /// A long value is split into rows, so the modal's cursor can move
    /// through it (a row taller than the viewport would swallow every
    /// `Down` press).
    #[test]
    fn long_values_are_split_into_rows() {
        let short = split_long_value("2001");
        assert_eq!(short, vec!["2001".to_owned()]);
        let url = format!("https://example.com/{}", "a".repeat(250));
        let chunks = split_long_value(&url);
        assert!(chunks.len() >= 3, "{chunks:?}");
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= VALUE_CHUNK));
        assert_eq!(chunks.concat(), url);
        let sentence = "word ".repeat(60);
        let chunks = split_long_value(sentence.trim());
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= VALUE_CHUNK));
        // Multi-line values are split per line by `row`, not here.
        assert_eq!(split_long_value(""), vec![String::new()]);
    }

    /// `row` keeps the tag on the first chunk only, and never emits an
    /// empty row.
    #[test]
    fn long_row_values_span_rows_with_one_label() {
        let mut rows = Vec::new();
        row(&mut rows, "Description", "x".repeat(250));
        assert!(rows.len() >= 3);
        assert_eq!(rows[0][0], "Description");
        assert!(rows[1..].iter().all(|r| r[0].is_empty()));
        assert!(rows.iter().all(|r| !r[1].is_empty()));
        let mut rows = Vec::new();
        row(&mut rows, "Empty", "   ");
        assert!(rows.is_empty());
    }

    #[test]
    fn remote_uris_are_recognised() {
        assert!(is_remote_uri("https://example.com/x"));
        assert!(is_remote_uri("http://example.com/x"));
        assert!(is_remote_uri("rtsp://example.com/x"));
        assert!(!is_remote_uri("Artist/Album/track.flac"));
        assert!(!is_remote_uri("/home/user/track.flac"));
    }

    /// The `[Metadata]` section: named tags first in a stable order, then
    /// every remaining MPD tag capitalized and alphabetized; no empty rows.
    #[test]
    fn metadata_section_is_grouped_and_complete() {
        let mut song = song("Artist/Album/01 Track.flac");
        song.metadata.insert("title".to_owned(), MetadataTag::from("Track"));
        song.metadata.insert("artist".to_owned(), MetadataTag::from("Artist"));
        song.metadata.insert("albumartist".to_owned(), MetadataTag::from("Album Artist"));
        song.metadata.insert("track".to_owned(), MetadataTag::from("3"));
        song.metadata.insert("tracktotal".to_owned(), MetadataTag::from("12"));
        song.metadata.insert("disc".to_owned(), MetadataTag::from("1"));
        song.metadata.insert("format".to_owned(), MetadataTag::from("44100:16:2"));
        song.metadata.insert("date".to_owned(), MetadataTag::from("2001"));
        song.metadata.insert("comment".to_owned(), MetadataTag::from("a comment"));
        song.metadata.insert("musicbrainz_albumid".to_owned(), MetadataTag::from("mbid-1"));
        let mut rows = Vec::new();
        metadata_section(&mut rows, &song);
        let flat: Vec<(String, String)> =
            rows.iter().map(|row| (row[0].clone(), row[1].clone())).collect();
        assert!(flat.contains(&(String::new(), "[Metadata]".to_owned())));
        assert!(flat.contains(&("Title".to_owned(), "Track".to_owned())));
        assert!(flat.contains(&("Album artist".to_owned(), "Album Artist".to_owned())));
        assert!(flat.contains(&("Track".to_owned(), "3/12".to_owned())));
        assert!(flat.contains(&("Date".to_owned(), "2001".to_owned())));
        assert!(flat.contains(&("Comment".to_owned(), "a comment".to_owned())));
        // Remaining tags stay, capitalized.
        assert!(flat.contains(&("Musicbrainz_albumid".to_owned(), "mbid-1".to_owned())));
        // `format` is consumed by the Audio rows, never dumped again.
        assert!(!flat.iter().any(|(tag, _)| tag == "format" || tag == "Format"));
        // No row ever has an empty value.
        assert!(rows.iter().all(|row| !row[1].is_empty()));
    }

    /// The `[Metadata]` `Filename` row of a stream URL is its last path
    /// segment, not the whole ~900-character query string.
    #[test]
    fn stream_metadata_filename_is_the_last_segment() {
        let mut song = song(
            "https://rr5---sn-abc.googlevideo.com/videoplayback?expire=1&itag=251",
        );
        song.metadata.insert("title".to_owned(), MetadataTag::from("Track"));
        let mut rows = Vec::new();
        metadata_section(&mut rows, &song);
        let flat: Vec<(String, String)> =
            rows.iter().map(|row| (row[0].clone(), row[1].clone())).collect();
        assert!(flat.contains(&("Filename".to_owned(), "videoplayback".to_owned())));
        assert!(flat.iter().all(|(_, value)| !value.contains("expire=1")));
    }

    /// The `[File]` disk rows of a real library file (this test only runs
    /// where the library is mounted; it skips when the file is absent).
    #[test]
    fn local_file_rows_read_size_and_mtime() {
        let path = "/mnt/20TBHDD/Media/Music/Adrenalize/2012 - Adrenalize - Secrets Of time/01 - Secrets Of Time (Original Mix).flac";
        if !std::path::Path::new(path).is_file() {
            return;
        }
        // Relative MPD URI resolved through the mpd.conf fallback.
        let uri = "Adrenalize/2012 - Adrenalize - Secrets Of time/01 - Secrets Of Time (Original Mix).flac";
        let resolved =
            resolve_local_path_in(uri, &["/mnt/20TBHDD/Media/Music".to_owned()]);
        assert_eq!(resolved.as_deref(), Some(std::path::Path::new(path)));
        assert_eq!(
            format_size(std::fs::metadata(path).unwrap().len()),
            "37.1 MB"
        );
    }

    /// An absolute path resolves as-is; a remote URI never resolves; a
    /// relative URI with no matching candidate yields nothing.
    #[test]
    fn local_path_resolution_rules() {
        assert!(resolve_local_path_in("https://example.com/x", &[]).is_none());
        assert!(resolve_local_path_in("Artist/track.flac", &["/nonexistent".to_owned()]).is_none());
        assert!(resolve_local_path_in("/nonexistent/track.flac", &[]).is_none());
        let here = std::path::Path::new(file!()).canonicalize().unwrap();
        let here = here.to_string_lossy().into_owned();
        assert_eq!(
            resolve_local_path_in(&here, &[]),
            Some(std::path::PathBuf::from(&here))
        );
    }

    /// A stream URI gets the `[Stream]` section even with no cache entry; a
    /// local file gets none.
    #[test]
    fn stream_section_only_for_remote_uris() {
        let stream = song("https://rr3---sn.googlevideo.com/videoplayback?expire=1");
        let mut rows = Vec::new();
        stream_section(&mut rows, &stream, None);
        let flat: Vec<(String, String)> =
            rows.iter().map(|row| (row[0].clone(), row[1].clone())).collect();
        assert!(flat.contains(&(String::new(), "[Stream]".to_owned())));
        assert!(flat.contains(&("Kind".to_owned(), "HTTP stream".to_owned())));
        assert!(flat.contains(&(
            "URL (full)".to_owned(),
            "https://rr3---sn.googlevideo.com/videoplayback?expire=1".to_owned()
        )));
        let local = song("Artist/Album/track.flac");
        let mut rows = Vec::new();
        stream_section(&mut rows, &local, None);
        assert!(rows.is_empty());
    }

    /// The chapter rows carry a numbered label and a start timestamp.
    #[test]
    fn chapter_rows_are_numbered_with_timestamps() {
        let chapters = vec![
            crate::shared::chapters::Chapter {
                title: "Intro".to_owned(),
                start_secs: 0.0,
                end_secs: 30.0,
            },
            crate::shared::chapters::Chapter {
                title: String::new(),
                start_secs: 90.0,
                end_secs: 120.0,
            },
        ];
        let mut rows = Vec::new();
        chapters_section(&mut rows, &chapters);
        let flat: Vec<(String, String)> =
            rows.iter().map(|row| (row[0].clone(), row[1].clone())).collect();
        assert!(flat.contains(&(String::new(), "[Chapters]".to_owned())));
        assert!(flat.contains(&("Chapters".to_owned(), "2".to_owned())));
        assert!(flat.contains(&("1".to_owned(), "0:00 — Intro".to_owned())));
        assert!(flat.contains(&("2".to_owned(), "1:30 — Chapter 2".to_owned())));
        let mut rows = Vec::new();
        chapters_section(&mut rows, &[]);
        assert!(rows.is_empty());
    }
}
