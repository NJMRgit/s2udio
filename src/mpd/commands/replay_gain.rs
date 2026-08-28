use anyhow::anyhow;
use serde::Serialize;

use crate::mpd::{FromMpd, LineHandled, ParseErrorExt, errors::MpdError};

/// The MPD per-client replay gain mode (SET `replay_gain_mode
/// {off|track|album}`, GET `replay_gain_status` -> `replay_gain_mode:`).
/// Round 53: exposed in Settings > MPD and persisted/restored across
/// connects.
///
/// NOTE (round 53, live-verified against MPD 0.24 + source 0.18-0.24): the
/// SET command is `replay_gain_mode`, not the pre-0.18 `replay_gain` name —
/// modern MPD answers "unknown command" for `replay_gain`.
///
/// Preamp/limit (`replaygain_preamp`, `replaygain_limit`) are mpd.conf-only
/// server options — NOT protocol-settable — so they stay out of scope here.
#[derive(Debug, Serialize, Default, PartialEq, Eq, Clone, Copy)]
pub enum ReplayGain {
    #[default]
    Off,
    Track,
    Album,
}

impl ReplayGain {
    /// The cycle order for the settings row: off -> track -> album -> off.
    pub const ALL: [Self; 3] = [Self::Off, Self::Track, Self::Album];

    /// The MPD wire value ("off" / "track" / "album").
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Track => "track",
            Self::Album => "album",
        }
    }
}

impl std::fmt::Display for ReplayGain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Off => "Off",
                Self::Track => "Track",
                Self::Album => "Album",
            }
        )
    }
}

impl std::str::FromStr for ReplayGain {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "track" => Ok(Self::Track),
            "album" => Ok(Self::Album),
            val => Err(anyhow!("Received unknown value for ReplayGain '{val}'")),
        }
    }
}

impl FromMpd for ReplayGain {
    fn next_internal(&mut self, key: &str, value: String) -> Result<LineHandled, MpdError> {
        if key == "replay_gain_mode" {
            *self = value.parse().logerr(key, &value)?;
            Ok(LineHandled::Yes)
        } else {
            Ok(LineHandled::No { value })
        }
    }
}
