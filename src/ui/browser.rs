use anyhow::Result;
use enum_map::EnumMap;
use ratatui::{prelude::Rect, widgets::ListState};

use crate::{
    ctx::Ctx,
    mpd::client::Client,
    shared::{
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::{Enqueue, MpdClientExt},
    },
    ui::{
        dirstack::{Dir, DirStack, DirStackItem, WalkDirStackItem},
        song_list::SongListCore,
        widgets::browser::BrowserArea,
    },
};

pub use crate::ui::song_list::MoveDirection;

/// The dir-stack list pane: a [`SongListCore`] over the stack's current
/// dir, plus the dir-stack-specific parts — three-column areas (previous /
/// current / preview), stack navigation (open descends, left pops), the
/// walk-based enqueue, lazy data fetching for empty directories, and the
/// previous/preview mouse areas.
///
/// After the Phase-1 consolidation (docs/design/Rewrite/ui-reuse-rewrite.md)
/// every dir-stack pane's [`SongListCore`] impl delegates the stack-aware
/// hooks (open/leave/enqueue/fetch/scrollbar/mouse) to this trait's
/// defaults; the shared `CommonAction` arms, list mouse handling,
/// filtering, marking and the context menu live in [`SongListCore`] once.
#[allow(unused)]
pub(in crate::ui) trait BrowserPane<T>: SongListCore<T, ListState>
where
    T: DirStackItem + std::fmt::Debug + Clone + Send + Sync + 'static,
{
    fn stack(&self) -> &DirStack<T, ListState>;
    fn stack_mut(&mut self) -> &mut DirStack<T, ListState>;
    fn browser_areas(&self) -> EnumMap<BrowserArea, Rect>;

    fn list(&self) -> &Dir<T, ListState> {
        self.stack().current()
    }

    fn list_mut(&mut self) -> &mut Dir<T, ListState> {
        self.stack_mut().current_mut()
    }

    fn scrollbar_area(&self) -> Option<Rect> {
        let areas = self.browser_areas();
        let scrollbar = areas[BrowserArea::Scrollbar];
        if scrollbar.width > 0 { Some(scrollbar) } else { None }
    }

    fn list_area(&self) -> Option<Rect> {
        Some(self.browser_areas()[BrowserArea::Current])
    }

    fn open(&mut self, autoplay: bool, ctx: &Ctx) -> Result<()> {
        let Some(selected) = self.stack().current().selected() else {
            log::error!("Failed to move deeper inside dir. Current value is None");
            return Ok(());
        };

        if selected.is_file() {
            let (items, hovered_song_idx) = BrowserPane::enqueue(
                self,
                self.stack()
                    .current()
                    .items
                    .iter()
                    // Only add songs here in case the directory contains combination of
                    // directories, playlists and songs to be able to use autoplay from the
                    // hovered song properly.
                    .filter(|item| item.is_file()),
            );
            if !items.is_empty() {
                let (position, autoplay) = if autoplay {
                    (crate::config::keys::actions::Position::Replace, crate::config::keys::actions::AutoplayKind::Hovered)
                } else {
                    (crate::config::keys::actions::Position::EndOfQueue, crate::config::keys::actions::AutoplayKind::None)
                };

                Client::resolve_and_enqueue(ctx, items, position, autoplay, None, hovered_song_idx);
            }
        } else {
            self.stack_mut().enter();
            ctx.render()?;
        }

        Ok(())
    }

    fn leave(&mut self, ctx: &Ctx) -> Result<()> {
        self.stack_mut().leave();
        BrowserPane::fetch_data_internal(self, ctx);
        ctx.render()?;
        Ok(())
    }

    fn fetch_data_internal(&mut self, ctx: &Ctx) -> Result<()> {
        // Only attempt to fetch for empty directories
        if self.stack().next_dir_items().is_none_or(|d| d.is_empty())
            && let Some(selected) = self.stack().current().selected()
            && !selected.is_file()
        {
            self.fetch_data(selected, ctx)
        } else {
            Ok(())
        }
    }

    fn enqueue<'a>(&self, items: impl Iterator<Item = &'a T>) -> (Vec<Enqueue>, Option<usize>) {
        let path = self.stack().path();
        let hovered = self.stack().current().selected();
        let (items, idx) = items
            .flat_map(|item| item.walk(self.stack(), path.clone()))
            .enumerate()
            .fold((Vec::new(), None), |mut acc, (idx, item)| {
                let filename = item.as_path().to_owned();
                if let Some(hovered) = hovered
                    && hovered.is_file()
                    && hovered.as_path() == filename
                {
                    acc.1 = Some(idx);
                }
                acc.0.push(Enqueue::File { path: filename });

                acc
            });

        (items, idx)
    }

    fn initial_playlist_name(&self, all: bool) -> Option<String> {
        if all {
            return self.stack().path().current_dir().map(|v| v.to_owned());
        }

        if !self.stack().current().marked().is_empty() {
            None
        } else if let Some(selected) = self.stack().current().selected()
            && !selected.is_file()
        {
            Some(selected.as_path().to_owned())
        } else {
            None
        }
    }

    /// The dir-stack mouse handler: previous-area click pops the stack,
    /// preview-area click opens into the preview, everything else falls
    /// through to the shared list handling (`SongListCore`).
    fn handle_stack_mouse_action(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if self.handle_scrollbar_interaction(event, ctx)? {
            return Ok(());
        }

        let areas = self.browser_areas();
        let prev_area = areas[BrowserArea::Previous];
        let preview_area = areas[BrowserArea::Preview];

        let position = event.into();
        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
                if prev_area.contains(position) =>
            {
                let clicked_row: usize = event.y.saturating_sub(prev_area.y).into();
                if let Some(prev_stack) = self.stack_mut().previous_mut() {
                    if let Some(idx_to_select) = prev_stack.state.get_at_rendered_row(clicked_row) {
                        prev_stack.select_idx(idx_to_select, ctx.config.scrolloff);
                    }
                    self.stack_mut().leave();
                    BrowserPane::fetch_data_internal(self, ctx);
                }
            }
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
                if preview_area.contains(position) =>
            {
                let clicked_row: usize = event.y.saturating_sub(preview_area.y).into();
                // Offset does not need to be accounted for since it is always
                // scrolled all the way to the top when going
                // deeper
                let idx_to_select = self.stack().next_dir_items().and_then(|preview| {
                    if clicked_row < preview.len() { Some(clicked_row) } else { None }
                });

                BrowserPane::open(self, false, ctx)?;
                self.stack_mut().current_mut().select_idx(idx_to_select.unwrap_or_default(), 0);

                BrowserPane::fetch_data_internal(self, ctx);
            }
            _ => {
                self.handle_list_mouse_action(event, ctx)?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod scrollbar_tests {
    use ratatui::layout::Rect;

    use crate::shared::mouse_event::{MouseEvent, MouseEventKind};

    #[test]
    fn test_mouse_event_in_scrollbar_area() {
        let scrollbar_area = Rect::new(29, 1, 1, 8);

        let inside_event = MouseEvent { kind: MouseEventKind::LeftClick, x: 29, y: 3 , ..Default::default() };
        assert!(scrollbar_area.contains(inside_event.into()));

        let outside_event = MouseEvent { kind: MouseEventKind::LeftClick, x: 28, y: 3 , ..Default::default() };
        assert!(!scrollbar_area.contains(outside_event.into()));
    }

    #[test]
    fn test_scrollbar_drag_events() {
        let scrollbar_area = Rect::new(29, 1, 1, 8);
        let drag_start = ratatui::layout::Position { x: 29, y: 1 };
        let drag_event = MouseEvent {
            kind: MouseEventKind::Drag { drag_start_position: drag_start },
            x: 29,
            y: 5,
        modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(scrollbar_area.contains(drag_event.into()));
        assert!(matches!(drag_event.kind, MouseEventKind::Drag { .. }));
    }
}
