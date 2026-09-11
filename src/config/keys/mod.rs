use std::{borrow::Cow, collections::HashMap, path::PathBuf};
use anyhow::{Result, bail};
#[cfg(debug_assertions)]
pub use actions::LogsActions;
#[cfg(debug_assertions)]
use actions::LogsActionsFile;
pub use actions::{
    AlbumsActions, ArtistsActions, CommonAction, DirectoriesActions, GlobalAction,
    QueueActions, SearchActions,
};
pub use actions::{
    CommonActionFile, DirectoriesActionsFile, GlobalActionFile, QueueActionsFile,
};
pub use key::{Key, KeySequence};
use serde::{Deserialize, Serialize};
use crate::shared::paths::legacy_config_dir;
pub(crate) mod actions;
pub mod key;
#[derive(Debug, PartialEq, Clone)]
pub struct KeyConfig {
    pub global: HashMap<KeySequence, GlobalAction>,
    pub navigation: HashMap<KeySequence, CommonAction>,
    pub albums: HashMap<KeySequence, AlbumsActions>,
    pub artists: HashMap<KeySequence, ArtistsActions>,
    pub directories: HashMap<KeySequence, DirectoriesActions>,
    pub search: HashMap<KeySequence, SearchActions>,
    #[cfg(debug_assertions)]
    pub logs: HashMap<KeySequence, LogsActions>,
    pub queue: HashMap<KeySequence, QueueActions>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyConfigFile {
    #[serde(default)]
    pub clear: bool,
    #[serde(default)]
    pub global: HashMap<KeySequence, GlobalActionFile>,
    #[serde(default)]
    pub navigation: HashMap<KeySequence, CommonActionFile>,
    #[serde(default)]
    pub directories: HashMap<KeySequence, DirectoriesActionsFile>,
    #[cfg(debug_assertions)]
    #[serde(default)]
    pub logs: HashMap<KeySequence, LogsActionsFile>,
    #[serde(default)]
    pub queue: HashMap<KeySequence, QueueActionsFile>,
}
impl Default for KeyConfigFile {
    #[rustfmt::skip]
    #[allow(unused_imports)]
    fn default() -> Self {
        use GlobalActionFile as G;
        use CommonActionFile as C;
        use DirectoriesActionsFile as D;
        #[cfg(debug_assertions)]
        use LogsActionsFile as L;
        use QueueActionsFile as Q;
        let s = || KeySequence::new();
        let global = HashMap::from([
            (s().char(' '), G::TogglePause),
            (s().tab(), G::NextTab),
            (s().tab().shift(), G::ToggleMpdMode),
            // Round 73.2 revision B (user request, 2026-09-11): `c` is the
            // only letter that cycles libraries (Shift+Right/Left stay as
            // the alternates); `q`/`w`/`E`/`Q` are NOT library cyclers any
            // more. `w`/`s` went back to plain movement (see the navigation
            // and directories maps) and `q` is deliberately UNBOUND — the
            // key system cannot tell Shift+Q from bare Q (both normalize to
            // `Key{ Char('Q'), SHIFT }`), so `Q` is the quit key and binding
            // bare `q` to anything would make quitting ambiguous... `q` is
            // simply left free for the user (`keybinds.ron`).
            // `c` also carries the queue map's ToggleChapters (both actions
            // land in the one trie node): on the Queue tab the tab's own
            // pane claims the queue half first, so Audio/Video/Chapters/
            // Radio cycling is unchanged, and `cycle_library` is a no-op
            // while the Queue tab is active — the two meanings never fire
            // together.
            (s().char('c'), G::NextLibraryTab),
            (s().right().shift(), G::NextLibraryTab),
            (s().left().shift(), G::PreviousLibraryTab),
            (s().char('>'), G::NextTrack),
            // Round 73.2: `Q` (Shift+Q) is the ONLY quit key. No Ctrl+Q:
            // in insert mode it is not a control arm in
            // `InputEvent::from_key_event`, so it would type a literal "q"
            // into the focused search/filter input instead of quitting.
            (s().char('Q'), G::Quit),
            // Round 73.2 revision B: `S` (Shift+S) jumps to the active
            // library tab's Search page with the query input focused
            // (claimed by the MPD/Playlists/Jellyfin panes; a no-op on tabs
            // without a search page). Lowercase `s` is movement again, and
            // `S` deliberately takes the slot that used to be
            // SelectDown — range selection now lives on Shift+Up/Shift+Down
            // only, and `W`/`S` are NOT re-added as a pair (asymmetric).
            (s().char('S'), G::LibrarySearch),
            (s().esc(), G::ShowSettings),
            // Round 55.5: Ctrl+D opens the downloads modal (no conflicts
            // with the defaults above; navigation keeps <C-a>/<C-s>/<C-c>).
            (s().char('d').ctrl(), G::ShowDownloads),
        ]);
        let navigation = HashMap::from([
            (s().esc(), C::Close),
            (s().cr(), C::Confirm),
            // Round 73.2 revision B: the `w`/`s` movement twins are back
            // (they briefly cycled libraries in revision A). `W`/`S` are NOT
            // re-added: `S` is the search jump now and `Shift+Up/Down` carry
            // range selection. The letter actions below are NOT movement and
            // stay as they are: they edit the lyrics pane.
            (s().char('w'), C::Up),
            (s().up(), C::Up),
            (s().char('s'), C::Down),
            (s().down(), C::Down),
            (s().char('a').ctrl(), C::SelectAll),
            (s().up().shift(), C::SelectUp),
            (s().down().shift(), C::SelectDown),
            (s().page_up(), C::PageUp),
            (s().page_down(), C::PageDown),
            (s().delete(), C::Delete),
            (s().left(), C::Left),
            (s().right(), C::Right),
            ("<S-+>".parse().unwrap(), C::NudgeUp),
            (s().char('-'), C::NudgeDown),
            (s().char('s').ctrl(), C::SaveLyrics),
            (s().char('d'), C::DeleteLyricsWord),
            (s().char('e'), C::EditLyricsLine),
            (s().char('i'), C::InsertLyricsLineBefore),
            (s().char('a'), C::InsertLyricsLineAfter),
            (s().char('o'), C::AddLyricsLineAfter),
            (s().char('O'), C::AddLyricsLineBefore),
            (s().char('t'), C::SetLyricsLineTime),
            (s().char('c').ctrl(), C::SaveLyricsAndExit),
        ]);
        let queue = HashMap::from([
            (s().char('c'), Q::ToggleChapters),
            (s().tab().shift(), Q::ToggleChapters),
        ]);
        // Round 73.2 revision B: the wasd directory keys are back, exactly
        // as before the round — `w`/`s` move the tree highlight, `a`
        // collapses, `d` expands, plus the arrow twins `<-`
        // (FolderCollapse) and `->` (PlayFile, which the tree browser
        // treats like FolderExpand on a folder row).
        let directories = HashMap::from([
            (s().char('w'), D::FolderUp),
            (s().char('s'), D::FolderDown),
            (s().char('a'), D::FolderCollapse),
            (s().char('d'), D::FolderExpand),
            (s().right(), D::PlayFile),
            (s().left(), D::FolderCollapse),
        ]);
        #[cfg(debug_assertions)]
        let logs = HashMap::from([
            (s().char('D'), L::Clear),
            (s().char('S'), L::ToggleScroll),
        ]);
        #[cfg(not(debug_assertions))]
        return KeyConfigFile {
            clear: false,
            global,
            navigation,
            directories,
            queue,
        };
        #[cfg(debug_assertions)]
        return KeyConfigFile {
            clear: false,
            global,
            navigation,
            directories,
            queue,
            logs,
        };
    }
}
impl Default for KeyConfig {
    fn default() -> Self {
        KeyConfigFile {
            clear: true,
            ..Default::default()
        }
            .try_into()
            .expect("Default KeyConfigFile should convert to KeyConfig")
    }
}
impl TryFrom<KeyConfigFile> for KeyConfig {
    type Error = anyhow::Error;
    fn try_from(value: KeyConfigFile) -> Result<Self, Self::Error> {
        if value.clear {
            Ok(KeyConfig {
                global: value.global.into_iter().map(|(k, v)| (k, v.into())).collect(),
                navigation: value
                    .navigation
                    .into_iter()
                    .map(|(k, v)| -> anyhow::Result<_> { Ok((k, v.try_into()?)) })
                    .collect::<anyhow::Result<_>>()?,
                albums: HashMap::new(),
                artists: HashMap::new(),
                directories: value
                    .directories
                    .into_iter()
                    .map(|(k, v)| (k, v.into()))
                    .collect(),
                search: HashMap::new(),
                #[cfg(debug_assertions)]
                logs: value.logs.into_iter().map(|(k, v)| (k, v.into())).collect(),
                queue: value
                    .queue
                    .into_iter()
                    .map(|(k, v)| -> anyhow::Result<_> { Ok((k, v.try_into()?)) })
                    .collect::<anyhow::Result<_>>()?,
            })
        } else {
            let global: HashMap<KeySequence, GlobalAction> = value
                .global
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect();
            let navigation: HashMap<KeySequence, CommonAction> = value
                .navigation
                .into_iter()
                .map(|(k, v)| -> anyhow::Result<_> { Ok((k, v.try_into()?)) })
                .collect::<anyhow::Result<_>>()?;
            let queue: HashMap<KeySequence, QueueActions> = value
                .queue
                .into_iter()
                .map(|(k, v)| -> anyhow::Result<_> { Ok((k, v.try_into()?)) })
                .collect::<anyhow::Result<_>>()?;
            let directories: HashMap<KeySequence, DirectoriesActions> = value
                .directories
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect();
            #[cfg(debug_assertions)]
            let logs: HashMap<KeySequence, LogsActions> = value
                .logs
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect();
            let mut result = KeyConfig::default();
            let all_key_overrides = global
                .keys()
                .chain(navigation.keys())
                .chain(directories.keys())
                .chain(queue.keys());
            #[cfg(debug_assertions)]
            let all_key_overrides = all_key_overrides.chain(logs.keys());
            for key in all_key_overrides {
                result.global.remove(key);
                result.navigation.remove(key);
                result.queue.remove(key);
                #[cfg(debug_assertions)] result.logs.remove(key);
            }
            for (k, v) in global {
                result.global.insert(k, v);
            }
            for (k, v) in navigation {
                result.navigation.insert(k, v);
            }
            for (k, v) in queue {
                result.queue.insert(k, v);
            }
            for (k, v) in directories {
                result.directories.insert(k, v);
            }
            #[cfg(debug_assertions)]
            for (k, v) in logs {
                result.logs.insert(k, v);
            }
            Ok(result)
        }
    }
}
pub trait ToDescription {
    fn to_description(&self) -> Cow<'static, str>;
}
/// Sidecar file (`~/.config/s2udio/keybinds.ron`, round 19 — s2udio-only
/// runtime remaps moved out of `~/.config/rmpc/`) holding runtime key
/// remaps made from the Settings panel. The main config file is never
/// rewritten; instead this file records which keys to drop and which
/// (key, action) bindings to add on top of the configured keybinds.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindsOverrides {
    /// Key sequences to remove from every keybind map.
    pub remove: Vec<KeySequence>,
    pub global: HashMap<KeySequence, GlobalActionFile>,
    pub navigation: HashMap<KeySequence, CommonActionFile>,
    pub directories: HashMap<KeySequence, DirectoriesActionsFile>,
    pub queue: HashMap<KeySequence, QueueActionsFile>,
}
pub const KEYBINDS_OVERRIDE_FILE: &str = "keybinds.ron";
impl KeybindsOverrides {
    /// The s2udio sidecar path (`~/.config/s2udio/keybinds.ron`).
    pub fn path() -> Option<PathBuf> {
        crate::shared::paths::s2udio_config_dir()
            .map(|dir| dir.join(KEYBINDS_OVERRIDE_FILE))
    }
    /// The legacy pre-round-23 sidecar path (`~/.config/rmpc/keybinds.ron`).
    fn legacy_path() -> Option<PathBuf> {
        legacy_config_dir().map(|dir| dir.join(KEYBINDS_OVERRIDE_FILE))
    }
    /// Read the sidecar file, if it exists and parses. Falls back to the
    /// legacy `~/.config/rmpc/keybinds.ron` when the s2udio file is
    /// absent (migration).
    pub fn load() -> Option<Self> {
        let path = Self::path()
            .filter(|p| p.exists())
            .or_else(Self::legacy_path)
            .or_else(Self::path)?;
        let content = std::fs::read_to_string(&path).ok()?;
        match ron::de::from_str(&content) {
            Ok(overrides) => Some(overrides),
            Err(err) => {
                log::warn!(path:?; "Failed to parse keybinds overrides: {err}");
                None
            }
        }
    }
    /// Apply the overrides on top of already-merged keybinds.
    pub fn apply_to(&self, keybinds: &mut KeyConfig) {
        for seq in &self.remove {
            keybinds.global.remove(seq);
            keybinds.navigation.remove(seq);
            keybinds.directories.remove(seq);
            keybinds.queue.remove(seq);
        }
        for (seq, action) in &self.global {
            keybinds.global.insert(seq.clone(), action.clone().into());
        }
        for (seq, action) in &self.navigation {
            if let Ok(action) = action.clone().try_into() {
                keybinds.navigation.insert(seq.clone(), action);
            }
        }
        for (seq, action) in &self.directories {
            keybinds.directories.insert(seq.clone(), action.clone().into());
        }
        for (seq, action) in &self.queue {
            if let Ok(action) = action.clone().try_into() {
                keybinds.queue.insert(seq.clone(), action);
            }
        }
    }
    /// Persist the overrides to the sidecar file.
    pub fn save(&self) -> Result<()> {
        let Some(path) = Self::path() else {
            bail!("Could not determine config directory");
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = ron::ser::to_string_pretty(
            self,
            ron::ser::PrettyConfig::default(),
        )?;
        std::fs::write(&path, content)?;
        Ok(())
    }
}
