use std::time::Duration;
use anyhow::anyhow;
use serde::Serialize;
use super::Volume;
use crate::mpd::{FromMpd, LineHandled, ParseErrorExt, errors::MpdError};
#[derive(Debug, Serialize, Default, Clone)]
pub struct Status {
    pub partition: String,
    pub volume: Volume,
    pub repeat: bool,
    pub random: bool,
    pub single: OnOffOneshot,
    pub consume: OnOffOneshot,
    pub playlist: Option<u32>,
    pub playlistlength: u32,
    pub state: State,
    pub song: Option<usize>,
    pub songid: Option<u32>,
    pub nextsong: Option<u32>,
    pub nextsongid: Option<u32>,
    pub elapsed: Duration,
    pub duration: Duration,
    pub bitrate: Option<u32>,
    pub xfade: Option<u32>,
    pub mixrampdb: Option<String>,
    pub mixrampdelay: Option<String>,
    pub audio: Option<String>,
    pub updating_db: Option<u32>,
    pub error: Option<String>,
    pub lastloadedplaylist: Option<String>,
}
impl Status {
    pub fn samplerate(&self) -> Option<u32> {
        self.audio
            .as_ref()
            .and_then(|audio| {
                audio.split(':').next().and_then(|rate_str| rate_str.parse().ok())
            })
    }
    pub fn bits(&self) -> Option<u32> {
        self.audio
            .as_ref()
            .and_then(|audio| {
                audio.split(':').nth(1).and_then(|bits_str| bits_str.parse().ok())
            })
    }
    pub fn channels(&self) -> Option<u32> {
        self.audio
            .as_ref()
            .and_then(|audio| {
                audio
                    .split(':')
                    .nth(2)
                    .and_then(|channels_str| channels_str.parse().ok())
            })
    }
}
impl FromMpd for Status {
    fn next_internal(
        &mut self,
        key: &str,
        value: String,
    ) -> Result<LineHandled, MpdError> {
        match key {
            "partition" => self.partition = value,
            "volume" if value == "-1" => {
                // -1 = MPD has no mixer (an audio output without one, or
                // nothing played yet). The audio that comes out is full
                // scale, so record 100 rather than 0: a 0 here made every
                // mpv launch start silent (`--volume=<this>`).
                log::warn!(
                    command = "status", key, value = value.as_str();
                    "No mixer, reporting full volume"
                );
                self.volume = Volume::new(100);
            }
            "volume" => self.volume = Volume::new(value.parse().logerr(key, &value)?),
            "repeat" => self.repeat = value != "0",
            "random" => self.random = value != "0",
            "single" => self.single = value.parse().logerr(key, &value)?,
            "consume" => self.consume = value.parse().logerr(key, &value)?,
            "playlist" => self.playlist = Some(value.parse().logerr(key, &value)?),
            "playlistlength" => self.playlistlength = value.parse()?,
            "state" => self.state = value.parse().logerr(key, &value)?,
            "song" => self.song = Some(value.parse().logerr(key, &value)?),
            "songid" => self.songid = Some(value.parse().logerr(key, &value)?),
            "nextsong" => self.nextsong = Some(value.parse().logerr(key, &value)?),
            "nextsongid" => self.nextsongid = Some(value.parse().logerr(key, &value)?),
            "elapsed" => {
                self.elapsed = Duration::from_secs_f32(
                    value.parse().logerr(key, &value)?,
                );
            }
            "duration" => {
                self.duration = Duration::from_secs_f32(
                    value.parse().logerr(key, &value)?,
                );
            }
            "bitrate" if value != "0" => {
                self.bitrate = Some(value.parse().logerr(key, &value)?);
            }
            "xfade" => self.xfade = Some(value.parse().logerr(key, &value)?),
            "mixrampdb" => self.mixrampdb = Some(value),
            "mixrampdelay" => self.mixrampdelay = Some(value),
            "audio" => self.audio = Some(value),
            "updating_db" => self.updating_db = Some(value.parse().logerr(key, &value)?),
            "error" => self.error = Some(value),
            "bitrate" => self.bitrate = None,
            "lastloadedplaylist" => {
                if !value.is_empty() {
                    self.lastloadedplaylist = Some(value);
                }
            }
            "time" => {}
            _ => return Ok(LineHandled::No { value }),
        }
        Ok(LineHandled::Yes)
    }
}
#[derive(Debug, Serialize, Default, PartialEq, Clone, Copy)]
pub enum State {
    Play,
    #[default]
    Stop,
    Pause,
}
#[derive(Debug, Serialize, Default, Clone, Copy, PartialEq, Eq, strum::AsRefStr)]
pub enum OnOffOneshot {
    #[strum(serialize = "On")]
    On,
    #[default]
    #[strum(serialize = "Off")]
    Off,
    #[strum(serialize = "OS")]
    Oneshot,
}
impl OnOffOneshot {
    pub fn cycle(self) -> Self {
        match self {
            OnOffOneshot::On => OnOffOneshot::Off,
            OnOffOneshot::Off => OnOffOneshot::Oneshot,
            OnOffOneshot::Oneshot => OnOffOneshot::On,
        }
    }
    /// Single is always enabled as a oneshot: `Off <-> Oneshot` (a plain
    /// `On` counts as enabled, so clicking it disables).
    pub fn cycle_single(self) -> Self {
        match self {
            OnOffOneshot::On => OnOffOneshot::Off,
            OnOffOneshot::Off => OnOffOneshot::Oneshot,
            OnOffOneshot::Oneshot => OnOffOneshot::Off,
        }
    }
    pub fn cycle_skip_oneshot(self) -> Self {
        match self {
            OnOffOneshot::On => OnOffOneshot::Off,
            OnOffOneshot::Off => OnOffOneshot::On,
            OnOffOneshot::Oneshot => OnOffOneshot::Off,
        }
    }
    pub fn to_mpd_value(self) -> &'static str {
        match self {
            OnOffOneshot::On => "1",
            OnOffOneshot::Off => "0",
            OnOffOneshot::Oneshot => "oneshot",
        }
    }
}
impl std::fmt::Display for OnOffOneshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f, "{}", match self { OnOffOneshot::On => "On", OnOffOneshot::Off => "Off",
            OnOffOneshot::Oneshot => "OS", }
        )?;
        Ok(())
    }
}
impl std::str::FromStr for OnOffOneshot {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "0" => Ok(OnOffOneshot::Off),
            "1" => Ok(OnOffOneshot::On),
            "oneshot" => Ok(OnOffOneshot::Oneshot),
            val => Err(anyhow!("Received unknown value for OnOffOneshot '{val}'")),
        }
    }
}
impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f, "{}", match self { State::Play => "Playing", State::Stop => "Stopped",
            State::Pause => "Paused", }
        )
    }
}
impl std::str::FromStr for State {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "play" => Ok(Self::Play),
            "stop" => Ok(Self::Stop),
            "pause" => Ok(Self::Pause),
            _ => Err(anyhow!("Invalid State: '{s}'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OnOffOneshot;

    #[test]
    fn single_cycles_off_oneshot() {
        // Single is always enabled as a oneshot: off <-> oneshot, and a
        // plain on counts as enabled (clicking it disables).
        assert_eq!(OnOffOneshot::Off.cycle_single(), OnOffOneshot::Oneshot);
        assert_eq!(OnOffOneshot::Oneshot.cycle_single(), OnOffOneshot::Off);
        assert_eq!(OnOffOneshot::On.cycle_single(), OnOffOneshot::Off);
    }

    #[test]
    fn consume_cycles_all_three_states() {
        assert_eq!(OnOffOneshot::Off.cycle(), OnOffOneshot::Oneshot);
        assert_eq!(OnOffOneshot::Oneshot.cycle(), OnOffOneshot::On);
        assert_eq!(OnOffOneshot::On.cycle(), OnOffOneshot::Off);
    }

    /// `volume: -1` means "no mixer": full volume comes out of the output,
    /// so the status must not report 0 (that made every mpv launch silent).
    #[test]
    fn no_mixer_volume_is_full_not_zero() {
        use crate::{
            mpd::FromMpd,
            mpd::commands::{Volume, volume::Bound as _},
        };

        let mut status = super::Status::default();
        status.next_internal("volume", "-1".to_owned()).unwrap();
        assert_eq!(*status.volume.value(), 100);

        status.next_internal("volume", "42".to_owned()).unwrap();
        assert_eq!(*status.volume.value(), 42);
        assert_eq!(Volume::new(120).value(), &100, "clamped");
    }
}
