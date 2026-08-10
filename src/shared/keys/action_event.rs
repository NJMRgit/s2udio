use std::{ops::Not, sync::Arc};

#[cfg(debug_assertions)]
use crate::config::keys::LogsActions;
use crate::{
    config::keys::{CommonAction, DirectoriesActions, GlobalAction, QueueActions},
    shared::keys::actions::Actions,
};

#[derive(Debug)]
pub struct ActionEvent {
    pub actions: Arc<Vec<Actions>>,
    already_handled: bool,
    /// The keypress was fully consumed by a pane (e.g. Esc cleared a
    /// selection), so the global half bound to the same key (e.g.
    /// ShowSettings) must not also fire. Independent of
    /// `already_handled`, which `abandon()` resets.
    consumed: bool,
}

impl From<Arc<Vec<Actions>>> for ActionEvent {
    fn from(value: Arc<Vec<Actions>>) -> Self {
        Self { actions: value, already_handled: false, consumed: false }
    }
}

impl ActionEvent {
    pub fn abandon(&mut self) {
        self.already_handled = false;
    }

    /// Mark the keypress as fully consumed: no further handlers should
    /// act on it (used when Esc clears a selection so the settings panel
    /// bound to the same key does not also open).
    pub fn consume(&mut self) {
        self.consumed = true;
    }

    pub fn is_consumed(&self) -> bool {
        self.consumed
    }

    pub fn claim_global(&mut self) -> Option<&GlobalAction> {
        let result = self
            .already_handled
            .not()
            .then(|| self.actions.iter().find_map(|act| act.as_global()))
            .flatten();
        if result.is_some() {
            self.already_handled = true;
        }
        result
    }

    pub fn claim_common(&mut self) -> Option<&CommonAction> {
        let result = self
            .already_handled
            .not()
            .then(|| self.actions.iter().find_map(|act| act.as_common()))
            .flatten();
        if result.is_some() {
            self.already_handled = true;
        }
        result
    }

    pub fn claim_queue(&mut self) -> Option<&QueueActions> {
        let result = self
            .already_handled
            .not()
            .then(|| self.actions.iter().find_map(|act| act.as_queue()))
            .flatten();
        if result.is_some() {
            self.already_handled = true;
        }
        result
    }

    pub fn claim_directories(&mut self) -> Option<&DirectoriesActions> {
        let result = self
            .already_handled
            .not()
            .then(|| self.actions.iter().find_map(|act| act.as_directories()))
            .flatten();
        if result.is_some() {
            self.already_handled = true;
        }
        result
    }

    #[cfg(debug_assertions)]
    pub fn claim_logs(&mut self) -> Option<&LogsActions> {
        let result = self
            .already_handled
            .not()
            .then(|| self.actions.iter().find_map(|act| act.as_logs()))
            .flatten();
        if result.is_some() {
            self.already_handled = true;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consume_survives_abandon_and_blocks_nothing_else() {
        // Esc binds Common(Close) + Global(ShowSettings) to one keypress.
        let mut ev: ActionEvent = Arc::new(vec![
            Actions::Common(CommonAction::Close),
            Actions::Global(GlobalAction::ShowSettings),
        ])
        .into();
        assert!(!ev.is_consumed());

        // A pane clears its selection: it claims the Close half and marks
        // the keypress consumed so the settings half does not fire.
        assert!(ev.claim_common().is_some());
        ev.consume();
        assert!(ev.is_consumed());

        // The pane's global handling then abandons (re-enabling claim_global
        // for later stages) — the consumed flag must survive that.
        ev.abandon();
        assert!(ev.is_consumed());
        assert!(ev.claim_global().is_some());
    }

    #[test]
    fn not_consumed_keeps_the_global_half_available() {
        let mut ev: ActionEvent = Arc::new(vec![
            Actions::Common(CommonAction::Close),
            Actions::Global(GlobalAction::ShowSettings),
        ])
        .into();
        // No selection: the pane does not consume, so the settings half
        // still fires.
        assert!(!ev.is_consumed());
        assert!(ev.claim_global().is_some());
    }
}
