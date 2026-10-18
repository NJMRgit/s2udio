use std::{collections::HashMap, sync::Arc};

use itertools::Itertools;

use crate::{
    config::keys::{CommonAction, Key, KeyConfig},
    shared::keys::actions::Actions,
    ui::input::InputModeDiscriminants,
};

#[derive(Debug, Default)]
pub(super) struct KeyTreeNode {
    children: HashMap<Key, KeyTreeNode>,
    actions: Arc<Vec<Actions>>,
}

fn is_insert_mode_action(action: &CommonAction) -> bool {
    matches!(action, CommonAction::Close | CommonAction::Confirm)
}

impl KeyTreeNode {
    pub fn build_trie(cfg: &KeyConfig, mode: InputModeDiscriminants) -> Self {
        let mut root = KeyTreeNode::default();

        if matches!(mode, InputModeDiscriminants::Normal) {
            for seq in &cfg.global {
                root.insert(&seq.0.0, Actions::Global(seq.1.clone()));
            }
        }

        for seq in cfg.navigation.iter().filter(|(_, act)| {
            matches!(mode, InputModeDiscriminants::Normal) || is_insert_mode_action(act)
        }) {
            root.insert(&seq.0.0, Actions::Common(seq.1.clone()));
        }

        if matches!(mode, InputModeDiscriminants::Normal) {
            for seq in &cfg.queue {
                root.insert(&seq.0.0, Actions::Queue(*seq.1));
            }
        }

        if matches!(mode, InputModeDiscriminants::Normal) {
            for seq in &cfg.directories {
                root.insert(&seq.0.0, Actions::Directories(*seq.1));
            }
        }

        if matches!(mode, InputModeDiscriminants::Normal) {
            #[cfg(debug_assertions)]
            for seq in &cfg.logs {
                root.insert(&seq.0.0, Actions::Logs(*seq.1));
            }
        }

        root
    }

    pub fn get(&self, key: &Key) -> Option<&KeyTreeNode> {
        self.children.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    pub fn action(&self) -> &Arc<Vec<Actions>> {
        &self.actions
    }

    fn insert(&mut self, keys: &[Key], action: Actions) {
        let mut node = self;
        for key in keys {
            node = node.children.entry(*key).or_default();
        }

        if !node.actions.is_empty() {
            log::warn!(
                actions:? = node.actions,
                action:?,
                seq = keys.iter().map(|k| k.to_string()).join("").as_str();
                "Multiple existing actions for key sequence"
            );
        }

        // Insert action to the final node in sequence.
        if let Some(actions) = Arc::get_mut(&mut node.actions) {
            actions.push(action);
        } else {
            // This should never happen since the trie is built from scratch on
            // initialization.
            log::warn!(
                seq = keys.iter().map(|k| k.to_string()).join("").as_str();
                "Failed to insert action for key sequence",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use crate::config::keys::{GlobalAction, KeySequence};

    fn seq(spec: &str) -> KeySequence {
        KeySequence::from_str(spec).unwrap_or_else(|e| panic!("{spec:?} should parse as a key sequence: {e}"))
    }

    /// Walk the built trie and collect every sequence that ended up with
    /// more than one action.
    ///
    /// `build_trie` is pure — no terminal, MPD connection or `Ctx` — so the
    /// whole default keymap can be checked headlessly. Before this module
    /// `trie.rs` had no test at all, which is why the round-111 report's nine
    /// startup conflict warnings had nothing pinning them.
    fn collisions(cfg: &KeyConfig, mode: InputModeDiscriminants) -> Vec<String> {
        let mut found = Vec::new();
        let root = KeyTreeNode::build_trie(cfg, mode);
        let mut stack = vec![(Vec::new(), &root)];
        while let Some((prefix, node)) = stack.pop() {
            if node.action().len() > 1 {
                let seq = prefix.iter().map(std::string::ToString::to_string).collect::<String>();
                let actions = node
                    .action()
                    .iter()
                    .map(|a| format!("{a:?}"))
                    .collect::<Vec<_>>()
                    .join("|");
                found.push(format!("{seq} => {actions}"));
            }
            for (key, child) in &node.children {
                let mut next = prefix.clone();
                next.push(*key);
                stack.push((next, child));
            }
        }
        found.sort();
        found
    }

    fn colliding_sequences(cfg: &KeyConfig, mode: InputModeDiscriminants) -> Vec<String> {
        collisions(cfg, mode)
            .iter()
            .map(|entry| entry.split(" => ").next().unwrap_or("").to_string())
            .collect()
    }

    /// The contract this pins: with the built-in defaults, the *entire* set
    /// of colliding sequences is the nine the round-111 report observed, and
    /// each is an intentional cross-section overlap (a global key that also
    /// means something in a pane). The recon that preceded this test
    /// confirmed all nine are deliberate: the focused pane claims the half it
    /// wants via `ActionEvent::claim_*`, and `ui/mod.rs` compensates for
    /// `<Esc>`.
    ///
    /// It is an exact-set assertion on purpose. A tenth collision — or the
    /// silent loss of one of these — is a keybinding regression that should
    /// fail the build rather than scroll past in a startup log.
    #[test]
    fn default_keymap_has_exactly_the_documented_cross_section_collisions() {
        let cfg = KeyConfig::default();
        let found = collisions(&cfg, InputModeDiscriminants::Normal);
        let sequences = colliding_sequences(&cfg, InputModeDiscriminants::Normal);

        let mut expected =
            vec!["<Esc>", "<Left>", "<Right>", "<S-Tab>", "a", "c", "d", "s", "w"];
        expected.sort_unstable();

        assert_eq!(
            sequences, expected,
            "the default keymap's cross-section collisions changed.\n\
             Got:\n{found:#?}\n\
             These nine are intentional (docs/design/Frontend/interaction.md). \
             A new entry means two sections now claim the same key: give one a \
             different sequence, or document the overlap and update this list."
        );
    }

    /// `<Esc>` is the collision with real behavioural weight: the report
    /// asked whether `Common(Close)` losing to `Global(ShowSettings)` is a
    /// bug. It is not — `ui/mod.rs` deliberately inspects the raw action list
    /// so a pane claiming Close cannot swallow Settings — but the *presence*
    /// of both is load-bearing, since consumers take the first action of
    /// their kind. Pin it so a `build_trie` reorder cannot quietly drop one.
    #[test]
    fn esc_keeps_close_and_show_settings_together() {
        let cfg = KeyConfig::default();
        let root = KeyTreeNode::build_trie(&cfg, InputModeDiscriminants::Normal);
        let node = root.get(&seq("<Esc>").0[0]).expect("<Esc> is bound in the default keymap");

        let rendered: Vec<String> = node.action().iter().map(|a| format!("{a:?}")).collect();
        assert!(
            rendered.iter().any(|a| a.contains("Close")),
            "<Esc> must still offer Close so modals/popups can be dismissed: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|a| a.contains("ShowSettings")),
            "<Esc> must still open Settings: {rendered:?}"
        );
    }

    /// Insert mode is a different trie: it only carries the handful of
    /// common actions that make sense while typing. It must not inherit the
    /// normal-mode collisions, or typing in the lyrics/search box would start
    /// opening panes.
    #[test]
    fn insert_mode_trie_has_no_collisions() {
        let cfg = KeyConfig::default();
        let found = collisions(&cfg, InputModeDiscriminants::Insert);
        assert!(
            found.is_empty(),
            "insert mode must not inherit normal-mode key collisions, got:\n{found:#?}"
        );
    }

    /// Guards the guard: if `build_trie` ever stopped recording more than one
    /// action per node, the exact-set test above would pass vacuously. This
    /// forces a collision and asserts the walk still detects it.
    #[test]
    fn a_deliberate_collision_is_detected() {
        let mut cfg = KeyConfig::default();
        // `w` already means FolderUp (directories) and Up (navigation).
        cfg.global.insert(seq("w"), GlobalAction::ShowSettings);

        let found = collisions(&cfg, InputModeDiscriminants::Normal);
        let w = found.iter().find(|e| e.starts_with("w => ")).expect("w collides");
        assert_eq!(
            w.matches('|').count(),
            2,
            "three actions should render as two separators, got: {w}"
        );
    }
}
