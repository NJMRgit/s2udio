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
    /// The last user position we declined to override, so the decision is
    /// made once per track change instead of on every render.
    declined_for: Option<Instant>,
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

    /// The decision for a track change happening at `now`. Records the
    /// outcome so a caller that asks again for the same change (a re-render,
    /// a second `SongChanged` for the same track) gets a stable answer even
    /// after the window has elapsed.
    pub(crate) fn should_follow(&mut self, now: Instant) -> bool {
        if self.user_is_navigating(now) {
            self.declined_for = Some(now);
            return false;
        }
        // A change that was declined keeps being declined: the user's
        // position must survive until the *next* track, not just the window.
        if self.declined_for.is_some() {
            return false;
        }
        true
    }

    /// A new track change begins: clear the previous track's decision.
    pub(crate) fn begin_track_change(&mut self) {
        self.declined_for = None;
    }
}

