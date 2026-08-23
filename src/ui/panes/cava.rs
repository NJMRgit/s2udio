use std::{
    io::{Read, Write},
    process::{Child, Stdio},
    thread::JoinHandle, time::Duration,
};
use anyhow::{Context, Result, anyhow, bail};
use crossbeam::channel::{Receiver, RecvError, Sender, TryRecvError};
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    queue, style::{PrintStyledContent, Stylize},
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use ratatui::{
    Frame, layout::Rect, prelude::FromCrossterm, style::{Color, Style},
    widgets::Block,
};
/// Maximum number of cava bars; beyond this the bar width grows instead of
/// adding more bars, so a wider window makes thicker bars (and stops
/// restarting cava on every resize once the cap is reached).
const MAX_CAVA_BARS: u16 = 64;
use super::Pane;
use crate::{
    config::{cava::Cava, theme::cava::{CavaTheme, Orientation}},
    ctx::Ctx, mpd::commands::State,
    shared::{dependencies::CAVA, keys::ActionEvent, terminal::{TERMINAL, TtyWriter}},
    status_warn, try_skip, ui::{UiEvent, image::clear_area},
};
#[derive(Debug)]
pub struct CavaPane {
    area: Rect,
    /// Geometry the cava thread is currently rendering at (sent via Start).
    sent_area: Rect,
    handle: Option<JoinHandle<Result<()>>>,
    command_channel: (Sender<CavaCommand>, Receiver<CavaCommand>),
    is_modal_open: bool,
    /// A Start is deferred until the first frame has been drawn: sending it
    /// from before_show makes the bars paint *before* the UI flush, so the
    /// visualizer appears first and looks unpolished (and the flush can
    /// briefly wipe it). Cleared by `maybe_start` (after the first frame)
    /// and by every real `run()`.
    pending_start: bool,
    /// Lyrics edit mode is on: the visualizer is paused and the pane shows
    /// the edit-controls legend instead of the bars (round 35).
    legend_shown: bool,
}
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum CavaCommand {
    Start { area: Rect },
    Stop,
    Pause,
    ConfigChanged { config: Cava, theme: CavaTheme },
}
struct ProcessGuard {
    handle: Child,
}
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Err(e) = self.handle.kill() {
            log::error!("Failed to kill cava process: {e}");
            return;
        }
        if let Err(e) = self.handle.wait() {
            log::error!("Failed to wait for cava process to die: {e}");
        }
    }
}
impl CavaPane {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            area: Rect::default(),
            sent_area: Rect::default(),
            handle: None,
            is_modal_open: false,
            pending_start: false,
            legend_shown: false,
            command_channel: crossbeam::channel::bounded(0),
        }
    }
    /// Start drawing the visualizer: cava reads bars and renders them into
    /// `self.area`. No-op while a modal is open — the bars are a
    /// terminal-side overlay and would paint over the modal's full-window
    /// view (Settings, paste popup, …).
    pub fn run(&mut self, ctx: &Ctx) -> Result<()> {
        if self.is_modal_open {
            log::debug!("Cava run suppressed: a modal is open");
            return Ok(());
        }
        if ctx.lyrics_edit_mode.get() {
            log::debug!("Cava run suppressed: lyrics edit mode");
            return Ok(());
        }
        self.pending_start = false;
        self.clear(ctx)?;
        self.command(CavaCommand::Start {
            area: self.area,
        })?;
        self.sent_area = self.area;
        Ok(())
    }
    /// Deferred start: called by the UI right after the first frame of a
    /// show (startup, tab switch, resize-settled) has been flushed, so the
    /// bars always paint over a complete UI instead of racing it. If the
    /// player is not playing (or a modal is open) the pending start is
    /// dropped — the event handlers that fire then will start the bars.
    pub fn maybe_start(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.pending_start {
            return Ok(());
        }
        if self.is_modal_open || !matches!(ctx.status.state, State::Play) {
            log::debug!("Cava deferred start dropped (modal open or not playing)");
            self.pending_start = false;
            return Ok(());
        }
        self.run(ctx)
    }
    /// Reinitialize the cava thread when its geometry changed (resize,
    /// layout change), so the bars always match the pane. Skipped while a
    /// Start is pending (the deferred `maybe_start` sends the real area
    /// after the frame is flushed).
    fn reinit_if_area_changed(&mut self, ctx: &Ctx) {
        if !self.pending_start && self.area != self.sent_area && !self.is_modal_open
            && matches!(ctx.status.state, State::Play) && !ctx.resizing.get()
            && !ctx.lyrics_edit_mode.get()
        {
            self.sent_area = self.area;
            self.command(CavaCommand::Start {
                    area: self.area,
                })
                .ok();
        }
    }
    /// The lyrics edit-mode controls legend, drawn over the cava pane
    /// while edit mode is on (round 35): two columns of `key  action`,
    /// the keys in the highlight style, the actions in the text style.
    /// Narrow panes fall back to a single column; short panes clip.
    fn render_legend(&self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        use unicode_width::UnicodeWidthStr;
        let buf = frame.buffer_mut();
        let text_style = ctx.config.as_text_style();
        let key_style = ctx.config.theme.highlighted_item_style;
        let width = area.width as usize;
        let rows = area.height;
        if rows == 0 || width == 0 {
            return;
        }
        let cell = |
            buf: &mut ratatui::buffer::Buffer,
            x: u16,
            y: u16,
            max_w: usize,
            key: &str,
            desc: &str|
        {
            buf.set_stringn(x, y, key, max_w, key_style);
            let dx = x + key.width() as u16 + 1;
            if dx < area.x + area.width {
                buf.set_stringn(
                    dx,
                    y,
                    desc,
                    (area.x + area.width - dx) as usize,
                    text_style,
                );
            }
        };
        buf.set_stringn(area.x, area.y, "Lyrics edit mode", width, key_style);
        let body_rows = [
            ("\u{2190} \u{2192}", "word", "\u{2191} \u{2193} / w s", "line"),
            ("+ \u{2212}", "nudge \u{b1}10 ms", "Enter", "exact word time"),
            ("t", "line timestamp", "e", "edit line text"),
            ("d", "delete word", "i / a", "insert word"),
            ("o / O", "add line", "C-c", "save + exit"),
            ("C-s", "save in place", "Esc", "discard"),
            ("pause", "select current word", "", ""),
        ];
        let two_col = width >= 44;
        let half = (width / 2).min(30);
        for (r, (lk, ld, rk, rd)) in body_rows.iter().enumerate() {
            let y = area.y + 1 + r as u16;
            if y >= area.y + rows {
                break;
            }
            cell(buf, area.x, y, half, lk, ld);
            if two_col {
                cell(buf, area.x + half as u16, y, width - half, rk, rd);
            }
        }
    }
    #[inline]
    pub fn read_cava_data(
        height: u16,
        read_buffer: &mut [u8],
        columns: &mut [f32],
        stdout: &mut impl Read,
        stderr: &mut impl Read,
    ) -> Result<()> {
        if let Err(err) = stdout.read_exact(read_buffer) {
            let mut buf = String::new();
            stderr.read_to_string(&mut buf)?;
            log::error!(err:?, stderr = buf.as_str(); "Cava failed");
            bail!("Cava failed {err}");
        }
        for x in 0..columns.len() {
            let value = u16::from_le_bytes([read_buffer[2 * x], read_buffer[2 * x + 1]]);
            columns[x] = value as f32 * height as f32 / 65535.0f32;
        }
        Ok(())
    }
    #[inline]
    pub fn render_cava(
        writer: &TtyWriter,
        area: Rect,
        bar_width: u16,
        columns: &mut [f32],
        prev_columns: &mut [f32],
        x_offset: u16,
        empty_bar_symbol: &str,
        theme: &CavaTheme,
    ) -> Result<()> {
        let mut writer = writer.lock();
        let height = match theme.orientation {
            Orientation::Top | Orientation::Bottom => area.height,
            Orientation::Horizontal => area.height / 2,
        };
        queue!(writer, BeginSynchronizedUpdate, SavePosition)?;
        for (col_idx, column) in columns.iter().enumerate() {
            if *column == prev_columns[col_idx] {
                continue;
            }
            prev_columns[col_idx] = *column;
            let col_idx = col_idx as u16;
            let x = area.x + x_offset + col_idx * bar_width
                + col_idx * theme.bar_spacing;
            for y in 0..height {
                let color = theme.bar_color.get_color(y as usize, height);
                let fill_amount = (*column - f32::from(y)).clamp(0.0, 0.99);
                if matches!(
                    theme.orientation, Orientation::Horizontal | Orientation::Bottom
                ) {
                    let y = area.y + (height - 1) - y;
                    queue!(writer, MoveTo(x, y))?;
                    if fill_amount < 0.01 {
                        queue!(
                            writer, PrintStyledContent(empty_bar_symbol.on(theme
                            .bg_color))
                        )?;
                    } else {
                        let char_index = (fill_amount * theme.bar_symbols_count as f32)
                            .floor() as usize;
                        let fill_char = theme.bar_symbols[char_index].as_str();
                        queue!(
                            writer, PrintStyledContent(fill_char.with(color).on(theme
                            .bg_color))
                        )?;
                    }
                }
                let y = match theme.orientation {
                    Orientation::Top => Some(area.y + y),
                    Orientation::Horizontal => Some(area.y + height + y),
                    Orientation::Bottom => None,
                };
                if let Some(y) = y {
                    queue!(writer, MoveTo(x, y))?;
                    if fill_amount < 0.01 {
                        queue!(
                            writer, PrintStyledContent(empty_bar_symbol.on(theme
                            .bg_color))
                        )?;
                    } else {
                        let char_index = (fill_amount
                            * theme.inverted_bar_symbols_count as f32)
                            .floor() as usize;
                        let fill_char = theme.inverted_bar_symbols[char_index].as_str();
                        queue!(
                            writer, PrintStyledContent(fill_char.with(color).on(theme
                            .bg_color))
                        )?;
                    }
                }
            }
        }
        queue!(writer, RestorePosition, EndSynchronizedUpdate)?;
        writer.flush()?;
        Ok(())
    }
    fn spawn_cava(bars: u16, config: &Cava) -> Result<ProcessGuard> {
        let cfg_dir = std::env::temp_dir().join("rmpc");
        std::fs::create_dir_all(&cfg_dir)?;
        let cfg_path = cfg_dir
            .join(format!("cava-{}.conf", rustix::process::geteuid().as_raw()));
        let mut config = config.clone();
        config.input.source = Self::normalize_pipewire_source(&config.input.source);
        let node_name = Self::node_name_rename_env(config.input.node_name.as_deref());
        let config = config.to_cava_config_file(bars)?;
        std::fs::write(&cfg_path, config)?;
        let mut cmd = std::process::Command::new("cava");
        cmd.arg("-p")
            .arg(cfg_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        if let Some(name) = node_name {
            if let Some(shim) = crate::shared::paths::cava_node_name_shim() {
                cmd.env("LD_PRELOAD", &shim).env("CAVA_NODE_NAME", name);
            } else {
                log::warn!(
                    name;
                    "cava node_name is configured but the name shim is missing — re-run setup.sh to build it (S2UDIO_CAVA_NAME_SHIM overrides the path)"
                );
            }
        }
        Ok(ProcessGuard {
            handle: cmd.spawn()?,
        })
    }
    /// The `CAVA_NODE_NAME` value to use for a configured rename: a
    /// non-empty configured name, or `None` to keep cava's own.
    fn node_name_rename_env(node_name: Option<&str>) -> Option<&str> {
        node_name.filter(|n| !n.is_empty())
    }
    /// cava's pipewire `source` must name a *monitor* (`X.monitor`) to
    /// capture what sink `X` plays: a raw sink name makes cava set
    /// `target.object`, which PipeWire does not feed reliably on every setup
    /// (the bars come back flat). Sink names are rewritten to their
    /// `.monitor` form; `auto`, mics/virtual sources and existing monitors
    /// are left untouched.
    fn normalize_pipewire_source(source: &str) -> String {
        let Ok(out) = std::process::Command::new("pactl")
            .arg("list")
            .arg("short")
            .arg("sinks")
            .output() else {
            return source.to_string();
        };
        let sinks = String::from_utf8_lossy(&out.stdout);
        Self::normalize_pipewire_source_with(source, sinks.lines())
    }
    /// The pure core of [`normalize_pipewire_source`], split out for tests:
    /// `sink_names` are the `pactl list short sinks` lines (the sink name is
    /// the second tab-separated column; a bare name also matches).
    fn normalize_pipewire_source_with<'a>(
        source: &str,
        mut sink_names: impl Iterator<Item = &'a str>,
    ) -> String {
        if source.ends_with(".fifo") || source.contains('/') {
            return "auto".to_string();
        }
        if source.is_empty() || source == "auto" || source == "auto_input"
            || source.ends_with(".monitor")
        {
            return source.to_string();
        }
        let is_sink = sink_names.any(|line| line.split('\t').any(|col| col == source));
        if is_sink { format!("{source}.monitor") } else { source.to_string() }
    }
    /// Whether a Start at the given geometry needs to (re)spawn the cava
    /// process: when no process is running yet, when the bar count changed
    /// (a resize crossing a bar-width boundary), or when the cava config
    /// changed (Settings Save). Everything else — pause/resume, modal
    /// open/close, tab switches, a resize keeping the same bar count —
    /// reuses the running process: a respawn makes the audio DAC stop and
    /// renegotiate its ALSA period (an audible dropout with the pipewire
    /// input), so it must be avoided unless the visualizer really needs it.
    fn needs_respawn(
        process_running: bool,
        spawned_bars: u16,
        bars: u16,
        config_dirty: bool,
    ) -> bool {
        !process_running || spawned_bars != bars || config_dirty
    }
    fn run_cava_loop(
        receiver: &Receiver<CavaCommand>,
        writer: &TtyWriter,
        cava_config: Cava,
        cava_theme: CavaTheme,
    ) -> Result<()> {
        let mut prev_command: Option<Result<CavaCommand, RecvError>> = None;
        let mut cava_config = cava_config;
        let mut cava_theme = cava_theme;
        let mut area: Rect;
        let mut process: Option<ProcessGuard> = None;
        let mut spawned_bars: u16 = 0;
        let mut config_dirty = true;
        'outer: loop {
            log::trace!(prev_command:?; "Waiting for command");
            let command = prev_command.take().unwrap_or_else(|| receiver.recv());
            log::trace!(command:?; "Received command");
            match command {
                Ok(CavaCommand::Start { area: new_area }) => {
                    area = new_area;
                }
                Ok(CavaCommand::Pause) => {
                    log::debug!("Cava paused (process kept alive)");
                    continue 'outer;
                }
                Ok(CavaCommand::Stop) => {
                    break 'outer;
                }
                Ok(CavaCommand::ConfigChanged { config, theme }) => {
                    log::trace!("Cava config changed, updating");
                    cava_config = config;
                    cava_theme = theme;
                    config_dirty = true;
                    continue 'outer;
                }
                Err(RecvError) => {
                    log::error!("Error when trying to receive CavaCommand");
                    break 'outer;
                }
            }
            let base_bar_width = cava_theme.bar_width;
            let bar_spacing = cava_theme.bar_spacing;
            let slot = (base_bar_width + bar_spacing).max(1);
            let fit = area.width / slot;
            let bars = fit.clamp(1, MAX_CAVA_BARS);
            let bar_width = if fit > MAX_CAVA_BARS {
                area.width.saturating_sub((bars - 1) * bar_spacing) / bars
            } else {
                base_bar_width
            };
            if bars == 0 || area.height == 0 {
                log::debug!(
                    area:?; "Cava area too small for bars, waiting for a real area"
                );
                continue 'outer;
            }
            if Self::needs_respawn(process.is_some(), spawned_bars, bars, config_dirty) {
                log::debug!(
                    bars, previous_bars = spawned_bars, config_changed = config_dirty;
                    "Cava (re)spawning the process"
                );
                let new_process = Self::spawn_cava(bars, &cava_config)?;
                process = Some(new_process);
                spawned_bars = bars;
                config_dirty = false;
            }
            let process = process.as_mut().expect("cava process spawned above");
            let stdout = process
                .handle
                .stdout
                .as_mut()
                .context("Failed to spawn cava. No stdout.")?;
            let stderr = process
                .handle
                .stderr
                .as_mut()
                .context("Failed to spawn cava. No stderr.")?;
            let total_bar_width = bars * bar_width;
            let total_spacing_width = (bars - 1) * bar_spacing;
            let total_width = total_bar_width + total_spacing_width;
            let empty_bar_symbol = " ".repeat(bar_width as usize);
            let x_offset = (area.width - total_width) / 2;
            log::debug!(cava_theme:?; "theme");
            let mut columns = vec![0_f32; bars as usize];
            let mut prev_columns = vec![0_f32; bars as usize];
            let mut buf = vec![0_u8; 2 * bars as usize];
            let bar_height = match cava_theme.orientation {
                Orientation::Top | Orientation::Bottom => area.height,
                Orientation::Horizontal => area.height / 2,
            };
            'inner: loop {
                Self::read_cava_data(
                    bar_height,
                    &mut buf,
                    &mut columns,
                    stdout,
                    stderr,
                )?;
                Self::render_cava(
                    writer,
                    area,
                    bar_width,
                    &mut columns,
                    &mut prev_columns,
                    x_offset,
                    &empty_bar_symbol,
                    &cava_theme,
                )?;
                match receiver.try_recv() {
                    Ok(CavaCommand::Stop) => {
                        break 'outer;
                    }
                    Ok(CavaCommand::Pause) => {
                        break 'inner;
                    }
                    Ok(CavaCommand::Start { area }) => {
                        prev_command = Some(Ok(CavaCommand::Start { area }));
                        break 'inner;
                    }
                    Ok(CavaCommand::ConfigChanged { config, theme }) => {
                        prev_command = Some(
                            Ok(CavaCommand::ConfigChanged {
                                config,
                                theme,
                            }),
                        );
                        break 'inner;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        log::error!(
                            "CavaCommand channel disconnected. This should never happen."
                        );
                        break 'outer;
                    }
                }
            }
            log::debug!("Cava finished outer loop iteration");
        }
        log::debug!("Cava thread finished");
        Ok(())
    }
    pub fn spawn(&mut self, cava_config: Cava, cava_theme: CavaTheme) -> Result<()> {
        if self.handle.is_some() {
            log::debug!("Cava already running, skipping spawn");
            return Ok(());
        }
        if !CAVA.installed {
            status_warn!(
                "Cava has not been found on your system. Please install it to use the visualiser."
            );
            return Ok(());
        }
        let writer = TERMINAL.writer();
        let receiver = self.command_channel.1.clone();
        self.handle = Some(
            std::thread::Builder::new()
                .name("cava".to_owned())
                .spawn(move || -> Result<_> {
                    try_skip!(
                        Self::run_cava_loop(& receiver, & writer, cava_config,
                        cava_theme), "Cava thread encountered an error"
                    );
                    Ok(())
                })
                .context("Failed to spawn cava thread")?,
        );
        Ok(())
    }
    fn pause_and_clear(&mut self, ctx: &Ctx) -> Result<()> {
        log::debug!("Stopping cava thread and clearing area");
        self.pending_start = false;
        self.command(CavaCommand::Pause)?;
        log::debug!("Waiting for cava thread to finish");
        self.clear(ctx)?;
        Ok(())
    }
    fn clear(&self, ctx: &Ctx) -> Result<()> {
        let writer = TERMINAL.writer();
        let mut w = writer.lock();
        clear_area(w.by_ref(), ctx.config.theme.cava.bg_color.into(), self.area)?;
        Ok(())
    }
    fn command(&self, cmd: CavaCommand) -> Result<()> {
        let Some(handle) = self.handle.as_ref() else {
            log::trace!(cmd:?; "Cava thread is not running, not sending command");
            return Ok(());
        };
        if handle.is_finished() {
            log::debug!("Cava thread has finished, not sending command");
            return Ok(());
        }
        log::trace!(cmd:?; "Sending CavaCommand");
        self.command_channel
            .0
            .send_timeout(cmd, Duration::from_secs(3))
            .map_err(|err| anyhow!("Failed to send command to cava thread: {err}"))
    }
}
impl Pane for CavaPane {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &Ctx,
    ) -> anyhow::Result<()> {
        self.area = area;
        if ctx.lyrics_edit_mode.get() {
            if !self.legend_shown {
                self.legend_shown = true;
                self.pause_and_clear(ctx)?;
            }
            self.render_legend(frame, area, ctx);
            return Ok(());
        }
        if self.legend_shown {
            self.legend_shown = false;
            if matches!(ctx.status.state, State::Play) {
                self.run(ctx)?;
            }
        }
        frame
            .render_widget(
                Block::default()
                    .style(
                        Style::default()
                            .bg(Color::from_crossterm(ctx.config.theme.cava.bg_color)),
                    ),
                area,
            );
        self.reinit_if_area_changed(ctx);
        Ok(())
    }
    fn calculate_areas(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        self.area = area;
        self.reinit_if_area_changed(ctx);
        Ok(())
    }
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        self.spawn(ctx.config.cava.clone(), ctx.config.theme.cava.clone())?;
        if matches!(ctx.status.state, State::Play) {
            self.pending_start = true;
        }
        Ok(())
    }
    fn handle_action(
        &mut self,
        _ev: &mut ActionEvent,
        _ctx: &mut Ctx,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn on_hide(&mut self, ctx: &Ctx) -> Result<()> {
        self.pause_and_clear(ctx)?;
        Ok(())
    }
    fn on_event(
        &mut self,
        event: &mut UiEvent,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match event {
            UiEvent::Exit => {
                self.command(CavaCommand::Stop)?;
                if let Some(handle) = self.handle.take() {
                    handle.join().expect("Failed to join cava thread")?;
                }
            }
            UiEvent::ConfigChanged => {
                self.command(CavaCommand::ConfigChanged {
                    config: ctx.config.cava.clone(),
                    theme: ctx.config.theme.cava.clone(),
                })?;
                if is_visible && !self.is_modal_open
                    && matches!(ctx.status.state, State::Play)
                {
                    self.run(ctx)?;
                }
            }
            UiEvent::Displayed if is_visible => {
                if is_visible && !self.is_modal_open
                    && matches!(ctx.status.state, State::Play)
                {
                    self.run(ctx)?;
                }
            }
            UiEvent::Hidden if is_visible => {
                if !self.is_modal_open {
                    self.pause_and_clear(ctx)?;
                }
            }
            UiEvent::ModalOpened if is_visible => {
                if !self.is_modal_open && !ctx.lyrics_edit_mode.get() {
                    self.pause_and_clear(ctx)?;
                }
                self.is_modal_open = true;
            }
            UiEvent::ModalClosed if is_visible => {
                self.is_modal_open = false;
                if matches!(ctx.status.state, State::Play) {
                    self.run(ctx)?;
                }
            }
            UiEvent::PlaybackStateChanged if is_visible => {
                match ctx.status.state {
                    State::Play => {
                        self.run(ctx)?;
                    }
                    State::Stop | State::Pause => {
                        log::debug!(
                            "CavaPane: Player event received, clearing cava area"
                        );
                        self.pending_start = false;
                        self.command(CavaCommand::Pause)?;
                        if !ctx.lyrics_edit_mode.get() {
                            self.clear(ctx)?;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn resize(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        if self.is_modal_open {
            return Ok(());
        }
        self.area = area;
        self.pause_and_clear(ctx)?;
        if matches!(ctx.status.state, State::Play) {
            self.run(ctx)?;
        }
        Ok(())
    }
}
