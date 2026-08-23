use anyhow::Result;
use ratatui::{
    Frame, prelude::{Buffer, Rect},
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
            (Some(artist), Some(title)) => {
                Line::from(
                    vec![
                        Span::styled(artist, artist_style), Span::styled(" - ",
                        separator_style), Span::styled(title, title_style),
                    ],
                )
            }
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
    fn draw_volume(
        buf: &mut Buffer,
        area: Rect,
        ctx: &Ctx,
        start: u16,
        slider_w: u16,
    ) -> u16 {
        let theme = ControlsTheme::from_ctx(ctx);
        let y = area.y + 2;
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
        let elapsed_ms = if crate::core::mpv::mpv_is_ui_source(ctx) {
            (ctx.mpv.position * 1000.0) as u64
        } else {
            ctx.song_played.unwrap_or(ctx.status.elapsed).as_millis() as u64
        };
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
        let mouse = ctx.mouse_pos();
        let show_modes = area.width >= 42;
        let (mode_start, mpv_buttons) = if crate::core::mpv::mpv_is_ui_source(ctx) {
            let buttons = Self::mpv_button_layout(area, ctx);
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
        let separator_style = ctx.config.as_border_style();
        for x in area.x..area.right() {
            buf.set_string(x, y1, "─", separator_style);
        }
        let (transport_start, _) = Self::transport_zones(area);
        let transport_end = transport_start + TRANSPORT_CLUSTER_W;
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
        x
            += Self::put(
                buf,
                x,
                y2,
                "◀◀   ",
                hover_zone(transport_start + 4, transport_start + 9, theme.transport),
            );
        let play_w = play_pause_label.width();
        x
            += Self::put(
                buf,
                x,
                y2,
                play_pause_label,
                hover_zone(transport_start + 9, transport_start + 13, theme.transport),
            );
        if play_w < 4 {
            x += Self::put(buf, x, y2, " ".repeat(4 - play_w).as_str(), theme.transport);
        }
        x
            += Self::put(
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
                } else if y == self.area.y + 2 {
                    let (_, zones) = Self::transport_zones(self.area);
                    for (btn, z0, z1) in zones {
                        if x >= z0 && x < z1 {
                            return self.do_transport(btn, ctx);
                        }
                    }
                    let (transport_start, _) = Self::transport_zones(self.area);
                    let (volume_start, volume_w) = volume_geometry(
                        self.area,
                        transport_start + TRANSPORT_CLUSTER_W,
                    );
                    if x >= volume_start && x < volume_start + volume_w {
                        return Self::set_volume(
                            ctx,
                            x,
                            self.area,
                            volume_start,
                            volume_w,
                        );
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
                let (volume_start, volume_w) = volume_geometry(
                    self.area,
                    transport_start + TRANSPORT_CLUSTER_W,
                );
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
