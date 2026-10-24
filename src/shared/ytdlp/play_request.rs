//! The generation of the user's latest **play this now** resolve.
//!
//! A play request (a double-click on a queue row, a stored playlist entry,
//! a pasted link's *Play*) resolves through `yt-dlp`, which costs seconds.
//! Every such request stamps its `ResolveYtStreams` message with a fresh
//! generation; a newer play request — or an explicit cancel — bumps the
//! generation and makes the older request **stale**:
//!
//! - the resolve pool skips a stale job that has not started yet (no wasted
//!   `yt-dlp` run),
//! - the event loop drops a stale result instead of inserting or playing it
//!   (a slow resolve can no longer play the entry the user has moved on
//!   from — the 2026-10-25 report: double-clicking a playlist entry, then
//!   another before the first resolved, played the *first* one).
//!
//! Non-play resolves (queue appends, the look-ahead swap, metadata warms, the
//! automatic decode-error recovery) are never stamped: they are not the
//! user's current play intent and must survive any newer request.
use std::sync::atomic::{AtomicU64, Ordering};

/// The current play-request generation. `0` means no play request has been
/// made yet (nothing is stamped with it).
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Start a new play request, superseding every earlier one. The returned
/// generation is what the request is stamped with.
pub(crate) fn next() -> u64 {
    GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Supersede the current play request without starting a new one: the pending
/// resolve is cancelled (skipped while queued, its late result dropped).
pub(crate) fn cancel_current() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// Whether `generation` is still the latest play request.
pub(crate) fn is_current(generation: u64) -> bool {
    GENERATION.load(Ordering::SeqCst) == generation
}

#[cfg(test)]
mod tests {
    use super::{cancel_current, is_current, next};

    /// Every play request supersedes the previous one; only the newest is
    /// current.
    #[test]
    fn only_the_newest_play_request_is_current() {
        let _guard = crate::tests::fixtures::PLAY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let first = next();
        assert!(is_current(first));
        let second = next();
        assert!(second > first);
        assert!(!is_current(first), "a newer play request supersedes the older");
        assert!(is_current(second));
    }

    /// Cancelling leaves no current request: the pending resolve is stale.
    #[test]
    fn cancelling_supersedes_without_starting_a_new_one() {
        let _guard = crate::tests::fixtures::PLAY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = next();
        cancel_current();
        assert!(!is_current(pending));
    }
}
