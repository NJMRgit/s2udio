use std::fmt::Display;

use anyhow::Result;
use bon::bon;
use ratatui::Frame;

use super::{ListConfirm, ListModal, Modal};
use crate::{
    ctx::Ctx,
    shared::{
        id::Id,
        keys::ActionEvent,
        mouse_event::MouseEvent,
    },
    ui::input::InputResultEvent,
};

/// The single-select option picker (Phase-3 thin adapter over `ListModal`):
/// a numbered, scrollable list plus Confirm/Cancel buttons; Enter moves
/// the focus to the buttons where a second Enter confirms, Esc closes,
/// wheel scrolls, click/double-click select/confirm.
#[derive(derive_more::Debug)]
pub struct SelectModal<'a, V: Display, Callback: FnOnce(&Ctx, V, usize) -> Result<()> + Send + Sync + 'a> {
    #[debug(skip)]
    inner: ListModal<'a, V>,
    #[debug(skip)]
    _callback: std::marker::PhantomData<Callback>,
}

#[bon]
impl<'a, V: Display + std::fmt::Debug, Callback: FnOnce(&Ctx, V, usize) -> Result<()> + Send + Sync + 'a>
    SelectModal<'a, V, Callback>
{
    #[builder]
    pub fn new(
        ctx: &Ctx,
        title: Option<&'a str>,
        options: Vec<V>,
        on_confirm: Callback,
        confirm_label: Option<&'a str>,
    ) -> Self {
        fn row<V: Display>(v: &V, _marked: bool, idx: usize) -> String {
            format!("{:>3}: {v}", idx + 1)
        }
        let inner = ListModal::builder()
            .ctx(ctx)
            .title(title.unwrap_or_default().to_owned())
            .options(options)
            .row_fn(row::<V>)
            .size_fn(|_| (80, 15))
            .buttons(vec![confirm_label.unwrap_or("Confirm"), "Cancel"])
            .confirm_buttons(1)
            .on_confirm(move |ctx, confirm: ListConfirm<V>| {
                let ListConfirm { value, index, .. } = confirm;
                let value =
                    value.expect("single-select confirm always carries the confirmed option");
                (on_confirm)(ctx, value, index)
            })
            .build();
        Self { inner, _callback: std::marker::PhantomData }
    }
}

impl<V: Display + std::fmt::Debug, Callback: FnOnce(&Ctx, V, usize) -> Result<()> + Send + Sync> Modal
    for SelectModal<'_, V, Callback>
{
    fn id(&self) -> Id {
        self.inner.id()
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        self.inner.render(frame, ctx)
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &Ctx) -> Result<()> {
        self.inner.handle_insert_mode(kind, ctx)
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        self.inner.handle_key(key, ctx)
    }

    fn handle_raw_key(&mut self, key: crossterm::event::KeyEvent, ctx: &mut Ctx) -> Result<bool> {
        self.inner.handle_raw_key(key, ctx)
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        self.inner.handle_mouse_event(event, ctx)
    }
}
