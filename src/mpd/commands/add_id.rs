use crate::mpd::{FromMpd, LineHandled, ParseErrorExt, errors::MpdError};

/// Response of `addid`: the queue id of the newly added song.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AddId(pub u32);

impl FromMpd for AddId {
    fn next_internal(&mut self, key: &str, value: String) -> Result<LineHandled, MpdError> {
        if key == "id" {
            self.0 = value.parse().logerr(key, &value)?;
        }
        Ok(LineHandled::Yes)
    }
}
