use std::borrow::Cow;

use anyhow::Result;
use ratatui::{Frame, symbols};

use super::UiEvent;
use crate::{
    MpdQueryResult,
    ctx::Ctx,
    shared::{id::Id, keys::ActionEvent, mouse_event::MouseEvent},
    ui::input::InputResultEvent,
};

pub mod add_random_modal;
pub mod confirm_modal;
pub mod decoders;
pub mod downloads;
pub mod info_list_modal;
pub mod info_modal;
pub mod input_modal;
pub mod language;
pub mod list_modal;
pub use info_list_modal::InfoListModal;
pub use list_modal::{ListConfirm, ListModal};
pub mod menu;
pub mod outputs;
pub mod paste;
pub mod remap_keys;
pub mod select_modal;
pub mod settings;
pub mod tab_help;
pub mod torrent_file_picker;

#[allow(unused)]
pub(crate) trait Modal: std::fmt::Debug {
    fn id(&self) -> Id;

    fn render(&mut self, frame: &mut Frame, ctx: &mut crate::ctx::Ctx) -> Result<()>;

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()>;

    /// Handle a raw key event before it reaches the key resolver. Returns
    /// `true` when the key was consumed (used by the key-remapping view to
    /// capture the new key).
    fn handle_raw_key(&mut self, _key: crossterm::event::KeyEvent, _ctx: &mut Ctx) -> Result<bool> {
        Ok(false)
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()>;

    /// Whether a right click on this modal closes it (like Esc's Close
    /// action). Modals that consume right-click for their own in-modal
    /// actions (or need to route it through `handle_mouse_event`, e.g. the
    /// Settings panel's save/discard prompt) override this to `false`.
    fn right_click_closes(&self) -> bool {
        true
    }

    fn on_query_finished(
        &mut self,
        id: &'static str,
        data: &mut MpdQueryResult,
        ctx: &Ctx,
    ) -> Result<()> {
        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn replacement_id(&self) -> Option<&Cow<'static, str>> {
        None
    }

    fn hide(&mut self, ctx: &Ctx) -> Result<()> {
        ctx.app_event_sender
            .send(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(self.id())))?;
        Ok(())
    }
}

const BUTTON_GROUP_SYMBOLS: symbols::border::Set = symbols::border::Set {
    top_right: symbols::line::NORMAL.vertical_left,
    top_left: symbols::line::NORMAL.vertical_right,
    ..symbols::border::ROUNDED
};
