use anyhow::Result;
use ratatui::{
    Frame,
    prelude::{Buffer, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
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
        ext::duration::DurationExt,
        keys::ActionEvent,
        macros::modal,
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

/// Transport cluster on line 3: `|  ◀◀   ▶   ▶▶  |  ■  ` with the stop
/// button separated from the rest by a pipe. Width of the whole cluster.
const TRANSPORT_CLUSTER_W: u16 = 25;

/// Mode toggle buttons, right-aligned on the first line.
const MODE_SLOT: u16 = 9;

/// Width of the " 100%" text right of the volume slider.
const VOLUME_PCT_W: u16 = 5;

/// Mouse scroll step for the volume slider (finer than the keybind step).
const VOLUME_SCROLL_STEP: u32 = 2;

/// Volume slider geometry: (start_x, slider_width) fitting between the
/// transport buttons and the percentage text, capped so the slider stays
/// compact (21 cols -> 20 positions -> exactly 5% per click step).
fn volume_geometry(area: Rect, transport_end: u16) -> (u16, u16) {
    let right_edge = area.right().saturating_sub(1);
    let available = right_edge.saturating_sub(transport_end + 2 + VOLUME_PCT_W);
    let slider_w = available.clamp(6, 21);
    let start = right_edge.saturating_sub(VOLUME_PCT_W + slider_w);
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
    /// The carousel clock value the title marquee was last restarted at
    /// (a new song/video, or the tab being switched): the phase is
    /// `clock - anchor`, so the title always starts from its beginning with
    /// the 2s hold when it is (re)shown.
    carousel_anchor: Option<u64>,
    /// The tab last rendered (the carousel restarts when it changes).
    last_tab: Option<String>,
}

impl ControlsPane {
    pub fn new() -> Self {
        Self {
            area: Rect::default(),
            bitrate_sec: u64::MAX,
            display_bitrate: None,
            carousel_anchor: None,
            last_tab: None,
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
                if ctx.status.repeat {
                    OnOffOneshot::On
                } else {
                    OnOffOneshot::Off
                }
            }
            Mode::Random => {
                if ctx.status.random {
                    OnOffOneshot::On
                } else {
                    OnOffOneshot::Off
                }
            }
            Mode::Single => ctx.status.single,
            Mode::Consume => ctx.status.consume,
        }
    }

    fn mode_start_x(area: Rect) -> u16 {
        area.right().saturating_sub(1 + MODE_SLOT * 4)
    }

    /// Click zones of the line-3 transport cluster, in render order. Each
    /// zone covers the button's slot including its padding.
    fn transport_zones(area: Rect) -> (u16, [(Transport, u16, u16); 4]) {
        let start = area.x + area.width.saturating_sub(TRANSPORT_CLUSTER_W) / 2;
        let zones = [
            (Transport::Prev, start + 4, start + 9),
            (Transport::PlayPause, start + 9, start + 13),
            (Transport::Next, start + 13, start + 17),
            (Transport::Stop, start + 22, start + 25),
        ];
        (start, zones)
    }

    /// `Artist - Title` for the now-playing line (fallbacks "Unknown" /
    /// "No Playback"), joined with a dash per the user's layout. While an
    /// mpv video plays, shows the video's title/series instead; a resolved
    /// YouTube audio stream shows the video title.
    fn artist_title_line(ctx: &Ctx) -> Line<'static> {
        let theme = ControlsTheme::from_ctx(ctx);
        let artist_style = theme.artist;
        let title_style = theme.title;
        let separator_style = theme.separator;
        let separator = &ctx.config.theme.format_tag_separator;
        let strategy = ctx.config.theme.multiple_tag_resolution_strategy;

        if crate::core::mpv::mpv_is_ui_source(ctx) {
            // The video/episode/movie title is its own centered line; the
            // channel/show rides the line above (see `channel_line`).
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
        // A YouTube-style stream has no metadata of its own; show the video
        // title that was captured when the stream was resolved.
        if let Some(yt) = ctx.yt_info.borrow().get(&song.file)
            && !yt.title.is_empty()
        {
            return Line::from(Span::styled(yt.title.clone(), title_style));
        }
        // Omit missing tags instead of showing an "Unknown" / "No Playback"
        // placeholder: only the known parts of `Artist - Title` are shown.
        let artist = song
            .metadata
            .get("artist")
            .map(|tag| strategy.resolve(tag, separator).into_owned())
            .filter(|s| !s.trim().is_empty());
        let title = song
            .metadata
            .get("title")
            .map(|tag| strategy.resolve(tag, separator).into_owned())
            .filter(|s| !s.trim().is_empty());
        match (artist, title) {
            (Some(artist), Some(title)) => Line::from(vec![
                Span::styled(artist, artist_style),
                Span::styled(" - ", separator_style),
                Span::styled(title, title_style),
            ]),
            (Some(artist), None) => Line::from(Span::styled(artist, artist_style)),
            (None, Some(title)) => Line::from(Span::styled(title, title_style)),
            (None, None) => Line::from(Span::styled("No Playback", title_style)),
        }
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
            // Jellyfin: the show/series (or album artist); YouTube: the
            // channel. Empty for e.g. a movie with no artist.
            if ctx.mpv.artist.is_empty() {
                return Line::default();
            }
            return Line::from(Span::styled(ctx.mpv.artist.clone(), style));
        }
        let song = ctx.find_current_song_in_queue().map(|(_, song)| song);
        let Some(song) = song else { return Line::default() };
        // A YouTube-style stream (playing as audio through MPD): the
        // channel/uploader.
        if let Some(yt) = crate::ui::modals::paste::current_yt_info(ctx)
            && let Some(channel) = yt.channel.as_deref().filter(|c| !c.trim().is_empty())
        {
            return Line::from(Span::styled(channel.to_owned(), style));
        }
        // The album tag (omitted when missing, like the title line omits
        // missing tags); radio/stream entries fall back to their `name` tag
        // (the station name).
        let album = song
            .metadata
            .get("album")
            .map(|tag| strategy.resolve(tag, separator).into_owned())
            .filter(|s| !s.trim().is_empty());
        match album {
            Some(album) => Line::from(Span::styled(album, style)),
            None => song
                .metadata
                .get("name")
                .map(|tag| strategy.resolve(tag, separator).into_owned())
                .filter(|s| !s.trim().is_empty())
                .map_or_else(Line::default, |name| Line::from(Span::styled(name, style))),
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
        // Process in cluster order (Subs, Audio, Download) with a
        // right-decreasing cursor: Subs lands furthest right, Download
        // furthest left.
        for (btn, label) in cluster {
            let w = label.width() as u16;
            x = x.saturating_sub(w);
            out.push((btn, label, x, w));
            x = x.saturating_sub(1); // 1-column gap between buttons
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
        modal!(ctx, crate::ui::modals::language::LanguageModal::new(ctx, title, audio));
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
        // While an mpv video plays, the time reflects mpv's position (the
        // clock format always keeps the hours for videos >= 1h).
        if crate::core::mpv::mpv_is_ui_source(ctx) {
            let elapsed = crate::ui::panes::lyrics::format_clock(ctx.mpv.position as u64);
            let duration = crate::ui::panes::lyrics::format_clock(ctx.mpv.duration as u64);
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

    fn draw_volume(buf: &mut Buffer, area: Rect, ctx: &Ctx, start: u16, slider_w: u16) -> u16 {
        let theme = ControlsTheme::from_ctx(ctx);
        let y = area.y + 2;
        let volume = crate::core::mpv::ui_volume(ctx).min(100) as u16;
        let filled_len =
            (f64::from(slider_w - 1) * f64::from(volume.min(100)) / 100.0).round() as u16;

        // Hovering the slider lightens its colors (clickable control).
        let hovered =
            ctx.mouse_pos().is_some_and(|p| p.y == y && p.x >= start && p.x < start + slider_w);
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
        // While an mpv video is the UI source, the transport controls
        // drive mpv.
        if crate::core::mpv::mpv_is_ui_source(ctx)
            && let Some(socket) = ctx.mpv.socket.clone()
        {
            match btn {
                Transport::Prev => {
                    // Restart when past the first seconds, else nothing.
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
                    // Single is always enabled as a oneshot.
                    client.single(single.cycle_single())?;
                    Ok(())
                });
            }
            Mode::Consume => {
                let consume = ctx.status.consume;
                ctx.command(move |client| {
                    // Consume cycles all three states: off -> on -> oneshot.
                    client.consume(consume.cycle())?;
                    Ok(())
                });
            }
        }
        ctx.render()?;
        Ok(())
    }

    fn set_volume(ctx: &Ctx, x: u16, _area: Rect, start: u16, slider_w: u16) -> Result<()> {
        // Map the click across the visible slider bar only.
        let within = x.saturating_sub(start).min(slider_w - 1);
        let ratio = f32::from(within) / f32::from(slider_w - 1);
        let new_volume = (ratio * 100.0).clamp(0.0, 100.0).round() as u32;
        crate::core::mpv::set_volume(ctx, new_volume);
        ctx.render()?;
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
        if area.height < 3 || area.width < 10 {
            return Ok(());
        }
        let buf = frame.buffer_mut();
        let theme = ControlsTheme::from_ctx(ctx);
        let y0 = area.y;
        let y1 = area.y + 1;
        let y2 = area.y + 2;
        // Drive the carousel from rmpc's smooth local clock (which advances
        // with sub-update precision while playing) instead of MPD's elapsed,
        // which is only reported to 0.1s and makes the 5 col/sec steps arrive
        // with visible jitter. While an mpv video plays, MPD is paused (so
        // that clock is frozen): the carousel follows mpv's position instead.
        let elapsed_ms = if crate::core::mpv::mpv_is_ui_source(ctx) {
            (ctx.mpv.position * 1000.0) as u64
        } else {
            ctx.song_played.unwrap_or(ctx.status.elapsed).as_millis() as u64
        };
        // The carousel phase restarts at the current clock when the pane is
        // first shown, the clock went backwards (a new song/video reset it
        // to zero), or the tab was switched — the title always begins with
        // its 2s hold.
        if self.last_tab.as_deref() != Some(ctx.active_tab.as_str()) {
            self.last_tab = Some(ctx.active_tab.to_string());
            self.carousel_anchor = None;
        }
        let carousel_phase = match self.carousel_anchor {
            Some(anchor) if elapsed_ms >= anchor => elapsed_ms - anchor,
            _ => {
                self.carousel_anchor = Some(elapsed_ms);
                0
            }
        };

        // ---------- line 1: Channel/Show/Album | modes or mpv buttons ----------
        // The channel line is left-aligned (truncated, never scrolls) in the
        // space between the left border and the right-aligned buttons; the
        // buttons are the MPD mode toggles, or — while an mpv session is the
        // UI source — the mpv buttons (Download / Audio / Subs).
        let mouse = ctx.mouse_pos();
        let show_modes = area.width >= 42;
        let (mode_start, mpv_buttons) = if crate::core::mpv::mpv_is_ui_source(ctx) {
            let buttons = Self::mpv_button_layout(area, ctx);
            // `mpv_button_layout` pushes rightmost-first ([Sub],
            // [Audio], ⤓), so the cluster's left edge is the minimum x
            // (⤓ when a ytdlp stream plays, else [Audio]). Taking the
            // first entry instead — the rightmost [Sub] — would let the
            // title region run over the buttons.
            let left = buttons
                .iter()
                .map(|(_, _, x, _)| *x)
                .min()
                .unwrap_or_else(|| area.right().saturating_sub(1));
            (left, Some(buttons))
        } else if show_modes {
            (Self::mode_start_x(area), None)
        } else {
            (area.right(), None)
        };
        if let Some(buttons) = &mpv_buttons {
            for (_, label, x, w) in buttons {
                let mut style = theme.active;
                let slot = Rect { x: *x, y: y0, width: *w, height: 1 };
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
                // Hovering a toggle lightens it (clickable text).
                let slot = Rect { x: mx, y: y0, width: MODE_SLOT, height: 1 };
                if mouse.is_some_and(|p| slot.contains(p)) {
                    style = crate::config::hover_style(style);
                }
                buf.set_string(mx, y0, Self::mode_info(mode), style);
                mx += MODE_SLOT;
            }
        }

        // Channel/Show/Album between the left border and the left edge of
        // the buttons: left-aligned and truncated (never scrolls), up to a
        // third of the row so the title keeps the rest.
        let channel = Self::channel_line(ctx);
        let channel_region = mode_start.saturating_sub(area.x);
        let channel_max = (channel_region / 3).max(8);
        let channel_w = if channel.spans.is_empty() {
            0
        } else {
            let w = (channel.width() as u16).min(channel_max);
            Self::draw_line(buf, area.x, y0, w, &channel, Style::new(), false);
            w
        };

        // ---------- title: centered in the space between the channel and
        // the buttons ----------
        // The Artist+Song / episode / movie / video title is centered in the
        // region between the channel line and the buttons; when it overflows
        // it becomes a continuous marquee inside that region.
        let group = Self::artist_title_line(ctx);
        let title_region_start = area.x + channel_w + if channel_w > 0 { 2 } else { 0 };
        let title_region = mode_start.saturating_sub(title_region_start);
        if title_region > 2 && !group.spans.is_empty() {
            let group_w = group.width() as u16;
            if group_w <= title_region {
                let x0 = title_region_start + (title_region - group_w) / 2;
                Self::draw_line(buf, x0, y0, group_w, &group, Style::new(), false);
            } else if title_region > 4 {
                crate::ui::widgets::marquee::draw_marquee(
                    buf,
                    title_region_start + 1,
                    y0,
                    title_region - 2,
                    &group,
                    Style::new(),
                    carousel_phase,
                );
            } else {
                crate::ui::widgets::marquee::draw_marquee(
                    buf,
                    title_region_start,
                    y0,
                    title_region,
                    &group,
                    Style::new(),
                    carousel_phase,
                );
            }
        }

        // ---------- line 2: horizontal separator (outline color) ----------
        let separator_style = ctx.config.as_border_style();
        for x in area.x..area.right() {
            buf.set_string(x, y1, "─", separator_style);
        }

        // ---------- line 3: time | | prev play next | stop | volume ----------
        // The transport cluster stays centered on the pane width so changing
        // text (bitrate) never shifts it; time is truncated on the left and
        // volume is hidden if it would overlap.
        let (transport_start, _) = Self::transport_zones(area);
        let transport_end = transport_start + TRANSPORT_CLUSTER_W;

        // Refresh the displayed bitrate once per elapsed second so the time
        // text updates as a unit (matching the elapsed cadence) instead of
        // jittering whenever MPD reports a new VBR value mid-second.
        if ctx.status.elapsed.as_secs() != self.bitrate_sec {
            self.bitrate_sec = ctx.status.elapsed.as_secs();
            self.display_bitrate = ctx.status.bitrate;
        }
        let time = Self::time_text(ctx, self.display_bitrate);
        let time_max = transport_start.saturating_sub(area.x + 1).max(1);
        Self::draw_line(buf, area.x, y2, time_max, &Line::from(time), theme.time, false);

        let (volume_start, volume_w) = volume_geometry(area, transport_end);
        if volume_w >= 6 {
            Self::draw_volume(buf, area, ctx, volume_start, volume_w);
        }

        // `|  ◀◀   ▶   ▶▶  |  ■` — the stop button is separated from the
        // transport cluster by a pipe. Hovering a button lightens it.
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
        let mut x = transport_start;
        x += Self::put(buf, x, y2, "|   ", theme.transport);
        x += Self::put(
            buf,
            x,
            y2,
            "◀◀   ",
            hover_zone(transport_start + 4, transport_start + 9, theme.transport),
        );
        let play_w = play_pause_label.width();
        x += Self::put(
            buf,
            x,
            y2,
            play_pause_label,
            hover_zone(transport_start + 9, transport_start + 13, theme.transport),
        );
        // pad the play slot to 4 columns so the cluster keeps its shape
        if play_w < 4 {
            x += Self::put(buf, x, y2, " ".repeat(4 - play_w).as_str(), theme.transport);
        }
        x += Self::put(
            buf,
            x,
            y2,
            "▶▶  ",
            hover_zone(transport_start + 13, transport_start + 17, theme.transport),
        );
        x += Self::put(buf, x, y2, "  |  ", theme.transport);
        let _ = Self::put(
            buf,
            x,
            y2,
            "■  ",
            hover_zone(transport_start + 22, transport_start + 25, theme.transport),
        );

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

        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                if y == self.area.y {
                    // row 0: mpv buttons (Download / Audio / Subs) while an
                    // mpv session is the UI source, else the MPD modes.
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
                            let mode = [Mode::Repeat, Mode::Random, Mode::Single, Mode::Consume]
                                [slot as usize];
                            return self.do_mode(mode, ctx);
                        }
                    }
                } else if y == self.area.y + 2 {
                    // transport cluster (| prev play next | stop)
                    let (_, zones) = Self::transport_zones(self.area);
                    for (btn, z0, z1) in zones {
                        if x >= z0 && x < z1 {
                            return self.do_transport(btn, ctx);
                        }
                    }
                    // volume (clickable over the visible slider only)
                    let (transport_start, _) = Self::transport_zones(self.area);
                    let (volume_start, volume_w) =
                        volume_geometry(self.area, transport_start + TRANSPORT_CLUSTER_W);
                    if x >= volume_start && x < volume_start + volume_w {
                        return Self::set_volume(ctx, x, self.area, volume_start, volume_w);
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
                let (transport_start, _) = Self::transport_zones(self.area);
                let (volume_start, volume_w) =
                    volume_geometry(self.area, transport_start + TRANSPORT_CLUSTER_W);
                if drag_start_position.y == self.area.y + 2
                    && drag_start_position.x >= volume_start
                    && drag_start_position.x < volume_start + volume_w
                {
                    return Self::set_volume(ctx, x, self.area, volume_start, volume_w);
                }
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::{collections::HashMap, time::Duration};

    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    use super::*;
    use crate::{
        mpd::commands::{Song, State, Status, Volume, status::OnOffOneshot},
        tests::fixtures::ctx,
    };

    fn playing_ctx(mut ctx: Ctx) -> Ctx {
        let song = Song {
            id: 1,
            file: "file.flac".to_owned(),
            duration: Some(Duration::from_secs(248)),
            metadata: HashMap::from([
                ("artist".to_string(), "Delta Heavy".into()),
                ("album".to_string(), "Paradise Lost".into()),
                ("title".to_string(), "Punish My Love".into()),
            ]),
            last_modified: chrono::Utc::now(),
            added: None,
        };
        ctx.queue = vec![song];
        ctx.status = Status {
            state: State::Play,
            songid: Some(1),
            song: Some(0),
            elapsed: Duration::from_secs(140),
            duration: Duration::from_secs(248),
            volume: Volume::new(40),
            bitrate: Some(819),
            repeat: false,
            random: true,
            single: OnOffOneshot::Off,
            consume: OnOffOneshot::On,
            ..Default::default()
        };
        ctx
    }

    fn controls_lines(ctx: &Ctx, width: u16) -> Vec<String> {
        let backend = TestBackend::new(width, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut pane = ControlsPane::new();
        terminal
            .draw(|frame| {
                let inner = Rect { x: 0, y: 0, width, height: 3 };
                pane.render(frame, inner, ctx).unwrap();
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..3)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect()
            })
            .collect()
    }

    #[rstest::rstest]
    fn now_playing_line_is_artist_dash_title(ctx: Ctx) {
        let ctx = playing_ctx(ctx);
        let lines = controls_lines(&ctx, 90);
        // Row 0 carries the channel (left), the Artist - Title (centered in
        // the middle region) and the modes (right).
        assert!(lines[0].contains("Delta Heavy - Punish My Love"));
        assert!(lines[0].starts_with("Paradise Lost"));
        assert!(lines[0].trim_end().ends_with("Consume"));
    }

    #[rstest::rstest]
    fn channel_line_is_album_for_audio(ctx: Ctx) {
        let ctx = playing_ctx(ctx);
        let line = ControlsPane::channel_line(&ctx);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Paradise Lost");
    }

    #[rstest::rstest]
    fn channel_line_is_show_for_mpv_video(mut ctx: Ctx) {
        let mut ctx = playing_ctx(ctx);
        ctx.mpv.active = true;
        ctx.mpv.paused = false;
        ctx.mpv.title = "Test Episode".into();
        ctx.mpv.artist = "Test Show".into();
        let line = ControlsPane::channel_line(&ctx);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Test Show");
        // the title line shows the episode/movie/video name only
        let title = ControlsPane::artist_title_line(&ctx);
        let title_text: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(title_text, "Test Episode");
    }

    #[rstest::rstest]
    fn transport_cluster_has_pipes_around_buttons(ctx: Ctx) {
        let ctx = playing_ctx(ctx);
        let lines = controls_lines(&ctx, 90);
        let line3 = &lines[2];
        assert!(line3.contains("◀◀"));
        assert!(line3.contains("▶▶"));
        assert!(line3.contains("■"));
        // the stop button is separated from prev/play/next by a pipe
        let before_stop = &line3[..line3.rfind("■").unwrap()];
        assert!(before_stop.trim_end().ends_with("|"));
        // and the transport cluster starts with a pipe too
        assert!(line3.contains("|   ◀◀"));
    }

    #[rstest::rstest]
    fn time_and_volume_are_shown(ctx: Ctx) {
        let ctx = playing_ctx(ctx);
        let lines = controls_lines(&ctx, 90);
        assert!(lines[2].contains("2:20 / 4:08 (819 kbps)"));
        assert!(lines[2].contains("40%"));
    }

    #[rstest::rstest]
    fn missing_artist_is_omitted_from_now_playing_line(mut ctx: Ctx) {
        let song = Song {
            id: 7,
            file: "file.flac".to_owned(),
            duration: Some(Duration::from_secs(120)),
            metadata: HashMap::from([("title".to_string(), "The Sound of Grey".into())]),
            last_modified: chrono::Utc::now(),
            added: None,
        };
        ctx.queue = vec![song];
        ctx.status = Status { state: State::Play, songid: Some(7), ..Default::default() };
        let line = ControlsPane::artist_title_line(&ctx);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "The Sound of Grey");
        assert!(!text.contains("Unknown"));
    }

    #[rstest::rstest]
    fn missing_title_is_omitted_from_now_playing_line(mut ctx: Ctx) {
        let song = Song {
            id: 8,
            file: "file.flac".to_owned(),
            duration: Some(Duration::from_secs(120)),
            metadata: HashMap::from([("artist".to_string(), "Neemias Teixeira".into())]),
            last_modified: chrono::Utc::now(),
            added: None,
        };
        ctx.queue = vec![song];
        ctx.status = Status { state: State::Play, songid: Some(8), ..Default::default() };
        let line = ControlsPane::artist_title_line(&ctx);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Neemias Teixeira");
        assert!(!text.contains("No Playback"));
    }

    #[rstest::rstest]
    fn artist_title_marquees_when_truncated(ctx: Ctx) {
        let mut ctx = playing_ctx(ctx);
        // Narrow window: the title overflows the middle region. Freeze the
        // clock in the marquee's hold phase so the truncated start is on
        // screen.
        ctx.status.elapsed = Duration::ZERO;
        let lines = controls_lines(&ctx, 24);
        assert!(lines[0].contains("Delta Heavy"), "row: {:?}", lines[0]);
    }

    /// The mpv video is the UI source while it plays: the now-playing lines,
    /// the time and the volume come from mpv, even if MPD reports playing
    /// (the transient state before the mutual exclusion settles). The
    /// channel/show rides row 0, the video title row 1.
    #[rstest::rstest]
    fn controls_show_the_video_while_it_plays(ctx: Ctx) {
        let mut ctx = playing_ctx(ctx);
        ctx.mpv.active = true;
        ctx.mpv.paused = false;
        ctx.mpv.title = "Test Video Title".into();
        ctx.mpv.artist = "Test Channel".into();
        ctx.mpv.duration = 600.0;
        ctx.mpv.position = 42.0;
        ctx.mpv.volume = Some(77);
        let lines = controls_lines(&ctx, 90);
        assert!(lines[0].contains("Test Video Title"), "{lines:?}");
        assert!(lines[0].starts_with("Test Channel"), "{lines:?}");
        assert!(!lines[0].contains("Delta Heavy"), "{lines:?}");
        // mpv's clock and volume, not MPD's.
        assert!(lines[2].contains("0:42 / 10:00"), "{lines:?}");
        assert!(lines[2].contains("77%"), "{lines:?}");
    }

    /// MPD playback took over (the mutual exclusion paused the video): the
    /// controls follow the audio — the now-playing line, clock and volume
    /// are MPD's, and the play/pause button mirrors MPD's playing state.
    #[rstest::rstest]
    fn controls_follow_the_audio_when_music_takes_over(ctx: Ctx) {
        let mut ctx = playing_ctx(ctx);
        ctx.mpv.active = true;
        ctx.mpv.paused = true; // the mutual exclusion paused the video
        ctx.mpv.title = "Test Video Title".into();
        ctx.mpv.artist = "Test Channel".into();
        ctx.mpv.duration = 600.0;
        ctx.mpv.position = 42.0;
        ctx.mpv.volume = Some(77);
        let lines = controls_lines(&ctx, 90);
        assert!(lines[0].contains("Delta Heavy - Punish My Love"), "{lines:?}");
        assert!(!lines[0].contains("Test Video Title"), "{lines:?}");
        assert!(lines[2].contains("2:20 / 4:08"), "{lines:?}");
        assert!(lines[2].contains("40%"), "{lines:?}");
        // the pause button: MPD is playing.
        assert!(lines[2].contains("❙❙"), "{lines:?}");
    }

    /// The music stopped: the controls return to the (still paused) video,
    /// so the transport keys resume it.
    #[rstest::rstest]
    fn controls_return_to_the_video_when_music_stops(ctx: Ctx) {
        let mut ctx = playing_ctx(ctx);
        ctx.status.state = State::Pause;
        ctx.mpv.active = true;
        ctx.mpv.paused = true;
        ctx.mpv.title = "Test Video Title".into();
        ctx.mpv.artist = "Test Channel".into();
        ctx.mpv.duration = 600.0;
        ctx.mpv.position = 42.0;
        let lines = controls_lines(&ctx, 90);
        assert!(lines[0].contains("Test Video Title"), "{lines:?}");
        assert!(lines[0].starts_with("Test Channel"), "{lines:?}");
        assert!(!lines[0].contains("Delta Heavy"), "{lines:?}");
        // the play button: the video is paused.
        assert!(lines[2].contains("▶"), "{lines:?}");
    }

    /// The mpv-mode buttons (Audio / Subs) replace the MPD mode toggles
    /// while a video plays; the Download button only appears once the
    /// playing media is a resolved ytdlp stream.
    #[rstest::rstest]
    fn mpv_buttons_replace_modes_while_a_video_plays(ctx: Ctx) {
        let mut ctx = playing_ctx(ctx);
        ctx.mpv.active = true;
        ctx.mpv.paused = false;
        ctx.mpv.title = "Video".into();
        ctx.mpv.artist = "Channel".into();
        let lines = controls_lines(&ctx, 90);
        // [Sub] furthest right, [Audio] to its left; no Repeat/Random/Single.
        assert!(lines[0].contains("[Sub]"), "{lines:?}");
        assert!(lines[0].contains("[Audio]"), "{lines:?}");
        assert!(!lines[0].contains("Repeat"), "{lines:?}");
        assert!(!lines[0].contains("Random"), "{lines:?}");
        assert!(!lines[0].contains("Single"), "{lines:?}");
        // no stream resolved -> no download button
        assert!(!lines[0].contains("⤓"), "{lines:?}");
    }

    #[rstest::rstest]
    fn download_button_only_shows_for_a_resolved_stream(ctx: Ctx) {
        let mut ctx = playing_ctx(ctx);
        ctx.mpv.active = true;
        ctx.mpv.paused = false;
        ctx.mpv.title = "Video".into();
        ctx.mpv.artist = "Channel".into();
        use crate::shared::ytdlp::YtStreamInfo;
        ctx.yt_info.borrow_mut().insert(
            "https://www.youtube.com/watch?v=abc".to_owned(),
            YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                original_url: "https://www.youtube.com/watch?v=abc".to_owned(),
                title: "Video".to_owned(),
                channel: Some("Channel".to_owned()),
                ..Default::default()
            },
        );
        // the mpv playlist entry is the original link
        ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
            "Video",
            "https://www.youtube.com/watch?v=abc",
            None,
        ));
        let lines = controls_lines(&ctx, 90);
        assert!(lines[0].contains("⤓"), "{lines:?}");
        // ordering: download left of [Audio] left of [Sub]
        let d = lines[0].find("⤓").unwrap();
        let a = lines[0].find("[Audio]").unwrap();
        let s = lines[0].find("[Sub]").unwrap();
        assert!(d < a && a < s, "{lines:?}");
    }

    /// The controls row 0 matches the reference layout: channel left,
    /// title centered between it and the buttons, separator + transport
    /// rows below.
    #[rstest::rstest]
    fn controls_row_0_channel_title_and_buttons(ctx: Ctx) {
        let mut ctx = playing_ctx(ctx);
        ctx.mpv.active = true;
        ctx.mpv.paused = false;
        ctx.mpv.title = "Shaders ARE Changing FOREVER".into();
        ctx.mpv.artist = "GOATED".into();
        ctx.mpv.duration = 400.0;
        ctx.mpv.position = 1.0;
        ctx.mpv.volume = Some(50);
        // a resolved stream so the download button shows
        use crate::shared::ytdlp::YtStreamInfo;
        ctx.yt_info.borrow_mut().insert(
            "https://www.youtube.com/watch?v=abc".to_owned(),
            YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                original_url: "https://www.youtube.com/watch?v=abc".to_owned(),
                title: "Shaders ARE Changing FOREVER".to_owned(),
                channel: Some("GOATED".to_owned()),
                ..Default::default()
            },
        );
        ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
            "Shaders ARE Changing FOREVER",
            "https://www.youtube.com/watch?v=abc",
            None,
        ));
        let lines = controls_lines(&ctx, 98);
        // row 0: channel left, title centered, buttons right
        assert!(lines[0].starts_with("GOATED"), "{:?}", lines[0]);
        assert!(lines[0].contains("Shaders ARE Changing FOREVER"), "{:?}", lines[0]);
        assert!(lines[0].trim_end().ends_with("⤓ [Audio] [Sub]"), "{:?}", lines[0]);
        let title_x = lines[0].find("Shaders").unwrap();
        let buttons_x = lines[0].find("⤓").unwrap();
        assert!(title_x > 8 && title_x < buttons_x, "{:?}", lines[0]);
        // separator + transport rows
        assert!(lines[1].starts_with("─"), "{:?}", lines[1]);
        assert!(lines[2].contains("|   ◀◀"), "{:?}", lines[2]);
        assert!(lines[2].contains("0:01 / 6:40"), "{:?}", lines[2]);
        assert!(lines[2].contains("50%"), "{:?}", lines[2]);
    }

    /// A long mpv-video title overflows its region and marquees *inside*
    /// it — the right-aligned buttons (⤓ / [Audio] / [Sub]) stay visible,
    /// never overwritten. Regression: the title region must end at the
    /// cluster's left edge (the ⤓ when a stream is resolved), not at the
    /// rightmost [Sub].
    #[rstest::rstest]
    fn mpv_long_title_never_overwrites_the_buttons(ctx: Ctx) {
        let mut ctx = playing_ctx(ctx);
        ctx.mpv.active = true;
        ctx.mpv.paused = false;
        ctx.mpv.title =
            "THE VERY LONG EPISODE TITLE THAT KEEPS GOING AND GOING AND GOING PAST THE BUTTONS"
                .into();
        ctx.mpv.artist = "GOATED".into();
        ctx.mpv.duration = 400.0;
        ctx.mpv.position = 1.0; // within the 2 s marquee hold: offset 0
        ctx.mpv.volume = Some(50);
        // A resolved ytdlp stream -> the download button shows.
        use crate::shared::ytdlp::YtStreamInfo;
        ctx.yt_info.borrow_mut().insert(
            "https://www.youtube.com/watch?v=abc".to_owned(),
            YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                original_url: "https://www.youtube.com/watch?v=abc".to_owned(),
                title: "x".to_owned(),
                channel: Some("GOATED".to_owned()),
                ..Default::default()
            },
        );
        ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
            "x",
            "https://www.youtube.com/watch?v=abc",
            None,
        ));
        let lines = controls_lines(&ctx, 98);
        let row = &lines[0];
        let d = row.find("⤓").expect("the download button must stay visible");
        // Nothing from the title may render at or right of the button: that
        // stretch is exactly the button cluster.
        assert_eq!(row[d..].trim_end(), "⤓ [Audio] [Sub]", "title overwrote the buttons: {row:?}");
        // And the channel still leads the row.
        assert!(row.starts_with("GOATED"), "{row:?}");
    }
}
