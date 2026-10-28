//! Auto theming: resolve the desktop's accent color from one of three
//! sources and hand it to the theme watcher.
//!
//! Sources, in the `auto` priority order:
//!
//! 1. [`sttm`] — the blur-mode scheduler (`~/.blur-schedule` + `blsw`), which
//!    s2udio has always followed. Kept first so an existing blur-mode install
//!    keeps the exact accent it had before this module existed.
//! 2. [`matugen`] — a matugen palette JSON, the wallpaper-derived accent that
//!    many WMs/shells share.
//! 3. [`kde`] — the Plasma accent color from the XDG desktop portal, with
//!    `kdeglobals` as the fallback.
//!
//! Every provider returns one raw RGB color; [`resolve`] applies the
//! `desaturate` transform and reports which provider won, so the watcher can
//! log it and the settings panel can show it.
use std::time::Duration;

use parking_lot::Mutex;
use ratatui::style::Color;

use crate::config::{
    auto_theme::{AccentSource, AppliedAccent, AutoThemeConfig},
    theme::UiConfig,
};

pub(crate) mod kde;
pub(crate) mod matugen;
pub(crate) mod sttm;

/// How long the portal read may take before the source is abandoned. The
/// portal answers from memory, so anything slower than this means the portal
/// service is wedged (or absent) and the KDE fallback file should be used.
pub(crate) const PORTAL_TIMEOUT: Duration = Duration::from_millis(300);

/// Resolve the accent the current config asks for, in priority order. Returns
/// the first source that has a color.
pub(crate) fn resolve(cfg: &AutoThemeConfig) -> Option<AppliedAccent> {
    for source in cfg.source.providers() {
        let raw = match source {
            AccentSource::Sttm => sttm::read_accent(cfg),
            AccentSource::Matugen => matugen::read_accent(cfg),
            AccentSource::Kde => kde::read_accent(cfg),
        };
        if let Some(color) = raw {
            return Some(AppliedAccent {
                source: *source,
                color: desaturate(color, cfg.desaturate),
            });
        }
    }
    None
}

/// Mute `color` toward grey by `amount` (0.0 = the source's raw color, 1.0 =
/// pure luminance grey), lifting dark accents to a readability floor so a dark
/// mode hue stays visible as text and borders on a dark terminal.
///
/// `amount` = 0.4 reproduces the blur watcher's historical muting.
pub(crate) fn desaturate(color: Color, amount: f32) -> Color {
    let Color::Rgb(r, g, b) = color else { return color };
    let s = f64::from(amount.clamp(0.0, 1.0));
    if s == 0.0 {
        return color;
    }
    let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
    let luminance = 0.299 * r + 0.587 * g + 0.114 * b;
    let mut r = r * (1.0 - s) + luminance * s;
    let mut g = g * (1.0 - s) + luminance * s;
    let mut b = b * (1.0 - s) + luminance * s;
    let max_channel = r.max(g).max(b);
    if max_channel > 0.0 && max_channel < 160.0 {
        let lift = 160.0 / max_channel;
        r *= lift;
        g *= lift;
        b *= lift;
    }
    Color::Rgb(r as u8, g as u8, b as u8)
}

/// What changed when the watcher recorded a freshly resolved accent.
#[derive(Debug, Clone)]
pub(crate) enum AccentChange {
    /// Same source and color as before: nothing to re-render.
    Unchanged,
    /// A new accent to write into `theme.text_color`.
    Applied(AppliedAccent),
    /// The accent went away (source turned off, file removed, ...): put this
    /// theme snapshot back. It is the theme as it was before the accent was
    /// applied, so the configured appearance colors return untouched.
    Cleared(UiConfig),
}

/// The runtime accent state shared between the event loop, the resolve worker
/// and the settings panel.
///
/// `applied` is what the theme currently shows; `baseline` is the theme as it
/// was before the accent was applied (remembered so turning auto theming off
/// restores the configured colors instead of freezing the last accent).
#[derive(Debug, Default)]
pub(crate) struct AccentState {
    applied: Mutex<Option<AppliedAccent>>,
    baseline: Mutex<Option<UiConfig>>,
    /// A resolve worker is running; the ticker skips spawning another one.
    resolving: std::sync::atomic::AtomicBool,
}

impl AccentState {
    /// Start from an accent resolved before the event loop existed (startup,
    /// where the theme was already adjusted so the first frame never flashes
    /// a stale accent).
    pub(crate) fn seed(&self, applied: Option<AppliedAccent>, baseline: Option<UiConfig>) {
        *self.applied.lock() = applied;
        *self.baseline.lock() = baseline;
    }

    /// The accent currently applied to the theme.
    pub(crate) fn applied(&self) -> Option<AppliedAccent> {
        *self.applied.lock()
    }

    /// The theme as it was before the accent was applied: the configured
    /// colors the settings save must persist instead of the transient accent.
    pub(crate) fn baseline(&self) -> Option<UiConfig> {
        self.baseline.lock().clone()
    }

    /// Apply a configured change to the pre-accent baseline too, so turning
    /// the accent off restores the newest configuration instead of the one
    /// from when the accent first applied.
    pub(crate) fn patch_baseline(&self, patch: impl FnOnce(&mut UiConfig)) {
        if let Some(theme) = self.baseline.lock().as_mut() {
            patch(theme);
        }
    }

    /// Record a resolved accent and report what the theme has to do. The
    /// baseline snapshot is taken from `current_theme` on the first apply and
    /// handed back when the accent goes away.
    pub(crate) fn record(&self, next: Option<AppliedAccent>, current_theme: &UiConfig) -> AccentChange {
        let mut applied = self.applied.lock();
        if *applied == next {
            return AccentChange::Unchanged;
        }
        let mut baseline = self.baseline.lock();
        match (applied.is_some(), next) {
            (false, Some(accent)) => {
                *baseline = Some(current_theme.clone());
                *applied = Some(accent);
                AccentChange::Applied(accent)
            }
            (true, Some(accent)) => {
                *applied = Some(accent);
                AccentChange::Applied(accent)
            }
            (true, None) => {
                *applied = None;
                AccentChange::Cleared(baseline.take().unwrap_or_else(|| current_theme.clone()))
            }
            (false, None) => AccentChange::Unchanged,
        }
    }

    /// Claim the resolve worker slot. `false` means a resolve is already in
    /// flight and this tick must not spawn another.
    pub(crate) fn try_begin_resolve(&self) -> bool {
        !self
            .resolving
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Release the resolve worker slot.
    pub(crate) fn finish_resolve(&self) {
        self.resolving.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red() -> Color {
        Color::Rgb(255, 0, 0)
    }

    fn accent(source: AccentSource, color: Color) -> AppliedAccent {
        AppliedAccent { source, color }
    }

    #[test]
    fn desaturate_zero_keeps_the_raw_color() {
        assert_eq!(desaturate(Color::Rgb(0x3d, 0xae, 0xe9), 0.0), Color::Rgb(0x3d, 0xae, 0xe9));
        assert_eq!(desaturate(Color::Rgb(0x0a, 0x00, 0x00), 0.0), Color::Rgb(0x0a, 0x00, 0x00));
    }

    #[test]
    fn desaturate_mutes_the_hue_but_keeps_it() {
        let muted = desaturate(Color::Rgb(255, 127, 153), 0.4);
        let Color::Rgb(r, g, b) = muted else { panic!("expected rgb") };
        assert!(r > b && r > g, "pink should still read as pink: {muted:?}");
        assert!(r < 255, "a muted red channel should drop: {muted:?}");
    }

    #[test]
    fn desaturate_lifts_dark_accents_to_the_floor() {
        let lifted = desaturate(Color::Rgb(100, 0, 41), 0.4);
        let Color::Rgb(r, _, _) = lifted else { panic!("expected rgb") };
        assert!(r >= 0x80, "a dark accent must stay readable: {lifted:?}");
    }

    #[test]
    fn desaturate_passes_non_rgb_colors_through() {
        assert_eq!(desaturate(Color::Blue, 0.4), Color::Blue);
        assert_eq!(desaturate(Color::Reset, 1.0), Color::Reset);
    }

    #[test]
    fn resolve_returns_nothing_when_off() {
        let cfg = AutoThemeConfig {
            source: crate::config::auto_theme::AutoThemeSource::Off,
            ..AutoThemeConfig::default()
        };
        assert_eq!(resolve(&cfg), None);
    }

    #[test]
    fn recording_the_same_accent_is_unchanged() {
        let state = AccentState::default();
        let theme = UiConfig::default();
        assert!(matches!(
            state.record(Some(accent(AccentSource::Kde, red())), &theme),
            AccentChange::Applied(_)
        ));
        assert!(matches!(
            state.record(Some(accent(AccentSource::Kde, red())), &theme),
            AccentChange::Unchanged
        ));
        assert!(state.applied().is_some());
    }

    #[test]
    fn switching_source_reapplies_without_touching_the_baseline() {
        let state = AccentState::default();
        let theme = UiConfig::default();
        let _ = state.record(Some(accent(AccentSource::Sttm, red())), &theme);
        match state.record(Some(accent(AccentSource::Matugen, red())), &theme) {
            AccentChange::Applied(changed) => {
                assert_eq!(changed, accent(AccentSource::Matugen, red()));
            }
            other => panic!("expected a re-apply, got {other:?}"),
        }
    }

    #[test]
    fn clearing_restores_the_baseline_theme() {
        let state = AccentState::default();
        let theme = UiConfig {
            text_color: Some(Color::Rgb(1, 2, 3)),
            ..UiConfig::default()
        };
        let _ = state.record(Some(accent(AccentSource::Kde, red())), &theme);
        // The live theme now carries the accent; clearing must hand back the
        // snapshot taken before it.
        let live = UiConfig::default();
        match state.record(None, &live) {
            AccentChange::Cleared(restored) => {
                assert_eq!(restored.text_color, Some(Color::Rgb(1, 2, 3)));
            }
            other => panic!("expected a restore, got {other:?}"),
        }
    }

    #[test]
    fn clearing_without_a_baseline_restores_the_current_theme() {
        let state = AccentState::default();
        let theme = UiConfig::default();
        assert!(matches!(state.record(None, &theme), AccentChange::Unchanged));
    }

    #[test]
    fn seeding_an_applied_accent_counts_as_active() {
        let state = AccentState::default();
        state.seed(Some(accent(AccentSource::Sttm, red())), Some(UiConfig::default()));
        assert_eq!(state.applied(), Some(accent(AccentSource::Sttm, red())));
        // A first recorded clear still yields the seeded baseline.
        assert!(matches!(
            state.record(None, &UiConfig::default()),
            AccentChange::Cleared(_)
        ));
    }

    /// A throwaway directory for the file-based sources (the repo has no
    /// tempfile dev-dependency; the fixture cache uses the same approach).
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "s2udio-accent-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        fn write(&self, name: &str, content: &str) -> String {
            let path = self.0.join(name);
            std::fs::write(&path, content).expect("temp file");
            path.display().to_string()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Point a config at a temp dir with a blur schedule + blsw and a matugen
    /// palette (all file-based, so no desktop is needed).
    fn file_sources(dir: &TempDir) -> AutoThemeConfig {
        let schedule = dir.write("blur-schedule", "_MODE=night\n");
        let blsw = dir.write("blsw", "NIGHT_OUTLINE_COLOR_ACTIVE=\"0,94,138\"\n");
        let matugen = dir.write(
            "colors.json",
            r##"{ "colors": { "primary": { "default": { "color": "#ff6673" } } } }"##,
        );
        AutoThemeConfig {
            source: crate::config::auto_theme::AutoThemeSource::Auto,
            desaturate: 0.0,
            sttm_schedule: schedule,
            sttm_script: blsw,
            matugen_json: Some(matugen),
            kdeglobals: dir.write("kdeglobals", "[General]\nAccentColor=1,2,3\n"),
            ..AutoThemeConfig::default()
        }
    }

    #[test]
    fn the_blur_setup_wins_auto() {
        let dir = TempDir::new("priority-sttm");
        let cfg = file_sources(&dir);
        assert_eq!(
            resolve(&cfg),
            Some(accent(AccentSource::Sttm, Color::Rgb(0, 94, 138))),
            "an existing blur-mode setup keeps winning"
        );
    }

    #[test]
    fn matugen_is_the_second_choice() {
        let dir = TempDir::new("priority-matugen");
        let mut cfg = file_sources(&dir);
        // No blur mode scheduled any more: matugen takes over.
        cfg.sttm_schedule = dir.0.join("missing-schedule").display().to_string();
        assert_eq!(
            resolve(&cfg),
            Some(accent(AccentSource::Matugen, Color::Rgb(0xff, 0x66, 0x73)))
        );
    }

    #[test]
    fn an_explicit_source_ignores_the_others() {
        let dir = TempDir::new("explicit");
        let mut cfg = file_sources(&dir);
        cfg.source = crate::config::auto_theme::AutoThemeSource::Matugen;
        assert_eq!(
            resolve(&cfg),
            Some(accent(AccentSource::Matugen, Color::Rgb(0xff, 0x66, 0x73)))
        );
        cfg.source = crate::config::auto_theme::AutoThemeSource::Sttm;
        assert_eq!(
            resolve(&cfg),
            Some(accent(AccentSource::Sttm, Color::Rgb(0, 94, 138)))
        );
    }

    #[test]
    fn the_desaturate_knob_applies_to_the_resolved_accent() {
        let dir = TempDir::new("desaturate");
        let mut cfg = file_sources(&dir);
        cfg.source = crate::config::auto_theme::AutoThemeSource::Matugen;
        cfg.desaturate = 0.4;
        let raw = Color::Rgb(0xff, 0x66, 0x73);
        assert_eq!(
            resolve(&cfg),
            Some(accent(AccentSource::Matugen, desaturate(raw, 0.4)))
        );
        assert_ne!(resolve(&cfg).map(|a| a.color), Some(raw));
    }

    #[test]
    fn patching_the_baseline_keeps_the_newest_configuration() {
        let state = AccentState::default();
        let theme = UiConfig {
            text_color: Some(Color::Rgb(9, 9, 9)),
            ..UiConfig::default()
        };
        let _ = state.record(Some(accent(AccentSource::Sttm, red())), &theme);
        let edited = Color::Rgb(1, 2, 3);
        state.patch_baseline(|theme| theme.text_color = Some(edited));
        match state.record(None, &UiConfig::default()) {
            AccentChange::Cleared(restored) => {
                assert_eq!(restored.text_color, Some(edited), "the edit is what returns");
            }
            other => panic!("expected a restore, got {other:?}"),
        }
    }

    #[test]
    fn patching_without_a_baseline_is_a_no_op() {
        let state = AccentState::default();
        state.patch_baseline(|theme| theme.text_color = Some(red()));
        assert!(state.baseline().is_none(), "no accent, no baseline to patch");
    }

    #[test]
    fn only_one_resolve_runs_at_a_time() {
        let state = AccentState::default();
        assert!(state.try_begin_resolve());
        assert!(!state.try_begin_resolve());
        state.finish_resolve();
        assert!(state.try_begin_resolve());
    }
}
