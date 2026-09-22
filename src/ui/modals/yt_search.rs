//! Round 96: the Queue tab's YouTube search popup (`Shift+S`).
//!
//! Ported from rmpv's `S`-bound `search_yt.sh` (a rofi prompt over
//! `yt-dlp --flat-playlist ytsearch<10>`), but native: a centered
//! rectangular popup that grows as results arrive, in s2udio's own design
//! language — the round-60c search frame (input row, connected
//! `├───…───┤` divider, list, key hints on the bottom border), the
//! round-60 A6 scrollbar strip and the shared hover/selection styles.
//!
//! The popup asks one provider (YouTube, SoundCloud, and NicoVideo when the
//! Settings toggle enables it) for [`SEARCH_LIMIT`] flat search results and
//! does nothing with them until the user picks one: `Enter` plays it as an
//! audio stream now (the round-91 tagged `#s2u-audio` link path), `a` appends
//! it to the MPD queue, `v` plays it as video in mpv.

use anyhow::Result;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::Modal;
use crate::{
    config::keys::{CommonAction, GlobalAction},
    ctx::Ctx,
    shared::{
        events::WorkRequest,
        id::{self, Id},
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
        ytdlp::{YtDlpHost, YtDlpSearchItem},
    },
    ui::{
        UiEvent,
        dirstack::DirState,
        input::{BufferId, InputResultEvent},
    },
};

/// How many results one search asks the provider for. rmpv asks for 10; 25
/// fills the popup without making the single flat yt-dlp call noticeably
/// slower.
pub(crate) const SEARCH_LIMIT: usize = 25;
/// The input's empty-state hint (round 100, user feedback): it lives in the
/// input row itself and is gone as soon as the first character is typed.
const INPUT_HINT: &str = "Type a query and press Enter";
/// The most result rows the popup grows to before the list scrolls.
const MAX_VISIBLE_ROWS: u16 = 15;
/// Popup width bounds: about two thirds of the terminal, clamped. The lower
/// bound still fits the longest bottom-border hint.
const MIN_WIDTH: u16 = 46;
const MAX_WIDTH: u16 = 96;

/// What the popup is showing under its input row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Nothing asked yet: no message row at all — the empty input carries the
    /// hint (round 100, user feedback).
    Idle,
    /// A request is in flight.
    Searching,
    /// A list of results is on screen.
    Results,
    /// The last search returned nothing.
    Empty,
    /// yt-dlp failed; its message is shown in place.
    Failed(String),
}

/// What picking a result does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    /// `Enter` / `d`: queue the stream as an audio link and start it.
    PlayAudio,
    /// `a`: queue the stream as an audio link, without touching playback.
    QueueAudio,
    /// `v`: hand the link to mpv as a video.
    PlayVideo,
}

#[derive(Debug)]
pub struct YtSearchModal {
    id: Id,
    input_buffer_id: BufferId,
    /// The providers Tab cycles through: YouTube and SoundCloud.
    providers: Vec<YtDlpHost>,
    provider_idx: usize,
    results: Vec<YtDlpSearchItem>,
    /// The query whose reply the popup is waiting for (globally unique, so a
    /// slow answer to an older query is dropped instead of replacing a newer
    /// list).
    pending_request: Option<u64>,
    phase: Phase,
    /// The query the shown results (or the "no results" line) belong to.
    last_query: String,
    /// Round 96 (user feedback): the source is chosen *before* searching. Once
    /// a search has been sent, `Tab` is inert and drops out of the hints until
    /// the popup is reopened — so a result list always belongs to the source
    /// named on the top border.
    source_locked: bool,
    /// The configured duration format, captured when the popup opens (the
    /// result rows' right-aligned column, like the queue's).
    duration_format: crate::shared::duration_format::DurationFormat,
    state: DirState<ListState>,
    input_area: Rect,
    list_area: Rect,
    hints_area: Rect,
    scrollbar_area: Rect,
}

impl YtSearchModal {
    pub fn new(ctx: &Ctx) -> Self {
        let input_buffer_id = BufferId::new();
        ctx.input.insert_mode(input_buffer_id);
        // YouTube ⇄ SoundCloud. NicoVideo is not offered: yt-dlp's
        // `nicosearch` extractor returned no entries for any query tried, so
        // the popup did not earn a third stop (round 96 feedback). The host
        // itself stays supported for pasted NicoVideo links.
        let providers = vec![YtDlpHost::Youtube, YtDlpHost::Soundcloud];
        Self {
            id: id::new(),
            input_buffer_id,
            providers,
            provider_idx: 0,
            results: Vec::new(),
            pending_request: None,
            phase: Phase::Idle,
            last_query: String::new(),
            source_locked: false,
            duration_format: ctx.config.duration_format.clone(),
            state: DirState::default(),
            input_area: Rect::default(),
            list_area: Rect::default(),
            hints_area: Rect::default(),
            scrollbar_area: Rect::default(),
        }
    }

    fn provider(&self) -> YtDlpHost {
        self.providers.get(self.provider_idx).copied().unwrap_or_default()
    }

    /// Run the query currently in the input buffer, if it is not empty.
    fn search(&mut self, ctx: &Ctx) -> Result<()> {
        let query = ctx.input.value(self.input_buffer_id).trim().to_owned();
        if query.is_empty() {
            self.phase = Phase::Idle;
            ctx.render()?;
            return Ok(());
        }
        let request_id = *id::new() as u64;
        self.pending_request = Some(request_id);
        self.phase = Phase::Searching;
        self.last_query = query.clone();
        // The source was chosen before this query went out; it stays fixed so a
        // result list can never be silently relabelled under another provider.
        self.source_locked = true;
        self.results.clear();
        self.state.select(None, 0);
        if let Err(err) = ctx.work_sender.send(WorkRequest::SearchYtModal {
            request_id,
            query,
            kind: self.provider(),
            limit: SEARCH_LIMIT,
        }) {
            self.pending_request = None;
            self.phase = Phase::Failed(format!("Failed to request the search: {err}"));
        }
        ctx.render()?;
        Ok(())
    }

    /// Cycle the provider (Tab), while the source is still open to change.
    fn cycle_provider(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.source_locked && self.providers.len() > 1 {
            self.provider_idx = (self.provider_idx + 1) % self.providers.len();
        }
        ctx.render()?;
        Ok(())
    }

    /// Pick the highlighted result.
    fn activate(&mut self, action: RowAction, ctx: &Ctx) -> Result<()> {
        let Some(idx) = self.state.get_selected() else {
            return Ok(());
        };
        let Some(item) = self.results.get(idx) else {
            return Ok(());
        };
        let link = item.url.clone();
        let title = item
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| link.clone());
        match action {
            RowAction::PlayAudio | RowAction::QueueAudio => {
                // Round 91/95: the entry goes into the queue as the tagged
                // link and resolves when it is played — the same shape a
                // pasted YouTube link or a stored playlist entry takes.
                crate::ui::modals::paste::queue_search_result(
                    ctx,
                    &link,
                    &title,
                    matches!(action, RowAction::PlayAudio),
                );
            }
            RowAction::PlayVideo => {
                let mut entry =
                    crate::core::mpv::MpvPlaylistEntry::new(title, link.clone(), item.duration);
                entry.original_url = Some(link);
                crate::core::mpv::play_video_entries(ctx, vec![entry]);
            }
        }
        self.close(ctx)?;
        Ok(())
    }

    /// Leave insert mode and drop the popup.
    fn close(&mut self, ctx: &Ctx) -> Result<()> {
        ctx.input.destroy_buffer(self.input_buffer_id);
        self.hide(ctx)
    }

    /// Focus the query input again.
    fn focus_input(&mut self, ctx: &Ctx) -> Result<()> {
        ctx.input.insert_mode(self.input_buffer_id);
        ctx.render()?;
        Ok(())
    }

    /// Move the highlight one row, following the configured wrap rule like
    /// every other list.
    fn move_selection(&mut self, delta: i64, ctx: &Ctx) -> Result<()> {
        if self.results.is_empty() {
            return Ok(());
        }
        let wrap = ctx.config.wrap_navigation;
        if delta < 0 {
            self.state.prev(ctx.config.scrolloff, wrap);
        } else {
            self.state.next(ctx.config.scrolloff, wrap);
        }
        ctx.render()?;
        Ok(())
    }

    fn page(&mut self, up: bool, ctx: &Ctx) -> Result<()> {
        if self.results.is_empty() {
            return Ok(());
        }
        if up {
            self.state.prev_half_viewport(ctx.config.scrolloff);
        } else {
            self.state.next_half_viewport(ctx.config.scrolloff);
        }
        ctx.render()?;
        Ok(())
    }

    /// The popup's height: two borders, the input row, the connected divider
    /// and the hints row, plus one message row while there is a message, plus
    /// one row per result once there are results, capped so the list scrolls
    /// instead of growing past [`MAX_VISIBLE_ROWS`]. The idle popup has no
    /// message row (round 100), so it sits one row shorter than before.
    fn popup_height(&self, screen: Rect) -> u16 {
        // Borders, input, divider, hints.
        const FRAME_ROWS: u16 = 5;
        // The cap leaves room for the frame rows, so the popup always fits on
        // screen.
        let cap = MAX_VISIBLE_ROWS.min(screen.height.saturating_sub(7)).max(1);
        if self.results.is_empty() {
            return FRAME_ROWS + u16::from(self.message().is_some());
        }
        let rows = u16::try_from(self.results.len()).unwrap_or(u16::MAX);
        FRAME_ROWS + rows.clamp(1, cap)
    }

    fn popup_width(&self, screen: Rect) -> u16 {
        (screen.width * 2 / 3)
            .clamp(MIN_WIDTH, MAX_WIDTH)
            .min(screen.width.saturating_sub(2))
            .max(MIN_WIDTH.min(screen.width.saturating_sub(2)))
    }

    /// The one-row message shown instead of the list while there is nothing
    /// to select. `None` while idle (the input's own hint covers that state)
    /// and while results are on screen, so those states have no message row.
    fn message(&self) -> Option<Line<'static>> {
        match &self.phase {
            Phase::Idle | Phase::Results => None,
            Phase::Searching => Some(Line::from("Searching…")),
            Phase::Empty => Some(Line::from(format!("No results for \"{}\"", self.last_query))),
            Phase::Failed(err) => Some(Line::from(format!("Search failed: {err}"))),
        }
    }

    /// The key hints for the footer row: each entry is its **key** in the
    /// list-name style with its action dim, separated by `·` — the same split
    /// the Help popup's footer uses (round 96 feedback: one weight for the
    /// whole row reads as a line of words, not as controls). `Tab source`
    /// disappears once a search has fixed the source, and entries that do not
    /// fit are dropped whole rather than cut in half.
    fn hint_line(&self, width: u16, ctx: &Ctx) -> Line<'static> {
        let mut entries: Vec<(&'static str, &'static str)> =
            vec![("Enter", "play"), ("a", "queue")];
        if !self.results.is_empty() {
            entries.push(("v", "video"));
        }
        if !self.source_locked {
            entries.push(("Tab", "source"));
        }
        entries.push(("Esc", "close"));
        let key_style = ctx.config.as_list_name_style();
        let action_style = ctx.config.as_list_text_style().add_modifier(Modifier::DIM);
        let budget = (width as usize).saturating_sub(4);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for (idx, (key, action)) in entries.iter().enumerate() {
            let sep = if idx == 0 { 0 } else { 3 };
            let entry = format!("{key} {action}");
            if used + sep + UnicodeWidthStr::width(entry.as_str()) > budget {
                break;
            }
            if sep > 0 {
                spans.push(Span::styled(" · ", action_style));
                used += sep;
            }
            used += UnicodeWidthStr::width(entry.as_str());
            spans.push(Span::styled((*key).to_owned(), key_style));
            spans.push(Span::styled(format!(" {action}"), action_style));
        }
        Line::from(spans)
    }

    /// The top-border title: the provider's name and what the popup is.
    fn title(&self) -> String {
        format!(" {} search ", self.provider().search_label())
    }

    fn row_line(&self, item: &YtDlpSearchItem, idx: usize) -> Line<'static> {
        let ctx_width = self.list_area.width as usize;
        let duration = item
            .duration
            .map(|seconds| {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let seconds = seconds.max(0.0) as u64;
                self.duration_format.format(seconds)
            })
            .unwrap_or_else(|| "-".to_owned());
        // Columns: index (right-aligned), title, channel (right-aligned),
        // duration (right-aligned at the edge, like the queue's columns).
        let index = format!("{:>3}. ", idx + 1);
        let duration_w = duration.width().max(4);
        let channel_w = if ctx_width >= 64 { 24 } else { 0 };
        let fixed = index.width() + channel_w + duration_w + usize::from(channel_w > 0) + 1;
        let title_w = ctx_width.saturating_sub(fixed);
        let title = truncate(item.title.as_deref().unwrap_or("<no title>"), title_w);
        let channel = if channel_w == 0 {
            String::new()
        } else {
            let channel = truncate(item.channel.as_deref().unwrap_or(""), channel_w);
            format!("{}{} ", " ".repeat(channel_w.saturating_sub(channel.width())), channel)
        };
        let title_pad = title_w.saturating_sub(title.width());
        let duration_pad = duration_w.saturating_sub(duration.width());
        Line::from(format!(
            "{index}{title}{}{channel}{}{duration}",
            " ".repeat(title_pad),
            " ".repeat(duration_pad),
        ))
    }
}

/// Truncate `text` to `max_cols` display columns, marking a cut with the
/// design language's `…` (display width based, so wide glyphs never shift a
/// column).
fn truncate(text: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if text.width() <= max_cols {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let char_width = ch.width().unwrap_or(0);
        if width + char_width > max_cols.saturating_sub(1) {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.push('…');
    out
}

impl Modal for YtSearchModal {
    fn id(&self) -> Id {
        self.id
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        let screen = frame.area();
        if screen.width < 12 || screen.height < 6 {
            return Ok(());
        }
        let width = self.popup_width(screen);
        let height = self.popup_height(screen);
        let popup = screen.centered(
            ratatui::layout::Constraint::Length(width),
            ratatui::layout::Constraint::Length(height),
        );
        frame.render_widget(Clear, popup);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(
                Block::default().style(Style::default().bg(bg_color)),
                popup,
            );
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(symbols::border::ROUNDED)
            .border_style(ctx.config.as_border_style())
            .title_alignment(Alignment::Center)
            .title(self.title().bold());
        let inner = block.inner(popup);
        self.input_area = Rect { height: 1, ..inner };
        // The block is rendered first so the search frame's `├`/`┤`
        // junctions stick at the border cells (round 60c S1).
        frame.render_widget(block, popup);
        let query = ctx.input.value(self.input_buffer_id);
        let focused = ctx.input.is_active(self.input_buffer_id);
        let content =
            crate::ui::render_search_frame_top(frame, inner, &query, focused, Some(INPUT_HINT), ctx);
        // Round 96 (user feedback): the key hints are their own row at the
        // bottom of the popup, inside the border, with the list above them.
        // The row is reserved whenever there is any content row at all — the
        // idle popup is exactly one content row tall (round 100), and it must
        // be the hints row.
        let (content, hints_area) = if content.height >= 1 {
            (
                Rect { height: content.height - 1, ..content },
                Rect { y: content.bottom() - 1, height: 1, ..content },
            )
        } else {
            (content, Rect::default())
        };
        self.hints_area = hints_area;
        if hints_area.height > 0 {
            // The spans carry their own styles, so no Paragraph-wide style: a
            // row-wide one would flatten the key/action split.
            frame.render_widget(
                Paragraph::new(self.hint_line(width, ctx)).alignment(Alignment::Center),
                hints_area,
            );
        }
        let (list_area, scrollbar_area) = if ctx.config.theme.scrollbar.is_some() {
            crate::ui::scrollbar_strip(content)
        } else {
            (content, Rect::default())
        };
        self.list_area = list_area;
        self.scrollbar_area = scrollbar_area;
        if let Some(message) = self.message() {
            let style = ctx.config.as_list_text_style().add_modifier(Modifier::DIM);
            frame.render_widget(
                Paragraph::new(message).style(style),
                Rect {
                    x: list_area.x + 1,
                    y: list_area.y,
                    width: list_area.width.saturating_sub(1),
                    height: 1,
                },
            );
            return Ok(());
        }
        if self.results.is_empty() {
            // Idle: the input's own hint is the whole empty state.
            return Ok(());
        }
        self.state
            .set_content_and_viewport_len(self.results.len(), list_area.height.into());
        if self.state.get_selected().is_none() {
            self.state.select(Some(0), 0);
        }
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.modal_mouse_pos(),
            list_area,
            self.state.offset(),
            self.results.len(),
            1,
        );
        let selected = self.state.get_selected();
        let items: Vec<ListItem> = self
            .results
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let style = if hover_idx == Some(idx) {
                    ctx.config.theme.hovered_item_style
                } else if selected == Some(idx) {
                    ctx.config.theme.current_item_style
                } else {
                    ctx.config.as_list_name_style()
                };
                ListItem::new(self.row_line(item, idx)).style(style)
            })
            .collect();
        // The row styles above already carry the selection highlight, so the
        // widget's own highlight must not patch anything on top of them.
        let list = List::new(items)
            .style(ctx.config.as_list_name_style())
            .highlight_style(Style::default());
        frame.render_stateful_widget(list, list_area, self.state.as_render_state_ref());
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && self.scrollbar_area.width > 0
        {
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                self.scrollbar_area,
                self.state.as_scrollbar_state_ref(),
            );
        }
        Ok(())
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &Ctx) -> Result<()> {
        match kind {
            InputResultEvent::Confirm => self.search(ctx)?,
            // Esc in the input only leaves it: the platform restores normal
            // mode, so the results stay selectable and a second Esc closes
            // the popup (the search bars' staged-exit shape).
            InputResultEvent::Cancel => {}
            InputResultEvent::Push
            | InputResultEvent::Pop
            | InputResultEvent::NoChange
            | InputResultEvent::AtStart
            | InputResultEvent::CursorLeft => {}
        }
        Ok(())
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        // The common half is claimed FIRST: `Esc` carries `Common(Close)` and
        // `Global(ShowSettings)` at once, so claiming the global half first
        // would swallow the close. `Tab` never gets this far — `handle_raw_key`
        // claims it in both modes.
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::Down => self.move_selection(1, ctx)?,
                CommonAction::Up => self.move_selection(-1, ctx)?,
                CommonAction::PageDown => self.page(false, ctx)?,
                CommonAction::PageUp => self.page(true, ctx)?,
                CommonAction::Confirm => self.activate(RowAction::PlayAudio, ctx)?,
                CommonAction::Close => self.close(ctx)?,
                _ => {}
            }
            return Ok(());
        }
        if let Some(action) = key.claim_global() {
            if matches!(action, GlobalAction::NextTab) {
                self.cycle_provider(ctx)?;
            }
        }
        Ok(())
    }

    fn handle_raw_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        ctx: &mut Ctx,
    ) -> Result<bool> {
        if key.kind == crossterm::event::KeyEventKind::Release || !key.modifiers.is_empty() {
            return Ok(false);
        }
        // Round 96 (user feedback): `Tab` switches the source from **both**
        // modes, so the provider can be picked while the query input still has
        // focus. Once a search has fixed the source it is consumed and does
        // nothing (the resolver's `NextTab` never reaches the popup).
        if key.code == crossterm::event::KeyCode::Tab {
            if !self.source_locked {
                self.cycle_provider(ctx)?;
            }
            return Ok(true);
        }
        // While the query input is focused these letters are text, not
        // actions — the raw hook runs for every key, before the buffer sees
        // it, so it must stand aside (the `ListModal` rule).
        if ctx.input.is_active(self.input_buffer_id) {
            return Ok(false);
        }
        match key.code {
            // `a`, `v` and `d` are unbound in the shipped keymap, so they can
            // only reach the popup raw; they must not fire while the query
            // input is focused (there they are just letters).
            crossterm::event::KeyCode::Char('a') => {
                self.activate(RowAction::QueueAudio, ctx)?;
                Ok(true)
            }
            crossterm::event::KeyCode::Char('v') => {
                self.activate(RowAction::PlayVideo, ctx)?;
                Ok(true)
            }
            crossterm::event::KeyCode::Char('d') => {
                self.activate(RowAction::PlayAudio, ctx)?;
                Ok(true)
            }
            crossterm::event::KeyCode::Char('/')
            | crossterm::event::KeyCode::Char('i') => {
                self.focus_input(ctx)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        let content_len = self
            .state
            .content_len()
            .unwrap_or(self.results.len())
            .saturating_sub(self.state.viewport_len().unwrap_or(0))
            .saturating_add(1)
            .max(1);
        let viewport_len = self
            .state
            .viewport_len()
            .unwrap_or(self.scrollbar_area.height as usize);
        let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
        if let Some(perc) = self.state.scrollbar_drag.handle(
            event,
            self.scrollbar_area,
            content_len,
            viewport_len,
            self.state.offset(),
            begin_len,
            end_len,
        ) {
            self.state.scroll_to(perc, ctx.config.scrolloff);
            ctx.render()?;
            return Ok(());
        }
        match event.kind {
            MouseEventKind::LeftClick => {
                if self.input_area.contains(event.into()) {
                    self.focus_input(ctx)?;
                } else if self.list_area.contains(event.into()) {
                    let row = usize::from(event.y.saturating_sub(self.list_area.y));
                    if let Some(idx) = self.state.get_at_rendered_row(row) {
                        self.state.select(Some(idx), 0);
                        ctx.render()?;
                    }
                }
            }
            MouseEventKind::DoubleClick => {
                if self.list_area.contains(event.into()) {
                    let row = usize::from(event.y.saturating_sub(self.list_area.y));
                    if let Some(idx) = self.state.get_at_rendered_row(row) {
                        self.state.select(Some(idx), 0);
                        self.activate(RowAction::PlayAudio, ctx)?;
                    }
                }
            }
            MouseEventKind::ScrollUp if self.list_area.contains(event.into()) => {
                self.state.prev(ctx.config.scrolloff, true);
                ctx.render()?;
            }
            MouseEventKind::ScrollDown if self.list_area.contains(event.into()) => {
                self.state.next(ctx.config.scrolloff, true);
                ctx.render()?;
            }
            MouseEventKind::MiddleClick
            | MouseEventKind::RightClick
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::Drag { .. }
            | MouseEventKind::LeftRelease
            | MouseEventKind::Moved => {}
        }
        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, ctx: &Ctx) -> Result<()> {
        if let UiEvent::YtSearchResults { request_id, items, error } = event {
            if self.pending_request != Some(*request_id) {
                return Ok(());
            }
            self.pending_request = None;
            self.results = items.clone();
            self.phase = match (error, self.results.is_empty()) {
                (Some(err), _) => Phase::Failed(err.clone()),
                (None, true) => Phase::Empty,
                (None, false) => Phase::Results,
            };
            let selected = (!self.results.is_empty()).then_some(0);
            self.state.select(selected, 0);
            ctx.render()?;
        }
        Ok(())
    }
}
