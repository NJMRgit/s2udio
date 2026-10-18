use std::collections::HashSet;

use anyhow::Result;
use ratatui::{
    Frame,
    layout::Flex,
    prelude::{Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Row},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{Areas, QueuePane};
use crate::{
    config::theme::{AlbumSeparator, properties::{Property, SongProperty}},
    ctx::Ctx,
    mpd::commands::Song,
    shared::{ext::rect::RectExt, song_ext::SongsExt},
    ui::widgets::virtualized_table::VirtualizedTable,
};

/// Round 95: the spinner the queue's Duration column shows for a stream
/// whose duration is not known yet while its parse is in flight — the same
/// braille frames (and step) as the Jellyfin loading indicator, one cell
/// wide, so a spinning cell never shifts the column.
pub(crate) const SPINNER_FRAMES: [&str; 10] =
    ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub(crate) const SPINNER_FRAME_MS: u64 = 120;

/// The frame to draw right now (wall clock, so the animation is shared by
/// every spinning row and survives a missing tick). Also used by the info
/// box's `[YouTube] Resolving` row (round 111), so a pending resolve reads
/// as in-progress there too.
pub(crate) fn spinner_frame() -> &'static str {
    let step = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
        / SPINNER_FRAME_MS) as usize;
    SPINNER_FRAMES[step % SPINNER_FRAMES.len()]
}

impl QueuePane {
    /// The `● Audio ○ Video ○ Chapters` toggle, drawn on the row above the
    /// box that contains the queue list (the config reserves a 1-row spacer
    /// there). TabScreen calls this after the box block renders; the active
    /// tab gets the filled dot and clicking a segment switches the mode (`c`
    /// keybind cycles). The ●/○ glyphs are both single-width so the row
    /// never shifts between modes. Audio and Video always show; Chapters
    /// appears when the current track has markers.
    /// The `● Audio ⭘ Video ⭘ Radio` (＋ `Chapters` when the track has
    /// markers) toggle row. Round 62 (Q3) moved it to the TOP of the queue
    /// page, directly under the navbar — the layout reserves a 1-row strip
    /// there and this paints the row onto it (`tab_area` is the Queue
    /// tab's content area, so its top row is the strip). Clicking a
    /// segment switches the mode (`c` keybind cycles the same order).
    pub(crate) fn render_toggle_on_border(
        &mut self,
        frame: &mut Frame,
        tab_area: Rect,
        ctx: &Ctx,
    ) {
        self.toggle_areas = [Rect::default(); 4];
        if tab_area.height == 0 {
            return;
        }
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Chapters
            && !Self::chapters_available(ctx)
        {
            ctx.queue_tab.set(crate::ctx::QueueTabMode::Audio);
        }
        let active = ctx.queue_tab.get();
        let chapters_visible = Self::chapters_available(ctx);
        // Audio and Video always show; Chapters is conditional on the
        // track having markers; Radio always shows (round 62 Q2).
        //
        // Round 74 (74-2): the painted order IS the toggle_areas index order
        // the click handler (see `handle_mouse_event`) and the `c` cycle
        // use, so Chapters has to sit BEFORE Radio. It used to be appended
        // after Radio, which put the "Chapters" label in the fourth click
        // area and the "Radio" label in the third: clicking Radio selected
        // Chapters and clicking Chapters opened Radio (the labels and the
        // hit areas were off by one whenever the track had markers).
        let mut segments = vec![
            crate ::ui::widgets::sub_tab_bar::Segment { label : "Audio", active : active
            == crate ::ctx::QueueTabMode::Audio, }, crate
            ::ui::widgets::sub_tab_bar::Segment { label : "Video", active : active ==
            crate ::ctx::QueueTabMode::Video, },
        ];
        if chapters_visible {
            segments
                .push(crate::ui::widgets::sub_tab_bar::Segment {
                    label: "Chapters",
                    active: active == crate::ctx::QueueTabMode::Chapters,
                });
        }
        segments.push(crate::ui::widgets::sub_tab_bar::Segment {
            label: "Radio",
            active: active == crate::ctx::QueueTabMode::Radio,
        });
        let right = tab_area.right().saturating_sub(1);
        let bar = crate::ui::widgets::sub_tab_bar::SubTabBar::new(
            &segments,
            tab_area.x.saturating_add(1),
            tab_area.y,
            right,
        );
        for (idx, area) in bar.render(frame, ctx).into_iter().enumerate() {
            if idx < self.toggle_areas.len() {
                self.toggle_areas[idx] = area;
            }
        }
    }

    /// Round 95: arm (and re-arm) the redraw tick that animates the
    /// Duration column's spinner, the same way the Jellyfin indicator does.
    fn schedule_spinner_tick(&mut self, ctx: &Ctx) {
        let id = self.loading_tick.unwrap_or_else(|| {
            let id = crate::shared::id::new();
            self.loading_tick = Some(id);
            id
        });
        ctx.scheduler.schedule_replace(
            id,
            std::time::Duration::from_millis(SPINNER_FRAME_MS),
            move |(tx, _)| {
                tx.send(crate::AppEvent::RequestRender)?;
                Ok(())
            },
        );
    }

    /// Round 95: stop the tick once no row is parsing any more.
    fn cancel_spinner_tick(&mut self, ctx: &Ctx) {
        if let Some(id) = self.loading_tick.take() {
            ctx.scheduler.cancel(id);
        }
    }
}

impl QueuePane {
    pub(super) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &Ctx,
    ) -> anyhow::Result<()> {
        let Ctx { config, .. } = ctx;
        self.calculate_areas(area, ctx)?;
        match ctx.queue_tab.get() {
            crate::ctx::QueueTabMode::Video => return self.render_video(frame, ctx),
            crate::ctx::QueueTabMode::Chapters if Self::chapters_available(ctx) => {
                return self.render_chapters(frame, ctx);
            }
            // Round 62 (Q2): in Radio mode the queue pane is hidden by the
            // mode-aware layout (the radio browser renders in its place);
            // this arm is defensive in case it is ever rendered anyway.
            crate::ctx::QueueTabMode::Radio => return Ok(()),
            _ => {}
        }
        let filter_text = self.queue.filter_text(self.areas[Areas::Table].width, ctx);
        let table_block = {
            let border_style = config.as_border_style();
            let mut b = Block::default().border_style(border_style);
            if self.areas[Areas::FilterArea].height == 0
                && let Some(ref title) = filter_text
            {
                b = b.title(title.clone());
            }
            b
        };
        self.queue
            .state
            .set_content_and_viewport_len(
                self.queue.len(),
                self.areas[Areas::Table].height as usize,
            );
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            self.areas[Areas::Table],
            self.queue.state.offset(),
            self.queue.len(),
            1,
        );
        let row_highlight = if hover_idx == self.queue.state.get_selected() {
            config.theme.hovered_item_style
        } else {
            config.theme.current_item_style
        };
        let widths = Layout::horizontal(self.column_widths.as_slice())
            .flex(Flex::Start)
            .spacing(1)
            .split(self.areas[Areas::Table]);
        let formats = &config.theme.song_table_format;
        let new_album_indices: HashSet<usize> = self
            .queue
            .items
            .as_slice()
            .to_album_ranges()
            .map(|range| range.end.saturating_sub(1))
            .collect();
        let current_song_id = ctx.find_current_song_in_queue().map(|(_, song)| song.id);
        let marked = std::mem::take(self.queue.marked_mut());
        let filter = ctx.input.value(self.queue.filter_buffer_id);
        // Round 95: the spinner in a Duration cell needs a redraw tick; one
        // is armed while any stream resolve is in flight and dropped as soon
        // as none is. Round 107: the play-marker column's spinner uses the
        // same tick, so a pending entry resolve arms it too.
        let spinning_any = !ctx.pending_yt_links.borrow().is_empty()
            || !ctx.pending_stream_resolve.borrow().is_empty();
        if spinning_any {
            self.schedule_spinner_tick(ctx);
        } else {
            self.cancel_spinner_tick(ctx);
        }
        let table = VirtualizedTable::new(&self.queue.items)
            .column_widths(self.column_widths.clone())
            .row_highlight_style(row_highlight)
            .map_fn(|idx, song| {
                let is_current = current_song_id.is_some_and(|v| v == song.id);
                let is_marked = marked.contains(&idx);
                let is_hovered = hover_idx == Some(idx);
                // Round 95: a stream row is a link (`watch?v=ID#s2u-audio`)
                // until it is played, so the overlay matches the row by its
                // normalized link and reads the resolved info first, the
                // flat playlist listing second. A stream row with no info
                // yet still renders through the overlay: its Album cell
                // stays empty and its Duration cell can spin while the
                // stream is parsed.
                let stream_row = crate::ui::modals::paste::is_stream_row(ctx, &song.file);
                let row_meta = stream_row
                    .then(|| crate::ui::modals::paste::stream_row_meta(ctx, &song.file))
                    .flatten();
                let spinning = stream_row
                    && song.duration.is_none()
                    && row_meta.as_ref().and_then(|meta| meta.duration).is_none()
                    && crate::ui::modals::paste::stream_parse_pending(ctx, &song.file);
                // Round 107: the row the user just started (or the row the
                // look ahead is warming) shows a spinner in the play-marker
                // column until playback begins, when `❯` takes its place.
                // Gated to the current or selected row so a warmed next row
                // does not spin in the play column unasked.
                let resolving = (is_current
                    || ctx.queue_selected_id.get() == Some(song.id))
                    && crate::ui::modals::paste::stream_resolve_pending_for(
                        ctx,
                        &song.file,
                        song.id,
                    );
                let columns = (0..formats.len())
                    .map(|i| {
                        let mut max_len: usize = widths[i].width.into();
                        let marker = if i == 0 && resolving {
                            max_len = max_len.saturating_sub(2);
                            Some(Span::styled(
                                format!("{} ", spinner_frame()),
                                Style::default(),
                            ))
                        } else if i == 0 && is_current {
                            max_len = max_len.saturating_sub(2);
                            Some(Span::styled("❯ ", Style::default()))
                        } else {
                            None
                        };
                        let mut line = if stream_row {
                            stream_column_line(
                                    &formats[i].prop,
                                    row_meta.as_ref(),
                                    song,
                                    spinning,
                                    max_len,
                                    &config.theme.symbols,
                                )
                                .unwrap_or_else(|| {
                                    song.as_line_ellipsized(
                                            &formats[i].prop,
                                            max_len,
                                            &config.theme.symbols,
                                            &config.theme.format_tag_separator,
                                            config.theme.multiple_tag_resolution_strategy,
                                            ctx,
                                        )
                                        .unwrap_or_default()
                                })
                        } else {
                            song.as_line_ellipsized(
                                    &formats[i].prop,
                                    max_len,
                                    &config.theme.symbols,
                                    &config.theme.format_tag_separator,
                                    config.theme.multiple_tag_resolution_strategy,
                                    ctx,
                                )
                                .unwrap_or_default()
                        };
                        if let Some(marker) = marker {
                            let mut spans = Vec::with_capacity(line.spans.len() + 1);
                            spans.push(marker);
                            spans.extend(line.spans);
                            line = Line::from(spans);
                        }
                        line.alignment(formats[i].alignment.into())
                    });
                let is_matching_search = is_current
                    || if self.queue.filter_active {
                        song.matches(self.column_formats.as_slice(), &filter, ctx)
                    } else {
                        Default::default()
                    };
                let mut row = QueueRow::default();
                if is_marked {
                    row.cell_style = Some(config.theme.marked_item_style);
                } else if is_hovered {
                    row.cell_style = Some(config.theme.hovered_item_style);
                } else if is_matching_search {
                    row.cell_style = Some(config.theme.highlighted_item_style);
                }
                let sep = ctx.config.theme.song_table_album_separator;
                if new_album_indices.contains(&idx)
                    && matches!(sep, AlbumSeparator::Underline)
                    && idx != self.queue.items.len().saturating_sub(1)
                {
                    row.underlined = true;
                }
                row.into_row(columns)
            });
        frame.render_widget(table_block, self.areas[Areas::Table]);
        frame
            .render_stateful_widget(
                table,
                self.areas[Areas::Table],
                &mut self.queue.state,
            );
        let _ = std::mem::replace(self.queue.marked_mut(), marked);
        let selected_id = self.queue.selected().map(|s| s.id);
        // Round 111: the render pass is the one place every selection route
        // funnels through, so the selection-time metadata warm is driven from
        // here — it used to hang off `SongListCore::fetch_data_internal`,
        // which the Queue pane never reaches (`w`/`s` are
        // `DirectoriesActions::FolderUp/FolderDown`, claimed by this pane's own
        // handler, and the mouse paths call `select_idx` directly), so a merely
        // selected stream row never had its description or artwork resolved.
        // Only a selection that actually moved reschedules the debounce.
        if selection_moved(self.warmed_selection_id, selected_id) {
            self.warmed_selection_id = selected_id;
            super::schedule_selection_warm(ctx);
        }
        ctx.queue_selected_id.set(selected_id);
        if let Some(scrollbar) = config.as_styled_scrollbar()
            && self.areas[Areas::Scrollbar].width > 0
        {
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                self.areas[Areas::Scrollbar],
                self.queue.state.as_scrollbar_state_ref(),
            );
        }
        if let Some(filter_text) = filter_text
            && self.areas[Areas::FilterArea].height > 0
        {
            frame
                .render_widget(
                    Line::from(filter_text)
                        .style(
                            config
                                .theme
                                .text_color
                                .map(|c| Style::default().fg(c))
                                .unwrap_or_default(),
                        ),
                    self.areas[Areas::FilterArea],
                );
        }
        Ok(())
    }

    pub(super) fn calculate_areas(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        let Ctx { config, .. } = ctx;
        let (table_area, scrollbar_area) = if config.theme.scrollbar.is_some()
            && area.width > 4
        {
            crate::ui::scrollbar_strip(area)
        } else {
            (area, Rect::default())
        };
        let mut table_area = if self.queue.filter_active {
            self.areas[Areas::FilterArea] = Rect::new(
                table_area.x,
                table_area.y,
                table_area.width,
                1,
            );
            table_area.shrink_from_top(1)
        } else {
            self.areas[Areas::FilterArea] = Rect::default();
            table_area
        };
        table_area.width = table_area.width.saturating_sub(1);
        self.areas[Areas::Table] = table_area;
        self.areas[Areas::Scrollbar] = scrollbar_area;
        ctx.queue_table_width.set(Some(table_area.width));
        Ok(())
    }
}

/// Truncate `s` so its display width fits `max_cols`, keeping whole
/// graphemes (wide glyphs take two columns, so a grapheme count is not
/// enough to keep the following columns in place).
pub(super) fn truncate_to_width(s: &mut String, max_cols: usize) {
    if s.width() <= max_cols {
        return;
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in s.graphemes(true) {
        let w = grapheme.width();
        if used + w > max_cols {
            break;
        }
        out.push_str(grapheme);
        used += w;
    }
    *s = out;
}

/// The queue-table cell of a YouTube-style stream row for the Title / Album
/// / Artist / Duration columns: the resolved stream info or the flat
/// playlist listing (both via `stream_row_meta`) — title in Title, channel
/// in Artist, the duration in Duration. Ellipsized to the column width.
/// `None` for every other column, and for a column the row has no value for
/// (the caller then renders the row's own property, i.e. the column default).
///
/// Round 95 (user): the Album cell of such a row is **empty** — the album
/// only ever repeated the title. The Duration cell shows the spinning frame
/// while the row's duration is still unknown and its parse is in flight.
fn stream_column_line(
    prop: &Property<SongProperty>,
    meta: Option<&crate::shared::ytdlp::YtListMeta>,
    song: &Song,
    spinning: bool,
    max_len: usize,
    symbols: &crate::config::theme::SymbolsConfig,
) -> Option<Line<'static>> {
    use crate::config::theme::properties::{PropertyKindOrText, SongProperty};
    let text = match &prop.kind {
        PropertyKindOrText::Property(SongProperty::Title) => {
            let title = meta.map(|meta| meta.title.clone()).unwrap_or_default();
            if title.is_empty() {
                return None;
            }
            title
        }
        // Round 95 (user): never repeat the title as the album of a
        // YouTube-style stream — the cell stays empty.
        PropertyKindOrText::Property(SongProperty::Album) => String::new(),
        PropertyKindOrText::Property(SongProperty::Artist) => {
            let channel = meta
                .and_then(|meta| meta.channel.clone())
                .unwrap_or_default();
            if channel.is_empty() {
                return None;
            }
            channel
        }
        PropertyKindOrText::Property(SongProperty::Duration) => {
            // MPD knows the length of a local file (and of a stream it has
            // opened) — that always wins.
            if song.duration.is_some() {
                return None;
            }
            match meta.and_then(|meta| meta.duration) {
                // The same text a local file's Duration cell shows
                // (`Song::duration`), so the column reads uniformly:
                // `m:ss`, and `h:mm:ss` past the hour.
                Some(seconds) => {
                    use crate::shared::ext::duration::DurationExt;
                    std::time::Duration::from_secs(seconds.max(0.0).round() as u64)
                        .to_string()
                }
                None if spinning => spinner_frame().to_owned(),
                None => return None,
            }
        }
        _ => return None,
    };
    let mut text = text;
    if text.width() > max_len {
        let mut out = String::new();
        let mut used = 0;
        let budget = max_len.saturating_sub(symbols.ellipsis.width());
        for grapheme in text.graphemes(true) {
            let w = grapheme.width();
            if used + w > budget {
                break;
            }
            out.push_str(grapheme);
            used += w;
        }
        out.push_str(&symbols.ellipsis);
        text = out;
    }
    Some(Line::from(Span::styled(text, prop.style.unwrap_or_default())))
}

#[derive(Default)]
struct QueueRow {
    cell_style: Option<Style>,
    underlined: bool,
}
impl QueueRow {
    fn into_row<'a>(self, cells: impl Iterator<Item = Line<'a>>) -> Row<'a> {
        let mut row = if let Some(style) = self.cell_style {
            Row::new(cells.map(|column| column.patch_style(style))).style(style)
        } else {
            Row::new(cells)
        };
        if self.underlined {
            row = row.style(self.cell_style.unwrap_or_default().underlined());
        }
        row
    }
}

/// Round 111: whether the queue selection actually moved since the warm was
/// last scheduled. Only a real move restarts the debounce, so a render that
/// merely repaints (a spinner tick, a resize, a mouse hover) does not keep
/// pushing the warm back and no stream row can starve it.
///
/// A move **to** `None` (the queue was cleared) is not a move: there is
/// nothing to resolve, so it just records the new state.
fn selection_moved(previous: Option<u32>, current: Option<u32>) -> bool {
    current.is_some() && previous != current
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod hover_render_tests {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

use super::{Areas, QueuePane};
    use crate::tests::fixtures::ctx as fixture_ctx;

    fn queue_ctx() -> (crate::ctx::Ctx, crossbeam::channel::Sender<crate::shared::events::AppEvent>)
    {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let ctx = fixture_ctx(
            (app_tx.clone(), _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        (ctx, app_tx)
    }

    fn entries(n: usize) -> Vec<crate::core::mpv::MpvPlaylistEntry> {
        (0..n)
            .map(|i| {
                crate::core::mpv::MpvPlaylistEntry::new(format!("v{i}"), format!("url{i}"), None)
            })
            .collect()
    }

    fn video_pane(ctx: &mut crate::ctx::Ctx) -> QueuePane {
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Video);
        let mut pane = QueuePane::new(ctx);
        pane.video_items_len = ctx.video_playlist.borrow().len();
        pane
    }

    /// Render the pane once and return the table area (so the row maths
    /// below line up with the drawn rows).
    fn render(pane: &mut QueuePane, ctx: &crate::ctx::Ctx) -> Rect {
        let backend = TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 24), ctx).unwrap()).unwrap();
        pane.areas[Areas::Table]
    }

    /// The text actually drawn on table row `row` — the cell contents, not the
    /// inputs that produced them. Used to assert on the rendered Duration cell
    /// rather than on the helper behind it: a test on the helper alone would
    /// still pass if the row stopped calling it.
    fn row_text(
        pane: &mut QueuePane,
        ctx: &crate::ctx::Ctx,
        table: Rect,
        row: u16,
    ) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 24), ctx).unwrap()).unwrap();
        let buf = terminal.backend().buffer();
        (table.x..table.x + table.width)
            .map(|x| buf[(x, table.y + row)].symbol().to_owned())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    /// The reported bug at the surface the user saw: a queued YouTube video
    /// whose length is known only from the resolve cache rendered `-` in its
    /// Duration cell. The row must show the real length, formatted like every
    /// other duration in the app.
    #[test]
    fn a_resolved_stream_row_renders_its_real_duration() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.yt_info.borrow_mut().insert(
            "https://www.youtube.com/watch?v=abc123".to_string(),
            crate::shared::ytdlp::YtStreamInfo {
                title: "A video".to_string(),
                duration: Some(185.0),
                ..Default::default()
            },
        );
        ctx.video_playlist
            .borrow_mut()
            .push(crate::core::mpv::MpvPlaylistEntry::new(
                "A video",
                "https://www.youtube.com/watch?v=abc123",
                None,
            ));
        let mut pane = video_pane(&mut ctx);
        let table = render(&mut pane, &ctx);

        let drawn = row_text(&mut pane, &ctx, table, 0);
        assert!(
            drawn.ends_with("3:05"),
            "the Duration cell shows the resolved length (185 s), not `-`: {drawn:?}"
        );
    }

    /// The converse, so the fix cannot be "print a duration for every row": a
    /// stream nothing has resolved yet still renders `-` rather than a
    /// fabricated number.
    #[test]
    fn an_unresolved_stream_row_still_renders_a_dash() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist
            .borrow_mut()
            .push(crate::core::mpv::MpvPlaylistEntry::new(
                "A video",
                "https://www.youtube.com/watch?v=abc123",
                None,
            ));
        let mut pane = video_pane(&mut ctx);
        let table = render(&mut pane, &ctx);

        let drawn = row_text(&mut pane, &ctx, table, 0);
        assert!(
            drawn.ends_with('-'),
            "an unresolved stream keeps the honest `-`: {drawn:?}"
        );
    }

    fn row_bg(
        pane: &mut QueuePane,
        ctx: &crate::ctx::Ctx,
        table: Rect,
        row: u16,
    ) -> Option<ratatui::style::Color> {
        let backend = TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 24), ctx).unwrap()).unwrap();
        let buf = terminal.backend().buffer();
        buf[(table.x + 1, table.y + row)].style().bg
    }

    #[test]
    fn video_row_under_mouse_gets_the_hover_highlight() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);
        let table = render(&mut pane, &ctx);

        // No mouse: the row background is the plain list text (a Reset bg).
        assert_eq!(row_bg(&mut pane, &ctx, table, 2), Some(ratatui::style::Color::Reset));

        // Point the mouse at row 2: the row gets the hover highlight.
        ctx.set_mouse_pos(Some(ratatui::layout::Position { x: table.x + 1, y: table.y + 2 }));
        let hovered = ctx.config.theme.hovered_item_style.bg;
        assert_eq!(row_bg(&mut pane, &ctx, table, 2), hovered);

        // The other rows stay plain.
        assert_eq!(row_bg(&mut pane, &ctx, table, 3), Some(ratatui::style::Color::Reset));
        ctx.set_mouse_pos(None);
    }

    #[test]
    fn hovering_the_selected_video_row_uses_the_hover_highlight() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);

        // Select row 2 (the keyboard cursor); hovering it must still show
        // the hover highlight (brighter than the plain selection).
        let table = render(&mut pane, &ctx);
        pane.video_state.select(Some(2));
        ctx.set_mouse_pos(Some(ratatui::layout::Position { x: table.x + 1, y: table.y + 2 }));
        let hovered = ctx.config.theme.hovered_item_style.bg;
        assert_eq!(
            row_bg(&mut pane, &ctx, table, 2),
            hovered,
            "hover wins over the selection highlight on the selected row"
        );
        ctx.set_mouse_pos(None);
    }

    #[test]
    fn a_selection_move_schedules_the_warm() {
        // The regression: `w`/`s` are `DirectoriesActions::FolderUp/FolderDown`
        // (claimed by the Queue pane), so `fetch_data_internal` never ran and
        // the selection-time metadata warm never fired on this tab. The render
        // pass now drives it, on a real selection move only.
        use super::selection_moved;
        // First render of a selected row: nothing warmed yet.
        assert!(selection_moved(None, Some(7)));
        // A different row: warm the new one.
        assert!(selection_moved(Some(7), Some(8)));
        // The same row repainting (spinner tick, resize, hover): no reschedule,
        // so the 400 ms debounce is never pushed back and cannot starve.
        assert!(!selection_moved(Some(7), Some(7)));
        // A cleared queue has nothing to resolve.
        assert!(!selection_moved(Some(7), None));
        assert!(!selection_moved(None, None));
    }
}
