
use anyhow::Result;
use ratatui::{Frame, prelude::Rect, widgets::Paragraph};

use super::Pane;
use crate::{
    ctx::Ctx,
    mpd::{
        commands::State,
        mpd_client::{MpdClient, ValueChange},
    },
    shared::{
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::widgets::progress_bar::ProgressBar,
};

#[derive(Debug)]
pub struct ProgressBarPane {
    area: Rect,
}

impl ProgressBarPane {
    pub fn new() -> Self {
        Self { area: Rect::default() }
    }
}

impl Pane for ProgressBarPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> anyhow::Result<()> {
        self.area = area;

        match ctx.messages.last() {
            Some(status) if status.created.elapsed() < status.timeout => {
                let status_bar = Paragraph::new(status.message.clone())
                    .alignment(ratatui::prelude::Alignment::Center)
                    .style(status.level.into_style(&ctx.config.theme.level_styles));
                frame.render_widget(status_bar, self.area);
            }
            _ => {
                let bar_cfg = &ctx.config.theme.progress_bar;
                // While an mpv video plays, the seekbar mirrors mpv.
                let (elapsed, duration) = if crate::core::mpv::mpv_is_ui_source(ctx) {
                    (ctx.mpv.position as u64, ctx.mpv.duration as u64)
                } else {
                    (ctx.status.elapsed.as_secs(), ctx.status.duration.as_secs())
                };
                let value = if duration == 0 {
                    0.0
                } else {
                    elapsed as f32 / duration as f32
                };
                // Hovering the seekbar lightens the colors left of the
                // pointer (the "played-portion" highlight); the rest of
                // the bar keeps its normal colors.
                let hovered = ctx.mouse_pos().is_some_and(|p| self.area.contains(p));
                let hover_col = ctx
                    .mouse_pos()
                    .filter(|_| hovered)
                    .map(|p| p.x.saturating_sub(self.area.x));
                // While the seekbar owns the keyboard (Ctrl+Tab on the
                // Queue tab) the cursor renders at the seek position and
                // the thumb + played-portion highlight follow it.
                let cursor_col = crate::ui::seekbar::cursor_fraction(ctx)
                    .map(|f| (f * f32::from(self.area.width)).round() as u16)
                    .map(|c| c.min(self.area.width.saturating_sub(1)));
                let (elapsed_style, thumb_style, track_style) = if hovered || cursor_col.is_some() {
                    (
                        crate::config::hover_style(bar_cfg.elapsed_style),
                        crate::config::hover_style(bar_cfg.thumb_style),
                        bar_cfg.track_style,
                    )
                } else {
                    (bar_cfg.elapsed_style, bar_cfg.thumb_style, bar_cfg.track_style)
                };
                let bar = ProgressBar::builder()
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

                frame.render_widget(bar, self.area);
            }
        }

        Ok(())
    }

    fn before_show(&mut self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if !self.area.contains(event.into()) {
            return Ok(());
        }

        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
                if crate::core::mpv::mpv_is_ui_source(ctx)
                    && let Some(socket) = ctx.mpv.socket.clone() =>
            {
                let fraction =
                    f32::from(event.x.saturating_sub(self.area.x)) / f32::from(self.area.width);
                crate::core::mpv::mpv_seek(&socket, f64::from(ctx.mpv.duration as f32 * fraction));
                ctx.render()?;
            }
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
                if matches!(ctx.status.state, State::Play | State::Pause) =>
            {
                let second_to_seek_to = ctx
                    .status
                    .duration
                    .mul_f32(
                        f32::from(event.x.saturating_sub(self.area.x)) / f32::from(self.area.width),
                    )
                    .as_secs();
                ctx.command(move |client| {
                    client.seek_current(ValueChange::Set(u32::try_from(second_to_seek_to)?))?;
                    Ok(())
                });

                ctx.render()?;
            }
            _ => {}
        }

        Ok(())
    }

    fn handle_action(&mut self, _event: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}
