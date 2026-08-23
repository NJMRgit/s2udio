use std::{cell::RefCell, sync::Arc, time::Duration};
use crossterm::event::KeyEventKind;
use itertools::Itertools;
use crate::{
    config::{Config, keys::Key},
    ctx::Ctx,
    shared::{
        events::AppEvent, id::{self, Id},
        keys::{actions::Actions, trie::KeyTreeNode},
    },
    ui::input::{InputMode, InputModeDiscriminants},
};
#[derive(Debug)]
pub struct KeyResolver {
    normal_root: KeyTreeNode,
    insert_root: KeyTreeNode,
    timeout_id: Id,
    buffer: RefCell<Vec<Key>>,
    normal_timeout: Duration,
    insert_timeout: Duration,
}
#[derive(Debug)]
enum TraverseResult {
    Exact(Arc<Vec<Actions>>),
    Ambiguous(Arc<Vec<Actions>>),
    Prefix,
    Mismatch,
}
impl KeyResolver {
    pub fn new(cfg: &Config) -> Self {
        Self {
            normal_root: KeyTreeNode::build_trie(
                &cfg.keybinds,
                InputModeDiscriminants::Normal,
            ),
            insert_root: KeyTreeNode::build_trie(
                &cfg.keybinds,
                InputModeDiscriminants::Insert,
            ),
            timeout_id: id::new(),
            buffer: RefCell::new(Vec::new()),
            normal_timeout: Duration::from_millis(cfg.normal_timeout_ms),
            insert_timeout: Duration::from_millis(cfg.insert_timeout_ms),
        }
    }
    pub fn buffer_to_string(&self) -> String {
        self.buffer.borrow().iter().map(|k| k.to_string()).join("")
    }
    pub fn handle_timeout(&self, ctx: &Ctx) {
        log::trace!(q:? = self.buffer; "Key timeout occurred");
        let mut buf = self.buffer.borrow_mut();
        if buf.is_empty() {
            return;
        }
        let root = match ctx.input.mode() {
            InputMode::Normal => &self.normal_root,
            InputMode::Insert(_) => &self.insert_root,
        };
        match ctx.input.mode() {
            InputMode::Normal => {
                match self.traverse(&buf, root) {
                    TraverseResult::Exact(action) => {
                        self.execute_action(action, ctx);
                    }
                    TraverseResult::Ambiguous(action) => {
                        self.execute_action(action, ctx);
                    }
                    TraverseResult::Mismatch => {}
                    TraverseResult::Prefix => {}
                }
            }
            InputMode::Insert(_) => {
                match self.traverse(&buf, root) {
                    TraverseResult::Exact(action) => {
                        self.flush_insert_buffer(
                            Some(action),
                            std::mem::take(&mut buf),
                            ctx,
                        );
                    }
                    TraverseResult::Ambiguous(action) => {
                        self.flush_insert_buffer(
                            Some(action),
                            std::mem::take(&mut buf),
                            ctx,
                        );
                    }
                    TraverseResult::Mismatch => {
                        self.flush_insert_buffer(None, std::mem::take(&mut buf), ctx);
                    }
                    TraverseResult::Prefix => {
                        self.flush_insert_buffer(None, std::mem::take(&mut buf), ctx);
                    }
                }
            }
        }
        buf.clear();
    }
    pub fn handle_key_event(&self, key: Key, kind: KeyEventKind, ctx: &Ctx) {
        self.cancel_timeout(ctx);
        let mut buf = self.buffer.borrow_mut();
        buf.push(key);
        match ctx.input.mode() {
            InputMode::Normal => {
                match self.traverse(&buf, &self.normal_root) {
                    TraverseResult::Exact(action) => {
                        let repeat = kind == KeyEventKind::Repeat
                            && action.iter().any(|a| a.steps_once());
                        if !repeat {
                            self.execute_action(action, ctx);
                        }
                        buf.clear();
                    }
                    TraverseResult::Ambiguous(_action) => {
                        self.schedule_timeout(ctx);
                    }
                    TraverseResult::Mismatch => {
                        buf.clear();
                    }
                    TraverseResult::Prefix => {
                        self.schedule_timeout(ctx);
                    }
                }
            }
            InputMode::Insert(_) => {
                match self.traverse(&buf, &self.insert_root) {
                    TraverseResult::Exact(action) => {
                        self.flush_insert_buffer(
                            Some(action),
                            std::mem::take(&mut buf),
                            ctx,
                        );
                    }
                    TraverseResult::Ambiguous(_action) => {
                        self.schedule_timeout(ctx);
                    }
                    TraverseResult::Mismatch => {
                        self.flush_insert_buffer(None, std::mem::take(&mut buf), ctx);
                    }
                    TraverseResult::Prefix => {
                        self.schedule_timeout(ctx);
                    }
                }
            }
        }
    }
    fn schedule_timeout(&self, ctx: &Ctx) {
        let timeout = match ctx.input.mode() {
            InputMode::Normal => self.normal_timeout,
            InputMode::Insert(_) => self.insert_timeout,
        };
        ctx.scheduler
            .schedule_replace(
                self.timeout_id,
                timeout,
                |(tx, _)| {
                    log::trace!("Key sequence timeout reached, sending timeout event");
                    Ok(tx.send(AppEvent::KeyTimeout)?)
                },
            );
    }
    fn cancel_timeout(&self, ctx: &Ctx) {
        ctx.scheduler.cancel(self.timeout_id);
    }
    fn execute_action(&self, action: Arc<Vec<Actions>>, ctx: &Ctx) {
        if let Err(err) = ctx
            .app_event_sender
            .send(AppEvent::ActionResolved(action.into()))
        {
            log::error!(err:?; "Failed to send ActionResolved event");
        }
    }
    fn flush_insert_buffer(
        &self,
        action: Option<Arc<Vec<Actions>>>,
        buf: Vec<Key>,
        ctx: &Ctx,
    ) {
        if let Err(err) = ctx
            .app_event_sender
            .send(AppEvent::InsertModeFlush((action.map(|a| a.into()), buf)))
        {
            log::error!(err:?; "Failed to send InsertModeFlush event");
        }
    }
    fn traverse(&self, keys: &[Key], root: &KeyTreeNode) -> TraverseResult {
        let mut curr = root;
        for key in keys {
            match curr.get(key) {
                Some(next) => curr = next,
                None => return TraverseResult::Mismatch,
            }
        }
        match (curr.action().is_empty(), curr.is_empty()) {
            (false, true) => TraverseResult::Exact(Arc::clone(curr.action())),
            (false, false) => TraverseResult::Ambiguous(Arc::clone(curr.action())),
            (true, false) => TraverseResult::Prefix,
            (true, true) => {
                log::warn!(
                    keys:?; "Key sequence leads to a node with no action and no children"
                );
                TraverseResult::Mismatch
            }
        }
    }
}
