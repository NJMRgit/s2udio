use std::{collections::HashMap, time::Instant};

use anyhow::{Context, Result};
use itertools::Itertools;
use ratatui::{Frame, layout::Rect, style::Style, widgets::Block};

use super::{Pane as _, PaneContainer, Panes, panes::pane_call};
use crate::{
    config::{
        keys::CommonAction,
        tabs::{PaneType, SizedPaneOrSplit},
    },
    ctx::Ctx,
    shared::{
        ext::{rect::RectExt, vec::VecExt},
        id::Id,
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
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
        let focused =
            panes.panes_iter().next().context("Tab needs at least one pane to be valid!")?.id;
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
        self.panes.for_each_pane_custom_data(
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
                let block = block.border_style(if focused.is_some_and(|p| p.id == pane.id) {
                    pane.border_active_style.unwrap_or_else(|| ctx.config.as_focused_border_style())
                } else {
                    pane.border_style.unwrap_or_else(|| ctx.config.as_border_style())
                });
                if let Some(bg_color) = bg_color {
                    frame
                        .render_widget(Block::default().style(Style::default().bg(bg_color)), area);
                }

                let mut pane_instance = pane_container.get_mut(&pane.pane, ctx)?;
                pane_call!(pane_instance, render(frame, area, ctx))?;
                frame.render_widget(block, block_area);
                // The queue/chapters toggle lives inline on the box's top
                // border; drawing it after the box keeps it visible.
                if let Panes::Queue(queue_pane) = &mut pane_instance {
                    queue_pane.render_toggle_on_border(frame, pane.borders, block_area, ctx);
                }
                Ok(())
            },
            &mut |block, block_area, background_color, frame| {
                if let Some(bg_color) = background_color {
                    frame.render_widget(
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
        let Some(focused) = self.panes.panes_iter().find(|pane| pane.id == self.focused) else {
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
            log::warn!(focused:? = self.focused, pane_areas:? = self.pane_data; "Tried to find focused pane area but it does not exist");
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
                    .and_then(|(id, _)| self.panes.panes_iter().find(|pane| pane.id == *id));

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
                    .and_then(|(id, _)| self.panes.panes_iter().find(|pane| pane.id == *id));

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
                    .and_then(|(id, _)| self.panes.panes_iter().find(|pane| pane.id == *id));

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
                    .and_then(|(id, _)| self.panes.panes_iter().find(|pane| pane.id == *id));

                if let Some(pane) = pane_to_focus {
                    self.set_focused(pane.id);
                }
                ctx.render()?;
            }
            Some(_) | None => {
                event.abandon();
                let Some(focused) = self.panes.panes_iter().find(|pane| pane.id == self.focused)
                else {
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
        let queue_pane_id =
            self.panes.panes_iter().find(|p| p.pane == PaneType::Queue).map(|p| p.id);
        let pane_id = {
            // The queue/chapters toggle row sits above the queue box (in the
            // spacer row, outside the box) — clicks there belong to the
            // queue pane.
            let on_toggle = queue_pane_id.is_some_and(|_| {
                matches!(
                    panes.get_mut(&PaneType::Queue, ctx),
                    Ok(Panes::Queue(q))
                        if q.toggle_areas.iter().any(|area| area.contains(position))
                )
            });
            let found = if on_toggle {
                queue_pane_id.map(|id| (id, self.pane_data.get(&id)))
            } else {
                self.pane_data
                    .iter()
                    .find(|(_, PaneData { area, .. })| area.contains(position))
                    .or_else(|| {
                        // Not inside any pane's content area: it may be on a
                        // pane's border. Route it to the pane whose full box
                        // it is on.
                        self.pane_data
                            .iter()
                            .find(|(_, PaneData { block_area, .. })| block_area.contains(position))
                    })
                    .map(|(id, data)| (*id, Some(data)))
            };
            let Some((pane_id, data)) = found.and_then(|(id, data)| data.map(|data| (id, data)))
            else {
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
        self.panes.for_each_pane(
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
            // The Queue pane is a tab's main list: when the layout also puts
            // other focusable panes above it (album art, lyrics), the queue
            // still gets the keyboard focus on open. Other tabs keep the
            // geometric default (top-left-most focusable pane).
            let pane_to_focus = self
                .panes
                .panes_iter()
                .find(|pane| pane.pane == PaneType::Queue && pane.is_focusable())
                .map(|pane| pane.id)
                .or_else(|| {
                    self.pane_data
                        .iter()
                        .filter(|(_, PaneData { focusable, .. })| *focusable)
                        .min_by(|(_, PaneData { area: a, .. }), (_, PaneData { area: b, .. })| {
                            a.left().cmp(&b.left()).then(a.top().cmp(&b.top()))
                        })
                        .and_then(|entry| self.panes.panes_iter().find(|pane| &pane.id == entry.0))
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
        self.panes.for_each_pane(
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

    fn panes_directly_above(&self, focused_area: Rect) -> impl Iterator<Item = (&Id, &PaneData)> {
        self.pane_data.iter().filter(move |data| {
            data.1.focusable
                && focused_area.top() == data.1.block_area.bottom()
                && data.1.block_area.overlaps_in_x(&focused_area)
        })
    }

    fn closest_panes_above(&self, focused_area: Rect) -> Vec<(&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(|data| {
                data.1.focusable
                    && focused_area.top() > data.1.block_area.bottom()
                    && data.1.block_area.overlaps_in_x(&focused_area)
            })
            .max_set_by(|a, b| a.1.area.bottom().cmp(&b.1.area.bottom()))
    }

    fn panes_directly_below(&self, focused_area: Rect) -> impl Iterator<Item = (&Id, &PaneData)> {
        self.pane_data.iter().filter(move |data| {
            data.1.focusable
                && focused_area.bottom() == data.1.block_area.top()
                && data.1.block_area.overlaps_in_x(&focused_area)
        })
    }

    fn closest_panes_below(&self, focused_area: Rect) -> Vec<(&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(|data| {
                data.1.focusable
                    && focused_area.bottom() < data.1.block_area.top()
                    && data.1.block_area.overlaps_in_x(&focused_area)
            })
            .min_set_by(|a, b| a.1.area.top().cmp(&b.1.area.top()))
    }

    fn panes_directly_left(&self, focused_area: Rect) -> impl Iterator<Item = (&Id, &PaneData)> {
        self.pane_data.iter().filter(move |data| {
            data.1.focusable
                && focused_area.left() == data.1.block_area.right()
                && data.1.block_area.overlaps_in_y(&focused_area)
        })
    }

    fn closest_panes_left(&self, focused_area: Rect) -> Vec<(&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(|data| {
                data.1.focusable
                    && focused_area.left() > data.1.block_area.right()
                    && data.1.block_area.overlaps_in_y(&focused_area)
            })
            .max_set_by(|a, b| a.1.area.left().cmp(&b.1.area.left()))
    }

    fn panes_directly_right(&self, focused_area: Rect) -> impl Iterator<Item = (&Id, &PaneData)> {
        self.pane_data.iter().filter(move |data| {
            data.1.focusable
                && focused_area.right() == data.1.block_area.left()
                && data.1.block_area.overlaps_in_y(&focused_area)
        })
    }

    fn closest_panes_right(&self, focused_area: Rect) -> Vec<(&Id, &PaneData)> {
        self.pane_data
            .iter()
            .filter(|data| {
                data.1.focusable
                    && focused_area.right() < data.1.block_area.left()
                    && data.1.block_area.overlaps_in_y(&focused_area)
            })
            .min_set_by(|a, b| a.1.area.left().cmp(&b.1.area.left()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{
        config::tabs::{Pane as ConfigPane, PaneType, SizedPaneOrSplit, SizedSubPane},
        mpd::commands::Song,
        shared::chapters::Chapter,
        ui::panes::PaneContainer,
    };

    fn pane(pane_type: PaneType) -> SizedPaneOrSplit {
        SizedPaneOrSplit::Pane(ConfigPane {
            pane: pane_type,
            background_color: None,
            borders: ratatui::widgets::Borders::NONE,
            border_style: None,
            border_active_style: None,
            border_title: Vec::new(),
            border_title_position: ratatui::widgets::TitlePosition::Top,
            border_title_alignment: ratatui::layout::Alignment::Left,
            border_symbols: crate::config::theme::borders::BorderSymbols::default(),
            id: crate::shared::id::new(),
        })
    }

    fn sub(pane: SizedPaneOrSplit, size: &str) -> SizedSubPane {
        SizedSubPane {
            size: size.parse().unwrap(),
            collapse_below: None,
            shrink_below: None,
            window_sizes: Vec::new(),
            pane,
        }
    }

    fn split(
        panes: Vec<SizedSubPane>,
        direction: ratatui::layout::Direction,
        borders: ratatui::widgets::Borders,
    ) -> SizedPaneOrSplit {
        SizedPaneOrSplit::Split {
            background_color: None,
            borders,
            border_style: None,
            border_title: Vec::new(),
            border_title_position: ratatui::widgets::TitlePosition::Top,
            border_title_alignment: ratatui::layout::Alignment::Left,
            border_symbols: crate::config::theme::borders::BorderSymbols::default(),
            direction,
            panes,
        }
    }

    /// The merged queue box from the user's live config: a 1-row spacer
    /// (the toggle row), then one box with ALL borders holding the header
    /// row + divider and the queue list.
    fn queue_tab_panes() -> SizedPaneOrSplit {
        use ratatui::layout::Direction::{Horizontal, Vertical};
        split(
            vec![
                sub(pane(PaneType::Empty), "1"),
                sub(
                    split(
                        vec![
                            sub(
                                split(
                                    vec![
                                        sub(pane(PaneType::Empty), "1"),
                                        sub(pane(PaneType::QueueHeader()), "100%"),
                                    ],
                                    Horizontal,
                                    ratatui::widgets::Borders::NONE,
                                ),
                                "2",
                            ),
                            sub(
                                split(
                                    vec![
                                        sub(pane(PaneType::Empty), "1"),
                                        sub(pane(PaneType::Queue), "100%"),
                                        sub(pane(PaneType::Empty), "2"),
                                    ],
                                    Horizontal,
                                    ratatui::widgets::Borders::NONE,
                                ),
                                "100%",
                            ),
                        ],
                        Vertical,
                        ratatui::widgets::Borders::ALL,
                    ),
                    "100%",
                ),
            ],
            Vertical,
            ratatui::widgets::Borders::NONE,
        )
    }

    /// A Queue tab whose top-left corner holds another focusable pane (like the
    /// user's live layout, where the album-art/lyrics row sits above the queue
    /// box). `before_show` must land the initial keyboard focus on the Queue
    /// pane — the pane the queue's navigation keys drive — even though that
    /// other pane is focusable and geometrically first.
    fn queue_tab_with_art_and_lyrics() -> SizedPaneOrSplit {
        use ratatui::layout::Direction::{Horizontal, Vertical};
        split(
            vec![
                sub(pane(PaneType::Lyrics), "20"),
                sub(pane(PaneType::Empty), "1"),
                sub(
                    split(
                        vec![
                            sub(
                                split(
                                    vec![
                                        sub(pane(PaneType::Empty), "1"),
                                        sub(pane(PaneType::QueueHeader()), "100%"),
                                    ],
                                    Horizontal,
                                    ratatui::widgets::Borders::NONE,
                                ),
                                "2",
                            ),
                            sub(
                                split(
                                    vec![
                                        sub(pane(PaneType::Empty), "1"),
                                        sub(pane(PaneType::Queue), "100%"),
                                        sub(pane(PaneType::Empty), "2"),
                                    ],
                                    Horizontal,
                                    ratatui::widgets::Borders::NONE,
                                ),
                                "100%",
                            ),
                        ],
                        Vertical,
                        ratatui::widgets::Borders::ALL,
                    ),
                    "100%",
                ),
            ],
            Vertical,
            ratatui::widgets::Borders::NONE,
        )
    }

    #[test]
    fn queue_tab_initial_focus_lands_on_the_queue_pane() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        // before_show on the focusable panes above the queue (album art,
        // lyrics) requests art/search work on the work and client channels:
        // keep cloned receivers alive so those sends succeed (the fixture
        // drops the originals it is handed).
        let _work_rx = work_rx.clone();
        let _client_rx = client_rx.clone();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx, work_rx),
            (client_tx, client_rx),
        );
        let mut panes = PaneContainer::new(&ctx).unwrap();
        let mut screen = TabScreen::new(queue_tab_with_art_and_lyrics()).unwrap();
        let area = Rect::new(0, 0, 160, 50);
        screen.before_show(&mut panes, area, &ctx).unwrap();

        let focused_pane = screen
            .panes
            .panes_iter()
            .find(|pane| pane.id == screen.focused)
            .map(|pane| pane.pane.clone());
        assert_eq!(
            focused_pane,
            Some(PaneType::Queue),
            "opening the Queue tab must focus the queue list, not the panes above it"
        );
    }

    #[test]
    fn queue_toggle_renders_above_the_merged_queue_box() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        // A current track with chapter markers.
        ctx.queue =
            vec![Song { id: 1, file: "/mnt/music/a.flac".to_owned(), ..Default::default() }];
        ctx.status.songid = Some(1);
        ctx.status.state = crate::mpd::commands::State::Play;
        ctx.chapters.borrow_mut().insert(
            "/mnt/music/a.flac".to_owned(),
            vec![Chapter { title: "Intro".into(), start_secs: 0.0, end_secs: 60.0 }],
        );
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Chapters);

        let mut panes = PaneContainer::new(&ctx).unwrap();
        let mut screen = TabScreen::new(queue_tab_panes()).unwrap();
        let area = Rect::new(0, 0, 100, 60);
        screen.before_show(&mut panes, area, &ctx).unwrap();

        let backend = ratatui::backend::TestBackend::new(100, 60);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| screen.render(&mut panes, frame, area, 60, &ctx).unwrap()).unwrap();
        let buf = terminal.backend().buffer();

        // The toggle appears exactly once, on its own row directly above
        // the box's top border (`╭` below it, no border glyphs on its row).
        let rows: Vec<String> = (0..60u16)
            .map(|y| (0..100).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect();
        let toggle_rows: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains("Audio") && line.contains("Chapters"))
            .map(|(y, _)| y)
            .collect();
        assert_eq!(toggle_rows.len(), 1, "expected exactly one toggle row");
        let toggle_y = toggle_rows[0];
        // The box top border sits right below the toggle row.
        assert_eq!(buf[(1, toggle_y as u16 + 1)].symbol(), "─");
        assert!(
            matches!(buf[(0, toggle_y as u16 + 1)].symbol(), "╭" | "┌"),
            "box top border corner missing below the toggle row"
        );
        // The box holds the header labels + divider + the list, all inside
        // one box.
        let header_y = toggle_y + 2;
        let line = &rows[header_y];
        assert!(
            line.contains("Chapter") && line.contains("Time") && line.contains("Duration"),
            "chapters header missing inside the box: {line}"
        );
        assert!(rows[header_y + 1].contains("─"), "divider row missing under the header");
    }

    #[test]
    fn clicking_each_toggle_segment_switches_the_queue_list() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        // A current track with chapter markers (Chapters segment shows).
        ctx.queue =
            vec![Song { id: 1, file: "/mnt/music/a.flac".to_owned(), ..Default::default() }];
        ctx.status.songid = Some(1);
        ctx.status.state = crate::mpd::commands::State::Play;
        ctx.chapters.borrow_mut().insert(
            "/mnt/music/a.flac".to_owned(),
            vec![Chapter { title: "Intro".into(), start_secs: 0.0, end_secs: 60.0 }],
        );
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Audio);

        let mut panes = PaneContainer::new(&ctx).unwrap();
        let mut screen = TabScreen::new(queue_tab_panes()).unwrap();
        let area = Rect::new(0, 0, 100, 60);
        screen.before_show(&mut panes, area, &ctx).unwrap();

        let backend = ratatui::backend::TestBackend::new(100, 60);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| screen.render(&mut panes, frame, area, 60, &ctx).unwrap()).unwrap();

        // The queue pane's toggle areas are wired up after the render.
        let Panes::Queue(queue_pane) =
            panes.get_mut(&crate::config::tabs::PaneType::Queue, &ctx).unwrap()
        else {
            panic!("no queue pane");
        };
        let chapters_area = queue_pane.toggle_areas[2];
        let video_area = queue_pane.toggle_areas[1];
        let audio_area = queue_pane.toggle_areas[0];
        // All three segments are on the row above the box.
        assert_eq!(chapters_area.y, audio_area.y);
        assert!(chapters_area.width > 0);

        // Clicks route through TabScreen to the queue pane and switch the
        // list: Chapters (the previously-unreachable segment), then back.
        let click =
            |screen: &mut TabScreen, panes: &mut PaneContainer, ctx: &mut Ctx, area: Rect| {
                screen
                    .handle_mouse_event(
                        panes,
                        MouseEvent {
                            x: area.x + 2,
                            y: area.y,
                            kind: MouseEventKind::LeftClick,
                            modifiers: crossterm::event::KeyModifiers::NONE,
                        },
                        ctx,
                    )
                    .unwrap()
            };
        click(&mut screen, &mut panes, &mut ctx, chapters_area);
        assert_eq!(ctx.queue_tab.get(), crate::ctx::QueueTabMode::Chapters);
        click(&mut screen, &mut panes, &mut ctx, audio_area);
        assert_eq!(ctx.queue_tab.get(), crate::ctx::QueueTabMode::Audio);
        click(&mut screen, &mut panes, &mut ctx, video_area);
        assert_eq!(ctx.queue_tab.get(), crate::ctx::QueueTabMode::Video);
    }
}
