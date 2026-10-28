//! Auto theming: follow the desktop's accent color instead of the theme
//! file's static accent.
//!
//! Three sources are supported, in this priority order when `source` is
//! `Auto`:
//!
//! 1. `Sttm` — the blur-mode scheduler (`~/.blur-schedule` + `blsw`), the
//!    original s2udio integration; kept first so an existing blur-mode setup
//!    keeps winning exactly as it did before auto theming existed.
//! 2. `Matugen` — a matugen palette JSON (`~/.cache/matugen/colors.json` by
//!    convention), the wallpaper-derived accent used across many WMs/shells.
//! 3. `Kde` — the Plasma accent color, read from the XDG desktop portal
//!    (`org.freedesktop.appearance` `accent-color`) with `kdeglobals` as the
//!    fallback.
//!
//! Whichever source resolves first supplies one accent color; the watcher
//! writes it into `theme.text_color` and `derive_theme_accents` derives the
//! borders, selection, cava bars, seekbar and tab highlights from it — the
//! same pipeline the blur watcher has always used.
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

/// One concrete source of the auto-theme accent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccentSource {
    Sttm,
    Matugen,
    Kde,
}

impl AccentSource {
    /// Lowercase name used in logs, `state.ron` and the settings row.
    pub fn as_str(self) -> &'static str {
        match self {
            AccentSource::Sttm => "sttm",
            AccentSource::Matugen => "matugen",
            AccentSource::Kde => "kde",
        }
    }
}

/// The `auto_theme.source` selector (Settings -> Appearance, "auto theme").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoThemeSource {
    /// Never touch the theme (the theme file's own accent stays).
    Off,
    /// First available of [`AutoThemeSource::AUTO_PRIORITY`].
    #[default]
    Auto,
    Sttm,
    Matugen,
    Kde,
}

impl AutoThemeSource {
    /// The priority order `Auto` resolves in: the blur-mode setup first (so
    /// an existing STTM install keeps its exact look), then matugen, then the
    /// Plasma accent.
    pub const AUTO_PRIORITY: [AccentSource; 3] =
        [AccentSource::Sttm, AccentSource::Matugen, AccentSource::Kde];

    /// Every selector value, in the settings row's cycle order.
    pub const ALL: [AutoThemeSource; 5] = [
        AutoThemeSource::Off,
        AutoThemeSource::Auto,
        AutoThemeSource::Sttm,
        AutoThemeSource::Matugen,
        AutoThemeSource::Kde,
    ];

    /// Lowercase name used in logs, `state.ron` and the settings row.
    pub fn as_str(self) -> &'static str {
        match self {
            AutoThemeSource::Off => "off",
            AutoThemeSource::Auto => "auto",
            AutoThemeSource::Sttm => "sttm",
            AutoThemeSource::Matugen => "matugen",
            AutoThemeSource::Kde => "kde",
        }
    }

    /// Parse a persisted / user-facing name (`state.ron`, config edits).
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|source| source.as_str() == value)
    }

    /// The providers this selector allows, in priority order. `Off` allows
    /// none, so the watcher resolves nothing and leaves the theme alone.
    pub fn providers(self) -> &'static [AccentSource] {
        match self {
            AutoThemeSource::Off => &[],
            AutoThemeSource::Auto => &Self::AUTO_PRIORITY,
            AutoThemeSource::Sttm => &[AccentSource::Sttm],
            AutoThemeSource::Matugen => &[AccentSource::Matugen],
            AutoThemeSource::Kde => &[AccentSource::Kde],
        }
    }
}

/// Which variant of a matugen palette the accent comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatugenMode {
    /// The variant matugen itself rendered (`colors.<key>.default`), i.e. the
    /// palette the rest of the desktop was just themed with.
    #[default]
    Default,
    Dark,
    Light,
}

impl MatugenMode {
    /// Lowercase name used in logs and config documentation.
    pub fn as_str(self) -> &'static str {
        match self {
            MatugenMode::Default => "default",
            MatugenMode::Dark => "dark",
            MatugenMode::Light => "light",
        }
    }

    /// The palette JSON key holding this variant.
    pub(crate) fn palette_key(self) -> &'static str {
        self.as_str()
    }

}

/// The resolved accent the watcher applies: which source supplied it and the
/// color (already run through the `desaturate` transform).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedAccent {
    pub source: AccentSource,
    pub color: Color,
}

/// The `auto_theme` section of `config.ron`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoThemeConfig {
    /// Which accent source to follow. Default `Auto` keeps an existing
    /// blur-mode setup working untouched and picks up matugen / Plasma
    /// automatically for everyone else.
    pub source: AutoThemeSource,
    /// How much to mute the accent toward grey, 0.0..=1.0. `0.0` uses the
    /// source's raw color; `0.4` (the default) is the blur watcher's
    /// historical muting.
    pub desaturate: f32,
    /// The blur-mode schedule file (`_MODE=<mode>`).
    pub sttm_schedule: String,
    /// The blur-mode color script. **Read, never executed.**
    pub sttm_script: String,
    /// Read the matugen palette from this file only. `None` (the default)
    /// discovers one: `~/.cache/matugen/colors.json`, then
    /// `~/.config/matugen/colors.json`, then the newest file in matugen's own
    /// cache (`~/.cache/matugen/images/*.json`, written with
    /// `caching = true`).
    pub matugen_json: Option<String>,
    /// Which palette color supplies the accent (`colors.<key>`).
    pub matugen_color: String,
    /// Which variant of that palette color to use.
    pub matugen_mode: MatugenMode,
    /// Plasma's `kdeglobals`, the fallback when the portal has no answer.
    pub kdeglobals: String,
}

impl Default for AutoThemeConfig {
    fn default() -> Self {
        Self {
            source: AutoThemeSource::default(),
            desaturate: 0.4,
            sttm_schedule: "~/.blur-schedule".to_owned(),
            sttm_script: "~/.local/bin/blsw".to_owned(),
            matugen_json: None,
            matugen_color: "primary".to_owned(),
            matugen_mode: MatugenMode::default(),
            kdeglobals: "~/.config/kdeglobals".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_the_blur_setup_first() {
        let cfg = AutoThemeConfig::default();
        assert_eq!(cfg.source, AutoThemeSource::Auto);
        assert_eq!(cfg.desaturate, 0.4);
        assert_eq!(cfg.source.providers(), &AutoThemeSource::AUTO_PRIORITY);
        assert_eq!(cfg.matugen_color, "primary");
    }

    #[test]
    fn auto_priority_is_sttm_then_matugen_then_kde() {
        assert_eq!(
            AutoThemeSource::Auto.providers(),
            &[AccentSource::Sttm, AccentSource::Matugen, AccentSource::Kde]
        );
    }

    #[test]
    fn off_allows_no_provider() {
        assert!(AutoThemeSource::Off.providers().is_empty());
    }

    #[test]
    fn single_source_selectors_allow_one_provider() {
        for (selector, source) in [
            (AutoThemeSource::Sttm, AccentSource::Sttm),
            (AutoThemeSource::Matugen, AccentSource::Matugen),
            (AutoThemeSource::Kde, AccentSource::Kde),
        ] {
            assert_eq!(selector.providers(), &[source]);
        }
    }

    #[test]
    fn selectors_round_trip_through_their_names() {
        for selector in AutoThemeSource::ALL {
            assert_eq!(AutoThemeSource::parse(selector.as_str()), Some(selector));
        }
        assert_eq!(AutoThemeSource::parse("AUTO"), Some(AutoThemeSource::Auto));
        assert_eq!(AutoThemeSource::parse(" plasma "), None);
        assert_eq!(AutoThemeSource::parse(""), None);
    }

    #[test]
    fn the_section_parses_from_ron() {
        let cfg: AutoThemeConfig = ron::de::from_str(
            r#"(
                source: Kde,
                desaturate: 0.0,
                matugen_json: Some("/tmp/matugen.json"),
                matugen_mode: Dark,
            )"#,
        )
        .expect("the auto_theme section parses");
        assert_eq!(cfg.source, AutoThemeSource::Kde);
        assert_eq!(cfg.desaturate, 0.0);
        assert_eq!(cfg.matugen_json.as_deref(), Some("/tmp/matugen.json"));
        assert_eq!(cfg.matugen_mode, MatugenMode::Dark);
        // Unmentioned keys fall back to the defaults.
        assert_eq!(cfg.matugen_color, "primary");
        assert_eq!(cfg.sttm_schedule, "~/.blur-schedule");
    }

    #[test]
    fn a_missing_section_is_the_default() {
        let cfg: AutoThemeConfig = ron::de::from_str("()").expect("an empty section parses");
        assert_eq!(cfg, AutoThemeConfig::default());
    }

    #[test]
    fn matugen_mode_names_match_the_palette_keys() {
        assert_eq!(MatugenMode::Default.palette_key(), "default");
        assert_eq!(MatugenMode::Dark.palette_key(), "dark");
        assert_eq!(MatugenMode::Light.palette_key(), "light");
        for mode in [MatugenMode::Default, MatugenMode::Dark, MatugenMode::Light] {
            assert!(!mode.as_str().is_empty());
            assert_eq!(mode.as_str(), mode.palette_key());
        }
    }

    #[test]
    fn the_shipped_example_config_documents_the_section() {
        // `assets/example_config.ron` is compiled in and documents every
        // option: a new key has to appear there too, and the example has to
        // parse back into the same values.
        let example = include_str!("../../assets/example_config.ron");
        assert!(
            example.contains("auto_theme: ("),
            "the example config must document the auto_theme section"
        );
        let file: crate::config::ConfigFile =
            ron::de::from_str(example).expect("the example config parses");
        assert_eq!(file.auto_theme.source, AutoThemeSource::Auto);
        assert_eq!(file.auto_theme.desaturate, 0.4);
        assert_eq!(file.auto_theme.matugen_color, "primary");
        assert_eq!(file.auto_theme.matugen_mode, MatugenMode::Default);
    }

    #[test]
    fn applied_accent_compares_by_source_and_color() {
        let sttm_red = AppliedAccent {
            source: AccentSource::Sttm,
            color: Color::Rgb(255, 0, 0),
        };
        assert_eq!(sttm_red, sttm_red);
        assert_ne!(
            sttm_red,
            AppliedAccent {
                source: AccentSource::Kde,
                color: Color::Rgb(255, 0, 0),
            }
        );
    }
}
