//! One shared animation clock for the whole UI.
//!
//! Every surface that needs fresh frames goes through [`Animator`]: while it
//! renders, a pane asks for a frame (motion — "draw me again on the next budget
//! tick") or for a stepped cadence (a spinner glyph that advances every 120 ms).
//! One scheduler job — never one per surface — is armed with the shortest
//! interval any of them asked for, and it is cancelled the moment nothing asks.
//!
//! Why this exists:
//!
//! * **One tick.** Four independent one-shot ticks used to race the frame
//!   budget (the queue's Duration spinner, the Jellyfin loading indicator, the
//!   controls `▶▶` spinner and the torrent picker), each with its own job id and
//!   its own cancel path. They are one job now: cancelling is the absence of a
//!   request, and `max_fps` is enforced in exactly one place.
//! * **Time is a parameter.** [`Anim::sample`] takes `now` instead of reading
//!   the clock, so a surface's animation is decided by the frame that draws it
//!   (not by how often a job happens to fire) and every curve is testable
//!   without sleeping.
//!
//! The clock never renders. It says *when the next frame is due*; the event
//! loop owns the pacing (`min_frame_duration`) and, in continuous mode, renders
//! on every budget tick regardless.

use std::{
    cell::Cell,
    time::{Duration, Instant},
};

use crate::shared::id::{self, Id};

/// The highest `[max_fps]` the frame budget honours.
pub(crate) const MAX_FPS: u32 = 240;

/// The frame budget of a configured `max_fps`: the shortest gap two frames may
/// have. Clamped to `1..=MAX_FPS` so a config file's `max_fps: 0` cannot
/// produce an infinite (panicking) [`Duration`].
pub(crate) fn frame_budget(max_fps: u32) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(max_fps.clamp(1, MAX_FPS)))
}

/// An easing curve applied to linear progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Ease {
    /// No easing: the progress itself.
    Linear,
    /// Decelerating; the default for anything that moves into place.
    #[default]
    OutQuad,
}

impl Ease {
    /// The eased progress of linear `p`, clamped into `0.0..=1.0`. A NaN
    /// progress reads as 0, so a broken clock can never paint garbage.
    pub(crate) fn at(self, p: f32) -> f32 {
        let p = if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) };
        match self {
            Ease::Linear => p,
            Ease::OutQuad => p * (2.0 - p),
        }
    }
}

/// A running animation: a start instant, a duration and a curve.
///
/// `now` is always a parameter, never read from the clock inside, so the value
/// a frame draws follows the frame's own timestamp. A zero duration is already
/// finished (progress 1.0) — an animation the user cannot perceive must not
/// hold the frame clock open.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Anim {
    started: Instant,
    duration: Duration,
    ease: Ease,
}

impl Anim {
    /// An animation that starts at `now` and ends `duration` later.
    pub(crate) fn new(now: Instant, duration: Duration, ease: Ease) -> Self {
        Self { started: now, duration, ease }
    }

    /// Linear progress in `0.0..=1.0` at `now`.
    pub(crate) fn progress(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        (elapsed / self.duration.as_secs_f32()).clamp(0.0, 1.0)
    }

    /// The eased value at `now` (what a widget should draw).
    pub(crate) fn sample(&self, now: Instant) -> f32 {
        self.ease.at(self.progress(now))
    }

    /// Whether the animation has reached its end at `now`.
    pub(crate) fn is_done(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }

    /// The configured duration (a surface reads it to schedule the next step).
    pub(crate) fn duration(&self) -> Duration {
        self.duration
    }

}

/// A value that moves toward a target over time — the shape every transition
/// needs (the modal backdrop factor today; a reveal or a crossfade next).
///
/// A `Fade` starts at rest at its value. Retargeting it starts a new animation
/// **from wherever it currently is**, so a fade interrupted halfway reverses
/// smoothly instead of snapping to the other end.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fade {
    from: f32,
    to: f32,
    anim: Anim,
}

impl Fade {
    /// A fade already at rest at `value`.
    pub(crate) fn at_rest(value: f32) -> Self {
        Self {
            from: value,
            to: value,
            anim: Anim::new(Instant::now(), Duration::ZERO, Ease::Linear),
        }
    }

    /// Where the fade is heading.
    pub(crate) fn target(&self) -> f32 {
        self.to
    }

    /// The value at `now`.
    pub(crate) fn value(&self, now: Instant) -> f32 {
        if self.anim.duration().is_zero() {
            return self.to;
        }
        let eased = self.anim.sample(now);
        self.from + (self.to - self.from) * eased
    }

    /// Head for `target`, starting from the current value at `now`.
    pub(crate) fn retarget(&mut self, now: Instant, target: f32, over: Duration) {
        self.from = self.value(now);
        self.to = target;
        self.anim = Anim::new(now, over, Ease::OutQuad);
    }

    /// Whether the fade has reached its target at `now` (a surface that is still
    /// moving must keep asking the clock for frames).
    pub(crate) fn is_resting(&self, now: Instant) -> bool {
        self.anim.is_done(now)
    }
}

/// The interval the one frame tick must be armed with, or `None` when nothing
/// asked for a frame this pass (the tick is then cancelled and the event loop
/// blocks on events again).
///
/// * `stepped` — the shortest stepped cadence requested (spinners).
/// * `motion` — a surface wants every frame (an animation or a live timeline).
/// * `budget` — the `max_fps` frame budget, a hard ceiling: no request may make
///   frames arrive faster than it.
/// * `paused` — the UI is not on screen (a hidden tmux pane): never arm.
pub(crate) fn plan(
    stepped: Option<Duration>,
    motion: bool,
    budget: Duration,
    paused: bool,
) -> Option<Duration> {
    if paused {
        return None;
    }
    if motion {
        return Some(budget);
    }
    stepped.map(|step| step.max(budget))
}

/// The app's frame clock state: what the pass being built asked for, and the
/// single tick that drives it.
///
/// Interior mutability, because a render pass only has `&Ctx`: threading a
/// `&mut` through every pane just to say "animate me" would infect every
/// signature in the UI.
#[derive(Debug, Default)]
pub(crate) struct Animator {
    /// A surface wants a frame on every budget tick.
    motion: Cell<bool>,
    /// The shortest stepped cadence requested this pass.
    stepped: Cell<Option<Duration>>,
    /// The one scheduler job id this clock owns, once it has been armed.
    job: Cell<Option<Id>>,
}

impl Animator {
    /// A surface wants a frame on every budget tick — an animation is running,
    /// or a live timeline (the playback position) is on screen.
    pub(crate) fn request_frame(&self) {
        self.motion.set(true);
    }

    /// A surface wants frames on a stepped cadence, e.g. a spinner whose glyph
    /// changes every 120 ms. The shortest cadence wins; the frame budget still
    /// clamps it.
    pub(crate) fn request_every(&self, every: Duration) {
        let current = self.stepped.get();
        self.stepped.set(Some(current.map_or(every, |cur| cur.min(every))));
    }

    /// Whether anything has asked for frames since the last frame finished.
    /// A test probe: production code reacts to `take_interval`, which consumes
    /// the same requests.
    #[cfg(test)]
    pub(crate) fn wants_frames(&self) -> bool {
        self.motion.get() || self.stepped.get().is_some()
    }

    /// Consume this pass's requests and decide the next tick's interval. A
    /// second call without new requests returns `None` — the tick that was
    /// armed for this pass is not re-armed, which is what stops the clock.
    pub(crate) fn take_interval(&self, budget: Duration, paused: bool) -> Option<Duration> {
        let stepped = self.stepped.take();
        let motion = self.motion.replace(false);
        plan(stepped, motion, budget, paused)
    }

    /// The stable id of the one tick this clock arms (created on first use, so
    /// re-arming replaces the same job instead of piling one up per frame).
    pub(crate) fn job(&self) -> Id {
        let id = self.job.get().unwrap_or_else(id::new);
        self.job.set(Some(id));
        id
    }

    /// Forget the armed tick, returning its id so the caller can cancel it.
    /// `None` when the clock was already idle — the caller then does nothing.
    pub(crate) fn idle(&self) -> Option<Id> {
        self.job.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn dur(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn frame_budget_is_clamped() {
        assert_eq!(frame_budget(0), dur(1000), "0 must not divide by zero");
        assert_eq!(frame_budget(1), dur(1000));
        assert_eq!(frame_budget(60), Duration::from_secs_f64(1.0 / 60.0));
        // 240 is the documented ceiling; anything above it is clamped, not
        // honoured (a 10 kHz tick would spin a core for nothing).
        assert_eq!(frame_budget(10_000), frame_budget(MAX_FPS));
    }

    #[test]
    fn ease_curves_pin_their_endpoints() {
        for ease in [Ease::Linear, Ease::OutQuad] {
            assert!(
                (ease.at(0.0) - 0.0).abs() < f32::EPSILON,
                "{ease:?} must start at 0"
            );
            assert!(
                (ease.at(1.0) - 1.0).abs() < f32::EPSILON,
                "{ease:?} must land exactly on 1"
            );
        }
    }

    #[test]
    fn ease_clamps_and_survives_nan() {
        for ease in [Ease::Linear, Ease::OutQuad] {
            assert!((ease.at(-1.0) - 0.0).abs() < f32::EPSILON, "{ease:?} below 0");
            assert!((ease.at(2.0) - 1.0).abs() < f32::EPSILON, "{ease:?} above 1");
            assert!((ease.at(f32::NAN) - 0.0).abs() < f32::EPSILON, "{ease:?} NaN");
        }
    }

    #[test]
    fn ease_curves_are_monotonic() {
        for ease in [Ease::Linear, Ease::OutQuad] {
            let mut prev = ease.at(0.0);
            for step in 1u16..=100 {
                let value = ease.at(f32::from(step) / 100.0);
                assert!(value >= prev, "{ease:?} went backwards at {step}");
                prev = value;
            }
        }
    }

    #[test]
    fn a_decelerating_curve_is_ahead_of_linear_in_the_middle() {
        // The point of OutQuad as the default: the motion reads as "arriving"
        // rather than "sliding at constant speed".
        assert!(Ease::OutQuad.at(0.5) > 0.5);
        assert!((Ease::Linear.at(0.5) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn anim_progress_runs_from_start_to_end() {
        let start = Instant::now();
        let anim = Anim::new(start, dur(200), Ease::Linear);
        assert!((anim.progress(start) - 0.0).abs() < f32::EPSILON);
        assert!((anim.progress(start + dur(100)) - 0.5).abs() < 1e-6);
        assert!((anim.progress(start + dur(200)) - 1.0).abs() < f32::EPSILON);
        // Past the end it stays pinned at 1 (a late frame cannot overshoot).
        assert!((anim.progress(start + dur(900)) - 1.0).abs() < f32::EPSILON);
        assert!(anim.is_done(start + dur(200)));
        assert!(!anim.is_done(start + dur(199)));
    }

    #[test]
    fn a_zero_duration_anim_is_already_done() {
        let start = Instant::now();
        let anim = Anim::new(start, Duration::ZERO, Ease::OutQuad);
        assert!((anim.progress(start) - 1.0).abs() < f32::EPSILON);
        assert!((anim.sample(start) - 1.0).abs() < f32::EPSILON);
        assert!(anim.is_done(start));
    }

    #[test]
    fn a_clock_that_went_backwards_reads_as_not_started() {
        // `now` earlier than the start must not underflow into a huge progress.
        let start = Instant::now();
        let anim = Anim::new(start + dur(50), dur(200), Ease::Linear);
        assert!((anim.progress(start) - 0.0).abs() < f32::EPSILON);
        // Halfway through the duration is still before the start.
        assert!((anim.progress(start + dur(25)) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn sample_applies_the_curve_to_the_progress() {
        let start = Instant::now();
        let anim = Anim::new(start, dur(100), Ease::OutQuad);
        let p = anim.progress(start + dur(50));
        assert!((anim.sample(start + dur(50)) - Ease::OutQuad.at(p)).abs() < 1e-6);
        assert!(anim.sample(start + dur(50)) > p, "OutQuad leads linear mid-way");
    }

    #[test]
    fn plan_prefers_the_budget_for_motion() {
        let budget = frame_budget(60);
        assert_eq!(plan(None, true, budget, false), Some(budget));
        // Motion wins over a slower stepped cadence.
        assert_eq!(plan(Some(dur(120)), true, budget, false), Some(budget));
    }

    #[test]
    fn plan_never_beats_the_frame_budget() {
        let budget = frame_budget(60);
        // A 120 ms spinner stays 120 ms when the budget is 16 ms...
        assert_eq!(plan(Some(dur(120)), false, budget, false), Some(dur(120)));
        // ...but a request faster than the budget is clamped to it (max_fps is
        // a ceiling, so a pane cannot buy more frames than the user allowed).
        assert_eq!(plan(Some(dur(1)), false, budget, false), Some(budget));
    }

    #[test]
    fn plan_idles_without_requests_and_while_paused() {
        let budget = frame_budget(60);
        assert_eq!(plan(None, false, budget, false), None);
        // Paused wins over everything, including live motion.
        assert_eq!(plan(Some(dur(120)), true, budget, true), None);
        assert_eq!(plan(None, true, budget, true), None);
    }

    #[test]
    fn animator_keeps_the_shortest_stepped_request() {
        let animator = Animator::default();
        animator.request_every(dur(120));
        animator.request_every(dur(500));
        assert_eq!(
            animator.take_interval(frame_budget(60), false),
            Some(dur(120)),
            "the shortest cadence asked for wins"
        );
        animator.request_every(dur(120));
        animator.request_every(dur(60));
        assert_eq!(
            animator.take_interval(frame_budget(60), false),
            Some(dur(60)),
            "a faster surface wins over a slower one"
        );
        // Requests were consumed: nothing asked, nothing armed.
        assert_eq!(animator.take_interval(frame_budget(60), false), None);
        assert!(!animator.wants_frames());
    }

    #[test]
    fn animator_motion_outranks_a_stepped_request() {
        let animator = Animator::default();
        animator.request_every(dur(120));
        animator.request_frame();
        assert!(animator.wants_frames());
        assert_eq!(animator.take_interval(frame_budget(60), false), Some(frame_budget(60)));
        assert_eq!(animator.take_interval(frame_budget(60), false), None);
    }

    #[test]
    fn animator_job_id_is_stable_and_idle_clears_it() {
        let animator = Animator::default();
        let first = animator.job();
        assert_eq!(animator.job(), first, "re-arming must reuse the same job");
        assert_eq!(animator.idle(), Some(first));
        assert_eq!(animator.idle(), None, "already idle: nothing to cancel");
        assert_ne!(animator.job(), first, "a fresh arm gets a fresh id");
    }

    #[test]
    fn a_paused_animator_drops_the_request_it_consumed() {
        // A hidden tmux pane must not leave a stale request that fires frames
        // as soon as it comes back; the request is consumed either way.
        let animator = Animator::default();
        animator.request_frame();
        assert_eq!(animator.take_interval(frame_budget(60), true), None);
        assert!(!animator.wants_frames());
        assert_eq!(animator.take_interval(frame_budget(60), false), None);
    }

    #[test]
    fn anim_timestamps_round_trip_through_millis() {
        // Guards the `as_secs_f32` conversion: a 1 s animation must sample
        // exactly at its midpoint, which a float-sloppy conversion would miss.
        let start = Instant::now();
        let anim = Anim::new(start, Duration::from_secs(1), Ease::Linear);
        assert!((anim.progress(start + Duration::from_millis(500 * MS / MS)) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_fade_at_rest_reads_its_value_immediately() {
        let now = Instant::now();
        let fade = Fade::at_rest(0.4);
        assert!((fade.value(now) - 0.4).abs() < f32::EPSILON);
        assert!(fade.is_resting(now));
    }

    #[test]
    fn a_retargeted_fade_travels_to_its_target() {
        let start = Instant::now();
        let mut fade = Fade::at_rest(0.0);
        fade.retarget(start, 1.0, dur(120));
        assert!((fade.value(start) - 0.0).abs() < f32::EPSILON, "starts where it was");
        assert!(!fade.is_resting(start), "still moving");
        assert!(
            fade.value(start + dur(120)) > 0.999,
            "lands on the target (got {})",
            fade.value(start + dur(120))
        );
        assert!(fade.is_resting(start + dur(120)));
    }

    #[test]
    fn an_interrupted_fade_reverses_from_where_it_is() {
        let start = Instant::now();
        let mut fade = Fade::at_rest(0.0);
        fade.retarget(start, 1.0, dur(100));
        let halfway = fade.value(start + dur(50));
        assert!(halfway > 0.0 && halfway < 1.0, "mid-flight: {halfway}");

        // Reverse in the middle: the value must not jump to either end.
        fade.retarget(start + dur(50), 0.0, dur(100));
        assert!((fade.value(start + dur(50)) - halfway).abs() < 1e-6);
        assert!(fade.value(start + dur(100)) < halfway, "it is heading back down");
        assert!(fade.value(start + dur(150)) < 1e-3, "and reaches zero");
    }

    #[test]
    fn a_zero_length_fade_snaps() {
        let now = Instant::now();
        let mut fade = Fade::at_rest(0.0);
        fade.retarget(now, 1.0, Duration::ZERO);
        assert!((fade.value(now) - 1.0).abs() < f32::EPSILON);
        assert!(fade.is_resting(now));
    }
}
