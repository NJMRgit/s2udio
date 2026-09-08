use anyhow::Result;
use ratatui::{
    Frame, prelude::{Buffer, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};
use unicode_width::UnicodeWidthStr;
use super::Pane;
use crate::{
    ctx::Ctx,
    mpd::{
        commands::{State, status::OnOffOneshot},
        mpd_client::{MpdClient, ValueChange},
    },
    shared::{
        ext::duration::DurationExt, keys::ActionEvent, macros::modal,
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::MpdClientExt,
    },
};
/// The controls pane's palette, derived from the theme's text color so the
/// transport buttons, mode toggles, volume bars and the horizontal separator
/// all follow blur mode changes. Falls back to the grey palette when the
/// theme has no text color.
#[derive(Clone, Copy)]
struct ControlsTheme {
    active: Style,
    inactive: Style,
    /// The Single/Consume toggles render oneshot with yellow text.
    oneshot: Style,
    transport: Style,
    artist: Style,
    title: Style,
    separator: Style,
    time: Style,
    volume_filled: Style,
    volume_track: Style,
}
impl ControlsTheme {
    fn from_ctx(ctx: &Ctx) -> Self {
        let base = ctx.config.theme.text_color.unwrap_or(Color::Rgb(0x8f, 0x8f, 0x8f));
        let dim = crate::config::scale_color(base, 0.6);
        let track = crate::config::scale_color(base, 0.4);
        Self {
            active: Style::new().fg(base).add_modifier(Modifier::BOLD),
            inactive: Style::new().fg(dim).add_modifier(Modifier::DIM),
            oneshot: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            transport: Style::new().fg(base),
            artist: Style::new().fg(base).add_modifier(Modifier::BOLD),
            title: Style::new().add_modifier(Modifier::BOLD),
            separator: Style::new().fg(track),
            time: Style::new().fg(base),
            volume_filled: Style::new().fg(base),
            volume_track: Style::new().fg(track),
        }
    }
}
/// Round 60c (S4): the transport cluster (row 2, centered) — the buttons
/// `◀◀  ▶  ▶▶  ■` between the row's `│` column separators. Content width
/// (with the 1-2 cell play/pause label padded to 2): `   ◀◀   ` + label+
/// `   ▶▶  ■   ` = 21 columns.
const TRANSPORT_CLUSTER_W: u16 = 21;
/// Mode toggle buttons, right-aligned on the first line.
const MODE_SLOT: u16 = 9;
/// Width of the " 100%" text right of the volume slider.
const VOLUME_PCT_W: u16 = 5;
/// The row-3 badge cell `  NORMAL  ` (10 columns) between the box's left
/// `│` and the badge separator `│` (round 61 P3).
const BADGE_CELL_W: u16 = 10;
/// Mouse scroll step for the volume slider (finer than the keybind step).
const VOLUME_SCROLL_STEP: u32 = 2;
/// The slider cap: 21 cols -> 20 positions -> exactly 5% per click step.
const VOLUME_SLIDER_MAX_W: u16 = 21;
/// Where a RIGHT-FITTED volume block would start — the boundary the
/// transport cluster is centered against (round 60 A5 / 60c S4). Round 61
/// (P4) moved the volume block INSIDE its right column, so this fit only
/// keeps the transport's centering exactly where it was; the drawn and
/// clickable block comes from [`Self::volume_geometry`].
fn volume_fit_start(area: Rect, min_left: u16) -> u16 {
    let right_edge = area.right().saturating_sub(1);
    let available = right_edge.saturating_sub(min_left + 3 + VOLUME_PCT_W);
    let slider_w = available.clamp(6, VOLUME_SLIDER_MAX_W);
    right_edge.saturating_sub(VOLUME_PCT_W + slider_w)
}
/// Row-2 volume block geometry (round 61 P4): the slider + percent block
/// is CENTERED inside the right column between the `sep2` separator and
/// the right border, with equal margins both sides. Returns (start_x,
/// slider_width); `slider_w == 0` when the column is too narrow.
fn volume_geometry(area: Rect, sep2: u16) -> (u16, u16) {
    let right = area.right().saturating_sub(1);
    let col_w = right.saturating_sub(sep2 + 1);
    // The centered block needs the column to hold the smallest slider (6)
    // + the percent text (5) + at least one margin each side; below that
    // the volume is hidden rather than drawn over the border.
    const MIN_COL_W: u16 = 6 + VOLUME_PCT_W + 2;
    if col_w < MIN_COL_W {
        return (sep2 + 1, 0);
    }
    let slider_w = (col_w - VOLUME_PCT_W - 2).clamp(6, VOLUME_SLIDER_MAX_W);
    let block_w = slider_w + VOLUME_PCT_W;
    let start = sep2 + 1 + (col_w - block_w) / 2;
    (start, slider_w)
}
#[derive(Debug, Clone, Copy)]
enum Transport {
    Prev,
    PlayPause,
    Next,
    Stop,
}
#[derive(Debug, Clone, Copy)]
enum Mode {
    Repeat,
    Random,
    Single,
    Consume,
}
/// The mpv-mode buttons on row 0 (shown while an mpv session is the UI
/// source): Download (only while a ytdlp stream plays), Audio language and
/// Subtitle language. They replace the MPD mode toggles (Repeat/Random/
/// Single/Consume), which only apply to MPD playback.
#[derive(Debug, Clone, Copy)]
enum MpvButton {
    Download,
    Audio,
    Subs,
}
#[derive(Debug)]
pub struct ControlsPane {
    area: Rect,
    /// Elapsed second (status clock) the displayed bitrate was sampled at,
    /// so the bitrate refreshes once per second like the elapsed text.
    bitrate_sec: u64,
    display_bitrate: Option<u32>,
    /// The wall-clock anchor the title/left marquees were last restarted
    /// at (a new song/video, or the tab being switched): the phase is
    /// `anchor.elapsed()`, so the texts always start from their beginning
    /// with the 2s hold when (re)shown — and keep advancing on a wall
    /// clock even while playback is paused/stopped (round 63 3b; the old
    /// playback-elapsed clock stalled whenever elapsed did).
    carousel_anchor: Option<std::time::Instant>,
    /// The tab last rendered (the carousel restarts when it changes).
    last_tab: Option<String>,
    /// The track the marquee is showing — `(song id, file)` for MPD
    /// playback, `(0, mpv title)` while an mpv session is the UI source;
    /// `None` when nothing plays. The carousel restarts when it changes.
    last_song_key: Option<(u32, String)>,
}
impl ControlsPane {
    pub fn new() -> Self {
        Self {
            area: Rect::default(),
            bitrate_sec: u64::MAX,
            display_bitrate: None,
            carousel_anchor: None,
            last_tab: None,
            last_song_key: None,
        }
    }
    fn mode_info(mode: Mode) -> &'static str {
        match mode {
            Mode::Repeat => "Repeat",
            Mode::Random => "Random",
            Mode::Single => "Single",
            Mode::Consume => "Consume",
        }
    }
    fn mode_state(mode: Mode, ctx: &Ctx) -> OnOffOneshot {
        match mode {
            Mode::Repeat => {
                if ctx.status.repeat { OnOffOneshot::On } else { OnOffOneshot::Off }
            }
            Mode::Random => {
                if ctx.status.random { OnOffOneshot::On } else { OnOffOneshot::Off }
            }
            Mode::Single => ctx.status.single,
            Mode::Consume => ctx.status.consume,
        }
    }
    fn mode_start_x(area: Rect) -> u16 {
        area.right().saturating_sub(1 + MODE_SLOT * 4)
    }
    /// The transport cluster's left content column (row 2, centered): the
    /// cluster is centered between the time text and the volume block's
    /// right-fitted start (round 60 A5 / 60c S4). Round 61 (P4) keeps
    /// this centering exactly where it was and recenters the volume block
    /// inside its column instead.
    fn transport_cluster_start(area: Rect, time_w: u16) -> u16 {
        let volume_start = volume_fit_start(area, area.x + 1 + time_w);
        let free = volume_start.saturating_sub(area.x + 1 + time_w);
        area.x + 1 + time_w + free.saturating_sub(TRANSPORT_CLUSTER_W) / 2
    }
    /// The row-2 column separator columns: `│` at `sep1` (after the time
    /// text), `│` at `sep2` (after the transport cluster). The dividers
    /// above/below row 2 place their `┬`/`┴` junctions at the same columns.
    fn transport_columns(area: Rect, time_w: u16) -> (u16, u16) {
        let start = Self::transport_cluster_start(area, time_w);
        (start.saturating_sub(1), start.saturating_add(TRANSPORT_CLUSTER_W))
    }
    /// Click zones of the row-2 transport cluster, in render order. Each
    /// zone covers the button's slot including its padding.
    fn transport_zones(area: Rect, time_w: u16) -> (u16, [(Transport, u16, u16); 4]) {
        let start = Self::transport_cluster_start(area, time_w);
        let zones = [
            (Transport::Prev, start + 3, start + 8),
            (Transport::PlayPause, start + 8, start + 12),
            (Transport::Next, start + 12, start + 16),
            (Transport::Stop, start + 16, start + 21),
        ];
        (start, zones)
    }
    /// The playback-center top row (round 60 A5) splits into **artist–album
    /// on the left, title centered** and the queue modifiers (Repeat/Random/
    /// Single/Consume — or the mpv-session buttons) right-aligned.
    fn artist_album_line(ctx: &Ctx) -> Line<'static> {
        let theme = ControlsTheme::from_ctx(ctx);
        let artist_style = theme.artist;
        let separator_style = theme.separator;
        let title_style = theme.title;
        let separator = &ctx.config.theme.format_tag_separator;
        let strategy = ctx.config.theme.multiple_tag_resolution_strategy;
        let song = ctx.find_current_song_in_queue().map(|(_, song)| song);
        let Some(song) = song else {
            return Line::from(Span::styled("No Playback", title_style));
        };
        let tag = |key: &str| {
            song.metadata
                .get(key)
                .map(|t| strategy.resolve(t, separator).into_owned())
                .filter(|s| !s.trim().is_empty())
        };
        match (tag("artist"), tag("album")) {
            (Some(artist), Some(album)) => Line::from(vec![
                Span::styled(artist, artist_style),
                Span::styled(" - ", separator_style),
                Span::styled(album, title_style),
            ]),
            (Some(artist), None) => Line::from(Span::styled(artist, artist_style)),
            (None, Some(album)) => Line::from(Span::styled(album, title_style)),
            (None, None) => {
                // Radio / stream fallback: the station/stream name.
                song.metadata
                    .get("name")
                    .map(|t| strategy.resolve(t, separator).into_owned())
                    .filter(|s| !s.trim().is_empty())
                    .map_or_else(
                        Line::default,
                        |name| Line::from(Span::styled(name, title_style)),
                    )
            }
        }
    }
    /// The now-playing title line (row 0, centered). While an mpv video
    /// plays it shows the video's title; a resolved YouTube audio stream
    /// shows the video title; MPD songs show the Title tag alone (the
    /// artist lives in the left half).
    fn title_line(ctx: &Ctx) -> Line<'static> {
        let theme = ControlsTheme::from_ctx(ctx);
        let title_style = theme.title;
        if crate::core::mpv::mpv_is_ui_source(ctx) {
            let title = ctx.mpv.title.clone();
            if title.is_empty() {
                return Line::from(Span::styled("Playing on mpv", title_style));
            }
            return Line::from(Span::styled(title, title_style));
        }
        let song = ctx.find_current_song_in_queue().map(|(_, song)| song);
        let Some(song) = song else {
            return Line::from(Span::styled("No Playback", title_style));
        };
        if let Some(yt) = ctx.yt_info.borrow().get(&song.file) && !yt.title.is_empty() {
            return Line::from(Span::styled(yt.title.clone(), title_style));
        }
        let separator = &ctx.config.theme.format_tag_separator;
        let strategy = ctx.config.theme.multiple_tag_resolution_strategy;
        song.metadata
            .get("title")
            .map(|tag| strategy.resolve(tag, separator).into_owned())
            .filter(|s| !s.trim().is_empty())
            .map_or_else(
                || Line::from(Span::styled("No Playback", title_style)),
                |title| Line::from(Span::styled(title, title_style)),
            )
    }
    /// The channel/show/album line (row 0): the album for a music track,
    /// the channel for a YouTube-style stream, the show/series for a
    /// Jellyfin episode. Left aligned and truncated (never scrolls).
    fn channel_line(ctx: &Ctx) -> Line<'static> {
        let theme = ControlsTheme::from_ctx(ctx);
        let style = theme.artist;
        let strategy = ctx.config.theme.multiple_tag_resolution_strategy;
        let separator = &ctx.config.theme.format_tag_separator;
        if crate::core::mpv::mpv_is_ui_source(ctx) {
            if ctx.mpv.artist.is_empty() {
                return Line::default();
            }
            return Line::from(Span::styled(ctx.mpv.artist.clone(), style));
        }
        let song = ctx.find_current_song_in_queue().map(|(_, song)| song);
        let Some(song) = song else { return Line::default() };
        if let Some(yt) = crate::ui::modals::paste::current_yt_info(ctx)
            && let Some(channel) = yt.channel.as_deref().filter(|c| !c.trim().is_empty())
        {
            return Line::from(Span::styled(channel.to_owned(), style));
        }
        let album = song
            .metadata
            .get("album")
            .map(|tag| strategy.resolve(tag, separator).into_owned())
            .filter(|s| !s.trim().is_empty());
        match album {
            Some(album) => Line::from(Span::styled(album, style)),
            None => {
                song.metadata
                    .get("name")
                    .map(|tag| strategy.resolve(tag, separator).into_owned())
                    .filter(|s| !s.trim().is_empty())
                    .map_or_else(
                        Line::default,
                        |name| Line::from(Span::styled(name, style)),
                    )
            }
        }
    }
    /// The audio-language button label: `[Audio]` (opens the language
    /// popup).
    fn audio_label() -> &'static str {
        "[Audio]"
    }
    /// The subtitle-language button label: `[Sub]` (opens the language
    /// popup).
    fn subtitle_label() -> &'static str {
        "[Sub]"
    }
    /// The mpv-mode buttons on row 0, right-aligned: `⤓` (Download, only
    /// while a ytdlp stream plays — furthest left), `[Audio]` (to the left
    /// of subtitles), `[Sub]` (furthest right). Returns (button, label, x,
    /// width); the leftmost button's x is the left edge of the cluster.
    fn mpv_button_layout(area: Rect, ctx: &Ctx) -> Vec<(MpvButton, String, u16, u16)> {
        let mut cluster: Vec<(MpvButton, String)> = Vec::new();
        cluster.push((MpvButton::Subs, Self::subtitle_label().to_owned()));
        cluster.push((MpvButton::Audio, Self::audio_label().to_owned()));
        if crate::ui::modals::paste::mpv_yt_info(ctx).is_some() {
            cluster.push((MpvButton::Download, "⤓".to_owned()));
        }
        let end = area.right().saturating_sub(1);
        let mut x = end;
        let mut out = Vec::new();
        for (btn, label) in cluster {
            let w = label.width() as u16;
            x = x.saturating_sub(w);
            out.push((btn, label, x, w));
            x = x.saturating_sub(1);
        }
        out
    }
    /// Click zones of the row-0 mpv buttons, in render order.
    fn mpv_button_zones(area: Rect, ctx: &Ctx) -> Vec<(MpvButton, u16, u16)> {
        Self::mpv_button_layout(area, ctx)
            .into_iter()
            .map(|(btn, _, x, w)| (btn, x, x + w))
            .collect()
    }
    /// Open the help-style language popup (audio or subtitles) and apply
    /// the chosen preference: update the runtime config, persist it to
    /// state.ron and re-select the matching track on the live mpv instance.
    fn open_language_menu(ctx: &Ctx, title: &str, audio: bool) {
        modal!(ctx, crate ::ui::modals::language::LanguageModal::new(title, audio));
    }
    /// The Download button: open the save-as menu for the ytdlp stream
    /// currently playing in mpv (audio/video, chapters as one file or per
    /// chapter).
    fn open_download_menu(ctx: &Ctx) {
        let Some(info) = crate::ui::modals::paste::mpv_yt_info(ctx) else { return };
        crate::ui::modals::paste::open_stream_download_menu(
            ctx,
            &info,
            &crate::shared::ytdlp::ReplaceAction::None,
        );
    }
    /// Click handler for the row-0 mpv buttons.
    fn do_mpv_button(&self, btn: MpvButton, ctx: &Ctx) -> Result<()> {
        match btn {
            MpvButton::Download => Self::open_download_menu(ctx),
            MpvButton::Audio => Self::open_language_menu(ctx, "Audio language", true),
            MpvButton::Subs => Self::open_language_menu(ctx, "Subtitle language", false),
        }
        Ok(())
    }
    fn time_text(ctx: &Ctx, bitrate: Option<u32>) -> String {
        if crate::core::mpv::mpv_is_ui_source(ctx) {
            let elapsed = crate::ui::panes::lyrics::format_clock(
                ctx.mpv.position as u64,
            );
            let duration = crate::ui::panes::lyrics::format_clock(
                ctx.mpv.duration as u64,
            );
            return format!("{elapsed} / {duration}");
        }
        let elapsed = ctx.status.elapsed.to_string();
        let duration = ctx.status.duration.to_string();
        match bitrate {
            Some(bitrate) => format!("{elapsed} / {duration} ({bitrate} kbps)"),
            None => format!("{elapsed} / {duration}"),
        }
    }
    /// Draw a styled line into `width` columns at (x, y). `center` centers
    /// the line when it fits; when it overflows, only the first `width`
    /// columns are shown. `style` is the base style, patched by each span's
    /// own style.
    #[allow(clippy::too_many_arguments)]
    fn draw_line(
        buf: &mut Buffer,
        x: u16,
        y: u16,
        width: u16,
        line: &Line,
        style: Style,
        center: bool,
    ) {
        let text_width = line.width() as u16;
        if width == 0 || line.width() == 0 {
            return;
        }
        if text_width <= width {
            let x0 = if center { x + (width - text_width) / 2 } else { x };
            let mut cx = x0;
            for span in &line.spans {
                buf.set_string(cx, y, span.content.as_ref(), style.patch(span.style));
                cx += span.width() as u16;
            }
            return;
        }
        Self::draw_spans(buf, x, y, &line.spans, 0, width, style);
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
    /// Draw a box divider row with junction glyphs: `left_glyph` at the
    /// left border column, `right_glyph` at the right border column, `─`
    /// between, and the (x, glyph) junction pairs overwrite their cells.
    fn draw_controls_divider(
        buf: &mut Buffer,
        y: u16,
        left: u16,
        right: u16,
        left_glyph: &str,
        right_glyph: &str,
        junctions: &[(u16, &str)],
        style: Style,
    ) {
        if right <= left {
            return;
        }
        buf.set_string(left, y, left_glyph, style);
        for col in (left + 1)..right {
            buf.set_string(col, y, "─", style);
        }
        buf.set_string(right, y, right_glyph, style);
        for (x, glyph) in junctions {
            if *x > left && *x < right {
                buf.set_string(*x, y, glyph, style);
            }
        }
    }
    /// The row offsets of the Playback Center: with the full 7-row box the
    /// content sits inside the borders (row 1 at `area.y + 1`, row 2 at
    /// `area.y + 3`); with a shorter pane the rows pack from the top.
    fn content_top(area: Rect) -> u16 {
        if area.height >= 7 { area.y + 1 } else { area.y }
    }
    fn draw_volume(
        buf: &mut Buffer,
        area: Rect,
        ctx: &Ctx,
        start: u16,
        slider_w: u16,
    ) -> u16 {
        let theme = ControlsTheme::from_ctx(ctx);
        let y = Self::content_top(area) + 2;
        let volume = crate::core::mpv::ui_volume(ctx).min(100) as u16;
        let filled_len = (f64::from(slider_w - 1) * f64::from(volume.min(100)) / 100.0)
            .round() as u16;
        let hovered = ctx
            .mouse_pos()
            .is_some_and(|p| p.y == y && p.x >= start && p.x < start + slider_w);
        let (filled, track) = if hovered {
            (
                crate::config::hover_style(theme.volume_filled),
                crate::config::hover_style(theme.volume_track),
            )
        } else {
            (theme.volume_filled, theme.volume_track)
        };
        for i in 0..slider_w {
            let style = if i < filled_len { filled } else { track };
            let c = if i == filled_len && filled_len < slider_w { "●" } else { "─" };
            buf.set_string(start + i, y, c, style);
        }
        buf.set_string(start + slider_w + 1, y, format!("{volume}%"), filled);
        start
    }
    fn do_transport(&self, btn: Transport, ctx: &Ctx) -> Result<()> {
        if crate::core::mpv::mpv_is_ui_source(ctx)
            && let Some(socket) = ctx.mpv.socket.clone()
        {
            match btn {
                Transport::Prev => {
                    if ctx.mpv.position > 3.0 {
                        crate::core::mpv::mpv_seek(&socket, 0.0);
                    }
                }
                Transport::PlayPause => crate::core::mpv::mpv_toggle_pause(&socket),
                Transport::Next => {
                    crate::core::mpv::mpv_seek(&socket, ctx.mpv.position + 30.0);
                }
                Transport::Stop => crate::core::mpv::mpv_quit(&socket),
            }
            ctx.render()?;
            return Ok(());
        }
        let state = ctx.status.state;
        let keep_state = ctx.config.keep_state_on_song_change;
        match btn {
            Transport::Prev => {
                if state != State::Stop {
                    let rewind_to_start = ctx.config.rewind_to_start_sec;
                    let elapsed_sec = ctx.status.elapsed.as_secs();
                    ctx.command(move |client| {
                        match rewind_to_start {
                            Some(value) => {
                                if elapsed_sec >= value {
                                    client.seek_current(ValueChange::Set(0))?;
                                } else {
                                    client.prev_keep_state(keep_state, state)?;
                                }
                            }
                            None => {
                                client.prev_keep_state(keep_state, state)?;
                            }
                        }
                        Ok(())
                    });
                    ctx.render()?;
                }
            }
            Transport::PlayPause => {
                if matches!(state, State::Play | State::Pause) {
                    ctx.command(move |client| {
                        client.pause_toggle()?;
                        Ok(())
                    });
                } else {
                    ctx.command(move |client| {
                        client.play()?;
                        Ok(())
                    });
                }
                ctx.render()?;
            }
            Transport::Next => {
                if state != State::Stop {
                    ctx.command(move |client| {
                        client.next_keep_state(keep_state, state)?;
                        Ok(())
                    });
                    ctx.render()?;
                }
            }
            Transport::Stop => {
                if matches!(state, State::Play | State::Pause) {
                    ctx.command(move |client| {
                        client.stop()?;
                        Ok(())
                    });
                    ctx.render()?;
                }
            }
        }
        Ok(())
    }
    fn do_mode(&self, mode: Mode, ctx: &Ctx) -> Result<()> {
        match mode {
            Mode::Repeat => {
                let repeat = !ctx.status.repeat;
                ctx.command(move |client| {
                    client.repeat(repeat)?;
                    Ok(())
                });
            }
            Mode::Random => {
                let random = !ctx.status.random;
                ctx.command(move |client| {
                    client.random(random)?;
                    Ok(())
                });
            }
            Mode::Single => {
                let single = ctx.status.single;
                ctx.command(move |client| {
                    client.single(single.cycle_single())?;
                    Ok(())
                });
            }
            Mode::Consume => {
                let consume = ctx.status.consume;
                ctx.command(move |client| {
                    client.consume(consume.cycle())?;
                    Ok(())
                });
            }
        }
        ctx.render()?;
        Ok(())
    }
    fn set_volume(
        ctx: &Ctx,
        x: u16,
        _area: Rect,
        start: u16,
        slider_w: u16,
    ) -> Result<()> {
        let within = x.saturating_sub(start).min(slider_w - 1);
        let ratio = f32::from(within) / f32::from(slider_w - 1);
        let new_volume = (ratio * 100.0).clamp(0.0, 100.0).round() as u32;
        crate::core::mpv::set_volume(ctx, new_volume);
        ctx.render()?;
        Ok(())
    }
    /// Badge-row seekbar geometry (round 61 P3): the bar sits BESIDE the
    /// input-mode badge — one column gap after the badge separator `│`,
    /// one column before the right border. Shared by render and the mouse
    /// handlers so the click/drag scrub zones always match the drawn bar.
    fn seekbar_geometry(area: Rect) -> (u16, u16) {
        let nsep = area.x + 1 + BADGE_CELL_W;
        let bar_x = nsep + 2;
        let bar_w = area.right().saturating_sub(1).saturating_sub(bar_x);
        (bar_x, bar_w)
    }
    /// Draw the row-3 seekbar (the legacy bottom tail bar's seekbar moved
    /// into the badge row — round 61 P3). Hovering highlights the played
    /// portion; the keyboard-seek cursor (Ctrl+Tab on the Queue tab)
    /// still renders here.
    fn draw_seekbar(buf: &mut Buffer, area: Rect, y: u16, ctx: &Ctx) {
        let bar_cfg = &ctx.config.theme.progress_bar;
        let (bar_x, bar_w) = Self::seekbar_geometry(area);
        if bar_w == 0 {
            return;
        }
        let (elapsed, duration) = if crate::core::mpv::mpv_is_ui_source(ctx) {
            (ctx.mpv.position as u64, ctx.mpv.duration as u64)
        } else {
            (ctx.status.elapsed.as_secs(), ctx.status.duration.as_secs())
        };
        let value = if duration == 0 { 0.0 } else { elapsed as f32 / duration as f32 };
        let mouse = ctx.mouse_pos();
        let hovered = mouse.is_some_and(|p| p.y == y && p.x >= bar_x && p.x < bar_x + bar_w);
        let hover_col = mouse.filter(|_| hovered).map(|p| p.x.saturating_sub(bar_x));
        let cursor_col = crate::ui::seekbar::cursor_fraction(ctx)
            .map(|f| (f * f32::from(bar_w)).round() as u16)
            .map(|c| c.min(bar_w.saturating_sub(1)));
        let (elapsed_style, thumb_style, track_style) = if hovered || cursor_col.is_some() {
            (
                crate::config::hover_style(bar_cfg.elapsed_style),
                crate::config::hover_style(bar_cfg.thumb_style),
                bar_cfg.track_style,
            )
        } else {
            (bar_cfg.elapsed_style, bar_cfg.thumb_style, bar_cfg.track_style)
        };
        let bar = crate::ui::widgets::progress_bar::ProgressBar::builder()
            .elapsed_style(elapsed_style)
            .thumb_style(thumb_style)
            .track_style(track_style)
            .start_char(&bar_cfg.symbols[0])
            .elapsed_char(&bar_cfg.symbols[1])
            .thumb_char(&bar_cfg.symbols[2])
            .track_char(&bar_cfg.symbols[3])
            .end_char(&bar_cfg.symbols[4])
            .use_track_when_empty(ctx.config.theme.progress_bar.use_track_when_empty)
            .value(value)
            .maybe_hover_col(hover_col)
            .maybe_cursor_col(cursor_col)
            .build();
        bar.render(Rect::new(bar_x, y, bar_w, 1), buf);
    }
    /// Scrub the row-3 seekbar (click/drag): seek the mpv session when mpv
    /// is the UI source, otherwise the MPD server — mirroring the removed
    /// legacy ProgressBar pane's behavior.
    fn do_seekbar(ctx: &Ctx, x: u16, bar_x: u16, bar_w: u16) -> Result<()> {
        if crate::core::mpv::mpv_is_ui_source(ctx)
            && let Some(socket) = ctx.mpv.socket.clone()
        {
            let fraction = f32::from(x.saturating_sub(bar_x)) / f32::from(bar_w);
            crate::core::mpv::mpv_seek(&socket, f64::from(ctx.mpv.duration as f32 * fraction));
            ctx.render()?;
            return Ok(());
        }
        if matches!(ctx.status.state, State::Play | State::Pause) {
            let second_to_seek_to = ctx
                .status
                .duration
                .mul_f32(f32::from(x.saturating_sub(bar_x)) / f32::from(bar_w))
                .as_secs();
            ctx.command(move |client| {
                client.seek_current(ValueChange::Set(u32::try_from(second_to_seek_to)?))?;
                Ok(())
            });
            ctx.render()?;
        }
        Ok(())
    }
    /// Draw `text` at (x, y) with `style`; returns its width in columns.
    fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style) -> u16 {
        buf.set_string(x, y, text, style);
        text.width() as u16
    }
}
impl Pane for ControlsPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        self.area = area;

        if area.height == 0 || area.width < 10 {
            return Ok(());
        }
        let buf = frame.buffer_mut();
        let theme = ControlsTheme::from_ctx(ctx);
        let left = area.x;
        let right = area.right().saturating_sub(1);
        // Round 60c (S4): the Playback Center matches the design mock —
        // a box with the top border, row 1 (artist–album | title |
        // queue modifiers), the `├───┬───┬───┤` divider, row 2 (the three
        // `│`-separated columns time+bitrate | transport | volume), the
        // `├───┬───┴───…───┤` divider and row 3 (the NORMAL badge), then
        // the bottom border. The box borders need 7 rows; with a shorter
        // pane the content rows pack from the top (no borders).
        let full = area.height >= 7;
        let top_edge = area.y.saturating_sub(u16::from(!full));
        let y0 = if full { area.y + 1 } else { area.y }; // row 1
        let y1 = y0 + 1; // divider 1
        let y2 = y0 + 2; // row 2
        let y3 = y0 + 3; // divider 2
        let y4 = y0 + 4; // row 3
        let bottom_edge = if full { area.bottom() - 1 } else { u16::MAX };
        // The NORMAL-badge cell boundary column shared by the badge row's
        // `│` separator and divider 2's `┬` (round 63 3a: the bottom border
        // mirrors the divider with a `┴` at the same column).
        let nsep = left + 1 + BADGE_CELL_W;
        let border_style = ctx.config.as_border_style();
        if full {
            for x in (left + 1)..right {
                buf.set_string(x, top_edge, "─", border_style);
                buf.set_string(x, bottom_edge, "─", border_style);
            }
            buf.set_string(left, top_edge, "╭", border_style);
            buf.set_string(right, top_edge, "╮", border_style);
            buf.set_string(left, bottom_edge, "╰", border_style);
            buf.set_string(right, bottom_edge, "╯", border_style);
            if nsep > left && nsep < right {
                buf.set_string(nsep, bottom_edge, "┴", border_style);
            }
        }
        // Round 63 (3b): the marquee clock is a WALL CLOCK, not playback
        // elapsed — the title (and the left line) keep scrolling while the
        // playback is paused, stopped or at the very start of a track. The
        // anchor still restarts per track (a new song id/file, or the mpv
        // title while an mpv session is the UI source) and per tab switch.
        let track_key = if crate::core::mpv::mpv_is_ui_source(ctx) {
            Some((0, ctx.mpv.title.clone()))
        } else {
            ctx.find_current_song_in_queue()
                .map(|(_, song)| (song.id, song.file.clone()))
        };
        if self.last_song_key != track_key
            || self.last_tab.as_deref() != Some(ctx.active_tab.as_str())
        {
            self.last_song_key = track_key;
            self.last_tab = Some(ctx.active_tab.to_string());
            self.carousel_anchor = None;
        }
        let carousel_phase = match self.carousel_anchor {
            Some(anchor) => anchor.elapsed().as_millis() as u64,
            None => {
                self.carousel_anchor = Some(std::time::Instant::now());
                0
            }
        };
        let mouse = ctx.mouse_pos();
        let show_modes = area.width >= 42;
        let (mode_start, mpv_buttons) = if crate::core::mpv::mpv_is_ui_source(ctx) {
            let buttons = Self::mpv_button_layout(area, ctx);
            let left_x = buttons
                .iter()
                .map(|(_, _, x, _)| *x)
                .min()
                .unwrap_or_else(|| area.right().saturating_sub(1));
            (left_x, Some(buttons))
        } else if show_modes {
            (Self::mode_start_x(area), None)
        } else {
            (area.right(), None)
        };
        if y0 < area.bottom() {
            buf.set_string(left, y0, "│", border_style);
            buf.set_string(right, y0, "│", border_style);
            if let Some(buttons) = &mpv_buttons {
                for (_, label, x, w) in buttons {
                    let mut style = theme.active;
                    let slot = Rect {
                        x: *x,
                        y: y0,
                        width: *w,
                        height: 1,
                    };
                    if mouse.is_some_and(|p| slot.contains(p)) {
                        style = crate::config::hover_style(style);
                    }
                    buf.set_string(*x, y0, label, style);
                }
            } else if show_modes {
                let mut mx = mode_start;
                for mode in [Mode::Repeat, Mode::Random, Mode::Single, Mode::Consume] {
                    let mut style = match Self::mode_state(mode, ctx) {
                        OnOffOneshot::Off => theme.inactive,
                        OnOffOneshot::On => theme.active,
                        OnOffOneshot::Oneshot => theme.oneshot,
                    };
                    let slot = Rect {
                        x: mx,
                        y: y0,
                        width: MODE_SLOT,
                        height: 1,
                    };
                    if mouse.is_some_and(|p| slot.contains(p)) {
                        style = crate::config::hover_style(style);
                    }
                    buf.set_string(mx, y0, Self::mode_info(mode), style);
                    mx += MODE_SLOT;
                }
            }
            // Row 1: artist–album LEFT, title CENTERED, queue modifiers
            // RIGHT. mpv-session and YouTube rows keep their per-source
            // lines (show/channel on the left).
            let channel_max = (mode_start.saturating_sub(area.x) / 3).max(8);
            let left_line = if crate::core::mpv::mpv_is_ui_source(ctx)
                || crate::ui::modals::paste::current_yt_info(ctx).is_some_and(|yt| {
                    yt.channel.as_deref().is_some_and(|c| !c.trim().is_empty())
                })
            {
                Self::channel_line(ctx)
            } else {
                Self::artist_album_line(ctx)
            };
            let title = Self::title_line(ctx);
            let channel_w = if left_line.spans.is_empty() {
                0
            } else if (left_line.width() as u16) > channel_max {
                // Round 63 (3b): an overflowing left line marquees like the
                // title (the whole reserved slot scrolls on the shared
                // wall-clock carousel instead of truncating forever).
                crate::ui::widgets::marquee::draw_marquee(
                    buf,
                    area.x + 1,
                    y0,
                    channel_max,
                    &left_line,
                    Style::new(),
                    carousel_phase,
                );
                channel_max
            } else {
                let w = left_line.width() as u16;
                Self::draw_line(buf, area.x + 1, y0, w, &left_line, Style::new(), false);
                w
            };
            let title_region_start = area.x + 1 + channel_w + if channel_w > 0 { 2 } else { 0 };
            let title_region = mode_start.saturating_sub(title_region_start);
            if title_region > 2 && !title.spans.is_empty() {
                let group_w = title.width() as u16;
                if group_w <= title_region {
                    let x0 = title_region_start + (title_region - group_w) / 2;
                    Self::draw_line(buf, x0, y0, group_w, &title, Style::new(), false);
                } else if title_region > 4 {
                    crate::ui::widgets::marquee::draw_marquee(
                        buf,
                        title_region_start + 1,
                        y0,
                        title_region - 2,
                        &title,
                        Style::new(),
                        carousel_phase,
                    );
                } else {
                    crate::ui::widgets::marquee::draw_marquee(
                        buf,
                        title_region_start,
                        y0,
                        title_region,
                        &title,
                        Style::new(),
                        carousel_phase,
                    );
                }
            }
        }
        if ctx.status.elapsed.as_secs() != self.bitrate_sec {
            self.bitrate_sec = ctx.status.elapsed.as_secs();
            self.display_bitrate = ctx.status.bitrate;
        }
        let time = Self::time_text(ctx, self.display_bitrate);
        // Row-2 columns: time (left) / transport (centered) / volume
        // (right). The divider rows and the `│` separators share the two
        // column-boundary columns (`sep1`, `sep2`) so the junctions line
        // up across the box.
        let time_max = area
            .width
            .saturating_sub(TRANSPORT_CLUSTER_W + VOLUME_PCT_W + 4 * 2)
            .max(1);
        let time_w = (time.width() as u16).min(time_max);
        let (sep1, sep2) = Self::transport_columns(area, time_w);
        // Divider 1: `├───┬───┬───┤` (three `┬` columns).
        if y1 < area.bottom() {
            Self::draw_controls_divider(
                buf,
                y1,
                left,
                right,
                "├",
                "┤",
                &[(sep1, "┬"), (sep2, "┬")],
                border_style,
            );
        }
        if y2 < area.bottom() {
            let border_cols = border_style;
            buf.set_string(left, y2, "│", border_cols);
            buf.set_string(sep1, y2, "│", border_cols);
            buf.set_string(sep2, y2, "│", border_cols);
            buf.set_string(right, y2, "│", border_cols);
            Self::draw_line(
                buf,
                area.x + 1,
                y2,
                sep1.saturating_sub(area.x + 2),
                &Line::from(time),
                theme.time,
                false,
            );
            // Round 61 (P4): the volume block is centered inside its
            // right column (`sep2`…right border), not fitted to the edge.
            let (volume_start, volume_w) = volume_geometry(area, sep2);
            if volume_w >= 6 && volume_start < right {
                Self::draw_volume(buf, area, ctx, volume_start, volume_w);
            }
            let transport_start = sep1 + 1;
            let hover_zone = |z0: u16, z1: u16, base: Style| {
                if mouse.is_some_and(|p| p.y == y2 && p.x >= z0 && p.x < z1) {
                    crate::config::hover_style(base)
                } else {
                    base
                }
            };
            let play_pause_label = if crate::core::mpv::mpv_is_ui_source(ctx) {
                if ctx.mpv.paused { "▶" } else { "❙❙" }
            } else if ctx.status.state == State::Play {
                "❙❙"
            } else {
                "▶"
            };
            // The cluster `   ◀◀   ▶   ▶▶  ■   ` between the separators
            // (the stop button is no longer pipe-separated — round 60c S4
            // renders the mock's plain cluster).
            let mut x = transport_start;
            x += Self::put(buf, x, y2, "   ◀◀   ", hover_zone(transport_start + 3, transport_start + 8, theme.transport));
            let play_w = play_pause_label.width();
            x += Self::put(
                buf,
                x,
                y2,
                play_pause_label,
                hover_zone(transport_start + 8, transport_start + 12, theme.transport),
            );
            if play_w < 2 {
                x += Self::put(buf, x, y2, " ".repeat(2 - play_w).as_str(), theme.transport);
            }
            x += Self::put(buf, x, y2, "  ▶▶  ", hover_zone(transport_start + 12, transport_start + 16, theme.transport));
            let _ = Self::put(
                buf,
                x,
                y2,
                " ■   ",
                hover_zone(transport_start + 16, transport_start + 21, theme.transport),
            );
        }
        // NORMAL row: the left cell carries the input-mode badge; the
        // rest of the row carries the FUNCTIONAL seekbar (round 61 P3 —
        // the legacy bottom tail bar's badge + seekbar merged into this
        // one row; nothing renders below the box).
        if y3 < area.bottom() {
            Self::draw_controls_divider(
                buf,
                y3,
                left,
                right,
                "├",
                "┤",
                &[(nsep, "┬"), (sep1, "┴"), (sep2, "┴")],
                border_style,
            );
        }
        if y4 < area.bottom() {
            buf.set_string(left, y4, "│", border_style);
            buf.set_string(nsep, y4, "│", border_style);
            buf.set_string(right, y4, "│", border_style);
            // Round 62 (P1): the badge follows the THEME, no hardcoded
            // ANSI colors. NORMAL uses the border accent family the
            // navbar/outlines use (`highlight_border_style`, the same hue
            // `as_focused_border_style` resolves to) as the background with
            // black/bold text; INSERT keeps a distinct hue from the theme's
            // tab-bar active style (another theme-owned color, never a
            // constant), falling back to the accent when a theme provides
            // none.
            let badge_accent = ctx
                .config
                .theme
                .highlight_border_style
                .fg
                .or(ctx.config.theme.borders_style.fg)
                .unwrap_or(Color::Blue);
            let (label, style) = match ctx.input.mode() {
                crate::ui::input::InputMode::Insert(_) => {
                    let bg = ctx
                        .config
                        .theme
                        .tab_bar
                        .active_style
                        .bg
                        .unwrap_or(badge_accent);
                    (
                        "  INSERT  ",
                        Style::new().fg(Color::Black).bg(bg).add_modifier(Modifier::BOLD),
                    )
                }
                crate::ui::input::InputMode::Normal => {
                    (
                        "  NORMAL  ",
                        Style::new().fg(Color::Black).bg(badge_accent).add_modifier(Modifier::BOLD),
                    )
                }
            };
            // The 10-column badge cell, fully cleared then centered.
            let cell_x = area.x + 1;
            for col in cell_x..nsep {
                buf[(col, y4)].set_symbol(" ").set_style(style);
            }
            for (offset, ch) in label.char_indices() {
                let col = cell_x + offset as u16;
                if col < nsep {
                    buf[(col, y4)].set_symbol(&ch.to_string()).set_style(style);
                }
            }
            // One blank column between the badge separator and the bar.
            buf[(nsep + 1, y4)].set_symbol(" ");
            Self::draw_seekbar(buf, area, y4, ctx);
        }
        Ok(())
    }
    fn handle_action(&mut self, _event: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if !self.area.contains(event.into()) {
            return Ok(());
        }
        let x = event.x;
        let y = event.y;
        let row0 = Self::content_top(self.area);
        let row2 = row0 + 2;
        let row3 = row0 + 4; // the badge + seekbar row (round 61 P3)
        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                if y == row0 {
                    if crate::core::mpv::mpv_is_ui_source(ctx) {
                        for (btn, z0, z1) in Self::mpv_button_zones(self.area, ctx) {
                            if x >= z0 && x < z1 {
                                return self.do_mpv_button(btn, ctx);
                            }
                        }
                    } else if self.area.width >= 42 {
                        let mode_start = Self::mode_start_x(self.area);
                        if x >= mode_start && x < mode_start + MODE_SLOT * 4 {
                            let slot = (x - mode_start) / MODE_SLOT;
                            let mode = [
                                Mode::Repeat,
                                Mode::Random,
                                Mode::Single,
                                Mode::Consume,
                            ][slot as usize];
                            return self.do_mode(mode, ctx);
                        }
                    }
                } else if y == row2 {
                    let time_w = Self::time_text(ctx, self.display_bitrate).width() as u16;
                    let (_, zones) = Self::transport_zones(self.area, time_w);
                    for (btn, z0, z1) in zones {
                        if x >= z0 && x < z1 {
                            return self.do_transport(btn, ctx);
                        }
                    }
                    // The volume geometry is shared with render, so the
                    // click zone matches the centered block (round 61 P4).
                    let (_, sep2) = Self::transport_columns(self.area, time_w);
                    let (volume_start, volume_w) = volume_geometry(self.area, sep2);
                    if x >= volume_start && x < volume_start + volume_w {
                        return Self::set_volume(
                            ctx,
                            x,
                            self.area,
                            volume_start,
                            volume_w,
                        );
                    }
                } else if y == row3 {
                    // Scrub zone: any click on the badge-row seekbar seeks
                    // to the clicked fraction (round 61 P3).
                    let (bar_x, bar_w) = Self::seekbar_geometry(self.area);
                    if bar_w > 0 && x >= bar_x && x < bar_x + bar_w {
                        return Self::do_seekbar(ctx, x, bar_x, bar_w);
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                let base = crate::core::mpv::ui_volume(ctx) as i16;
                let new_volume = (base + VOLUME_SCROLL_STEP as i16).clamp(0, 100) as u32;
                crate::core::mpv::set_volume(ctx, new_volume);
                ctx.render()?;
            }
            MouseEventKind::ScrollDown => {
                let base = crate::core::mpv::ui_volume(ctx) as i16;
                let new_volume = (base - VOLUME_SCROLL_STEP as i16).clamp(0, 100) as u32;
                crate::core::mpv::set_volume(ctx, new_volume);
                ctx.render()?;
            }
            MouseEventKind::Drag { drag_start_position } => {
                let time_w = Self::time_text(ctx, self.display_bitrate).width() as u16;
                let (_, sep2) = Self::transport_columns(self.area, time_w);
                let (volume_start, volume_w) = volume_geometry(self.area, sep2);
                if drag_start_position.y == row2
                    && drag_start_position.x >= volume_start
                    && drag_start_position.x < volume_start + volume_w
                {
                    return Self::set_volume(ctx, x, self.area, volume_start, volume_w);
                }
                // Drag on the badge-row seekbar scrubs (round 61 P3).
                let (bar_x, bar_w) = Self::seekbar_geometry(self.area);
                if drag_start_position.y == row3
                    && drag_start_position.x >= bar_x
                    && drag_start_position.x < bar_x + bar_w
                {
                    return Self::do_seekbar(ctx, x, bar_x, bar_w);
                }
            }
            _ => {}
        }
        Ok(())
    }
}
