use std::{
    collections::BTreeMap, io::BufReader, path::{Path, PathBuf},
    time::Duration,
};
use anyhow::{Context, Result};
use itertools::Itertools;
use serde::Serialize;
use unicase::UniCase;
use walkdir::WalkDir;
use crate::{mpd::commands::Song, shared::{lrc::lyrics::LrcMetadata, macros::try_cont}};
/// Index of LRC files for fast song-to-lyrics matching.
///
/// This structure maintains an in-memory index of all LRC files in a directory,
/// allowing for fast lookup of lyrics based on song metadata (artist, title,
/// album). The index is built using efficient metadata-only parsing to avoid
/// processing the entire content of each LRC file during startup.
#[derive(Debug, Default, Serialize)]
pub struct LrcIndex {
    index: BTreeMap<PathBuf, LrcMetadata>,
}
impl LrcIndex {
    pub fn index(lyrics_dir: &Path) -> Self {
        let start = std::time::Instant::now();
        let dir = WalkDir::new(lyrics_dir);
        log::info!(dir:?; "Starting lyrics index lyrics");
        let mut index = BTreeMap::new();
        for child in dir {
            let child = try_cont!(child, "skipping child");
            let child = child.path();
            let metadata = try_cont!(
                Self::index_single(child), "Failed to parse as index entry"
            );
            let Some(metadata) = metadata else {
                log::trace!(
                    child:?; "Entry did not have enough metadata to index, skipping"
                );
                continue;
            };
            log::trace!(child:?; "Successfully indexed lyrics file");
            {
                use std::collections::btree_map::Entry;
                match index.entry(child.to_path_buf()) {
                    Entry::Occupied(mut entry) => {
                        entry.insert(metadata);
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(metadata);
                    }
                }
            }
        }
        log::info!(
            found_count = index.len(), elapsed:? = start.elapsed(); "Indexed lrc files"
        );
        Self { index }
    }
    pub fn index_single(path: &Path) -> Result<Option<LrcMetadata>> {
        if path.extension().is_none_or(|ext| !ext.to_string_lossy().ends_with("lrc")) {
            log::trace!(path:?; "skipping non lrc file");
            return Ok(None);
        }
        let file = std::fs::File::open(path).context("failed to open a lyrics file")?;
        log::trace!(file:?, path:?; "Trying to index lyrics file");
        LrcMetadata::read(BufReader::new(file)).context("Failed to read a lyrics file")
    }
    fn album_matches(metadata: &LrcMetadata, song_album: Option<&str>) -> bool {
        match (&metadata.album, song_album) {
            (Some(meta_album), Some(song_album)) => {
                UniCase::new(meta_album) == UniCase::new(song_album)
            }
            _ => true,
        }
    }
    fn album_matches_exactly(metadata: &LrcMetadata, song_album: Option<&str>) -> bool {
        match (&metadata.album, song_album) {
            (Some(meta_album), Some(song_album)) => {
                UniCase::new(meta_album) == UniCase::new(song_album)
            }
            _ => false,
        }
    }
    pub(crate) fn find_entry(&self, song: &Song) -> Option<(&Path, &LrcMetadata)> {
        let artist = song.metadata.get("artist").map(|v| v.last())?;
        let title = song.metadata.get("title").map(|v| v.last())?;
        let album = song.metadata.get("album").map(|v| v.last());
        let mut results = self
            .index
            .iter()
            .filter(|(_, metadata)| {
                let artist_matches = metadata
                    .artist
                    .as_ref()
                    .is_some_and(|a| UniCase::new(a) == UniCase::new(artist));
                let title_matches = metadata
                    .title
                    .as_ref()
                    .is_some_and(|t| UniCase::new(t) == UniCase::new(title));
                artist_matches && title_matches && Self::album_matches(metadata, album)
            })
            .collect_vec();
        match results[..] {
            [] => {
                log::trace!(artist, title, album; "No Lyrics found for song");
                return None;
            }
            [result] => {
                log::trace!(
                    artist, title, album; "Found exactly one Lyrics entry for song"
                );
                let (path, metadata) = result;
                return Some((path, metadata));
            }
            _ => {}
        }
        log::trace!(
            artist, title, album;
            "Found multiple Lyrics entries for song, getting closest match by length"
        );
        let Some(target_duration) = song.duration else {
            let (path, metadata) = results[0];
            return Some((path, metadata));
        };
        if results
            .iter()
            .any(|(_, metadata)| Self::album_matches_exactly(metadata, album))
        {
            results.retain(|(_, metadata)| Self::album_matches_exactly(metadata, album));
        }
        let (with_length, without_length): (Vec<_>, Vec<_>) = results
            .into_iter()
            .partition(|(_, e)| e.length.is_some());
        let closest_length_candidate = with_length
            .iter()
            .filter(|(_, metadata)| {
                metadata
                    .length
                    .is_some_and(|l| {
                        l.abs_diff(target_duration) < Duration::from_secs(5)
                    })
            })
            .min_by_key(|(_, e)| e.length);
        if let Some(&(path, metadata)) = closest_length_candidate {
            return Some((path, metadata));
        }
        if !without_length.is_empty() {
            let (path, metadata) = without_length[0];
            return Some((path, metadata));
        }
        with_length
            .iter()
            .sorted_by(|(_, a), (_, b)| {
                a.length
                    .unwrap_or_default()
                    .abs_diff(target_duration)
                    .cmp(&b.length.unwrap_or_default().abs_diff(target_duration))
            })
            .next()
            .map(|&(p, e)| (p.as_path(), e))
    }
    pub(crate) fn add(&mut self, path: PathBuf, metadata: LrcMetadata) {
        use std::collections::btree_map::Entry;
        match self.index.entry(path) {
            Entry::Occupied(mut entry) => {
                entry.insert(metadata);
            }
            Entry::Vacant(entry) => {
                entry.insert(metadata);
            }
        }
    }
}
