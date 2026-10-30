//! The display timeline: MPD's playback position extrapolated to the frame
//! being drawn.
//!
//! MPD reports `elapsed` at the instant it answered. Between answers the value
//! is stale by up to one poll interval, so a bar drawn straight from
//! `status.elapsed` steps: at the default 1 s poll a 3-minute track visibly
//! jumps four times a second, and at the 16 ms poll the example config uses it
//! still only moves when a status message lands.
//!
//! Every frame now draws `elapsed + (now - last answer)` instead. Two guards
//! keep the extrapolation honest:
//!
//! * a paused or stopped player does not advance at all (the answer *is* the
//!   position), and
//! * the advance is capped at [`MAX_ADVANCE`], so a dropped or stalled MPD
//!   connection shows a bar that stops rather than one that walks to the end of
//!   the track on its own.
//!
//! Backend logic keeps using `status.elapsed`: this is a display value, never a
//! seek target, a position report to Jellyfin, or a lyrics lookup key.

use std::time::Duration;

/// The furthest the display will run past MPD's last answer.
///
/// Long enough to cover any sane `status_update_interval_ms` (1 s by default),
/// short enough that a dead connection stops moving the bar within a few
/// seconds instead of lying about the position for the rest of the track.
pub(crate) const MAX_ADVANCE: Duration = Duration::from_secs(5);

/// The position to draw: `elapsed` advanced by `since_answer` when `playing`,
/// clamped to the track and to [`MAX_ADVANCE`].
///
/// A zero `duration` (an internet radio stream, an unresolved entry) is not a
/// track length, so it does not clamp: the elapsed clock must still run.
pub(crate) fn display_elapsed(
    elapsed: Duration,
    duration: Duration,
    since_answer: Duration,
    playing: bool,
) -> Duration {
    if !playing {
        return elapsed;
    }
    let advanced = elapsed.saturating_add(since_answer.min(MAX_ADVANCE));
    if duration.is_zero() { advanced } else { advanced.min(duration) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_paused_player_does_not_advance() {
        assert_eq!(display_elapsed(secs(30), secs(200), secs(2), false), secs(30));
    }

    #[test]
    fn a_playing_player_advances_by_the_time_since_the_answer() {
        assert_eq!(display_elapsed(secs(30), secs(200), secs(2), true), secs(32));
    }

    #[test]
    fn the_position_never_runs_past_the_track() {
        // A status answer that arrives late must not draw a bar past the end.
        assert_eq!(display_elapsed(secs(199), secs(200), secs(9), true), secs(200));
    }

    #[test]
    fn a_stream_with_no_duration_still_counts_up() {
        // Radio: `duration` is zero, which is "unknown", not "zero length".
        assert_eq!(display_elapsed(secs(30), Duration::ZERO, secs(2), true), secs(32));
    }

    #[test]
    fn a_stalled_connection_stops_the_bar() {
        // Five minutes without an answer: the advance is capped, so the bar
        // stalls instead of walking to the end of the track.
        assert_eq!(
            display_elapsed(secs(10), secs(600), Duration::from_secs(300), true),
            secs(10) + MAX_ADVANCE
        );
    }

    #[test]
    fn the_advance_is_exactly_capped_at_the_limit() {
        assert_eq!(
            display_elapsed(secs(10), secs(600), MAX_ADVANCE - Duration::from_millis(1), true),
            secs(10) + MAX_ADVANCE - Duration::from_millis(1)
        );
        assert_eq!(
            display_elapsed(secs(10), secs(600), MAX_ADVANCE, true),
            secs(10) + MAX_ADVANCE
        );
    }
}
