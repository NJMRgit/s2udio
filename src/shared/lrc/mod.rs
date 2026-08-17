mod edit;
mod index;
mod lyrics;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
pub use edit::{EditableLine, LrcEditSession};
pub use index::LrcIndex;
pub use lyrics::{Lrc, LrcMetadata};

#[derive(Debug, Default, Clone, Copy)]
pub struct LrcOffset {
    negative: bool,
    value: Duration,
}

impl LrcOffset {
    pub fn from_millis(value: i64) -> Self {
        if value < 0 {
            Self { negative: true, value: Duration::from_millis(-value as u64) }
        } else {
            Self { negative: false, value: Duration::from_millis(value as u64) }
        }
    }
}

fn parse_length(input: &str) -> anyhow::Result<Duration> {
    let (minutes, seconds) = input.split_once(':').context("Invalid lrc length format")?;
    let minutes: u64 = minutes.parse().context("Invalid minutes format in lrc length")?;
    let seconds: u64 = seconds.parse().context("Invalid seconds format in lrc length")?;
    Ok(Duration::from_secs(minutes * 60 + seconds))
}

/// The `.lrc` path colocated with an audio file: same directory, stem +
/// `.lrc` (e.g. `…/Artist/Album/01 Track.flac` → `…/Artist/Album/01
/// Track.lrc`).
pub(crate) fn colocated_lrc_path(song_path: &Path) -> Result<PathBuf> {
    let Some(stem) = song_path.file_stem().map(|stem| format!("{}.lrc", stem.to_string_lossy()))
    else {
        bail!("No file stem for lyrics path: {}", song_path.display());
    };
    let mut path = song_path.to_path_buf();
    path.set_file_name(stem);
    Ok(path)
}

/// The `.lrc` path for a song inside a lyrics directory: `lyrics_dir`
/// joined with the song file, extension swapped to `.lrc`. With a
/// relative MPD song path this mirrors the library layout inside
/// `lyrics_dir` (round 23: s2udio's own lyrics library).
pub(crate) fn get_lrc_path(lyrics_dir: &str, song_file: &str) -> Result<PathBuf> {
    let mut path: PathBuf = PathBuf::from(lyrics_dir);
    path.push(song_file);
    colocated_lrc_path(&path)
}
