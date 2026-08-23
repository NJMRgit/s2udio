use anyhow::Result;
use ratatui::{Frame, prelude::Rect};
use super::Pane;
use crate::{
    config::tabs::VolumeType, ctx::Ctx,
    shared::{keys::ActionEvent, mouse_event::{MouseEvent, MouseEventKind}},
};
#[derive(Debug)]
pub struct VolumePane {
    area: Rect,
    config: VolumeType,
}
impl VolumePane {
    pub fn new(config: VolumeType) -> Self {
        Self {
            area: Rect::default(),
            config,
        }
    }
}
impl Pane for VolumePane {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &Ctx,
    ) -> anyhow::Result<()> {
        self.area = area;
        match &self.config {
            VolumeType::Slider(config) => {
                if area.height < 1 || area.width < 1 {
                    return Ok(());
                }
                let symbols = &config.symbols;
                let filled_len = (f64::from(area.width - 1)
                    * f64::from(crate::core::mpv::ui_volume(ctx).min(100)) / 100.0)
                    as u16;
                for i in 0..area.width {
                    let style = if i <= filled_len && filled_len > 0 {
                        config.filled_style
                    } else {
                        config.track_style
                    };
                    let (c, style) = if let Some(sym) = &config.symbols.start && i == 0 {
                        (sym, style)
                    } else if i < filled_len {
                        (&symbols.filled, style)
                    } else if let Some(sym) = &config.symbols.end && i == area.width - 1
                    {
                        (sym, style)
                    } else if i == filled_len {
                        (&symbols.thumb, config.thumb_style)
                    } else {
                        (&symbols.track, style)
                    };
                    frame.buffer_mut().set_string(area.x + i, area.y, c, style);
                }
            }
        }
        Ok(())
    }
    fn before_show(&mut self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                if !self.area.contains(event.into()) {
                    return Ok(());
                }
                if self.area.width == 0 {
                    return Ok(());
                }
                let volume_ratio = f32::from(event.x.saturating_sub(self.area.x))
                    / f32::from(self.area.width - 1);
                let new_volume = (volume_ratio * 100.0).clamp(0.0, 100.0).round() as u32;
                crate::core::mpv::set_volume(ctx, new_volume);
                ctx.render()?;
            }
            MouseEventKind::ScrollUp => {
                if !self.area.contains(event.into()) {
                    return Ok(());
                }
                let volume_step: u32 = ctx.config.volume_step.into();
                let base = crate::core::mpv::ui_volume(ctx);
                crate::core::mpv::set_volume(ctx, base + volume_step);
                ctx.render()?;
            }
            MouseEventKind::ScrollDown => {
                if !self.area.contains(event.into()) {
                    return Ok(());
                }
                let volume_step: u32 = ctx.config.volume_step.into();
                let base = crate::core::mpv::ui_volume(ctx);
                crate::core::mpv::set_volume(ctx, base.saturating_sub(volume_step));
                ctx.render()?;
            }
            MouseEventKind::Drag { drag_start_position } => {
                if !self.area.contains(drag_start_position) {
                    return Ok(());
                }
                let volume_ratio = f32::from(event.x.saturating_sub(self.area.x))
                    / f32::from(self.area.width - 1);
                let new_volume = (volume_ratio * 100.0).clamp(0.0, 100.0).round() as u32;
                crate::core::mpv::set_volume(ctx, new_volume);
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
