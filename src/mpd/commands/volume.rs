use derive_more::AsRef;
use serde::Serialize;

use crate::mpd::{FromMpd, LineHandled, errors::MpdError};

#[derive(Debug, Serialize, Default, PartialEq, AsRef, Clone, Copy)]
pub struct Volume(u32);

impl Bound<u32> for Volume {
    fn value(&self) -> &u32 {
        &self.0
    }

    fn set_value(&mut self, value: u32) -> &Self {
        self.0 = value.clamp(0, 100);
        self
    }

    fn inc(&mut self) -> &Self {
        if self.0 < 100 {
            self.0 += 1;
        }
        self
    }

    fn inc_by(&mut self, step: u32) -> &Self {
        self.0 = self.0.saturating_add(step).min(100);
        self
    }

    fn dec(&mut self) -> &Self {
        if self.0 > 0 {
            self.0 -= 1;
        }
        self
    }

    fn dec_by(&mut self, step: u32) -> &Self {
        self.0 = self.0.saturating_sub(step);
        self
    }
}

impl Volume {
    pub fn new(value: u32) -> Self {
        Self(value.clamp(0, 100))
    }
}

#[allow(dead_code)]
pub trait Bound<T> {
    fn value(&self) -> &T;
    fn set_value(&mut self, value: T) -> &Self;
    fn inc(&mut self) -> &Self;
    fn inc_by(&mut self, step: T) -> &Self;
    fn dec(&mut self) -> &Self;
    fn dec_by(&mut self, step: T) -> &Self;
}

impl FromMpd for Volume {
    fn next_internal(&mut self, key: &str, value: String) -> Result<LineHandled, MpdError> {
        if key == "volume" {
            // -1 = no mixer (see `commands::status`): full volume plays
            // out, so report 100 instead of failing the parse / showing 0.
            self.0 = if value == "-1" { 100 } else { value.parse()? };
            Ok(LineHandled::Yes)
        } else {
            Ok(LineHandled::No { value })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Bound as _, Volume};
    use crate::mpd::FromMpd;

    #[test]
    fn no_mixer_volume_parses_as_full() {
        let mut volume = Volume::default();
        volume.next_internal("volume", "-1".to_owned()).unwrap();
        assert_eq!(*volume.value(), 100);

        volume.next_internal("volume", "65".to_owned()).unwrap();
        assert_eq!(*volume.value(), 65);
    }
}
