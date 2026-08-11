use anyhow::Result;
use ratatui::Frame;

use super::{InfoListModal, Modal};
use crate::{
    ctx::Ctx,
    mpd::commands::Decoder,
    shared::{
        id::Id,
        keys::ActionEvent,
        mouse_event::MouseEvent,
    },
    ui::input::InputResultEvent,
};

/// The MPD decoder-plugins table (Phase-3 thin adapter over
/// `InfoListModal`): a read-only three-column list of the decoder name,
/// MIME types and suffixes, scrollable with the wheel and closeable with
/// Esc.
#[derive(Debug)]
pub struct DecodersModal {
    inner: InfoListModal,
}

impl DecodersModal {
    pub fn new(decoders: Vec<Decoder>) -> Self {
        let rows: Vec<Vec<String>> = decoders
            .into_iter()
            .map(|decoder| {
                vec![
                    decoder.name.clone(),
                    decoder.mime_types.join(", "),
                    decoder.suffixes.join(", "),
                ]
            })
            .collect();
        let inner = InfoListModal::builder()
            .title("Decoder plugins")
            .column_widths(&[10, 45, 45])
            .header(vec!["Plugin".to_owned(), "MIME types".to_owned(), "Suffixes".to_owned()])
            .rows(rows)
            .build();
        Self { inner }
    }
}

impl Modal for DecodersModal {
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

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        self.inner.handle_mouse_event(event, ctx)
    }
}
