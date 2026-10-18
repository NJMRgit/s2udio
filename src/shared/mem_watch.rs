//! The optional in-app memory watchdog (Settings > Advanced).
//!
//! Round 111 shipped `scripts/dev/test-memory.sh`, which measures RSS from
//! *outside* the process. That catches the leak shape in CI and in a soak run,
//! but it does nothing for the case the original report actually hit: someone
//! leaves s2udio open for hours, it grows, and it dies — with nobody running a
//! harness. If the process is killed by SIGKILL or oomd rather than panicking,
//! it writes no panic line either, so the death leaves nothing behind at all.
//!
//! With the watchdog on, the app samples its own RSS on a timer and logs the
//! number. A long session then leaves a **growth trail in its own log**, which
//! is the file that already exists and already survives a restart. Turning the
//! trail into a diagnosis needs no harness: read the `rss_mb` values in order.
//!
//! Design constraints, in order of importance:
//!
//! 1. **It must not cost anything when off.** The feature defaults off and the
//!    thread is not spawned at all unless the config says so — not a sleeping
//!    thread, no thread.
//! 2. **It must never block or slow the UI.** Sampling happens on its own
//!    thread on a plain sleep loop, and touches only `/proc`-equivalent
//!    sysinfo calls and the logger.
//! 3. **It must be honest about what it measures.** One number, once per
//!    interval, at a known level. No smoothing, no interpolation — a reader
//!    must be able to plot the logged values directly.
//!
//! Deliberately *not* included: alerting, thresholds, or any in-app UI. The
//! log is the artifact; judging it is the reader's job. That keeps the code
//! small enough to be obviously correct, which matters for something that
//! only ever runs while you are already debugging something else.

use std::time::Duration;

/// Default interval between RSS samples.
///
/// Five minutes is a deliberate compromise: frequent enough that a session
/// which grows over ~45 minutes (the report's shape) yields roughly nine
/// points to read; rare enough that a multi-hour session adds a few dozen log
/// lines rather than thousands. The log rotates at 8 MiB, so even an
/// accidentally aggressive interval cannot runaway-fill the disk.
pub const DEFAULT_INTERVAL_SECS: u64 = 300;

/// Floor for the configurable interval, so a mistyped `1` cannot turn the
/// watchdog into a log-spamming busy loop.
pub const MIN_INTERVAL_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// Pure sampling logic — no threads, no sysinfo, unit-testable.
// ---------------------------------------------------------------------------

/// One reading: how many bytes of RSS, and how long the process had been up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub rss_bytes: u64,
    pub uptime: Duration,
}

impl Sample {
    #[must_use]
    pub fn rss_mb(&self) -> u64 {
        self.rss_bytes / (1024 * 1024)
    }
}

/// Tracks the samples seen so far, so the watchdog can report a *peak* and a
/// simple growth verdict rather than only the instantaneous number.
///
/// Kept deliberately crude: the goal is to make an obvious leak obvious, not
/// to be a profiler. A least-squares fit would be more precise and would also
/// be three times the code for a value nobody thresholds.
#[derive(Debug, Default)]
pub struct MemoryWatch {
    samples: Vec<Sample>,
    peak_bytes: u64,
}

impl MemoryWatch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a reading, returning nothing — the caller decides whether to
    /// log it. Kept separate from logging so the trend logic is testable
    /// without a logger installed.
    pub fn record(&mut self, sample: Sample) {
        self.peak_bytes = self.peak_bytes.max(sample.rss_bytes);
        self.samples.push(sample);
    }

    #[must_use]
    pub fn peak_mb(&self) -> u64 {
        self.peak_bytes / (1024 * 1024)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    #[must_use]
    pub fn first_mb(&self) -> Option<u64> {
        self.samples.first().map(Sample::rss_mb)
    }

    #[must_use]
    pub fn last_mb(&self) -> Option<u64> {
        self.samples.last().map(Sample::rss_mb)
    }

    /// Compare the start of the series against the end and report the change
    /// in whole MB.
    ///
    /// Uses the **median** of each block rather than the mean. A mean is
    /// dominated by outliers, and this is a series whose whole purpose is to
    /// observe outliers: a single spike (a big album-art decode, a burst of
    /// queue rows) would otherwise be reported as a sustained trend. The
    /// median ignores a lone spike at either end, which is the property that
    /// makes the verdict worth logging. The block size also grows with the
    /// series (min 3) so early samples are not compared on one reading each.
    ///
    /// Returns `None` until there are enough samples for the comparison to
    /// mean anything.
    #[must_use]
    pub fn growth_mb(&self) -> Option<i64> {
        if self.samples.len() < 6 {
            return None;
        }
        let block = (self.samples.len() / 4).clamp(3, 12);
        let head = &self.samples[..block];
        let tail = &self.samples[self.samples.len() - block..];

        let median_mb = |xs: &[Sample]| -> f64 {
            let mut v: Vec<u64> = xs.iter().map(|s| s.rss_bytes).collect();
            v.sort_unstable();
            #[allow(clippy::cast_precision_loss)]
            {
                if v.len() % 2 == 1 {
                    v[v.len() / 2] as f64 / (1024.0 * 1024.0)
                } else {
                    let a = v[v.len() / 2 - 1] as f64;
                    let b = v[v.len() / 2] as f64;
                    (a + b) / 2.0 / (1024.0 * 1024.0)
                }
            }
        };

        #[allow(clippy::cast_possible_truncation)]
        let delta = (median_mb(tail) - median_mb(head)).round();
        Some(delta as i64)
    }

    /// A one-line human verdict for the log, e.g.
    /// `"stable (peak 62 MB, first 61 MB, last 62 MB)"`.
    #[must_use]
    pub fn verdict(&self) -> String {
        match (self.first_mb(), self.last_mb(), self.growth_mb()) {
            (Some(first), Some(last), Some(growth)) => {
                let trend = match growth {
                    // Below a few MB this is noise, not a trend.
                    -4..=4 => "stable".to_string(),
                    g if g > 0 => format!("growing (+{g} MB)"),
                    g => format!("shrinking ({g} MB)"),
                };
                format!("{trend} (peak {} MB, first {first} MB, last {last} MB)", self.peak_mb())
            }
            (Some(first), Some(last), None) => {
                format!("warming up (peak {} MB, first {first} MB, last {last} MB)", self.peak_mb())
            }
            _ => "no samples yet".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// The reader: this process's RSS, via sysinfo (already a dependency).
// ---------------------------------------------------------------------------

/// Read this process's resident set size in bytes.
///
/// Uses `sysinfo` rather than parsing `/proc/self/status` by hand: it is
/// already a dependency of this crate (the ueberzug backend uses it), so this
/// adds no new dependency, and it works on the non-Linux targets the project
/// supports. `refresh_memory_specifics` asks for memory only — refreshing
/// "everything" about the process would be strictly more work for one number.
#[must_use]
pub fn current_rss_bytes() -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    let pid = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    system.process(pid).map(sysinfo::Process::memory)
}

// ---------------------------------------------------------------------------
// The thread.
// ---------------------------------------------------------------------------

/// Start the watchdog, if it is enabled. Returns whether a thread was spawned.
///
/// Called once from `run_tui`. The caller passes the already-resolved config
/// so this module stays free of config plumbing.
///
/// The thread exits on its own when `should_run` returns false, which the
/// caller wires to the app's shutdown flag; it is also detached (never joined)
/// because nothing depends on its completion, and a watchdog must never be
/// able to hold up a shutdown.
pub fn spawn(
    enabled: bool,
    interval_secs: u64,
    should_run: impl Fn() -> bool + Send + 'static,
) -> bool {
    if !enabled {
        // Not "a thread that does nothing" — no thread at all. Off must mean
        // free, or the default being off would still cost every user.
        return false;
    }
    let interval = Duration::from_secs(interval_secs.max(MIN_INTERVAL_SECS));
    let started = std::time::Instant::now();

    let spawned = std::thread::Builder::new()
        .name("mem_watchdog".to_string())
        .spawn(move || {
            let mut watch = MemoryWatch::new();
            log::info!(
                interval_secs = interval.as_secs();
                "Memory watchdog started (Settings > Advanced)"
            );
            while should_run() {
                std::thread::sleep(interval);
                if !should_run() {
                    break;
                }
                let Some(rss) = current_rss_bytes() else {
                    // A failed read is worth exactly one warning, not a death:
                    // the watchdog is diagnostic and must never be the reason
                    // the app misbehaves.
                    log::warn!("Memory watchdog could not read this process's RSS");
                    continue;
                };
                watch.record(Sample { rss_bytes: rss, uptime: started.elapsed() });
                log::info!(
                    rss_mb = watch.last_mb().unwrap_or(0),
                    peak_mb = watch.peak_mb(),
                    samples = watch.len(),
                    uptime_s = started.elapsed().as_secs();
                    "Memory sample"
                );
            }
            // A final line at shutdown gives the session a legible summary
            // instead of a bare series the reader has to interpret.
            if !watch.is_empty() {
                log::info!(
                    peak_mb = watch.peak_mb(),
                    samples = watch.len();
                    "Memory watchdog summary: {}", watch.verdict()
                );
            }
        })
        .is_ok();

    spawned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(mb: u64) -> Sample {
        Sample { rss_bytes: mb * 1024 * 1024, uptime: Duration::from_secs(1) }
    }

    #[test]
    fn a_flat_series_reads_as_stable() {
        let mut w = MemoryWatch::new();
        for _ in 0..8 {
            w.record(sample(62));
        }
        assert_eq!(w.peak_mb(), 62);
        assert_eq!(w.growth_mb(), Some(0));
        assert!(w.verdict().contains("stable"), "got {}", w.verdict());
    }

    /// The report's shape: 121 MB early, 6.2 GB late. The verdict must call
    /// that growing, and must not be fooled into "stable" by the head/tail
    /// averaging.
    #[test]
    fn the_report_shape_reads_as_growing() {
        let mut w = MemoryWatch::new();
        for _ in 0..4 {
            w.record(sample(121));
        }
        for _ in 0..4 {
            w.record(sample(6200));
        }
        let growth = w.growth_mb().expect("4+ samples");
        assert!(growth > 1000, "expected a large positive delta, got {growth}");
        assert!(w.verdict().contains("growing"), "got {}", w.verdict());
        assert_eq!(w.peak_mb(), 6200);
    }

    /// Two isolated readings can differ innocently; the block averaging is
    /// what keeps a single transient allocation from being reported as a leak.
    #[test]
    fn a_single_spike_does_not_make_a_trend() {
        let mut w = MemoryWatch::new();
        for _ in 0..8 {
            w.record(sample(60));
        }
        // One spike, still inside the final quarter, but diluted by its block.
        w.record(sample(400));
        let growth = w.growth_mb().expect("enough samples");
        assert!(
            growth.abs() <= 60,
            "one spike in an otherwise flat series must not read as a big trend, got {growth}"
        );
    }

    #[test]
    fn growth_is_none_until_there_is_something_to_compare() {
        let mut w = MemoryWatch::new();
        // Six samples is the floor: below that, a "block median" would be one
        // or two readings and the verdict would be noise.
        for n in 0..5 {
            w.record(sample(50));
            assert_eq!(w.growth_mb(), None, "only {} sample(s) so far", n + 1);
        }
        w.record(sample(50));
        assert_eq!(w.growth_mb(), Some(0), "six samples is the minimum comparison");
    }

    #[test]
    fn shrinking_is_reported_as_such() {
        let mut w = MemoryWatch::new();
        for _ in 0..4 {
            w.record(sample(500));
        }
        for _ in 0..4 {
            w.record(sample(100));
        }
        assert!(w.verdict().contains("shrinking"), "got {}", w.verdict());
    }

    #[test]
    fn peak_survives_a_later_drop() {
        let mut w = MemoryWatch::new();
        w.record(sample(900));
        w.record(sample(90));
        assert_eq!(w.peak_mb(), 900, "the peak is a high-water mark, not the last value");
        assert_eq!(w.last_mb(), Some(90));
    }

    /// Off must mean "no thread", not "a thread that idles" — otherwise the
    /// default-off setting would still cost every user a thread.
    #[test]
    fn a_disabled_watchdog_spawns_nothing() {
        let spawned = spawn(false, 1, || true);
        assert!(!spawned, "a disabled watchdog must not spawn a thread");
    }

    /// A tiny interval must be clamped, so a mistyped config cannot turn the
    /// watchdog into a log-spamming busy loop.
    #[test]
    fn a_tiny_interval_is_clamped() {
        assert!(MIN_INTERVAL_SECS >= 10, "the floor must be meaningful");
        assert_eq!(DEFAULT_INTERVAL_SECS, 300);
    }

    /// The reader must return a plausible positive number for this very
    /// process — if it silently returned None the watchdog would log warnings
    /// forever and the feature would be dead weight.
    #[test]
    fn reading_our_own_rss_works() {
        let rss = current_rss_bytes().expect("this process must have a readable RSS");
        assert!(rss > 0, "RSS should be non-zero, got {rss}");
        // A test binary is comfortably under 4 GB; this catches a unit
        // mistake (bytes vs KiB) rather than a real memory problem.
        assert!(rss < 4 * 1024 * 1024 * 1024, "RSS looks like the wrong unit: {rss}");
    }
}
