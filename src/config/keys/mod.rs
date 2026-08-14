use std::{borrow::Cow, collections::HashMap, path::PathBuf};

use anyhow::{Result, bail};

#[cfg(debug_assertions)]
pub use actions::LogsActions;
#[cfg(debug_assertions)]
use actions::LogsActionsFile;
pub use actions::{
    AlbumsActions,
    ArtistsActions,
    CommonAction,
    DirectoriesActions,
    GlobalAction,
    QueueActions,
    SearchActions,
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

// It is important here that the deserialization does not put in filled key maps
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

        // Minimal keybind set: everything else was removed from the defaults.
        // Esc is bound to both Close (menu) and ShowSettings (no menu open).
        let global = HashMap::from([
            (s().char(' '),                       G::TogglePause),
            (s().tab(),                           G::NextTab),
            (s().tab().shift(),                   G::ToggleMpdMode),
            (s().char('E'),                       G::NextTab),
            (s().char('Q'),                       G::PreviousTab),
            (s().char('>'),                       G::NextTrack),
            (s().char('q'),                       G::Quit),
            (s().esc(),                           G::ShowSettings),
        ]);

        let navigation = HashMap::from([
            (s().esc(),                           C::Close),
            (s().cr(),                            C::Confirm),
            (s().char('w'),                       C::Up),
            (s().up(),                            C::Up),
            (s().char('s'),                       C::Down),
            (s().down(),                          C::Down),
            (s().char('W'),                       C::SelectUp),
            (s().char('S'),                       C::SelectDown),
            (s().char('a').ctrl(),                C::SelectAll),
            (s().up().shift(),                    C::SelectUp),
            (s().down().shift(),                  C::SelectDown),
            (s().page_up(),                       C::PageUp),
            (s().page_down(),                     C::PageDown),
            (s().delete(),                        C::Delete),
            // Lyrics edit mode (round 34): `←`/`→` move across words,
            // `+`/`-` nudge the selected word's time (10 ms), `<C-s>`
            // saves without leaving edit mode. (`+` is Shift+= on the
            // user's layout, reported as `<S-+>`.)
            (s().left(),                          C::Left),
            (s().right(),                         C::Right),
            ("<S-+>".parse().unwrap(),            C::NudgeUp),
            (s().char('-'),                       C::NudgeDown),
            (s().char('s').ctrl(),                C::SaveLyrics),
            // Lyrics edit mode (round 35): `d` deletes the current line,
            // `e` edits its text, `i`/`a` insert a line before/after,
            // `t` sets the line's timestamp.
            (s().char('d'),                       C::DeleteLyricsLine),
            (s().char('e'),                       C::EditLyricsLine),
            (s().char('i'),                       C::InsertLyricsLineBefore),
            (s().char('a'),                       C::InsertLyricsLineAfter),
            (s().char('t'),                       C::SetLyricsLineTime),
            // Lyrics edit mode (round 37): `<C-c>` saves and exits (Esc
            // discards, `<C-s>` saves in place).
            (s().char('c').ctrl(),                C::SaveLyricsAndExit),
        ]);

        let queue = HashMap::from([
            (s().char('c'), Q::ToggleChapters),
            (s().tab().shift(), Q::ToggleChapters),
        ]);

        let directories = HashMap::from([
            (s().char('w'),                       D::FolderUp),
            (s().char('s'),                       D::FolderDown),
            (s().char('a'),                       D::FolderCollapse),
            (s().char('d'),                       D::FolderExpand),
            (s().right(),                         D::PlayFile),
            (s().left(),                          D::FolderCollapse),
        ]);

        #[cfg(debug_assertions)]
        let logs = HashMap::from([
            (s().char('D'),                       L::Clear),
            (s().char('S'),                       L::ToggleScroll),
        ]);

        #[cfg(not(debug_assertions))]
        return KeyConfigFile { clear: false, global, navigation, directories, queue };

        #[cfg(debug_assertions)]
        return KeyConfigFile { clear: false, global, navigation, directories, queue, logs };
    }
}

impl Default for KeyConfig {
    fn default() -> Self {
        KeyConfigFile { clear: true, ..Default::default() }
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
            let global: HashMap<KeySequence, GlobalAction> =
                value.global.into_iter().map(|(k, v)| (k, v.into())).collect();
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
            let logs: HashMap<KeySequence, LogsActions> =
                value.logs.into_iter().map(|(k, v)| (k, v.into())).collect();

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
                #[cfg(debug_assertions)]
                result.logs.remove(key);
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
        crate::shared::paths::s2udio_config_dir().map(|dir| dir.join(KEYBINDS_OVERRIDE_FILE))
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
        let content = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())?;
        std::fs::write(&path, content)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;

    use crossterm::event::{KeyCode, KeyModifiers};

    use super::{Key, KeyConfig, KeyConfigFile, KeySequence, KeybindsOverrides};
    #[cfg(debug_assertions)]
    use crate::config::keys::LogsActions;
    #[cfg(debug_assertions)]
    use crate::config::keys::LogsActionsFile;
    use crate::config::keys::{
        CommonAction,
        DirectoriesActionsFile,
        GlobalAction,
        QueueActions,
        actions::{CommonActionFile, GlobalActionFile, QueueActionsFile},
    };

    fn k(s: &str) -> KeySequence {
        KeySequence::new().char(s.chars().next().unwrap())
    }

    #[test]
    fn default_directories_bindings_include_collapse_on_a() {
        let default = KeyConfigFile::default();
        assert_eq!(
            default.directories.get(&k("a")),
            Some(&DirectoriesActionsFile::FolderCollapse),
            "a collapses / steps out of the selected folder"
        );
        assert_eq!(
            default.directories.get(&k("d")),
            Some(&DirectoriesActionsFile::FolderExpand)
        );
        assert_eq!(
            default.directories.get(&k("w")),
            Some(&DirectoriesActionsFile::FolderUp)
        );
        assert_eq!(
            default.directories.get(&k("s")),
            Some(&DirectoriesActionsFile::FolderDown)
        );
    }

    #[test]
    fn default_navigation_bindings_include_ctrl_a_select_all() {
        let default = KeyConfigFile::default();
        assert_eq!(
            default.navigation.get(&KeySequence::new().char('a').ctrl()),
            Some(&CommonActionFile::SelectAll),
            "Ctrl+A selects all items of the current list"
        );
    }

    #[test]
    fn default_queue_bindings_include_shift_tab_for_cycling_lists() {
        let default = KeyConfigFile::default();
        assert_eq!(
            default.queue.get(&KeySequence::new().tab().shift()),
            Some(&QueueActionsFile::ToggleChapters),
            "<S-Tab> cycles the audio/video/chapters lists like c"
        );
        assert_eq!(
            default.queue.get(&k("c")),
            Some(&QueueActionsFile::ToggleChapters)
        );
    }

    #[test]
    fn default_global_bindings_include_shift_q_e_for_tab_navigation() {
        let default = KeyConfigFile::default();
        assert_eq!(
            default.global.get(&k("E")),
            Some(&GlobalActionFile::NextTab),
            "Shift+E moves right through the tabs"
        );
        assert_eq!(
            default.global.get(&k("Q")),
            Some(&GlobalActionFile::PreviousTab),
            "Shift+Q moves left through the tabs"
        );
        assert_eq!(
            default.global.get(&k("q")),
            Some(&GlobalActionFile::Quit),
            "Quit moved off Shift+Q onto plain q"
        );
    }

    #[test]
    #[rustfmt::skip]
    fn converts() {
        let input = KeyConfigFile {
            clear: true,
            global: HashMap::from([(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), GlobalActionFile::Quit)]),
            directories: HashMap::new(),

            #[cfg(debug_assertions)]
            logs: HashMap::from([(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), LogsActionsFile::Clear)]),
            queue: HashMap::from([(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), QueueActionsFile::Play),
                                  (Key { key: KeyCode::Char('b'), modifiers: KeyModifiers::SHIFT, }.into(), QueueActionsFile::JumpToCurrent)]),
            navigation: HashMap::from([
                (Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), CommonActionFile::Up),
                (Key { key: KeyCode::Char('b'), modifiers: KeyModifiers::SHIFT }.into(), CommonActionFile::Up)
            ])
        };
        let expected = KeyConfig {
            global: HashMap::from([(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), GlobalAction::Quit)]),
            #[cfg(debug_assertions)]
            logs: HashMap::from([(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), LogsActions::Clear)]),
            queue: HashMap::from([(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), QueueActions::Play),
                                  (Key { key: KeyCode::Char('b'), modifiers: KeyModifiers::SHIFT, }.into(), QueueActions::JumpToCurrent)]),
            albums: HashMap::from([]),
            artists: HashMap::from([]),
            directories: HashMap::from([]),
            search: HashMap::from([]),
            navigation: HashMap::from([(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL }.into(), CommonAction::Up),
                                       (Key { key: KeyCode::Char('b'), modifiers: KeyModifiers::SHIFT }.into(), CommonAction::Up)]),
        };

        let result: KeyConfig = input.try_into().unwrap();


        assert_eq!(result, expected);
    }

    #[test]
    #[rustfmt::skip]
    fn converts_without_clearing() {
        let input = KeyConfigFile {
            clear: false,
            global: HashMap::from([
                (Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), GlobalActionFile::Quit),
                (Key { key: KeyCode::Char(' '), modifiers: KeyModifiers::NONE }.into(), GlobalActionFile::TogglePause),
            ]),
            directories: HashMap::new(),
            queue: HashMap::from([(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), QueueActionsFile::Play),
                                  (Key { key: KeyCode::Char('b'), modifiers: KeyModifiers::SHIFT, }.into(), QueueActionsFile::JumpToCurrent)]),
            navigation: HashMap::from([
                (Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), CommonActionFile::Up),
                (Key { key: KeyCode::Char('b'), modifiers: KeyModifiers::SHIFT }.into(), CommonActionFile::Up),
            ]),
            #[cfg(debug_assertions)]
            logs: HashMap::from([(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), LogsActionsFile::Clear)]),
        };

        let mut default: KeyConfig = KeyConfig::default();
        default.global.insert(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), GlobalAction::Quit);
        default.queue.insert(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), QueueActions::Play);
        default.queue.insert(Key { key: KeyCode::Char('b'), modifiers: KeyModifiers::SHIFT, }.into(), QueueActions::JumpToCurrent);
        default.navigation.insert(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL }.into(), CommonAction::Up);
        default.navigation.insert(Key { key: KeyCode::Char('b'), modifiers: KeyModifiers::SHIFT }.into(), CommonAction::Up);
        #[cfg(debug_assertions)]
        default.logs.insert(Key { key: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, }.into(), LogsActions::Clear);

        // <Space> is mapped in global keys, it has to remove the default `Select` mapping from navigation keys
        default.global.insert(Key { key: KeyCode::Char(' '), modifiers: KeyModifiers::NONE, }.into(), GlobalAction::TogglePause);
        default.navigation.remove(&Key { key: KeyCode::Char(' '), modifiers: KeyModifiers::NONE, }.into());

        let result: KeyConfig = input.try_into().unwrap();

        assert_eq!(result, default);
    }

    #[test]
    fn keybinds_overrides_ron_roundtrip() {
        let overrides = KeybindsOverrides {
            remove: vec![k("q")],
            global: HashMap::from([(k("r"), GlobalActionFile::Quit)]),
            navigation: HashMap::from([(k("p"), CommonActionFile::Close)]),
            directories: HashMap::from([(k("d"), DirectoriesActionsFile::PlayFile)]),
            queue: HashMap::from([(k("x"), QueueActionsFile::Delete)]),
        };
        let content =
            ron::ser::to_string_pretty(&overrides, ron::ser::PrettyConfig::default()).unwrap();
        let parsed: KeybindsOverrides = ron::de::from_str(&content).unwrap();
        assert_eq!(parsed, overrides);
    }

    #[test]
    fn keybinds_overrides_apply_removes_and_rebinds() {
        let mut keybinds = KeyConfig::default();
        let overrides = KeybindsOverrides {
            remove: vec![k("q")],
            global: HashMap::from([(k("r"), GlobalActionFile::Quit)]),
            ..Default::default()
        };
        overrides.apply_to(&mut keybinds);
        assert!(!keybinds.global.contains_key(&k("q")));
        assert_eq!(keybinds.global.get(&k("r")), Some(&GlobalAction::Quit));
    }

    #[test]
    fn keybinds_overrides_apply_overrides_existing_binding() {
        let mut keybinds = KeyConfig::default();
        // "z" toggles repeat by default; rebind it to pause.
        let overrides = KeybindsOverrides {
            global: HashMap::from([(k("z"), GlobalActionFile::TogglePause)]),
            ..Default::default()
        };
        overrides.apply_to(&mut keybinds);
        assert_eq!(keybinds.global.get(&k("z")), Some(&GlobalAction::TogglePause));
    }
}
