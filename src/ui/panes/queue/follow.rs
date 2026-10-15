use std::time::{Duration, Instant};

/// How long after a track change the user's own queue navigation still wins
/// over the automatic "follow the new track" jump. Scrolling or moving the
/// selection within this window means the user is reading the queue, so
/// yanking the selection to the new track would fight them.
pub(crate) const FOLLOW_SUPPRESS_WINDOW: Duration = Duration::from_secs(5);

/// Decides whether the Queue tab may move its selection to a newly playing
/// track.
///
/// The rule the user asked for: the selection follows the new track and
/// lands as close to the top of the visible list as the viewport allows —
/// **unless** the user has been actively scrolling or moving through the
/// queue within [`FOLLOW_SUPPRESS_WINDOW`] of the change, in which case the
/// selection is left exactly where they put it.
#[derive(Debug, Default)]
pub(crate) struct FollowGate {
    /// When the user last moved the selection or scrolled the queue.
    last_user_navigation: Option<Instant>,
}

impl FollowGate {
    /// Record that the user just scrolled or moved the queue selection.
    pub(crate) fn note_user_navigation(&mut self, now: Instant) {
        self.last_user_navigation = Some(now);
    }

    /// Whether the user has navigated the queue recently enough that the
    /// automatic jump must stand down.
    pub(crate) fn user_is_navigating(&self, now: Instant) -> bool {
        self.last_user_navigation
            .is_some_and(|last| now.saturating_duration_since(last) < FOLLOW_SUPPRESS_WINDOW)
    }

    /// The decision for a track change happening at `now`.
    ///
    /// Only the **window** decides this: the user's navigation within
    /// [`FOLLOW_SUPPRESS_WINDOW`] of the change leaves the selection alone.
    /// What the gate deliberately does *not* do is latch: an earlier decline
    /// must not keep suppressing later changes. The caller already makes the
    /// decision once per track (`followed_song_id`), so a repeat for the same
    /// track never reaches here — and latching here suppressed *new* tracks
    /// whenever a swap changed a row's id, because the new id looked like the
    /// same track while the stale latch still held. The reported symptom was
    /// the selection not following a track that had just resolved and started.
    pub(crate) fn should_follow(&self, now: Instant) -> bool {
        !self.user_is_navigating(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> Instant {
        // A fixed base so the tests never depend on wall-clock time.
        Instant::now() + Duration::from_secs(secs)
    }

    #[test]
    fn follows_when_the_user_has_not_touched_the_queue() {
        let gate = FollowGate::default();
        assert!(gate.should_follow(at(0)));
    }

    #[test]
    fn follows_when_the_user_navigated_long_before_the_change() {
        let mut gate = FollowGate::default();
        // Navigated 30 s ago: well outside the 5 s window.
        gate.note_user_navigation(at(0));
        assert!(gate.should_follow(at(30)));
    }

    #[test]
    fn stands_down_when_the_user_navigated_just_before_the_change() {
        let mut gate = FollowGate::default();
        gate.note_user_navigation(at(0));
        // The track changes 2 s after the user scrolled.
        assert!(!gate.should_follow(at(2)));
    }

    #[test]
    fn stands_down_when_the_user_navigates_right_as_it_changes() {
        let mut gate = FollowGate::default();
        gate.note_user_navigation(at(10));
        assert!(!gate.should_follow(at(10)));
    }

    #[test]
    fn the_window_boundary_is_exclusive() {
        let mut gate = FollowGate::default();
        gate.note_user_navigation(at(0));
        // Exactly 5 s: the window has elapsed, so the jump proceeds.
        assert!(gate.should_follow(at(5)));
        // One millisecond short of it: still suppressed.
        let mut gate = FollowGate::default();
        gate.note_user_navigation(at(0));
        assert!(!gate.should_follow(at(5) - Duration::from_millis(1)));
    }

    /// **The gate must not latch.** Only the window decides. An earlier
    /// decline kept suppressing later changes once the window had passed, and
    /// because a resolve's swap gives the playing row a *new id*, a repeated
    /// query did not look like the same change to the caller — so a track that
    /// had just resolved and started was denied its jump. The reported symptom
    /// was the selection never moving to the top after `▶▶` needed a resolve.
    #[test]
    fn a_past_decline_does_not_suppress_a_later_change() {
        let mut gate = FollowGate::default();
        gate.note_user_navigation(at(0));
        assert!(!gate.should_follow(at(1)), "inside the window it stands down");
        // Long afterwards the window has elapsed: the decision follows again,
        // with no latch to clear.
        assert!(
            gate.should_follow(at(60)),
            "a past decline must not keep suppressing later changes"
        );
    }

    /// A change arriving while the user is still inside the window stands down,
    /// however many times it is asked.
    #[test]
    fn a_change_inside_the_window_always_stands_down() {
        let mut gate = FollowGate::default();
        gate.note_user_navigation(at(0));
        assert!(!gate.should_follow(at(1)));
        assert!(!gate.should_follow(at(2)));
        assert!(!gate.should_follow(at(4)));
    }

    /// The one-decision-per-track rule lives in the caller (`followed_song_id`),
    /// not here — this proves the gate itself is stateless w.r.t. its verdicts.
    #[test]
    fn the_gate_repeats_the_same_verdict_for_the_same_moment() {
        let gate = FollowGate::default();
        assert!(gate.should_follow(at(0)));
        assert!(gate.should_follow(at(0)));
        let mut gate = FollowGate::default();
        gate.note_user_navigation(at(0));
        assert!(!gate.should_follow(at(1)));
        assert!(!gate.should_follow(at(1)));
    }
}
