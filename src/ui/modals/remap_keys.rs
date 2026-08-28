//! Key-remapping helpers shared by the Settings panel's keybinds section.
//! The remap flow: list the configured actions with their keys, pick one,
//! capture the new key, rebind it in the runtime config. Persisting to the
//! `keybinds.ron` sidecar is deferred to the panel's Save action.

use std::sync::Arc;

use anyhow::Result;
use itertools::Itertools;
use strum::VariantArray;

use crate::{
    config::{
        keys::{
            actions::{AddKind, DeleteKind, RateKind, SaveKind},
            CommonAction, DirectoriesActions, GlobalAction, Key, KeyConfig, KeySequence,
            QueueActions,
        },
        tabs::TabName,
    },
    ctx::Ctx,
    shared::keys::{Actions, KeyResolver},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Global,
    Navigation,
    Queue,
    Directories,
}

impl Section {
    fn order(self) -> u8 {
        match self {
            Section::Global => 0,
            Section::Navigation => 1,
            Section::Queue => 2,
            Section::Directories => 3,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Section::Global => "Global",
            Section::Navigation => "Navigation",
            Section::Queue => "Queue",
            Section::Directories => "Regions / browser",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RemapRow {
    pub(crate) section: Section,
    pub(crate) action: Actions,
    pub(crate) keys: Vec<KeySequence>,
    pub(crate) display: String,
}

pub(crate) fn action_display(action: &Actions) -> String {
    match action {
        Actions::Global(a) => a.to_string(),
        Actions::Common(a) => a.to_string(),
        Actions::Queue(a) => a.to_string(),
        Actions::Directories(a) => a.to_string(),
        #[cfg(debug_assertions)]
        Actions::Logs(a) => a.to_string(),
    }
}

fn actions_eq(a: &Actions, b: &Actions) -> bool {
    match (a, b) {
        (Actions::Global(x), Actions::Global(y)) => x == y,
        (Actions::Common(x), Actions::Common(y)) => x == y,
        (Actions::Queue(x), Actions::Queue(y)) => x == y,
        (Actions::Directories(x), Actions::Directories(y)) => x == y,
        #[cfg(debug_assertions)]
        (Actions::Logs(x), Actions::Logs(y)) => x == y,
        _ => false,
    }
}

pub(crate) fn key_display(keys: &[KeySequence]) -> String {
    keys.iter().map(|k| k.to_string()).join(" / ")
}

/// Short, table-friendly description for an action (the full `ToDescription`
/// strings are too long for the keybinds table).
pub(crate) fn remap_description(action: &Actions) -> String {
    use crate::config::keys::{CommonAction, DirectoriesActions, GlobalAction, QueueActions};
    match action {
        Actions::Global(a) => match a {
            GlobalAction::Quit => "Exit rmpc",
            GlobalAction::ShowHelp => "Show keybindings",
            GlobalAction::ShowSettings => "Open settings",
            GlobalAction::ShowCurrentSongInfo => "Show song info",
            GlobalAction::ShowOutputs => "Show outputs",
            GlobalAction::ShowDecoders => "Show decoders",
            GlobalAction::ShowDownloads => "Show downloads",
            GlobalAction::Partition { .. } => "Switch partition",
            GlobalAction::AddRandom => "Add random songs",
            GlobalAction::NextTrack => "Next track",
            GlobalAction::PreviousTrack => "Previous track",
            GlobalAction::Stop => "Stop playback",
            GlobalAction::ToggleRepeat => "Toggle repeat",
            GlobalAction::ToggleSingle => "Toggle single",
            GlobalAction::ToggleRandom => "Toggle random",
            GlobalAction::ToggleConsume => "Toggle consume",
            GlobalAction::ToggleSingleOnOff => "Single on/off",
            GlobalAction::ToggleConsumeOnOff => "Consume on/off",
            GlobalAction::TogglePause => "Play / pause",
            GlobalAction::VolumeUp => "Volume up",
            GlobalAction::VolumeDown => "Volume down",
            GlobalAction::CrossfadeUp => "Crossfade up",
            GlobalAction::CrossfadeDown => "Crossfade down",
            GlobalAction::SeekForward => "Seek forward",
            GlobalAction::SeekBack => "Seek back",
            GlobalAction::SeekToStart => "Seek to start",
            GlobalAction::Update => "Update library",
            GlobalAction::Rescan => "Rescan library",
            GlobalAction::CommandMode => "Command mode",
            GlobalAction::NextTab => "Next tab",
            GlobalAction::PreviousTab => "Previous tab",
            GlobalAction::ToggleMpdMode => "Toggle Library/Search (MPD tab)",
            GlobalAction::SwitchToTab(name) => return format!("Go to {name}"),
            GlobalAction::Command { .. } => "Run command",
            GlobalAction::ExternalCommand { .. } => "External command",
        }
        .to_string(),
        Actions::Common(a) => match a {
            CommonAction::Down => "Down",
            CommonAction::Up => "Up",
            CommonAction::Right => "Right",
            CommonAction::Left => "Left",
            CommonAction::PaneDown => "Focus pane down",
            CommonAction::PaneUp => "Focus pane up",
            CommonAction::PaneRight => "Focus pane right",
            CommonAction::PaneLeft => "Focus pane left",
            CommonAction::MoveDown => "Move item down",
            CommonAction::MoveUp => "Move item up",
            CommonAction::DownHalf => "Half page down",
            CommonAction::UpHalf => "Half page up",
            CommonAction::PageUp => "Page up",
            CommonAction::PageDown => "Page down",
            CommonAction::Top => "Jump to top",
            CommonAction::Bottom => "Jump to bottom",
            CommonAction::EnterSearch => "Search",
            CommonAction::NextResult => "Next result",
            CommonAction::PreviousResult => "Previous result",
            CommonAction::Select => "Select item",
            CommonAction::SelectAll => "Select all items",
            CommonAction::InvertSelection => "Invert selection",
            CommonAction::SelectDown => "Select down",
            CommonAction::SelectUp => "Select up",
            CommonAction::Delete => "Delete",
            CommonAction::Rename => "Rename",
            CommonAction::Close => "Close / cancel",
            CommonAction::Confirm => "Confirm",
            CommonAction::FocusInput => "Focus input",
            CommonAction::AddOptions { .. } => "Add to queue",
            CommonAction::ShowInfo => "Show info",
            CommonAction::ContextMenu => "Context menu",
            CommonAction::Rate { .. } => "Rate song",
            CommonAction::Save { .. } => "Save playlist",
            CommonAction::DeleteFromPlaylist { .. } => "Remove from playlist",
            CommonAction::LyricsNudgeUp => "Nudge word time up",
            CommonAction::LyricsNudgeDown => "Nudge word time down",
            CommonAction::LyricsSave => "Save lyrics edit",
            CommonAction::LyricsDeleteWord => "Delete selected word",
            CommonAction::LyricsEditLine => "Edit lyric line text",
            CommonAction::LyricsInsertBefore => "Insert word before selected",
            CommonAction::LyricsInsertAfter => "Insert word after selected",
            CommonAction::LyricsAddLineBefore => "Add lyric line before",
            CommonAction::LyricsAddLineAfter => "Add lyric line after",
            CommonAction::LyricsLineTime => "Set lyric line time",
            CommonAction::LyricsSaveAndExit => "Save lyrics and exit",
        }
        .to_string(),
        Actions::Queue(a) => match a {
            QueueActions::Delete => "Delete from queue",
            QueueActions::DeleteAll => "Clear queue",
            QueueActions::Play => "Play",
            QueueActions::JumpToCurrent => "Jump to current",
            QueueActions::Shuffle => "Shuffle queue",
            QueueActions::SortByColumn(_) => "Sort by column",
            QueueActions::ToggleChapters => "Toggle queue / chapters",
            QueueActions::Unused => "Unused",
        }
        .to_string(),
        Actions::Directories(a) => match a {
            DirectoriesActions::FolderUp => "Move up",
            DirectoriesActions::FolderDown => "Move down",
            DirectoriesActions::FolderCollapse => "Collapse",
            DirectoriesActions::FolderExpand => "Expand / play",
            DirectoriesActions::PlayFile => "Play file",
        }
        .to_string(),
        #[cfg(debug_assertions)]
        Actions::Logs(a) => match a {
            crate::config::keys::LogsActions::Clear => "Clear logs",
            crate::config::keys::LogsActions::ToggleScroll => "Toggle log scroll",
        }
        .to_string(),
    }
}

/// The complete remapable Global-action catalog: every persistable action
/// in its base form. Payload-carrying actions appear with their default
/// payload so a fresh bind produces a working, persistable binding (the
/// row's `actions_eq` then matches only the same payload — different
/// configured payloads keep their own rows when bound).
fn global_catalog() -> Vec<GlobalAction> {
    use crate::config::keys::actions::GlobalActionDiscriminants as D;
    D::VARIANTS
        .iter()
        .map(|disc| match disc {
            D::Quit => GlobalAction::Quit,
            D::ShowHelp => GlobalAction::ShowHelp,
            D::ShowSettings => GlobalAction::ShowSettings,
            D::ShowCurrentSongInfo => GlobalAction::ShowCurrentSongInfo,
            D::ShowOutputs => GlobalAction::ShowOutputs,
            D::ShowDecoders => GlobalAction::ShowDecoders,
            D::ShowDownloads => GlobalAction::ShowDownloads,
            D::Partition => GlobalAction::Partition { name: None, autocreate: false },
            D::AddRandom => GlobalAction::AddRandom,
            D::NextTrack => GlobalAction::NextTrack,
            D::PreviousTrack => GlobalAction::PreviousTrack,
            D::Stop => GlobalAction::Stop,
            D::ToggleRepeat => GlobalAction::ToggleRepeat,
            D::ToggleSingle => GlobalAction::ToggleSingle,
            D::ToggleRandom => GlobalAction::ToggleRandom,
            D::ToggleConsume => GlobalAction::ToggleConsume,
            D::ToggleSingleOnOff => GlobalAction::ToggleSingleOnOff,
            D::ToggleConsumeOnOff => GlobalAction::ToggleConsumeOnOff,
            D::TogglePause => GlobalAction::TogglePause,
            D::VolumeUp => GlobalAction::VolumeUp,
            D::VolumeDown => GlobalAction::VolumeDown,
            D::CrossfadeUp => GlobalAction::CrossfadeUp,
            D::CrossfadeDown => GlobalAction::CrossfadeDown,
            D::SeekForward => GlobalAction::SeekForward,
            D::SeekBack => GlobalAction::SeekBack,
            D::SeekToStart => GlobalAction::SeekToStart,
            D::Update => GlobalAction::Update,
            D::Rescan => GlobalAction::Rescan,
            D::CommandMode => GlobalAction::CommandMode,
            D::NextTab => GlobalAction::NextTab,
            D::PreviousTab => GlobalAction::PreviousTab,
            D::ToggleMpdMode => GlobalAction::ToggleMpdMode,
            D::SwitchToTab => GlobalAction::SwitchToTab(TabName::from("Queue")),
            D::Command => GlobalAction::Command { command: String::new(), description: None },
            D::ExternalCommand => GlobalAction::ExternalCommand {
                command: Arc::new(Vec::new()),
                description: None,
            },
        })
        .collect()
}

/// The complete remapable Navigation (Common) action catalog, base form.
fn common_catalog() -> Vec<CommonAction> {
    use crate::config::keys::actions::CommonActionDiscriminants as D;
    D::VARIANTS
        .iter()
        .map(|disc| match disc {
            D::Down => CommonAction::Down,
            D::Up => CommonAction::Up,
            D::Right => CommonAction::Right,
            D::Left => CommonAction::Left,
            D::PaneDown => CommonAction::PaneDown,
            D::PaneUp => CommonAction::PaneUp,
            D::PaneRight => CommonAction::PaneRight,
            D::PaneLeft => CommonAction::PaneLeft,
            D::MoveDown => CommonAction::MoveDown,
            D::MoveUp => CommonAction::MoveUp,
            D::DownHalf => CommonAction::DownHalf,
            D::UpHalf => CommonAction::UpHalf,
            D::PageUp => CommonAction::PageUp,
            D::PageDown => CommonAction::PageDown,
            D::Top => CommonAction::Top,
            D::Bottom => CommonAction::Bottom,
            D::EnterSearch => CommonAction::EnterSearch,
            D::NextResult => CommonAction::NextResult,
            D::PreviousResult => CommonAction::PreviousResult,
            D::Select => CommonAction::Select,
            D::SelectAll => CommonAction::SelectAll,
            D::InvertSelection => CommonAction::InvertSelection,
            D::SelectDown => CommonAction::SelectDown,
            D::SelectUp => CommonAction::SelectUp,
            D::Delete => CommonAction::Delete,
            D::Rename => CommonAction::Rename,
            D::Close => CommonAction::Close,
            D::Confirm => CommonAction::Confirm,
            D::FocusInput => CommonAction::FocusInput,
            D::LyricsNudgeUp => CommonAction::LyricsNudgeUp,
            D::LyricsNudgeDown => CommonAction::LyricsNudgeDown,
            D::LyricsSave => CommonAction::LyricsSave,
            D::LyricsDeleteWord => CommonAction::LyricsDeleteWord,
            D::LyricsEditLine => CommonAction::LyricsEditLine,
            D::LyricsInsertBefore => CommonAction::LyricsInsertBefore,
            D::LyricsInsertAfter => CommonAction::LyricsInsertAfter,
            D::LyricsAddLineBefore => CommonAction::LyricsAddLineBefore,
            D::LyricsAddLineAfter => CommonAction::LyricsAddLineAfter,
            D::LyricsLineTime => CommonAction::LyricsLineTime,
            D::LyricsSaveAndExit => CommonAction::LyricsSaveAndExit,
            D::AddOptions => CommonAction::AddOptions { kind: AddKind::default() },
            D::ShowInfo => CommonAction::ShowInfo,
            D::ContextMenu => CommonAction::ContextMenu,
            D::Rate => CommonAction::Rate {
                kind: RateKind::default(),
                current: false,
                min_rating: 0,
                max_rating: 10,
            },
            D::Save => CommonAction::Save { kind: SaveKind::default() },
            D::DeleteFromPlaylist => {
                CommonAction::DeleteFromPlaylist { kind: DeleteKind::default() }
            }
        })
        .collect()
}

/// The complete remapable Queue-action catalog, base form (`Unused` is not
/// persistable and stays skipped).
fn queue_catalog() -> Vec<QueueActions> {
    use crate::config::keys::actions::QueueActionsDiscriminants as D;
    D::VARIANTS
        .iter()
        .filter(|disc| !matches!(disc, D::Unused))
        .map(|disc| match disc {
            D::Delete => QueueActions::Delete,
            D::DeleteAll => QueueActions::DeleteAll,
            D::Play => QueueActions::Play,
            D::JumpToCurrent => QueueActions::JumpToCurrent,
            D::Shuffle => QueueActions::Shuffle,
            D::SortByColumn => QueueActions::SortByColumn(0),
            D::ToggleChapters => QueueActions::ToggleChapters,
            D::Unused => unreachable!("filtered out above"),
        })
        .collect()
}

/// The complete remapable Regions / browser catalog (unit-only enum).
const DIRECTORIES_CATALOG: [DirectoriesActions; 5] = [
    DirectoriesActions::FolderUp,
    DirectoriesActions::FolderDown,
    DirectoriesActions::FolderCollapse,
    DirectoriesActions::FolderExpand,
    DirectoriesActions::PlayFile,
];

/// Every remapable action in its base/persisted form, grouped by section.
/// Used to seed the keybinds list with ALL actions — bound or not.
fn action_catalog() -> Vec<(Section, Actions)> {
    let mut catalog: Vec<(Section, Actions)> = Vec::new();
    catalog.extend(global_catalog().into_iter().map(|a| (Section::Global, Actions::Global(a))));
    catalog.extend(common_catalog().into_iter().map(|a| (Section::Navigation, Actions::Common(a))));
    catalog.extend(queue_catalog().into_iter().map(|a| (Section::Queue, Actions::Queue(a))));
    catalog.extend(
        DIRECTORIES_CATALOG
            .iter()
            .copied()
            .map(|a| (Section::Directories, Actions::Directories(a))),
    );
    catalog
}

/// The remapable actions with their current keys, grouped and sorted.
///
/// The list is seeded from the full per-section action catalog, so actions
/// with no bound key (e.g. `ShowDownloads`) get a row with an empty key
/// cell and still participate in capture/rebind. Bound keys then attach to
/// their catalog row; configured actions whose payload differs from any
/// catalog base form keep their own rows so no live binding is hidden.
pub(crate) fn build_remap_rows(ctx: &Ctx) -> Vec<RemapRow> {
    build_remap_rows_from(&ctx.config.keybinds)
}

/// Pure core of [`build_remap_rows`] (no `Ctx` needed): seeds the rows from
/// the action catalog, then attaches every bound key.
fn build_remap_rows_from(keybinds: &KeyConfig) -> Vec<RemapRow> {
    let mut rows: Vec<RemapRow> = Vec::new();

    let mut upsert = |section: Section, seq: Option<KeySequence>, action: Actions| {
        // Skip actions that cannot be persisted back to the config format.
        if matches!(action, Actions::Queue(QueueActions::Unused)) {
            return;
        }
        let display = action_display(&action);
        if let Some(row) =
            rows.iter_mut().find(|row| row.section == section && actions_eq(&row.action, &action))
        {
            if let Some(seq) = seq {
                row.keys.push(seq);
            }
        } else {
            rows.push(RemapRow { section, action, keys: seq.into_iter().collect(), display });
        }
    };

    for (section, action) in action_catalog() {
        upsert(section, None, action);
    }
    for (seq, action) in &keybinds.global {
        upsert(Section::Global, Some(seq.clone()), Actions::Global(action.clone()));
    }
    for (seq, action) in &keybinds.navigation {
        upsert(Section::Navigation, Some(seq.clone()), Actions::Common(action.clone()));
    }
    for (seq, action) in &keybinds.queue {
        upsert(Section::Queue, Some(seq.clone()), Actions::Queue(*action));
    }
    for (seq, action) in &keybinds.directories {
        upsert(Section::Directories, Some(seq.clone()), Actions::Directories(*action));
    }

    for row in &mut rows {
        row.keys.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
    }
    rows.sort_by(|a, b| a.section.order().cmp(&b.section.order()).then(a.display.cmp(&b.display)));
    rows
}

/// Rebind `action` in `section` to `new_key` in the runtime config and key
/// resolver, returning the action's previous keys (needed to persist the
/// change to `keybinds.ron`). The sidecar is NOT written — the caller
/// decides when to persist (the Settings panel stages the change until the
/// panel is closed with Save).
pub(crate) fn apply_remap_runtime(
    ctx: &mut Ctx,
    section: Section,
    action: &Actions,
    new_key: Key,
) -> Result<Vec<KeySequence>> {
    let seq = KeySequence(vec![new_key]);
    let old_keys: Vec<KeySequence> = build_remap_rows(ctx)
        .into_iter()
        .find(|row| row.section == section && actions_eq(&row.action, action))
        .map(|row| row.keys)
        .unwrap_or_default();

    let mut config = ctx.config.as_ref().clone();
    let keybinds = &mut config.keybinds;
    // The new key must not fire anything else; drop it from every map.
    keybinds.global.retain(|k, _| k != &seq);
    keybinds.navigation.retain(|k, _| k != &seq);
    keybinds.directories.retain(|k, _| k != &seq);
    keybinds.queue.retain(|k, _| k != &seq);
    // Rebind the action in its own section (removing its previous keys).
    match (section, action) {
        (Section::Global, Actions::Global(action)) => {
            keybinds.global.retain(|_, v| v != action);
            keybinds.global.insert(seq.clone(), action.clone());
        }
        (Section::Navigation, Actions::Common(action)) => {
            keybinds.navigation.retain(|_, v| v != action);
            keybinds.navigation.insert(seq.clone(), action.clone());
        }
        (Section::Queue, Actions::Queue(action)) => {
            keybinds.queue.retain(|_, v| v != action);
            keybinds.queue.insert(seq.clone(), *action);
        }
        (Section::Directories, Actions::Directories(action)) => {
            keybinds.directories.retain(|_, v| v != action);
            keybinds.directories.insert(seq.clone(), *action);
        }
        _ => unreachable!("remap row always matches its section"),
    }
    ctx.config = std::sync::Arc::new(config);
    ctx.key_resolver = KeyResolver::new(&ctx.config);
    Ok(old_keys)
}

/// Write the remap into the keybinds.ron sidecar: drop the action's previous
/// keys and the new key from the removal list, then bind the new key.
pub(crate) fn save_override(
    section: Section,
    action: &Actions,
    new_key: &KeySequence,
    old_keys: &[KeySequence],
) -> Result<()> {
    use crate::config::keys::{
        CommonActionFile, DirectoriesActionsFile, GlobalActionFile, KeybindsOverrides,
        QueueActionsFile,
    };

    let mut overrides = KeybindsOverrides::load().unwrap_or_default();
    overrides.remove.retain(|k| k != new_key);
    for old in old_keys {
        if old != new_key && !overrides.remove.contains(old) {
            overrides.remove.push(old.clone());
        }
    }

    match section {
        Section::Global => {
            let Actions::Global(action) = action else { return Ok(()) };
            let action_file: GlobalActionFile = action.clone().into();
            overrides.global.retain(|_, v| v != &action_file);
            overrides.global.retain(|k, _| k != new_key);
            overrides.global.insert(new_key.clone(), action_file);
        }
        Section::Navigation => {
            let Actions::Common(action) = action else { return Ok(()) };
            let action_file: CommonActionFile = action.clone().into();
            overrides.navigation.retain(|_, v| v != &action_file);
            overrides.navigation.retain(|k, _| k != new_key);
            overrides.navigation.insert(new_key.clone(), action_file);
        }
        Section::Queue => {
            let Actions::Queue(action) = action else { return Ok(()) };
            let action_file: QueueActionsFile = (*action).into();
            overrides.queue.retain(|_, v| v != &action_file);
            overrides.queue.retain(|k, _| k != new_key);
            overrides.queue.insert(new_key.clone(), action_file);
        }
        Section::Directories => {
            let Actions::Directories(action) = action else { return Ok(()) };
            let action_file: DirectoriesActionsFile = (*action).into();
            overrides.directories.retain(|_, v| v != &action_file);
            overrides.directories.retain(|k, _| k != new_key);
            overrides.directories.insert(new_key.clone(), action_file);
        }
    }

    overrides.save()
}
