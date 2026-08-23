use std::time::Instant;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crate::{ctx::Ctx, mpd::mpd_client::MpdClient, shared::id::Id};
/// Seek direction latched while the user holds Space + an arrow key in
/// interactive-seek mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekDir {
    Back,
    Forward,
    Up,
    Down,
}
/// Keyboard control of the seekbar (entered with Ctrl+Tab on the Queue
/// tab). While `focused`, the seekbar owns the keyboard:
/// - `a` / `d` / left / right move the seek cursor (±2 s, clamped to the
///   track) without seeking.
/// - Tapping Space or Enter seeks to the cursor position and returns
///   keyboard control to the queue list.
/// - Holding Space enters interactive seek mode: arrows seek relative to
///   the current position (audio: left/right ±2 s, up/down ±5 s; video:
///   up/down ±5 s, left/right frame by frame). A held direction
///   auto-repeats once per second.
pub struct SeekbarState {
    pub focused: bool,
    /// The seek cursor, in seconds (0..=duration).
    cursor: f64,
    /// Track duration in seconds at focus time (for clamping the cursor).
    duration: f64,
    /// Interactive mode (Space held + an arrow key): arrows seek instead
    /// of moving the cursor.
    interactive: bool,
    /// Space is held down.
    space_down: bool,
    /// Space is down and no repeat has been seen yet: releasing within the
    /// repeat window counts as a tap (seek + exit).
    tap_pending: bool,
    /// The direction latched by the last arrow while in interactive mode.
    last_dir: Option<SeekDir>,
    /// Time of the last interactive seek (1 s repeat throttle).
    last_seek: Instant,
    /// Scheduler id of the pending release-check one-shot.
    release_check: Option<Id>,
}
impl Default for SeekbarState {
    fn default() -> Self {
        Self {
            focused: false,
            cursor: 0.0,
            duration: 0.0,
            interactive: false,
            space_down: false,
            tap_pending: false,
            last_dir: None,
            last_seek: Instant::now(),
            release_check: None,
        }
    }
}
/// Absolute seek (used by tap / Enter): mpv when it is the UI source, else
/// MPD.
fn seek_absolute(ctx: &Ctx, seconds: f64) {
    if crate::core::mpv::mpv_is_ui_source(ctx)
        && let Some(socket) = ctx.mpv.socket.clone()
    {
        crate::core::mpv::mpv_seek(&socket, seconds);
        return;
    }
    ctx.command(move |client| {
        use crate::mpd::mpd_client::ValueChange;
        client.seek_current(ValueChange::Set(seconds.max(0.0) as u32))?;
        Ok(())
    });
}
/// Relative seek: mpv via its relative seek command, MPD via seekcur ±N.
fn seek_relative(ctx: &Ctx, delta_seconds: f64) {
    if crate::core::mpv::mpv_is_ui_source(ctx)
        && let Some(socket) = ctx.mpv.socket.clone()
    {
        crate::core::mpv::mpv_seek_relative(&socket, delta_seconds);
        return;
    }
    ctx.command(move |client| {
        use crate::mpd::mpd_client::ValueChange;
        if delta_seconds < 0.0 {
            client.seek_current(ValueChange::Decrease((-delta_seconds).round() as u32))?;
        } else {
            client.seek_current(ValueChange::Increase(delta_seconds.round() as u32))?;
        }
        Ok(())
    });
}
fn seek_frame(ctx: &Ctx, forward: bool) {
    if crate::core::mpv::mpv_is_ui_source(ctx)
        && let Some(socket) = ctx.mpv.socket.clone()
    {
        crate::core::mpv::mpv_frame_step(&socket, forward);
    }
}
/// The current playback position in seconds (the source the seekbar shows).
pub fn playback_position(ctx: &Ctx) -> f64 {
    if crate::core::mpv::mpv_is_ui_source(ctx) {
        ctx.mpv.position
    } else {
        ctx.status.elapsed.as_secs_f64()
    }
}
/// Exit seekbar keyboard control without seeking.
pub fn clear(ctx: &Ctx) {
    let mut state = ctx.seekbar.borrow_mut();
    cancel_release_check_state(&mut state, ctx);
    state.focused = false;
    state.interactive = false;
    state.space_down = false;
    state.tap_pending = false;
    state.last_dir = None;
    state.release_check = None;
}
pub fn is_focused(ctx: &Ctx) -> bool {
    ctx.seekbar.borrow().focused
}
/// The seek cursor as a fraction of the track (for the progress bar).
pub fn cursor_fraction(ctx: &Ctx) -> Option<f32> {
    let state = ctx.seekbar.borrow();
    if !state.focused || state.duration <= 0.0 {
        return None;
    }
    Some((state.cursor / state.duration) as f32)
}
/// Cancel the pending release-check one-shot (borrow-free: the caller
/// already holds the state's RefMut).
fn cancel_release_check_state(state: &mut SeekbarState, ctx: &Ctx) {
    if let Some(id) = state.release_check.take() {
        ctx.scheduler.cancel(id);
    }
}
/// Schedule the release-check fallback: terminals without release events
/// send only Press/Repeat, so a Space that stops repeating within the
/// window is treated as released (tap or exit-interactive).
fn schedule_release_check_state(state: &mut SeekbarState, ctx: &Ctx) {
    cancel_release_check_state(state, ctx);
    let id = crate::shared::id::new();
    state.release_check = Some(id);
    ctx.scheduler
        .schedule_replace(
            id,
            std::time::Duration::from_millis(300),
            move |(tx, _)| {
                Ok(
                    tx
                        .send(
                            crate::shared::events::AppEvent::UiEvent(
                                crate::ui::UiAppEvent::SeekbarReleaseCheck,
                            ),
                        )?,
                )
            },
        );
}
/// Fired by the scheduler (or a real release event): decide tap vs hold.
pub fn on_release_check(ctx: &Ctx) {
    let mut state = ctx.seekbar.borrow_mut();
    if !state.focused {
        return;
    }
    state.release_check = None;
    let tap = state.space_down && state.tap_pending;
    let interactive = state.interactive;
    state.space_down = false;
    state.interactive = false;
    state.tap_pending = false;
    state.last_dir = None;
    if interactive {
        state.cursor = if state.duration > 0.0 {
            playback_position(ctx).clamp(0.0, state.duration)
        } else {
            state.cursor
        };
        drop(state);
        ctx.render().ok();
    } else if tap {
        let seconds = state.cursor;
        drop(state);
        seek_absolute(ctx, seconds);
        clear(ctx);
        ctx.render().ok();
    }
}
/// Handle a raw key event while the seekbar owns the keyboard. Returns
/// `true` when the event was consumed.
pub fn handle_key(ctx: &Ctx, key: KeyEvent) -> bool {
    use KeyEventKind as K;
    let code = key.code;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let is_nav = code == KeyCode::Left || code == KeyCode::Right || code == KeyCode::Up
        || code == KeyCode::Down || code == KeyCode::Char('a')
        || code == KeyCode::Char('d');
    let is_space = code == KeyCode::Char(' ') && !ctrl;
    let is_enter = code == KeyCode::Enter;
    let is_esc = code == KeyCode::Esc;
    if !is_nav && !is_space && !is_enter && !is_esc {
        return false;
    }
    let mut state = ctx.seekbar.borrow_mut();
    if !state.focused {
        return false;
    }
    match key.kind {
        K::Release => {
            if is_space {
                let tap = state.space_down && state.tap_pending;
                let interactive = state.interactive;
                state.space_down = false;
                state.interactive = false;
                state.tap_pending = false;
                state.last_dir = None;
                cancel_release_check_state(&mut state, ctx);
                if interactive {
                    state.cursor = if state.duration > 0.0 {
                        playback_position(ctx).clamp(0.0, state.duration)
                    } else {
                        state.cursor
                    };
                    drop(state);
                    ctx.render().ok();
                } else if tap {
                    let seconds = state.cursor;
                    drop(state);
                    seek_absolute(ctx, seconds);
                    clear(ctx);
                    ctx.render().ok();
                }
                return true;
            }
            if is_nav && state.interactive {
                state.last_dir = None;
                return true;
            }
            return true;
        }
        K::Repeat => {
            if is_space {
                state.space_down = true;
                state.tap_pending = false;
                schedule_release_check_state(&mut state, ctx);
                return true;
            }
            if is_nav && state.interactive {
                if let Some(dir) = state.last_dir
                    && matches!(dir_for(code), Some(d) if d == dir)
                    && state.last_seek.elapsed() >= std::time::Duration::from_secs(1)
                {
                    state.last_seek = Instant::now();
                    interactive_seek(ctx, &mut state, dir);
                    drop(state);
                    ctx.render().ok();
                }
                return true;
            }
            if is_nav && !state.space_down {
                if let Some(delta) = cursor_delta(code) {
                    let new = (state.cursor + delta).clamp(0.0, state.duration.max(0.0));
                    if (new - state.cursor).abs() > f64::EPSILON {
                        state.cursor = new;
                        drop(state);
                        ctx.render().ok();
                    }
                }
                return true;
            }
            return true;
        }
        K::Press => {
            if is_esc {
                drop(state);
                clear(ctx);
                ctx.render().ok();
                return true;
            }
            if is_enter {
                let seconds = state.cursor;
                drop(state);
                seek_absolute(ctx, seconds);
                clear(ctx);
                ctx.render().ok();
                return true;
            }
            if is_space {
                if state.space_down {
                    state.tap_pending = false;
                    schedule_release_check_state(&mut state, ctx);
                } else {
                    state.space_down = true;
                    state.tap_pending = true;
                    schedule_release_check_state(&mut state, ctx);
                }
                return true;
            }
            if is_nav {
                if state.space_down {
                    if let Some(dir) = dir_for(code) {
                        if state.interactive && state.last_dir == Some(dir) {
                            if state.last_seek.elapsed()
                                >= std::time::Duration::from_secs(1)
                            {
                                state.last_seek = Instant::now();
                                interactive_seek(ctx, &mut state, dir);
                                drop(state);
                                ctx.render().ok();
                            }
                        } else {
                            state.interactive = true;
                            state.tap_pending = false;
                            state.last_dir = Some(dir);
                            state.last_seek = Instant::now();
                            cancel_release_check_state(&mut state, ctx);
                            interactive_seek(ctx, &mut state, dir);
                            drop(state);
                            ctx.render().ok();
                        }
                    }
                    return true;
                }
                if let Some(delta) = cursor_delta(code) {
                    let new = (state.cursor + delta).clamp(0.0, state.duration.max(0.0));
                    if (new - state.cursor).abs() > f64::EPSILON {
                        state.cursor = new;
                        drop(state);
                        ctx.render().ok();
                    }
                }
                return true;
            }
            true
        }
    }
}
/// Map a nav key to a seek direction.
fn dir_for(code: KeyCode) -> Option<SeekDir> {
    match code {
        KeyCode::Left | KeyCode::Char('a') => Some(SeekDir::Back),
        KeyCode::Right | KeyCode::Char('d') => Some(SeekDir::Forward),
        KeyCode::Up => Some(SeekDir::Up),
        KeyCode::Down => Some(SeekDir::Down),
        _ => None,
    }
}
/// Cursor movement in non-interactive mode (±2 s per step).
fn cursor_delta(code: KeyCode) -> Option<f64> {
    match code {
        KeyCode::Left | KeyCode::Char('a') => Some(-2.0),
        KeyCode::Right | KeyCode::Char('d') => Some(2.0),
        _ => None,
    }
}
/// Perform one interactive seek in the latched direction and update the
/// cursor so the bar stays honest. The caller renders after dropping the
/// state borrow (render re-reads the seekbar state).
fn interactive_seek(ctx: &Ctx, state: &mut SeekbarState, dir: SeekDir) {
    let video = crate::core::mpv::mpv_is_ui_source(ctx);
    let delta = match (video, dir) {
        (false, SeekDir::Back) => -2.0,
        (false, SeekDir::Forward) => 2.0,
        (false, SeekDir::Down) => -5.0,
        (false, SeekDir::Up) => 5.0,
        (true, SeekDir::Up) => 5.0,
        (true, SeekDir::Down) => -5.0,
        (true, SeekDir::Forward) => {
            seek_frame(ctx, true);
            return;
        }
        (true, SeekDir::Back) => {
            seek_frame(ctx, false);
            return;
        }
    };
    seek_relative(ctx, delta);
    if state.duration > 0.0 {
        state.cursor = (state.cursor + delta).clamp(0.0, state.duration);
    }
}
