use std::{cmp::Ordering, str::FromStr};
use crate::{
    config::{
        ShowPlaylistsMode, sort_mode::{SortMode, SortOptions},
        theme::{TagResolutionStrategy, properties::SongProperty},
    },
    mpd::commands::{Song, lsinfo::{Dir, LsInfoEntry}},
    shared::cmp::StringCompare,
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DirOrSong {
    Dir {
        name: String,
        full_path: String,
        last_modified: chrono::DateTime<chrono::Utc>,
        playlist: bool,
    },
    Song(Song),
}
impl DirOrSong {
    pub fn name_only(name: String) -> Self {
        DirOrSong::Dir {
            name,
            full_path: String::new(),
            last_modified: chrono::Utc::now(),
            playlist: false,
        }
    }
    pub fn playlist_name_only(name: String) -> Self {
        DirOrSong::Dir {
            name,
            full_path: String::new(),
            last_modified: chrono::Utc::now(),
            playlist: true,
        }
    }
    pub fn last_modified(&self) -> chrono::DateTime<chrono::Utc> {
        match self {
            DirOrSong::Dir { last_modified, .. } => *last_modified,
            DirOrSong::Song(song) => song.last_modified,
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SongCustomSort<'a, 'opts> {
    song: &'a Song,
    opts: &'opts SortOptions,
}
impl Song {
    pub(crate) fn with_custom_sort<'song, 'opts>(
        &'song self,
        opts: &'opts SortOptions,
    ) -> SongCustomSort<'song, 'opts> {
        SongCustomSort { song: self, opts }
    }
}
impl Ord for SongCustomSort<'_, '_> {
    fn cmp(&self, other: &Self) -> Ordering {
        match &self.opts.mode {
            SortMode::Format(items) => {
                let ignore_the = self.opts.ignore_leading_the;
                let mut a_is_leading = true;
                let mut b_is_leading = true;
                for prop in items {
                    let result = CmpByProp::song_cmp(
                        self.song,
                        other.song,
                        prop,
                        self.opts.fold_case,
                        a_is_leading && ignore_the,
                        b_is_leading && ignore_the,
                    );
                    if !result.first_empty {
                        a_is_leading = false;
                    }
                    if !result.second_empty {
                        b_is_leading = false;
                    }
                    if result.ordering != Ordering::Equal {
                        return if self.opts.reverse {
                            result.ordering.reverse()
                        } else {
                            result.ordering
                        };
                    }
                }
                Ordering::Equal
            }
            SortMode::ModifiedTime => {
                let result = self.song.last_modified.cmp(&other.song.last_modified);
                if self.opts.reverse { result.reverse() } else { result }
            }
        }
    }
}
impl PartialOrd for SongCustomSort<'_, '_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DirOrSongCustomSort<'dirsong, 'opts> {
    dir_or_song: &'dirsong DirOrSong,
    opts: &'opts SortOptions,
}
impl DirOrSong {
    pub(crate) fn with_custom_sort<'dirsong, 'opts>(
        &'dirsong self,
        opts: &'opts SortOptions,
    ) -> DirOrSongCustomSort<'dirsong, 'opts> {
        DirOrSongCustomSort {
            dir_or_song: self,
            opts,
        }
    }
}
impl Ord for DirOrSongCustomSort<'_, '_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.opts.group_by_type {
            let type_order = match (self.dir_or_song, other.dir_or_song) {
                (DirOrSong::Song(_), DirOrSong::Dir { playlist: true, .. }) => {
                    Some(Ordering::Less)
                }
                (DirOrSong::Song(_), DirOrSong::Dir { playlist: false, .. }) => {
                    Some(Ordering::Greater)
                }
                (DirOrSong::Dir { playlist: true, .. }, DirOrSong::Song(_)) => {
                    Some(Ordering::Greater)
                }
                (DirOrSong::Dir { playlist: false, .. }, DirOrSong::Song(_)) => {
                    Some(Ordering::Less)
                }
                (
                    DirOrSong::Dir { playlist: true, .. },
                    DirOrSong::Dir { playlist: false, .. },
                ) => Some(Ordering::Greater),
                (
                    DirOrSong::Dir { playlist: false, .. },
                    DirOrSong::Dir { playlist: true, .. },
                ) => Some(Ordering::Less),
                _ => None,
            };
            if let Some(order) = type_order {
                return if self.opts.reverse { order.reverse() } else { order };
            }
        }
        let order = match &self.opts.mode {
            SortMode::ModifiedTime => {
                self.dir_or_song.last_modified().cmp(&other.dir_or_song.last_modified())
            }
            SortMode::Format(items) => {
                match (self.dir_or_song, other.dir_or_song) {
                    (DirOrSong::Dir { name: a, .. }, DirOrSong::Dir { name: b, .. }) => {
                        StringCompare::from(self.opts).compare(a, b)
                    }
                    (DirOrSong::Song(a), DirOrSong::Song(b)) => {
                        let ord = a
                            .with_custom_sort(self.opts)
                            .cmp(&b.with_custom_sort(self.opts));
                        if self.opts.reverse { ord.reverse() } else { ord }
                    }
                    (a @ DirOrSong::Dir { name, .. }, DirOrSong::Song(song))
                    | (a @ DirOrSong::Song(song), DirOrSong::Dir { name, .. }) => {
                        let mut is_leading = true;
                        for prop in items {
                            let cmp = StringCompare::builder()
                                .ignore_leading_the(
                                    is_leading && self.opts.ignore_leading_the,
                                )
                                .fold_case(self.opts.fold_case)
                                .build();
                            let s = song.format(prop, "", TagResolutionStrategy::All);
                            if let Some(s) = s {
                                if !s.is_empty() {
                                    is_leading = false;
                                }
                                let result = if matches!(a, DirOrSong::Song(..)) {
                                    cmp.compare(s.as_ref(), name)
                                } else {
                                    cmp.compare(name, s.as_ref())
                                };
                                if result != Ordering::Equal {
                                    return if self.opts.reverse {
                                        result.reverse()
                                    } else {
                                        result
                                    };
                                }
                            }
                        }
                        if matches!(a, DirOrSong::Song(_)) {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                }
            }
        };
        return if self.opts.reverse { order.reverse() } else { order };
    }
}
impl PartialOrd for DirOrSongCustomSort<'_, '_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl LsInfoEntry {
    pub(crate) fn into_dir_or_song(
        self,
        show_playlists_mode: ShowPlaylistsMode,
    ) -> Option<DirOrSong> {
        match self {
            LsInfoEntry::File(song) => Some(DirOrSong::Song(song)),
            LsInfoEntry::Dir(Dir { name, full_path, last_modified }) => {
                Some(DirOrSong::Dir {
                    name,
                    full_path,
                    last_modified,
                    playlist: false,
                })
            }
            LsInfoEntry::Playlist(playlist) => {
                match show_playlists_mode {
                    ShowPlaylistsMode::All => {
                        Some(DirOrSong::Dir {
                            name: playlist.name,
                            full_path: playlist.full_path,
                            last_modified: playlist.last_modified,
                            playlist: true,
                        })
                    }
                    ShowPlaylistsMode::None => None,
                    ShowPlaylistsMode::NonRoot if playlist.name == playlist.full_path => {
                        None
                    }
                    ShowPlaylistsMode::NonRoot => {
                        Some(DirOrSong::Dir {
                            name: playlist.name,
                            full_path: playlist.full_path,
                            last_modified: playlist.last_modified,
                            playlist: true,
                        })
                    }
                }
            }
        }
    }
}
pub struct CmpByProp {
    pub ordering: Ordering,
    pub first_empty: bool,
    pub second_empty: bool,
}
impl CmpByProp {
    fn opt_str<T: AsRef<str>>(
        a: Option<T>,
        b: Option<T>,
        fold_case: bool,
        a_ignore_leading_the: bool,
        b_ignore_leading_the: bool,
    ) -> Self {
        match (a, b) {
            (Some(a), Some(b)) => {
                let a = a.as_ref();
                let b = b.as_ref();
                Self {
                    ordering: StringCompare::builder()
                        .fold_case(fold_case)
                        .ignore_leading_the_in_a(a_ignore_leading_the)
                        .ignore_leading_the_in_b(b_ignore_leading_the)
                        .build()
                        .compare(a, b),
                    first_empty: a.is_empty(),
                    second_empty: b.is_empty(),
                }
            }
            (_, Some(b)) => {
                Self {
                    ordering: Ordering::Greater,
                    first_empty: true,
                    second_empty: b.as_ref().is_empty(),
                }
            }
            (Some(a), _) => {
                Self {
                    ordering: Ordering::Less,
                    first_empty: a.as_ref().is_empty(),
                    second_empty: true,
                }
            }
            (None, None) => {
                Self {
                    ordering: Ordering::Equal,
                    first_empty: true,
                    second_empty: true,
                }
            }
        }
    }
    fn opt_str_parse<T: AsRef<str>, N: FromStr + Ord>(
        a: Option<T>,
        b: Option<T>,
        fold_case: bool,
        a_ignore_leading_the: bool,
        b_ignore_leading_the: bool,
    ) -> Self {
        match (a, b) {
            (Some(a), Some(b)) => {
                match (a.as_ref().parse::<N>(), b.as_ref().parse::<N>()) {
                    (Ok(a), Ok(b)) => {
                        Self {
                            ordering: a.cmp(&b),
                            first_empty: false,
                            second_empty: false,
                        }
                    }
                    _ => {
                        Self::opt_str(
                            Some(a),
                            Some(b),
                            fold_case,
                            a_ignore_leading_the,
                            b_ignore_leading_the,
                        )
                    }
                }
            }
            (_, Some(b)) => {
                Self {
                    ordering: Ordering::Greater,
                    first_empty: true,
                    second_empty: b.as_ref().is_empty(),
                }
            }
            (Some(a), _) => {
                Self {
                    ordering: Ordering::Less,
                    first_empty: a.as_ref().is_empty(),
                    second_empty: true,
                }
            }
            (None, None) => {
                Self {
                    ordering: Ordering::Equal,
                    first_empty: true,
                    second_empty: true,
                }
            }
        }
    }
    fn cmp<T: Ord>(a: Option<T>, b: Option<T>) -> Self {
        match (a, b) {
            (Some(a), Some(b)) => {
                Self {
                    ordering: a.cmp(&b),
                    first_empty: false,
                    second_empty: false,
                }
            }
            (_, Some(_)) => {
                Self {
                    ordering: Ordering::Greater,
                    first_empty: true,
                    second_empty: false,
                }
            }
            (Some(_), _) => {
                Self {
                    ordering: Ordering::Less,
                    first_empty: false,
                    second_empty: true,
                }
            }
            (None, None) => {
                Self {
                    ordering: Ordering::Equal,
                    first_empty: true,
                    second_empty: true,
                }
            }
        }
    }
    pub fn song_cmp(
        a: &Song,
        b: &Song,
        property: &SongProperty,
        fold_case: bool,
        ignore_the: bool,
        ignore_the_other: bool,
    ) -> CmpByProp {
        match property {
            SongProperty::Filename => {
                CmpByProp::opt_str(
                    a.file_name(),
                    b.file_name(),
                    fold_case,
                    ignore_the,
                    ignore_the_other,
                )
            }
            SongProperty::FileExtension => {
                CmpByProp::opt_str(
                    a.file_ext(),
                    b.file_ext(),
                    fold_case,
                    ignore_the,
                    ignore_the_other,
                )
            }
            SongProperty::File => {
                CmpByProp::opt_str(
                    Some(&a.file),
                    Some(&b.file),
                    fold_case,
                    ignore_the,
                    ignore_the_other,
                )
            }
            SongProperty::Title => {
                CmpByProp::opt_str(
                    a.metadata.get("title").map(|v| v.join("")),
                    b.metadata.get("title").map(|v| v.join("")),
                    fold_case,
                    ignore_the,
                    ignore_the_other,
                )
            }
            SongProperty::Artist => {
                CmpByProp::opt_str(
                    a.metadata.get("artist").map(|v| v.join("")),
                    b.metadata.get("artist").map(|v| v.join("")),
                    fold_case,
                    ignore_the,
                    ignore_the_other,
                )
            }
            SongProperty::Album => {
                CmpByProp::opt_str(
                    a.metadata.get("album").map(|v| v.join("")),
                    b.metadata.get("album").map(|v| v.join("")),
                    fold_case,
                    ignore_the,
                    ignore_the_other,
                )
            }
            SongProperty::Other(prop) => {
                CmpByProp::opt_str(
                    a.metadata.get(prop).map(|v| v.join("")),
                    b.metadata.get(prop).map(|v| v.join("")),
                    fold_case,
                    ignore_the,
                    ignore_the_other,
                )
            }
            SongProperty::Track => {
                let self_track = a.metadata.get("track").map(|v| v.join(""));
                let other_track = b.metadata.get("track").map(|v| v.join(""));
                CmpByProp::opt_str_parse::<
                    _,
                    i32,
                >(self_track, other_track, fold_case, ignore_the, ignore_the_other)
            }
            SongProperty::Position => {
                let self_pos = a.metadata.get("pos").map(|v| v.last());
                let other_pos = b.metadata.get("pos").map(|v| v.last());
                CmpByProp::opt_str_parse::<
                    _,
                    usize,
                >(self_pos, other_pos, fold_case, ignore_the, ignore_the_other)
            }
            SongProperty::Disc => {
                let self_disc = a.metadata.get("disc").map(|v| v.join(""));
                let other_disc = b.metadata.get("disc").map(|v| v.join(""));
                CmpByProp::opt_str_parse::<
                    _,
                    i32,
                >(self_disc, other_disc, fold_case, ignore_the, ignore_the_other)
            }
            SongProperty::Duration => CmpByProp::cmp(a.duration, b.duration),
            SongProperty::SampleRate() => CmpByProp::cmp(a.samplerate(), b.samplerate()),
            SongProperty::Bits() => CmpByProp::cmp(a.bits(), b.bits()),
            SongProperty::Channels() => CmpByProp::cmp(a.channels(), b.channels()),
            SongProperty::Added() => CmpByProp::cmp(a.added, b.added),
            SongProperty::LastModified() => {
                CmpByProp::cmp(Some(a.last_modified), Some(b.last_modified))
            }
        }
    }
}
