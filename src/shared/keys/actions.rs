#[cfg(debug_assertions)]
use crate::config::keys::LogsActions;
use crate::config::keys::{CommonAction, DirectoriesActions, GlobalAction, QueueActions};

#[derive(Debug, Clone)]
pub enum Actions {
    Global(GlobalAction),
    Common(CommonAction),
    Directories(DirectoriesActions),
    Queue(QueueActions),
    #[cfg(debug_assertions)]
    Logs(LogsActions),
}

impl From<GlobalAction> for Actions {
    fn from(value: GlobalAction) -> Self {
        Actions::Global(value)
    }
}

impl From<CommonAction> for Actions {
    fn from(value: CommonAction) -> Self {
        Actions::Common(value)
    }
}

impl From<QueueActions> for Actions {
    fn from(value: QueueActions) -> Self {
        Actions::Queue(value)
    }
}

impl From<DirectoriesActions> for Actions {
    fn from(value: DirectoriesActions) -> Self {
        Actions::Directories(value)
    }
}

#[cfg(debug_assertions)]
impl From<LogsActions> for Actions {
    fn from(value: LogsActions) -> Self {
        Actions::Logs(value)
    }
}

impl Actions {
    pub fn as_global(&self) -> Option<&GlobalAction> {
        if let Actions::Global(action) = self { Some(action) } else { None }
    }

    pub fn as_common(&self) -> Option<&CommonAction> {
        if let Actions::Common(action) = self { Some(action) } else { None }
    }

    pub fn as_queue(&self) -> Option<&QueueActions> {
        if let Actions::Queue(action) = self { Some(action) } else { None }
    }

    pub fn as_directories(&self) -> Option<&DirectoriesActions> {
        if let Actions::Directories(action) = self { Some(action) } else { None }
    }

    #[cfg(debug_assertions)]
    pub fn as_logs(&self) -> Option<&LogsActions> {
        if let Actions::Logs(action) = self { Some(action) } else { None }
    }

    /// Actions that must fire once per key press even when the key is held
    /// down: the terminal auto-repeat (Repeat events under the kitty
    /// keyboard protocol) must not re-trigger them. Tab navigation is the
    /// only single-step action family today — holding Tab / Shift+Q /
    /// Shift+E moves exactly one tab.
    pub fn steps_once(&self) -> bool {
        matches!(
            self,
            Actions::Global(
                GlobalAction::NextTab | GlobalAction::PreviousTab | GlobalAction::ToggleMpdMode
            )
        )
    }
}
