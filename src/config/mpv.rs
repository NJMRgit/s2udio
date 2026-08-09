use serde::{Deserialize, Serialize};

/// The OS language as an ISO 639-1 code from the locale environment
/// (`LC_ALL` / `LC_MESSAGES` / `LANG` / `LC_CTYPE`), e.g. `en_US.UTF-8` ->
/// `en`. `C`/`POSIX` locales (and unset variables) yield None — mpv then
/// picks the original track.
pub fn os_language_code() -> Option<String> {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG", "LC_CTYPE"] {
        if let Ok(value) = std::env::var(var) {
            let code = value
                .split(['_', '.', '@'])
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !code.is_empty() && code != "c" && code != "posix" {
                return Some(code);
            }
        }
    }
    None
}

/// Audio language preference when playing video in mpv — a preference chain:
/// first the system language or a chosen language, then "original" (mpv's
/// default track). Realized with `--alang`: mpv selects a track matching the
/// preferred language when one exists and falls back to the default
/// (original) track otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MpvAudioLang {
    /// 1st: the track matching the OS language; 2nd: original.
    #[default]
    #[serde(rename = "system")]
    System,
    /// 1st: the chosen language; 2nd: original.
    #[serde(rename = "custom")]
    Custom {
        lang: String,
    },
}

impl MpvAudioLang {
    /// String form persisted to state.ron: `system` / `custom:<lang>`.
    /// Legacy values (`en`, empty) parse back too.
    pub fn as_str(&self) -> String {
        match self {
            Self::System => "system".to_owned(),
            Self::Custom { lang } => format!("custom:{lang}"),
        }
    }

    /// The first preference, as shown in the settings panel.
    pub fn label(&self) -> String {
        match self {
            Self::System => "system language".to_owned(),
            Self::Custom { lang } => lang.clone(),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty()
            || s.eq_ignore_ascii_case("system")
            || s.eq_ignore_ascii_case("system language")
        {
            return Some(Self::System);
        }
        // `custom:<lang>`, or a bare language code (legacy "en" and friends).
        Some(match s.strip_prefix("custom:") {
            Some(lang) => Self::Custom { lang: lang.to_owned() },
            None => Self::Custom { lang: s.to_owned() },
        })
    }

    /// The `--alang` value: the OS language code for System (None when the
    /// locale can't be determined — mpv then picks the original track), the
    /// chosen code otherwise.
    pub fn alang(&self) -> Option<String> {
        match self {
            Self::System => os_language_code(),
            Self::Custom { lang } => Some(lang.clone()),
        }
    }
}

/// Subtitle preference when playing video in mpv — a preference chain: first
/// "signs" (forced subtitle tracks), then a second preference (hidden /
/// system language / a chosen language). Realized with the forced-track
/// flags plus the second preference: mpv shows forced tracks when present,
/// otherwise the second preference applies.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MpvSubtitleMode {
    /// Signs first; if no forced track, no subtitles at all.
    #[default]
    #[serde(rename = "hidden")]
    Hidden,
    /// Signs first; else subtitles matching the system/OS language.
    #[serde(rename = "system")]
    SystemLanguage,
    /// Signs first; else subtitles in a specific language.
    #[serde(rename = "custom")]
    Custom {
        lang: String,
    },
}

impl MpvSubtitleMode {
    /// String form persisted to state.ron; custom carries its code as
    /// `custom:<lang>`. Legacy "signs"/"off" map to "hidden" on parse.
    pub fn as_str(&self) -> String {
        match self {
            Self::Hidden => "hidden".to_owned(),
            Self::SystemLanguage => "system".to_owned(),
            Self::Custom { lang } => format!("custom:{lang}"),
        }
    }

    /// The second preference, as shown in the settings panel.
    pub fn label(&self) -> String {
        match self {
            Self::Hidden => "hidden".to_owned(),
            Self::SystemLanguage => "system language".to_owned(),
            Self::Custom { lang } => lang.clone(),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            // Legacy "signs" (forced tracks only) and "off" (no subtitles)
            // both become the "signs > hidden" chain.
            "signs" | "off" | "hidden" => Some(Self::Hidden),
            "system" | "system language" => Some(Self::SystemLanguage),
            _ => s
                .trim()
                .strip_prefix("custom:")
                .map(|lang| Self::Custom { lang: lang.to_owned() }),
        }
    }
}

/// mpv launch preferences: the audio language chain (system/chosen >
/// original) and the subtitle chain (signs > hidden/system/chosen).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mpv {
    pub audio_lang: MpvAudioLang,
    pub subtitles: MpvSubtitleMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct MpvFile {
    pub audio_lang: Option<MpvAudioLang>,
    pub subtitles: Option<MpvSubtitleMode>,
}


impl From<MpvFile> for Mpv {
    fn from(value: MpvFile) -> Self {
        Self {
            audio_lang: value.audio_lang.unwrap_or_default(),
            subtitles: value.subtitles.unwrap_or_default(),
        }
    }
}
