use std::time::{Duration, Instant};
use crossterm::event::{
    MouseButton, MouseEvent as CTMouseEvent, MouseEventKind as CTMouseEventKind,
};
use ratatui::layout::{Position, Rect};
const DOUBLE_CLICK_TIMEOUT: Duration = Duration::from_millis(500);
#[derive(Debug, Default, Clone, Copy)]
pub struct MouseEventTracker {
    last_left_click: Option<TimedMouseEvent>,
    drag_start_position: Option<Position>,
}
impl MouseEventTracker {
    pub fn track_and_get(&mut self, event: CTMouseEvent) -> Option<MouseEvent> {
        self.crossterm_ev_to_mouse_event(event)
            .inspect(|ev| match ev.kind {
                MouseEventKind::LeftClick => {
                    self.last_left_click = (*ev).into();
                    self.drag_start_position = Some((*ev).into());
                }
                MouseEventKind::DoubleClick => {
                    self.last_left_click = None;
                    self.drag_start_position = None;
                }
                MouseEventKind::Drag { .. } => {}
                _ => {
                    self.drag_start_position = None;
                }
            })
    }
    pub fn crossterm_ev_to_mouse_event(
        &self,
        value: CTMouseEvent,
    ) -> Option<MouseEvent> {
        let x = value.column;
        let y = value.row;
        match value.kind {
            CTMouseEventKind::Down(MouseButton::Left) => {
                if self.last_left_click.is_some_and(|c| c.is_doubled(x, y)) {
                    Some(MouseEvent {
                        x,
                        y,
                        kind: MouseEventKind::DoubleClick,
                        modifiers: value.modifiers,
                    })
                } else {
                    Some(MouseEvent {
                        x,
                        y,
                        kind: MouseEventKind::LeftClick,
                        modifiers: value.modifiers,
                    })
                }
            }
            CTMouseEventKind::Down(MouseButton::Right) => {
                Some(MouseEvent {
                    x,
                    y,
                    kind: MouseEventKind::RightClick,
                    modifiers: value.modifiers,
                })
            }
            CTMouseEventKind::Down(MouseButton::Middle) => {
                Some(MouseEvent {
                    x,
                    y,
                    kind: MouseEventKind::MiddleClick,
                    modifiers: value.modifiers,
                })
            }
            CTMouseEventKind::ScrollDown => {
                Some(MouseEvent {
                    x,
                    y,
                    kind: MouseEventKind::ScrollDown,
                    modifiers: value.modifiers,
                })
            }
            CTMouseEventKind::ScrollUp => {
                Some(MouseEvent {
                    x,
                    y,
                    kind: MouseEventKind::ScrollUp,
                    modifiers: value.modifiers,
                })
            }
            CTMouseEventKind::Up(MouseButton::Left) => {
                Some(MouseEvent {
                    x,
                    y,
                    kind: MouseEventKind::LeftRelease,
                    modifiers: value.modifiers,
                })
            }
            CTMouseEventKind::Up(_) => None,
            CTMouseEventKind::Drag(MouseButton::Left) => {
                Some(MouseEvent {
                    x,
                    y,
                    kind: MouseEventKind::Drag {
                        drag_start_position: self
                            .drag_start_position
                            .unwrap_or(Position { x, y }),
                    },
                    modifiers: value.modifiers,
                })
            }
            CTMouseEventKind::Drag(_) => None,
            CTMouseEventKind::Moved => {
                Some(MouseEvent {
                    x,
                    y,
                    kind: MouseEventKind::Moved,
                    modifiers: value.modifiers,
                })
            }
            CTMouseEventKind::ScrollLeft => None,
            CTMouseEventKind::ScrollRight => None,
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    pub x: u16,
    pub y: u16,
    pub kind: MouseEventKind,
    /// Key modifiers held while the event occurred (ctrl/shift/alt).
    pub modifiers: crossterm::event::KeyModifiers,
}
impl Default for MouseEvent {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            kind: MouseEventKind::LeftClick,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub enum MouseEventKind {
    LeftClick,
    DoubleClick,
    MiddleClick,
    RightClick,
    ScrollDown,
    ScrollUp,
    /// The pointer moved to a new cell with no button held. Consumed by the
    /// UI to update the hover position (drives the mouseover effects); it is
    /// never dispatched to panes like a real interaction.
    Moved,
    /// The left button was released. Pane buttons use it to end the
    /// pressed-while-held visual (the `⭘` marker) — the marker shows only
    /// between `LeftClick` and `LeftRelease`, never persistently.
    LeftRelease,
    Drag { drag_start_position: Position },
}
#[derive(Debug, Clone, Copy)]
pub struct TimedMouseEvent {
    x: u16,
    y: u16,
    time: Instant,
}
impl From<MouseEvent> for Option<TimedMouseEvent> {
    fn from(value: MouseEvent) -> Option<TimedMouseEvent> {
        if matches!(value.kind, MouseEventKind::LeftClick) {
            Some(TimedMouseEvent {
                time: Instant::now(),
                x: value.x,
                y: value.y,
            })
        } else {
            None
        }
    }
}
impl TimedMouseEvent {
    pub fn is_doubled(&self, x: u16, y: u16) -> bool {
        if self.x != x || self.y != y {
            return false;
        }
        self.time.elapsed() < DOUBLE_CLICK_TIMEOUT
    }
}
impl From<MouseEvent> for Position {
    fn from(value: MouseEvent) -> Self {
        Self { x: value.x, y: value.y }
    }
}
/// check if a mouse event should interact with the scrollbar, considering drag
/// start position
fn is_scrollbar_interaction(event: MouseEvent, scrollbar_area: Rect) -> bool {
    if scrollbar_area.height == 0 {
        return false;
    }
    let scrollbar_x = scrollbar_area.right().saturating_sub(1);
    match event.kind {
        MouseEventKind::LeftClick => {
            event.x == scrollbar_x && scrollbar_area.contains(event.into())
        }
        MouseEventKind::Drag { drag_start_position } => {
            (drag_start_position.x == scrollbar_x
                && scrollbar_area.contains(drag_start_position))
                || (event.x == scrollbar_x && scrollbar_area.contains(event.into()))
        }
        _ => false,
    }
}
/// The on-screen geometry of a vertical scrollbar track, matching how
/// ratatui's `Scrollbar` places the thumb. `content_len` is the number of
/// scrollable positions (max offset + 1), `viewport_len` how many rows are
/// visible, `position` the current offset, `begin_len`/`end_len` the width
/// of the begin/end arrow symbols (0 when they are disabled).
#[derive(Debug, Clone, Copy)]
pub struct ScrollbarGeometry {
    pub track_top: u16,
    pub track_len: u16,
    pub thumb_size: u16,
    pub thumb_offset: u16,
}
impl ScrollbarGeometry {
    /// Compute the track + thumb geometry the way ratatui's Scrollbar widget
    /// would render it for the given state, so the drag math can keep the
    /// thumb glued to the pointer.
    pub fn vertical(
        area: Rect,
        content_len: usize,
        viewport_len: usize,
        position: usize,
        begin_len: u16,
        end_len: u16,
    ) -> Self {
        let track_len = area.height.saturating_sub(begin_len).saturating_sub(end_len);
        if track_len == 0 {
            return Self {
                track_top: area.y,
                track_len: 0,
                thumb_size: 0,
                thumb_offset: 0,
            };
        }
        let max_position = content_len.saturating_sub(1);
        let start_position = position.min(max_position);
        let max_viewport_position = max_position.saturating_add(viewport_len);
        let track_len_us = track_len as usize;
        if max_viewport_position == 0 {
            return Self {
                track_top: area.y,
                track_len,
                thumb_size: track_len,
                thumb_offset: 0,
            };
        }
        let thumb_size = rounding_divide(
                viewport_len * track_len_us,
                max_viewport_position,
            )
            .clamp(1, track_len_us);
        let thumb_offset = rounding_divide(
                start_position * track_len_us,
                max_viewport_position,
            )
            .clamp(0, track_len_us.saturating_sub(1));
        Self {
            track_top: area.y + begin_len,
            track_len,
            thumb_size: thumb_size as u16,
            thumb_offset: thumb_offset as u16,
        }
    }
    /// Absolute y of the first thumb row.
    pub fn thumb_top(&self) -> u16 {
        self.track_top.saturating_add(self.thumb_offset)
    }
    /// The maximum y the thumb's first row can reach while staying inside
    /// the track.
    pub fn thumb_top_max(&self) -> u16 {
        self.track_top.saturating_add(self.track_len.saturating_sub(self.thumb_size))
    }
}
const fn rounding_divide(numerator: usize, denominator: usize) -> usize {
    (numerator + denominator / 2) / denominator
}
/// Tracks a scrollbar drag so the thumb follows the pointer 1:1 instead of
/// the pointer position being mapped directly onto the whole track (which
/// made the thumb move *less* than the mouse). A drag is started by a
/// left-click on the thumb; a click on the track still jumps.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScrollbarDrag {
    /// How far below the thumb's first row the pointer was when the drag
    /// started; `None` when no drag is in progress.
    grab_offset: Option<u16>,
}
impl ScrollbarDrag {
    /// Feed a mouse event. Returns the scroll fraction (0.0..=1.0) to jump
    /// or drag to, or `None` when the event is not a scrollbar interaction
    /// (or is the grab itself, which only anchors the thumb). `begin_len` /
    /// `end_len` are the display widths of the begin/end arrow symbols (0
    /// when they are disabled), matching how `as_styled_scrollbar` maps the
    /// theme's scrollbar symbols.
    pub fn handle(
        &mut self,
        event: MouseEvent,
        area: Rect,
        content_len: usize,
        viewport_len: usize,
        position: usize,
        begin_len: u16,
        end_len: u16,
    ) -> Option<f64> {
        let Some(geometry) = (area.height > 0)
            .then(|| {
                ScrollbarGeometry::vertical(
                    area,
                    content_len,
                    viewport_len,
                    position,
                    begin_len,
                    end_len,
                )
            }) else {
            return None;
        };
        if geometry.track_len == 0 || !is_scrollbar_interaction(event, area) {
            return None;
        }
        match event.kind {
            MouseEventKind::LeftClick => {
                let y = event.y;
                let thumb_top = geometry.thumb_top();
                if y >= thumb_top && y < thumb_top + geometry.thumb_size {
                    self.grab_offset = Some(y.saturating_sub(thumb_top));
                    None
                } else {
                    self.grab_offset = Some(0);
                    Some(Self::fraction_for_y(y, &geometry))
                }
            }
            MouseEventKind::Drag { .. } => {
                if self.grab_offset.is_none() {
                    return None;
                }
                let target_top = event
                    .y
                    .saturating_sub(self.grab_offset.unwrap_or(0))
                    .clamp(geometry.track_top, geometry.thumb_top_max());
                let travel = geometry.track_len.saturating_sub(geometry.thumb_size);
                if travel == 0 {
                    return Some(0.0);
                }
                Some(
                    (f64::from(target_top.saturating_sub(geometry.track_top))
                        / f64::from(travel))
                        .clamp(0.0, 1.0),
                )
            }
            _ => {
                self.grab_offset = None;
                None
            }
        }
    }
    /// The scroll fraction for a pointer row within the track, used for
    /// track clicks (the thumb jumps so its first row is under the pointer).
    fn fraction_for_y(y: u16, geometry: &ScrollbarGeometry) -> f64 {
        let travel = geometry.track_len.saturating_sub(geometry.thumb_size);
        if travel == 0 {
            return 0.0;
        }
        (f64::from(y.saturating_sub(geometry.track_top).min(travel)) / f64::from(travel))
            .clamp(0.0, 1.0)
    }
}
