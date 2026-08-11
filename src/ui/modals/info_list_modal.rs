use std::time::Duration;

use anyhow::Result;
use bon::bon;
use itertools::Itertools;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    macros::constraint,
    style::Style,
    symbols::border,
    text::Text,
    widgets::{Block, Borders, Cell, Clear, Row, Table, TableState},
};

use super::Modal;
use crate::{
    config::keys::CommonAction,
    ctx::Ctx,
    mpd::commands::Song,
    shared::{
        ext::duration::DurationExt,
        id::{self, Id},
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::dirstack::DirState,
};

/// The row source of `InfoListModal` (a local newtype so `From<Vec<Song>>`
/// and `From<&Song>` can provide the key/value rows).
#[derive(Debug)]
pub struct KeyValues(Vec<Vec<String>>);

/// One master implementation of the "read-only N-column table modal"
/// shape (Phase 3): a bordered table with a header row, scrollbar,
/// wheel/click selection and Close. Absorbs the legacy two-column
/// key/value info modal and the three-column decoders modal; per-modal
/// differences are args (`rows`, `header` labels, `column_widths`,
/// `title`, `size`) — never a fork.
#[derive(Debug)]
pub struct InfoListModal {
    id: Id,
    scrolling_state: DirState<TableState>,
    table_area: Rect,
    rows: Vec<Vec<String>>,
    column_widths: &'static [u16],
    header: Vec<String>,
    title: &'static str,
    size: (u16, u16),
}

#[bon]
impl InfoListModal {
    #[builder]
    pub fn new(
        rows: impl Into<KeyValues>,
        title: &'static str,
        column_widths: &'static [u16],
        header: Option<Vec<String>>,
        size: Option<(u16, u16)>,
    ) -> Self {
        let mut scrolling_state = DirState::default();
        scrolling_state.select(Some(0), 0);
        Self {
            id: id::new(),
            scrolling_state,
            rows: rows.into().0,
            table_area: Rect::default(),
            title,
            column_widths,
            header: header.unwrap_or_else(|| vec!["Tag".to_owned(), "Value".to_owned()]),
            size: size.unwrap_or((80, 80)),
        }
    }

    /// Wrap every cell at its column's width and zip the wrapped lines
    /// into table rows (shorter columns pad with empty cells).
    fn rows_for<'a>(&self, column_areas: &[Rect]) -> Vec<Row<'a>> {
        let wrapped: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|cells| {
                cells
                    .iter()
                    .zip(column_areas.iter())
                    .map(|(cell, area)| {
                        textwrap::wrap(cell, area.width as usize)
                            .into_iter()
                            .map(String::from)
                            .collect()
                    })
                    .collect()
            })
            .collect();

        let max_lines = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        (0..max_lines)
            .map(|line| {
                Row::new(
                    (0..column_areas.len())
                        .map(|col| {
                            Cell::from(Text::from(
                                wrapped.get(line).and_then(|r| r.get(col)).cloned().unwrap_or_default(),
                            ))
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }
}

impl Modal for InfoListModal {
    fn id(&self) -> Id {
        self.id
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        let (w, h) = self.size;
        let popup_area = frame.area().centered(constraint!(==w%), constraint!(==h%));
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(Block::default().style(Style::default().bg(bg_color)), popup_area);
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(ctx.config.as_border_style())
            .title_alignment(ratatui::prelude::Alignment::Center)
            .title(self.title);

        let margin = Margin { horizontal: 1, vertical: 0 };
        let [header_area, table_area] =
            Layout::vertical([Constraint::Length(2), Constraint::Percentage(100)])
                .areas(block.inner(popup_area));
        let header_area = header_area.inner(margin);
        let table_area = table_area.inner(margin);

        let column_constraints =
            self.column_widths.iter().map(|w| Constraint::Percentage(*w)).collect_vec();
        let column_areas = Layout::horizontal(&column_constraints).spacing(1).split(table_area);

        let rows = self.rows_for(&column_areas);

        self.scrolling_state.set_content_and_viewport_len(rows.len(), table_area.height.into());

        let header_table = Table::new(
            vec![Row::new(self.header.iter().map(|h| Cell::from(h.as_str())).collect::<Vec<_>>())],
            &column_constraints,
        )
        .column_spacing(1)
        .block(
            Block::default().borders(Borders::BOTTOM).border_style(ctx.config.as_border_style()),
        );
        let table = Table::new(rows, &column_constraints)
            .column_spacing(1)
            .style(ctx.config.as_text_style())
            .row_highlight_style(ctx.config.theme.current_item_style);

        self.table_area = table_area;

        frame.render_widget(block, popup_area);
        frame.render_widget(header_table, header_area);
        frame.render_stateful_widget(table, table_area, self.scrolling_state.as_render_state_ref());
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar() {
            frame.render_stateful_widget(
                scrollbar,
                popup_area.inner(Margin { horizontal: 0, vertical: 1 }),
                self.scrolling_state.as_scrollbar_state_ref(),
            );
        }

        return Ok(());
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::DownHalf => {
                    self.scrolling_state.next_half_viewport(ctx.config.scrolloff);

                    ctx.render()?;
                }
                CommonAction::UpHalf => {
                    self.scrolling_state.prev_half_viewport(ctx.config.scrolloff);

                    ctx.render()?;
                }
                CommonAction::Up => {
                    self.scrolling_state.prev(ctx.config.scrolloff, ctx.config.wrap_navigation);

                    ctx.render()?;
                }
                CommonAction::Down => {
                    self.scrolling_state.next(ctx.config.scrolloff, ctx.config.wrap_navigation);

                    ctx.render()?;
                }
                CommonAction::Bottom => {
                    self.scrolling_state.last();

                    ctx.render()?;
                }
                CommonAction::Top => {
                    self.scrolling_state.first();

                    ctx.render()?;
                }
                CommonAction::Close => {
                    self.hide(ctx)?;
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        if !self.table_area.contains(event.into()) {
            return Ok(());
        }

        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                let y: usize = event.y.saturating_sub(self.table_area.y).into();
                if let Some(idx) = self.scrolling_state.get_at_rendered_row(y) {
                    self.scrolling_state.select(Some(idx), ctx.config.scrolloff);
                    ctx.render()?;
                }
            }
            MouseEventKind::MiddleClick => {}
            MouseEventKind::RightClick => {}
            MouseEventKind::ScrollDown => {
                self.scrolling_state.scroll_down(ctx.config.scroll_amount, ctx.config.scrolloff);
                ctx.render()?;
            }
            MouseEventKind::ScrollUp => {
                self.scrolling_state.scroll_up(ctx.config.scroll_amount, ctx.config.scrolloff);
                ctx.render()?;
            }
            MouseEventKind::Drag { drag_start_position: _ } => {}
            MouseEventKind::LeftRelease => {}
            MouseEventKind::Moved => {}
        }

        Ok(())
    }
}

impl From<Vec<Vec<String>>> for KeyValues {
    fn from(rows: Vec<Vec<String>>) -> Self {
        KeyValues(rows)
    }
}

impl From<Vec<Song>> for KeyValues {
    fn from(value: Vec<Song>) -> Self {
        let mut result = Vec::new();

        let total_duration: Duration = value.iter().filter_map(|v| v.duration).sum();
        let total_artists = value
            .iter()
            .filter_map(|v| v.metadata.get("artist"))
            .flat_map(|tag| tag.iter())
            .unique()
            .count();
        let total_albums = value
            .iter()
            .filter_map(|v| v.metadata.get("album"))
            .flat_map(|tag| tag.iter())
            .unique()
            .count();
        let total_genres = value
            .iter()
            .filter_map(|v| v.metadata.get("genre"))
            .flat_map(|tag| tag.iter())
            .unique()
            .count();

        result.push(vec!["Songs".to_owned(), value.len().to_string()]);
        result.push(vec![
            "Total duration".to_owned(),
            total_duration.to_string(),
        ]);
        result.push(vec!["Artists".to_owned(), total_artists.to_string()]);
        result.push(vec!["Albums".to_owned(), total_albums.to_string()]);
        result.push(vec!["Genres".to_owned(), total_genres.to_string()]);
        KeyValues(result)
    }
}

impl From<&Song> for KeyValues {
    fn from(song: &Song) -> Self {
        let mut result = Vec::new();
        result.push(vec!["File".to_owned(), song.file.clone()]);
        let file_name = song.file_name().unwrap_or_default();
        if !file_name.is_empty() {
            result.push(vec!["Filename".to_owned(), file_name.into_owned()]);
        }

        if let Some(title) = song.metadata.get("title") {
            result.extend(
                title
                    .iter()
                    .map(|item| vec!["Title".to_owned(), item.to_owned()]),
            );
        }

        if let Some(artist) = song.metadata.get("artist") {
            result.extend(
                artist
                    .iter()
                    .map(|item| vec!["Artist".to_owned(), item.to_owned()]),
            );
        }

        if let Some(album) = song.metadata.get("album") {
            result.extend(
                album
                    .iter()
                    .map(|item| vec!["Album".to_owned(), item.to_owned()]),
            );
        }

        let duration = song.duration.as_ref().map(|d| d.as_secs().to_string()).unwrap_or_default();
        if !duration.is_empty() {
            result.push(vec!["Duration".to_owned(), duration]);
        }

        result.extend(
            song.metadata
                .iter()
                .filter(|(key, _)| {
                    !["title", "album", "artist", "duration"].contains(&(*key).as_str())
                })
                .flat_map(|(k, v)| {
                    v.iter().map(|item| vec![k.to_owned(), item.to_owned()])
                }),
        );

        KeyValues(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{config::keys::CommonAction, shared::keys::Actions, ui::ActionEvent};

    fn test_ctx() -> (Ctx, crossbeam::channel::Receiver<crate::AppEvent>) {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        (ctx, app_rx)
    }

    fn render(modal: &mut InfoListModal, ctx: &mut Ctx) -> String {
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, ctx).expect("modal renders"))
            .expect("draw ok");
        terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect()
    }

    /// The two-column key/value shape (legacy InfoListModal behavior):
    /// the header defaults to Tag/Value and the rows render.
    #[test]
    fn two_column_info_renders_tag_value_header_and_rows() {
        let (mut ctx, _rx) = test_ctx();
        let rows = vec![
            vec!["Songs".to_owned(), "2".to_owned()],
            vec!["Total duration".to_owned(), "1:30".to_owned()],
        ];
        let mut modal = InfoListModal::builder()
            .title("Playlist info")
            .column_widths(&[30, 70])
            .rows(rows)
            .size((40, 20))
            .build();
        let out = render(&mut modal, &mut ctx);
        assert!(out.contains("Tag"), "default header renders Tag: {out}");
        assert!(out.contains("Value"), "default header renders Value");
        assert!(out.contains("Songs"), "row renders");
        assert!(out.contains("1:30"), "wrapped row renders");
    }

    /// The three-column decoders shape renders its own header and rows.
    #[test]
    fn three_column_decoders_shape_renders_custom_header() {
        let (mut ctx, _rx) = test_ctx();
        let rows = vec![
            vec!["mad".to_owned(), "audio/mpeg".to_owned(), "mp3".to_owned()],
            vec!["flac".to_owned(), "audio/flac".to_owned(), "flac".to_owned()],
        ];
        let mut modal = InfoListModal::builder()
            .title("Decoder plugins")
            .column_widths(&[10, 45, 45])
            .header(vec!["Plugin".to_owned(), "MIME types".to_owned(), "Suffixes".to_owned()])
            .rows(rows)
            .build();
        let out = render(&mut modal, &mut ctx);
        assert!(out.contains("Decoder plugins"), "title renders");
        assert!(out.contains("Plugin"), "custom header renders");
        assert!(out.contains("MIME types"), "custom header renders");
        assert!(out.contains("Suffixes"), "custom header renders");
        assert!(out.contains("audio/mpeg"), "rows render");
    }

    /// Long cells wrap into multiple table rows (the zip_longest shape).
    #[test]
    fn long_cells_wrap_into_multiple_rows() {
        let (mut ctx, _rx) = test_ctx();
        let rows =
            vec![vec!["Key".to_owned(), "a very long value that wraps across lines".to_owned()]];
        let mut modal = InfoListModal::builder()
            .title("Song info")
            .column_widths(&[30, 70])
            .rows(rows)
            .build();
        let out = render(&mut modal, &mut ctx);
        assert!(out.contains("Key"), "first row renders after wrapping");
        assert!(out.contains("wraps"), "wrapped continuation renders");
    }

    /// Esc closes the modal (PopModal with the modal's id).
    #[test]
    fn close_hides_the_modal() {
        let (mut ctx, app_rx) = test_ctx();
        let mut modal = InfoListModal::builder()
            .title("Song info")
            .column_widths(&[30, 70])
            .rows(vec![vec!["File".to_owned(), "a.mp3".to_owned()]])
            .build();
        let mut action = ActionEvent::from(Arc::new(vec![Actions::Common(CommonAction::Close)]));
        modal.handle_key(&mut action, &mut ctx).unwrap();
        assert!(
            app_rx.iter().any(|ev| matches!(
                ev,
                crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(id)) if id == modal.id()
            )),
            "Esc must close the info modal"
        );
    }
}
