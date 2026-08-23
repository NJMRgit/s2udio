use std::time::Duration;
use anyhow::Result;
use ratatui::{
    Frame, layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{List, ListItem, ListState, Paragraph, StatefulWidget},
};
use super::Pane;
use crate::{
    config::keys::CommonAction, core::command::{create_env, run_external_blocking},
    ctx::Ctx, mpd::commands::{Song, State},
    shared::{
        events::WorkRequest, ext::duration::DurationExt, keys::ActionEvent,
        lrc::{Lrc, LrcEditSession, get_lrc_path},
        macros::{modal, status_error, status_info},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_query::run_status_update,
    },
    ui::{
        UiEvent, modals::input_modal::InputModal,
        widgets::wrap::{wrap_spans, wrap_to_width},
    },
};
/// A clickable word in the edit-mode lyrics view (screen coords + the
/// edit session's `(line, word)` indices).
#[derive(Debug, Clone, Copy)]
struct WordArea {
    rect: Rect,
    line: usize,
    word: usize,
}
/// One word + its timing, ready to render in edit mode.
struct EditUnit {
    word: String,
    time: String,
    word_w: usize,
    time_w: usize,
}
/// Draws edit-mode rows: words in the lyrics style, timings in the
/// edit-timing style, the selected word (and its timing) highlighted.
struct EditRowDrawer<'frame, 'buf> {
    frame: &'frame mut Frame<'buf>,
    default_style: Style,
    selected_style: Style,
    timing_style: Style,
    selection: Option<(usize, usize)>,
}
impl<'frame, 'buf> EditRowDrawer<'frame, 'buf> {
    /// Place one wrapped row of word units at `row`, recording each
    /// word's hit area.
    fn place(
        &mut self,
        row: Rect,
        units: &[EditUnit],
        row_units: &[usize],
        line_idx: usize,
        word_areas: &mut Vec<WordArea>,
    ) {
        let buf = self.frame.buffer_mut();
        let mut x = row.x;
        for &ui in row_units {
            let Some(u) = units.get(ui) else { continue };
            let sel = self.selection == Some((line_idx, ui));
            let word_style = if sel { self.selected_style } else { self.default_style };
            let time_style = if sel { self.selected_style } else { self.timing_style };
            let word_w = u.word_w as u16;
            let time_w = u.time_w as u16;
            buf.set_stringn(x, row.y, &u.word, u.word_w, word_style);
            buf.set_stringn(x + word_w, row.y, " ", 1, self.default_style);
            buf.set_stringn(x + word_w + 1, row.y, &u.time, u.time_w, time_style);
            buf.set_stringn(x + word_w + 1 + time_w, row.y, " ", 1, self.default_style);
            word_areas
                .push(WordArea {
                    rect: Rect {
                        x,
                        y: row.y,
                        width: word_w + 2 + time_w,
                        height: 1,
                    },
                    line: line_idx,
                    word: ui,
                });
            x += word_w + 2 + time_w;
        }
    }
    /// Place one row of plain (non-word-timed) lyrics text, centered.
    fn place_plain(&mut self, row: Rect, text: &str) {
        let text = Text::from(text.to_owned()).centered().style(self.default_style);
        self.frame.render_widget(text, row);
    }
}
/// Wrap word+timing units into rows of unit indices that fit `width`
/// (units wrap whole: a word is never split from its timing).
fn wrap_edit_units(units: &[EditUnit], width: usize) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut row: Vec<usize> = Vec::new();
    let mut x = 0usize;
    for (i, u) in units.iter().enumerate() {
        let w = u.word_w + 2 + u.time_w;
        if x + w > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            x = 0;
        }
        row.push(i);
        x += w;
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}
/// The wrapped display rows of a plain (non-word-timed) line, with the
/// `[mm:ss.xx]` prefix when the timestamp flag is on (mirrors the normal
/// karaoke view).
fn plain_edit_chunks(
    line: &crate::shared::lrc::EditableLine,
    timestamp: bool,
    width: usize,
) -> Vec<String> {
    let formatted = if timestamp && !line.content.is_empty() {
        format!("[{}] {}", LrcEditSession::format_time(line.time), line.content)
    } else {
        line.content.clone()
    };
    textwrap::wrap(&formatted, width)
        .into_iter()
        .map(|s| s.as_ref().to_owned())
        .collect()
}
#[derive(Debug)]
pub struct LyricsPane {
    current_lyrics: Option<Lrc>,
    initialized: bool,
    last_requested_line_idx: usize,
    /// Scroll state of the track-info view.
    info_state: ListState,
    /// Area of the info view (for mouse scrolling).
    info_area: Rect,
    /// Number of rows in the current info view (for the scroll bounds).
    info_items_len: usize,
    /// Id of the song whose info is currently shown (reset the scroll when
    /// it changes).
    info_song_id: Option<u32>,
    /// The video (item id / title) whose info is currently shown (reset
    /// the scroll when it changes).
    info_video_key: Option<String>,
    /// When the current video's info was first shown (starts the title
    /// marquee's static pause; wall-clock so wheel scrolling the
    /// description never nudges the animation).
    info_video_shown_at: Option<std::time::Instant>,
    /// Area of the info view's scrollbar (for click/drag scrolling).
    info_scrollbar_area: Rect,
    /// Drag state of the info view's scrollbar (thumb follows the pointer).
    info_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag,
    /// Click zone of the `hide lyrics` / `show lyrics` button (screen
    /// coords, from the last lyrics render).
    wrong_btn_area: Rect,
    /// Click zone of the `fetch lyrics` button (screen coords,
    /// from the last lyrics render).
    fetch_btn_area: Rect,
    /// Click zone of the edit-mode pencil button (screen coords, from
    /// the last lyrics render).
    edit_btn_area: Rect,
    /// Edit mode (round 34): the pencil button toggles it; while ON and
    /// paused the lyrics stay visible with editable per-word timings.
    edit_mode: bool,
    /// The editing session over the source `.lrc` (raw text + marker
    /// positions); `None` outside edit mode.
    edit_session: Option<LrcEditSession>,
    /// The selected word as `(line, word)` into the edit session.
    edit_selection: Option<(usize, usize)>,
    /// Round 35/40: the (line, word) a just-inserted lyric or word
    /// should land on; the `LyricsIndexed` reload selects it (set before
    /// the insert modal opens, consumed by `rebuild_edit_session`).
    pending_insert_select: Option<(usize, usize)>,
    /// Screen rects of every rendered word (edit mode), for click
    /// selection.
    word_areas: Vec<WordArea>,
    /// File of the song whose lyrics were marked wrong (hidden) by the
    /// `hide lyrics` button. Per-song, in-session state; `fetch lyrics`
    /// clears it.
    wrong_song_file: Option<String>,
    /// A `fetch lyrics` run is in flight (a second click while running is
    /// ignored; the marker itself is pressed-while-held only, driven by
    /// `pressed_btn`, never by this flag).
    fetching: bool,
    /// The lyric button currently held down (shows the pressed `⭘` marker
    /// while the mouse button is down). Cleared by `LeftRelease` (or the
    /// release-check fallback on terminals without release events).
    pressed_btn: Option<LyricsBtn>,
    /// Id of the release-check one-shot scheduled on press (see
    /// `schedule_release_check`); cancelled on release.
    release_check: Option<crate::shared::id::Id>,
}
/// The lyrics-pane header buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LyricsBtn {
    Wrong,
    Fetch,
    Edit,
}
impl LyricsPane {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            current_lyrics: None,
            initialized: false,
            last_requested_line_idx: usize::MAX,
            info_state: ListState::default(),
            info_area: Rect::default(),
            info_items_len: 0,
            info_song_id: None,
            info_video_key: None,
            info_video_shown_at: None,
            info_scrollbar_area: Rect::default(),
            info_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            wrong_btn_area: Rect::default(),
            fetch_btn_area: Rect::default(),
            edit_btn_area: Rect::default(),
            edit_mode: false,
            edit_session: None,
            edit_selection: None,
            pending_insert_select: None,
            word_areas: Vec::new(),
            wrong_song_file: None,
            fetching: false,
            pressed_btn: None,
            release_check: None,
        }
    }
    fn update_lyrics(&mut self, ctx: &Ctx) -> Result<()> {
        self.current_lyrics = None;
        let lrc = ctx.find_lrc()?;
        let Some((_, lrc)) = lrc else { return Ok(()) };
        self.current_lyrics = Some(lrc);
        Ok(())
    }
    /// Whether the current song's lyrics are marked wrong (hidden).
    fn is_wrong(&self, ctx: &Ctx) -> bool {
        ctx.find_current_song_in_queue()
            .is_some_and(|(_, song)| {
                self.wrong_song_file.as_deref() == Some(song.file.as_str())
            })
    }
    /// The lyrics view frame (round 13 layout): a blank top row, the body
    /// — the lyrics, or the paused-style info panel while the current song
    /// is wrong-marked — a full-width margin line, and the `hide lyrics` /
    /// `show lyrics` + `fetch lyrics` button cluster on the bottom row,
    /// right-aligned one cell in from the right border, styled as one
    /// group (`● hide lyrics | ● fetch lyrics`, `● show lyrics` while the
    /// current song is wrong-marked, collapsing to `● hide | ● fetch` /
    /// `● show | ● fetch` when narrow and hidden entirely when even that
    /// does not fit). Hover applies the queue-list row highlight (bg +
    /// bold + brightening) to the **label text only**; the `●`/`⭘` glyph
    /// keeps its completely normal style. Records the button click zones
    /// for the mouse handler and returns the body area between the blank
    /// top row and the margin line.
    fn render_frame(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Rect {
        use unicode_width::UnicodeWidthStr;
        let body = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(3),
        };
        if area.height < 4 {
            return body;
        }
        let base = ctx.config.as_text_style();
        let buf = frame.buffer_mut();
        let margin_style = ctx.config.as_border_style();
        let width = area.width as usize;
        let bottom_margin_y = area.bottom().saturating_sub(2);
        let buttons_y = area.bottom().saturating_sub(1);
        buf.set_stringn(
            area.x,
            bottom_margin_y,
            "─".repeat(width),
            width,
            margin_style,
        );
        let mouse = ctx.mouse_pos();
        let hovered_style = ctx.config.theme.hovered_item_style;
        let edit_glyph = if self.pressed_btn == Some(LyricsBtn::Edit) {
            "⭘"
        } else if self.edit_mode {
            "✏"
        } else {
            "✎"
        };
        self.edit_btn_area = Rect {
            x: area.x + 1,
            y: buttons_y,
            width: 1,
            height: 1,
        };
        let edit_hovered = mouse.is_some_and(|p| self.edit_btn_area.contains(p));
        buf.set_stringn(
            area.x + 1,
            buttons_y,
            edit_glyph,
            1,
            if edit_hovered {
                crate::config::hover_style(base).patch(hovered_style)
            } else {
                base
            },
        );
        let glyph_of = |pressed: bool| if pressed { "⭘" } else { "●" };
        let hide_show = if self.is_wrong(ctx) { "show" } else { "hide" };
        let full_wrong = format!(
            "{} {hide_show} lyrics", glyph_of(self.pressed_btn == Some(LyricsBtn::Wrong))
        );
        let full_fetch = format!(
            "{} fetch lyrics", glyph_of(self.pressed_btn == Some(LyricsBtn::Fetch))
        );
        let short_wrong = format!(
            "{} {hide_show}", glyph_of(self.pressed_btn == Some(LyricsBtn::Wrong))
        );
        let short_fetch = format!(
            "{} fetch", glyph_of(self.pressed_btn == Some(LyricsBtn::Fetch))
        );
        let (wrong_label, fetch_label) = if full_wrong.width() + 3 + full_fetch.width()
            <= area.width as usize
        {
            (full_wrong, full_fetch)
        } else if short_wrong.width() + 3 + short_fetch.width() <= area.width as usize {
            (short_wrong, short_fetch)
        } else {
            self.wrong_btn_area = Rect::default();
            self.fetch_btn_area = Rect::default();
            return body;
        };
        let wrong_w = wrong_label.width() as u16;
        let fetch_w = fetch_label.width() as u16;
        let cluster_w = wrong_w + 3 + fetch_w;
        let start = area.x + area.width.saturating_sub(cluster_w).saturating_sub(1);
        self.wrong_btn_area = Rect {
            x: start,
            y: buttons_y,
            width: wrong_w,
            height: 1,
        };
        self.fetch_btn_area = Rect {
            x: start + wrong_w + 3,
            y: buttons_y,
            width: fetch_w,
            height: 1,
        };
        let wrong_hovered = mouse.is_some_and(|p| self.wrong_btn_area.contains(p));
        let fetch_hovered = mouse.is_some_and(|p| self.fetch_btn_area.contains(p));
        fn button_line(
            label: &str,
            hovered: bool,
            base: Style,
            hovered_style: Style,
        ) -> Line<'_> {
            let (glyph, text) = match label.find(' ') {
                Some(split) => (&label[..split + 1], &label[split + 1..]),
                None => (label, ""),
            };
            let glyph_style = base;
            let text_style = if hovered {
                crate::config::hover_style(base).patch(hovered_style)
            } else {
                base
            };
            Line::from(
                vec![Span::styled(glyph, glyph_style), Span::styled(text, text_style)],
            )
        }
        buf.set_line(
            self.wrong_btn_area.x,
            buttons_y,
            &button_line(&wrong_label, wrong_hovered, base, hovered_style),
            wrong_w,
        );
        buf.set_line(
            self.fetch_btn_area.x,
            buttons_y,
            &button_line(&fetch_label, fetch_hovered, base, hovered_style),
            fetch_w,
        );
        buf.set_string(start + wrong_w, buttons_y, " | ", base);
        body
    }
    /// Toggle the current song's wrong-lyrics mark: marked lyrics are
    /// hidden; a second click (or a fetch) restores them.
    fn toggle_wrong(&mut self, ctx: &Ctx) -> Result<()> {
        let Some((_, song)) = ctx.find_current_song_in_queue() else { return Ok(()) };
        if self.wrong_song_file.as_deref() == Some(song.file.as_str()) {
            self.wrong_song_file = None;
        } else {
            self.wrong_song_file = Some(song.file.clone());
        }
        ctx.render()?;
        Ok(())
    }
    /// Whether the terminal emulator reports mouse release events: these
    /// send a `LeftRelease` on every press end, so the pressed marker is
    /// reverted by the release itself and the release-check one-shot would
    /// only fire mid-hold and flash the marker back to `●` while the
    /// button is still down (round 12 — the kitty bug). Emulators we
    /// cannot identify (`Unknown`, e.g. tmux-wrapped setups) keep the
    /// fallback so the marker never sticks.
    fn reports_mouse_release(emulator: crate::shared::terminal::Emulator) -> bool {
        matches!(
            emulator, crate ::shared::terminal::Emulator::Kitty | crate
            ::shared::terminal::Emulator::Ghostty | crate
            ::shared::terminal::Emulator::WezTerm | crate
            ::shared::terminal::Emulator::Konsole | crate
            ::shared::terminal::Emulator::Foot | crate
            ::shared::terminal::Emulator::Iterm2
        )
    }
    /// Schedule the release-check fallback for the pressed marker:
    /// terminals without mouse release events never send a `LeftRelease`,
    /// so the one-shot treats the press as ended after 300 ms (the marker
    /// can only ever show while the button is held, never persistently).
    /// Terminals that do report releases (the kitty family) skip the
    /// one-shot entirely — `LeftRelease` alone reverts the marker, and the
    /// `⭘` persists for the whole hold.
    fn schedule_release_check(&mut self, ctx: &Ctx) {
        if Self::reports_mouse_release(crate::shared::terminal::TERMINAL.emulator()) {
            return;
        }
        if let Some(id) = self.release_check.take() {
            ctx.scheduler.cancel(id);
        }
        let id = crate::shared::id::new();
        self.release_check = Some(id);
        ctx.scheduler
            .schedule_replace(
                id,
                std::time::Duration::from_millis(300),
                move |(tx, _)| {
                    Ok(
                        tx
                            .send(
                                crate::shared::events::AppEvent::UiEvent(
                                    crate::ui::UiAppEvent::LyricsReleaseCheck,
                                ),
                            )?,
                    )
                },
            );
    }
    /// End the pressed-while-held marker: a real `LeftRelease` or the
    /// release-check fallback. Re-renders so the marker reverts to `●`.
    pub(crate) fn release_btn(&mut self, ctx: &Ctx) -> Result<()> {
        if let Some(id) = self.release_check.take() {
            ctx.scheduler.cancel(id);
        }
        if self.pressed_btn.is_some() {
            self.pressed_btn = None;
            ctx.render()?;
        }
        Ok(())
    }
    /// The `fetch lyrics` button: force a refetch of the current song's
    /// lyrics (the configured `on_song_change` command, `rmpc-fetch-lyrics`
    /// by default) and reload them when the file lands. Clears any
    /// wrong-mark first, so hidden lyrics reappear.
    fn fetch_lyrics(&mut self, ctx: &Ctx) -> Result<()> {
        let Some((_, song)) = ctx.find_current_song_in_queue() else { return Ok(()) };
        let song_file = song.file.clone();
        self.wrong_song_file = None;
        if self.fetching {
            return Ok(());
        }
        self.fetching = true;
        ctx.render()?;
        let command = ctx
            .config
            .on_song_change
            .clone()
            .map(|cmd| cmd.as_ref().clone())
            .unwrap_or_else(|| vec!["rmpc-fetch-lyrics".to_owned()]);
        let envs = create_env(ctx, std::iter::empty::<&str>());
        let lyrics_dir = ctx.config.lyrics_dir.clone();
        let work_sender = ctx.work_sender.clone();
        std::thread::spawn(move || {
            let env_refs = envs
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>();
            if let Err(err) = run_external_blocking(&command, env_refs) {
                status_error!("Failed to fetch lyrics: '{err}'");
                return;
            }
            if let Some(dir) = lyrics_dir
                && let Ok(path) = get_lrc_path(&dir, &song_file)
            {
                crate::shared::macros::try_skip!(
                    work_sender.send(WorkRequest::IndexSingleLrc { path }),
                    "Failed to request lyrics index update"
                );
            }
        });
        Ok(())
    }
    /// Whether the box is currently showing track details instead of
    /// lyrics: paused/stopped, or playing a song without lyrics. Round 34:
    /// while edit mode is ON, pausing keeps the lyrics visible (edit mode
    /// is the explicit opt-in that shows and edits per-word timings);
    /// stopped still shows the info panel.
    fn showing_info(&self, ctx: &Ctx) -> bool {
        if self.edit_mode && matches!(ctx.status.state, State::Pause)
            && self.current_lyrics.is_some()
        {
            return false;
        }
        !matches!(ctx.status.state, State::Play) || self.current_lyrics.is_none()
    }
    /// The edit-mode anchor line: the timed line at/just before the paused
    /// position (raw file times — the editor works on the file, not the
    /// offset-adjusted karaoke times).
    fn anchor_line(&self, ctx: &Ctx) -> usize {
        self.edit_session
            .as_ref()
            .map(|s| {
                let elapsed = ctx.status.elapsed;
                s.lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| !l.content.trim().is_empty() && elapsed >= l.time)
                    .min_by(|a, b| {
                        a.1.time.abs_diff(elapsed).cmp(&b.1.time.abs_diff(elapsed))
                    })
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }
    /// Render the lyrics in edit mode (paused): every visible line shows
    /// each word with its raw file time next to it in the edit-timing
    /// style; the selected word is highlighted. Returns the word hit
    /// areas for click selection.
    fn render_edit_mode<'frame, 'buf>(
        &self,
        frame: &'frame mut Frame<'buf>,
        area: Rect,
        ctx: &Ctx,
    ) -> Vec<WordArea> {
        use unicode_width::UnicodeWidthStr;
        let Some(session) = &self.edit_session else { return Vec::new() };
        if area.height == 0 {
            return Vec::new();
        }
        let default_style = Style::default()
            .fg(ctx.config.theme.text_color.unwrap_or_default());
        let selected_style = ctx.config.theme.highlighted_item_style;
        let timing_style = ctx
            .config
            .theme
            .lyrics
            .edit_timing
            .unwrap_or_else(|| default_style.add_modifier(Modifier::DIM));
        let anchor = self.anchor_line(ctx);
        let areas = Layout::vertical((0..area.height).map(|_| Constraint::Length(1)))
            .split(area);
        let middle_row = area.height.saturating_sub(1) / 2;
        let width = area.width as usize;
        let timestamp = ctx.config.theme.lyrics.timestamp;
        let mut word_areas = Vec::new();
        let mut drawer = EditRowDrawer {
            frame,
            default_style,
            selected_style,
            timing_style,
            selection: self.edit_selection,
        };
        let units_of = |line: usize| -> Vec<EditUnit> {
            session
                .lines
                .get(line)
                .map(|l| {
                    l.words
                        .iter()
                        .map(|w| {
                            let time = LrcEditSession::format_time(w.time);
                            let time_w = time.width();
                            EditUnit {
                                word: w.text.clone(),
                                time,
                                word_w: w.text.width(),
                                time_w,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let current_units = units_of(anchor);
        let current_rows = wrap_edit_units(&current_units, width);
        let active_start = middle_row
            .saturating_sub(current_rows.len().saturating_sub(1) as u16);
        for (ri, row_units) in current_rows.iter().enumerate() {
            let Some(row) = areas.get((active_start + ri as u16) as usize).copied() else {
                break;
            };
            drawer.place(row, &current_units, row_units, anchor, &mut word_areas);
        }
        let mut after_row = active_start + current_rows.len() as u16;
        let mut before_row = active_start;
        let mut before_line = anchor;
        while before_line > 0 && before_row > 0 {
            before_line -= 1;
            let units = units_of(before_line);
            if units.is_empty() {
                for chunk in plain_edit_chunks(
                        &session.lines[before_line],
                        timestamp,
                        width,
                    )
                    .iter()
                    .rev()
                {
                    if before_row == 0 {
                        break;
                    }
                    before_row -= 1;
                    if let Some(row) = areas.get(before_row as usize).copied() {
                        drawer.place_plain(row, chunk);
                    }
                }
                continue;
            }
            let rows_ = wrap_edit_units(&units, width);
            for r in rows_.iter().rev() {
                if before_row == 0 {
                    break;
                }
                before_row -= 1;
                let Some(row) = areas.get(before_row as usize).copied() else {
                    break;
                };
                drawer.place(row, &units, r, before_line, &mut word_areas);
            }
        }
        let mut after_line = anchor;
        while after_line + 1 < session.lines.len() && after_row < areas.len() as u16 {
            after_line += 1;
            let units = units_of(after_line);
            if units.is_empty() {
                for chunk in plain_edit_chunks(
                    &session.lines[after_line],
                    timestamp,
                    width,
                ) {
                    if after_row >= areas.len() as u16 {
                        break;
                    }
                    if let Some(row) = areas.get(after_row as usize).copied() {
                        drawer.place_plain(row, &chunk);
                    }
                    after_row += 1;
                }
                continue;
            }
            let rows_ = wrap_edit_units(&units, width);
            for r in &rows_ {
                if after_row >= areas.len() as u16 {
                    break;
                }
                let Some(row) = areas.get(after_row as usize).copied() else {
                    break;
                };
                drawer.place(row, &units, r, after_line, &mut word_areas);
                after_row += 1;
            }
        }
        word_areas
    }
    /// The edit-session lines that have editable words (navigation
    /// targets; plain lines are skipped).
    fn selectable_lines(&self) -> Vec<usize> {
        self.edit_session
            .as_ref()
            .map(|s| {
                s.lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| !l.words.is_empty())
                    .map(|(i, _)| i)
                    .collect()
            })
            .unwrap_or_default()
    }
    fn word_count(&self, line: usize) -> usize {
        self.edit_session
            .as_ref()
            .and_then(|s| s.lines.get(line))
            .map(|l| l.words.len())
            .unwrap_or(0)
    }
    /// Select the anchor line's word that is current at the pause
    /// position (the last word whose raw time is <= the elapsed time —
    /// the word being sung), or the next line with words when the anchor
    /// line is plain. Runs when entering edit mode and when pausing, so
    /// the selection always lands on the current lyric.
    fn select_initial_word(&mut self, ctx: &Ctx) {
        let anchor = self.anchor_line(ctx);
        let lines = self.selectable_lines();
        if lines.contains(&anchor) {
            let word = self.current_word_at(ctx, anchor);
            self.edit_selection = Some((anchor, word));
            return;
        }
        self.edit_selection = lines
            .iter()
            .find(|&&l| l >= anchor)
            .or_else(|| lines.first())
            .map(|&l| (l, 0));
    }
    /// The anchor line's word current at the pause position: the last
    /// word whose raw time is <= the elapsed position (0 when the pause
    /// precedes every word of the line).
    fn current_word_at(&self, ctx: &Ctx, line: usize) -> usize {
        let Some(session) = &self.edit_session else { return 0 };
        let Some(l) = session.lines.get(line) else { return 0 };
        let elapsed = ctx.status.elapsed;
        l.words.iter().rposition(|w| w.time <= elapsed).unwrap_or(0)
    }
    /// Move the word selection: `dx` = previous/next word (wrapping
    /// across lines), `dy` = previous/next line (same word index,
    /// clamped).
    fn move_selection(&mut self, dx: isize, dy: isize) {
        let lines = self.selectable_lines();
        if lines.is_empty() {
            return;
        }
        let (cl, cw) = self.edit_selection.unwrap_or((lines[0], 0));
        let Some(li) = lines.iter().position(|&l| l == cl) else {
            self.edit_selection = Some((lines[0], 0));
            return;
        };
        if dy != 0 {
            let target = li as isize + dy;
            if target < 0 || target >= lines.len() as isize {
                return;
            }
            let tl = lines[target as usize];
            let nw = cw.min(self.word_count(tl).saturating_sub(1));
            self.edit_selection = Some((tl, nw));
            return;
        }
        let mut nl = li;
        let mut nw = cw as isize + dx;
        loop {
            let count = self.word_count(lines[nl]) as isize;
            if nw < 0 {
                if nl == 0 {
                    return;
                }
                nl -= 1;
                nw = self.word_count(lines[nl]) as isize - 1;
            } else if nw >= count {
                if nl + 1 >= lines.len() {
                    return;
                }
                nl += 1;
                nw = 0;
            } else {
                break;
            }
        }
        self.edit_selection = Some((lines[nl], nw as usize));
    }
    /// Nudge the selected word's time by `delta_ms` (10 ms steps).
    fn nudge_selection(&mut self, ctx: &Ctx, delta_ms: i64) -> Result<()> {
        let Some((l, w)) = self.edit_selection else { return Ok(()) };
        let Some(session) = &mut self.edit_session else { return Ok(()) };
        let Some(word) = session.lines.get(l).and_then(|ln| ln.words.get(w)) else {
            return Ok(());
        };
        let new = if delta_ms < 0 {
            word.time.saturating_sub(Duration::from_millis((-delta_ms) as u64))
        } else {
            word.time + Duration::from_millis(delta_ms as u64)
        };
        session.set_word_time(l, w, new)?;
        ctx.render()?;
        Ok(())
    }
    /// Write the pending edits back to the source `.lrc` and reload the
    /// lyrics (stays in edit mode).
    fn save_edit(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.edit_session.as_ref().is_some_and(|s| s.is_dirty()) {
            return Ok(());
        }
        if let Some(session) = &mut self.edit_session {
            session.save()?;
        }
        self.update_lyrics(ctx)?;
        ctx.render()?;
        status_info!("Lyrics timings saved");
        Ok(())
    }
    /// Toggle edit mode. Turning it ON builds the edit session over the
    /// source `.lrc` (the file `find_current_lyrics_path` resolved);
    /// turning it OFF writes any pending edits back to that file and
    /// reloads the lyrics so the karaoke view reflects the saved timings.
    fn set_edit_mode(&mut self, ctx: &Ctx, on: bool) -> Result<()> {
        if on == self.edit_mode {
            return Ok(());
        }
        if on {
            let Some(path) = ctx.find_current_lyrics_path() else {
                status_info!("No lyrics file to edit");
                return Ok(());
            };
            let raw = match std::fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(err) => {
                    status_error!("Failed to read lyrics file: '{err}'");
                    return Ok(());
                }
            };
            self.edit_session = Some(LrcEditSession::open(path, raw));
            self.edit_mode = true;
            self.select_initial_word(ctx);
            ctx.render()?;
        } else {
            if self.edit_session.as_ref().is_some_and(|s| s.is_dirty())
                && let Some(session) = &mut self.edit_session
                && let Err(err) = session.save()
            {
                status_error!("Failed to save lyrics: '{err}'");
            }
            self.edit_session = None;
            self.edit_selection = None;
            self.word_areas.clear();
            self.edit_mode = false;
            if let Err(err) = self.update_lyrics(ctx) {
                status_error!("Failed to reload lyrics file: '{err}'");
            }
            ctx.render()?;
        }
        ctx.lyrics_edit_mode.set(self.edit_mode);
        Ok(())
    }
    /// Round 37: leave edit mode WITHOUT writing the pending changes
    /// (Esc = discard). The session is dropped as-is and the lyrics
    /// reload from the untouched file.
    fn discard_edit_mode(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.edit_mode {
            return Ok(());
        }
        self.edit_session = None;
        self.edit_selection = None;
        self.pending_insert_select = None;
        self.word_areas.clear();
        self.edit_mode = false;
        ctx.lyrics_edit_mode.set(false);
        if let Err(err) = self.update_lyrics(ctx) {
            status_error!("Failed to reload lyrics file: '{err}'");
        }
        ctx.render()?;
        Ok(())
    }
    /// Rebuild the edit session from the current lyrics file (an in-edit
    /// exact-time write landed on disk). Keeps the selection when the
    /// word still exists. When the file is gone/unreadable the session is
    /// dropped and edit mode ends (there is nothing left to edit).
    fn rebuild_edit_session(&mut self, ctx: &Ctx) -> Result<()> {
        let selection = self.edit_selection;
        let pending_select = self.pending_insert_select.take();
        let Some(path) = ctx.find_current_lyrics_path() else {
            self.edit_session = None;
            self.edit_selection = None;
            self.edit_mode = false;
            ctx.lyrics_edit_mode.set(false);
            return Ok(());
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) => {
                status_error!("Failed to read lyrics file: '{err}'");
                self.edit_session = None;
                self.edit_selection = None;
                self.edit_mode = false;
                ctx.lyrics_edit_mode.set(false);
                return Ok(());
            }
        };
        self.edit_session = Some(LrcEditSession::open(path, raw));
        if let Some((sel_line, sel_word)) = pending_select
            && self
                .edit_session
                .as_ref()
                .is_some_and(|s| {
                    s.lines
                        .get(sel_line)
                        .is_some_and(|ln| {
                            !ln.words.is_empty()
                                && (sel_word == 0 || sel_word < ln.words.len())
                        })
                })
        {
            self.edit_selection = Some((sel_line, sel_word));
            return Ok(());
        }
        let valid = selection
            .is_some_and(|(l, w)| {
                self.edit_session
                    .as_ref()
                    .is_some_and(|s| s.lines.get(l).is_some_and(|ln| w < ln.words.len()))
            });
        if !valid {
            self.select_initial_word(ctx);
        }
        Ok(())
    }
    /// The exact-value popup for the selected word: an input modal
    /// prefilled with the current time (`mm:ss.xx`). Pending nudges are
    /// saved first so the popup's read-modify-write starts from the saved
    /// file; the confirm writes the new marker and requests a lyrics
    /// re-index, which reloads the pane.
    fn open_time_modal(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some((l, w)) = self.edit_selection else { return Ok(()) };
        let (path, line, word, current) = {
            let Some(session) = &self.edit_session else { return Ok(()) };
            let Some(word) = session.lines.get(l).and_then(|ln| ln.words.get(w)) else {
                return Ok(());
            };
            (session.path().clone(), l, w, LrcEditSession::format_time(word.time))
        };
        if let Some(session) = &mut self.edit_session && session.is_dirty() {
            session.save()?;
        }
        modal!(
            ctx, InputModal::new(ctx).title("Word time (mm:ss.xx)").confirm_label("Set")
            .input_label("Time:").initial_value(current).on_confirm(move | ctx, value | {
            let Some(time) = LrcEditSession::parse_time(value) else {
            status_error!("Invalid time: expected mm:ss.xx"); return Ok(()); }; let raw =
            std::fs::read_to_string(& path) ?; let new_raw =
            LrcEditSession::apply_to_raw(& raw, line, word, time) ?;
            LrcEditSession::write_atomic(& path, & new_raw) ?; crate
            ::shared::macros::try_skip!(ctx.work_sender.send(crate
            ::shared::events::WorkRequest::IndexSingleLrc { path }),
            "Failed to request lyrics index update"); Ok(()) })
        );
        Ok(())
    }
    /// Round 41: delete the selected WORD (`d`, pending until save like
    /// the word nudges). The line itself is removed only when the word
    /// was its last one; otherwise the selection moves to the word that
    /// took the deleted word's place (else the previous word / previous
    /// line).
    fn delete_current_word(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some((l, w)) = self.edit_selection else { return Ok(()) };
        let Some(session) = &mut self.edit_session else { return Ok(()) };
        if l >= session.lines.len() {
            return Ok(());
        }
        let line_removed = match session.delete_word_at(l, w) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        if line_removed {
            let selectable = self.selectable_lines();
            if let Some(&next) = selectable.iter().find(|&&x| x >= l) {
                self.edit_selection = Some((next, 0));
            } else if let Some(&prev) = selectable.last() {
                self.edit_selection = Some((prev, 0));
            } else {
                self.edit_selection = None;
            }
        } else {
            let word_count = session.lines.get(l).map(|ln| ln.words.len()).unwrap_or(0);
            let nw = if word_count == 0 {
                0
            } else if w < word_count {
                w
            } else {
                word_count - 1
            };
            self.edit_selection = Some((l, nw));
        }
        ctx.render()?;
        Ok(())
    }
    /// Round 35: edit the current line's text. Pending edits are saved
    /// first (round-34 pattern); the modal rewrites the line through a
    /// fresh session so word timings re-interpolate when the word count
    /// changes.
    fn open_line_text_modal(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some((l, _)) = self.edit_selection else { return Ok(()) };
        let (path, line_idx, current) = {
            let Some(session) = &self.edit_session else { return Ok(()) };
            let Some(line) = session.lines.get(l) else { return Ok(()) };
            (session.path().clone(), l, line.content.clone())
        };
        if let Some(session) = &mut self.edit_session && session.is_dirty() {
            session.save()?;
        }
        modal!(
            ctx, InputModal::new(ctx).title("Lyrics text").confirm_label("Set")
            .input_label("Text:").initial_value(current).on_confirm(move | ctx, value | {
            let raw = std::fs::read_to_string(& path) ?; let mut session =
            LrcEditSession::open(path.clone(), raw); session.set_line_text(line_idx,
            value) ?; session.save() ?; crate ::shared::macros::try_skip!(ctx.work_sender
            .send(crate ::shared::events::WorkRequest::IndexSingleLrc { path }),
            "Failed to request lyrics index update"); Ok(()) })
        );
        Ok(())
    }
    /// Round 35: set the current line's timestamp (the `[mm:ss.xx]` tag;
    /// word markers are untouched). Same save-first + fresh-session
    /// pattern as the text modal.
    fn open_line_time_modal(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some((l, _)) = self.edit_selection else { return Ok(()) };
        let (path, line_idx, current) = {
            let Some(session) = &self.edit_session else { return Ok(()) };
            let Some(line) = session.lines.get(l) else { return Ok(()) };
            (session.path().clone(), l, LrcEditSession::format_time(line.time))
        };
        if let Some(session) = &mut self.edit_session && session.is_dirty() {
            session.save()?;
        }
        modal!(
            ctx, InputModal::new(ctx).title("Line time (mm:ss.xx)").confirm_label("Set")
            .input_label("Time:").initial_value(current).on_confirm(move | ctx, value | {
            let Some(time) = LrcEditSession::parse_time(value) else {
            status_error!("Invalid time: expected mm:ss.xx"); return Ok(()); }; let raw =
            std::fs::read_to_string(& path) ?; let mut session =
            LrcEditSession::open(path.clone(), raw); session.set_line_time(line_idx,
            time) ?; session.save() ?; crate ::shared::macros::try_skip!(ctx.work_sender
            .send(crate ::shared::events::WorkRequest::IndexSingleLrc { path }),
            "Failed to request lyrics index update"); Ok(()) })
        );
        Ok(())
    }
    /// Round 35: insert a new lyric line before (`before`) or after the
    /// current line. The suggested timestamp is the midpoint to the
    /// neighbour (5 s past the anchor at the ends); the text comes from
    /// the modal, the line is written immediately (pending edits are
    /// saved first) and re-selected via the re-index.
    fn insert_new_line(&mut self, ctx: &mut Ctx, before: bool) -> Result<()> {
        let Some((l, _)) = self.edit_selection else { return Ok(()) };
        let (path, position, time) = {
            let Some(session) = &self.edit_session else { return Ok(()) };
            let Some(anchor) = session.lines.get(l) else { return Ok(()) };
            let position = if before { l } else { l + 1 };
            let time = if before {
                match session.lines.get(l.wrapping_sub(1)).map(|x| x.time) {
                    Some(prev) if prev < anchor.time => prev + (anchor.time - prev) / 2,
                    _ => anchor.time.saturating_sub(Duration::from_secs(5)),
                }
            } else {
                match session.lines.get(l + 1).map(|x| x.time) {
                    Some(next) if next > anchor.time => {
                        anchor.time + (next - anchor.time) / 2
                    }
                    _ => anchor.time + Duration::from_secs(5),
                }
            };
            (session.path().clone(), position, time)
        };
        if let Some(session) = &mut self.edit_session && session.is_dirty() {
            session.save()?;
        }
        self.pending_insert_select = Some((position, 0));
        modal!(
            ctx, InputModal::new(ctx).title(if before { "New lyric before" } else {
            "New lyric after" }).confirm_label("Insert").input_label("Text:")
            .on_confirm(move | ctx, value | { let raw = std::fs::read_to_string(& path)
            ?; let mut session = LrcEditSession::open(path.clone(), raw); let idx =
            session.insert_line_at(position, time) ?; session.set_line_text(idx, value)
            ?; session.save() ?; crate ::shared::macros::try_skip!(ctx.work_sender
            .send(crate ::shared::events::WorkRequest::IndexSingleLrc { path }),
            "Failed to request lyrics index update"); Ok(()) })
        );
        Ok(())
    }
    /// Round 40: `i`/`a` — insert a new WORD into the current line
    /// before (`before`) or after the selected word. The word stays on
    /// the SAME line; its time interpolates between the neighbours
    /// (`LrcEditSession::insert_word_at`). Pending edits are saved
    /// first, the text comes from the modal, the line is written
    /// immediately and re-indexed; the new word is re-selected via
    /// `pending_insert_select`.
    fn insert_word(&mut self, ctx: &mut Ctx, before: bool) -> Result<()> {
        let Some((l, w)) = self.edit_selection else { return Ok(()) };
        let (path, line_idx, word_idx) = {
            let Some(session) = &self.edit_session else { return Ok(()) };
            let Some(line) = session.lines.get(l) else { return Ok(()) };
            if w >= line.words.len() {
                return Ok(());
            }
            (session.path().clone(), l, w)
        };
        if let Some(session) = &mut self.edit_session && session.is_dirty() {
            session.save()?;
        }
        self.pending_insert_select = Some((line_idx, if before { w } else { w + 1 }));
        modal!(
            ctx, InputModal::new(ctx).title(if before { "New word before" } else {
            "New word after" }).confirm_label("Insert").input_label("Word:")
            .on_confirm(move | ctx, value | { let raw = std::fs::read_to_string(& path)
            ?; let mut session = LrcEditSession::open(path.clone(), raw); session
            .insert_word_at(line_idx, word_idx, ! before, value) ?; session.save() ?;
            crate ::shared::macros::try_skip!(ctx.work_sender.send(crate
            ::shared::events::WorkRequest::IndexSingleLrc { path }),
            "Failed to request lyrics index update"); Ok(()) })
        );
        Ok(())
    }
    /// Render the song details (File / Filename / Title / ... with the
    /// yellow group labels), matching the Directories / Radio info boxes.
    /// Scrollable with the mouse wheel; the offset resets when the shown
    /// song changes.
    fn render_info(&mut self, frame: &mut Frame, area: Rect, song: &Song, ctx: &Ctx) {
        let preview = song
            .to_preview(
                ctx.config.theme.preview_label_style,
                ctx.config.theme.preview_metadata_group_style,
                ctx,
            );
        let mut items: Vec<ListItem> = Vec::new();
        for group in preview {
            if let Some(name) = group.name {
                items
                    .push(
                        ListItem::new(
                            Line::styled(name, group.header_style.unwrap_or_default()),
                        ),
                    );
            }
            items.extend(group.items);
            items.push(ListItem::new(""));
        }
        self.info_items_len = items.len();
        self.info_area = area;
        if self.info_song_id != Some(song.id) {
            self.info_song_id = Some(song.id);
            self.info_state = ListState::default();
        }
        let overflow = items.len() > area.height as usize;
        let (list_area, scrollbar_area) = if overflow
            && ctx.config.as_styled_scrollbar().is_some()
        {
            let [a, b] = Layout::horizontal([
                    Constraint::Percentage(100),
                    Constraint::Length(1),
                ])
                .areas(area);
            (a, b)
        } else {
            (area, Rect::default())
        };
        let list = List::new(items).style(ctx.config.as_text_style());
        StatefulWidget::render(
            list,
            list_area,
            frame.buffer_mut(),
            &mut self.info_state,
        );
        if scrollbar_area.width > 0
            && let Some(scrollbar) = ctx.config.as_styled_scrollbar()
        {
            let max_offset = self
                .info_items_len
                .saturating_sub(list_area.height as usize);
            let position = self.info_state.offset().min(max_offset);
            StatefulWidget::render(
                scrollbar,
                scrollbar_area,
                frame.buffer_mut(),
                &mut ratatui::widgets::ScrollbarState::new(max_offset + 1)
                    .position(position)
                    .viewport_content_length(list_area.height as usize),
            );
        }
        self.info_scrollbar_area = scrollbar_area;
    }
    /// Render the details of the video playing in mpv (Title / Series /
    /// Year / Runtime / URL), with the same yellow group labels as the
    /// song info box. Scrollable with the mouse wheel; the offset resets
    /// when the video changes.
    fn render_mpv_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let key_style = ctx.config.theme.preview_label_style;
        let base = ctx.config.as_text_style();
        let list_style = ctx.config.as_list_text_style();
        let white = Style::default().fg(Color::White);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut title_prefix = String::new();
        let mut title = ctx.mpv.title.clone();
        let mut context_left: Vec<Span<'static>> = Vec::new();
        let mut context_right: Vec<Span<'static>> = Vec::new();
        let mut body: Vec<InfoBodyLine> = Vec::new();
        let mut credits: Vec<Line<'static>> = Vec::new();
        let body_width = (area.width.saturating_sub(4).max(10)) as usize;
        let push_body = |body: &mut Vec<InfoBodyLine>, text: &str, style: Style| {
            body.push(InfoBodyLine::new(text, style));
        };
        if let Some(yt) = crate::ui::modals::paste::current_yt_info(ctx) {
            if !yt.title.is_empty() {
                title = yt.title.clone();
            }
            let (left, right, body_parts) = yt_stream_info_parts(
                &yt,
                base,
                list_style,
                body_width,
            );
            context_left.extend(left);
            context_right.extend(right);
            body.extend(body_parts);
        } else {
            if let Some(item) = &ctx.mpv.item {
                let name = if item.kind == "Episode" {
                    item.series_name.clone().unwrap_or_else(|| item.name.clone())
                } else {
                    item.name.clone()
                };
                match item.year {
                    Some(year) => {
                        title_prefix = format!("{year} -- ");
                        title = name;
                    }
                    None => title = name,
                };
                if item.kind == "Episode" {
                    context_left.push(Span::styled("Episode: ", base));
                    context_left.push(Span::styled(item.name.clone(), base));
                    if let (Some(season), Some(episode)) = (
                        item.season_number,
                        item.index_number,
                    ) {
                        context_right
                            .push(
                                Span::styled(format!("S{season:02}E{episode:02}"), base),
                            );
                    }
                }
                if let Some(overview) = item
                    .overview
                    .as_deref()
                    .filter(|d| !d.trim().is_empty())
                {
                    for line in wrap_to_width(&scrub_emoji(overview), body_width) {
                        push_body(&mut body, &line, list_style);
                    }
                }
                if let Some(director) = &item.director {
                    credits
                        .push(Self::credit_line(key_style, white, "Director", director));
                }
                if let Some(writer) = &item.writer {
                    credits.push(Self::credit_line(key_style, white, "Writer", writer));
                }
                if !item.starring.is_empty() {
                    credits
                        .push(
                            Self::credit_line(
                                key_style,
                                white,
                                "Starring",
                                &item.starring.join(", "),
                            ),
                        );
                }
            } else if !ctx.mpv.title.is_empty() {
                title = ctx.mpv.title.clone();
            }
        }
        let has_context = !context_left.is_empty() || !context_right.is_empty();
        let header_h = 1 + usize::from(has_context) + usize::from(!body.is_empty());
        let credits_h = credits.len();
        let [header_area, body_area, credits_area] = Layout::vertical([
                Constraint::Length(header_h as u16),
                Constraint::Min(0),
                Constraint::Length(credits_h as u16),
            ])
            .areas(area);
        let duration = if crate::core::mpv::mpv_is_ui_source(ctx) {
            ctx.mpv.duration
        } else {
            ctx.status.duration.as_secs_f64()
        };
        let time_text = format!("Time: {}", format_clock(duration as u64));
        let time_w = (time_text.chars().count() + 4) as u16;
        let (title_area, time_area) = {
            let [a, b] = Layout::horizontal([
                    Constraint::Min(0),
                    Constraint::Length(time_w),
                ])
                .areas(header_area);
            (a, b)
        };
        frame
            .render_widget(
                Paragraph::new(Span::styled(time_text, bold))
                    .alignment(Alignment::Right),
                time_area,
            );
        let prefix_w = title_prefix.chars().count() as u16;
        let (prefix_area, marquee_area) = if prefix_w > 0 && prefix_w < title_area.width
        {
            let [a, b] = Layout::horizontal([
                    Constraint::Length(prefix_w),
                    Constraint::Min(0),
                ])
                .areas(title_area);
            (a, b)
        } else {
            (Rect::default(), title_area)
        };
        if prefix_w > 0 {
            frame
                .render_widget(
                    Paragraph::new(Span::styled(title_prefix, base)),
                    prefix_area,
                );
        }
        let title_len = title.chars().count() as u16;
        let offset = if title_len > marquee_area.width {
            let elapsed_ms = self
                .info_video_shown_at
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(0) as u64;
            crate::ui::widgets::marquee::marquee_offset(
                elapsed_ms,
                title_len,
                marquee_area.width,
            )
        } else {
            0
        };
        crate::ui::widgets::marquee::draw_panel_at(
            frame.buffer_mut(),
            marquee_area.x,
            marquee_area.y,
            marquee_area.width,
            &Line::from(Span::styled(title, white)),
            offset,
            white,
        );
        if has_context {
            let row = Rect {
                x: header_area.x,
                y: header_area.y + 1,
                width: header_area.width,
                height: 1,
            };
            let [left_area, right_area] = Layout::horizontal([
                    Constraint::Min(0),
                    Constraint::Length(time_w),
                ])
                .areas(row);
            if !context_left.is_empty() {
                frame.render_widget(Paragraph::new(Line::from(context_left)), left_area);
            }
            if !context_right.is_empty() {
                frame
                    .render_widget(
                        Paragraph::new(Line::from(context_right))
                            .alignment(Alignment::Right),
                        right_area,
                    );
            }
        }
        if !body.is_empty() {
            let row = Rect {
                x: header_area.x,
                y: header_area.y + 1 + usize::from(has_context) as u16,
                width: header_area.width,
                height: 1,
            };
            frame
                .render_widget(
                    Paragraph::new(
                        Line::from(
                            vec![
                                Span::styled("Description", key_style), Span::styled(" ↴",
                                white),
                            ],
                        ),
                    ),
                    row,
                );
        }
        for (i, line) in credits.iter().enumerate() {
            let row = Rect {
                x: credits_area.x,
                y: credits_area.y + i as u16,
                width: credits_area.width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(line.clone()), row);
        }
        self.info_items_len = body.len();
        self.info_area = body_area;
        let key = ctx
            .mpv
            .item_id
            .clone()
            .or_else(|| {
                (!crate::core::mpv::mpv_is_ui_source(ctx))
                    .then(|| {
                        ctx.find_current_song_in_queue()
                            .map(|(_, song)| song.file.clone())
                            .unwrap_or_default()
                    })
            })
            .or_else(|| Some(ctx.mpv.title.clone()))
            .unwrap_or_default();
        if self.info_video_key.as_deref() != Some(key.as_str()) {
            self.info_video_key = Some(key);
            self.info_state = ListState::default();
            self.info_video_shown_at = Some(std::time::Instant::now());
        }
        if body_area.height == 0 || body.is_empty() {
            self.info_scrollbar_area = Rect::default();
            return;
        }
        let (list_area, scrollbar_area) = if body.len() > body_area.height as usize
            && ctx.config.as_styled_scrollbar().is_some()
        {
            let [a, b] = Layout::horizontal([
                    Constraint::Percentage(100),
                    Constraint::Length(1),
                ])
                .areas(body_area);
            (a, b)
        } else {
            (body_area, Rect::default())
        };
        let hovered_link = ctx
            .mouse_pos()
            .and_then(|pos| {
                if !list_area.contains(pos) {
                    return None;
                }
                let idx = self.info_state.offset() + usize::from(pos.y - list_area.y);
                let x = pos.x.saturating_sub(list_area.x);
                (idx < body.len()
                    && body[idx].link_ranges.iter().any(|(a, b)| *a <= x && x < *b))
                    .then_some(idx)
            });
        let items = body
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let line = if Some(idx) == hovered_link {
                    row.hovered().line
                } else {
                    row.line.clone()
                };
                ListItem::new(line)
            })
            .collect::<Vec<_>>();
        let list = List::new(items).style(list_style);
        StatefulWidget::render(
            list,
            list_area,
            frame.buffer_mut(),
            &mut self.info_state,
        );
        if scrollbar_area.width > 0
            && let Some(scrollbar) = ctx.config.as_styled_scrollbar()
        {
            let max_offset = self
                .info_items_len
                .saturating_sub(list_area.height as usize);
            let position = self.info_state.offset().min(max_offset);
            StatefulWidget::render(
                scrollbar,
                scrollbar_area,
                frame.buffer_mut(),
                &mut ratatui::widgets::ScrollbarState::new(max_offset + 1)
                    .position(position)
                    .viewport_content_length(list_area.height as usize),
            );
        }
        self.info_scrollbar_area = scrollbar_area;
    }
    /// A credits row (Director / Writer / Starring): yellow label, value in
    /// the list color.
    fn credit_line(
        key_style: Style,
        list_style: Style,
        label: &'static str,
        value: &str,
    ) -> Line<'static> {
        Line::from(
            vec![
                Span::styled(format!("{label}: "), key_style), Span::styled(value
                .to_owned(), list_style),
            ],
        )
    }
}
impl Pane for LyricsPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        if area.height < crate::ui::panes::MIN_PANE_CONTENT_HEIGHT {
            self.info_area = Rect::default();
            self.info_scrollbar_area = Rect::default();
            self.wrong_btn_area = Rect::default();
            self.fetch_btn_area = Rect::default();
            self.edit_btn_area = Rect::default();
            self.word_areas.clear();
            self.pressed_btn = None;
            return Ok(());
        }
        if crate::core::mpv::mpv_is_ui_source(ctx) {
            self.wrong_btn_area = Rect::default();
            self.fetch_btn_area = Rect::default();
            self.edit_btn_area = Rect::default();
            self.word_areas.clear();
            self.pressed_btn = None;
            self.render_mpv_info(frame, area, ctx);
            return Ok(());
        }
        if self.showing_info(ctx) {
            self.wrong_btn_area = Rect::default();
            self.fetch_btn_area = Rect::default();
            self.edit_btn_area = Rect::default();
            self.word_areas.clear();
            self.pressed_btn = None;
            let selected = || {
                ctx.queue_selected_id
                    .get()
                    .and_then(|id| ctx.queue.iter().find(|song| song.id == id))
            };
            let song = if ctx.status.state == State::Stop {
                selected()
            } else {
                ctx.find_current_song_in_queue().map(|(_, song)| song).or_else(selected)
            };
            if crate::ui::modals::paste::current_yt_info(ctx).is_some() {
                self.render_mpv_info(frame, area, ctx);
                return Ok(());
            }
            if let Some(song) = song {
                self.render_info(frame, area, song, ctx);
            }
            return Ok(());
        }
        if self.current_lyrics.is_none() {
            return Ok(());
        }
        let body_area = self.render_frame(frame, area, ctx);
        if self.is_wrong(ctx) {
            let song = ctx
                .find_current_song_in_queue()
                .map(|(_, song)| song)
                .or_else(|| {
                    ctx.queue_selected_id
                        .get()
                        .and_then(|id| ctx.queue.iter().find(|song| song.id == id))
                });
            if let Some(song) = song {
                self.render_info(frame, body_area, song, ctx);
            }
            return Ok(());
        }
        if self.edit_mode && matches!(ctx.status.state, State::Pause) {
            self.word_areas = self.render_edit_mode(frame, body_area, ctx);
            return Ok(());
        }
        let area = body_area;
        let Some(lrc) = &self.current_lyrics else { return Ok(()) };
        let offset = ctx.config.lyrics_offset;
        let elapsed = ctx.status.elapsed;
        let (current_line_idx, first_line_reached) = lrc
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| !line.content.is_empty() && elapsed >= line.time(offset))
            .min_by(|a, b| {
                a.1
                    .time(offset)
                    .abs_diff(elapsed)
                    .cmp(&b.1.time(offset).abs_diff(elapsed))
            })
            .map_or((0, false), |result| (result.0, true));
        let rows = area.height;
        let areas = Layout::vertical((0..rows).map(|_| Constraint::Length(1)))
            .split(area);
        let middle_row = rows.saturating_sub(1) / 2;
        let default_style = Style::default()
            .fg(ctx.config.theme.text_color.unwrap_or_default());
        let middle_style = if first_line_reached {
            ctx.config.theme.highlighted_item_style
        } else {
            default_style
        };
        let timestamp = ctx.config.theme.lyrics.timestamp;
        let Some(current_line) = lrc.lines.get(current_line_idx) else {
            return Ok(());
        };
        let word_times = ctx
            .config
            .theme
            .lyrics
            .word_highlight
            .then(|| lrc.timed_words(current_line_idx, offset))
            .flatten();
        let current_rows: Vec<Line> = if let Some(word_times) = word_times {
            let t0 = current_line.time(offset);
            let mut spans: Vec<Span> = Vec::new();
            if timestamp && !current_line.content.is_empty() {
                spans.push(Span::styled(format!("[{}] ", t0.to_string()), middle_style));
            }
            for w in word_times {
                let style = if first_line_reached && elapsed >= w.time {
                    middle_style
                } else {
                    default_style
                };
                spans.push(Span::styled(w.text, style));
            }
            wrap_spans(&spans, area.width)
        } else {
            let formatted_line = if timestamp && !current_line.content.is_empty() {
                format!(
                    "[{}] {}", current_line.time(offset).to_string(), current_line
                    .content
                )
            } else {
                current_line.content.clone()
            };
            textwrap::wrap(&formatted_line, area.width as usize)
                .into_iter()
                .map(|l| {
                    Line::from(l.as_ref().to_owned())
                        .style(middle_style)
                        .alignment(Alignment::Center)
                })
                .collect()
        };
        let wrapped_lines_length = current_rows.len();
        let active_lyric_start_row = (middle_row as usize)
            .saturating_sub(wrapped_lines_length.saturating_sub(1));
        let mut current_area = active_lyric_start_row;
        for line in current_rows {
            let Some(area) = areas.get(current_area) else {
                break;
            };
            frame.render_widget(line, *area);
            current_area += 1;
        }
        let mut before_lyrics_cursor = current_line_idx;
        let mut before_area_cursor = active_lyric_start_row as usize;
        while before_lyrics_cursor > 0 && before_area_cursor > 0 {
            before_lyrics_cursor -= 1;
            let Some(line) = lrc.lines.get(before_lyrics_cursor) else {
                break;
            };
            let formatted_line = if timestamp && !line.content.is_empty() {
                &format!("[{}] {}", line.time(offset).to_string(), line.content)
            } else {
                &line.content
            };
            for l in textwrap::wrap(formatted_line, area.width as usize).iter().rev() {
                if before_area_cursor == 0 {
                    break;
                }
                let Some(area) = areas.get(before_area_cursor - 1) else {
                    break;
                };
                let text = Text::from(l.as_ref()).centered().style(default_style);
                frame.render_widget(text, *area);
                before_area_cursor -= 1;
            }
        }
        let mut after_lyrics_cursor = current_line_idx;
        let mut after_area_cursor = current_area.saturating_sub(1);
        while !areas.is_empty() && after_lyrics_cursor < lrc.lines.len() - 1
            && after_area_cursor < areas.len() - 1
        {
            after_lyrics_cursor += 1;
            let Some(line) = lrc.lines.get(after_lyrics_cursor) else {
                break;
            };
            let formatted_line = if timestamp && !line.content.is_empty() {
                &format!("[{}] {}", line.time(offset).to_string(), line.content)
            } else {
                &line.content
            };
            for l in textwrap::wrap(formatted_line, area.width as usize) {
                let Some(area) = areas.get(after_area_cursor + 1) else {
                    break;
                };
                let text = Text::from(l).centered().style(default_style);
                frame.render_widget(text, *area);
                after_area_cursor += 1;
            }
        }
        let next_idx = if first_line_reached {
            current_line_idx + 1
        } else {
            current_line_idx
        };
        if self.last_requested_line_idx != next_idx
            && let Some(line) = lrc.lines.get(next_idx)
        {
            self.last_requested_line_idx = next_idx;
            ctx.scheduler
                .schedule(
                    line.time(offset).saturating_sub(ctx.status.elapsed),
                    run_status_update,
                );
        }
        Ok(())
    }
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.initialized {
            if let Err(err) = self.update_lyrics(ctx) {
                status_error!("Failed to load lyrics file: '{err}'");
            }
            self.last_requested_line_idx = usize::MAX;
            self.initialized = true;
        }
        Ok(())
    }
    fn on_event(
        &mut self,
        event: &mut UiEvent,
        _is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match event {
            UiEvent::SongChanged | UiEvent::Reconnected => {
                if self.edit_mode {
                    self.set_edit_mode(ctx, false)?;
                }
                self.fetching = false;
                if let Err(err) = self.update_lyrics(ctx) {
                    status_error!("Failed to load lyrics file: '{err}'");
                }
                ctx.render()?;
                self.last_requested_line_idx = usize::MAX;
            }
            UiEvent::LyricsIndexed => {
                self.fetching = false;
                if let Err(err) = self.update_lyrics(ctx) {
                    status_error!("Failed to load lyrics file: '{err}'");
                }
                if self.edit_mode && let Err(err) = self.rebuild_edit_session(ctx) {
                    status_error!("Failed to reload the lyrics editor: '{err}'");
                }
                ctx.render()?;
                self.last_requested_line_idx = usize::MAX;
            }
            UiEvent::PlaybackStateChanged => {
                self.last_requested_line_idx = usize::MAX;
                if self.edit_mode && matches!(ctx.status.state, State::Pause)
                    && self.edit_session.is_some()
                {
                    self.select_initial_word(ctx);
                }
                ctx.render()?;
            }
            _ => {}
        }
        Ok(())
    }
    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if !self.edit_mode || self.edit_session.is_none() {
            return Ok(());
        }
        let paused = matches!(ctx.status.state, State::Pause);
        let claimed = event.claim_common();
        if !paused
            && !matches!(
                claimed, Some(CommonAction::Close) |
                Some(CommonAction::LyricsSaveAndExit) | Some(CommonAction::LyricsSave)
            )
        {
            event.abandon();
            return Ok(());
        }
        match claimed {
            Some(CommonAction::Left) => {
                self.move_selection(-1, 0);
                ctx.render()?;
            }
            Some(CommonAction::Right) => {
                self.move_selection(1, 0);
                ctx.render()?;
            }
            Some(CommonAction::Up) => {
                self.move_selection(0, -1);
                ctx.render()?;
            }
            Some(CommonAction::Down) => {
                self.move_selection(0, 1);
                ctx.render()?;
            }
            Some(CommonAction::Confirm) => self.open_time_modal(ctx)?,
            Some(CommonAction::Close) => {
                self.discard_edit_mode(ctx)?;
                event.consume();
            }
            Some(CommonAction::LyricsSaveAndExit) => {
                self.set_edit_mode(ctx, false)?;
            }
            Some(CommonAction::LyricsNudgeUp) => self.nudge_selection(ctx, 10)?,
            Some(CommonAction::LyricsNudgeDown) => self.nudge_selection(ctx, -10)?,
            Some(CommonAction::LyricsSave) => self.save_edit(ctx)?,
            Some(CommonAction::LyricsEditLine) => self.open_line_text_modal(ctx)?,
            Some(CommonAction::LyricsDeleteWord) => self.delete_current_word(ctx)?,
            Some(CommonAction::LyricsInsertBefore) => self.insert_word(ctx, true)?,
            Some(CommonAction::LyricsInsertAfter) => self.insert_word(ctx, false)?,
            Some(CommonAction::LyricsAddLineBefore) => self.insert_new_line(ctx, true)?,
            Some(CommonAction::LyricsAddLineAfter) => self.insert_new_line(ctx, false)?,
            Some(CommonAction::LyricsLineTime) => self.open_line_time_modal(ctx)?,
            _ => {}
        }
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                let pos: ratatui::layout::Position = event.into();
                if self.edit_btn_area.contains(pos) {
                    self.pressed_btn = Some(LyricsBtn::Edit);
                    self.schedule_release_check(ctx);
                    return self.set_edit_mode(ctx, !self.edit_mode);
                }
                if self.wrong_btn_area.contains(pos) {
                    if self.edit_mode {
                        self.set_edit_mode(ctx, false)?;
                    }
                    self.pressed_btn = Some(LyricsBtn::Wrong);
                    self.schedule_release_check(ctx);
                    return self.toggle_wrong(ctx);
                }
                if self.fetch_btn_area.contains(pos) {
                    if self.edit_mode {
                        self.set_edit_mode(ctx, false)?;
                    }
                    self.pressed_btn = Some(LyricsBtn::Fetch);
                    self.schedule_release_check(ctx);
                    return self.fetch_lyrics(ctx);
                }
                if self.edit_mode && matches!(ctx.status.state, State::Pause)
                    && !self.is_wrong(ctx)
                    && let Some(area) = self
                        .word_areas
                        .iter()
                        .find(|w| w.rect.contains(pos))
                {
                    self.edit_selection = Some((area.line, area.word));
                    ctx.render()?;
                    return Ok(());
                }
            }
            MouseEventKind::LeftRelease => {
                return self.release_btn(ctx);
            }
            _ => {}
        }
        if (self.showing_info(ctx) || self.is_wrong(ctx))
            && self.info_scrollbar_area.height > 0
            && matches!(
                event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }
            ) && self.info_scrollbar_area.contains(event.into())
        {
            let max = self.info_items_len.saturating_sub(self.info_area.height as usize);
            if max > 0 {
                let viewport_len = self.info_area.height as usize;
                let position = self.info_state.offset();
                let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
                if let Some(perc) = self
                    .info_scrollbar_drag
                    .handle(
                        event,
                        self.info_scrollbar_area,
                        max + 1,
                        viewport_len,
                        position,
                        begin_len,
                        end_len,
                    )
                {
                    let new = ((perc.clamp(0.0, 1.0)) * max as f64).floor() as usize;
                    if new != self.info_state.offset() {
                        *self.info_state.offset_mut() = new;
                        ctx.render()?;
                    }
                    return Ok(());
                }
            }
            return Ok(());
        }
        if (self.showing_info(ctx) || self.is_wrong(ctx))
            && self.info_area.contains(event.into())
        {
            let dir = match event.kind {
                MouseEventKind::ScrollUp => -1,
                MouseEventKind::ScrollDown => 1,
                _ => return Ok(()),
            };
            let max = self.info_items_len.saturating_sub(self.info_area.height as usize)
                as i64;
            let current = self.info_state.offset() as i64;
            let new = (current + dir).clamp(0, max.max(0)) as usize;
            if new != self.info_state.offset() {
                *self.info_state.offset_mut() = new;
                ctx.render()?;
            }
        }
        Ok(())
    }
}
/// Remove emoji and their variation selectors from a description (they
/// render as wide glyphs / tofu in the info box). Keeps arrows, dashes and
/// other text punctuation.
pub(crate) fn scrub_emoji(text: &str) -> String {
    text.chars()
        .filter(|c| {
            let cp = *c as u32;
            !(0x1F000..=0x1FAFF).contains(&cp) && !(0x2600..=0x27BF).contains(&cp)
                && !(0xFE00..=0xFE0F).contains(&cp) && !(0x200D..=0x200D).contains(&cp)
                && !(0x20E3..=0x20E3).contains(&cp)
        })
        .collect()
}
/// Compact count: 1710000 -> "1.71M", 15200000 -> "15.20M", 850 -> "850".
fn compact_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
/// Hyperlink blue in the info box: kitty's default ANSI blue, as an RGB
/// value so the standard hover lightening (blend toward white) applies —
/// ANSI colors pass through `hover_color` unchanged.
pub(crate) const LINK_BLUE: Color = Color::Rgb(0x0d, 0x73, 0xcc);
/// One row of the scrollable info-box body (a wrapped description line):
/// the line to render plus the cell x-ranges of its blue link spans, used
/// to lighten the link under the pointer (the standard hover effect).
pub(crate) struct InfoBodyLine {
    pub line: Line<'static>,
    link_ranges: Vec<(u16, u16)>,
}
impl InfoBodyLine {
    /// Build a row from wrapped description text, drawing `http(s)://`
    /// links in the link blue.
    fn new(text: &str, style: Style) -> Self {
        let (spans, link_ranges) = linkify_line(text, style);
        Self {
            line: Line::from(spans),
            link_ranges,
        }
    }
    /// The row with its blue link spans lightened (the standard hover
    /// effect) — applied to the row under the pointer.
    fn hovered(&self) -> Self {
        let mut x = 0u16;
        let spans: Vec<Span<'static>> = self
            .line
            .spans
            .iter()
            .map(|span| {
                let w = span.width() as u16;
                let is_link = self
                    .link_ranges
                    .iter()
                    .any(|(a, b)| *a == x && *b == x + w);
                x += w;
                if is_link {
                    Span::styled(
                        span.content.clone(),
                        crate::config::hover_style(span.style),
                    )
                } else {
                    span.clone()
                }
            })
            .collect();
        let mut line = Line::from(spans);
        line.alignment = self.line.alignment;
        line.style = self.line.style;
        Self {
            line,
            link_ranges: self.link_ranges.clone(),
        }
    }
}
/// Split a line into spans, drawing `http(s)://` and `www.` URLs in the
/// link blue. Returns the spans and each link span's cell x-range within
/// the line (the pointer hit area for the hover lightening).
pub(crate) fn linkify_line(
    line: &str,
    base_style: Style,
) -> (Vec<Span<'static>>, Vec<(u16, u16)>) {
    use unicode_width::UnicodeWidthStr;
    let link_style = base_style.fg(LINK_BLUE);
    let mut spans = Vec::new();
    let mut link_ranges = Vec::new();
    let mut rest = line;
    let mut x = 0u16;
    loop {
        let Some((start, len)) = find_url(rest) else {
            if !rest.is_empty() {
                spans.push(Span::styled(rest.to_owned(), base_style));
            }
            break;
        };
        let (head, tail) = rest.split_at(start);
        let (url, remaining) = tail.split_at(len);
        if !head.is_empty() {
            x += head.width() as u16;
            spans.push(Span::styled(head.to_owned(), base_style));
        }
        let w = url.width() as u16;
        link_ranges.push((x, x + w));
        x += w;
        spans.push(Span::styled(url.to_owned(), link_style));
        rest = remaining;
    }
    (spans, link_ranges)
}
/// Find the next `http(s)://` or `www.` link in `text`, returning the
/// byte range of the whole URL (scheme/prefix plus the body up to
/// whitespace, trailing punctuation trimmed). Multibyte-safe: a scheme can
/// only match at an ASCII byte, so the returned offsets are always char
/// boundaries.
fn find_url(text: &str) -> Option<(usize, usize)> {
    const SCHEMES: [&[u8]; 3] = [b"https://", b"http://", b"www."];
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        for scheme in SCHEMES {
            let scheme_end = i + scheme.len();
            if bytes.len() >= scheme_end
                && bytes[i..scheme_end]
                    .iter()
                    .zip(scheme.iter())
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                let body = text[scheme_end..].split_whitespace().next().unwrap_or("");
                let trimmed = body
                    .trim_end_matches(|c: char| {
                        matches!(
                            c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"'
                            | '\'' | '»'
                        )
                    });
                let valid = if scheme == b"www." {
                    trimmed.contains('.') && !trimmed.starts_with('.')
                } else {
                    true
                };
                if valid && !trimmed.is_empty() {
                    return Some((i, scheme.len() + trimmed.len()));
                }
            }
        }
    }
    None
}
/// The video-style info box content of a resolved YouTube-style stream,
/// shared by the queue tab's lyrics box (playing stream / mpv video) and
/// the Playlists tab (a selected stream entry): the channel/subs context
/// row and the wrapped, emoji-scrubbed description body. The caller adds
/// the title row and the "Description ↴" label on top.
pub(crate) fn yt_stream_info_parts(
    yt: &crate::shared::ytdlp::YtStreamInfo,
    base: ratatui::style::Style,
    list_style: ratatui::style::Style,
    body_width: usize,
) -> (Vec<Span<'static>>, Vec<Span<'static>>, Vec<InfoBodyLine>) {
    let mut context_left: Vec<Span<'static>> = Vec::new();
    let mut context_right: Vec<Span<'static>> = Vec::new();
    let mut body: Vec<InfoBodyLine> = Vec::new();
    if let Some(channel) = yt.channel.as_deref().filter(|c| !c.is_empty()) {
        context_left.push(Span::styled("Channel: ", base));
        context_left.push(Span::styled(channel.to_owned(), base));
    }
    if let Some(subs) = yt.subscribers.filter(|s| *s > 0) {
        context_right.push(Span::styled("Subs: ", base));
        context_right.push(Span::styled(compact_count(subs), base));
    }
    if let Some(description) = yt.description.as_deref().filter(|d| !d.trim().is_empty())
    {
        let scrubbed = scrub_emoji(description);
        for line in wrap_to_width(&scrubbed, body_width) {
            body.push(InfoBodyLine::new(&line, list_style));
        }
    }
    (context_left, context_right, body)
}
/// A clock time: h:mm:ss when an hour or more, else m:ss. The configured
/// duration_format (e.g. "%m:%S") can drop the hours, rendering 1:29:53 as
/// "29:53"; the info box header always shows the hours.
pub(crate) fn format_clock(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}
