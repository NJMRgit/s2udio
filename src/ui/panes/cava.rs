use std::{
    io::{Read, Write},
    process::{Child, Stdio},
    thread::JoinHandle,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam::channel::{Receiver, RecvError, Sender, TryRecvError};
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    queue,
    style::{PrintStyledContent, Stylize},
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use ratatui::{
    Frame,
    layout::Rect,
    prelude::FromCrossterm,
    style::{Color, Style},
    widgets::Block,
};

/// Maximum number of cava bars; beyond this the bar width grows instead of
/// adding more bars, so a wider window makes thicker bars (and stops
/// restarting cava on every resize once the cap is reached).
const MAX_CAVA_BARS: u16 = 64;

use super::Pane;
use crate::{
    config::{
        cava::Cava,
        theme::cava::{CavaTheme, Orientation},
    },
    ctx::Ctx,
    mpd::commands::State,
    shared::{
        dependencies::CAVA,
        keys::ActionEvent,
        terminal::{TERMINAL, TtyWriter},
    },
    status_warn,
    try_skip,
    ui::{UiEvent, image::clear_area},
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
        self.command(CavaCommand::Start { area: self.area })?;
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
        if !self.pending_start
            && self.area != self.sent_area
            && !self.is_modal_open
            && matches!(ctx.status.state, State::Play)
            && !ctx.resizing.get()
            && !ctx.lyrics_edit_mode.get()
        {
            self.sent_area = self.area;
            self.command(CavaCommand::Start { area: self.area }).ok();
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

        let cell = |buf: &mut ratatui::buffer::Buffer,
                    x: u16,
                    y: u16,
                    max_w: usize,
                    key: &str,
                    desc: &str| {
            buf.set_stringn(x, y, key, max_w, key_style);
            let dx = x + key.width() as u16 + 1;
            if dx < area.x + area.width {
                buf.set_stringn(dx, y, desc, (area.x + area.width - dx) as usize, text_style);
            }
        };

        buf.set_stringn(area.x, area.y, "Lyrics edit mode", width, key_style);

        let body_rows = [
            ("\u{2190} \u{2192}", "word", "\u{2191} \u{2193} / w s", "line"),
            ("+ \u{2212}", "nudge \u{b1}10 ms", "Enter", "exact word time"),
            ("t", "line timestamp", "e", "edit line text"),
            ("d", "delete line", "i / a", "insert word"),
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
            // Only redraw columns whose value changed since the last frame;
            // this keeps the terminal write volume bounded by the spectral
            // activity instead of the area size, so a larger TUI stays fast.
            if *column == prev_columns[col_idx] {
                continue;
            }
            prev_columns[col_idx] = *column;

            let col_idx = col_idx as u16;
            let x = area.x + x_offset + col_idx * bar_width + col_idx * theme.bar_spacing;

            for y in 0..height {
                let color = theme.bar_color.get_color(y as usize, height);
                let fill_amount = (*column - f32::from(y)).clamp(0.0, 0.99);

                // render from bottom to top
                if matches!(theme.orientation, Orientation::Horizontal | Orientation::Bottom) {
                    let y = area.y + (height - 1) - y;
                    queue!(writer, MoveTo(x, y))?;
                    if fill_amount < 0.01 {
                        queue!(writer, PrintStyledContent(empty_bar_symbol.on(theme.bg_color)))?;
                    } else {
                        let char_index =
                            (fill_amount * theme.bar_symbols_count as f32).floor() as usize;
                        let fill_char = theme.bar_symbols[char_index].as_str();
                        queue!(
                            writer,
                            PrintStyledContent(fill_char.with(color).on(theme.bg_color))
                        )?;
                    }
                }

                // render from top to bottom with inverted characters
                let y = match theme.orientation {
                    Orientation::Top => Some(area.y + y),
                    Orientation::Horizontal => Some(area.y + height + y),
                    Orientation::Bottom => None,
                };
                if let Some(y) = y {
                    queue!(writer, MoveTo(x, y))?;
                    if fill_amount < 0.01 {
                        queue!(writer, PrintStyledContent(empty_bar_symbol.on(theme.bg_color)))?;
                    } else {
                        let char_index = (fill_amount * theme.inverted_bar_symbols_count as f32)
                            .floor() as usize;
                        let fill_char = theme.inverted_bar_symbols[char_index].as_str();
                        queue!(
                            writer,
                            PrintStyledContent(fill_char.with(color).on(theme.bg_color))
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
        let cfg_path = cfg_dir.join(format!("cava-{}.conf", rustix::process::geteuid().as_raw()));
        // Round 30: cava is PipeWire-only. A `source` that names a sink
        // directly would make cava set `target.object`, which PipeWire does
        // not feed on every setup (the bars come back flat). Capture the
        // sink's monitor instead.
        let mut config = config.clone();
        config.input.source = Self::normalize_pipewire_source(&config.input.source);
        // Round 29: the configured node name must be read before `config`
        // becomes the generated conf text below.
        let node_name = Self::node_name_rename_env(config.input.node_name.as_deref());
        let config = config.to_cava_config_file(bars)?;
        std::fs::write(&cfg_path, config)?;

        let mut cmd = std::process::Command::new("cava");
        cmd.arg("-p")
            .arg(cfg_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        // Round 29: when a node name is configured, rename cava's PipeWire
        // stream node (cava hardcodes `node.name = "cava"`) via the
        // LD_PRELOAD shim that injects node.name/media.name from
        // CAVA_NODE_NAME. Only the s2udio-spawned cava gets the env, so the
        // other cava instances on the system keep their own names. A missing
        // shim is not fatal — cava just runs with its own name.
        if let Some(name) = node_name {
            if let Some(shim) = crate::shared::paths::cava_node_name_shim() {
                cmd.env("LD_PRELOAD", &shim).env("CAVA_NODE_NAME", name);
            } else {
                log::warn!(name;
                    "cava node_name is configured but the name shim is missing — re-run setup.sh to build it (S2UDIO_CAVA_NAME_SHIM overrides the path)"
                );
            }
        }

        Ok(ProcessGuard { handle: cmd.spawn()? })
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
            .output()
        else {
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
        // Round 30: a leftover MPD-fifo path (configs written for the old
        // fifo tap) is meaningless for the PipeWire input — use the default
        // capture source.
        if source.ends_with(".fifo") || source.contains('/') {
            return "auto".to_string();
        }
        if source.is_empty() || source == "auto" || source == "auto_input"
            || source.ends_with(".monitor")
        {
            return source.to_string();
        }
        let is_sink = sink_names.any(|line| line.split('\t').any(|col| col == source));
        if is_sink {
            format!("{source}.monitor")
        } else {
            source.to_string()
        }
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
        // Set by the first Start command; the geometry code below only runs
        // after a Start has assigned it.
        let mut area: Rect;

        // One cava process for the whole session: spawned on the first
        // Start and respawned only when the bar count changes (a resize
        // crossing a bar-width boundary) or the cava config changed
        // (Settings Save). Everything else — pause/resume, modal open/close,
        // tab switches, a resize keeping the same bar count — reuses the
        // running process: with the pipewire input a respawn makes the USB
        // DAC stop and renegotiate its ALSA period, which
        // drops the audio for a moment. Only Stop (app exit) kills the
        // process.
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
                    // Keep the process (and its audio-graph connection) alive;
                    // rendering resumes on the next Start. The UI has cleared
                    // the area meanwhile, and cava simply blocks on its output
                    // pipe until the bars are wanted again.
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
            // When the window is wider than MAX_CAVA_BARS bars at the base
            // width, grow the bar width to fill the space instead of adding
            // more bars.
            let bar_width = if fit > MAX_CAVA_BARS {
                area.width.saturating_sub((bars - 1) * bar_spacing) / bars
            } else {
                base_bar_width
            };

            if bars == 0 || area.height == 0 {
                // No horizontal space for bars: do not spawn cava. The read of
                // a zero-length frame would return immediately, hot-looping the
                // render below and flooding the terminal with empty
                // synchronized-update sequences, which starves the UI (and can
                // crash tmux). Wait for a new Start with a real area instead.
                log::debug!(area:?; "Cava area too small for bars, waiting for a real area");
                continue 'outer;
            }

            // Spawn only when there is no process yet, the bar count changed,
            // or the cava config changed. A Start keeping the same bar count
            // (a resize within the same bar-width slot, a re-show after a
            // pause/modal/tab switch) just re-centers the bars at the new
            // area with the existing process.
            if Self::needs_respawn(process.is_some(), spawned_bars, bars, config_dirty) {
                log::debug!(
                    bars,
                    previous_bars = spawned_bars,
                    config_changed = config_dirty;
                    "Cava (re)spawning the process"
                );
                // The replacement is spawned before the old guard is dropped
                // (killing the old process), so the gap is as short as
                // possible.
                let new_process = Self::spawn_cava(bars, &cava_config)?;
                process = Some(new_process);
                spawned_bars = bars;
                config_dirty = false;
            }

            let process = process.as_mut().expect("cava process spawned above");
            let stdout =
                process.handle.stdout.as_mut().context("Failed to spawn cava. No stdout.")?;
            let stderr =
                process.handle.stderr.as_mut().context("Failed to spawn cava. No stderr.")?;

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
                Self::read_cava_data(bar_height, &mut buf, &mut columns, stdout, stderr)?;
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
                        prev_command = Some(Ok(CavaCommand::ConfigChanged { config, theme }));
                        break 'inner;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        log::error!("CavaCommand channel disconnected. This should never happen.");
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
                        Self::run_cava_loop(&receiver, &writer, cava_config, cava_theme),
                        "Cava thread encountered an error"
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
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> anyhow::Result<()> {
        self.area = area;
        if ctx.lyrics_edit_mode.get() {
            // Round 35: lyrics edit mode swaps the visualizer for the
            // edit-controls legend. The cava process is paused (kept
            // alive — restarting it would drop the audio for a moment)
            // and the bars are cleared before the legend draws.
            if !self.legend_shown {
                self.legend_shown = true;
                self.pause_and_clear(ctx)?;
            }
            self.render_legend(frame, area, ctx);
            return Ok(());
        }
        if self.legend_shown {
            self.legend_shown = false;
            // Leaving edit mode restores the visualizer when the player is
            // playing; a pause keeps it off until playback resumes.
            if matches!(ctx.status.state, State::Play) {
                self.run(ctx)?;
            }
        }
        frame.render_widget(
            Block::default()
                .style(Style::default().bg(Color::from_crossterm(ctx.config.theme.cava.bg_color))),
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

        // Defer the Start until the first frame of this show is flushed
        // (`maybe_start` runs right after it). Starting from here would paint
        // the bars before the UI draws and look unpolished.
        if matches!(ctx.status.state, State::Play) {
            self.pending_start = true;
        }

        Ok(())
    }

    fn handle_action(&mut self, _ev: &mut ActionEvent, _ctx: &mut Ctx) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_hide(&mut self, ctx: &Ctx) -> Result<()> {
        self.pause_and_clear(ctx)?;
        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, is_visible: bool, ctx: &Ctx) -> Result<()> {
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

                if is_visible && !self.is_modal_open && matches!(ctx.status.state, State::Play) {
                    self.run(ctx)?;
                }
            }
            UiEvent::Displayed if is_visible => {
                if is_visible && !self.is_modal_open && matches!(ctx.status.state, State::Play) {
                    self.run(ctx)?;
                }
            }
            UiEvent::Hidden if is_visible => {
                if !self.is_modal_open {
                    self.pause_and_clear(ctx)?;
                }
            }
            UiEvent::ModalOpened if is_visible => {
                // Round 40: during lyrics edit mode the pane shows the
                // legend, which lives in the ratatui buffer — a direct
                // terminal write here would wipe it from the screen
                // without invalidating the buffer, hiding it until a full
                // re-render. The cava process is already paused by edit
                // mode (legend path), so there is nothing to clear.
                if !self.is_modal_open && !ctx.lyrics_edit_mode.get() {
                    self.pause_and_clear(ctx)?;
                }
                self.is_modal_open = true;
            }
            UiEvent::ModalClosed if is_visible => {
                self.is_modal_open = false;
                // Restart the bars only when the player is playing; a pause
                // leaves them off until playback resumes. The flag must
                // clear in every case so a later playback start (or the next
                // modal) sees the correct state.
                if matches!(ctx.status.state, State::Play) {
                    self.run(ctx)?;
                }
            }
            UiEvent::PlaybackStateChanged if is_visible => match ctx.status.state {
                State::Play => {
                    self.run(ctx)?;
                }
                State::Stop | State::Pause => {
                    log::debug!("CavaPane: Player event received, clearing cava area");
                    self.pending_start = false;
                    self.command(CavaCommand::Pause)?;
                    // Round 35: during lyrics edit mode the pane shows the
                    // edit-controls legend — clearing the area would wipe
                    // it (the bars are already paused, and no render
                    // follows this event to restore the legend until the
                    // next status change).
                    if !ctx.lyrics_edit_mode.get() {
                        self.clear(ctx)?;
                    }
                }
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> Ctx {
        crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }

    /// The cava bars are a terminal-side overlay: while any modal (the
    /// Settings panel, paste popup, …) is open they must never start
    /// drawing — the overlay would paint over the modal's full-window view.
    #[test]
    fn run_is_suppressed_while_a_modal_is_open() {
        let mut ctx = test_ctx();
        let mut pane = CavaPane::new(&ctx);

        // The settings panel opened while playing: the bars stop.
        pane.area = Rect::new(0, 0, 40, 12);
        pane.on_event(&mut UiEvent::ModalOpened, true, &ctx).unwrap();
        assert!(pane.is_modal_open);

        // Any run() attempt while the modal is open must not start the bars.
        pane.run(&ctx).unwrap();
        assert_eq!(pane.sent_area, Rect::default(), "bars must not start under a modal");

        // Closing the modal (still playing) clears the flag and restarts.
        ctx.status.state = State::Play;
        pane.on_event(&mut UiEvent::ModalClosed, true, &ctx).unwrap();
        assert!(!pane.is_modal_open);
        assert_eq!(pane.sent_area, pane.area, "bars restart after the modal closes");
    }

    /// Closing the modal while paused must still clear the flag, so a later
    /// playback start (PlaybackStateChanged) draws the bars again instead of
    /// being stuck behind a stale is_modal_open.
    #[test]
    fn modal_closed_clears_the_flag_when_paused_too() {
        let mut ctx = test_ctx();
        let mut pane = CavaPane::new(&ctx);

        pane.on_event(&mut UiEvent::ModalOpened, true, &ctx).unwrap();
        assert!(pane.is_modal_open);

        // The modal closes while the player is paused.
        ctx.status.state = State::Pause;
        pane.on_event(&mut UiEvent::ModalClosed, true, &ctx).unwrap();
        assert!(!pane.is_modal_open, "the flag clears even when paused");

        // Playback resumes: the bars start again.
        pane.area = Rect::new(0, 0, 40, 12);
        ctx.status.state = State::Play;
        pane.on_event(&mut UiEvent::PlaybackStateChanged, true, &ctx).unwrap();
        assert_eq!(pane.sent_area, pane.area, "playback start redraws the bars");
    }

    /// Lyrics edit mode swaps the visualizer for the edit-controls
    /// legend: rendering with the flag set draws the legend and pauses the
    /// bars; clearing the flag (while playing) restarts them.
    #[test]
    fn edit_mode_renders_the_legend_and_pauses_the_bars() {
        let mut ctx = test_ctx();
        let mut pane = CavaPane::new(&ctx);
        pane.area = Rect::new(0, 0, 60, 12);
        ctx.status.state = State::Play;
        ctx.lyrics_edit_mode.set(true);

        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| pane.render(f, pane.area, &ctx).unwrap())
            .unwrap();
        assert!(pane.legend_shown, "edit mode shows the legend");
        let buf = terminal.backend().buffer();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Lyrics edit mode"), "legend title: {text}");
        assert!(text.contains("delete line"), "delete line entry: {text}");
        assert!(text.contains("insert word"), "insert-word entry: {text}");
        assert!(text.contains("add line"), "add-line entry: {text}");
        assert!(text.contains("nudge"), "nudge entry: {text}");
        assert!(text.contains("save + exit"), "ctrl+c entry: {text}");
        assert!(text.contains("discard"), "esc-discard entry: {text}");
        assert!(text.contains("save in place"), "ctrl+s entry: {text}");

        // Leaving edit mode while playing restores the visualizer.
        ctx.lyrics_edit_mode.set(false);
        terminal
            .draw(|f| pane.render(f, pane.area, &ctx).unwrap())
            .unwrap();
        assert!(!pane.legend_shown, "legend goes away after edit mode");
        assert_eq!(pane.sent_area, pane.area, "bars restart after leaving edit mode");
    }

    /// The bars must never start while lyrics edit mode is on, even via
    /// the deferred/geometry paths (before_show's pending start, area
    /// reinit) — the legend owns the pane for the duration.
    #[test]
    fn edit_mode_suppresses_deferred_and_geometry_starts() {
        let mut ctx = test_ctx();
        let mut pane = CavaPane::new(&ctx);
        pane.area = Rect::new(0, 0, 40, 12);
        ctx.status.state = State::Play;
        ctx.lyrics_edit_mode.set(true);

        pane.run(&ctx).unwrap();
        assert_eq!(pane.sent_area, Rect::default(), "run suppressed during edit mode");
        pane.reinit_if_area_changed(&ctx);
        assert_eq!(pane.sent_area, Rect::default(), "geometry reinit suppressed");
    }

    /// The first Start is deferred until the frame after `before_show`
    /// (feedback: the bars painted before the UI). `before_show` only arms
    /// `pending_start`; `maybe_start` (called by the UI after the frame is
    /// flushed) actually starts the bars — and only when playing, with no
    /// modal open.
    #[test]
    fn start_is_deferred_until_after_the_first_frame() {
        let mut ctx = test_ctx();
        let mut pane = CavaPane::new(&ctx);
        pane.area = Rect::new(0, 0, 40, 12);
        ctx.status.state = State::Play;

        // before_show arms the pending start but must not send it yet.
        pane.before_show(&ctx).unwrap();
        assert!(pane.pending_start, "before_show arms the deferred start");
        assert_eq!(pane.sent_area, Rect::default(), "bars must not start yet");

        // The post-frame hook starts them.
        pane.maybe_start(&ctx).unwrap();
        assert!(!pane.pending_start, "maybe_start consumes the pending start");
        assert_eq!(pane.sent_area, pane.area, "bars start after the frame");
    }

    /// Respawns are the expensive (audio-graph-churning) part of the cava
    /// visualizer: with the pipewire input killing and respawning the
    /// process makes the USB DAC stop and renegotiate its
    /// ALSA period, dropping the audio for a moment. The process must only
    /// be (re)spawned when it is missing, when the bar count changed (a
    /// resize crossing a bar-width boundary), or when the cava config
    /// changed (Settings Save).
    #[test]
    fn respawns_only_when_necessary() {
        // First Start with no process: spawn.
        assert!(CavaPane::needs_respawn(false, 0, 20, false));
        // Same bar count with a running process: reuse it (pause/resume,
        // modal close, tab re-show, resize within the same bar-width slot).
        assert!(!CavaPane::needs_respawn(true, 20, 20, false));
        // Bar count changed: respawn (the bars cannot grow/shrink without
        // a new cava config).
        assert!(CavaPane::needs_respawn(true, 20, 21, false));
        assert!(CavaPane::needs_respawn(true, 20, 19, false));
        // Config changed (Settings Save): respawn even with the same bars.
        assert!(CavaPane::needs_respawn(true, 20, 20, true));
    }

    /// While a terminal resize is in progress (the event loop's 500 ms
    /// debounce window) the pane must not restart the visualizer on every
    /// frame: each restart churns the audio graph and drops the audio for a
    /// moment with the pipewire input. The settled resize that follows
    /// re-initializes the bars.
    #[test]
    fn no_restart_while_resizing() {
        let mut ctx = test_ctx();
        let mut pane = CavaPane::new(&ctx);
        pane.area = Rect::new(0, 0, 40, 12);
        ctx.status.state = State::Play;
        pane.sent_area = Rect::new(0, 0, 20, 12);

        // Resize in progress: geometry changes are ignored.
        ctx.resizing.set(true);
        pane.calculate_areas(Rect::new(0, 0, 40, 12), &ctx).unwrap();
        assert_eq!(
            pane.sent_area,
            Rect::new(0, 0, 20, 12),
            "no restart while the resize is in progress"
        );

        // Resize settled: the new geometry is applied.
        ctx.resizing.set(false);
        pane.calculate_areas(Rect::new(0, 0, 40, 12), &ctx).unwrap();
        assert_eq!(pane.sent_area, pane.area, "settled resize re-initializes the bars");
    }

    /// A pipewire `source` naming a sink is rewritten to its `.monitor`
    /// form (the capture cava can actually visualize); sources, `auto` and
    /// existing monitors are left alone.
    #[test]
    /// Round 29: only a configured, non-empty node name triggers the
    /// LD_PRELOAD rename env (empty/absent = cava's own "cava" node).
    #[test]
    fn node_name_rename_env_filters_empty_names() {
        assert_eq!(CavaPane::node_name_rename_env(None), None);
        assert_eq!(CavaPane::node_name_rename_env(Some("")), None);
        assert_eq!(CavaPane::node_name_rename_env(Some("s2udio-cava")), Some("s2udio-cava"));
    }

    fn pipewire_source_normalizes_sink_names_to_monitors() {
        // pactl short-sinks lines (name is the second tab column) and a
        // bare name both match.
        let sinks = [
            "83\talsa_output.usb-FiiO_FiiO_KA3_FiiO_KA3-00.analog-stereo\tPipeWire",
            "easyeffects_sink",
        ];
        let norm = |s: &str| CavaPane::normalize_pipewire_source_with(s, sinks.iter().copied());

        // A sink name becomes its monitor form (cava capture.sink semantics).
        assert_eq!(
            norm("alsa_output.usb-FiiO_FiiO_KA3_FiiO_KA3-00.analog-stereo"),
            "alsa_output.usb-FiiO_FiiO_KA3_FiiO_KA3-00.analog-stereo.monitor"
        );
        assert_eq!(norm("easyeffects_sink"), "easyeffects_sink.monitor");
        // Already a monitor / auto / empty: untouched.
        assert_eq!(norm("Media.monitor"), "Media.monitor");
        assert_eq!(norm("auto"), "auto");
        assert_eq!(norm(""), "");
        // A mic source that is not a sink: untouched.
        assert_eq!(
            norm("alsa_input.usb-BurrBrown_from_Texas_Instruments_USB_AUDIO_CODEC-00.analog-stereo"),
            "alsa_input.usb-BurrBrown_from_Texas_Instruments_USB_AUDIO_CODEC-00.analog-stereo"
        );
        // Round 30: a leftover MPD-fifo path falls back to the PipeWire
        // default capture source.
        assert_eq!(norm("/tmp/mpd-cava.fifo"), "auto");
        assert_eq!(norm("mpd-cava.fifo"), "auto");
    }

    /// A deferred start while paused (or with a modal open) is dropped
    /// instead of drawing: the playback/modal event handlers will start the
    /// bars at the right moment.
    #[test]
    fn deferred_start_is_dropped_when_paused() {
        let mut ctx = test_ctx();
        let mut pane = CavaPane::new(&ctx);
        pane.area = Rect::new(0, 0, 40, 12);

        // Paused: before_show does not even arm the start.
        ctx.status.state = State::Pause;
        pane.before_show(&ctx).unwrap();
        assert!(!pane.pending_start, "paused: no deferred start armed");
        pane.maybe_start(&ctx).unwrap();
        assert_eq!(pane.sent_area, Rect::default(), "paused: bars stay off");

        // Playing but a modal is open: maybe_start drops the start rather
        // than painting over the modal.
        pane.pending_start = true;
        pane.is_modal_open = true;
        ctx.status.state = State::Play;
        pane.maybe_start(&ctx).unwrap();
        assert!(!pane.pending_start, "modal: pending start dropped");
        assert_eq!(pane.sent_area, Rect::default(), "modal: bars never start");
    }
}
