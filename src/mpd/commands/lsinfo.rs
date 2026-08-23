use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use derive_more::{AsMut, AsRef, Into, IntoIterator};
use super::Song;
use crate::mpd::{FromMpd, LineHandled, ParseErrorExt, errors::MpdError};
#[derive(Debug, Default, IntoIterator, AsRef, AsMut, Into)]
pub struct LsInfo(pub Vec<LsInfoEntry>);
impl LsInfo {
    pub fn into_files(self) -> impl Iterator<Item = String> {
        self.into_iter()
            .filter_map(|item| match item {
                LsInfoEntry::File(song) => Some(song.file),
                LsInfoEntry::Dir(_) => None,
                LsInfoEntry::Playlist(_) => None,
            })
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum LsInfoEntry {
    Dir(Dir),
    File(Song),
    Playlist(Playlist),
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Dir {
    /// Last segment of the path, the dir name
    pub name: String,
    /// this is the full path from mpd root
    pub full_path: String,
    pub last_modified: DateTime<Utc>,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Playlist {
    /// Last segment of the path, the playlist name
    pub name: String,
    /// this is the full path from mpd root
    pub full_path: String,
    pub last_modified: DateTime<Utc>,
}
impl FromMpd for Dir {
    fn next_internal(
        &mut self,
        key: &str,
        value: String,
    ) -> Result<LineHandled, MpdError> {
        match key {
            "directory" => {
                value
                    .split('/')
                    .next_back()
                    .context(
                        anyhow!(
                            "Failed to parse dir name. Key: '{key}' Value: '{value}'"
                        ),
                    )?
                    .clone_into(&mut self.name);
                self.full_path = value;
            }
            "last-modified" => {
                self.last_modified = value
                    .parse()
                    .context("failed to parse date")
                    .logerr(key, &value)?;
            }
            _ => return Ok(LineHandled::No { value }),
        }
        Ok(LineHandled::Yes)
    }
}
impl FromMpd for Playlist {
    fn next_internal(
        &mut self,
        key: &str,
        value: String,
    ) -> Result<LineHandled, MpdError> {
        match key {
            "playlist" => {
                value
                    .split('/')
                    .next_back()
                    .context(
                        anyhow!(
                            "Failed to parse playlist name. Key: '{key}' Value: '{value}'"
                        ),
                    )?
                    .clone_into(&mut self.name);
                self.full_path = value;
            }
            "last-modified" => {
                self.last_modified = value
                    .parse()
                    .context("failed to parse date")
                    .logerr(key, &value)?;
            }
            _ => return Ok(LineHandled::No { value }),
        }
        Ok(LineHandled::Yes)
    }
}
impl FromMpd for LsInfo {
    fn next_internal(
        &mut self,
        key: &str,
        value: String,
    ) -> Result<LineHandled, MpdError> {
        if key == "file" {
            self.0.push(LsInfoEntry::File(Song::default()));
        }
        if key == "directory" {
            self.0.push(LsInfoEntry::Dir(Dir::default()));
        }
        if key == "playlist" {
            self.0.push(LsInfoEntry::Playlist(Playlist::default()));
        }
        match self
            .0
            .last_mut()
            .context(
                anyhow!(
                    "No element in accumulator while parsing LsInfo. Key '{key}' Value :'{value}'"
                ),
            )?
        {
            LsInfoEntry::Dir(dir) => dir.next_internal(key, value),
            LsInfoEntry::File(song) => song.next_internal(key, value),
            LsInfoEntry::Playlist(playlist) => playlist.next_internal(key, value),
        }
    }
}
