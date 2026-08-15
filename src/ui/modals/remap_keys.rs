//! Key-remapping helpers shared by the Settings panel's keybinds section.
//! The remap flow: list the configured actions with their keys, pick one,
//! capture the new key, rebind it in the runtime config. Persisting to the
//! `keybinds.ron` sidecar is deferred to the panel's Save action.

use anyhow::Result;
use itertools::Itertools;

use crate::{
    config::keys::{Key, KeySequence, KeyConfig, QueueActions},
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
            CommonAction::LyricsDeleteLine => "Delete lyric line",
            CommonAction::LyricsEditLine => "Edit lyric line text",
            CommonAction::LyricsInsertBefore => "Split lyric line before word",
            CommonAction::LyricsInsertAfter => "Split lyric line after word",
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

/// The remapable actions with their current keys, grouped and sorted.
pub(crate) fn build_remap_rows(ctx: &Ctx) -> Vec<RemapRow> {
    let keybinds: &KeyConfig = &ctx.config.keybinds;
    let mut rows: Vec<RemapRow> = Vec::new();

    let mut push = |section: Section, seq: KeySequence, action: Actions| {
        // Skip actions that cannot be persisted back to the config format.
        if matches!(action, Actions::Queue(QueueActions::Unused)) {
            return;
        }
        let display = action_display(&action);
        if let Some(row) =
            rows.iter_mut().find(|row| row.section == section && actions_eq(&row.action, &action))
        {
            row.keys.push(seq);
        } else {
            rows.push(RemapRow { section, action, keys: vec![seq], display });
        }
    };

    for (seq, action) in &keybinds.global {
        push(Section::Global, seq.clone(), Actions::Global(action.clone()));
    }
    for (seq, action) in &keybinds.navigation {
        push(Section::Navigation, seq.clone(), Actions::Common(action.clone()));
    }
    for (seq, action) in &keybinds.queue {
        push(Section::Queue, seq.clone(), Actions::Queue(*action));
    }
    for (seq, action) in &keybinds.directories {
        push(Section::Directories, seq.clone(), Actions::Directories(*action));
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
