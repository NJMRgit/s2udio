use std::{collections::HashMap, time::Instant};
use anyhow::{Context, Result};
use itertools::Itertools;
use ratatui::{Frame, layout::Rect, style::Style, widgets::Block};
use super::{Pane as _, PaneContainer, Panes, panes::pane_call};
use crate::{
    config::{keys::CommonAction, tabs::{PaneType, SizedPaneOrSplit}},
    ctx::Ctx,
    shared::{
        ext::{rect::RectExt, vec::VecExt},
        id::Id, keys::ActionEvent, mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::input::InputResultEvent,
};
#[derive(Debug)]
pub struct PaneData {
    area: Rect,
    block_area: Rect,
    focusable: bool,
    active: Instant,
}
impl PaneData {
    fn new(focusable: bool) -> Self {
        Self {
            focusable,
            active: Instant::now(),
            area: Rect::default(),
            block_area: Rect::default(),
        }
    }
}
#[derive(Debug)]
pub struct TabScreen {
    focused: Id,
    pub panes: SizedPaneOrSplit,
    pane_data: HashMap<Id, PaneData>,
    initialized: bool,
    root_height: u16,
}
impl TabScreen {
    pub fn new(panes: SizedPaneOrSplit) -> Result<Self> {
        let focused = panes
            .panes_iter()
            .next()
            .context("Tab needs at least one pane to be valid!")?
            .id;
        Ok(Self {
            panes,
            focused,
            initialized: false,
            root_height: 0,
            pane_data: HashMap::default(),
        })
    }
    fn set_focused(&mut self, id: Id) {
        self.focused = id;
        if let Some(data) = self.pane_data.get_mut(&id) {
            data.active = Instant::now();
        }
    }
}
impl TabScreen {
    pub fn render(
        &mut self,
        pane_container: &mut PaneContainer,
        frame: &mut Frame,
        area: Rect,
        root_height: u16,
        ctx: &Ctx,
    ) -> Result<()> {
        self.root_height = root_height;
        let focused = self.panes.panes_iter().find(|pane| pane.id == self.focused);
        self.panes
            .for_each_pane_custom_data(
                area,
                root_height,
                frame,
                &mut |pane, area, block, block_area, bg_color, frame| {
                    let pane_data = self
                        .pane_data
                        .entry(pane.id)
                        .or_insert_with(|| PaneData::new(pane.is_focusable()));
                    pane_data.area = area;
                    pane_data.block_area = block_area;
                    let block = block
                        .border_style(
                            if focused.is_some_and(|p| p.id == pane.id) {
                                pane.border_active_style
                                    .unwrap_or_else(|| ctx.config.as_focused_border_style())
                            } else {
                                pane.border_style
                                    .unwrap_or_else(|| ctx.config.as_border_style())
                            },
                        );
                    if let Some(bg_color) = bg_color {
                        frame
                            .render_widget(
                                Block::default().style(Style::default().bg(bg_color)),
                                area,
                            );
                    }
                    let mut pane_instance = pane_container.get_mut(&pane.pane, ctx)?;
                    pane_call!(pane_instance, render(frame, area, ctx))?;
                    frame.render_widget(block, block_area);
                    if let Panes::Queue(queue_pane) = &mut pane_instance {
                        queue_pane
                            .render_toggle_on_border(
                                frame,
                                pane.borders,
                                block_area,
                                ctx,
                            );
                    }
                    Ok(())
                },
                &mut |block, block_area, background_color, frame| {
                    if let Some(bg_color) = background_color {
                        frame
                            .render_widget(
                                Block::default().style(Style::default().bg(bg_color)),
                                block.inner(block_area),
                            );
                    }
                    frame.render_widget(block, block_area);
                    Ok(())
                },
                ctx,
            )?;
        Ok(())
    }
    pub(in crate::ui) fn handle_insert_mode(
        &mut self,
        panes: &mut PaneContainer,
        kind: InputResultEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        let Some(focused) = self.panes.panes_iter().find(|pane| pane.id == self.focused)
        else {
            log::error!(
                "Unable to find focused pane, this should not happen. Please report this issue."
            );
            return Ok(());
        };
        let mut pane = panes.get_mut(&focused.pane, ctx)?;
        pane_call!(pane, handle_insert_mode(kind, ctx))?;
        Ok(())
    }
    pub(in crate::ui) fn handle_action(
        &mut self,
        panes: &mut PaneContainer,
        event: &mut ActionEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        let Some(focused_pane_data) = self.pane_data.get(&self.focused) else {
            log::warn!(
                focused:? = self.focused, pane_areas:? = self.pane_data;
                "Tried to find focused pane area but it does not exist"
            );
            return Ok(());
        };
        let focused_area = focused_pane_data.area;
        match event.claim_common() {
            Some(CommonAction::PaneUp) => {
                let pane_to_focus = self
                    .panes_directly_above(focused_area)
                    .collect_vec()
                    .or_else_if_empty(|| self.closest_panes_above(focused_area))
                    .into_iter()
                    .max_by_key(|(_, data)| data.active)
                    .and_then(|(id, _)| {
                        self.panes.panes_iter().find(|pane| pane.id == *id)
                    });
                if let Some(pane) = pane_to_focus {
                    self.set_focused(pane.id);
                }
                ctx.render()?;
            }
            Some(CommonAction::PaneDown) => {
                let pane_to_focus = self
                    .panes_directly_below(focused_area)
                    .collect_vec()
                    .or_else_if_empty(|| self.closest_panes_below(focused_area))
                    .into_iter()
                    .max_by_key(|(_, data)| data.active)
                    .and_then(|(id, _)| {
                        self.panes.panes_iter().find(|pane| pane.id == *id)
                    });
                if let Some(pane) = pane_to_focus {
                    self.set_focused(pane.id);
                }
                ctx.render()?;
            }
            Some(CommonAction::PaneRight) => {
                let pane_to_focus = self
                    .panes_directly_right(focused_area)
                    .collect_vec()
                    .or_else_if_empty(|| self.closest_panes_right(focused_area))
                    .into_iter()
                    .max_by_key(|(_, data)| data.active)
                    .and_then(|(id, _)| {
                        self.panes.panes_iter().find(|pane| pane.id == *id)
                    });
                if let Some(pane) = pane_to_focus {
                    self.set_focused(pane.id);
                }
                ctx.render()?;
            }
            Some(CommonAction::PaneLeft) => {
                let pane_to_focus = self
                    .panes_directly_left(focused_area)
                    .collect_vec()
                    .or_else_if_empty(|| self.closest_panes_left(focused_area))
                    .into_iter()
                    .max_by_key(|(_, data)| data.active)
                    .and_then(|(id, _)| {
                        self.panes.panes_iter().find(|pane| pane.id == *id)
                    });
                if let Some(pane) = pane_to_focus {
                    self.set_focused(pane.id);
                }
                ctx.render()?;
            }
            Some(_) | None => {
                event.abandon();
                let Some(focused) = self
                    .panes
                    .panes_iter()
                    .find(|pane| pane.id == self.focused) else {
                    log::error!(
                        "Unable to find focused pane, this should not happen. Please report this issue."
                    );
                    return Ok(());
                };
                let mut pane = panes.get_mut(&focused.pane, ctx)?;
                pane_call!(pane, handle_action(event, ctx))?;
            }
        }
        Ok(())
    }
    pub(in crate::ui) fn handle_mouse_event(
        &mut self,
        panes: &mut PaneContainer,
        event: MouseEvent,
        ctx: &Ctx,
    ) -> Result<()> {
        let position = event.into();
        let queue_pane_id = self
            .panes
            .panes_iter()
            .find(|p| p.pane == PaneType::Queue)
            .map(|p| p.id);
        let pane_id = {
            let on_toggle = queue_pane_id
                .is_some_and(|_| {
                    matches!(
                        panes.get_mut(& PaneType::Queue, ctx), Ok(Panes::Queue(q)) if q
                        .toggle_areas.iter().any(| area | area.contains(position))
                    )
                });
            let found = if on_toggle {
                queue_pane_id.map(|id| (id, self.pane_data.get(&id)))
            } else {
                self.pane_data
                    .iter()
                    .find(|(_, PaneData { area, .. })| area.contains(position))
                    .or_else(|| {
                        self.pane_data
                            .iter()
                            .find(|(_, PaneData { block_area, .. })| {
                                block_area.contains(position)
                            })
                    })
                    .map(|(id, data)| (*id, Some(data)))
            };
            let Some((pane_id, data)) = found
                .and_then(|(id, data)| data.map(|data| (id, data))) else {
                return Ok(());
            };
            if matches!(event.kind, MouseEventKind::LeftClick) && data.focusable {
                self.set_focused(pane_id);
            }
            pane_id
        };
        let Some(pane) = self.panes.panes_iter().find(|pane| pane.id == pane_id) else {
            return Ok(());
        };
        let mut pane = panes.get_mut(&pane.pane, ctx)?;
        pane_call!(pane, handle_mouse_event(event, ctx))?;
        Ok(())
    }
    pub fn on_hide(&mut self, panes: &mut PaneContainer, ctx: &Ctx) -> Result<()> {
        for pane in self.panes.panes_iter() {
            let mut pane = panes.get_mut(&pane.pane, ctx)?;
            pane_call!(pane, on_hide(ctx))?;
        }
        Ok(())
    }
    pub fn before_show(
        &mut self,
        pane_container: &mut PaneContainer,
        area: Rect,
        ctx: &Ctx,
    ) -> Result<()> {
        self.panes
            .for_each_pane(
                area,
                self.root_height,
                &mut |pane, pane_area, _, block_area, _| {
                    let pane_data = self
                        .pane_data
                        .entry(pane.id)
                        .or_insert_with(|| PaneData::new(pane.is_focusable()));
                    pane_data.area = pane_area;
                    pane_data.block_area = block_area;
                    let mut pane_instance = pane_container.get_mut(&pane.pane, ctx)?;
                    pane_call!(pane_instance, calculate_areas(pane_area, ctx))?;
                    pane_call!(pane_instance, before_show(ctx))?;
                    Ok(())
                },
                ctx,
            )?;
        if !self.initialized {
            let pane_to_focus = self
                .panes
                .panes_iter()
                .find(|pane| pane.pane == PaneType::Queue && pane.is_focusable())
                .map(|pane| pane.id)
                .or_else(|| {
                    self.pane_data
                        .iter()
                        .filter(|(_, PaneData { focusable, .. })| *focusable)
                        .min_by(|
                            (_, PaneData { area: a, .. }),
                            (_, PaneData { area: b, .. })|
                        { a.left().cmp(&b.left()).then(a.top().cmp(&b.top())) })
                        .and_then(|entry| {
                            self.panes.panes_iter().find(|pane| &pane.id == entry.0)
                        })
                        .map(|pane| pane.id)
                });
            if let Some(pane) = pane_to_focus {
                self.set_focused(pane);
            }
            self.initialized = true;
        }
        Ok(())
    }
    pub fn resize(
        &mut self,
        pane_container: &mut PaneContainer,
        area: Rect,
        ctx: &Ctx,
    ) -> Result<()> {
        self.panes
            .for_each_pane(
                area,
                self.root_height,
                &mut |pane, pane_area, _, block_area, _| {
                    let pane_data = self
                        .pane_data
                        .entry(pane.id)
                        .or_insert_with(|| PaneData::new(pane.is_focusable()));
                    pane_data.area = area;
                    pane_data.block_area = block_area;
                    let mut pane_instance = pane_container.get_mut(&pane.pane, ctx)?;
                    pane_call!(pane_instance, calculate_areas(pane_area, ctx))?;
                    pane_call!(pane_instance, resize(pane_area, ctx))?;
                    Ok(())
                },
                ctx,
            )
    }
    fn panes_directly_above(
        &self,
        focused_area: Rect,
    ) -> impl Iterator<Item = (&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(move |data| {
                data.1.focusable && focused_area.top() == data.1.block_area.bottom()
                    && data.1.block_area.overlaps_in_x(&focused_area)
            })
    }
    fn closest_panes_above(&self, focused_area: Rect) -> Vec<(&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(|data| {
                data.1.focusable && focused_area.top() > data.1.block_area.bottom()
                    && data.1.block_area.overlaps_in_x(&focused_area)
            })
            .max_set_by(|a, b| a.1.area.bottom().cmp(&b.1.area.bottom()))
    }
    fn panes_directly_below(
        &self,
        focused_area: Rect,
    ) -> impl Iterator<Item = (&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(move |data| {
                data.1.focusable && focused_area.bottom() == data.1.block_area.top()
                    && data.1.block_area.overlaps_in_x(&focused_area)
            })
    }
    fn closest_panes_below(&self, focused_area: Rect) -> Vec<(&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(|data| {
                data.1.focusable && focused_area.bottom() < data.1.block_area.top()
                    && data.1.block_area.overlaps_in_x(&focused_area)
            })
            .min_set_by(|a, b| a.1.area.top().cmp(&b.1.area.top()))
    }
    fn panes_directly_left(
        &self,
        focused_area: Rect,
    ) -> impl Iterator<Item = (&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(move |data| {
                data.1.focusable && focused_area.left() == data.1.block_area.right()
                    && data.1.block_area.overlaps_in_y(&focused_area)
            })
    }
    fn closest_panes_left(&self, focused_area: Rect) -> Vec<(&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(|data| {
                data.1.focusable && focused_area.left() > data.1.block_area.right()
                    && data.1.block_area.overlaps_in_y(&focused_area)
            })
            .max_set_by(|a, b| a.1.area.left().cmp(&b.1.area.left()))
    }
    fn panes_directly_right(
        &self,
        focused_area: Rect,
    ) -> impl Iterator<Item = (&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(move |data| {
                data.1.focusable && focused_area.right() == data.1.block_area.left()
                    && data.1.block_area.overlaps_in_y(&focused_area)
            })
    }
    fn closest_panes_right(&self, focused_area: Rect) -> Vec<(&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(|data| {
                data.1.focusable && focused_area.right() < data.1.block_area.left()
                    && data.1.block_area.overlaps_in_y(&focused_area)
            })
            .min_set_by(|a, b| a.1.area.left().cmp(&b.1.area.left()))
    }
}
