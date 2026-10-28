use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::{
    appearance::color_hex,
    model::{
        AdvancedRow, Click, ContentRow, DeferredButton, GeneralRow, JellyfinRow, MpvRow, MpdRow,
        TorrentRow,
    },
    SettingsModal,
};
use crate::{
    ctx::Ctx,
    mpd::commands::{replay_gain::ReplayGain, status::OnOffOneshot},
    ui::modals::remap_keys,
};

/// Truncate `s` to at most `width` columns, appending an ellipsis when cut.
pub(super) fn truncate_col(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

impl SettingsModal {
    pub(super) fn styles(&self, ctx: &Ctx) -> (Style, Style, Style) {
        let base = ctx
            .config
            .theme
            .text_color
            .map_or_else(Style::default, |c| Style::default().fg(c));
        let dim = base.add_modifier(Modifier::DIM);
        let active = ctx.config.theme.current_item_style;
        (base, dim, active)
    }

    /// The row-wide click target covering the whole row.
    fn row_click(
        targets: &mut Vec<(Rect, Click)>,
        x: u16,
        y: u16,
        width: u16,
        click: Click,
    ) {
        targets.push((Rect { x, y, width, height: 1 }, click));
    }

    /// Render a row's label + control parts. The control is right-aligned
    /// by the caller; buttons inside it are deferred (their click rects are
    /// placed once the control's x is known). The fourth element is the
    /// row-wide click action (covering the whole row, including the label).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn row_main(
        &self,
        row: &ContentRow,
        ctx: &Ctx,
        style: Style,
        dim: Style,
        avail: usize,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<DeferredButton>, Option<Click>) {
        match row {
            ContentRow::Mpv(m) => match m {
                MpvRow::Header => (
                    vec![Span::styled("[ mpv ]", dim)],
                    Vec::new(),
                    Vec::new(),
                    None,
                ),
                MpvRow::AudioLang => {
                    let active_label = self.mpv_audio_pending.label();
                    (
                        vec![Span::styled(" audio", style)],
                        vec![
                            Span::styled(format!(" {active_label} "), style.bold()),
                            Span::styled(" > ", dim),
                            Span::styled(" original ", dim),
                        ],
                        Vec::new(),
                        Some(Click::Toggle),
                    )
                }
                MpvRow::Subtitles => {
                    let second = self.mpv_subtitles_pending.label();
                    (
                        vec![Span::styled(" subtitles", style)],
                        vec![
                            Span::styled(" signs ", dim),
                            Span::styled(" > ", dim),
                            Span::styled(format!(" {second} "), style.bold()),
                        ],
                        Vec::new(),
                        Some(Click::Toggle),
                    )
                }
                MpvRow::Svp => Self::toggle_row("svp support", self.mpv_svp_pending, style),
                MpvRow::EditConf => (
                    vec![Span::styled(" edit mpv.conf", style)],
                    vec![
                        Span::styled(" [ open editor ]", style.bold()),
                        Span::styled(" >", dim),
                    ],
                    Vec::new(),
                    Some(Click::Activate),
                ),
            },
            ContentRow::General(g) => match g {
                GeneralRow::AlbumArt
                | GeneralRow::Lyrics
                | GeneralRow::Cava
                | GeneralRow::Jellyfin
                | GeneralRow::AutoChapters
                | GeneralRow::Mpdis2Notifications
                | GeneralRow::AutoSens
                | GeneralRow::VirtualDevices
                | GeneralRow::Monstercat
                | GeneralRow::Waves => {
                    let (label, enabled) = match g {
                        GeneralRow::AlbumArt => ("show album art", self.ui_pending.show_album_art),
                        GeneralRow::Lyrics => ("show lyrics", self.ui_pending.show_lyrics),
                        GeneralRow::Cava => ("show cava", self.ui_pending.show_cava),
                        GeneralRow::Jellyfin => {
                            ("show jellyfin tab", self.ui_pending.show_jellyfin_tab)
                        }
                        GeneralRow::AutoChapters => (
                            "If media contains chapters open to chapters list",
                            self.ui_pending.auto_show_chapters,
                        ),
                        GeneralRow::Mpdis2Notifications => (
                            "mpdris2 desktop notifications",
                            self.ui_pending.mpdris2_notifications,
                        ),
                        GeneralRow::AutoSens => ("auto-sens", self.cava_pending.autosens),
                        GeneralRow::VirtualDevices => {
                            ("show virtual devices", self.ui_pending.show_virtual_devices)
                        }
                        GeneralRow::Monstercat => ("monstercat smoothing", self.monstercat()),
                        GeneralRow::Waves => ("waves smoothing", self.waves()),
                        _ => unreachable!(),
                    };
                    Self::toggle_row(label, enabled, style)
                }
                GeneralRow::RadioReload => {
                    let control = vec![Span::styled("[Reload]", style.bold())];
                    let buttons = vec![DeferredButton {
                        offset: 0,
                        label: "[Reload]",
                        click: Click::Activate,
                    }];
                    (
                        vec![Span::styled(" reload radio stations", style)],
                        control,
                        buttons,
                        Some(Click::Activate),
                    )
                }
                GeneralRow::VideoPlayback => {
                    let current = self.video_pending;
                    let mut control = Vec::new();
                    for (i, mode) in crate::config::video::VideoPlaybackMode::ALL
                        .iter()
                        .enumerate()
                    {
                        if i > 0 {
                            control.push(Span::styled("|", dim));
                        }
                        let active = *mode == current;
                        control.push(Span::styled(
                            format!(" {} ", mode.as_str()),
                            if active { style.bold() } else { dim },
                        ));
                    }
                    (
                        vec![Span::styled(" Jellyfin media playback preference", style)],
                        control,
                        Vec::new(),
                        Some(Click::Toggle),
                    )
                }
                GeneralRow::Sensitivity => self.stepper_row(
                    "sensitivity",
                    self.sensitivity().to_string(),
                    style,
                ),
                GeneralRow::Fps => {
                    self.stepper_row("frame rate 15-120", self.fps().to_string(), style)
                }
                GeneralRow::FreqMin => self.stepper_row(
                    "min sampling frequency",
                    self.freq_min().to_string(),
                    style,
                ),
                GeneralRow::FreqMax => self.stepper_row(
                    "max sampling frequency",
                    self.freq_max().to_string(),
                    style,
                ),
                GeneralRow::Channels => {
                    let current = self.channels();
                    Self::option_row(
                        "channels",
                        style,
                        dim,
                        &[("1", current == 1), ("2", current == 2)],
                    )
                }
                GeneralRow::Device => {
                    let source = self.source();
                    let description = self
                        .nodes
                        .iter()
                        .find(|n| n.name == source)
                        .map(|n| n.description.clone())
                        .unwrap_or_else(|| {
                            if source == "auto" {
                                "auto (default output)".to_string()
                            } else {
                                source
                            }
                        });
                    let max_desc =
                        avail.saturating_sub(1 + "device".len() + 2 + "[<] [>]".len() + 2);
                    let description = truncate_col(&description, max_desc);
                    let d = description.chars().count();
                    (
                        vec![Span::styled(" device", style)],
                        vec![
                            Span::styled(description, style.bold()),
                            Span::raw("  "),
                            Span::styled("[<]", style.bold()),
                            Span::raw(" "),
                            Span::styled("[>]", style.bold()),
                        ],
                        vec![
                            DeferredButton {
                                offset: d + 2,
                                label: "[<]",
                                click: Click::Prev,
                            },
                            DeferredButton {
                                offset: d + 6,
                                label: "[>]",
                                click: Click::Next,
                            },
                        ],
                        Some(Click::Next),
                    )
                }
                GeneralRow::NoiseReduction => self.stepper_row(
                    "noise reduction",
                    self.noise_reduction().to_string(),
                    style,
                ),
                _ => (Vec::new(), Vec::new(), Vec::new(), None),
            },
            ContentRow::Appearance(_) => (Vec::new(), Vec::new(), Vec::new(), None),
            ContentRow::AutoTheme => {
                let current = self.auto_theme_pending;
                let options: Vec<(&str, bool)> =
                    crate::config::auto_theme::AutoThemeSource::ALL
                        .iter()
                        .map(|source| (source.as_str(), *source == current))
                        .collect();
                // The label names the source currently winning the resolve,
                // so "why is my accent this color" is answerable at a glance.
                let label = match ctx.accent.applied() {
                    Some(accent) => format!(" auto theme ({})", accent.source.as_str()),
                    None => " auto theme (desktop accent)".to_owned(),
                };
                Self::option_row(&label, style, dim, &options)
            }
            ContentRow::Mpd(m) => match m {
                MpdRow::LibraryPath => {
                    let path = if self.library_path.is_empty() {
                        "(whole library)".to_string()
                    } else {
                        self.library_path.clone()
                    };
                    let max_path = avail
                        .saturating_sub(1 + "library location".len() + 2 + "[edit]".len() + 2);
                    let path = truncate_col(&path, max_path);
                    let p = path.chars().count();
                    (
                        vec![Span::styled(" library location", style)],
                        vec![
                            Span::styled(path, style.bold()),
                            Span::raw("  "),
                            Span::styled("[edit]", style.bold()),
                        ],
                        vec![DeferredButton {
                            offset: p + 2,
                            label: "[edit]",
                            click: Click::Edit,
                        }],
                        Some(Click::Edit),
                    )
                }
                MpdRow::LibraryPlaylists => Self::toggle_row(
                    "show .m3u/.pls/.xspf playlists from the music library",
                    self.ui_pending.library_playlist_files,
                    style,
                ),
                MpdRow::Update => {
                    Self::action_row("update library", "scan for new / changed files", style, dim)
                }
                MpdRow::Rescan => {
                    Self::action_row("rescan library", "force re-read of all files", style, dim)
                }
                MpdRow::Outputs => {
                    Self::action_row("outputs", "list / toggle MPD outputs", style, dim)
                }
                MpdRow::Crossfade => self.stepper_row(
                    "crossfade",
                    ctx.status.xfade.unwrap_or(0).to_string(),
                    style,
                ),
                MpdRow::Repeat | MpdRow::Random | MpdRow::Single | MpdRow::Consume => {
                    let (label, enabled) = match m {
                        MpdRow::Repeat => ("repeat", ctx.status.repeat),
                        MpdRow::Random => ("random", ctx.status.random),
                        MpdRow::Single => {
                            ("single", !matches!(ctx.status.single, OnOffOneshot::Off))
                        }
                        MpdRow::Consume => {
                            ("consume", !matches!(ctx.status.consume, OnOffOneshot::Off))
                        }
                        _ => unreachable!(),
                    };
                    Self::toggle_row(label, enabled, style)
                }
                MpdRow::ReplayGain => {
                    let current = self.replay_gain_pending;
                    Self::option_row(
                        "replay gain",
                        style,
                        dim,
                        &[
                            ("off", current == ReplayGain::Off),
                            ("track", current == ReplayGain::Track),
                            ("album", current == ReplayGain::Album),
                        ],
                    )
                }
                _ => (Vec::new(), Vec::new(), Vec::new(), None),
            },
            ContentRow::Jellyfin(m) => match m {
                JellyfinRow::ServerUrl | JellyfinRow::Username | JellyfinRow::Password => {
                    let (label, value) = match m {
                        JellyfinRow::ServerUrl => ("server url", self.jellyfin_url.clone()),
                        JellyfinRow::Username => ("username", self.jellyfin_username.clone()),
                        JellyfinRow::Password => (
                            "password",
                            "•".repeat(self.jellyfin_password.chars().count().min(8)),
                        ),
                        _ => unreachable!(),
                    };
                    let display = if self.editing_jellyfin_row == Some(*m) {
                        format!("{}_", self.edit_buffer)
                    } else if value.is_empty() {
                        "(not set)".to_string()
                    } else {
                        value
                    };
                    let max_value =
                        avail.saturating_sub(1 + label.len() + 2 + "[edit]".len() + 2);
                    let display = truncate_col(&display, max_value);
                    let d = display.chars().count();
                    (
                        vec![Span::styled(format!(" {label}"), style)],
                        vec![
                            Span::styled(display, style.bold()),
                            Span::raw("  "),
                            Span::styled("[edit]", style.bold()),
                        ],
                        vec![DeferredButton {
                            offset: d + 2,
                            label: "[edit]",
                            click: Click::Edit,
                        }],
                        Some(Click::Edit),
                    )
                }
                JellyfinRow::SignIn => {
                    let signed_in = self.jellyfin_credentials.is_some();
                    let label = if signed_in { "sign in ✓" } else { "sign in" };
                    (
                        vec![Span::styled(
                            format!(" {label}"),
                            if signed_in { style.bold() } else { style },
                        )],
                        vec![Span::styled("fetch a session token for the app", dim)],
                        Vec::new(),
                        Some(Click::Activate),
                    )
                }
                _ => (Vec::new(), Vec::new(), Vec::new(), None),
            },
            ContentRow::Torrent(m) => match m {
                TorrentRow::StreamDownloads => Self::toggle_row(
                    "stream also downloads",
                    self.torrent_stream_downloads_pending,
                    style,
                ),
                TorrentRow::WebUi => {
                    let running = self.torrent_webui_running(ctx);
                    let desc = if running {
                        "engine running — open the browser"
                    } else {
                        "start the engine + open the browser"
                    };
                    let button = if running { "[open]" } else { "[start]" };
                    let d = desc.chars().count();
                    (
                        vec![Span::styled(" web ui", style)],
                        vec![
                            Span::styled(desc, dim),
                            Span::raw("  "),
                            Span::styled(button, style.bold()),
                        ],
                        vec![DeferredButton {
                            offset: d + 2,
                            label: button,
                            click: Click::Activate,
                        }],
                        Some(Click::Activate),
                    )
                }
                TorrentRow::StopEngine => {
                    let running = self.torrent_webui_running(ctx);
                    let desc = if running {
                        "kill the standalone engine"
                    } else {
                        "no engine running"
                    };
                    let d = desc.chars().count();
                    (
                        vec![Span::styled(" stop engine", style)],
                        vec![
                            Span::styled(desc, dim),
                            Span::raw("  "),
                            Span::styled("[stop]", style.bold()),
                        ],
                        vec![DeferredButton {
                            offset: d + 2,
                            label: "[stop]",
                            click: Click::Activate,
                        }],
                        Some(Click::Activate),
                    )
                }
                TorrentRow::SocksProxy => {
                    let value = self.torrent_socks_proxy_pending.clone();
                    let display = if value.is_empty() {
                        "(not set)".to_string()
                    } else {
                        value
                    };
                    let max_value =
                        avail.saturating_sub(1 + "socks proxy".len() + 2 + "[edit]".len() + 2);
                    let display = truncate_col(&display, max_value);
                    let d = display.chars().count();
                    (
                        vec![Span::styled(" socks proxy", style)],
                        vec![
                            Span::styled(display, style.bold()),
                            Span::raw("  "),
                            Span::styled("[edit]", style.bold()),
                        ],
                        vec![DeferredButton {
                            offset: d + 2,
                            label: "[edit]",
                            click: Click::Edit,
                        }],
                        Some(Click::Edit),
                    )
                }
                _ => (Vec::new(), Vec::new(), Vec::new(), None),
            },
            ContentRow::Advanced(m) => match m {
                AdvancedRow::MemoryWatchdog => {
                    Self::toggle_row("memory watchdog", self.ui_pending.memory_watchdog, style)
                }
                AdvancedRow::MemoryWatchdogInterval => {
                    // Round 111: render through the shared stepper row. The
                    // hand-rolled version registered no `DeferredButton`s, so
                    // `<`/`>` had no click targets at all and the row-wide
                    // click was a bare `Inc` — the reported "the controls
                    // don't function, the value only increases".
                    self.stepper_row(
                        "how often it samples",
                        format!("{}s", self.ui_pending.memory_watchdog_interval_secs),
                        style,
                    )
                }
                AdvancedRow::DiagnosticsHeader => (Vec::new(), Vec::new(), Vec::new(), None),
            },
            _ => (Vec::new(), Vec::new(), Vec::new(), None),
        }
    }

    /// A `[x]` / `[ ]` toggle row: label left, checkbox right-aligned.
    fn toggle_row(
        label: &'static str,
        enabled: bool,
        style: Style,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<DeferredButton>, Option<Click>) {
        (
            vec![Span::styled(format!(" {label}"), style)],
            vec![Span::styled(
                format!("[{}]", if enabled { "x" } else { " " }),
                style.bold(),
            )],
            Vec::new(),
            Some(Click::Toggle),
        )
    }

    /// A `a | b | c` choice row: the current option is bold, the others dim.
    fn option_row(
        label: &str,
        style: Style,
        dim: Style,
        options: &[(&str, bool)],
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<DeferredButton>, Option<Click>) {
        let mut control = Vec::new();
        for (i, (text, active)) in options.iter().enumerate() {
            if i > 0 {
                control.push(Span::styled("|", dim));
            }
            control.push(Span::styled(
                format!(" {text} "),
                if *active { style.bold() } else { dim },
            ));
        }
        (
            vec![Span::styled(format!(" {label}"), style)],
            control,
            Vec::new(),
            Some(Click::Toggle),
        )
    }

    /// A stepper row: label left, `value [-] [+]` right-aligned. The
    /// buttons are deferred so the row-wide click (Inc) can't shadow them.
    fn stepper_row(
        &self,
        label: &str,
        value: String,
        style: Style,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<DeferredButton>, Option<Click>) {
        let vw = value.chars().count();
        let ctl = if self.adjusting == Some(self.selected) {
            style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            style.add_modifier(Modifier::BOLD)
        };
        let control = vec![
            Span::styled(value, ctl),
            Span::raw(" "),
            Span::styled("[-]", ctl),
            Span::raw(" "),
            Span::styled("[+]", ctl),
        ];
        let buttons = vec![
            DeferredButton {
                offset: vw + 1,
                label: "[-]",
                click: Click::Dec,
            },
            DeferredButton {
                offset: vw + 4,
                label: "[+]",
                click: Click::Inc,
            },
        ];
        (
            vec![Span::styled(format!(" {label}"), style)],
            control,
            buttons,
            Some(Click::Inc),
        )
    }

    /// A plain action row: label left, a dim description right-aligned.
    fn action_row(
        label: &'static str,
        hint: &'static str,
        style: Style,
        dim: Style,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<DeferredButton>, Option<Click>) {
        (
            vec![Span::styled(format!(" {label}"), style)],
            vec![Span::styled(hint, dim)],
            Vec::new(),
            Some(Click::Activate),
        )
    }

    /// The description shown right-aligned on a section header row ("" =
    /// none).
    fn section_hint(&self, row: &ContentRow) -> String {
        match row {
            ContentRow::General(GeneralRow::FeaturesHeader) => {
                "enable or disable features globally".into()
            }
            ContentRow::General(GeneralRow::CavaHeader) => "cava visualization settings".into(),
            ContentRow::General(GeneralRow::AppearanceHeader) => String::new(),
            ContentRow::Mpv(MpvRow::Header) => "mpv video playback".into(),
            ContentRow::Mpd(MpdRow::LibraryHeader) => "MPD database".into(),
            ContentRow::Mpd(MpdRow::PlaybackHeader) => "transport modes".into(),
            ContentRow::Mpd(MpdRow::DevicesHeader) => "outputs & sessions".into(),
            ContentRow::Jellyfin(JellyfinRow::Header) => "Jellyfin server".into(),
            ContentRow::Torrent(TorrentRow::Header) => "rqbit torrent engine".into(),
            ContentRow::Advanced(AdvancedRow::DiagnosticsHeader) => "diagnostics".into(),
            _ => String::new(),
        }
    }

    pub(super) fn render_row(
        &self,
        row: &ContentRow,
        selected: bool,
        hovered: bool,
        ctx: &Ctx,
        base: Style,
        dim: Style,
        active: Style,
        x: u16,
        y: u16,
        width: u16,
        targets: &mut Vec<(Rect, Click)>,
    ) -> Line<'static> {
        let style = if selected { active } else { base };
        let hovered = hovered && Self::is_clickable_row(row);
        let hover = |mut line: Line<'static>| -> Line<'static> {
            if hovered {
                crate::config::hover_line(&mut line);
            }
            line
        };
        match row {
            ContentRow::KeyTableHeader => {
                let d = self.desc_col_w.max(10);
                let a = self.action_col_w.max(6);
                let k = self.key_col_w.max(4);
                Line::from(vec![
                    Span::styled(format!(" {:<d$}", "Description"), dim),
                    Span::styled(format!("{:<a$}", "Action"), dim),
                    Span::styled(format!("{:<k$}", "Key"), dim),
                ])
            }
            ContentRow::KeyHeader(section) => {
                Line::from(vec![Span::styled(format!(" {}", section.name()), dim)])
            }
            ContentRow::KeyItem(item) => {
                Self::row_click(targets, x, y, width, Click::Activate);
                let d = self.desc_col_w;
                let a = self.action_col_w;
                let k = self.key_col_w;
                let desc = truncate_col(
                    &remap_keys::remap_description(&item.action),
                    d.max(10),
                );
                let action = truncate_col(&item.display, a.max(6));
                let keys = truncate_col(&remap_keys::key_display(&item.keys), k.max(4));
                hover(Line::from(vec![
                    Span::styled(format!(" {desc:<d$}"), active),
                    Span::styled(format!("{action:<a$}"), style),
                    Span::styled(keys, dim),
                ]))
            }
            ContentRow::General(
                GeneralRow::FeaturesHeader
                | GeneralRow::CavaHeader
                | GeneralRow::AppearanceHeader,
            )
            | ContentRow::Mpv(MpvRow::Header)
            | ContentRow::Mpd(
                MpdRow::LibraryHeader | MpdRow::PlaybackHeader | MpdRow::DevicesHeader,
            )
            | ContentRow::Jellyfin(JellyfinRow::Header)
            | ContentRow::Torrent(TorrentRow::Header) => {
                let title = match row {
                    ContentRow::General(GeneralRow::FeaturesHeader) => "[ features ]",
                    ContentRow::General(GeneralRow::CavaHeader) => "[ cava ]",
                    ContentRow::General(GeneralRow::AppearanceHeader) => "[ appearance ]",
                    ContentRow::Mpv(MpvRow::Header) => "[ mpv ]",
                    ContentRow::Mpd(MpdRow::LibraryHeader) => "[ library ]",
                    ContentRow::Mpd(MpdRow::PlaybackHeader) => "[ playback ]",
                    ContentRow::Mpd(MpdRow::DevicesHeader) => "[ devices ]",
                    ContentRow::Jellyfin(JellyfinRow::Header) => "[ server ]",
                    ContentRow::Torrent(TorrentRow::Header) => "[ rqbit ]",
                    _ => unreachable!(),
                };
                let hint = self.section_hint(row);
                let available = (width as usize).saturating_sub(2);
                let hint_keep = available.saturating_sub(title.len()).saturating_sub(1);
                let hint = truncate_col(&hint, hint_keep);
                let pad = available
                    .saturating_sub(title.len())
                    .saturating_sub(hint.len());
                let mut spans = vec![Span::styled(title, dim)];
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                if !hint.is_empty() {
                    spans.push(Span::styled(hint, dim));
                }
                Line::from(spans)
            }
            ContentRow::Appearance(target) => {
                Self::row_click(targets, x, y, width, Click::Toggle);
                let color = self.displayed_color(ctx, *target);
                let is_editing = self.editing_color == Some(*target);
                let field = if is_editing {
                    format!("#{}_", self.edit_buffer)
                } else {
                    color
                        .as_ref()
                        .map_or_else(|| "transparent".to_string(), color_hex)
                };
                let swatch = match color {
                    Some(color) => Span::styled("  ", Style::default().bg(color)),
                    None => Span::styled("▢▢", dim),
                };
                let field_s = format!("[{field}]");
                let label_w = 1 + target.name().len() + 2 + 2;
                let pad = (width as usize)
                    .saturating_sub(2)
                    .saturating_sub(label_w)
                    .saturating_sub(field_s.chars().count());
                let mut spans = vec![
                    Span::styled(format!(" {}", target.name()), style),
                    Span::raw("  "),
                    swatch,
                ];
                if pad > 0 {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                spans.push(Span::styled(field_s, style.bold()));
                hover(Line::from(spans))
            }
            // The diagnostics header is plain text; the two rows below it go
            // through the generic label/control path like every other
            // section's rows.
            ContentRow::Advanced(AdvancedRow::DiagnosticsHeader) => {
                Line::from(vec![Span::styled(" diagnostics", dim)])
            }
            ContentRow::AutoTheme
            | ContentRow::General(_)
            | ContentRow::Mpd(_)
            | ContentRow::Mpv(_)
            | ContentRow::Jellyfin(_)
            | ContentRow::Torrent(_)
            | ContentRow::Advanced(_) => {
                let (label, control, buttons, row_action) =
                    self.row_main(row, ctx, style, dim, width as usize);
                if let Some(click) = row_action {
                    Self::row_click(targets, x, y, width, click);
                }
                let control_w: usize = control.iter().map(|s| s.width()).sum();
                let available = (width as usize).saturating_sub(2);
                let (label, label_w, pad) = {
                    let label_w: usize = label.iter().map(|s| s.width()).sum();
                    let pad = available.saturating_sub(label_w + control_w);
                    if pad > 0 {
                        (label, label_w, pad)
                    } else {
                        let keep = available.saturating_sub(control_w).saturating_sub(1);
                        let mut shrunk = Vec::new();
                        if let Some(span) = label.into_iter().next() {
                            let text = span.content.to_string();
                            shrunk.push(Span::styled(truncate_col(&text, keep), span.style));
                        }
                        let shrunk_w: usize = shrunk.iter().map(|s| s.width()).sum();
                        (shrunk, shrunk_w, 1)
                    }
                };
                let control_x = x + label_w as u16 + pad as u16;
                for b in buttons {
                    targets.push((
                        Rect {
                            x: control_x + b.offset as u16,
                            y,
                            width: b.label.chars().count() as u16,
                            height: 1,
                        },
                        b.click,
                    ));
                }
                let mut spans = label;
                spans.push(Span::raw(" ".repeat(pad)));
                spans.extend(control);
                if row_action.is_some() {
                    hover(Line::from(spans))
                } else {
                    Line::from(spans)
                }
            }
        }
    }

    /// Whether the standalone web-UI engine is running: the GUI's
    /// in-memory engine OR an engine the `s2udio rq` CLI registered (the
    /// two share one registration file).
    pub(super) fn torrent_webui_running(&self, ctx: &Ctx) -> bool {
        ctx.torrent_webui_engine
            .borrow_mut()
            .as_mut()
            .map(|e| e.is_running())
            .unwrap_or(false) || crate::core::rqctl::registered_running().is_some()
    }

    /// Whether the row registers a row-wide click (only those lighten on
    /// hover).
    fn is_clickable_row(row: &ContentRow) -> bool {
        matches!(
            row,
            ContentRow::KeyItem(_)
                | ContentRow::Appearance(_)
                | ContentRow::AutoTheme
                | ContentRow::General(_)
                | ContentRow::Mpd(_)
                | ContentRow::Mpv(_)
                | ContentRow::Jellyfin(_)
                | ContentRow::Torrent(_)
        )
    }

    pub(super) fn render_scrollbar(
        &self,
        frame: &mut Frame,
        content_area: Rect,
        visible: usize,
        dim: Style,
    ) {
        if self.rows.len() > visible {
            let max_scroll = self.rows.len().saturating_sub(visible);
            let thumb_h = ((visible * visible) / self.rows.len()).max(1) as u16;
            let travel = (visible as u16).saturating_sub(thumb_h);
            let thumb_y = content_area.y
                + travel * self.scroll as u16 / max_scroll.max(1) as u16;
            let buf = frame.buffer_mut();
            for y in content_area.y..content_area.bottom() {
                let c = if y >= thumb_y && y < thumb_y + thumb_h {
                    "█"
                } else {
                    "│"
                };
                buf.set_string(content_area.right() - 1, y, c, dim);
            }
        }
    }
}
